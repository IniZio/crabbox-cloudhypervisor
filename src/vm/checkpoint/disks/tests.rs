//! Every disk of a checkpoint, root and cache alike: held as a delta over the
//! parent's, forked into files that name no checkpoint. Needs qemu-img and
//! e2fsprogs; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{DiskOf, HeldDisk};
use crate::protocol::{CacheVolumeRequest, CheckpointMade, Desired};
use crate::state::LeaseRecord;
use crate::vm::cache::LeaseCache;
use crate::vm::checkpoint::read_meta;
use crate::vm::checkpoint::tests::{have_qemu_img, image_info, running_lease, Api};
use crate::vm::spec::base_raw_path;
use crate::vm::tests::{record, FakeCh};
use crate::vm::RealBackend;
use crate::volumes::store::{Format, WorkingCopy};
use crate::volumes::tests::{have_e2fsprogs, noise, req};
use crate::volumes::Token;

const MIB: u64 = 1 << 20;

pub(in crate::vm) fn npm() -> Vec<CacheVolumeRequest> {
    vec![req("g1", "npm", &["~/.npm"], 1)]
}

pub(in crate::vm) fn desired(id: &str, cache_volumes: Vec<CacheVolumeRequest>) -> Desired {
    Desired {
        lease_id: id.to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
        cache_volumes,
        cache_scope: None,
    }
}

fn tools() -> bool {
    have_qemu_img() && have_e2fsprogs()
}

fn g1() -> Token {
    Token::parse("g1").unwrap()
}

/// Writes `len` bytes of `byte` at `at` into `image` through its format, as a guest would.
fn scribble(image: &Path, format: Format, at: u64, len: u64, byte: u8) {
    let fmt = match format {
        Format::Raw => "raw",
        Format::Qcow2 => "qcow2",
    };
    let out = Command::new("qemu-io")
        .args(["-f", fmt, "-c", &format!("write -P {byte} {at} {len}")])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Publishes a generation of group g1 from a throwaway lease.
fn publish_generation(b: &RealBackend) {
    fs::create_dir_all(b.store.lease_dir("p")).unwrap();
    let cache = b.restore_cache("p", &npm(), None).unwrap();
    let blob = tempfile::NamedTempFile::new().unwrap();
    fs::write(blob.path(), noise(40 * MIB)).unwrap();
    let wrote = Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!("write {} blob", blob.path().display()))
        .arg(&first(&cache).image)
        .output()
        .unwrap();
    assert!(wrote.status.success());
    b.settle_cache("p", true, &mut Vec::new());
}

/// A running lease `a` on a 1 MiB base image with cache group g1: an overlay
/// over a generation, or a raw disk when the store has none.
fn lease(root: &Path, format: Format) -> (RealBackend, LeaseRecord, FakeCh) {
    let (b, rec, ch) = running_lease(root);
    let base = noise(MIB);
    fs::create_dir_all(&b.config.cache_dir).unwrap();
    fs::write(base_raw_path(&b.config), &base).unwrap();
    let mut disk = base;
    disk.resize(2 << 20, 0);
    fs::write(b.store.disk_path("a"), disk).unwrap();
    if format == Format::Qcow2 {
        publish_generation(&b);
    }
    let cache = b.restore_cache("a", &npm(), None).unwrap();
    assert_eq!(first(&cache).format, format);
    (b, rec, ch)
}

/// Checkpoints lease `a` under `id` against a fresh stand-in API.
pub(in crate::vm) fn take(b: &RealBackend, rec: &LeaseRecord, id: &str) -> CheckpointMade {
    let _ = fs::remove_file(rec.api_socket());
    let api = Api::start(&rec.api_socket(), 5, false);
    let made = b.take_checkpoint(rec, id, "n", &mut Vec::new()).unwrap();
    api.finish();
    made
}

/// True when the guest reads the same bytes from images `a` and `b`.
fn same_content(a: &Path, b: &Path) -> bool {
    Command::new("qemu-img")
        .args(["compare", "-q"])
        .arg(a)
        .arg(b)
        .status()
        .unwrap()
        .success()
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

fn first(cache: &LeaseCache) -> &WorkingCopy {
    cache.copies().next().unwrap().1
}

fn cache_image(b: &RealBackend, lease: &str) -> PathBuf {
    first(&b.read_cache(lease)).image.clone()
}

/// A raw copy of what `image` reads as, at `to`.
fn flatten_to(image: &Path, to: &Path) {
    let ok = Command::new("qemu-img")
        .args(["convert", "-q", "-O", "raw", "-S", "4k"])
        .arg(image)
        .arg(to)
        .status()
        .unwrap();
    assert!(ok.success());
}

#[test]
fn a_checkpoint_holds_root_and_cache_disks_the_same_way_root_first() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let image = cache_image(&b, "a");
    let made = take(&b, &rec, "ck1");
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    assert_eq!(
        meta.disks,
        [
            HeldDisk {
                of: DiskOf::Root,
                format: Format::Raw,
            },
            HeldDisk {
                of: DiskOf::Cache(g1()),
                format: Format::Qcow2,
            },
        ]
    );
    let disks = dir.join("disks");
    for (stem, live, base) in [
        ("root", b.store.disk_path("a"), base_raw_path(&b.config)),
        (
            "cache-g1",
            image.clone(),
            image.with_file_name("g1.base.img"),
        ),
    ] {
        let layer = disks.join(format!("{stem}.qcow2"));
        assert_eq!(
            image_info(&layer)["backing-filename"],
            format!("{stem}.base.img")
        );
        assert_eq!(
            inode(&disks.join(format!("{stem}.base.img"))),
            inode(&base),
            "{stem}: what the disk started from is linked, not copied"
        );
        assert!(same_content(&layer, &live), "{stem} reads as the lease's");
    }
    assert!(
        made.unique_bytes < 4 * MIB,
        "linked bases are not this checkpoint's: {made:?}"
    );
}

#[test]
fn every_later_layer_of_a_disk_holds_only_what_changed_since_the_parent() {
    if !tools() {
        return;
    }
    for format in [Format::Raw, Format::Qcow2] {
        let root = tempfile::tempdir().unwrap();
        let (b, rec, _ch) = lease(root.path(), format);
        let image = cache_image(&b, "a");
        scribble(&image, format, 64 * MIB, 32 * MIB, 1);
        take(&b, &rec, "ck1");
        scribble(&image, format, 200 * MIB, MIB, 2);
        let made = take(&b, &rec, "ck2");
        assert_eq!(made.parent_checkpoint_id.as_deref(), Some("ck1"));
        let layer = b.checkpoint_dir("ck2").join("disks/cache-g1.qcow2");
        assert_eq!(
            image_info(&layer)["backing-filename"],
            "../../checkpoint-ck1/disks/cache-g1.qcow2"
        );
        assert!(
            same_content(&layer, &image),
            "{format:?}: ck2 reads as the disk"
        );
        assert!(
            allocated(&layer) < 4 * MIB,
            "{format:?}: a 1 MiB change makes a {} byte layer",
            allocated(&layer)
        );
        assert!(made.unique_bytes < 8 * MIB, "{format:?}: {made:?}");
    }
}

/// Checks lease `f`'s root disk starts with `want` and has the configured size.
fn assert_root_of_f(b: &RealBackend, want: &[u8]) {
    let mut got = vec![0; want.len()];
    let mut disk = fs::File::open(b.store.disk_path("f")).unwrap();
    disk.read_exact(&mut got).unwrap();
    assert_eq!(disk.metadata().unwrap().len(), b.config.disk_gib << 30);
    assert!(got == want, "the root reads as the lease's");
}

/// Deletes checkpoints ck2 and ck1, the base image and every generation of g1.
fn evict_chain_and_bases(b: &RealBackend) {
    b.remove_checkpoint("ck2").unwrap();
    b.remove_checkpoint("ck1").unwrap();
    fs::remove_file(base_raw_path(&b.config)).unwrap();
    for gen in fs::read_dir(b.volume_store(None).group_dir(&g1()))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
    {
        fs::remove_file(gen.path()).unwrap();
    }
}

#[test]
fn a_fork_runs_on_its_own_files_and_outlives_the_chain_and_the_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let image = cache_image(&b, "a");
    scribble(&image, Format::Qcow2, 64 * MIB, 8 * MIB, 1);
    take(&b, &rec, "ck1");
    scribble(&image, Format::Qcow2, 100 * MIB, MIB, 2);
    take(&b, &rec, "ck2");
    let want_cache = root.path().join("want-cache.img");
    flatten_to(&image, &want_cache);
    let want_root = fs::read(b.store.disk_path("a")).unwrap();

    let dir = b.checkpoint_dir("ck2");
    let meta = read_meta(&dir).unwrap();
    b.store.write(&record("f")).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()))
        .unwrap();
    let copy = first(&cache);
    let work = b.volume_store(None).work_dir("f");
    assert_eq!(copy.format, Format::Qcow2);
    assert_eq!(copy.image, work.join("g1.qcow2"));
    let base = fs::canonicalize(work.join("g1.base.img")).unwrap();
    assert_eq!(
        image_info(&copy.image)["backing-filename"],
        base.to_str().unwrap(),
        "the fork's overlay reads its generation through its own link"
    );
    assert_eq!(
        b.read_cache("f"),
        cache,
        "the fork's record names its copies"
    );
    let info = image_info(&b.store.disk_path("f"));
    assert_eq!(info["format"], "raw");
    assert!(info.get("backing-filename").is_none(), "{info}");

    evict_chain_and_bases(&b);
    assert!(same_content(&copy.image, &want_cache));
    assert_root_of_f(&b, &want_root);
}

#[test]
fn a_fork_of_a_raw_cache_disk_runs_a_raw_disk_of_its_own() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Raw);
    let image = cache_image(&b, "a");
    scribble(&image, Format::Raw, 64 * MIB, MIB, 1);
    take(&b, &rec, "ck1");
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    b.store.write(&record("f")).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()))
        .unwrap();
    let copy = first(&cache);
    assert_eq!(copy.format, Format::Raw);
    assert_eq!(
        copy.image,
        b.volume_store(None).work_dir("f").join("g1.img")
    );
    assert!(image_info(&copy.image).get("backing-filename").is_none());
    b.remove_checkpoint("ck1").unwrap();
    assert!(same_content(&copy.image, &image));
}

#[test]
fn removing_a_checkpoint_drops_its_link_to_the_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let base = cache_image(&b, "a").with_file_name("g1.base.img");
    let before = fs::metadata(&base).unwrap().nlink();
    take(&b, &rec, "ck1");
    assert_eq!(fs::metadata(&base).unwrap().nlink(), before + 1);
    b.remove_checkpoint("ck1").unwrap();
    assert_eq!(fs::metadata(&base).unwrap().nlink(), before);
}

#[test]
fn a_checkpoint_in_the_layout_before_disks_is_refused_so_the_caller_starts_cold() {
    let root = tempfile::tempdir().unwrap();
    let b = crate::vm::checkpoint::tests::backend(root.path());
    let dir = b.checkpoint_dir("old");
    fs::create_dir_all(&dir).unwrap();
    let old = r#"{"checkpointId":"old","name":"n","leaseId":"src","chVersion":"cloud-hypervisor v52.0",
        "size":{"cpus":2,"memory_mib":4096},"createdAt":1}"#;
    fs::write(dir.join("checkpoint.json"), old).unwrap();
    fs::write(dir.join("disk.qcow2"), "x").unwrap();
    let err = b
        .fork_checkpoint(&desired("f", Vec::new()), "old", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint old is unreadable: its checkpoint.json is bad or from an older layout"
    );
    assert!(b.store.read("f").unwrap().is_none());
}
