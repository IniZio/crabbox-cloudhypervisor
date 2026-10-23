//! The growth watcher: one detached process per running lease.
//!
//! Every [`INTERVAL`] it samples the guest over ssh, asks the [`Planner`] what
//! to change, applies it through the cloud-hypervisor API and logs each grow
//! and each failure. It exits when the lease's cloud-hypervisor exits.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Growth;
use crate::grow::{Action, CpuTimes, Current, Planner, Sample, GIB, MIB};

mod log;
mod real;

pub use log::Log;
pub use real::run;

/// Time between samples.
pub const INTERVAL: Duration = Duration::from_secs(10);
/// Wait after a raise before reading what the guest sees.
pub const SETTLE: Duration = Duration::from_secs(2);

/// Guest command printing root fs size and free bytes, CPU and memory counters.
pub const SAMPLE_SCRIPT: &str =
    "/usr/local/bin/crabbox-ready >/dev/null 2>&1 || { echo 'guest not ready' >&2; exit 1; }
df -B1 --output=size,avail / | tail -n 1
head -n 1 /proc/stat
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo
nproc";

/// Guest command growing the root partition, then its ext4 filesystem, to fill
/// the disk; prints the new filesystem size. Safe to run when nothing changed.
pub const GROW_FS_SCRIPT: &str = r#"set -eu
src=$(findmnt -no SOURCE /)
disk=/dev/$(lsblk -no PKNAME "$src")
part=$(lsblk -no PARTN "$src" | tr -d ' ')
if ! out=$(sudo growpart "$disk" "$part" 2>&1); then
  case $out in *NOCHANGE*) ;; *) echo "growpart: $out" | tr '\n' ' ' >&2; exit 1 ;; esac
fi
out=$(sudo resize2fs "$src" 2>&1) || { echo "resize2fs: $out" | tr '\n' ' ' >&2; exit 1; }
df -B1 --output=size / | tail -n 1"#;

/// What the watcher reads at start, written next to the lease record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSpec {
    /// Limits and triggers.
    pub growth: Growth,
    /// ssh client binary.
    pub ssh: PathBuf,
}

/// What the watcher needs from the VMM, the host and the guest; tests fake it.
pub trait Machine {
    /// True while the lease's cloud-hypervisor process lives.
    fn alive(&self) -> bool;
    /// True while the VM runs (not paused).
    fn running(&self) -> bool;
    /// Sizes the VMM currently gives the guest.
    fn current(&self) -> Result<Current, String>;
    /// One reading of the guest.
    fn sample(&self) -> Result<Sample, String>;
    /// Bytes free on the host filesystem that holds the disk image.
    fn host_free(&self) -> Result<u64, String>;
    /// Records why the disk cannot grow (`None` clears it) for the lease status.
    fn set_blocked(&self, reason: Option<&str>);
    /// Grows the root disk image to `bytes` while the VM runs.
    fn grow_disk(&self, bytes: u64) -> Result<(), String>;
    /// Grows the root partition and filesystem; returns the new fs size, bytes.
    fn grow_fs(&self) -> Result<u64, String>;
    /// Sets the vCPU count or memory size.
    fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String>;
}

/// Bytes as GiB with one decimal.
fn gib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / GIB;
    format!("{}.{} GiB", tenths / 10, tenths % 10)
}

/// Samples once and applies what the planner decides, logging each change.
/// `Err` only when the guest or VMM cannot be read.
pub fn tick(
    planner: &mut Planner,
    machine: &dyn Machine,
    settle: Duration,
    log: &mut Log<'_>,
) -> Result<(), String> {
    let current = machine.current()?;
    let sample = machine.sample()?;
    for action in planner.plan(&sample, &current) {
        match action {
            Action::GrowDisk { from, to } => grow_disk(machine, from, to, log),
            Action::GrowFs => match machine.grow_fs() {
                Ok(size) => {
                    planner.fs_grown(true);
                    log.note(&format!("root filesystem fills the disk: {}", gib(size)));
                }
                Err(e) => {
                    planner.fs_grown(false);
                    log.fail(format!("grow failed: root filesystem: {e}"));
                }
            },
            Action::DiskAtMax { max } => log.note(&format!(
                "root filesystem low on space ({} free) and disk at diskMaxGiB ({}); not growing",
                gib(sample.fs_avail),
                gib(max)
            )),
            Action::RaiseCpus { from, to } => raise_cpus(machine, from, to, settle, log),
            Action::RaiseMemory { from, to } => raise_memory(machine, from, to, settle, log),
        }
    }
    Ok(())
}

/// Grows the disk image when the host filesystem has room for the whole step;
/// a sparse grow past the host's free space would fail later as guest I/O errors.
fn grow_disk(machine: &dyn Machine, from: u64, to: u64, log: &mut Log<'_>) {
    let need = to.saturating_sub(from);
    match machine.host_free() {
        Ok(free) if free >= need => machine.set_blocked(None),
        Ok(_) => {
            let reason = format!(
                "host filesystem holding the disk has under {} free; disk stays at {}",
                gib(need),
                gib(from)
            );
            machine.set_blocked(Some(&reason));
            log.fail(format!("grow failed: {reason}"));
            return;
        }
        Err(e) => {
            log.fail(format!("grow failed: cannot read host free space: {e}"));
            return;
        }
    }
    match machine.grow_disk(to) {
        Ok(()) => log.note(&format!("grew disk {} -> {}", gib(from), gib(to))),
        Err(e) => log.fail(format!(
            "grow failed: disk {} -> {}: {e}",
            gib(from),
            gib(to)
        )),
    }
}

/// Raises vCPUs and checks the guest brought them online.
fn raise_cpus(machine: &dyn Machine, from: u32, to: u32, settle: Duration, log: &mut Log<'_>) {
    if let Err(e) = machine.resize(Some(to), None) {
        log.fail(format!("grow failed: vCPUs {from} -> {to}: {e}"));
        return;
    }
    std::thread::sleep(settle);
    match machine.sample() {
        Ok(s) if s.online_cpus >= to => log.note(&format!(
            "raised vCPUs {from} -> {to}; guest sees {}",
            s.online_cpus
        )),
        Ok(s) => log.fail(format!(
            "grow failed: raised vCPUs {from} -> {to} but the guest has {} online",
            s.online_cpus
        )),
        Err(e) => log.note(&format!(
            "raised vCPUs {from} -> {to}; cannot read the guest: {e}"
        )),
    }
}

/// Raises memory and reports what the guest now sees.
fn raise_memory(machine: &dyn Machine, from: u64, to: u64, settle: Duration, log: &mut Log<'_>) {
    let (from_mib, to_mib) = (from / MIB, to / MIB);
    if let Err(e) = machine.resize(None, Some(to)) {
        log.fail(format!(
            "grow failed: memory {from_mib} -> {to_mib} MiB: {e}"
        ));
        return;
    }
    std::thread::sleep(settle);
    let seen = machine.sample().map_or_else(
        |e| format!("cannot read the guest: {e}"),
        |s| format!("guest MemTotal {} MiB", s.mem_total / MIB),
    );
    log.note(&format!("raised memory {from_mib} -> {to_mib} MiB; {seen}"));
}

/// Parses [`SAMPLE_SCRIPT`] output.
pub fn parse_sample(text: &str) -> Result<Sample, String> {
    let bad = |what: &str| format!("guest sample: cannot read {what}");
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let numbers = |line: &str| -> Vec<u64> {
        line.split_whitespace()
            .filter_map(|w| w.parse().ok())
            .collect()
    };
    let df = lines.first().map(|l| numbers(l)).unwrap_or_default();
    let (Some(&fs_size), Some(&fs_avail)) = (df.first(), df.get(1)) else {
        return Err(bad("root filesystem size"));
    };
    let cpu_line = lines
        .iter()
        .find(|l| l.starts_with("cpu "))
        .ok_or_else(|| bad("cpu"))?;
    let ticks: Vec<u64> = numbers(cpu_line).into_iter().take(8).collect();
    let idle = ticks.get(3).copied().unwrap_or(0) + ticks.get(4).copied().unwrap_or(0);
    let total: u64 = ticks.iter().sum();
    let kib = |key: &str| {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| numbers(rest).first().copied())
            .map(|n| n * 1024)
            .ok_or_else(|| bad(key))
    };
    let online_cpus = lines
        .last()
        .and_then(|l| l.parse().ok())
        .ok_or_else(|| bad("nproc"))?;
    Ok(Sample {
        fs_size,
        fs_avail,
        cpu: CpuTimes {
            busy: total.saturating_sub(idle),
            total,
        },
        mem_total: kib("MemTotal:")?,
        mem_avail: kib("MemAvailable:")?,
        online_cpus,
    })
}

/// Ticks while the VM lives, skipping while it is paused. Failures are
/// counted per distinct message; a read failure before the guest was first
/// reached (boot) is not one.
pub fn watch_loop(
    machine: &dyn Machine,
    planner: &mut Planner,
    interval: Duration,
    settle: Duration,
    log: &mut Log<'_>,
) {
    let mut reached = false;
    while machine.alive() {
        if machine.running() {
            match tick(planner, machine, settle, log) {
                Ok(()) => {
                    reached = true;
                    log.end_tick();
                }
                Err(e) if reached => log.fail(format!("grow check failed: {e}")),
                Err(_) => {}
            }
        }
        std::thread::sleep(interval);
    }
    log.finish();
    log.note("cloud-hypervisor exited; watcher stops");
}

#[cfg(test)]
mod tests;
