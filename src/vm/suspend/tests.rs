#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::path::Path;

use super::check_room;
use crate::backend::{Backend, Power};
use crate::config::PauseMode;
use crate::size::Size;
use crate::state::LeaseRecord;
use crate::vm::tests::{info, live_fixture, serve, FakeCh};
use crate::vm::RealBackend;

const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

/// A disk-pause backend and a persisted 1 MiB record, so the free-space check passes.
fn disk_fixture(dir: &Path, ch: Option<&FakeCh>) -> (RealBackend, LeaseRecord) {
    let (mut backend, mut rec) = live_fixture(dir, ch);
    backend.config.pause = PauseMode::Disk;
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
fn failed_snapshot_leaves_the_vm_paused_and_no_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = disk_fixture(dir.path(), Some(&ch));
    let fail = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\n\r\n[\"no\"]";
    let seen = serve(&rec.api_socket(), vec![info("Paused"), fail.to_owned()]);
    let err = backend.pause(&rec, &mut Vec::new()).unwrap_err();
    seen.join().unwrap();
    assert!(err.contains("vm left paused in memory"), "{err}");
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
    assert!(err.contains("vm left paused in memory"), "{err}");
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
