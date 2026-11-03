//! Checkpoints: a snapshot of a running lease kept under a caller-chosen id.
//!
//! A checkpoint has the layout of a boot template (snapshot, disk, login key,
//! host pin), so a fork restores and rotates identity through the same code.
//! The lease's own login key plays the bootstrap key.

#[cfg(test)]
mod tests;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::template::restore::Origin;
use super::template::{
    ch_version, BOOTSTRAP_KEY, DISK_FILE, KNOWN_HOSTS, REQUIRED, SNAPSHOT_TIMEOUT, SNAP_DIR,
};
use super::{mkdir_private, remove_tree, suspend, RealBackend};
use crate::backend::Backend;
use crate::ch::ChClient;
use crate::host;
use crate::protocol::Desired;
use crate::ready::known_hosts_line;
use crate::size::Size;
use crate::state::{self, LeaseRecord};

/// What the checkpoint was taken from, inside its dir.
const META_FILE: &str = "checkpoint.json";
/// Prefix of a checkpoint dir in the cache dir; `.tmp` marks one being written.
const DIR_PREFIX: &str = "checkpoint-";
/// Timeout for the pause and resume calls around the snapshot.
const API_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes per block in `st_blocks`.
const BLOCK: u64 = 512;

/// What a checkpoint was taken from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Meta {
    /// The caller's id for it.
    pub(super) checkpoint_id: String,
    /// The caller's label for it.
    pub(super) name: String,
    /// Lease it was taken from.
    pub(super) lease_id: String,
    /// `cloud-hypervisor --version` of the binary that took the snapshot.
    pub(super) ch_version: String,
    /// Guest size.
    pub(super) size: Size,
    /// Unix time it was taken.
    pub(super) created_at: u64,
}

/// A checkpoint id that is safe as a directory name.
fn check_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err("invalid checkpoint id".to_owned())
    }
}

/// Bytes allocated on disk by every file under `dir`.
fn dir_bytes(dir: &Path) -> Result<u64, String> {
    let mut total = 0;
    let entries = fs::read_dir(dir).map_err(|e| format!("size {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let meta = entry
            .metadata()
            .map_err(|e| format!("size {}: {e}", entry.path().display()))?;
        total += if meta.is_dir() {
            dir_bytes(&entry.path())?
        } else {
            meta.blocks() * BLOCK
        };
    }
    Ok(total)
}

impl RealBackend {
    /// Where checkpoint `id` lives.
    pub(super) fn checkpoint_dir(&self, id: &str) -> PathBuf {
        self.config.cache_dir.join(format!("{DIR_PREFIX}{id}"))
    }

    /// Snapshots the running lease under `id`, resumes it, and returns the
    /// checkpoint's bytes on disk. An older checkpoint of that id is replaced.
    pub(super) fn take_checkpoint(
        &self,
        record: &LeaseRecord,
        id: &str,
        name: &str,
        diag: &mut dyn Write,
    ) -> Result<u64, String> {
        check_id(id)?;
        let lease = record.lease_id.as_str();
        if self.read_cache(lease).has_disks() {
            return Err(format!(
                "lease {lease} has cache volumes attached; a checkpoint cannot hold them"
            ));
        }
        let size = record.size.unwrap_or_else(|| self.default_size());
        let cache = &self.config.cache_dir;
        mkdir_private(cache)?;
        suspend::check_room(suspend::free_bytes(cache)?, size.memory_mib)?;
        let meta = Meta {
            checkpoint_id: id.to_owned(),
            name: name.to_owned(),
            lease_id: lease.to_owned(),
            ch_version: ch_version(&self.config.cloud_hypervisor)?,
            size,
            created_at: state::now_unix(),
        };
        let pin = known_hosts_line(&format!("crabbox-{lease}"), &self.read_host_key(lease)?);
        let tmp = cache.join(format!("{DIR_PREFIX}{id}.tmp"));
        remove_tree(&tmp)?;
        mkdir_private(&tmp)?;
        let started = Instant::now();
        let snap = tmp.join(SNAP_DIR);
        let paused = self.pause_vm(record, diag);
        let snapped = paused
            .clone()
            .and_then(|()| Self::snapshot_into(record, &snap, diag));
        let kept = snapped
            .clone()
            .and_then(|()| self.keep_lease_files(record, &tmp, &meta, &pin));
        // cloud-hypervisor stops a vhost-user backend's rings to save its state
        // and never restarts them on vm.resume, so the guest NIC would stay
        // dead. A restore from the snapshot sets the rings up again.
        let resumed = match (&paused, &snapped) {
            (Err(_), _) => Ok(()),
            (Ok(()), Ok(())) => Self::stop_processes(record, diag)
                .and_then(|()| self.restore_snapshot(record, &snap, false, diag))
                .map_err(|e| {
                    format!("lease {lease} did not restart from its snapshot, resume it: {e}")
                }),
            (Ok(()), Err(_)) => {
                let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
                client
                    .resume()
                    .and_then(|()| self.wait_state(&client, "Running"))
                    .map_err(|e| format!("lease {lease} is still paused, resume it: {e}"))
            }
        };
        let written = kept.and(resumed);
        let published = written.and_then(|()| {
            let dir = self.checkpoint_dir(id);
            remove_tree(&dir)?;
            fs::rename(&tmp, &dir).map_err(|e| format!("publish checkpoint: {e}"))?;
            dir_bytes(&dir)
        });
        if published.is_err() {
            let _ = remove_tree(&tmp);
        }
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(diag, "checkpoint {id} written in {secs:.1}s");
        published
    }

    /// Writes the paused lease's snapshot into `snap`.
    fn snapshot_into(
        record: &LeaseRecord,
        snap: &Path,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let lease = record.lease_id.as_str();
        mkdir_private(snap)?;
        ChClient::new(&record.api_socket())
            .with_timeout(SNAPSHOT_TIMEOUT)
            .snapshot(snap)
            .map_err(|e| format!("lease {lease}: snapshot failed: {e}"))?;
        suspend::dig_snapshot_holes(Path::new(host::DIG_HOLES_TOOL), snap, diag);
        Ok(())
    }

    /// Writes the paused lease's disk, login key, host pin and `meta` into `dir`.
    fn keep_lease_files(
        &self,
        record: &LeaseRecord,
        dir: &Path,
        meta: &Meta,
        pin: &str,
    ) -> Result<(), String> {
        let lease = record.lease_id.as_str();
        host::clone_disk(&self.store.disk_path(lease), &dir.join(DISK_FILE), 0)?;
        fs::copy(self.store.key_path(lease), dir.join(BOOTSTRAP_KEY))
            .map_err(|e| format!("keep login key: {e}"))?;
        fs::write(dir.join(KNOWN_HOSTS), pin).map_err(|e| format!("keep host pin: {e}"))?;
        let json = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
        fs::write(dir.join(META_FILE), json).map_err(|e| format!("write {META_FILE}: {e}"))
    }

    /// The checkpoint's meta once it is known to be complete and usable here.
    fn usable_checkpoint(&self, id: &str, dir: &Path) -> Result<Meta, String> {
        if !dir.is_dir() {
            return Err(format!("checkpoint not found: {id}"));
        }
        let meta: Meta = fs::read(dir.join(META_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| format!("checkpoint {id} is unreadable: its {META_FILE} is bad"))?;
        if let Some(missing) = REQUIRED.iter().find(|f| !dir.join(f).exists()) {
            return Err(format!(
                "checkpoint {id} is incomplete: it has no {missing}"
            ));
        }
        let running = ch_version(&self.config.cloud_hypervisor)?;
        if meta.ch_version != running {
            return Err(format!(
                "checkpoint {id} was taken by {}, running {running}",
                meta.ch_version
            ));
        }
        Ok(meta)
    }

    /// Starts the lease in `desired` from checkpoint `id`, running, with its
    /// own keys, hostname and machine id. A lease that fails is destroyed.
    pub(super) fn fork_checkpoint(
        &self,
        desired: &Desired,
        id: &str,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        check_id(id)?;
        let dir = self.checkpoint_dir(id);
        let meta = self.usable_checkpoint(id, &dir)?;
        let lease = desired.lease_id.as_str();
        state::validate_lease_id(lease)?;
        let run_dir = self.config.runtime_dir.join(state::run_dir_name(lease));
        super::spec::check_socket_len(&run_dir)?;
        self.scopes().check_bus()?;
        let size = self.lease_size(desired.class.as_deref())?;
        if size != meta.size {
            return Err(format!(
                "checkpoint {id} was taken at {} cpus and {} MiB; this lease would get {} cpus and {} MiB",
                meta.size.cpus, meta.size.memory_mib, size.cpus, size.memory_mib
            ));
        }
        let label = format!("checkpoint {id}");
        let origin = Origin {
            dir: &dir,
            label: &label,
        };
        let alias = format!("crabbox-{}", meta.lease_id);
        let restored = self
            .restore_from(desired, &run_dir, &origin, size, diag)
            .and_then(|record| self.take_identity_from(record, &dir, alias, diag));
        if restored.is_err() {
            if let Err(e) = self.destroy(lease, diag) {
                let _ = writeln!(diag, "cleanup after failed fork also failed: {e}");
            }
        }
        restored
    }

    /// Removes checkpoint `id` and any half-written copy; an unknown id is fine.
    pub(super) fn remove_checkpoint(&self, id: &str) -> Result<(), String> {
        check_id(id)?;
        remove_tree(&self.config.cache_dir.join(format!("{DIR_PREFIX}{id}.tmp")))?;
        remove_tree(&self.checkpoint_dir(id))
    }
}
