//! Copy: a working copy kept somewhere else, such as in a checkpoint, and back.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{mkdir_private, remove, Format, WorkingCopy, BASE_SUFFIX, OVERLAY_SUFFIX, RAW_SUFFIX};
use crate::host;
use crate::volumes::{ext4, Token};

/// How a copied overlay names its backing file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingName {
    /// By its file name, so the copy's dir can be renamed; for a copy nothing runs on.
    Relative,
    /// By its canonical path, as cloud-hypervisor opens it.
    Absolute,
}

/// The backing file of overlay `image` of `group`: `<group>.base.img` beside it.
fn base_of(image: &Path, group: &Token) -> PathBuf {
    image.with_file_name(format!("{}{BASE_SUFFIX}", group.as_str()))
}

/// Hard-links `from` to `to`, or copies it when they are on different filesystems.
fn link_or_copy(from: &Path, to: &Path) -> Result<(), String> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(_) => host::clone_disk(from, to, 0),
    }
}

/// Copies `copy` of `group` into `dir`, sharing no writable file with it.
///
/// A raw copy is copied whole. An overlay is copied with its backing generation
/// hard-linked beside it, so GC of the generation leaves the copy whole.
pub fn copy_working(
    qemu_img: &Path,
    group: &Token,
    copy: &WorkingCopy,
    dir: &Path,
    backing: BackingName,
) -> Result<WorkingCopy, String> {
    mkdir_private(dir)?;
    let name = group.as_str();
    let image = match copy.format {
        Format::Raw => {
            let image = dir.join(format!("{name}{RAW_SUFFIX}"));
            remove(&image)?;
            host::clone_disk(&copy.image, &image, 0)?;
            image
        }
        Format::Qcow2 => {
            let base = dir.join(format!("{name}{BASE_SUFFIX}"));
            let overlay = dir.join(format!("{name}{OVERLAY_SUFFIX}"));
            remove(&base)?;
            remove(&overlay)?;
            link_or_copy(&base_of(&copy.image, group), &base)?;
            host::clone_disk(&copy.image, &overlay, 0)?;
            let named = match backing {
                BackingName::Relative => PathBuf::from(format!("{name}{BASE_SUFFIX}")),
                BackingName::Absolute => fs::canonicalize(&base)
                    .map_err(|e| format!("resolve cache base: {}", e.kind()))?,
            };
            host::qemu_img(
                qemu_img,
                "cache copy",
                Command::new(qemu_img)
                    .args(["rebase", "-q", "-u", "-f", "qcow2", "-F", "raw", "-b"])
                    .arg(named)
                    .arg(&overlay),
            )?;
            overlay
        }
    };
    let baseline = ext4::allocated(&image)?;
    Ok(WorkingCopy {
        image,
        format: copy.format,
        baseline,
    })
}
