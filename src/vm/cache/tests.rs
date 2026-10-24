#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::backend::Backend;
use crate::state::Store as LeaseStore;
use crate::vm::tests::config;
use crate::volumes::tests::{have_e2fsprogs, noise, req};

fn backend(root: &Path) -> RealBackend {
    RealBackend::new(config(root), PathBuf::from("/bin/cb"))
}

fn lease_dir(root: &Path, id: &str) -> PathBuf {
    let dir = LeaseStore::new(&root.join("state")).lease_dir(id);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn requests() -> Vec<CacheVolumeRequest> {
    vec![
        req("g1", "npm", &["~/.npm"], 1),
        req("g1", "bad name", &["~/x"], 0),
        req("g2", "cargo", &["~/.cargo/registry"], 1),
    ]
}

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

fn generations(root: &Path) -> usize {
    let store = root.join("volumes");
    fs::read_dir(&store)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != "leases")
        .flat_map(|c| fs::read_dir(c.path()).unwrap().flatten())
        .flat_map(|g| fs::read_dir(g.path()).unwrap().flatten())
        .filter(|f| f.file_name().to_string_lossy().starts_with("gen-"))
        .count()
}

#[test]
fn echo_lists_every_name_and_disks_carry_serials() {
    let skipped = Outcome::Skipped {
        reason: "x".to_owned(),
    };
    let plan = volumes::plan(&requests(), &crate::volumes::tests::limits(40));
    let rec = |i: usize, outcome: Outcome| RecordedGroup {
        group: plan.groups[i].clone(),
        outcome,
        image: PathBuf::from(format!("/w/{i}.img")),
        baseline: 0,
    };
    let cache = LeaseCache {
        groups: vec![rec(0, skipped), rec(1, Outcome::Empty)],
        refused: plan.skipped.clone(),
    };
    let echo: Vec<(String, String)> = cache
        .echo()
        .into_iter()
        .map(|e| (e.name, e.outcome))
        .collect();
    assert_eq!(
        echo,
        [
            ("npm".to_owned(), "skipped".to_owned()),
            ("cargo".to_owned(), "empty".to_owned()),
            ("bad name".to_owned(), "skipped".to_owned()),
        ]
    );
    let mut vm = json!({"disks": [{"id": "root"}, {"path": "/seed"}]});
    cache.attach(&mut vm);
    assert_eq!(
        vm["disks"][2],
        json!({"id": "cv0", "path": "/w/1.img", "image_type": "Raw", "readonly": false, "serial": "cbxcv0"})
    );
    assert_eq!(vm["disks"].as_array().unwrap().len(), 3);
    let units: Vec<String> = cache
        .guest_setup()
        .files
        .into_iter()
        .map(|f| f.path)
        .collect();
    assert!(units.contains(&r"/etc/systemd/system/var-lib-crabbox\x2dcache-g2.mount".to_owned()));
    assert!(!units.iter().any(|u| u.contains("cache-g1")));
}

#[test]
fn restore_records_the_echo_and_a_lease_without_volumes_has_none() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    lease_dir(dir.path(), "plain");
    assert_eq!(
        b.restore_cache("plain", &[]).unwrap(),
        LeaseCache::default()
    );
    assert!(b.cache_echo("plain").is_empty());

    lease_dir(dir.path(), "l1");
    b.restore_cache("l1", &requests()).unwrap();
    let echo: Vec<(String, String)> = b
        .cache_echo("l1")
        .into_iter()
        .map(|e| (e.name, e.outcome))
        .collect();
    assert_eq!(
        echo,
        [
            ("npm".to_owned(), "empty".to_owned()),
            ("cargo".to_owned(), "empty".to_owned()),
            ("bad name".to_owned(), "skipped".to_owned()),
        ]
    );
}

#[test]
fn release_publishes_only_after_a_clean_shutdown() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    for (id, quiesced) in [("killed", false), ("clean", true)] {
        lease_dir(dir.path(), id);
        let cache = b.restore_cache(id, &requests()[..1]).unwrap();
        grow_in_guest(&cache.groups[0].image);
        let mut diag = Vec::new();
        b.settle_cache(id, quiesced, &mut diag);
        let note = String::from_utf8(diag).unwrap();
        let want = if quiesced {
            "cache group g1: published gen"
        } else {
            "cache group g1: discarded"
        };
        assert!(note.starts_with(want), "{note}");
        assert!(!b.volume_store().work_dir(id).exists());
        assert_eq!(generations(dir.path()), usize::from(quiesced));
    }
}

#[test]
fn quiesce_is_skipped_for_a_lease_without_cache_disks() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    lease_dir(dir.path(), "plain");
    let record = crate::vm::tests::record("plain");
    let mut diag = Vec::new();
    assert!(!b.quiesce_for_cache(&record, &mut diag));
    assert!(diag.is_empty(), "no shutdown attempted");
}
