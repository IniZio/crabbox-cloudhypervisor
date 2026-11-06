//! Suspend to disk: snapshot a paused VM into its lease dir, stop its
//! processes, and restore it later with its guest processes intact.

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{mkdir_private, remove_tree, RealBackend};
use crate::backend::Power;
use crate::ch::ChClient;
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
/// File in a snapshot dir that holds guest RAM.
const MEMORY_FILE: &str = "memory-ranges";
/// Host memory a restored VM holds beyond its snapshot's allocated bytes, in
/// MiB; 20 was measured for cloud-hypervisor, passt and page tables.
const RESUME_OVERHEAD_MIB: u64 = 64;

/// Makes the snapshot's memory file sparse so only non-zero pages stay on disk.
/// A failure leaves the snapshot dense and is only noted: the snapshot is valid.
pub(super) fn dig_snapshot_holes(program: &Path, snap: &Path, diag: &mut dyn Write) {
    let memory = snap.join(MEMORY_FILE);
    if let Err(e) = crate::host::dig_holes(program, &memory) {
        let _ = writeln!(diag, "warn: snapshot kept dense: {e}");
    }
}

/// Fails unless `free` bytes hold a snapshot of `memory_mib` plus [`SPARE_BYTES`].
pub(super) fn check_room(free: u64, memory_mib: u64) -> Result<(), String> {
    let need = memory_mib.saturating_mul(MIB).saturating_add(SPARE_BYTES);
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "not enough free disk for a snapshot: need {} MiB, have {} MiB; pause aborted",
        need / MIB,
        free / MIB
    ))
}

/// Bytes available to this user on the filesystem holding `path`.
pub(super) fn free_bytes(path: &Path) -> Result<u64, String> {
    let st = rustix::fs::statvfs(path).map_err(|e| format!("statvfs state dir: {e}"))?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

impl RealBackend {
    /// Where a suspended lease keeps its snapshot.
    pub(super) fn snapshot_dir(&self, lease_id: &str) -> PathBuf {
        self.store.lease_dir(lease_id).join(SNAPSHOT_DIR)
    }

    /// Allocated bytes count because restore loads only those, not the holes.
    pub(super) fn snapshot_resume_mib(&self, lease_id: &str) -> Option<u64> {
        let memory = fs::metadata(self.snapshot_dir(lease_id).join(MEMORY_FILE)).ok()?;
        let allocated = memory.blocks().saturating_mul(512);
        Some(allocated.div_ceil(MIB).saturating_add(RESUME_OVERHEAD_MIB))
    }

    /// Live power, with a dead VM that left a snapshot reported as suspended.
    pub(super) fn power_or_suspended(&self, record: &LeaseRecord) -> Power {
        match Self::power_of(record) {
            Power::Stopped if self.snapshot_dir(&record.lease_id).is_dir() => Power::Suspended,
            power => power,
        }
    }

    /// Snapshots the VM to disk, then stops cloud-hypervisor and passt.
    /// A VM that was running keeps running if the snapshot cannot be taken;
    /// one found paused (by an older adapter) is left as it was found.
    pub(super) fn suspend(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let was_running = Self::power_of(record) == Power::Running;
        if was_running {
            self.pause_vm(record, diag)?;
        }
        let result = self.snapshot_and_stop(record, diag);
        if result.is_err() && was_running && Self::ch_alive(record) {
            let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
            if let Err(e) = client.resume() {
                let _ = writeln!(diag, "warn: resume after failed pause: {e}");
            }
        }
        result
    }

    /// Writes the snapshot of a paused VM and stops its processes.
    /// On failure the snapshot is removed and the VM is still alive.
    fn snapshot_and_stop(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let id = &record.lease_id;
        let size = self.record_size(record)?;
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
            return Err(format!("lease {id}: snapshot failed, pause aborted: {e}"));
        }
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(diag, "snapshot written in {secs:.1}s");
        if let Err(e) = Self::stop_processes(record, diag) {
            if Self::ch_alive(record) {
                let _ = remove_tree(&snap);
                return Err(format!(
                    "lease {id}: cloud-hypervisor outlived the snapshot, pause aborted: {e}"
                ));
            }
            let _ = writeln!(diag, "warn: {e}");
        }
        let scopes = self.scopes().stop(id, diag);
        // Last, so the guest's RAM is back with the host while this reads the file.
        dig_snapshot_holes(Path::new(crate::host::DIG_HOLES_TOOL), &snap, diag);
        scopes?;
        let _ = writeln!(diag, "vm suspended to disk");
        Ok(())
    }

    /// Starts cloud-hypervisor from the lease's snapshot and resumes it.
    /// A failed restore keeps the snapshot and returns the error.
    pub(super) fn restore(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let id = record.lease_id.clone();
        let snap = self.snapshot_dir(&id);
        let mut record = self.relaunch_prep(record, diag)?;
        let size = self.record_size(&record)?;
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
