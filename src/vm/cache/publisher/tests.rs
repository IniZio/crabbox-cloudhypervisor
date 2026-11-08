#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::process::Command;

use super::*;
use crate::backend::Backend;
use crate::protocol::CacheVolumeRequest;
use crate::size::Size;
use crate::test_exec::script;
use crate::vm::tests::{info, live_fixture, serve, FakeCh};
use crate::volumes::tests::{have_e2fsprogs, noise, req};
use crate::volumes::Token;

const MIB: u64 = 1 << 20;
const NO_CONTENT: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
const FAILED: &str = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\n\r\n[\"no\"]";

fn have_tools() -> bool {
    have_e2fsprogs() && Command::new("qemu-img").arg("--version").output().is_ok()
}

struct Lease {
    dir: tempfile::TempDir,
    ch: FakeCh,
    backend: RealBackend,
    record: LeaseRecord,
}

impl Lease {
    fn new(groups: &[&str], ssh_body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ch = FakeCh::spawn();
        let (mut backend, mut record) = live_fixture(dir.path(), Some(&ch));
        record.size = Some(Size {
            cpus: 1,
            memory_mib: 1,
        });
        backend.store.write(&record).unwrap();
        let ssh = dir.path().join("ssh");
        let log = dir.path().join("ssh.log");
        script(
            &ssh,
            &format!("echo \"$*\" >> {}\n{ssh_body}", log.display()),
        );
        let exe = dir.path().join("exe");
        let exe_log = dir.path().join("exe.log");
        let body = format!(
            "echo \"$* nice=$(nice) io=$(ionice)\" >> {}",
            exe_log.display()
        );
        script(&exe, &body);
        backend.config.ssh = ssh;
        backend.exe = exe;
        backend.config.qemu_img = PathBuf::from("qemu-img");
        let requests: Vec<CacheVolumeRequest> = groups
            .iter()
            .map(|g| req(g, "n", &[&format!("~/.{g}")], 1))
            .collect();
        backend.restore_cache("a", &requests, None).unwrap();
        Self {
            dir,
            ch,
            backend,
            record,
        }
    }

    fn guest_writes(&self, group: &str, bytes: u64) {
        let image = self
            .backend
            .volume_store(None)
            .work_paths(
                "a",
                &Token::parse(group).unwrap(),
                crate::volumes::store::Format::Raw,
            )
            .0;
        let blob = self.dir.path().join("blob");
        fs::write(&blob, noise(bytes)).unwrap();
        let out = Command::new("debugfs")
            .args(["-w", "-R"])
            .arg(format!("write {} blob", blob.display()))
            .arg(image)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    fn generations(&self, group: &str) -> usize {
        let dir = self
            .backend
            .volume_store(None)
            .group_dir(&Token::parse(group).unwrap());
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
            .count()
    }

    fn record_path(&self) -> PathBuf {
        self.backend.store.lease_dir("a").join(RECORD_FILE)
    }

    fn serve_pause(&self, extra: usize) -> std::thread::JoinHandle<Vec<String>> {
        let ok = NO_CONTENT.to_owned();
        let mut replies = vec![info("Running"); extra + 1];
        replies.extend([
            ok.clone(),
            info("Paused"),
            ok.clone(),
            ok.clone(),
            ok.clone(),
            ok,
        ]);
        serve(&self.record.api_socket(), replies)
    }

    fn pause_publishing(&self, diag: &mut dyn Write) {
        let api = self.serve_pause(1);
        self.backend.pause_and_publish(&self.record, diag).unwrap();
        api.join().unwrap();
    }

    fn publisher_runs(&self) -> String {
        let state = self.dir.path().join("state");
        let mut notes = Vec::new();
        run_publisher("a", &state, &mut notes).unwrap();
        String::from_utf8(notes).unwrap()
    }

    fn close(&self) -> String {
        let mut notes = Vec::new();
        self.backend.destroy("a", &mut notes).unwrap();
        String::from_utf8(notes).unwrap()
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_pause_without_the_field_syncs_nothing_records_nothing_and_a_close_publishes_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    l.guest_writes("g1", 300 * MIB);
    let api = l.serve_pause(0);
    l.backend.pause(&l.record, &mut Vec::new()).unwrap();
    api.join().unwrap();
    assert!(!l.record_path().exists());
    let at_pause = l.log("ssh.log");
    assert!(!at_pause.contains("sync -f"), "no cache sync");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(l.log("exe.log"), "", "no publisher");

    let notes = l.close();
    assert!(notes.contains("cache group g1: discarded"), "{notes}");
    assert_eq!(l.generations("g1"), 0);
    assert_eq!(
        l.log("ssh.log"),
        at_pause,
        "closing a sleeper reaches no guest"
    );
}

struct AliveAtPublisher {
    pid: u32,
    alive: Option<bool>,
}

impl Write for AliveAtPublisher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buf).contains("cache publisher started") {
            self.alive = Some(host::pid_alive(self.pid, crate::vm::CH_COMM));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_permitted_pause_syncs_each_group_then_publishes_at_low_priority_after_memory_is_free() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1", "g2"], "");
    let mut diag = AliveAtPublisher {
        pid: l.ch.child.id(),
        alive: None,
    };
    l.pause_publishing(&mut diag);
    assert_eq!(diag.alive, Some(false), "started after the VM stopped");

    let ssh = l.log("ssh.log");
    for group in ["g1", "g2"] {
        let sync = format!("timeout 5 sync -f /var/lib/crabbox-cache/{group}");
        assert!(ssh.contains(&sync), "{ssh}");
    }
    let synced = read_record(&l.backend.store.lease_dir("a")).unwrap().synced;
    assert_eq!(synced.get("g1"), Some(&true));
    assert_eq!(synced.get("g2"), Some(&true));
    wait_for("publisher ran", || !l.log("exe.log").is_empty());
    let run = l.log("exe.log");
    assert!(run.starts_with("publish a --state-dir "), "{run}");
    assert!(run.contains("nice=19"), "{run}");
    assert!(run.contains("io=idle"), "{run}");
}

#[test]
fn a_pause_succeeds_with_ssh_down_and_nothing_publishes_for_it() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "exit 255");
    l.guest_writes("g1", 300 * MIB);
    let mut notes = Vec::new();
    l.pause_publishing(&mut notes);
    let notes = String::from_utf8(notes).unwrap();
    assert!(notes.contains("cache group g1: sync failed"), "{notes}");
    assert!(notes.contains("nothing published"), "{notes}");
    assert_eq!(l.backend.power(&l.record), crate::backend::Power::Suspended);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(l.log("exe.log"), "", "no publisher without a synced group");

    assert!(l.publisher_runs().contains("not synced"));
    assert_eq!(l.generations("g1"), 0);
    l.close();
    assert_eq!(l.generations("g1"), 0);
}

#[test]
fn a_hung_sync_is_cut_off_at_its_bound_and_publishes_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "exec sleep 300");
    l.guest_writes("g1", 300 * MIB);
    let started = Instant::now();
    let synced = l.backend.sync_groups(&l.record, &mut Vec::new());
    let took = started.elapsed();
    assert_eq!(synced.get("g1"), Some(&false));
    assert!(took >= SYNC_LIMIT, "{took:?}");
    assert!(took < SYNC_LIMIT + Duration::from_secs(2), "{took:?}");

    l.pause_publishing(&mut Vec::new());
    l.publisher_runs();
    l.close();
    assert_eq!(l.generations("g1"), 0);
}

#[test]
fn a_hung_sync_notes_that_the_limit_killed_it() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "exec sleep 300");
    let mut notes = Vec::new();
    l.backend.sync_groups(&l.record, &mut notes);
    let notes = String::from_utf8(notes).unwrap();
    assert!(
        notes.contains("cache group g1: sync failed: killed at its 5s limit"),
        "{notes}"
    );
}

#[test]
fn a_permitted_pause_of_a_vm_not_running_notes_why_it_saves_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    let ok = NO_CONTENT.to_owned();
    let api = serve(
        &l.record.api_socket(),
        vec![
            info("Paused"),
            info("Paused"),
            ok.clone(),
            ok.clone(),
            ok.clone(),
            ok,
        ],
    );
    let mut notes = Vec::new();
    l.backend.pause_and_publish(&l.record, &mut notes).unwrap();
    api.join().unwrap();
    let notes = String::from_utf8(notes).unwrap();
    assert!(
        notes.contains("vm was not running at the pause; caches not synced, nothing published"),
        "{notes}"
    );
    assert_eq!(l.log("ssh.log"), "", "no sync");
    assert_eq!(l.log("exe.log"), "", "no publisher");
}

#[test]
fn one_group_failing_its_sync_costs_only_that_group() {
    if !have_tools() {
        return;
    }
    let body = "case \"$*\" in *cache/bad*) exit 1;; esac";
    let l = Lease::new(&["good", "bad"], body);
    l.guest_writes("good", 300 * MIB);
    l.guest_writes("bad", 300 * MIB);
    l.pause_publishing(&mut Vec::new());
    let notes = l.publisher_runs();
    assert!(notes.contains("cache group good: published"), "{notes}");
    assert!(notes.contains("cache group bad: not synced"), "{notes}");
    assert_eq!(l.generations("good"), 1);
    assert_eq!(l.generations("bad"), 0);
    let notes = l.close();
    assert!(notes.contains("cache group good: unchanged"), "{notes}");
    assert!(notes.contains("cache group bad: discarded"), "{notes}");
    assert_eq!(l.generations("bad"), 0);
}

#[test]
fn a_close_of_a_quiet_sleeper_finishes_what_the_sleep_left_and_repeats_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["big", "small"], "");
    l.guest_writes("big", 300 * MIB);
    l.guest_writes("small", 40 * MIB);
    l.pause_publishing(&mut Vec::new());
    let notes = l.publisher_runs();
    assert!(notes.contains("cache group small: unchanged"), "{notes}");
    assert_eq!((l.generations("big"), l.generations("small")), (1, 0));
    let notes = l.close();
    assert!(notes.contains("cache group small: published"), "{notes}");
    assert_eq!((l.generations("big"), l.generations("small")), (1, 1));
}

#[test]
fn a_pause_succeeds_with_qemu_img_missing_and_the_publisher_only_notes_it() {
    if !have_tools() {
        return;
    }
    let mut l = Lease::new(&["g1"], "");
    l.backend.config.qemu_img = PathBuf::from("/no/such/qemu-img");
    l.guest_writes("g1", 300 * MIB);
    l.pause_publishing(&mut Vec::new());
    let notes = l.publisher_runs();
    assert!(notes.contains("cache group g1: rejected"), "{notes}");
    assert_eq!(l.generations("g1"), 0);
}

fn serve_close_without_power_off(l: &Lease) -> std::thread::JoinHandle<Vec<String>> {
    let replies = vec![info("Running"), info("Running"), FAILED.to_owned()];
    serve(&l.record.api_socket(), replies)
}

#[test]
fn a_close_whose_guest_never_powers_off_still_publishes_after_a_good_sync() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    l.guest_writes("g1", 40 * MIB);
    let api = serve_close_without_power_off(&l);
    let notes = l.close();
    api.join().unwrap();
    assert!(notes.contains("cache group g1: published"), "{notes}");
    assert_eq!(l.generations("g1"), 1);
}

#[test]
fn a_close_with_neither_a_sync_nor_a_power_off_publishes_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "exit 1");
    l.guest_writes("g1", 40 * MIB);
    let api = serve_close_without_power_off(&l);
    let notes = l.close();
    api.join().unwrap();
    assert!(notes.contains("cache group g1: discarded"), "{notes}");
    assert_eq!(l.generations("g1"), 0);
}

#[test]
fn a_suspend_that_fails_records_nothing_and_starts_nothing() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    let ok = NO_CONTENT.to_owned();
    let replies = vec![
        info("Running"),
        info("Running"),
        ok.clone(),
        info("Paused"),
        FAILED.to_owned(),
        ok,
    ];
    let api = serve(&l.record.api_socket(), replies);
    let got = l.backend.pause_and_publish(&l.record, &mut Vec::new());
    api.join().unwrap();
    assert!(got.is_err());
    assert!(!l.record_path().exists());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(l.log("exe.log"), "", "no publisher for a VM still running");
}

#[test]
fn a_plain_pause_drops_a_record_left_by_an_interrupted_close() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    l.guest_writes("g1", 40 * MIB);
    let synced = BTreeMap::from([("g1".to_owned(), true)]);
    l.backend.write_publish_record("a", synced).unwrap();
    let api = l.serve_pause(0);
    l.backend.pause(&l.record, &mut Vec::new()).unwrap();
    api.join().unwrap();
    assert!(!l.record_path().exists());
    l.close();
    assert_eq!(l.generations("g1"), 0);
}

#[test]
fn a_publisher_does_nothing_without_a_record_or_when_another_holds_the_lock() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    l.guest_writes("g1", 300 * MIB);
    assert_eq!(l.publisher_runs(), "", "woken before it started");
    assert_eq!(l.generations("g1"), 0);

    let synced = BTreeMap::from([("g1".to_owned(), true)]);
    l.backend.write_publish_record("a", synced).unwrap();
    let held = open_lock(&l.backend.store.lease_dir("a")).unwrap().unwrap();
    assert!(try_hold(&held).unwrap());
    assert_eq!(l.publisher_runs(), "", "another holds it");
    assert_eq!(l.generations("g1"), 0);
}

#[test]
fn a_wake_removes_the_record_and_any_flat_copy_and_keeps_the_disk() {
    if !have_tools() {
        return;
    }
    let l = Lease::new(&["g1"], "");
    let synced = BTreeMap::from([("g1".to_owned(), true)]);
    l.backend.write_publish_record("a", synced).unwrap();
    let store = l.backend.volume_store(None);
    let flat = store.work_dir("a").join("g1.flat.tmp");
    fs::write(&flat, b"half").unwrap();
    l.backend.cancel_publish("a").unwrap();
    assert!(!l.record_path().exists());
    assert!(!flat.exists());
    assert!(store.work_dir("a").join("g1.img").exists());
}

#[test]
fn a_record_survives_a_round_trip_with_the_store_settings_a_publisher_needs() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, record) = live_fixture(dir.path(), None);
    backend.store.write(&record).unwrap();
    let synced = BTreeMap::from([("g1".to_owned(), false)]);
    backend.write_publish_record("a", synced.clone()).unwrap();
    let record = read_record(&backend.store.lease_dir("a")).unwrap();
    assert_eq!(record.cache, backend.config.cache);
    assert_eq!(record.synced, synced);
}
