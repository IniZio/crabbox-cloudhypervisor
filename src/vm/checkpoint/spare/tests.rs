//! Spare roots with real qemu-img and a real background job; no KVM.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::{room_for, take, Spare};
use crate::protocol::Desired;
use crate::state::LeaseRecord;
use crate::vm::checkpoint::disks::tests::take as checkpoint;
use crate::vm::checkpoint::disks::HeldDisk;
use crate::vm::checkpoint::read_meta;
use crate::vm::checkpoint::tests::{have_qemu_img, running_lease};
use crate::vm::tests::{record, FakeCh};
use crate::vm::RealBackend;

const GIB: u64 = 1 << 30;

/// A lease checkpointed as `ck1`, on a backend whose lease disks are 1 GiB.
fn checkpointed(root: &Path) -> (RealBackend, LeaseRecord, FakeCh) {
    let (b, rec, ch) = running_lease(root);
    checkpoint(&b, &rec, "ck1");
    let mut cfg = b.config;
    cfg.disk_gib = 1;
    (RealBackend::new(cfg, PathBuf::from("/bin/cb")), rec, ch)
}

fn root_of(b: &RealBackend) -> (PathBuf, HeldDisk) {
    let dir = b.checkpoint_dir("ck1");
    let root = read_meta(&dir).unwrap().disks.remove(0);
    (dir, root)
}

/// Forks lease `id`'s disks from `ck1`; the `method=` its root took.
fn fork(b: &RealBackend, id: &str) -> String {
    b.store.write(&record(id)).unwrap();
    let (dir, _) = root_of(b);
    let meta = read_meta(&dir).unwrap();
    let desired = Desired {
        lease_id: id.to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
        cache_volumes: Vec::new(),
        cache_scope: None,
    };
    let mut diag = Vec::new();
    b.fork_disks(id, &dir, &meta, &desired, &mut diag).unwrap();
    let diag = String::from_utf8(diag).unwrap();
    let line = diag
        .lines()
        .find(|l| l.starts_with("timing root-flatten"))
        .unwrap();
    line.rsplit("method=").next().unwrap().to_owned()
}

/// Whether lease `id`'s root reads as the checkpoint's root, grown to 1 GiB.
fn root_is_right(b: &RealBackend, id: &str) -> bool {
    let (dir, root) = root_of(b);
    let disk = b.store.disk_path(id);
    fs::metadata(&disk).unwrap().len() == GIB
        && Command::new("qemu-img")
            .args(["compare", "-q", "-f", "raw", "-F", "qcow2"])
            .arg(&disk)
            .arg(root.layer(&dir))
            .status()
            .unwrap()
            .success()
}

/// How many live processes are in process group `group`.
fn in_group(group: u32) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|e| fs::read_to_string(e.path().join("stat")).ok())
            .filter(|stat| {
                let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
                let fields: Vec<&str> = after.split_whitespace().collect();
                fields.first() != Some(&"Z") && fields.get(2) == Some(&group.to_string().as_str())
            })
            .count();
        if left == 0 || Instant::now() > deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn refill(b: &RealBackend) -> Spare {
    let (dir, root) = root_of(b);
    let mut diag = Vec::new();
    b.refill_spare(&dir, &root, 0, &mut diag);
    assert!(diag.is_empty(), "{}", String::from_utf8_lossy(&diag));
    Spare::of(&dir, &root)
}

#[test]
fn the_fork_after_a_refill_takes_the_spare_and_reads_the_same_root() {
    if !have_qemu_img() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = checkpointed(tmp.path());
    assert_eq!(fork(&b, "f1"), "flatten");
    let spare = refill(&b);
    assert!(wait_for(&spare.file), "the job made a spare");
    assert!(!spare.tmp.exists() && !spare.job.exists());
    assert_eq!(fork(&b, "f2"), "spare");
    assert!(!spare.file.exists(), "the fork took it");
    assert!(root_is_right(&b, "f1") && root_is_right(&b, "f2"));
}

#[test]
fn one_spare_goes_to_one_of_two_racing_forks() {
    let tmp = tempfile::tempdir().unwrap();
    let spare = tmp.path().join("root.spare.raw");
    for round in 0..300 {
        fs::write(&spare, vec![1u8; 4096]).unwrap();
        let images = [tmp.path().join("a"), tmp.path().join("b")];
        let barrier = std::sync::Barrier::new(2);
        let won: Vec<bool> = std::thread::scope(|s| {
            let racing: Vec<_> = images
                .iter()
                .map(|image| {
                    s.spawn(|| {
                        barrier.wait();
                        take(&spare, image, 4096).unwrap()
                    })
                })
                .collect();
            racing.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(
            won.iter().filter(|w| **w).count(),
            1,
            "round {round}: {won:?}"
        );
        for image in &images {
            let _ = fs::remove_file(image);
        }
    }
}

#[test]
fn a_spare_of_another_size_is_dropped_and_the_fork_flattens() {
    if !have_qemu_img() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = checkpointed(tmp.path());
    let (dir, root) = root_of(&b);
    let spare = Spare::of(&dir, &root);
    fs::write(&spare.file, vec![9u8; 4096]).unwrap();
    assert_eq!(fork(&b, "f1"), "flatten");
    assert!(!spare.file.exists());
    assert!(root_is_right(&b, "f1"));
}

#[test]
fn a_slow_job_blocks_no_fork_and_dies_with_its_checkpoint() {
    if !have_qemu_img() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = checkpointed(tmp.path());
    let mut cfg = b.config.clone();
    cfg.qemu_img = tmp.path().join("slow-qemu-img");
    crate::test_exec::script(
        &cfg.qemu_img,
        "case \"$1\" in convert) sleep 30 ;; esac\nexec qemu-img \"$@\"",
    );
    let slow = RealBackend::new(cfg, PathBuf::from("/bin/cb"));
    let spare = refill(&slow);
    let pid = spare.running().expect("the job runs");
    let started = Instant::now();
    assert_eq!(fork(&b, "f1"), "flatten", "no spare yet, so it copies");
    let mut again = Vec::new();
    let (dir, root) = root_of(&slow);
    slow.refill_spare(&dir, &root, 0, &mut again);
    assert_eq!(spare.running(), Some(pid), "a second refill starts no job");
    slow.remove_checkpoint("ck1").unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "nothing waited on the job"
    );
    assert_eq!(in_group(pid), 0, "the job and all it started are gone");
    assert!(!dir.exists(), "no spare, no copy, no record left");
}

#[test]
fn a_crashed_jobs_leftovers_are_replaced_by_the_next() {
    if !have_qemu_img() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = checkpointed(tmp.path());
    let (dir, root) = root_of(&b);
    let spare = Spare::of(&dir, &root);
    fs::write(&spare.tmp, "half a root").unwrap();
    fs::write(&spare.job, "4194300 1 no-such-boot").unwrap();
    refill(&b);
    assert!(wait_for(&spare.file));
    assert!(!spare.tmp.exists());
    assert_eq!(fork(&b, "f1"), "spare");
    assert!(root_is_right(&b, "f1"));
}

#[test]
fn a_crashed_copy_beside_a_finished_spare_is_removed() {
    if !have_qemu_img() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = checkpointed(tmp.path());
    let (dir, root) = root_of(&b);
    let spare = Spare::of(&dir, &root);
    fs::write(&spare.file, "a finished spare").unwrap();
    fs::write(&spare.tmp, "half a root").unwrap();
    refill(&b);
    assert!(!spare.tmp.exists() && spare.file.exists());
}

#[test]
fn a_spare_must_leave_the_host_above_its_floor() {
    let total = 1000 * GIB;
    assert!(room_for(300 * GIB, total, 50 * GIB, 20));
    assert!(
        !room_for(130 * GIB, total, 40 * GIB, 20),
        "100 GiB is the floor"
    );
    assert!(
        room_for(140 * GIB, total, 40 * GIB, 20),
        "exactly at the floor"
    );
    assert!(room_for(GIB, total, 40 * GIB, 0), "a floor of 0 is off");
}
