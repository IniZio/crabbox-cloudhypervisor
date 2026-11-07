//! Checkpoints as a chain: each one's disks are deltas over its parent's.

use std::fs::{self, File};
use std::io::Write;

use super::{check_id, read_meta, DIR_PREFIX};
use crate::vm::{mkdir_private, RealBackend};

/// In a lease dir: the checkpoint the lease's next checkpoint is a delta over.
const PARENT_FILE: &str = "parent-checkpoint";
/// In the cache dir: shared while a checkpoint is written or forked,
/// exclusive while one is deleted, so no delete pulls a parent out from under them.
const LOCK_FILE: &str = "checkpoint.lock";

#[cfg(test)]
mod tests;

impl RealBackend {
    /// Takes the checkpoint lock; released on drop.
    pub(super) fn checkpoint_lock(&self, exclusive: bool) -> Result<File, String> {
        let cache = &self.config.cache_dir;
        mkdir_private(cache)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache.join(LOCK_FILE))
            .map_err(|e| format!("open checkpoint lock: {e}"))?;
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.map_err(|e| format!("take checkpoint lock: {e}"))?;
        Ok(file)
    }

    /// Ids of the checkpoints whose disks are deltas over checkpoint `id`, sorted.
    pub(super) fn children(&self, id: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.config.cache_dir) else {
            return Vec::new();
        };
        let mut found: Vec<String> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(DIR_PREFIX)
            })
            .filter_map(|e| read_meta(&e.path()))
            .filter(|m| m.parent.as_deref() == Some(id) && m.checkpoint_id != id)
            .map(|m| m.checkpoint_id)
            .collect();
        found.sort();
        found
    }

    /// Refuses to drop checkpoint `id` while others are deltas over it.
    pub(super) fn check_no_children(&self, id: &str, doing: &str) -> Result<(), String> {
        let children = self.children(id);
        if children.is_empty() {
            return Ok(());
        }
        Err(format!(
            "cannot {doing} checkpoint {id}: checkpoints {} are deltas over it; delete them first",
            children.join(", ")
        ))
    }

    /// The checkpoint lease `lease`'s next checkpoint is a delta over: the one
    /// the lease last matched, while it is whole or still being written; else none.
    pub(super) fn parent_for(&self, lease: &str) -> Option<String> {
        let path = self.store.lease_dir(lease).join(PARENT_FILE);
        let parent = fs::read_to_string(path).ok().map(|p| p.trim().to_owned());
        parent.filter(|p| {
            let dir = self.checkpoint_dir(p);
            let pending = self.config.cache_dir.join(format!("{DIR_PREFIX}{p}.tmp"));
            check_id(p).is_ok()
                && (read_meta(&dir)
                    .is_some_and(|m| m.disks.iter().all(|d| d.layer(&dir).is_file()))
                    || super::layers::in_progress(&pending))
        })
    }

    /// Notes that lease `lease`'s disk now descends from checkpoint `id`.
    pub(super) fn set_parent(&self, lease: &str, id: &str, diag: &mut dyn Write) {
        let path = self.store.lease_dir(lease).join(PARENT_FILE);
        if let Err(e) = fs::write(&path, id) {
            let _ = writeln!(
                diag,
                "warn: lease {lease}'s next checkpoint holds its whole disk: {e}"
            );
        }
    }
}
