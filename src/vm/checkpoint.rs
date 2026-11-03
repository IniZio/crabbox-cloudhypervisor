//! Checkpoints: a snapshot of a running lease kept under a caller-chosen id.
//!
//! A checkpoint has the layout of a boot template (snapshot, disk, login key,
//! host pin), so a fork restores and rotates identity through the same code.
//! The lease's own login key plays the bootstrap key. Its disk is a qcow2
//! delta over its parent checkpoint's disk; a fork flattens the chain into a
//! raw disk, so a running lease never reads through a chain.

mod chain;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use self::chain::{Backing, DELTA_FILE};
use super::template::restore::Origin;
use super::template::{
    ch_version, BOOTSTRAP_KEY, DISK_FILE, KNOWN_HOSTS, SNAPSHOT_TIMEOUT, SNAP_DIR,
};
use super::{mkdir_private, remove_tree, suspend, RealBackend};
use crate::backend::Backend;
use crate::ch::ChClient;
use crate::host::{self, DiskSource};
use crate::protocol::{CheckpointMade, Desired};
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
/// Files a usable checkpoint has.
pub(super) const REQUIRED: [&str; 6] = [
    "snap/config.json",
    "snap/state.json",
    "snap/memory-ranges",
    DELTA_FILE,
    BOOTSTRAP_KEY,
    KNOWN_HOSTS,
];

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
    /// The checkpoint its disk is a delta over; `None` over the base image.
    #[serde(default)]
    pub(super) parent: Option<String>,
}

/// A checkpoint id that is safe as a directory name.
pub(super) fn check_id(id: &str) -> Result<(), String> {
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

/// Bytes of every file under a dir.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DirBytes {
    /// Allocated on disk.
    allocated: u64,
    /// File lengths, holes included.
    apparent: u64,
}

/// What every file under `dir` weighs.
fn dir_bytes(dir: &Path) -> Result<DirBytes, String> {
    let mut total = DirBytes::default();
    let entries = fs::read_dir(dir).map_err(|e| format!("size {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let meta = entry
            .metadata()
            .map_err(|e| format!("size {}: {e}", entry.path().display()))?;
        let add = if meta.is_dir() {
            dir_bytes(&entry.path())?
        } else {
            DirBytes {
                allocated: meta.blocks() * BLOCK,
                apparent: meta.len(),
            }
        };
        total.allocated += add.allocated;
        total.apparent += add.apparent;
    }
    Ok(total)
}

/// The meta of the checkpoint in `dir`, if it has a readable one.
pub(super) fn read_meta(dir: &Path) -> Option<Meta> {
    let bytes = fs::read(dir.join(META_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

impl RealBackend {
    /// Where checkpoint `id` lives.
    pub(super) fn checkpoint_dir(&self, id: &str) -> PathBuf {
        self.config.cache_dir.join(format!("{DIR_PREFIX}{id}"))
    }

    /// Snapshots the running lease under `id`, resumes it, then stores its disk
    /// as a delta over its parent. An older leaf checkpoint of that id is replaced.
    pub(super) fn take_checkpoint(
        &self,
        record: &LeaseRecord,
        id: &str,
        name: &str,
        diag: &mut dyn Write,
    ) -> Result<CheckpointMade, String> {
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
        let _lock = self.checkpoint_lock(false)?;
        self.check_no_children(id, "replace")?;
        let backing = self.backing_for(lease, id);
        let meta = Meta {
            checkpoint_id: id.to_owned(),
            name: name.to_owned(),
            lease_id: lease.to_owned(),
            ch_version: ch_version(&self.config.cloud_hypervisor)?,
            size,
            created_at: state::now_unix(),
            parent: backing.parent().map(str::to_owned),
        };
        let pin = known_hosts_line(&format!("crabbox-{lease}"), &self.read_host_key(lease)?);
        let tmp = cache.join(format!("{DIR_PREFIX}{id}.tmp"));
        remove_tree(&tmp)?;
        mkdir_private(&tmp)?;
        let started = Instant::now();
        self.tidy_guest(record, diag);
        let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
        let pausing = Instant::now();
        let written = self.pause_vm(record, diag).and_then(|()| {
            let captured = self.capture(record, &tmp, &meta, &pin, diag);
            let resumed = client
                .resume()
                .and_then(|()| self.wait_state(&client, "Running"))
                .map_err(|e| format!("lease {lease} is still paused, resume it: {e}"));
            captured.and(resumed)
        });
        let paused = pausing.elapsed().as_secs_f64();
        let published = written.and_then(|()| self.publish(&tmp, id, &backing));
        if published.is_err() {
            let _ = remove_tree(&tmp);
        }
        let bytes = published?;
        self.set_parent(lease, id, diag);
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(
            diag,
            "checkpoint {id} written in {secs:.1}s, the lease paused for {paused:.1}s"
        );
        Ok(CheckpointMade {
            checkpoint_id: id.to_owned(),
            parent_checkpoint_id: meta.parent,
            unique_bytes: bytes.allocated,
            apparent_bytes: bytes.apparent,
        })
    }

    /// Turns the raw disk copy in `tmp` into a delta over `backing`, then
    /// makes `tmp` checkpoint `id`; returns what the checkpoint weighs.
    fn publish(&self, tmp: &Path, id: &str, backing: &Backing) -> Result<DirBytes, String> {
        let qemu_img = &self.config.qemu_img;
        chain::write_delta(
            qemu_img,
            &tmp.join(DISK_FILE),
            &tmp.join(DELTA_FILE),
            backing,
        )?;
        let dir = self.checkpoint_dir(id);
        remove_tree(&dir)?;
        fs::rename(tmp, &dir).map_err(|e| format!("publish checkpoint: {e}"))?;
        dir_bytes(&dir)
    }

    /// Writes the paused lease's snapshot, a raw copy of its disk, its login key
    /// and host pin into `dir`.
    fn capture(
        &self,
        record: &LeaseRecord,
        dir: &Path,
        meta: &Meta,
        pin: &str,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let lease = record.lease_id.as_str();
        let snap = dir.join(SNAP_DIR);
        mkdir_private(&snap)?;
        ChClient::new(&record.api_socket())
            .with_timeout(SNAPSHOT_TIMEOUT)
            .snapshot(&snap)
            .map_err(|e| format!("lease {lease}: snapshot failed: {e}"))?;
        suspend::dig_snapshot_holes(Path::new(host::DIG_HOLES_TOOL), &snap, diag);
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
        let meta = read_meta(dir)
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

    /// Starts the lease in `desired` from checkpoint `id`, running on a raw
    /// disk flattened from the checkpoint's chain, with its own keys, hostname
    /// and machine id. A lease that fails is destroyed.
    pub(super) fn fork_checkpoint(
        &self,
        desired: &Desired,
        id: &str,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        check_id(id)?;
        let _lock = self.checkpoint_lock(false)?;
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
            disk: DiskSource::Qcow2(dir.join(DELTA_FILE)),
            label: &label,
        };
        let alias = format!("crabbox-{}", meta.lease_id);
        let restored = self
            .restore_from(desired, &run_dir, &origin, size, diag)
            .and_then(|record| self.take_identity_from(record, &dir, alias, diag));
        match &restored {
            Ok(_) => self.set_parent(lease, id, diag),
            Err(_) => {
                if let Err(e) = self.destroy(lease, diag) {
                    let _ = writeln!(diag, "cleanup after failed fork also failed: {e}");
                }
            }
        }
        restored
    }

    /// Removes checkpoint `id` and any half-written copy; an unknown id is
    /// fine, one that other checkpoints are deltas over is refused.
    pub(super) fn remove_checkpoint(&self, id: &str) -> Result<(), String> {
        check_id(id)?;
        let _lock = self.checkpoint_lock(true)?;
        self.check_no_children(id, "delete")?;
        remove_tree(&self.config.cache_dir.join(format!("{DIR_PREFIX}{id}.tmp")))?;
        remove_tree(&self.checkpoint_dir(id))
    }
}
