//! The growth watcher: one detached process per running lease.
//!
//! Every [`INTERVAL`] it samples the guest over ssh, asks the [`Planner`] what
//! to change, applies it through the cloud-hypervisor API and logs each grow
//! and each failure. It exits when the lease's cloud-hypervisor exits.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ch::ChClient;
use crate::config::Growth;
use crate::grow::{Action, CpuTimes, Current, Planner, Sample, GIB, MIB};
use crate::ready::ReadyProbe;
use crate::state::{self, LeaseRecord, Store};
use crate::vm::spec;

/// Time between samples.
pub const INTERVAL: Duration = Duration::from_secs(10);
/// Wait after a raise before reading what the guest sees.
const SETTLE: Duration = Duration::from_secs(2);
/// Longest a sample may take.
const SAMPLE_LIMIT: Duration = Duration::from_secs(30);
/// Longest a partition and filesystem grow may take.
const GROW_FS_LIMIT: Duration = Duration::from_secs(120);

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

/// What the watcher needs from the VMM and the guest; tests fake it.
pub trait Machine {
    /// Sizes the VMM currently gives the guest.
    fn current(&self) -> Result<Current, String>;
    /// One reading of the guest.
    fn sample(&self) -> Result<Sample, String>;
    /// Grows the root disk image to `bytes` while the VM runs.
    fn grow_disk(&self, bytes: u64) -> Result<(), String>;
    /// Grows the root partition and filesystem; returns the new fs size, bytes.
    fn grow_fs(&self) -> Result<u64, String>;
    /// Sets the vCPU count or memory size.
    fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String>;
}

/// Writes one timestamped log line.
fn note(log: &mut dyn Write, message: &str) {
    let _ = writeln!(log, "{} {message}", state::now_unix());
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
    log: &mut dyn Write,
) -> Result<(), String> {
    let current = machine.current()?;
    let sample = machine.sample()?;
    for action in planner.plan(&sample, &current) {
        match action {
            Action::GrowDisk { from, to } => match machine.grow_disk(to) {
                Ok(()) => note(log, &format!("grew disk {} -> {}", gib(from), gib(to))),
                Err(e) => note(
                    log,
                    &format!("grow failed: disk {} -> {}: {e}", gib(from), gib(to)),
                ),
            },
            Action::GrowFs => match machine.grow_fs() {
                Ok(size) => {
                    planner.fs_grown(true);
                    note(log, &format!("root filesystem fills the disk: {}", gib(size)));
                }
                Err(e) => {
                    planner.fs_grown(false);
                    note(log, &format!("grow failed: root filesystem: {e}"));
                }
            },
            Action::DiskAtMax { max } => note(
                log,
                &format!(
                    "root filesystem low on space ({} free) and disk at diskMaxGiB ({}); not growing",
                    gib(sample.fs_avail),
                    gib(max)
                ),
            ),
            Action::RaiseCpus { from, to } => raise_cpus(machine, from, to, settle, log),
            Action::RaiseMemory { from, to } => raise_memory(machine, from, to, settle, log),
        }
    }
    Ok(())
}

/// Raises vCPUs and checks the guest brought them online.
fn raise_cpus(machine: &dyn Machine, from: u32, to: u32, settle: Duration, log: &mut dyn Write) {
    if let Err(e) = machine.resize(Some(to), None) {
        note(log, &format!("grow failed: vCPUs {from} -> {to}: {e}"));
        return;
    }
    std::thread::sleep(settle);
    match machine.sample() {
        Ok(s) if s.online_cpus >= to => {
            note(
                log,
                &format!("raised vCPUs {from} -> {to}; guest sees {}", s.online_cpus),
            );
        }
        Ok(s) => note(
            log,
            &format!(
                "grow failed: raised vCPUs {from} -> {to} but the guest has {} online",
                s.online_cpus
            ),
        ),
        Err(e) => note(
            log,
            &format!("raised vCPUs {from} -> {to}; cannot read the guest: {e}"),
        ),
    }
}

/// Raises memory and reports what the guest now sees.
fn raise_memory(machine: &dyn Machine, from: u64, to: u64, settle: Duration, log: &mut dyn Write) {
    let (from_mib, to_mib) = (from / MIB, to / MIB);
    if let Err(e) = machine.resize(None, Some(to)) {
        note(
            log,
            &format!("grow failed: memory {from_mib} -> {to_mib} MiB: {e}"),
        );
        return;
    }
    std::thread::sleep(settle);
    let seen = machine.sample().map_or_else(
        |e| format!("cannot read the guest: {e}"),
        |s| format!("guest MemTotal {} MiB", s.mem_total / MIB),
    );
    note(
        log,
        &format!("raised memory {from_mib} -> {to_mib} MiB; {seen}"),
    );
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

/// The live VMM and guest of one lease.
struct RealMachine {
    /// cloud-hypervisor API.
    client: ChClient,
    /// ssh into the guest.
    ssh: ReadyProbe,
    /// Root disk image.
    disk: PathBuf,
}

impl Machine for RealMachine {
    fn current(&self) -> Result<Current, String> {
        let info = self.client.info()?;
        let field = |path: &str| {
            info.pointer(path)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("cloud-hypervisor vm.info: missing {path}"))
        };
        Ok(Current {
            disk_bytes: fs::metadata(&self.disk)
                .map_err(|e| format!("stat root disk: {e}"))?
                .len(),
            vcpus: u32::try_from(field("/config/cpus/boot_vcpus")?)
                .map_err(|_| "cloud-hypervisor vm.info: vcpus out of range".to_owned())?,
            memory_bytes: field("/config/memory/size")?,
        })
    }

    fn sample(&self) -> Result<Sample, String> {
        parse_sample(&self.ssh.output(SAMPLE_SCRIPT, SAMPLE_LIMIT)?)
    }

    fn grow_disk(&self, bytes: u64) -> Result<(), String> {
        self.client.resize_disk(spec::ROOT_DISK_ID, bytes)
    }

    fn grow_fs(&self) -> Result<u64, String> {
        let out = self.ssh.output(GROW_FS_SCRIPT, GROW_FS_LIMIT)?;
        out.trim()
            .parse()
            .map_err(|_| "grow fs: cannot read the new size".to_owned())
    }

    fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String> {
        self.client.resize(vcpus, ram_bytes)
    }
}

/// Runs the watcher for `lease_id` until its cloud-hypervisor exits.
pub fn run(lease_id: &str, state_dir: &Path, log: &mut dyn Write) -> Result<(), String> {
    state::validate_lease_id(lease_id)?;
    let store = Store::new(state_dir);
    let record = store
        .read(lease_id)?
        .ok_or_else(|| format!("lease {lease_id} not found"))?;
    let text =
        fs::read(store.watch_spec_path(lease_id)).map_err(|e| format!("read watch spec: {e}"))?;
    let watch: WatchSpec =
        serde_json::from_slice(&text).map_err(|e| format!("parse watch spec: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate adapter binary: {e}"))?;
    let machine = RealMachine {
        client: ChClient::new(&record.api_socket()),
        ssh: spec::guest_ssh(&watch.ssh, &store, &exe, state_dir, lease_id),
        disk: store.disk_path(lease_id),
    };
    let g = watch.growth;
    note(
        log,
        &format!(
            "watching {lease_id}: disk up to {} GiB, vCPUs up to {}, memory up to {} MiB",
            g.disk_max_gib, g.cpus_max, g.memory_mib_max
        ),
    );
    watch_loop(&record, &machine, &mut Planner::new(g), log);
    Ok(())
}

/// Ticks while the VM lives; a read failure is logged once until it clears.
fn watch_loop(
    record: &LeaseRecord,
    machine: &dyn Machine,
    planner: &mut Planner,
    log: &mut dyn Write,
) {
    let client = ChClient::new(&record.api_socket()).with_timeout(Duration::from_secs(5));
    let (mut reached, mut failing) = (false, None::<String>);
    while crate::vm::RealBackend::ch_alive(record) {
        if client.state().as_deref() == Ok("Running") {
            match tick(planner, machine, SETTLE, log) {
                Ok(()) => {
                    if failing.take().is_some() && reached {
                        note(log, "guest readable again");
                    }
                    reached = true;
                }
                Err(e) => {
                    if reached && failing.as_ref() != Some(&e) {
                        note(log, &format!("grow check failed: {e}"));
                    }
                    failing = Some(e);
                }
            }
        }
        std::thread::sleep(INTERVAL);
    }
    note(log, "cloud-hypervisor exited; watcher stops");
}

#[cfg(test)]
mod tests;
