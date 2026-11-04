//! Generation store: `<dir>/<compat>/<group>/gen-<unix_ns>-<lease8>.img`.
//!
//! A generation is a raw ext4 image, read-only once published. Restore gives a
//! lease a qcow2 overlay over the newest one, publish flattens a lease's
//! overlay into the next one, and readers pick the newest name. Restore and
//! publish share a store lock that GC holds exclusively. Working copies live
//! in `<dir>/leases/<lease>/` on the same filesystem, so an overlay's backing
//! file is a hard link there and publish ends in one rename(2).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{ext4, Group, Outcome, Token, GIB};
use crate::config::CacheConfig;

/// Directory of working copies, beside the compat dirs.
const LEASES_DIR: &str = "leases";
/// Lock file in [`LEASES_DIR`], which store listings already skip: shared by restore and publish, exclusive for GC.
const LOCK_FILE: &str = ".lock";
/// File whose mtime records a group's last restore or publish.
const USED_FILE: &str = "used";
/// Smallest growth (allocated bytes) worth publishing.
pub const MIN_GROWTH: u64 = 32 * 1024 * 1024;
/// A group at least this full is not published.
pub const FULL_PERCENT: u64 = 95;
/// Superseded generations stay this long, for restores already cloning them.
pub const GRACE: Duration = Duration::from_secs(3600);
/// Hex characters of the image hash in the compat tag.
const COMPAT_HASH: usize = 12;
/// Lease id characters in a generation name.
const LEASE_TAG: usize = 8;
/// Suffix of a raw working copy in a lease's work dir.
const RAW_SUFFIX: &str = ".img";
/// Suffix of a working copy that is an overlay over a generation.
const OVERLAY_SUFFIX: &str = ".qcow2";
/// Suffix of the overlay's backing file: a hard link to its generation, so
/// the generation outlives GC while the overlay needs it.
const BASE_SUFFIX: &str = ".base.img";

/// One published generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    /// Image file.
    pub path: PathBuf,
    /// Publish time, unix nanoseconds.
    pub ns: u64,
    /// Allocated bytes on the host.
    pub bytes: u64,
}

/// One group as GC sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSnapshot {
    /// Group directory.
    pub dir: PathBuf,
    /// Last restore or publish, unix nanoseconds.
    pub used: u64,
    /// Generations, oldest first.
    pub gens: Vec<Generation>,
}

/// How a working copy is stored on the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// A whole ext4 image of its own.
    #[default]
    Raw,
    /// A qcow2 overlay over a read-only generation.
    Qcow2,
}

impl Format {
    /// cloud-hypervisor's `image_type` for this format.
    #[must_use]
    pub const fn image_type(self) -> &'static str {
        match self {
            Self::Raw => "Raw",
            Self::Qcow2 => "Qcow2",
        }
    }

    /// qemu-img's name for this format.
    #[must_use]
    pub const fn qemu_format(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

/// A lease's writable disk for one group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingCopy {
    /// Image file.
    pub image: PathBuf,
    /// How `image` is stored; records written before overlays are raw.
    #[serde(default)]
    pub format: Format,
    /// Allocated bytes of `image` right after restore.
    pub baseline: u64,
}

/// What restore gave one lease for one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// What the lease got.
    pub outcome: Outcome,
    /// Working copy; absent on disk when skipped.
    pub copy: WorkingCopy,
}

/// What release did with a working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publish {
    /// Renamed in as the newest generation.
    Published {
        /// Its timestamp, unix nanoseconds.
        generation: u64,
    },
    /// Grew less than [`MIN_GROWTH`]; discarded.
    Unchanged,
    /// Not trusted; discarded.
    Rejected {
        /// Why.
        reason: String,
    },
}

/// What GC removed (or would remove).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Paths removed, in order.
    pub removed: Vec<PathBuf>,
    /// Why GC could not run or could not remove a path.
    pub errors: Vec<String>,
}

/// Generation store rooted at `cacheVolumeDir`.
#[derive(Debug, Clone)]
pub struct Store {
    /// Store root.
    root: PathBuf,
    /// Base image, arch and optional scope tag; a different tag never sees these generations.
    compat: String,
    /// Whole store budget in bytes.
    budget: u64,
    /// Idle time after which a group goes.
    ttl: Duration,
    /// `qemu-img` binary.
    qemu_img: PathBuf,
}

/// Unix nanoseconds of `t`; 0 before the epoch.
#[must_use]
pub fn unix_ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Creates `path` and its parents with mode 0700.
pub(crate) fn mkdir_private(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create store dir: {}", e.kind()))
}

/// Timestamp of a `gen-<ns>-<lease8>.img` file name.
fn generation_ns(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("gen-")?.strip_suffix(".img")?;
    let (ns, _) = rest.split_once('-')?;
    ns.parse().ok()
}

/// Generations in `dir`, oldest first; unreadable entries are ignored.
fn generations(dir: &Path) -> Vec<Generation> {
    let mut gens: Vec<Generation> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let ns = generation_ns(e.file_name().to_str()?)?;
            let path = e.path();
            let bytes = ext4::allocated(&path).ok()?;
            Some(Generation { path, ns, bytes })
        })
        .collect();
    gens.sort_by(|a, b| (a.ns, &a.path).cmp(&(b.ns, &b.path)));
    gens
}

/// Every sealed volume of `group` has the marker in each of its path dirs.
fn check_sealed(group: &Group, image: &Path) -> Result<(), String> {
    for (name, marker) in group.seal_markers() {
        match ext4::has_file(image, &marker) {
            Ok(true) => {}
            Ok(false) => return Err(format!("unsealed: {name}")),
            Err(e) => return Err(format!("unsealed: {name} ({e})")),
        }
    }
    Ok(())
}

/// Removes a file or a directory tree; a missing one is fine.
fn remove(path: &Path) -> Result<(), String> {
    let result = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    if let Err(e) = result {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("remove {}: {}", path.display(), e.kind()));
        }
    }
    Ok(())
}

/// Subdirectories of `dir`; empty when it is missing.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Paths GC removes, in order: superseded generations past [`GRACE`], groups
/// idle past `ttl`, then least recently used groups until the rest fit `budget`.
#[must_use]
pub fn plan_gc(groups: &[GroupSnapshot], now: u64, ttl: Duration, budget: u64) -> Vec<PathBuf> {
    let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    let mut removed = Vec::new();
    let mut kept: Vec<(u64, u64, &Path)> = Vec::new();
    for g in groups {
        let mut bytes = 0u64;
        for pair in g.gens.windows(2) {
            if let [old, newer] = pair {
                if now.saturating_sub(newer.ns) > nanos(GRACE) {
                    removed.push(old.path.clone());
                } else {
                    bytes = bytes.saturating_add(old.bytes);
                }
            }
        }
        bytes = bytes.saturating_add(g.gens.last().map_or(0, |n| n.bytes));
        if now.saturating_sub(g.used) > nanos(ttl) {
            removed.push(g.dir.clone());
        } else {
            kept.push((g.used, bytes, &g.dir));
        }
    }
    kept.sort_by_key(|(used, _, dir)| (*used, dir.to_path_buf()));
    let mut total: u64 = kept.iter().map(|(_, b, _)| *b).sum();
    for (_, bytes, dir) in kept {
        if total <= budget {
            break;
        }
        removed.push(dir.to_path_buf());
        total = total.saturating_sub(bytes);
    }
    removed
}

impl Store {
    /// Store for `cfg`, tagged with the base image hash, this host's arch and
    /// the tenant `scope`; `None` is the store leases without a scope share.
    #[must_use]
    pub fn new(
        cfg: &CacheConfig,
        qemu_img: &Path,
        image_sha256: &str,
        scope: Option<&str>,
    ) -> Self {
        let hash: String = image_sha256.chars().take(COMPAT_HASH).collect();
        let arch = std::env::consts::ARCH;
        Self {
            root: cfg.volume_dir.clone(),
            compat: scope.map_or_else(
                || format!("{hash}-{arch}"),
                |s| format!("{hash}-{arch}-{s}"),
            ),
            budget: cfg.store_max_gib.saturating_mul(GIB),
            ttl: Duration::from_secs(cfg.key_ttl_days.saturating_mul(86_400)),
            qemu_img: qemu_img.to_path_buf(),
        }
    }

    /// Directory of one group's generations.
    #[must_use]
    pub fn group_dir(&self, group: &Token) -> PathBuf {
        self.root.join(&self.compat).join(group.as_str())
    }

    /// Directory of one lease's working copies.
    #[must_use]
    pub fn work_dir(&self, lease_id: &str) -> PathBuf {
        self.root.join(LEASES_DIR).join(lease_id)
    }

    /// Where lease `lease_id`'s working copy of `group` in `format` lives, and
    /// for an overlay the link to its generation beside it.
    #[must_use]
    pub fn work_paths(
        &self,
        lease_id: &str,
        group: &Token,
        format: Format,
    ) -> (PathBuf, Option<PathBuf>) {
        let file = |suffix: &str| {
            self.work_dir(lease_id)
                .join(format!("{}{suffix}", group.as_str()))
        };
        match format {
            Format::Raw => (file(RAW_SUFFIX), None),
            Format::Qcow2 => (file(OVERLAY_SUFFIX), Some(file(BASE_SUFFIX))),
        }
    }

    /// Marks a group used now.
    fn touch(dir: &Path) {
        let _ = fs::File::create(dir.join(USED_FILE));
    }

    /// Removes a lease's working copies.
    pub fn discard(&self, lease_id: &str) -> Result<(), String> {
        remove(&self.work_dir(lease_id))
    }
}

mod gc;
mod publish;
mod restore;
#[cfg(test)]
mod tests;
