//! Restore: give a lease a working copy of a group's newest generation.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    ext4, generations, mkdir_private, remove, Format, Restored, Store, WorkingCopy, BASE_SUFFIX,
    OVERLAY_SUFFIX, RAW_SUFFIX,
};
use crate::host;
use crate::volumes::{Group, Outcome, Token};

impl Store {
    /// File `<group><suffix>` in lease `lease_id`'s work dir.
    fn work_file(&self, lease_id: &str, group: &Token, suffix: &str) -> PathBuf {
        self.work_dir(lease_id)
            .join(format!("{}{suffix}", group.as_str()))
    }

    /// An overlay over the newest generation, or a fresh filesystem when there
    /// is none. Any failure is `Skipped` and leaves no working copy.
    #[must_use]
    pub fn restore(&self, group: &Group, lease_id: &str) -> Restored {
        match self.try_restore(group, lease_id) {
            Ok((outcome, copy)) => Restored { outcome, copy },
            Err(reason) => {
                for suffix in [RAW_SUFFIX, OVERLAY_SUFFIX, BASE_SUFFIX] {
                    let _ = remove(&self.work_file(lease_id, &group.id, suffix));
                }
                Restored {
                    outcome: Outcome::Skipped { reason },
                    copy: WorkingCopy {
                        image: self.work_file(lease_id, &group.id, RAW_SUFFIX),
                        format: Format::Raw,
                        baseline: 0,
                    },
                }
            }
        }
    }

    /// Makes the working copy of `group` for `lease_id`.
    fn try_restore(&self, group: &Group, lease_id: &str) -> Result<(Outcome, WorkingCopy), String> {
        let _lock = self.lock(false)?;
        let dir = self.group_dir(&group.id);
        mkdir_private(&dir)?;
        mkdir_private(&self.work_dir(lease_id))?;
        Self::touch(&dir);
        let raw = self.work_file(lease_id, &group.id, RAW_SUFFIX);
        let Some(newest) = generations(&dir).pop() else {
            ext4::make(&raw, group.capacity)?;
            return Ok((Outcome::Empty, working_copy(raw, Format::Raw)?));
        };
        let outcome = Outcome::Restored {
            generation: newest.ns,
        };
        if needs_edit(group, &newest.path)? {
            host::clone_disk(&newest.path, &raw, 0)?;
            ext4::grow(&raw, group.capacity)?;
            for (_, marker) in group.seal_markers() {
                ext4::remove_file(&raw, &marker)?;
            }
            return Ok((outcome, working_copy(raw, Format::Raw)?));
        }
        let base = self.work_file(lease_id, &group.id, BASE_SUFFIX);
        let overlay = self.work_file(lease_id, &group.id, OVERLAY_SUFFIX);
        remove(&base)?;
        remove(&overlay)?;
        fs::hard_link(&newest.path, &base).map_err(|e| format!("link generation: {}", e.kind()))?;
        let base =
            fs::canonicalize(&base).map_err(|e| format!("resolve generation: {}", e.kind()))?;
        host::make_overlay(&self.qemu_img, "cache overlay", &base, "raw", &overlay)?;
        Ok((outcome, working_copy(overlay, Format::Qcow2)?))
    }
}

/// True when generation `image` cannot back an overlay of `group` as is: it is
/// smaller than the group, or still holds a seal marker (generations published
/// before markers were cleared at publish do).
fn needs_edit(group: &Group, image: &Path) -> Result<bool, String> {
    let len = fs::metadata(image)
        .map_err(|e| format!("stat generation: {}", e.kind()))?
        .len();
    if len < group.capacity {
        return Ok(true);
    }
    for (_, marker) in group.seal_markers() {
        if ext4::has_file(image, &marker)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `image` as a working copy, with its allocated bytes as the baseline.
fn working_copy(image: PathBuf, format: Format) -> Result<WorkingCopy, String> {
    let baseline = ext4::allocated(&image)?;
    Ok(WorkingCopy {
        image,
        format,
        baseline,
    })
}
