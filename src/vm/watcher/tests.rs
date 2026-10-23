//! Tests for starting, finding and stopping the growth watcher.

#![allow(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::backend::Backend;
use crate::host;
use crate::state::LeaseRecord;
use crate::vm::tests::{config, record};
use crate::vm::RealBackend;

/// A record for `id` whose watcher is the process `pid`, stamped in this boot.
fn watched(id: &str, pid: u32) -> LeaseRecord {
    LeaseRecord {
        boot_id: host::boot_id(),
        watch_pid: Some(pid),
        watch_start: host::proc_start(pid),
        ..record(id)
    }
}

#[test]
fn the_watcher_is_known_by_pid_and_start_time_whatever_the_binary_name() {
    let me = std::process::id();
    assert!(RealBackend::watch_alive(&watched("w1", me)));
    let reused = LeaseRecord {
        watch_start: Some(1),
        ..watched("w1", me)
    };
    assert!(
        !RealBackend::watch_alive(&reused),
        "a reused pid is not ours"
    );
}

#[test]
fn ensure_watcher_leaves_a_running_watcher_alone() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/false"));
    let rec = watched("w2", std::process::id());
    backend.store.write(&rec).unwrap();
    backend.ensure_watcher(&rec, &mut Vec::new()).unwrap();
    assert_eq!(backend.store.read("w2").unwrap().unwrap(), rec);
}

#[test]
fn ensure_watcher_starts_one_when_the_recorded_watcher_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/true"));
    let gone = LeaseRecord {
        watch_start: Some(1),
        ..watched("w3", std::process::id())
    };
    backend.store.write(&gone).unwrap();
    backend.ensure_watcher(&gone, &mut Vec::new()).unwrap();
    let now = backend.store.read("w3").unwrap().unwrap();
    assert_ne!(now.watch_pid, gone.watch_pid);
    assert!(backend.store.watch_spec_path("w3").exists());
}

#[test]
fn destroy_stops_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/false"));
    std::fs::create_dir_all(backend.store.lease_dir("w4")).unwrap();
    let log = dir.path().join("sleep.log");
    let args = [std::ffi::OsString::from("30")];
    let pid = host::spawn_detached(Path::new("sleep"), &args, &log).unwrap();
    assert!(
        host::wait_until_dead(|| !host::pid_alive(pid, "sleep"), Duration::from_secs(2)),
        "sleep did not start"
    );
    backend.store.write(&watched("w4", pid)).unwrap();
    backend.destroy("w4", &mut Vec::new()).unwrap();
    assert!(
        host::wait_gone(pid, "", Duration::from_secs(3)),
        "watcher {pid} outlived destroy"
    );
}
