#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::path::Path;

use super::check_room;
use crate::backend::{Backend, Power};
use crate::size::Size;
use crate::state::LeaseRecord;
use crate::vm::tests::{info, live_fixture, serve, FakeCh};
use crate::vm::RealBackend;

const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

/// A backend and a persisted 1 MiB record, so the free-space check passes.
fn disk_fixture(dir: &Path, ch: Option<&FakeCh>) -> (RealBackend, LeaseRecord) {
    let (backend, mut rec) = live_fixture(dir, ch);
    rec.size = Some(Size {
        cpus: 1,
        memory_mib: 1,
    });
    backend.store.write(&rec).unwrap();
    (backend, rec)
}

fn snap(backend: &RealBackend) -> std::path::PathBuf {
    backend.snapshot_dir("a")
}

#[test]
fn suspend_snapshots_then_stops_the_vm() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let ok = NO_CONTENT.to_owned();
    let replies = vec![info("Running"), ok.clone(), info("Paused"), ok.clone()];
    let seen = serve(&rec.api_socket(), [replies, vec![ok; 3]].concat());
    backend.pause(&rec, &mut Vec::new()).unwrap();
    let lines = seen.join().unwrap();
    let at = |ep: &str| lines.iter().position(|l| l.contains(ep)).expect(ep);
    assert!(at("vm.pause") < at("vm.snapshot"), "{lines:?}");
    assert!(at("vm.snapshot") < at("vm.shutdown"), "{lines:?}");
    assert!(at("vm.shutdown") < at("vmm.shutdown"), "{lines:?}");
    assert!(snap(&backend).is_dir(), "snapshot renamed into place");
    assert!(!backend.store.lease_dir("a").join("snap.tmp").exists());
    assert!(!crate::host::pid_alive(
        ch.child.id(),
        super::super::CH_COMM
    ));
    assert_eq!(backend.power(&rec), Power::Suspended);
}

#[test]
fn failed_snapshot_errors_and_leaves_the_vm_running() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let fail = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\n\r\n[\"no\"]";
    let ok = NO_CONTENT.to_owned();
    let replies = vec![
        info("Running"),
        ok.clone(),
        info("Paused"),
        fail.to_owned(),
        ok,
    ];
    let seen = serve(&rec.api_socket(), replies);
    let err = backend.pause(&rec, &mut Vec::new()).unwrap_err();
    let lines = seen.join().unwrap();
    assert!(err.contains("snapshot failed, pause aborted"), "{err}");
    assert!(
        lines.last().unwrap().contains("vm.resume"),
        "vm resumed after the failed snapshot: {lines:?}"
    );
    assert!(!snap(&backend).exists());
    assert!(!backend.store.lease_dir("a").join("snap.tmp").exists());
    assert!(crate::host::pid_alive(ch.child.id(), super::super::CH_COMM));
}

#[test]
fn snapshot_needs_guest_ram_plus_a_spare_gib_free() {
    let mib = 1024 * 1024;
    assert!(check_room(3 * 1024 * mib, 2048).is_ok());
    let err = check_room(3 * 1024 * mib - 1, 2048).unwrap_err();
    assert!(err.contains("need 3072 MiB"), "{err}");
    assert!(err.contains("pause aborted"), "{err}");
}

#[test]
fn not_enough_disk_resumes_the_running_vm() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, mut rec) = disk_fixture(dir.path(), Some(&ch));
    rec.size = Some(Size {
        cpus: 1,
        memory_mib: u64::MAX / (1024 * 1024),
    });
    let ok = NO_CONTENT.to_owned();
    let replies = vec![info("Running"), ok.clone(), info("Paused"), ok];
    let seen = serve(&rec.api_socket(), replies);
    let err = backend.pause(&rec, &mut Vec::new()).unwrap_err();
    let lines = seen.join().unwrap();
    assert!(err.contains("not enough free disk"), "{err}");
    assert!(lines.last().unwrap().contains("vm.resume"), "{lines:?}");
    assert!(!snap(&backend).exists());
}

#[test]
fn memory_paused_lease_from_an_older_adapter_suspends_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let ok = NO_CONTENT.to_owned();
    let seen = serve(
        &rec.api_socket(),
        vec![info("Paused"), ok.clone(), ok.clone(), ok.clone(), ok],
    );
    backend.pause(&rec, &mut Vec::new()).unwrap();
    let lines = seen.join().unwrap();
    assert!(!lines.iter().any(|l| l.contains("vm.pause")), "{lines:?}");
    assert!(snap(&backend).is_dir());
    assert_eq!(backend.power(&rec), Power::Suspended);
}

#[test]
fn half_written_snapshot_is_not_a_suspended_lease() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, rec) = disk_fixture(dir.path(), None);
    std::fs::create_dir_all(backend.store.lease_dir("a").join("snap.tmp")).unwrap();
    assert_eq!(backend.power(&rec), Power::Stopped);
    std::fs::create_dir_all(snap(&backend)).unwrap();
    assert_eq!(backend.power(&rec), Power::Suspended);
}

#[test]
fn failed_restore_keeps_the_snapshot_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, rec) = disk_fixture(dir.path(), None);
    std::fs::create_dir_all(snap(&backend)).unwrap();
    std::fs::write(snap(&backend).join("state.json"), "{}").unwrap();
    let err = backend.bring_up(&rec, &mut Vec::new()).unwrap_err();
    assert!(
        err.contains("restore from snapshot failed; snapshot kept"),
        "{err}"
    );
    assert!(snap(&backend).join("state.json").exists());
    assert_eq!(backend.power(&rec), Power::Suspended);
}

#[test]
fn release_deletes_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, _) = disk_fixture(dir.path(), None);
    std::fs::create_dir_all(snap(&backend)).unwrap();
    backend.destroy("a", &mut Vec::new()).unwrap();
    assert!(!snap(&backend).exists());
}

#[test]
fn restore_starts_cloud_hypervisor_from_the_snapshot() {
    let args = crate::vm::spec::ch_args(Path::new("/r/a/a"), Some(Path::new("/s/snap")));
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--api-socket",
            "/r/a/a",
            "--restore",
            "source_url=file:///s/snap"
        ]
    );
    assert_eq!(crate::vm::spec::ch_args(Path::new("/x"), None).len(), 2);
}

#[test]
fn failed_hole_digging_only_warns() {
    let dir = tempfile::tempdir().unwrap();
    let mut diag = Vec::new();
    super::dig_snapshot_holes(Path::new("/no/such/fallocate"), dir.path(), &mut diag);
    let text = String::from_utf8(diag).unwrap();
    assert!(text.contains("snapshot kept dense"), "{text}");
}

/// Records whether the VM process was alive when the hole digging reported.
struct AliveAtDig {
    pid: u32,
    alive: Option<bool>,
}

impl std::io::Write for AliveAtDig {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buf).contains("snapshot kept dense") {
            self.alive = Some(crate::host::pid_alive(self.pid, super::super::CH_COMM));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Replies for a pause of a running VM, from `vm.info` through `vmm.shutdown`.
fn pause_replies() -> Vec<String> {
    let ok = NO_CONTENT.to_owned();
    vec![
        info("Running"),
        ok.clone(),
        info("Paused"),
        ok.clone(),
        ok.clone(),
        ok.clone(),
        ok,
    ]
}

#[test]
fn suspend_frees_the_vm_before_digging_holes() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let seen = serve(&rec.api_socket(), pause_replies());
    let mut diag = AliveAtDig {
        pid: ch.child.id(),
        alive: None,
    };
    backend.pause(&rec, &mut diag).unwrap();
    seen.join().unwrap();
    assert_eq!(diag.alive, Some(false), "vm stopped when holes were dug");
}

#[test]
fn failed_hole_digging_still_leaves_a_suspended_lease() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let seen = serve(&rec.api_socket(), pause_replies());
    let mut diag = Vec::new();
    backend.pause(&rec, &mut diag).unwrap();
    seen.join().unwrap();
    let text = String::from_utf8(diag).unwrap();
    assert!(text.contains("snapshot kept dense"), "{text}");
    assert!(snap(&backend).is_dir());
    assert_eq!(backend.power(&rec), Power::Suspended);
}

#[test]
fn crash_before_digging_holes_leaves_a_dense_snapshot_to_restore() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, rec) = disk_fixture(dir.path(), None);
    std::fs::create_dir_all(snap(&backend)).unwrap();
    let memory = snap(&backend).join("memory-ranges");
    std::fs::write(&memory, vec![0_u8; 4096]).unwrap();
    assert_eq!(backend.power(&rec), Power::Suspended);
    let err = backend.bring_up(&rec, &mut Vec::new()).unwrap_err();
    assert!(err.contains("restore from snapshot failed"), "{err}");
    assert_eq!(std::fs::read(&memory).unwrap().len(), 4096);
}

#[test]
fn resuming_in_place_after_a_crash_before_stop_drops_the_stale_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    std::fs::create_dir_all(snap(&backend)).unwrap();
    let ok = NO_CONTENT.to_owned();
    let seen = serve(&rec.api_socket(), vec![info("Paused"), ok, info("Running")]);
    assert!(backend.bring_up(&rec, &mut Vec::new()).unwrap());
    let lines = seen.join().unwrap();
    assert!(lines.iter().any(|l| l.contains("vm.resume")), "{lines:?}");
    assert!(
        !snap(&backend).exists(),
        "guest disk moved past the snapshot"
    );
}

#[test]
fn resuming_takes_the_snapshots_allocated_memory_plus_overhead() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, rec) = disk_fixture(dir.path(), None);
    assert_eq!(backend.resume_memory_mib(&rec), None);
    std::fs::create_dir_all(snap(&backend)).unwrap();
    assert_eq!(backend.resume_memory_mib(&rec), None);
    let memory = std::fs::File::create(snap(&backend).join("memory-ranges")).unwrap();
    std::io::Write::write_all(&mut &memory, &vec![1_u8; 2 << 20]).unwrap();
    memory.set_len(1 << 30).unwrap();
    memory.sync_all().unwrap();
    let got = backend.resume_memory_mib(&rec).unwrap();
    assert!((66..=70).contains(&got), "2 MiB allocated gave {got} MiB");
}
