#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;
use crate::backend::Backend;
use crate::state::Store as LeaseStore;
use crate::vm::tests::config;
use crate::volumes::tests::{have_e2fsprogs, noise, req};

fn backend(root: &Path) -> RealBackend {
    let mut config = config(root);
    config.qemu_img = PathBuf::from("qemu-img");
    RealBackend::new(config, PathBuf::from("/bin/cb"))
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
        copy: WorkingCopy {
            image: PathBuf::from(format!("/w/{i}.img")),
            format: Format::Raw,
            baseline: 0,
        },
    };
    let cache = LeaseCache {
        groups: vec![rec(0, skipped), rec(1, Outcome::Empty)],
        refused: plan.skipped.clone(),
        scope: None,
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
        b.restore_cache("plain", &[], None).unwrap(),
        LeaseCache::default()
    );
    assert!(b.cache_echo("plain").is_empty());

    lease_dir(dir.path(), "l1");
    b.restore_cache("l1", &requests(), None).unwrap();
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
        let cache = b.restore_cache(id, &requests()[..1], None).unwrap();
        grow_in_guest(&cache.groups[0].copy.image);
        let mut diag = Vec::new();
        b.settle_cache(id, quiesced, &mut diag);
        let note = String::from_utf8(diag).unwrap();
        let want = if quiesced {
            "cache group g1: published gen"
        } else {
            "cache group g1: discarded"
        };
        assert!(note.starts_with(want), "{note}");
        assert!(!b.volume_store(None).work_dir(id).exists());
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

#[test]
fn a_generation_is_restored_only_under_the_scope_that_published_it() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let tenant_a = Some("00000000000000aa");
    lease_dir(dir.path(), "first");
    let cache = b
        .restore_cache("first", &requests()[..1], tenant_a)
        .unwrap();
    grow_in_guest(&cache.groups[0].copy.image);
    b.settle_cache("first", true, &mut Vec::new());

    let outcome = |id: &str, scope: Option<&str>| {
        lease_dir(dir.path(), id);
        b.restore_cache(id, &requests()[..1], scope).unwrap();
        b.cache_echo(id)[0].outcome.clone()
    };
    assert_eq!(outcome("other-tenant", Some("00000000000000bb")), "empty");
    assert_eq!(outcome("unscoped", None), "empty");
    assert_eq!(outcome("same-tenant", tenant_a), "restored");
}

fn publish_grown(b: &RealBackend, root: &Path, id: &str) {
    lease_dir(root, id);
    let cache = b.restore_cache(id, &requests()[..1], None).unwrap();
    grow_in_guest(&cache.groups[0].copy.image);
    b.settle_cache(id, true, &mut Vec::new());
}

fn gen_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(root.join("volumes"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != "leases")
        .flat_map(|c| fs::read_dir(c.path()).unwrap().flatten())
        .flat_map(|g| fs::read_dir(g.path()).unwrap().flatten())
        .map(|f| f.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("gen-"))
        .collect();
    out.sort();
    out
}

#[test]
fn release_collects_a_superseded_generation_without_cleanup() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    publish_grown(&b, dir.path(), "first");
    let old = gen_files(dir.path()).remove(0);
    let aged = old.with_file_name("gen-0000000000000000001-first000.img");
    fs::rename(&old, &aged).unwrap();
    let newer = aged.with_file_name("gen-0000000000000000002-second00.img");
    fs::copy(&aged, &newer).unwrap();
    assert_eq!(generations(dir.path()), 2);

    lease_dir(dir.path(), "third");
    let mut diag = Vec::new();
    b.destroy("third", &mut diag).unwrap();
    assert!(!aged.exists(), "{}", String::from_utf8_lossy(&diag));
    assert!(newer.exists());
    assert_eq!(generations(dir.path()), 1);
}

#[test]
fn acquire_removes_the_working_copy_of_a_lease_without_a_record() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let store = b.volume_store(None);
    fs::create_dir_all(store.work_dir("ghost")).unwrap();
    b.collect_cache_garbage(&mut Vec::new());
    assert!(!store.work_dir("ghost").exists());
}

#[test]
fn a_gc_failure_reaches_diag_and_the_restore_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    fs::create_dir_all(dir.path().join("volumes/leases/.lock")).unwrap();
    let mut diag = Vec::new();
    b.collect_cache_garbage(&mut diag);
    let note = String::from_utf8(diag).unwrap();
    assert!(note.contains("cache gc: open store lock"), "{note}");
    lease_dir(dir.path(), "l1");
    assert!(b.restore_cache("l1", &requests()[..1], None).is_ok());
}

#[test]
fn an_overlay_attaches_as_qcow2_with_its_backing_file_and_survives_a_reread() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    publish_grown(&b, dir.path(), "first");
    lease_dir(dir.path(), "second");
    b.restore_cache("second", &requests()[..1], None).unwrap();
    let cache = b.read_cache("second");
    assert_eq!(cache.groups[0].copy.format, Format::Qcow2);
    let mut vm = json!({"disks": []});
    cache.attach(&mut vm);
    assert_eq!(
        vm["disks"][0],
        json!({
            "id": "cv0",
            "path": cache.groups[0].copy.image,
            "image_type": "Qcow2",
            "backing_files": true,
            "readonly": false,
            "serial": "cbxcv0",
        })
    );
}

#[test]
fn a_cache_record_from_before_overlays_reads_as_raw() {
    let old = json!({
        "group": volumes::plan(&requests()[..1], &crate::volumes::tests::limits(40)).groups[0],
        "outcome": {"outcome": "empty"},
        "image": "/w/g1.img",
        "baseline": 7,
    });
    let g: RecordedGroup = serde_json::from_value(old).unwrap();
    assert_eq!(g.copy.format, Format::Raw);
    assert_eq!(g.copy.baseline, 7);
}

#[test]
fn a_cache_record_from_before_markers_went_reads_and_still_matches_its_plan() {
    let old = json!({
        "groups": [{
            "group": {
                "id": "g1",
                "volumes": [{"name": "npm", "paths": [{"Home": ".npm"}], "sealed": true}],
                "capacity": 21_474_836_480_u64,
            },
            "outcome": {"outcome": "empty"},
            "image": "/w/g1.img",
            "baseline": 7,
        }],
        "refused": [],
    });
    let cache: LeaseCache = serde_json::from_value(old).unwrap();
    let plan = volumes::plan(
        &[req("g1", "npm", &["~/.npm"], 0)],
        &crate::volumes::tests::limits(40),
    );
    assert_eq!(cache.groups[0].group, plan.groups[0]);
}
