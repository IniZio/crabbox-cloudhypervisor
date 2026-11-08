//! The growth watcher: one detached process per running lease.
//!
//! Every [`INTERVAL`] it samples the guest over ssh, asks the [`Planner`] what
//! to change, applies it through the cloud-hypervisor API and logs each grow
//! and each failure. It exits when the lease's cloud-hypervisor exits.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::balloon::{BalloonConfig, Inflator};
use crate::config::Growth;
use crate::grow::{Action, CacheAction, CacheFs, Current, Planner, Sample, GIB};
use crate::volumes::{Token, MOUNT_ROOT};

mod log;
mod real;

pub use log::Log;
pub use real::run;

/// Time between samples.
pub const INTERVAL: Duration = Duration::from_secs(10);

/// Guest command printing root fs size and free bytes.
pub const SAMPLE_SCRIPT: &str =
    "/usr/local/bin/crabbox-ready >/dev/null 2>&1 || { echo 'guest not ready' >&2; exit 1; }
df -B1 --output=size,avail / | tail -n 1";

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

/// Guest command printing, per mounted cache disk, its mount point, block
/// count, block size, free bytes and disk bytes. Numbers pass through as text.
pub const CACHE_SAMPLE_SCRIPT: &str = r#"for m in /var/lib/crabbox-cache/*; do
  mountpoint -q "$m" || continue
  src=$(findmnt -no SOURCE "$m")
  sudo dumpe2fs -h "$src" 2>/dev/null | awk -F: -v m="$m" \
    -v a="$(df -B1 --output=avail "$m" | tail -n 1)" -v d="$(lsblk -bdno SIZE "$src")" \
    '/^Block count:/{c=$2} /^Block size:/{s=$2} END{print m, c, s, a, d}'
done"#;

/// Guest command growing the ext4 filesystem at `mount` to `bytes`, rounded
/// down to whole 4 KiB blocks; prints its new block count and block size.
#[must_use]
pub fn grow_cache_script(mount: &str, bytes: u64) -> String {
    let kib = bytes / 4096 * 4;
    format!(
        r#"set -eu
src=$(findmnt -no SOURCE '{mount}')
out=$(sudo resize2fs "$src" {kib}K 2>&1) || {{ echo "resize2fs: $out" | tr '\n' ' ' >&2; exit 1; }}
sudo dumpe2fs -h "$src" 2>/dev/null | awk -F: '/^Block count:/{{c=$2}} /^Block size:/{{s=$2}} END{{print c, s}}'"#
    )
}

/// What the watcher reads at start, written next to the lease record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchSpec {
    /// Limits and triggers.
    pub growth: Growth,
    /// ssh client binary.
    pub ssh: PathBuf,
    /// Balloon settings.
    pub balloon: BalloonConfig,
    /// The lease's class memory, MiB.
    pub memory_mib: u64,
    /// Host directory holding the lease's cache disks.
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
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
    /// Free and total bytes of the host filesystem that holds the disk image,
    /// read now.
    fn host_space(&self) -> Result<(u64, u64), String>;
    /// Records why the disk cannot grow (`None` clears it) for the lease status.
    fn set_blocked(&self, reason: Option<&str>);
    /// Grows the root disk image to `bytes` while the VM runs.
    fn grow_disk(&self, bytes: u64) -> Result<(), String>;
    /// Grows the root partition and filesystem; returns the new fs size, bytes.
    fn grow_fs(&self) -> Result<u64, String>;
    /// The cache filesystems mounted in the guest.
    fn cache_samples(&self) -> Result<Vec<CacheFs>, String>;
    /// Free and total bytes of the host filesystem that holds the cache disks, read now.
    fn cache_host_space(&self) -> Result<(u64, u64), String>;
    /// Grows the cache filesystem at `mount` to `bytes`; returns its new size, bytes.
    fn grow_cache(&self, mount: &str, bytes: u64) -> Result<u64, String>;
    /// Blocks up to `timeout`; true when host memory pressure fired. Without a
    /// pressure feed it only sleeps and returns false.
    fn wait(&self, timeout: Duration) -> Result<bool, String>;
    /// Asks the VMM to hold `bytes` of guest memory in the balloon.
    fn set_balloon(&self, bytes: u64) -> Result<(), String>;
}

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;

/// Bytes as GiB with one decimal.
fn gib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / GIB;
    format!("{}.{} GiB", tenths / 10, tenths % 10)
}

/// Samples once and applies what the planner decides, logging each change.
/// `Err` only when the guest or VMM cannot be read.
pub fn tick(planner: &mut Planner, machine: &dyn Machine, log: &mut Log<'_>) -> Result<(), String> {
    let current = machine.current()?;
    let sample = machine.sample()?;
    for action in planner.plan(&sample, current) {
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
        }
    }
    for action in planner.plan_caches(&machine.cache_samples()?) {
        match action {
            CacheAction::Grow { mount, from, to } => grow_cache(machine, &mount, from, to, log),
            CacheAction::AtDisk { mount, avail } => log.note(&format!(
                "cache filesystem {mount} low on space ({} free) and fills its disk; not growing",
                gib(avail)
            )),
        }
    }
    Ok(())
}

/// Grows a cache filesystem when the host keeps its free floor after the step.
fn grow_cache(machine: &dyn Machine, mount: &str, from: u64, to: u64, log: &mut Log<'_>) {
    let need = to.saturating_sub(from);
    match machine.cache_host_space() {
        Ok((free, total)) => {
            let floor = crate::host::host_floor(total, HOST_FLOOR_GIB);
            if free.saturating_sub(need) < floor {
                log.fail(format!(
                    "grow failed: host filesystem holding cache {mount} has {} free; a {} grow must leave its floor of {}; cache stays at {}",
                    gib(free),
                    gib(need),
                    gib(floor),
                    gib(from)
                ));
                return;
            }
        }
        Err(e) => {
            log.fail(format!("grow failed: cannot read host free space: {e}"));
            return;
        }
    }
    match machine.grow_cache(mount, to) {
        Ok(size) => log.note(&format!(
            "grew cache filesystem {mount} {} -> {}",
            gib(from),
            gib(size)
        )),
        Err(e) => log.fail(format!("grow failed: cache filesystem {mount}: {e}")),
    }
}

/// Parses [`CACHE_SAMPLE_SCRIPT`] output. A mount outside the cache root or a
/// name that is not a group token is refused, since it reaches a guest command.
pub fn parse_cache_samples(text: &str) -> Result<Vec<CacheFs>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let bad = || format!("cache sample: cannot read {line:?}");
            let words: Vec<&str> = line.split_whitespace().collect();
            let [mount, count, block, avail, disk] = words.as_slice() else {
                return Err(bad());
            };
            let name = mount
                .strip_prefix(MOUNT_ROOT)
                .and_then(|r| r.strip_prefix('/'))
                .ok_or_else(|| format!("cache sample: {mount} is not under {MOUNT_ROOT}"))?;
            Token::parse(name).map_err(|e| format!("cache sample: {name} {e}"))?;
            let num = |w: &str| w.parse::<u64>().map_err(|_| bad());
            Ok(CacheFs {
                mount: (*mount).to_owned(),
                fs_size: num(count)?.saturating_mul(num(block)?),
                fs_avail: num(avail)?,
                disk_bytes: num(disk)?,
            })
        })
        .collect()
}

/// Parses `<block count> <block size>` into bytes.
pub fn parse_fs_bytes(text: &str) -> Result<u64, String> {
    let nums: Vec<u64> = text
        .split_whitespace()
        .filter_map(|w| w.parse().ok())
        .collect();
    match nums.as_slice() {
        [count, block] => Ok(count.saturating_mul(*block)),
        _ => Err(format!("cannot read the new size from {:?}", text.trim())),
    }
}

/// Smallest host free-space floor, GiB.
pub const HOST_FLOOR_GIB: u64 = 20;

/// Grows the disk image only when the host filesystem would still hold its free
/// floor after the whole step is written. Free space is read now because the
/// image is sparse and other Boxes may have filled the host since the last tick.
fn grow_disk(machine: &dyn Machine, from: u64, to: u64, log: &mut Log<'_>) {
    let need = to.saturating_sub(from);
    match machine.host_space() {
        Ok((free, total)) => {
            let floor = crate::host::host_floor(total, HOST_FLOOR_GIB);
            if free.saturating_sub(need) >= floor {
                machine.set_blocked(None);
            } else {
                let reason = format!(
                    "host filesystem holding the disk has {} free; a {} grow must leave its floor of {}; disk stays at {}",
                    gib(free),
                    gib(need),
                    gib(floor),
                    gib(from)
                );
                machine.set_blocked(Some(&reason));
                log.fail(format!("grow failed: {reason}"));
                return;
            }
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
    Ok(Sample { fs_size, fs_avail })
}

/// Runs the watcher's loop.
///
/// Waits for host memory pressure or `interval`, ticks the disk check once
/// per `interval` and moves the balloon on each wake. Skips paused VMs.
/// Disk failures are counted per distinct message; a read failure before the
/// guest was first reached (boot) is not one.
pub fn watch_loop(
    machine: &dyn Machine,
    planner: &mut Planner,
    mut balloon: Option<Inflator>,
    interval: Duration,
    log: &mut Log<'_>,
) {
    let mut reached = false;
    let mut last_disk: Option<Instant> = None;
    let start = Instant::now();
    while machine.alive() {
        if machine.running() && last_disk.is_none_or(|t| t.elapsed() >= interval) {
            last_disk = Some(Instant::now());
            match tick(planner, machine, log) {
                Ok(()) => {
                    reached = true;
                    log.end_tick();
                }
                Err(e) if reached => log.fail(format!("grow check failed: {e}")),
                Err(_) => {}
            }
        }
        let woke = match machine.wait(interval) {
            Ok(woke) => woke,
            Err(e) => {
                log.fail(format!("balloon off: {e}"));
                balloon = None;
                std::thread::sleep(interval);
                false
            }
        };
        if let Some(b) = balloon.as_mut() {
            if machine.running() {
                move_balloon(machine, b, woke, start.elapsed().as_secs(), log);
            }
        }
    }
    log.finish();
    log.note("cloud-hypervisor exited; watcher stops");
}

/// Applies one pressure event or quiet tick to the balloon and logs a change.
fn move_balloon(machine: &dyn Machine, b: &mut Inflator, event: bool, now: u64, log: &mut Log<'_>) {
    let before = b.target();
    let next = if event { b.event(now) } else { b.quiet(now) };
    let Some(bytes) = next else { return };
    match machine.set_balloon(bytes) {
        Ok(()) => log.note(&format!(
            "balloon {} -> {} MiB ({})",
            before / MIB,
            bytes / MIB,
            if event {
                "host memory pressure"
            } else {
                "quiet"
            }
        )),
        Err(e) => {
            b.revert(before);
            log.fail(format!("balloon resize failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests;
