//! Publish: flatten a lease's working copy into the next generation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{
    check_sealed, ext4, mkdir_private, unix_ns, Format, Publish, Store, WorkingCopy, FULL_PERCENT,
    LEASE_TAG, MIN_GROWTH,
};
use crate::host::{self, DiskSource};
use crate::volumes::Group;

/// Suffix of the flattened copy of an overlay, beside it in the work dir.
const FLAT_EXTENSION: &str = "flat.img";
/// Mode of a published generation: nothing writes it again.
const GENERATION_MODE: u32 = 0o444;

impl Store {
    /// Publishes `copy` as the next generation of `group` when it grew by at
    /// least [`MIN_GROWTH`] since restore, unmounted cleanly, every sealed
    /// volume left its marker, it passes a read-only fsck and is under
    /// [`FULL_PERCENT`] full. The generation holds the whole filesystem, minus
    /// the seal markers, and is read-only. Never touches the store otherwise.
    #[must_use]
    pub fn publish(
        &self,
        group: &Group,
        copy: &WorkingCopy,
        lease_id: &str,
        now: SystemTime,
    ) -> Publish {
        let rejected = |reason: String| Publish::Rejected { reason };
        match ext4::allocated(&copy.image) {
            Ok(bytes) if bytes < copy.baseline.saturating_add(MIN_GROWTH) => {
                return Publish::Unchanged
            }
            Ok(_) => {}
            Err(e) => return rejected(e),
        }
        let flat = match self.candidate(group, copy) {
            Ok(flat) => flat,
            Err(e) => return rejected(e),
        };
        let _lock = match self.lock(false) {
            Ok(lock) => lock,
            Err(e) => return rejected(e),
        };
        let dir = self.group_dir(&group.id);
        let generation = unix_ns(now);
        let tag: String = lease_id.chars().take(LEASE_TAG).collect();
        let dest = dir.join(format!("gen-{generation:019}-{tag}.img"));
        if let Err(e) = mkdir_private(&dir).and_then(|()| {
            fs::rename(&flat, &dest).map_err(|e| format!("rename into store: {}", e.kind()))
        }) {
            return rejected(e);
        }
        Self::touch(&dir);
        Publish::Published { generation }
    }

    /// A self-contained raw image of `copy` that passed every check, its seal
    /// markers cleared and its mode read-only; `copy` itself when it is raw.
    fn candidate(&self, group: &Group, copy: &WorkingCopy) -> Result<PathBuf, String> {
        let flat = match copy.format {
            Format::Raw => copy.image.clone(),
            Format::Qcow2 => {
                let flat = copy.image.with_extension(FLAT_EXTENSION);
                let source = DiskSource::Qcow2(copy.image.clone());
                host::make_disk(&self.qemu_img, &source, &flat, 0)?;
                flat
            }
        };
        check(group, &flat)?;
        for (_, marker) in group.seal_markers() {
            ext4::remove_file(&flat, &marker)?;
        }
        fs::set_permissions(&flat, fs::Permissions::from_mode(GENERATION_MODE))
            .map_err(|e| format!("seal generation: {}", e.kind()))?;
        Ok(flat)
    }
}

/// Clean unmount, every seal marker, a passing fsck, and room left.
fn check(group: &Group, image: &Path) -> Result<(), String> {
    if ext4::needs_recovery(image)? {
        return Err("not cleanly unmounted".to_owned());
    }
    check_sealed(group, image)?;
    match ext4::check(image) {
        Ok(usage) if usage.at_least(FULL_PERCENT) => {
            Err(format!("full ({}/{} blocks)", usage.used, usage.total))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("fsck: {e}")),
    }
}
