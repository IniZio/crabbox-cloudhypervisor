//! Cache disks in checkpoints: a fork's cache plan matched to the checkpoint's,
//! and the fork's copies put in its snapshot. Needs e2fsprogs; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;

use crate::protocol::CacheVolumeRequest;
use crate::size::Size;
use crate::vm::checkpoint::disks::tests::{desired, npm};
use crate::vm::checkpoint::tests::{backend, plant};
use crate::vm::checkpoint::{read_meta, META_FILE};
use crate::vm::RealBackend;
use crate::volumes::tests::{have_e2fsprogs, req};

const CH: &str = "cloud-hypervisor v52.0";
const DEFAULT: Size = Size {
    cpus: 2,
    memory_mib: 4096,
};

/// A complete checkpoint `ck1` at the default size whose cache record is lease `a`'s.
fn plant_with_cache(b: &RealBackend, requests: &[CacheVolumeRequest]) {
    let dir = plant(b, "ck1", DEFAULT, CH);
    fs::create_dir_all(b.store.lease_dir("a")).unwrap();
    let mut meta = read_meta(&dir).unwrap();
    meta.cache = b.restore_cache("a", requests, None).unwrap();
    fs::write(dir.join(META_FILE), serde_json::to_vec(&meta).unwrap()).unwrap();
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
fn a_snapshot_is_pointed_at_the_cache_copies_it_names_and_the_rest_are_hot_added() {
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
    assert!(cache.retarget(&mut config).unwrap().is_empty());
    assert_eq!(
        config["disks"][2]["path"],
        cache.groups[0].copy.image.to_str().unwrap()
    );
    assert_eq!(config["disks"][2]["serial"], "cbxcv0");
    assert_eq!(config["disks"][0]["path"], "/root");
    let mut bare = serde_json::json!({"disks": [{"id": "_disk0"}]});
    let hot = cache.retarget(&mut bare).unwrap();
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0]["id"], "cv0");
    assert_eq!(hot[0]["path"], cache.groups[0].copy.image.to_str().unwrap());
    assert_eq!(
        bare["disks"].as_array().unwrap().len(),
        1,
        "a template's snapshot is left as is"
    );
}
