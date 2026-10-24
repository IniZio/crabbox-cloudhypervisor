#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::process::Command;

use super::*;
use crate::volumes::tests::{have_e2fsprogs, limits, noise};
use crate::volumes::{GuestPath, Volume};

const MIB: u64 = 1024 * 1024;

fn store(root: &Path) -> Store {
    let mut cfg = limits(40);
    cfg.volume_dir = root.to_path_buf();
    Store::new(&cfg, "abcdef0123456789")
}

fn group(id: &str, capacity: u64) -> Group {
    Group {
        id: Token::parse(id).unwrap(),
        volumes: vec![Volume {
            name: Token::parse("npm").unwrap(),
            paths: vec![GuestPath::Home(".npm".to_owned())],
        }],
        capacity,
    }
}

/// Writes `bytes` of random data into `image` at `/npm/<name>` without mounting it.
fn guest_writes(image: &Path, name: &str, bytes: u64) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("blob");
    fs::write(&src, noise(bytes)).unwrap();
    let cmd = format!("mkdir npm\nwrite {} npm/{name}\n", src.display());
    let script = dir.path().join("cmds");
    fs::write(&script, cmd).unwrap();
    let out = Command::new("debugfs")
        .arg("-w")
        .arg("-f")
        .arg(&script)
        .arg(image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Size of `/npm/<name>` inside `image`, read without mounting it.
fn guest_size(image: &Path, name: &str) -> Option<u64> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat npm/{name}"))
        .arg(image)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let after = text.split("Size: ").nth(1)?;
    after.split_whitespace().next()?.parse().ok()
}

fn debugfs_w(image: &Path, request: &str) {
    let out = Command::new("debugfs")
        .arg("-w")
        .arg("-R")
        .arg(request)
        .arg(image)
        .output()
        .unwrap();
    assert!(out.status.success());
}

fn gens(store: &Store, id: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(store.group_dir(&Token::parse(id).unwrap()))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("gen-"))
        .collect();
    names.sort();
    names
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn first_restore_is_an_empty_sparse_working_copy() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let first = s.restore(&group("g1", GIB), "lease-one");
    assert_eq!(first.outcome, Outcome::Empty);
    assert_eq!(fs::metadata(&first.image).unwrap().len(), GIB);
    assert!(first.image.starts_with(s.work_dir("lease-one")));
    assert!(first.baseline < 32 * MIB);
}

/// Restores, writes `bytes` in the guest, publishes at `secs`.
fn publish_one(s: &Store, g: &Group, lease: &str, file: &str, secs: u64) -> Publish {
    let r = s.restore(g, lease);
    guest_writes(&r.image, file, 40 * MIB);
    s.publish(&g.id, &r.image, r.baseline, lease, at(secs))
}

#[test]
fn a_grown_copy_publishes_and_the_next_lease_restores_it() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g1", GIB);
    let published = publish_one(&s, &g, "lease-one", "pkg", 1_729_781_203);
    let generation = 1_729_781_203_000_000_000;
    assert_eq!(published, Publish::Published { generation });
    assert_eq!(gens(&s, "g1"), ["gen-1729781203000000000-lease-on.img"]);
    assert!(!s.work_dir("lease-one").join("g1.img").exists());

    let second = s.restore(&g, "lease-two");
    assert_eq!(second.outcome, Outcome::Restored { generation });
    assert_eq!(guest_size(&second.image, "pkg"), Some(40 * MIB));
    assert_eq!(
        gens(&s, "g1").len(),
        1,
        "restore never consumes a generation"
    );
}

#[test]
fn restore_picks_the_newest_generation_and_grows_it_to_capacity() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let small = group("g", 256 * MIB);
    for (lease, file, t) in [("a", "old", 10), ("b", "new", 20)] {
        let got = publish_one(&s, &small, lease, file, t);
        assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    }
    let big = group("g", 512 * MIB);
    let r = s.restore(&big, "c");
    assert_eq!(
        r.outcome,
        Outcome::Restored {
            generation: 20_000_000_000
        }
    );
    assert_eq!(guest_size(&r.image, "new"), Some(40 * MIB));
    assert_eq!(fs::metadata(&r.image).unwrap().len(), 512 * MIB);
    assert_eq!(ext4::check(&r.image).unwrap().total * 4096, 512 * MIB);
}

/// A fresh working copy of a 1 GiB group after the guest wrote `bytes`.
fn written(s: &Store, lease: &str, bytes: u64) -> Restored {
    let r = s.restore(&group("g", GIB), lease);
    guest_writes(&r.image, "pkg", bytes);
    r
}

fn publish_at(s: &Store, r: &Restored, lease: &str) -> Publish {
    s.publish(
        &Token::parse("g").unwrap(),
        &r.image,
        r.baseline,
        lease,
        at(1),
    )
}

#[test]
fn publish_without_growth_is_unchanged() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let r = written(&s, "idle", MIB);
    assert_eq!(publish_at(&s, &r, "idle"), Publish::Unchanged);
    assert!(gens(&s, "g").is_empty());
}

#[test]
fn publish_rejects_a_failed_fsck() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let r = written(&s, "corrupt", 40 * MIB);
    debugfs_w(&r.image, "set_bg 0 free_blocks_count 1");
    let got = publish_at(&s, &r, "corrupt");
    assert!(
        matches!(&got, Publish::Rejected { reason } if reason.starts_with("fsck")),
        "{got:?}"
    );
    assert!(gens(&s, "g").is_empty());
}

#[test]
fn publish_rejects_an_unclean_unmount() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let r = written(&s, "killed", 40 * MIB);
    debugfs_w(&r.image, "feature needs_recovery");
    let reason = "not cleanly unmounted".to_owned();
    assert_eq!(publish_at(&s, &r, "killed"), Publish::Rejected { reason });
    assert!(gens(&s, "g").is_empty());
}

#[test]
fn publish_rejects_a_full_group() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let tight = group("t", 128 * MIB);
    let r = s.restore(&tight, "full");
    let usage = ext4::check(&r.image).unwrap();
    guest_writes(&r.image, "pkg", (usage.total - usage.used) * 4096 - 2 * MIB);
    let got = s.publish(&tight.id, &r.image, r.baseline, "full", at(4));
    assert!(
        matches!(&got, Publish::Rejected { reason } if reason.starts_with("full")),
        "{got:?}"
    );
    assert!(gens(&s, "t").is_empty());
}

#[test]
fn a_failed_restore_is_skipped_and_leaves_no_working_copy() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", GIB);
    fs::create_dir_all(s.group_dir(&g.id)).unwrap();
    fs::write(
        s.group_dir(&g.id).join("gen-0000000000000000005-x.img"),
        b"not ext4",
    )
    .unwrap();
    let r = s.restore(&g, "lease");
    assert!(
        matches!(&r.outcome, Outcome::Skipped { .. }),
        "{:?}",
        r.outcome
    );
    assert!(!r.image.exists());
}

fn snap(dir: &str, used: u64, gens: &[(u64, u64)]) -> GroupSnapshot {
    GroupSnapshot {
        dir: PathBuf::from(dir),
        used,
        gens: gens
            .iter()
            .map(|(ns, bytes)| Generation {
                path: PathBuf::from(format!("{dir}/gen-{ns}")),
                ns: *ns,
                bytes: *bytes,
            })
            .collect(),
    }
}

#[test]
fn gc_drops_superseded_then_idle_then_least_recently_used() {
    let hour = 3_600_000_000_000u64;
    let day = 24 * hour;
    let now = 100 * day;
    let groups = [
        snap(
            "/s/a",
            now - hour,
            &[(now - 3 * hour, 5), (now - 2 * hour, 5), (now - 10, 5)],
        ),
        snap("/s/idle", now - 31 * day, &[(now - 31 * day, 50)]),
        snap("/s/lru", now - 5 * day, &[(now - 5 * day, 40)]),
        snap("/s/recent", now - day, &[(now - day, 40)]),
    ];
    let removed = plan_gc(&groups, now, Duration::from_hours(720), 60);
    let removed: Vec<&str> = removed.iter().map(|p| p.to_str().unwrap()).collect();
    let first_gen = format!("/s/a/gen-{}", now - 3 * hour);
    assert_eq!(removed, [first_gen.as_str(), "/s/idle", "/s/lru"]);

    let all_fit = plan_gc(&groups[2..], now, Duration::from_hours(720), 80);
    assert!(all_fit.is_empty());
}

#[test]
fn gc_removes_working_copies_of_dead_leases_and_honours_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    for id in ["live", "gone"] {
        fs::create_dir_all(s.work_dir(id)).unwrap();
    }
    let live = |id: &str| id == "live";
    let dry = s.gc(at(1), &live, true);
    assert_eq!(dry.removed, [s.work_dir("gone")]);
    assert!(s.work_dir("gone").exists());
    let real = s.gc(at(1), &live, false);
    assert_eq!(real.removed, [s.work_dir("gone")]);
    assert!(!s.work_dir("gone").exists());
    assert!(s.work_dir("live").exists());
}
