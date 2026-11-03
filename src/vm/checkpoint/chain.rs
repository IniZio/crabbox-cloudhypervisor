//! Checkpoints as a chain: each one's disk is a qcow2 delta over its parent
//! checkpoint's disk, or over the base image for the first of a chain.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use super::{check_id, read_meta, DIR_PREFIX};
use crate::host::{make_overlay, qemu_img};
use crate::vm::{mkdir_private, spec, RealBackend};

/// In a lease dir: the checkpoint the lease's next checkpoint is a delta over.
const PARENT_FILE: &str = "parent-checkpoint";
/// In the cache dir: shared while a checkpoint is written or forked,
/// exclusive while one is deleted, so no delete pulls a parent out from under them.
const LOCK_FILE: &str = "checkpoint.lock";

#[cfg(test)]
mod tests;

/// A checkpoint's disk inside its dir.
pub(in crate::vm) const DELTA_FILE: &str = "disk.qcow2";

/// What a checkpoint's delta is taken against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Backing {
    /// The disk of the checkpoint with this id.
    Checkpoint(String),
    /// The base image with this file name, in the cache dir.
    Base(String),
    /// Nothing: the delta holds the whole disk.
    Standalone,
}

impl Backing {
    /// The parent checkpoint's id, when there is one.
    pub(super) fn parent(&self) -> Option<&str> {
        match self {
            Self::Checkpoint(id) => Some(id),
            Self::Base(_) | Self::Standalone => None,
        }
    }

    /// `qemu-img rebase` arguments naming the backing file relative to a
    /// checkpoint dir, which sits next to the others and the base in the cache dir.
    fn rebase_args(&self) -> Vec<String> {
        match self {
            Self::Checkpoint(id) => vec![
                "-F".to_owned(),
                "qcow2".to_owned(),
                "-b".to_owned(),
                format!("../{DIR_PREFIX}{id}/{DELTA_FILE}"),
            ],
            Self::Base(name) => vec![
                "-F".to_owned(),
                "raw".to_owned(),
                "-b".to_owned(),
                format!("../{name}"),
            ],
            Self::Standalone => vec!["-b".to_owned(), String::new()],
        }
    }
}

/// Writes `delta` holding only the clusters where the raw disk `frozen`
/// differs from `backing`, then removes `frozen`. `delta` reads as `frozen` did.
pub(super) fn write_delta(
    program: &Path,
    frozen: &Path,
    delta: &Path,
    backing: &Backing,
) -> Result<(), String> {
    let what = "checkpoint delta";
    let frozen_abs = fs::canonicalize(frozen).map_err(|e| format!("{what}: {e}"))?;
    make_overlay(program, what, &frozen_abs, delta)?;
    qemu_img(
        program,
        what,
        Command::new(program)
            .args(["rebase", "-q", "-f", "qcow2"])
            .args(backing.rebase_args())
            .arg(delta),
    )?;
    fs::remove_file(frozen).map_err(|e| format!("{what}: remove the raw copy: {e}"))
}

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

    /// Ids of the checkpoints whose disk is a delta over checkpoint `id`, sorted.
    pub(super) fn children(&self, id: &str) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.config.cache_dir) else {
            return Vec::new();
        };
        let mut found: Vec<String> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(DIR_PREFIX) && !name.ends_with(".tmp")
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

    /// What lease `lease`'s next checkpoint, written under `id`, is a delta over:
    /// the checkpoint the lease last matched (that one's parent when `id`
    /// replaces it), else the base image.
    pub(super) fn backing_for(&self, lease: &str, id: &str) -> Backing {
        let path = self.store.lease_dir(lease).join(PARENT_FILE);
        let mut parent = fs::read_to_string(path).ok().map(|p| p.trim().to_owned());
        if parent.as_deref() == Some(id) {
            parent = read_meta(&self.checkpoint_dir(id)).and_then(|m| m.parent);
        }
        let usable = |p: &String| {
            let dir = self.checkpoint_dir(p);
            check_id(p).is_ok() && dir.join(DELTA_FILE).is_file() && read_meta(&dir).is_some()
        };
        if let Some(p) = parent.filter(usable) {
            return Backing::Checkpoint(p);
        }
        let base = spec::base_raw_path(&self.config);
        match base.file_name() {
            Some(name) if base.is_file() => Backing::Base(name.to_string_lossy().into_owned()),
            _ => Backing::Standalone,
        }
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
