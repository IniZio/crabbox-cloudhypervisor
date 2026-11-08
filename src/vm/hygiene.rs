//! Guest hygiene before a checkpoint or a suspend: flush and drop what the
//! guest only caches, so the snapshot's memory image and disk copy hold less.
//!
//! The guest reports free pages to the host lazily, so after the drop the
//! host's resident memory of cloud-hypervisor is polled until it stops falling.
//! Nothing here can fail a snapshot; a guest that cannot be tidied is
//! snapshotted as it is.

#[cfg(test)]
mod tests;

use std::io::Write;
use std::time::Duration;

use super::spec;
use super::RealBackend;
use crate::host;
use crate::state::LeaseRecord;

/// Guest command: flush dirty pages (deleted-but-unflushed disk blocks stay in
/// the disk copy otherwise), drop the page, dentry and inode caches, then
/// compact free memory so free-page reporting can hand whole blocks back.
pub(super) const GUEST_SCRIPT: &str =
    "sudo sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches; echo 1 > /proc/sys/vm/compact_memory'";
/// Longest the guest command may run before a checkpoint.
pub(super) const CHECKPOINT_LIMIT: Duration = Duration::from_secs(60);
/// Longest the guest command may run before a suspend, which crabbox gives two
/// minutes in all, snapshot included.
pub(super) const SUSPEND_LIMIT: Duration = Duration::from_secs(15);
/// Time between samples of the VMM's resident memory.
const POLL: Duration = Duration::from_millis(500);
/// Longest the settle wait lasts.
const CAP: Duration = Duration::from_secs(10);
/// A sample that fell by less than this since the last one counts as steady.
const STEADY_BYTES: u64 = 16 * 1024 * 1024;
/// Consecutive steady samples that end the wait. The guest reports free pages
/// in bursts about 2 s apart, so memory must hold still for longer than that.
const STEADY_RUN: u32 = 5;

/// How the wait for free-page reporting ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settled {
    /// Resident bytes at the first sample.
    pub(super) first: u64,
    /// Resident bytes at the last sample.
    pub(super) last: u64,
    /// Samples taken.
    pub(super) samples: u32,
    /// True when the cap ended the wait with memory still falling.
    pub(super) capped: bool,
}

/// Samples `resident` every `poll` until it falls by less than
/// [`STEADY_BYTES`] for [`STEADY_RUN`] samples in a row, or `cap` passes.
/// `None` when memory cannot be read.
pub(super) fn settle(
    cap: Duration,
    poll: Duration,
    mut resident: impl FnMut() -> Option<u64>,
) -> Option<Settled> {
    let first = resident()?;
    let max_samples = u32::try_from(cap.as_millis() / poll.as_millis().max(1)).unwrap_or(u32::MAX);
    let (mut last, mut samples, mut steady) = (first, 1, 0);
    while samples <= max_samples {
        std::thread::sleep(poll);
        let now = resident()?;
        samples += 1;
        steady = if last.saturating_sub(now) < STEADY_BYTES {
            steady + 1
        } else {
            0
        };
        last = now;
        if steady >= STEADY_RUN {
            return Some(Settled {
                first,
                last,
                samples,
                capped: false,
            });
        }
    }
    Some(Settled {
        first,
        last,
        samples,
        capped: true,
    })
}

/// Bytes as GiB with two decimals, for the log.
fn gib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss, reason = "a log figure")]
    let g = bytes as f64 / f64::from(1u32 << 30);
    format!("{g:.2} GiB")
}

impl RealBackend {
    /// Tidies the running guest for a snapshot, its command killed after
    /// `limit`; failures are only logged.
    pub(super) fn tidy_guest(&self, record: &LeaseRecord, limit: Duration, diag: &mut dyn Write) {
        let c = &self.config;
        let ssh = spec::guest_ssh(
            &c.ssh,
            &self.store,
            &self.exe,
            &c.state_dir,
            &record.lease_id,
        );
        if let Err(e) = ssh.output(GUEST_SCRIPT, limit) {
            let _ = writeln!(diag, "warn: guest not tidied before the snapshot: {e}");
            return;
        }
        let pid = record.ch_pid;
        match settle(CAP, POLL, || pid.and_then(host::resident_bytes)) {
            Some(s) => {
                let capped = if s.capped { " (cap reached)" } else { "" };
                let _ = writeln!(
                    diag,
                    "guest tidied: vm memory {} -> {} over {} samples{capped}",
                    gib(s.first),
                    gib(s.last),
                    s.samples
                );
            }
            None => {
                let _ = writeln!(
                    diag,
                    "warn: vm memory unreadable; not waiting for it to settle"
                );
            }
        }
    }
}
