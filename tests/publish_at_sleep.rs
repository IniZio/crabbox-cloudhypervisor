//! The real publisher process against a stand-in VM: detached at a pause, and
//! killed with everything it started before a wake touches the disk.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crabbox_cloudhypervisor::backend::Backend;
use crabbox_cloudhypervisor::config::Config;
use crabbox_cloudhypervisor::host;
use crabbox_cloudhypervisor::state::{LeaseRecord, Store};
use crabbox_cloudhypervisor::vm::RealBackend;
use crabbox_cloudhypervisor::volumes::store::{Format, Store as Volumes};
use crabbox_cloudhypervisor::volumes::{self, Group, GuestPath, Token, Volume};
use serde_json::json;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_crabbox-cloudhypervisor");
const ID: &str = "lease-a";
const MIB: usize = 1 << 20;

fn have_tools() -> bool {
    let tools = "mke2fs e2fsck debugfs qemu-img ionice nice setsid";
    Command::new("sh")
        .args(["-c", &format!("command -v {tools}")])
        .output()
        .unwrap()
        .status
        .success()
}

fn script(path: &Path, body: &str) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"cat >"$1" && chmod 755 "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn serve_vm(socket: &Path, mut vm: Child) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let mut state = "Running";
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..n]);
            let line = text.lines().next().unwrap_or_default().to_owned();
            if line.contains("vm.pause") {
                state = "Paused";
            }
            let reply = if line.contains("vm.info") {
                let body = format!(r#"{{"state":"{state}"}}"#);
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned()
            };
            let _ = stream.write_all(reply.as_bytes());
            if line.contains("vmm.shutdown") {
                let _ = vm.kill();
                let _ = vm.wait();
                return;
            }
        }
    });
}

struct Sleeper {
    dir: TempDir,
    backend: RealBackend,
    record: LeaseRecord,
    group: Group,
    volumes: Volumes,
}

fn config_in(root: &Path, qemu_img: &Path) -> Config {
    let guest_ssh = root.join("guest-ssh");
    script(&guest_ssh, "exit 0");
    let config = json!({
        "stateDir": root.join("state"),
        "runtimeDir": root.join("run"),
        "cacheDir": root.join("cache"),
        "cacheVolumeDir": root.join("volumes"),
        "cloudHypervisor": "/bin/false",
        "passt": "/bin/false",
        "ssh": guest_ssh,
        "qemuImg": qemu_img,
        "cgroup": "off",
    });
    Config::from_request(&config, &|_| None).unwrap()
}

fn a_group() -> Group {
    Group {
        id: Token::parse("g1").unwrap(),
        volumes: vec![Volume {
            name: Token::parse("n").unwrap(),
            paths: vec![GuestPath::Home(".n".to_owned())],
        }],
        capacity: 2 * volumes::GIB,
    }
}

fn guest_writes(root: &Path, image: &Path, bytes: usize) {
    let blob = root.join("blob");
    fs::write(&blob, vec![7u8; bytes]).unwrap();
    let wrote = Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!("write {} blob", blob.display()))
        .arg(image)
        .output()
        .unwrap();
    assert!(wrote.status.success());
}

fn stand_in_vm(root: &Path) -> Child {
    let stand_in = root.join("cloud-hypervisor");
    script(&stand_in, "read _");
    let vm = Command::new(&stand_in)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = vm.id();
    wait_for("the stand-in VM runs", 5, || {
        host::pid_alive(pid, "cloud-hyperviso")
    });
    vm
}

fn record_of(root: &Path, pid: u32) -> LeaseRecord {
    let run_dir = root.join("run").join("r");
    fs::create_dir_all(&run_dir).unwrap();
    let mut record = LeaseRecord {
        lease_id: ID.to_owned(),
        slug: "slug".to_owned(),
        name: "name".to_owned(),
        cloud_id: format!("ch-{ID}"),
        created_at: 1,
        ch_pid: None,
        passt_pid: None,
        run_dir,
        boot_id: None,
        ch_start: None,
        passt_start: None,
        watch_pid: None,
        watch_start: None,
        size: Some(crabbox_cloudhypervisor::size::Size {
            cpus: 1,
            memory_mib: 1,
        }),
    };
    record.set_ch_pid(pid);
    record
}

impl Sleeper {
    fn new(dir: TempDir, qemu_img: &Path, bytes: usize) -> Self {
        let root = dir.path();
        let config = config_in(root, qemu_img);
        let group = a_group();
        let volumes = Volumes::new(&config.cache, qemu_img, &config.image.sha256, None);
        let restored = volumes.restore(&group, ID);
        guest_writes(root, &restored.copy.image, bytes);
        let vm = stand_in_vm(root);
        let record = record_of(root, vm.id());
        let store = Store::new(&root.join("state"));
        store.write(&record).unwrap();
        let cache = json!({
            "groups": [{
                "group": group,
                "outcome": {"outcome": "empty"},
                "image": restored.copy.image,
                "baseline": restored.copy.baseline,
            }],
            "refused": [],
        });
        fs::write(
            store.lease_dir(ID).join("cache.json"),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        serve_vm(&record.api_socket(), vm);
        let backend = RealBackend::new(config, PathBuf::from(BIN));
        Self {
            dir,
            backend,
            record,
            group,
            volumes,
        }
    }

    fn lease_dir(&self) -> PathBuf {
        Store::new(&self.dir.path().join("state")).lease_dir(ID)
    }

    fn generations(&self) -> usize {
        fs::read_dir(self.volumes.group_dir(&self.group.id))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
            .count()
    }

    fn lock_is_free(&self) -> bool {
        fs::File::open(self.lease_dir().join("publish.lock"))
            .unwrap()
            .try_lock()
            .is_ok()
    }

    fn flat_copies(&self) -> usize {
        fs::read_dir(self.volumes.work_dir(ID))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".flat.tmp"))
            .count()
    }
}

fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_detached_publisher_publishes_after_the_pause_has_returned() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let l = Sleeper::new(dir, Path::new("qemu-img"), 300 * MIB);
    let mut diag = Vec::new();
    l.backend.pause_and_publish(&l.record, &mut diag).unwrap();
    let notes = String::from_utf8_lossy(&diag);
    assert!(notes.contains("cache publisher started"), "{notes}");
    wait_for("a generation appears", 60, || l.generations() == 1);
    wait_for("the publisher exits", 20, || l.lock_is_free());
    let said = fs::read_to_string(l.lease_dir().join("publish.log")).unwrap();
    assert!(said.contains("cache group g1: published"), "{said}");
    assert_eq!(l.flat_copies(), 0);
    let (disk, _) = l.volumes.work_paths(ID, &l.group.id, Format::Raw);
    assert!(disk.exists(), "the lease keeps its disk");
    l.backend.destroy(ID, &mut Vec::new()).unwrap();
    assert_eq!(l.generations(), 1, "a close repeats nothing");
}

fn stuck_in_flatten() -> (Sleeper, Vec<u32>) {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let slow = dir.path().join("slow-qemu-img");
    let body = format!(
        "sleep 300 &\nchild=$!\n: > \"${{*##* }}\"\necho \"$$ $child\" >> {}\nwait $child",
        pids.display()
    );
    script(&slow, &body);
    let l = Sleeper::new(dir, &slow, 300 * MIB);
    l.backend
        .pause_and_publish(&l.record, &mut Vec::new())
        .unwrap();
    wait_for("the publisher reaches its flatten", 30, || pids.exists());
    assert_eq!(l.flat_copies(), 1, "mid-publish");
    assert!(!l.lock_is_free(), "the publisher holds its lock");
    let text = fs::read_to_string(&pids).unwrap();
    let running: Vec<u32> = text
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    assert_eq!(running.len(), 2, "{text}");
    assert!(
        running.iter().all(|p| host::pid_alive(*p, "")),
        "{running:?}"
    );
    (l, running)
}

#[test]
fn a_child_of_a_publisher_that_died_alone_still_holds_the_wake_back_until_it_is_killed() {
    if !have_tools() {
        return;
    }
    let (l, running) = stuck_in_flatten();
    let lock = fs::read_to_string(l.lease_dir().join("publish.lock")).unwrap();
    let leader: u32 = lock.split(' ').next().unwrap().parse().unwrap();
    host::kill(leader, rustix::process::Signal::KILL).unwrap();
    wait_for("the publisher dies", 5, || !host::pid_alive(leader, ""));
    assert!(!l.lock_is_free(), "its children still hold the lock");

    let waking = l.backend.resume(&l.record, &mut Vec::new());

    assert!(waking.is_err(), "the stand-in cannot restore");
    assert!(
        running.iter().all(|p| !host::pid_alive(*p, "")),
        "{running:?}"
    );
    assert!(l.lock_is_free());
    assert_eq!(l.generations(), 0);
}

#[test]
fn a_close_waits_for_a_running_publisher_and_does_not_publish_again() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let slow = dir.path().join("slow-qemu-img");
    let body = format!(
        "echo \"$1\" >> {}\nsleep 2\nexec qemu-img \"$@\"",
        calls.display()
    );
    script(&slow, &body);
    let l = Sleeper::new(dir, &slow, 300 * MIB);
    l.backend
        .pause_and_publish(&l.record, &mut Vec::new())
        .unwrap();
    wait_for("the publisher reaches its flatten", 30, || calls.exists());

    l.backend.destroy(ID, &mut Vec::new()).unwrap();

    assert_eq!(l.generations(), 1);
    let said = fs::read_to_string(&calls).unwrap();
    assert_eq!(said.lines().count(), 1, "one flatten: {said}");
}

#[test]
fn a_wake_kills_the_publisher_and_all_it_started_and_nothing_is_half_published() {
    if !have_tools() {
        return;
    }
    let (l, running) = stuck_in_flatten();
    let lease_lock = l.backend.try_lock(ID).unwrap();
    assert!(
        lease_lock.is_some(),
        "a publisher leaves the lease lock free"
    );
    drop(lease_lock);

    let waking = l.backend.resume(&l.record, &mut Vec::new());

    assert!(
        waking
            .as_ref()
            .is_err_and(|e| e.contains("restore from snapshot failed")),
        "the stand-in cannot restore, but only after the publisher: {waking:?}"
    );
    assert!(
        running.iter().all(|p| !host::pid_alive(*p, "")),
        "{running:?}"
    );
    assert!(l.lock_is_free());
    assert_eq!(l.generations(), 0);
    assert_eq!(l.flat_copies(), 0);
    assert!(!l.lease_dir().join("pause-publish.json").exists());
    assert!(
        l.lease_dir().join("snap").is_dir(),
        "the wake has its snapshot"
    );
}
