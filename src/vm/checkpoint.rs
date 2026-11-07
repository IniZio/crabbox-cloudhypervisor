//! Checkpoints: a snapshot of a running lease kept under a caller-chosen id.
//!
//! A checkpoint has the layout of a boot template (snapshot, disk, login key,
//! host pin), so a fork restores and rotates identity through the same code.
//! The lease's own login key plays the bootstrap key. Its root and cache
//! disks are each a qcow2 delta over its parent checkpoint's copy of the same
//! disk (see [`disks`]); a fork gets one image per disk that reads through no
//! checkpoint, and only under the cache plan the checkpoint was taken with.

mod chain;
pub(super) mod disks;
mod spare;
#[cfg(test)]
pub(super) mod tests;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(super) use self::disks::DisksFrom;
use self::disks::HeldDisk;
use super::cache::LeaseCache;
use super::template::restore::Origin;
use super::template::{ch_version, BOOTSTRAP_KEY, KNOWN_HOSTS, SNAPSHOT_TIMEOUT, SNAP_DIR};
use super::{mkdir_private, remove_tree, suspend, RealBackend};
use crate::backend::Backend;
use crate::ch::ChClient;
use crate::host;
use crate::protocol::{CheckpointMade, Desired};
use crate::ready::known_hosts_line;
use crate::size::Size;
use crate::state::{self, LeaseRecord};
use crate::timing;

/// What the checkpoint was taken from, inside its dir.
pub(super) const META_FILE: &str = "checkpoint.json";
/// Prefix of a checkpoint dir in the cache dir; `.tmp` marks one being written.
const DIR_PREFIX: &str = "checkpoint-";
/// Timeout for the pause and resume calls around the snapshot.
const API_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes per block in `st_blocks`.
const BLOCK: u64 = 512;
/// Files a usable checkpoint has besides its disks.
pub(super) const REQUIRED: [&str; 5] = [
    "snap/config.json",
    "snap/state.json",
    "snap/memory-ranges",
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
    /// The checkpoint its disks are deltas over; `None` for the first of a chain.
    #[serde(default)]
    pub(super) parent: Option<String>,
    /// Its disks, root first.
    pub(super) disks: Vec<HeldDisk>,
    /// The cache record of its lease, whose plan a fork must ask for.
    pub(super) cache: LeaseCache,
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

/// What every file under `dir` weighs; a file with other hard links, such as a
/// cache generation the store still holds, is not counted.
fn dir_bytes(dir: &Path) -> Result<DirBytes, String> {
    let mut total = DirBytes::default();
    let entries = fs::read_dir(dir).map_err(|e| format!("size {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let meta = entry
            .metadata()
            .map_err(|e| format!("size {}: {e}", entry.path().display()))?;
        let add = if meta.is_dir() {
            dir_bytes(&entry.path())?
        } else if meta.nlink() > 1 {
            DirBytes::default()
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

    /// Snapshots the running lease under `id`, resumes it, then stores its
    /// disks as deltas over its parent's. An older leaf checkpoint of that id is replaced.
    pub(super) fn take_checkpoint(
        &self,
        record: &LeaseRecord,
        id: &str,
        name: &str,
        diag: &mut dyn Write,
    ) -> Result<CheckpointMade, String> {
        check_id(id)?;
        let lease = record.lease_id.as_str();
        let size = self.record_size(record)?;
        let cache = &self.config.cache_dir;
        mkdir_private(cache)?;
        suspend::check_room(suspend::free_bytes(cache)?, size.memory_mib)?;
        let started = Instant::now();
        let _lock = self.checkpoint_lock(false)?;
        timing::step(diag, "lock-wait", started.elapsed(), "running");
        self.check_no_children(id, "replace")?;
        let mut meta = Meta {
            checkpoint_id: id.to_owned(),
            name: name.to_owned(),
            lease_id: lease.to_owned(),
            ch_version: ch_version(&self.config.cloud_hypervisor)?,
            size,
            created_at: state::now_unix(),
            parent: self.parent_for(lease, id),
            disks: Vec::new(),
            cache: LeaseCache::default(),
        };
        let pin = known_hosts_line(&format!("crabbox-{lease}"), &self.read_host_key(lease)?);
        let tmp = cache.join(format!("{DIR_PREFIX}{id}.tmp"));
        remove_tree(&tmp)?;
        mkdir_private(&tmp)?;
        let tidying = Instant::now();
        self.tidy_guest(record, diag);
        timing::step(diag, "tidy", tidying.elapsed(), "running");
        let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
        let pausing = Instant::now();
        let written = self.pause_vm(record, diag).and_then(|()| {
            timing::step(diag, "pause", pausing.elapsed(), "paused");
            let captured = self.capture(record, &tmp, &mut meta, &pin, diag);
            let resuming = Instant::now();
            let resumed = client
                .resume()
                .and_then(|()| self.wait_state(&client, "Running"))
                .map_err(|e| format!("lease {lease} is still paused, resume it: {e}"));
            timing::step(diag, "resume", resuming.elapsed(), "paused");
            captured.and(resumed)
        });
        let paused = pausing.elapsed().as_secs_f64();
        let published = written.and_then(|()| self.publish(&tmp, id, &meta, diag));
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
        timing::step(diag, "checkpoint-total", started.elapsed(), "");
        Ok(CheckpointMade {
            checkpoint_id: id.to_owned(),
            parent_checkpoint_id: meta.parent,
            unique_bytes: bytes.allocated,
            apparent_bytes: bytes.apparent,
        })
    }

    /// Turns the disk copies in `tmp` into deltas over the parent's, then
    /// makes `tmp` checkpoint `id`; returns what the checkpoint weighs.
    fn publish(
        &self,
        tmp: &Path,
        id: &str,
        meta: &Meta,
        diag: &mut dyn Write,
    ) -> Result<DirBytes, String> {
        self.write_layers(tmp, &meta.disks, meta.parent.as_deref(), diag)?;
        let dir = self.checkpoint_dir(id);
        remove_tree(&dir)?;
        fs::rename(tmp, &dir).map_err(|e| format!("publish checkpoint: {e}"))?;
        dir_bytes(&dir)
    }

    /// Writes the paused lease's snapshot, copies of its disks, its login key
    /// and host pin into `dir`, and `meta` naming the disks.
    fn capture(
        &self,
        record: &LeaseRecord,
        dir: &Path,
        meta: &mut Meta,
        pin: &str,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let lease = record.lease_id.as_str();
        let snap = dir.join(SNAP_DIR);
        mkdir_private(&snap)?;
        let snapping = Instant::now();
        ChClient::new(&record.api_socket())
            .with_timeout(SNAPSHOT_TIMEOUT)
            .snapshot(&snap)
            .map_err(|e| format!("lease {lease}: snapshot failed: {e}"))?;
        let memory = snap.join("memory-ranges");
        let allocated = |p: &Path| fs::metadata(p).map_or(0, |m| m.blocks() * BLOCK);
        let detail = format!("paused bytes={}", allocated(&memory));
        timing::step(diag, "memory-snapshot", snapping.elapsed(), &detail);
        let digging = Instant::now();
        suspend::dig_snapshot_holes(Path::new(host::DIG_HOLES_TOOL), &snap, diag);
        let detail = format!("paused bytes={}", allocated(&memory));
        timing::step(diag, "dig-holes", digging.elapsed(), &detail);
        (meta.disks, meta.cache) = self.hold_disks(lease, dir, diag)?;
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
        let meta = read_meta(dir).ok_or_else(|| {
            format!("checkpoint {id} is unreadable: its {META_FILE} is bad or from an older layout")
        })?;
        let disks = meta.disks.iter().map(|d| d.layer(dir));
        let mut files = REQUIRED.iter().map(|f| dir.join(f)).chain(disks);
        if let Some(missing) = files.find(|f| !f.exists()) {
            let missing = missing.strip_prefix(dir).unwrap_or(&missing).display();
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

    /// Starts the lease in `desired` from checkpoint `id`, on its own copy of
    /// each disk, with its own keys, hostname and machine id. A lease that
    /// fails is destroyed.
    pub(super) fn fork_checkpoint(
        &self,
        desired: &Desired,
        id: &str,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        check_id(id)?;
        let waiting = Instant::now();
        let _lock = self.checkpoint_lock(false)?;
        timing::step(diag, "lock-wait", waiting.elapsed(), "");
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
        self.check_held(&meta.cache, desired)
            .map_err(|e| format!("checkpoint {id} {e}"))?;
        let label = format!("checkpoint {id}");
        let origin = Origin {
            dir: &dir,
            label: &label,
            disks: DisksFrom::Checkpoint(&dir, &meta),
        };
        let alias = format!("crabbox-{}", meta.lease_id);
        let restored = self
            .restore_from(desired, &run_dir, &origin, size, diag)
            .and_then(|record| self.take_identity_from(record, &dir, alias, diag));
        match &restored {
            Ok(_) => {
                self.set_parent(lease, id, diag);
                if let Some(root) = meta.disks.first() {
                    let bytes = crate::volumes::ext4::allocated(&self.store.disk_path(lease));
                    self.refill_spare(&dir, root, bytes.unwrap_or_default(), diag);
                }
            }
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
        let dir = self.checkpoint_dir(id);
        let spares = read_meta(&dir).map(|m| m.disks).unwrap_or_default();
        if !spares
            .iter()
            .take(1)
            .all(|root| spare::Spare::of(&dir, root).stop())
        {
            return Err(format!(
                "cannot delete checkpoint {id}: its spare root job will not stop"
            ));
        }
        remove_tree(&dir)
    }
}
