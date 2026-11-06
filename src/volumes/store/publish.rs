//! Publish: copy a lease's working copy into the next generation.
//!
//! Every publish flattens to a private file and works only on that, so the
//! working copy never changes and a lease can keep using it afterwards. A kill
//! at any point before the final rename leaves the store as it was.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::{
    ext4, generations, mkdir_private, remove, unix_ns, Format, Publish, Store, WorkingCopy,
    FULL_PERCENT, LEASE_TAG, MIN_GROWTH, SLEEP_GROWTH,
};
use crate::host;
use crate::volumes::{Group, Token};

/// Extension of the private flat copy, beside the working copy.
const FLAT_EXTENSION: &str = "flat.tmp";
/// Suffix of the record of what a lease's copy of a group last matched.
const PUBLISHED_SUFFIX: &str = ".published.json";
/// Mode of a published generation: nothing writes it again.
const GENERATION_MODE: u32 = 0o444;

/// The generation a lease's working copy last matched, and its size then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Published {
    /// Generation it was restored from or last published as; `None` when it started empty.
    generation: Option<u64>,
    /// Allocated bytes of the working copy at that moment.
    bytes: u64,
}

impl Store {
    /// Publishes `copy` when it grew by at least [`MIN_GROWTH`], as a close does.
    #[must_use]
    pub fn publish(
        &self,
        group: &Group,
        copy: &WorkingCopy,
        lease_id: &str,
        now: SystemTime,
    ) -> Publish {
        self.publish_grown(group, copy, lease_id, now, MIN_GROWTH)
    }

    /// Publishes `copy` when it grew by at least [`SLEEP_GROWTH`], as a sleep does.
    #[must_use]
    pub fn publish_asleep(
        &self,
        group: &Group,
        copy: &WorkingCopy,
        lease_id: &str,
        now: SystemTime,
    ) -> Publish {
        self.publish_grown(group, copy, lease_id, now, SLEEP_GROWTH)
    }

    /// Removes flat copies a killed publish left in lease `lease_id`'s work dir.
    pub fn remove_flat_copies(&self, lease_id: &str) {
        let suffix = format!(".{FLAT_EXTENSION}");
        for entry in fs::read_dir(self.work_dir(lease_id)).into_iter().flatten() {
            let Ok(entry) = entry else { continue };
            if entry.file_name().to_string_lossy().ends_with(&suffix) {
                let _ = remove(&entry.path());
            }
        }
    }

    /// Records what `lease_id`'s copy of `group` matches; without the record a
    /// later publish measures from the copy's baseline and skips the regression guard.
    pub(super) fn record_published(
        &self,
        lease_id: &str,
        group: &Token,
        generation: Option<u64>,
        bytes: u64,
    ) {
        let path = self.published_path(lease_id, group);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec(&Published { generation, bytes }).unwrap_or_default();
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    /// Where `lease_id`'s record for `group` lives.
    pub(super) fn published_path(&self, lease_id: &str, group: &Token) -> PathBuf {
        self.work_dir(lease_id)
            .join(format!("{}{PUBLISHED_SUFFIX}", group.as_str()))
    }

    /// Runs one publish and removes its flat copy whatever became of it.
    fn publish_grown(
        &self,
        group: &Group,
        copy: &WorkingCopy,
        lease_id: &str,
        now: SystemTime,
        min_growth: u64,
    ) -> Publish {
        let flat = copy.image.with_extension(FLAT_EXTENSION);
        let _ = remove(&flat);
        let got = self
            .try_publish(group, copy, lease_id, now, min_growth)
            .unwrap_or_else(|stopped| stopped);
        let _ = remove(&flat);
        got
    }

    /// Guards, flatten, checks, rename; `Err` is why it stopped.
    fn try_publish(
        &self,
        group: &Group,
        copy: &WorkingCopy,
        lease_id: &str,
        now: SystemTime,
        min_growth: u64,
    ) -> Result<Publish, Publish> {
        let flat = &copy.image.with_extension(FLAT_EXTENSION);
        let rejected = |reason: String| Publish::Rejected { reason };
        let published = fs::read(self.published_path(lease_id, &group.id))
            .ok()
            .and_then(|b| serde_json::from_slice::<Published>(&b).ok());
        let allocated = ext4::allocated(&copy.image).map_err(rejected)?;
        let from = published.map_or(copy.baseline, |p| p.bytes);
        if allocated < from.saturating_add(min_growth) {
            return Ok(Publish::Unchanged);
        }
        let most = allocated.saturating_add(self.backing_bytes(lease_id, group, copy));
        self.check_regression(group, published, most)?;
        self.check_floor(most)?;
        self.check_overlay(copy).map_err(rejected)?;
        host::flatten_private(&self.qemu_img, copy.format.qemu_format(), &copy.image, flat)
            .map_err(rejected)?;
        check(flat).map_err(rejected)?;
        let bytes = ext4::allocated(flat).map_err(rejected)?;
        self.check_regression(group, published, bytes)?;
        let generation = self
            .rename_in(group, lease_id, now, flat)
            .map_err(rejected)?;
        self.record_published(lease_id, &group.id, Some(generation), allocated);
        Ok(Publish::Published { generation })
    }

    /// Allocated bytes of the generation under an overlay; 0 for a raw copy.
    fn backing_bytes(&self, lease_id: &str, group: &Group, copy: &WorkingCopy) -> u64 {
        let (_, base) = self.work_paths(lease_id, &group.id, copy.format);
        base.and_then(|b| ext4::allocated(&b).ok()).unwrap_or(0)
    }

    /// Stops when another lease published a generation at least as large as
    /// `bytes` since this lease last matched the newest one.
    fn check_regression(
        &self,
        group: &Group,
        published: Option<Published>,
        bytes: u64,
    ) -> Result<(), Publish> {
        let Some(published) = published else {
            return Ok(());
        };
        let Some(newest) = generations(&self.group_dir(&group.id)).pop() else {
            return Ok(());
        };
        if published.generation == Some(newest.ns) || newest.bytes < bytes {
            return Ok(());
        }
        Err(Publish::Skipped {
            reason: format!("newer generation {} is as large", newest.ns / 1_000_000_000),
        })
    }

    /// Stops when writing `bytes` would take the host below its free-space floor.
    fn check_floor(&self, bytes: u64) -> Result<(), Publish> {
        if self.host_floor_gib == 0 {
            return Ok(());
        }
        let (free, total) =
            host::space(&self.root).map_err(|reason| Publish::Skipped { reason })?;
        let floor = host::host_floor(total, self.host_floor_gib);
        if free.saturating_sub(bytes) >= floor {
            return Ok(());
        }
        Err(Publish::Skipped {
            reason: format!("host has {free} bytes free, below the floor of {floor} after {bytes}"),
        })
    }

    /// Fails on a damaged overlay. A raw image has nothing for qemu-img to check.
    fn check_overlay(&self, copy: &WorkingCopy) -> Result<(), String> {
        if copy.format != Format::Qcow2 {
            return Ok(());
        }
        host::qemu_img(
            &self.qemu_img,
            "overlay check",
            Command::new(&self.qemu_img)
                .args(["check", "-q", "-f", "qcow2"])
                .arg(&copy.image),
        )
    }

    /// Makes `flat` read-only and renames it in as the newest generation of `group`.
    fn rename_in(
        &self,
        group: &Group,
        lease_id: &str,
        now: SystemTime,
        flat: &Path,
    ) -> Result<u64, String> {
        fs::set_permissions(flat, fs::Permissions::from_mode(GENERATION_MODE))
            .map_err(|e| format!("make generation read-only: {}", e.kind()))?;
        let _lock = self.lock(false)?;
        let dir = self.group_dir(&group.id);
        let generation = unix_ns(now);
        let tag: String = lease_id.chars().take(LEASE_TAG).collect();
        let dest = dir.join(format!("gen-{generation:019}-{tag}.img"));
        mkdir_private(&dir)?;
        fs::rename(flat, &dest).map_err(|e| format!("rename into store: {}", e.kind()))?;
        Self::touch(&dir);
        Ok(generation)
    }
}

/// Replays the journal and repairs what `e2fsck -p` may on the private copy,
/// then requires a clean journal, a passing read-only fsck and room left.
fn check(image: &Path) -> Result<(), String> {
    ext4::repair(image).map_err(|e| format!("fsck: {e}"))?;
    if ext4::needs_recovery(image)? {
        return Err("journal still dirty after replay".to_owned());
    }
    match ext4::check(image) {
        Ok(usage) if usage.at_least(FULL_PERCENT) => {
            Err(format!("full ({}/{} blocks)", usage.used, usage.total))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("fsck: {e}")),
    }
}
