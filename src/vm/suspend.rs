//! Suspend to disk: snapshot a paused VM into its lease dir, stop its
//! processes, and restore it later with its guest processes intact.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{mkdir_private, remove_tree, RealBackend};
use crate::backend::Power;
use crate::ch::ChClient;
use crate::config::PauseMode;
use crate::state::LeaseRecord;

/// Snapshot of a suspended VM, inside the lease dir.
const SNAPSHOT_DIR: &str = "snap";
/// Snapshot being written; renamed to [`SNAPSHOT_DIR`] only once complete.
const SNAPSHOT_TMP: &str = "snap.tmp";
/// Timeout for `vm.snapshot`, which writes all of guest RAM.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);
/// Timeout for each state query or power call around a restore.
const API_TIMEOUT: Duration = Duration::from_secs(2);
/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;
/// Free space a snapshot must leave on the state filesystem.
const SPARE_BYTES: u64 = 1024 * MIB;

/// Fails unless `free` bytes hold a snapshot of `memory_mib` plus [`SPARE_BYTES`].
pub(super) fn check_room(free: u64, memory_mib: u64) -> Result<(), String> {
    let need = memory_mib.saturating_mul(MIB).saturating_add(SPARE_BYTES);
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "not enough free disk for a snapshot: need {} MiB, have {} MiB; vm left paused in memory",
        need / MIB,
        free / MIB
    ))
}

/// Bytes available to this user on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Result<u64, String> {
    let st = rustix::fs::statvfs(path).map_err(|e| format!("statvfs state dir: {e}"))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

impl RealBackend {
    /// Where a suspended lease keeps its snapshot.
    pub(super) fn snapshot_dir(&self, lease_id: &str) -> PathBuf {
        self.store.lease_dir(lease_id).join(SNAPSHOT_DIR)
    }

    /// Live power, with a dead VM that left a snapshot reported as suspended.
    pub(super) fn power_or_suspended(&self, record: &LeaseRecord) -> Power {
        match Self::power_of(record) {
            Power::Stopped if self.snapshot_dir(&record.lease_id).is_dir() => Power::Suspended,
            power => power,
        }
    }

    /// Pauses in RAM, or suspends to disk, per the configured pause mode.
    pub(super) fn pause_per_mode(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        match self.config.pause {
            PauseMode::Memory => self.pause_vm(record, diag),
            PauseMode::Disk => self.suspend(record, diag),
        }
    }

    /// Pauses a running VM, snapshots it, then stops cloud-hypervisor and passt.
    /// Any failure before the processes stop leaves the VM paused in memory.
    pub(super) fn suspend(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let id = &record.lease_id;
        if Self::power_of(record) == Power::Running {
            self.pause_vm(record, diag)?;
        }
        let size = record.size.unwrap_or_else(|| self.default_size());
        let lease_dir = self.store.lease_dir(id);
        check_room(free_bytes(&lease_dir)?, size.memory_mib)?;
        let tmp = lease_dir.join(SNAPSHOT_TMP);
        remove_tree(&tmp)?;
        mkdir_private(&tmp)?;
        let started = Instant::now();
        let client = ChClient::new(&record.api_socket()).with_timeout(SNAPSHOT_TIMEOUT);
        let snap = self.snapshot_dir(id);
        let written = client.snapshot(&tmp).and_then(|()| {
            remove_tree(&snap)?;
            fs::rename(&tmp, &snap).map_err(|e| format!("rename snapshot: {e}"))
        });
        if let Err(e) = written {
            let _ = remove_tree(&tmp);
            return Err(format!(
                "lease {id}: snapshot failed, vm left paused in memory: {e}"
            ));
        }
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(diag, "snapshot written in {secs:.1}s");
        if let Err(e) = Self::stop_processes(record, diag) {
            if Self::ch_alive(record) {
                let _ = remove_tree(&snap);
                return Err(format!(
                    "lease {id}: cloud-hypervisor outlived the snapshot, vm left paused in memory: {e}"
                ));
            }
            let _ = writeln!(diag, "warn: {e}");
        }
        self.scopes().stop(id, diag)?;
        let _ = writeln!(diag, "vm suspended to disk");
        Ok(())
    }

    /// Starts cloud-hypervisor from the lease's snapshot and resumes it.
    /// A failed restore keeps the snapshot and returns the error.
    pub(super) fn restore(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let id = record.lease_id.clone();
        let snap = self.snapshot_dir(&id);
        let mut record = self.relaunch_prep(record, diag)?;
        let size = record.size.unwrap_or_else(|| self.default_size());
        let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
        let started = Instant::now();
        // Once the guest runs its disk moves past the snapshot, so the snapshot
        // must be gone before vm.resume or a later restore would corrupt it.
        let restored = self
            .start_processes(&mut record, size, Some(&snap), diag)
            .and_then(|()| self.wait_state(&client, "Paused"))
            .and_then(|()| remove_tree(&snap));
        if let Err(e) = restored {
            if let Err(stop) = Self::stop_processes(&record, diag) {
                let _ = writeln!(diag, "warn: {stop}");
            }
            return Err(format!(
                "lease {id}: restore from snapshot failed; snapshot kept: {e}"
            ));
        }
        client.resume()?;
        self.wait_state(&client, "Running")?;
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(diag, "vm restored from snapshot in {secs:.1}s");
        self.start_watcher(&mut record, diag)
    }
}

#[cfg(test)]
mod tests;
