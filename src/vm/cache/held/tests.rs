//! Cache disks in checkpoints: held at create, copied out to a fork, matched to
//! the fork's cache plan. Needs qemu-img and e2fsprogs; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::protocol::{CacheVolumeRequest, Desired};
use crate::size::Size;
use crate::vm::checkpoint::tests::{backend, have_qemu_img, image_info, plant, running_lease, Api};
use crate::vm::tests::record;
use crate::vm::RealBackend;
use crate::volumes::store::Format;
use crate::volumes::tests::{have_e2fsprogs, noise, req};

const CH: &str = "cloud-hypervisor v52.0";
const DEFAULT: Size = Size {
    cpus: 2,
    memory_mib: 4096,
};

fn npm() -> Vec<CacheVolumeRequest> {
    vec![req("g1", "npm", &["~/.npm"], 1)]
}

fn desired(id: &str, cache_volumes: Vec<CacheVolumeRequest>) -> Desired {
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

/// Writes 40 MiB of noise into the ext4 image `image`, as a guest would.
fn grow_in_guest(image: &Path) {
    let dir = tempfile::tempdir().unwrap();
    let blob = dir.path().join("blob");
    fs::write(&blob, noise(40 << 20)).unwrap();
    let out = Command::new("debugfs")
        .arg("-w")
        .arg("-R")
        .arg(format!("write {} blob", blob.display()))
        .arg(image)
        .output()
        .unwrap();
    assert!(out.status.success());
}

/// Publishes a grown generation of group g1 from a throwaway lease.
fn publish_generation(b: &RealBackend) {
    fs::create_dir_all(b.store.lease_dir("p")).unwrap();
    let cache = b.restore_cache("p", &npm(), None).unwrap();
    grow_in_guest(&cache.groups[0].copy.image);
    b.settle_cache("p", true, &mut Vec::new());
}

/// Checkpoints running lease `a` as `ck1`.
fn checkpoint(b: &RealBackend, rec: &crate::state::LeaseRecord) -> crate::protocol::CheckpointMade {
    let api = Api::start(&rec.api_socket(), 5, false);
    let made = b.take_checkpoint(rec, "ck1", "n", &mut Vec::new()).unwrap();
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

fn image_of(b: &RealBackend, lease: &str) -> PathBuf {
    b.read_cache(lease).groups[0].copy.image.clone()
}

#[test]
fn a_checkpoint_holds_a_cache_overlay_over_a_hard_link_of_its_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    publish_generation(&b);
    b.restore_cache("a", &npm(), None).unwrap();
    let lease_overlay = image_of(&b, "a");
    let made = checkpoint(&b, &rec);
    let held = b.checkpoint_dir("ck1").join("cache");
    let overlay = held.join("g1.qcow2");
    assert!(same_content(&overlay, &lease_overlay));
    assert_eq!(image_info(&overlay)["backing-filename"], "g1.base.img");
    assert_eq!(
        inode(&held.join("g1.base.img")),
        inode(&lease_overlay.with_file_name("g1.base.img")),
        "the generation is linked, not copied"
    );
    let base_bytes = fs::metadata(held.join("g1.base.img")).unwrap().blocks() * 512;
    assert!(made.unique_bytes > 0);
    assert!(
        made.unique_bytes < base_bytes,
        "a generation the store still holds is not this checkpoint's: {made:?}"
    );
}

#[test]
fn a_raw_cache_disk_is_copied_whole_and_counted() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    b.restore_cache("a", &npm(), None).unwrap();
    grow_in_guest(&image_of(&b, "a"));
    let made = checkpoint(&b, &rec);
    let image = b.checkpoint_dir("ck1").join("cache/g1.img");
    assert!(same_content(&image, &image_of(&b, "a")));
    assert!(made.unique_bytes > 40 << 20, "{made:?}");
}

#[test]
fn a_fork_gets_its_own_cache_overlays_and_outlives_the_checkpoint_and_the_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    publish_generation(&b);
    b.restore_cache("a", &npm(), None).unwrap();
    checkpoint(&b, &rec);
    let dir = b.checkpoint_dir("ck1");
    let want = root.path().join("want.img");
    let flattened = Command::new("qemu-img")
        .args(["convert", "-q", "-O", "raw", "-S", "4k"])
        .arg(dir.join("cache/g1.qcow2"))
        .arg(&want)
        .status()
        .unwrap();
    assert!(flattened.success());
    b.store.write(&record("f")).unwrap();
    let cache = b.adopt_cache("f", &dir, &desired("f", npm())).unwrap();
    let copy = &cache.groups[0].copy;
    assert_eq!(copy.format, Format::Qcow2);
    assert_eq!(
        copy.image,
        b.volume_store(None).work_dir("f").join("g1.qcow2")
    );
    let base = fs::canonicalize(copy.image.with_file_name("g1.base.img")).unwrap();
    assert_eq!(
        image_info(&copy.image)["backing-filename"],
        base.to_str().unwrap()
    );
    assert_ne!(inode(&copy.image), inode(&dir.join("cache/g1.qcow2")));
    assert_eq!(
        b.read_cache("f"),
        cache,
        "the fork's record names its copies"
    );

    b.remove_checkpoint("ck1").unwrap();
    for gen in fs::read_dir(b.volume_store(None).group_dir(&cache.groups[0].group.id))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
    {
        fs::remove_file(gen.path()).unwrap();
    }
    assert!(same_content(&copy.image, &want));
}

#[test]
fn a_fork_of_a_raw_cache_disk_gets_a_raw_copy() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    b.restore_cache("a", &npm(), None).unwrap();
    checkpoint(&b, &rec);
    b.store.write(&record("f")).unwrap();
    let dir = b.checkpoint_dir("ck1");
    let cache = b.adopt_cache("f", &dir, &desired("f", npm())).unwrap();
    assert_eq!(cache.groups[0].copy.format, Format::Raw);
    assert!(same_content(
        &cache.groups[0].copy.image,
        &dir.join("cache/g1.img")
    ));
}

/// A complete checkpoint `ck1` at the default size whose cache record is lease `a`'s.
fn plant_with_cache(b: &RealBackend, requests: &[CacheVolumeRequest]) {
    let dir = plant(b, "ck1", DEFAULT, CH);
    fs::create_dir_all(b.store.lease_dir("a")).unwrap();
    b.restore_cache("a", requests, None).unwrap();
    if !requests.is_empty() {
        fs::copy(
            b.store.lease_dir("a").join("cache.json"),
            dir.join("cache.json"),
        )
        .unwrap();
    }
}

#[test]
fn a_fork_under_another_cache_plan_is_refused_before_anything_is_made() {
    if !have_e2fsprogs() {
        return;
    }
    let cases: [(Vec<CacheVolumeRequest>, Vec<CacheVolumeRequest>, &str); 3] = [
        (
            npm(),
            vec![req("g2", "cargo", &["~/.cargo"], 1)],
            "checkpoint ck1 holds cache groups [g1]; this lease asks for cache groups [g2]",
        ),
        (
            npm(),
            Vec::new(),
            "checkpoint ck1 holds cache groups [g1]; this lease asks for no cache disks",
        ),
        (
            Vec::new(),
            npm(),
            "checkpoint ck1 holds no cache disks; this lease asks for cache groups [g1]",
        ),
    ];
    for (held, asked, want) in cases {
        let root = tempfile::tempdir().unwrap();
        let b = backend(root.path());
        plant_with_cache(&b, &held);
        let err = b
            .fork_checkpoint(&desired("f", asked), "ck1", &mut Vec::new())
            .unwrap_err();
        assert_eq!(err, want);
        assert!(b.store.read("f").unwrap().is_none());
        assert!(!b.volume_store(None).work_dir("f").exists());
    }
}

#[test]
fn a_fork_in_another_cache_scope_is_refused() {
    if !have_e2fsprogs() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let b = backend(root.path());
    plant_with_cache(&b, &npm());
    let mut asked = desired("f", npm());
    asked.cache_scope = Some("0123456789abcdef".to_owned());
    let err = b
        .fork_checkpoint(&asked, "ck1", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 holds cache groups [g1]; this lease asks for cache groups [g1] in scope 0123456789abcdef"
    );
}

#[test]
fn a_forks_snapshot_names_its_own_cache_copies() {
    if !have_e2fsprogs() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let b = backend(root.path());
    fs::create_dir_all(b.store.lease_dir("f")).unwrap();
    let cache = b.restore_cache("f", &npm(), None).unwrap();
    let mut config = serde_json::json!({"disks": [
        {"id": "_disk0", "path": "/root"},
        {"id": "_disk1", "path": "/seed"},
        {"id": "cv0", "path": "/source/lease/g1.img", "serial": "cbxcv0"},
    ]});
    cache.retarget(&mut config).unwrap();
    assert_eq!(
        config["disks"][2]["path"],
        cache.groups[0].copy.image.to_str().unwrap()
    );
    assert_eq!(config["disks"][2]["serial"], "cbxcv0");
    assert_eq!(config["disks"][0]["path"], "/root");
    let mut bare = serde_json::json!({"disks": [{"id": "_disk0"}]});
    assert_eq!(
        cache.retarget(&mut bare).unwrap_err(),
        "snapshot config.json has no cache disk cv0"
    );
}
