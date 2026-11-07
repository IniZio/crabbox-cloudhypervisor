//! A checkpoint's layer job, with real qemu-img and a stand-in job process.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{finish, in_progress, wait_ready};
use crate::state::LeaseRecord;
use crate::vm::checkpoint::read_meta;
use crate::vm::checkpoint::tests::{have_qemu_img, running_lease, Api};
use crate::vm::tests::FakeCh;
use crate::vm::RealBackend;
use crate::volumes::store::Format;

/// A running lease on a backend whose layer job is a script running `body`.
fn lease_with_job(root: &Path, body: &str) -> (RealBackend, LeaseRecord, FakeCh) {
    let (b, rec, ch) = running_lease(root);
    let job = root.join("job");
    crate::test_exec::script(&job, body);
    (RealBackend::new(b.config, job), rec, ch)
}

/// Takes checkpoint `id` and returns as soon as the lease runs again.
fn create(b: &RealBackend, rec: &LeaseRecord, id: &str) -> crate::protocol::CheckpointMade {
    let _ = fs::remove_file(rec.api_socket());
    let api = Api::start(&rec.api_socket(), 5, false);
    let made = b.take_checkpoint(rec, id, "n", &mut Vec::new()).unwrap();
    api.finish();
    made
}

fn tmp_of(b: &RealBackend, id: &str) -> PathBuf {
    b.config.cache_dir.join(format!("checkpoint-{id}.tmp"))
}

/// Pids of live processes in the group `job.pid` names.
fn job_group(b: &RealBackend, id: &str) -> Option<u32> {
    let text = fs::read_to_string(tmp_of(b, id).join("layers.lock")).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

fn group_alive(group: u32) -> bool {
    fs::read_dir("/proc").unwrap().flatten().any(|e| {
        let stat = fs::read_to_string(e.path().join("stat")).unwrap_or_default();
        let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
        let f: Vec<&str> = after.split_whitespace().collect();
        f.first() != Some(&"Z") && f.get(2) == Some(&group.to_string().as_str())
    })
}

fn wait_until(what: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if what() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_create_returns_before_its_layers_and_a_fork_cannot_read_it_yet() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "sleep 30");
    let made = create(&b, &rec, "ck1");
    assert!(!b.checkpoint_dir("ck1").exists(), "not whole yet");
    assert!(in_progress(&tmp_of(&b, "ck1")), "its job holds the lock");
    let err = wait_ready(&b.config.cache_dir, "ck1", Duration::from_millis(300)).unwrap_err();
    assert!(err.contains("still being written"), "{err}");
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    assert!(!tmp_of(&b, "ck1").exists(), "the job moved the dir");
    assert!(
        made.unique_bytes >= 65_536,
        "the frozen root counts: {made:?}"
    );
    wait_ready(&b.config.cache_dir, "ck1", Duration::ZERO).unwrap();
}

#[test]
fn a_waiting_fork_goes_on_once_the_layers_are_whole() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "sleep 30");
    create(&b, &rec, "ck1");
    let cache = b.config.cache_dir.clone();
    let waiter = std::thread::spawn(move || {
        let began = Instant::now();
        wait_ready(&cache, "ck1", Duration::from_secs(20)).map(|()| began.elapsed())
    });
    std::thread::sleep(Duration::from_millis(500));
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    let took = waiter.join().unwrap().unwrap();
    assert!(took >= Duration::from_millis(400), "{took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
}

#[test]
fn deleting_a_checkpoint_being_written_kills_its_job_and_leaves_nothing() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "sleep 30 & sleep 30");
    create(&b, &rec, "ck1");
    let group = job_group(&b, "ck1").unwrap();
    assert!(group_alive(group));
    let began = Instant::now();
    b.remove_checkpoint("ck1").unwrap();
    assert!(began.elapsed() < Duration::from_secs(10));
    assert!(!group_alive(group), "the job and what it started are gone");
    assert!(!tmp_of(&b, "ck1").exists() && !b.checkpoint_dir("ck1").exists());
}

#[test]
fn a_job_that_died_leaves_a_checkpoint_never_ready_until_retaken() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "exit 0");
    create(&b, &rec, "ck1");
    let tmp = tmp_of(&b, "ck1");
    assert!(wait_until(|| !in_progress(&tmp)), "the job exited");
    let err = wait_ready(&b.config.cache_dir, "ck1", Duration::from_secs(5)).unwrap_err();
    assert!(err.contains("was never finished"), "{err}");
    let desired = crate::vm::checkpoint::disks::tests::desired("f", Vec::new());
    let err = b
        .fork_checkpoint(&desired, "ck1", &mut Vec::new())
        .unwrap_err();
    assert!(err.contains("was never finished"), "{err}");
    fs::write(tmp.join("left-by-the-crash"), "x").unwrap();
    create(&b, &rec, "ck1");
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    assert!(b.checkpoint_dir("ck1").is_dir() && !tmp.exists());
    assert!(!b.checkpoint_dir("ck1").join("left-by-the-crash").exists());
}

#[test]
fn a_child_writes_its_layers_only_over_a_whole_parent() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "sleep 30");
    create(&b, &rec, "ck1");
    fs::write(b.store.disk_path("a"), vec![9u8; 65_536]).unwrap();
    let made = create(&b, &rec, "ck2");
    assert_eq!(
        made.parent_checkpoint_id.as_deref(),
        Some("ck1"),
        "a pending parent counts"
    );
    let err = b.remove_checkpoint("ck1").unwrap_err();
    assert!(err.contains("ck2"), "{err}");
    let cache = b.config.cache_dir.clone();
    let child = std::thread::spawn(move || finish(&cache, "ck2", &mut Vec::new()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !b.checkpoint_dir("ck2").exists(),
        "the child waits for its parent"
    );
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    child.join().unwrap().unwrap();
    let dir = b.checkpoint_dir("ck2");
    let layer = dir.join("disks/root.qcow2");
    let info = crate::vm::checkpoint::tests::image_info(&layer);
    let backing = info
        .get("backing-filename")
        .and_then(serde_json::Value::as_str);
    assert_eq!(backing, Some("../../checkpoint-ck1/disks/root.qcow2"));
    assert_eq!(read_meta(&dir).unwrap().parent.as_deref(), Some("ck1"));
    let image = crate::vm::checkpoint::tests::read_image(&layer);
    assert_eq!(image, vec![9u8; 65_536]);
}

#[test]
fn the_job_needs_nothing_of_the_lease_so_a_release_cannot_break_it() {
    if !have_qemu_img() || !crate::volumes::tests::have_e2fsprogs() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = crate::vm::checkpoint::disks::tests::lease(root.path(), Format::Qcow2);
    let job = root.path().join("job");
    crate::test_exec::script(&job, "exit 0");
    let b = RealBackend::new(b.config, job);
    let image = b.volume_store(None).work_dir("a").join("g1.qcow2");
    let want = root.path().join("want.raw");
    let flat = |from: &Path, to: &Path| {
        let ok = std::process::Command::new("qemu-img")
            .args(["convert", "-O", "raw"])
            .arg(from)
            .arg(to)
            .status()
            .unwrap();
        assert!(ok.success());
    };
    flat(&image, &want);
    create(&b, &rec, "ck1");
    fs::remove_dir_all(b.volume_store(None).work_dir("a")).unwrap();
    fs::remove_dir_all(b.store.lease_dir("a")).unwrap();
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    let layer = b.checkpoint_dir("ck1").join("disks/cache-g1.qcow2");
    let same = std::process::Command::new("qemu-img")
        .args(["compare", "-q", "-F", "raw"])
        .arg(&layer)
        .arg(&want)
        .status()
        .unwrap();
    assert!(same.success(), "the layer reads as the lease's cache disk");
}

#[test]
fn a_checkpoint_being_written_cannot_be_taken_again() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_with_job(root.path(), "sleep 30");
    create(&b, &rec, "ck1");
    let _ = fs::remove_file(rec.api_socket());
    let err = b
        .take_checkpoint(&rec, "ck1", "n", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 is being written; delete it before taking it again"
    );
    assert!(in_progress(&tmp_of(&b, "ck1")), "the first job carries on");
    finish(&b.config.cache_dir, "ck1", &mut Vec::new()).unwrap();
    assert!(b.checkpoint_dir("ck1").is_dir());
}
