//! Generation store: `<dir>/<compat>/<group>/gen-<unix_ns>-<lease8>.img`.
//!
//! Lockless. Restore clones the newest generation into a private working copy,
//! publish renames a working copy in as the next one, and readers pick the
//! newest name. Working copies live in `<dir>/leases/<lease>/` on the same
//! filesystem, so clone can reflink and publish is one rename(2).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{ext4, Group, Outcome, Token, GIB};
use crate::config::CacheConfig;
use crate::host;

/// Directory of working copies, beside the compat dirs.
const LEASES_DIR: &str = "leases";
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

/// A private working copy for one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// What the lease got.
    pub outcome: Outcome,
    /// Working copy; absent when skipped.
    pub image: PathBuf,
    /// Allocated bytes of the working copy right after restore.
    pub baseline: u64,
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
}

/// Generation store rooted at `cacheVolumeDir`.
#[derive(Debug, Clone)]
pub struct Store {
    /// Store root.
    root: PathBuf,
    /// Base image and arch tag; a different base never sees these generations.
    compat: String,
    /// Whole store budget in bytes.
    budget: u64,
    /// Idle time after which a group goes.
    ttl: Duration,
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
    /// Store for `cfg`, tagged with the base image hash and this host's arch.
    #[must_use]
    pub fn new(cfg: &CacheConfig, image_sha256: &str) -> Self {
        let hash: String = image_sha256.chars().take(COMPAT_HASH).collect();
        Self {
            root: cfg.volume_dir.clone(),
            compat: format!("{hash}-{}", std::env::consts::ARCH),
            budget: cfg.store_max_gib.saturating_mul(GIB),
            ttl: Duration::from_secs(cfg.key_ttl_days.saturating_mul(86_400)),
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

    /// Marks a group used now.
    fn touch(dir: &Path) {
        let _ = fs::File::create(dir.join(USED_FILE));
    }

    /// A private copy of the newest generation, or a fresh filesystem when
    /// there is none. Any failure is `Skipped` and leaves no working copy.
    #[must_use]
    pub fn restore(&self, group: &Group, lease_id: &str) -> Restored {
        let image = self
            .work_dir(lease_id)
            .join(format!("{}.img", group.id.as_str()));
        match self.try_restore(group, &image) {
            Ok((outcome, baseline)) => Restored {
                outcome,
                image,
                baseline,
            },
            Err(reason) => {
                let _ = remove(&image);
                Restored {
                    outcome: Outcome::Skipped { reason },
                    image,
                    baseline: 0,
                }
            }
        }
    }

    /// Clones or creates `image`, grows it to the group capacity; (outcome, baseline).
    fn try_restore(&self, group: &Group, image: &Path) -> Result<(Outcome, u64), String> {
        let dir = self.group_dir(&group.id);
        mkdir_private(&dir)?;
        if let Some(parent) = image.parent() {
            mkdir_private(parent)?;
        }
        Self::touch(&dir);
        let outcome = if let Some(newest) = generations(&dir).pop() {
            host::clone_disk(&newest.path, image, 0)?;
            ext4::grow(image, group.capacity)?;
            for (_, marker) in group.seal_markers() {
                ext4::remove_file(image, &marker)?;
            }
            Outcome::Restored {
                generation: newest.ns,
            }
        } else {
            ext4::make(image, group.capacity)?;
            Outcome::Empty
        };
        Ok((outcome, ext4::allocated(image)?))
    }

    /// Renames `image` in as the next generation of `group` when it grew by at
    /// least [`MIN_GROWTH`] over `baseline`, unmounted cleanly, every sealed
    /// volume left its marker, it passes a read-only fsck and is under
    /// [`FULL_PERCENT`] full. Never touches the store otherwise.
    #[must_use]
    pub fn publish(
        &self,
        group: &Group,
        image: &Path,
        baseline: u64,
        lease_id: &str,
        now: SystemTime,
    ) -> Publish {
        let rejected = |reason: String| Publish::Rejected { reason };
        match ext4::allocated(image) {
            Ok(bytes) if bytes < baseline.saturating_add(MIN_GROWTH) => return Publish::Unchanged,
            Ok(_) => {}
            Err(e) => return rejected(e),
        }
        match ext4::needs_recovery(image) {
            Ok(false) => {}
            Ok(true) => return rejected("not cleanly unmounted".to_owned()),
            Err(e) => return rejected(e),
        }
        if let Err(e) = check_sealed(group, image) {
            return rejected(e);
        }
        match ext4::check(image) {
            Ok(usage) if usage.at_least(FULL_PERCENT) => {
                return rejected(format!("full ({}/{} blocks)", usage.used, usage.total));
            }
            Ok(_) => {}
            Err(e) => return rejected(format!("fsck: {e}")),
        }
        let dir = self.group_dir(&group.id);
        let generation = unix_ns(now);
        let tag: String = lease_id.chars().take(LEASE_TAG).collect();
        let dest = dir.join(format!("gen-{generation:019}-{tag}.img"));
        if let Err(e) = mkdir_private(&dir).and_then(|()| {
            fs::rename(image, &dest).map_err(|e| format!("rename into store: {}", e.kind()))
        }) {
            return rejected(e);
        }
        Self::touch(&dir);
        Publish::Published { generation }
    }

    /// Removes a lease's working copies.
    pub fn discard(&self, lease_id: &str) -> Result<(), String> {
        remove(&self.work_dir(lease_id))
    }

    /// Every group of every compat tag, for GC.
    fn snapshot(&self) -> Vec<GroupSnapshot> {
        let leases = self.root.join(LEASES_DIR);
        subdirs(&self.root)
            .into_iter()
            .filter(|d| *d != leases)
            .flat_map(|compat| subdirs(&compat))
            .map(|dir| {
                let used = fs::metadata(dir.join(USED_FILE))
                    .or_else(|_| fs::metadata(&dir))
                    .and_then(|m| m.modified())
                    .map_or(0, unix_ns);
                GroupSnapshot {
                    gens: generations(&dir),
                    dir,
                    used,
                }
            })
            .collect()
    }

    /// Removes working copies of leases `live` rejects, then what [`plan_gc`]
    /// picks. `dry_run` only reports.
    pub fn gc(&self, now: SystemTime, live: &dyn Fn(&str) -> bool, dry_run: bool) -> GcReport {
        let mut removed: Vec<PathBuf> = subdirs(&self.root.join(LEASES_DIR))
            .into_iter()
            .filter(|d| {
                d.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !live(n))
            })
            .collect();
        removed.extend(plan_gc(
            &self.snapshot(),
            unix_ns(now),
            self.ttl,
            self.budget,
        ));
        if !dry_run {
            removed.retain(|p| remove(p).is_ok());
        }
        GcReport { removed }
    }
}

#[cfg(test)]
mod tests;
