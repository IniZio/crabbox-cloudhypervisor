//! The disks a checkpoint holds, root first and then each cache disk, all kept
//! one way. `disks/<stem>.qcow2` is a qcow2 delta over the parent checkpoint's
//! disk of the same stem, else over `disks/<stem>.base.img`, else over nothing.
//! The base is a hard link to what the disk started from (the base image, or
//! the cache generation), and every checkpoint of a chain links it, so store
//! GC never breaks a chain and a fork finds the base beside the layer.

#[cfg(test)]
pub(in crate::vm) mod tests;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::Meta;
use crate::host;
use crate::protocol::Desired;
use crate::vm::cache::LeaseCache;
use crate::vm::{mkdir_private, spec, RealBackend};
use crate::volumes::store::{Format, WorkingCopy};
use crate::volumes::{self, ext4, Token};

/// Dir of the disks inside a checkpoint dir.
const DISKS_DIR: &str = "disks";
/// Bytes per GiB.
const GIB: u64 = 1024 * 1024 * 1024;

/// Which of a lease's disks a held disk is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::vm) enum DiskOf {
    /// The root disk.
    Root,
    /// The cache disk of this group.
    Cache(Token),
}

impl DiskOf {
    /// File name stem in `disks/`; the prefix keeps a group named `root` apart.
    fn stem(&self) -> String {
        match self {
            Self::Root => "root".to_owned(),
            Self::Cache(group) => format!("cache-{}", group.as_str()),
        }
    }
}

/// One disk a checkpoint holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::vm) struct HeldDisk {
    /// Which disk.
    pub of: DiskOf,
    /// How the lease ran it, and so how a fork runs its copy.
    pub format: Format,
}

impl HeldDisk {
    /// Its layer in checkpoint dir `dir`.
    pub(super) fn layer(&self, dir: &Path) -> PathBuf {
        self.file(dir, "qcow2")
    }

    /// The link to what it started from, in checkpoint dir `dir`.
    fn base(&self, dir: &Path) -> PathBuf {
        self.file(dir, "base.img")
    }

    /// The copy taken while the lease was paused, until its layer is written.
    fn frozen(&self, dir: &Path) -> PathBuf {
        self.file(dir, "frozen")
    }

    /// Its file with `extension` in checkpoint dir `dir`.
    fn file(&self, dir: &Path, extension: &str) -> PathBuf {
        dir.join(DISKS_DIR)
            .join(format!("{}.{extension}", self.of.stem()))
    }
}

/// Where a new lease's disks come from.
#[derive(Debug, Clone, Copy)]
pub(in crate::vm) enum DisksFrom<'a> {
    /// A raw root image to copy; cache disks restore from the generation store.
    Image(&'a Path),
    /// What checkpoint dir `.0` holds, per its meta `.1`.
    Checkpoint(&'a Path, &'a Meta),
}

/// Writes `delta`, a qcow2 image that reads as `source` and holds only the
/// clusters where `source` differs from `backing` (its format and the name
/// `delta` records), or every cluster without one.
fn delta_over(
    qemu_img: &Path,
    source: &Path,
    format: Format,
    delta: &Path,
    backing: Option<(Format, &Path)>,
) -> Result<(), String> {
    let what = "disk delta";
    let source = fs::canonicalize(source).map_err(|e| format!("{what}: {e}"))?;
    host::make_overlay(qemu_img, what, &source, format.qemu_format(), delta)?;
    let mut rebase = Command::new(qemu_img);
    rebase.args(["rebase", "-q", "-f", "qcow2"]);
    match backing {
        Some((format, name)) => rebase.args(["-F", format.qemu_format(), "-b"]).arg(name),
        None => rebase.args(["-b", ""]),
    };
    host::qemu_img(qemu_img, what, rebase.arg(delta))
}

impl RealBackend {
    /// Lease `lease`'s disks, root first, each with the file it runs and the
    /// file it started from, if that is still there.
    fn live_disks(
        &self,
        lease: &str,
        cache: &LeaseCache,
    ) -> Vec<(HeldDisk, PathBuf, Option<PathBuf>)> {
        let base = spec::base_raw_path(&self.config);
        let root = HeldDisk {
            of: DiskOf::Root,
            format: Format::Raw,
        };
        let store = self.volume_store(cache.scope());
        let caches = cache.copies().map(|(group, copy)| {
            let (_, base) = store.work_paths(lease, group, copy.format);
            let held = HeldDisk {
                of: DiskOf::Cache(group.clone()),
                format: copy.format,
            };
            (held, copy.image.clone(), base.filter(|b| b.is_file()))
        });
        std::iter::once((
            root,
            self.store.disk_path(lease),
            Some(base).filter(|b| b.is_file()),
        ))
        .chain(caches)
        .collect()
    }

    /// Copies lease `lease`'s disks into checkpoint dir `dir`, each beside a
    /// link to its base; the lease must be paused. Returns them and its cache record.
    pub(super) fn hold_disks(
        &self,
        lease: &str,
        dir: &Path,
    ) -> Result<(Vec<HeldDisk>, LeaseCache), String> {
        let cache = self.read_cache(lease);
        mkdir_private(&dir.join(DISKS_DIR))?;
        let mut held = Vec::new();
        for (disk, image, base) in self.live_disks(lease, &cache) {
            if let Some(base) = base {
                host::link_or_copy(&base, &disk.base(dir))?;
            }
            host::clone_disk(&image, &disk.frozen(dir), 0)?;
            held.push(disk);
        }
        Ok((held, cache))
    }

    /// Turns each disk's frozen copy in `dir` into its layer: a delta over
    /// checkpoint `parent`'s disk of the same stem, else over its base.
    pub(super) fn write_layers(
        &self,
        dir: &Path,
        disks: &[HeldDisk],
        parent: Option<&str>,
    ) -> Result<(), String> {
        for disk in disks {
            let stem = disk.of.stem();
            let prefix = super::DIR_PREFIX;
            let backing = match parent {
                Some(p) if disk.layer(&self.checkpoint_dir(p)).is_file() => Some((
                    Format::Qcow2,
                    PathBuf::from(format!("../../{prefix}{p}/{DISKS_DIR}/{stem}.qcow2")),
                )),
                _ if disk.base(dir).is_file() => {
                    Some((Format::Raw, PathBuf::from(format!("{stem}.base.img"))))
                }
                _ => None,
            };
            let frozen = disk.frozen(dir);
            let backing = backing.as_ref().map(|(f, name)| (*f, name.as_path()));
            delta_over(
                &self.config.qemu_img,
                &frozen,
                disk.format,
                &disk.layer(dir),
                backing,
            )
            .map_err(|e| format!("{stem}: {e}"))?;
            fs::remove_file(&frozen).map_err(|e| format!("{stem}: remove the copy: {e}"))?;
        }
        Ok(())
    }

    /// Writes `image`, lease's copy of `disk` from checkpoint dir `dir`, which
    /// names no checkpoint file: a raw disk flattened from the chain and grown
    /// to `size`, or for an overlay one layer over `base`, the lease's own link
    /// of the disk's base.
    fn fork_disk(
        &self,
        dir: &Path,
        disk: &HeldDisk,
        image: &Path,
        base: Option<&Path>,
        size: u64,
    ) -> Result<(), String> {
        let qemu_img = &self.config.qemu_img;
        match (disk.format, base) {
            (Format::Raw, _) => host::flatten(qemu_img, &disk.layer(dir), image, size),
            (Format::Qcow2, Some(base)) => {
                host::link_or_copy(&disk.base(dir), base)?;
                let base = fs::canonicalize(base).map_err(|e| format!("resolve base: {e}"))?;
                let backing = Some((Format::Raw, base.as_path()));
                delta_over(qemu_img, &disk.layer(dir), Format::Qcow2, image, backing)
            }
            (Format::Qcow2, None) => Err("an overlay with no base".to_owned()),
        }
    }

    /// Gives lease `id` its own copy of every disk checkpoint dir `dir` holds,
    /// and records the cache copies as the lease's.
    pub(in crate::vm) fn fork_disks(
        &self,
        id: &str,
        dir: &Path,
        meta: &Meta,
        desired: &Desired,
    ) -> Result<LeaseCache, String> {
        let root = meta
            .disks
            .first()
            .filter(|d| d.of == DiskOf::Root)
            .ok_or("it holds no root disk")?;
        let size = self.config.disk_gib * GIB;
        self.fork_disk(dir, root, &self.store.disk_path(id), None, size)?;
        let store = self.volume_store(meta.cache.scope());
        let refused = volumes::plan(&desired.cache_volumes, &self.config.cache).skipped;
        let cache = meta.cache.with_copies(refused, |group| {
            let disk = meta
                .disks
                .iter()
                .find(|d| d.of == DiskOf::Cache(group.clone()))
                .ok_or_else(|| format!("it holds no disk for cache group {}", group.as_str()))?;
            let (image, base) = store.work_paths(id, group, disk.format);
            mkdir_private(&store.work_dir(id))?;
            self.fork_disk(dir, disk, &image, base.as_deref(), 0)
                .map_err(|e| format!("copy cache group {}: {e}", group.as_str()))?;
            let baseline = ext4::allocated(&image)?;
            Ok(WorkingCopy {
                image,
                format: disk.format,
                baseline,
            })
        })?;
        self.write_cache(id, &cache)?;
        Ok(cache)
    }
}
