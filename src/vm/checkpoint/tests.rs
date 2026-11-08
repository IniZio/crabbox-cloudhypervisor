//! Checkpoint storage and capture, with a stand-in cloud-hypervisor API and no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::process::Command;

use super::disks::{DiskOf, HeldDisk};
use super::{dir_bytes, Meta, REQUIRED};
use crate::protocol::{CheckpointMade, Desired};
use crate::size::Size;
use crate::state::LeaseRecord;
use crate::vm::tests::{config, record, FakeCh};
use crate::vm::RealBackend;
use crate::volumes::store::Format;

const CH: &str = "cloud-hypervisor v52.0";
pub(super) const SMALL: Size = Size {
    cpus: 1,
    memory_mib: 64,
};

/// True when `qemu-img` is on `PATH`; tests that write deltas skip without it.
pub(in crate::vm) fn have_qemu_img() -> bool {
    Command::new("qemu-img").arg("--version").output().is_ok()
}

/// What `qemu-img info` says of `image`.
pub(in crate::vm) fn image_info(image: &Path) -> serde_json::Value {
    let out = Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// What the guest reads from disk image `image`, flattened through its chain.
pub(in crate::vm) fn read_image(image: &Path) -> Vec<u8> {
    let out = image.with_extension("read.raw");
    let status = Command::new("qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(image)
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let bytes = fs::read(&out).unwrap();
    fs::remove_file(out).unwrap();
    bytes
}

/// A backend whose cloud-hypervisor prints [`CH`].
pub(in crate::vm) fn backend(root: &Path) -> RealBackend {
    let mut cfg = config(root);
    cfg.qemu_img = PathBuf::from("qemu-img");
    cfg.cloud_hypervisor = root.join("cloud-hypervisor");
    crate::test_exec::script(&cfg.cloud_hypervisor, &format!("echo '{CH}'"));
    RealBackend::new(cfg, PathBuf::from("/bin/cb"))
}

fn desired(id: &str) -> Desired {
    Desired {
        lease_id: id.to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
        cache_volumes: Vec::new(),
        cache_scope: None,
    }
}

/// Lays out a complete checkpoint `id` taken at `size` by `ch`.
pub(in crate::vm) fn plant(b: &RealBackend, id: &str, size: Size, ch: &str) -> PathBuf {
    plant_child(b, id, None, size, ch)
}

/// Like [`plant`], for a checkpoint whose disks are deltas over `parent`.
pub(super) fn plant_child(
    b: &RealBackend,
    id: &str,
    parent: Option<&str>,
    size: Size,
    ch: &str,
) -> PathBuf {
    let dir = b.checkpoint_dir(id);
    let root = HeldDisk {
        of: DiskOf::Root,
        format: Format::Raw,
    };
    let files = REQUIRED.iter().map(|f| dir.join(f));
    for path in files.chain([root.layer(&dir)]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    let meta = Meta {
        checkpoint_id: id.to_owned(),
        name: "n".to_owned(),
        lease_id: "src".to_owned(),
        ch_version: ch.to_owned(),
        size,
        created_at: 1,
        parent: parent.map(str::to_owned),
        disks: vec![root],
        cache: crate::vm::cache::LeaseCache::default(),
    };
    fs::write(
        dir.join(super::META_FILE),
        serde_json::to_vec(&meta).unwrap(),
    )
    .unwrap();
    dir
}

#[test]
fn a_checkpoint_lives_in_the_cache_dir_under_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    assert_eq!(
        b.checkpoint_dir("ck_1-a"),
        dir.path().join("cache/checkpoint-ck_1-a")
    );
}

#[test]
fn ids_that_could_leave_the_cache_dir_are_refused_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let mut diag = Vec::new();
    for bad in ["", "../x", "a/b", "a.b", ".."] {
        let want = "invalid checkpoint id";
        assert_eq!(b.remove_checkpoint(bad).unwrap_err(), want);
        let err = b
            .fork_checkpoint(&desired("l2"), bad, &mut diag)
            .unwrap_err();
        assert_eq!(err, want);
        let err = b.take_whole(&record("a"), bad, "n", &mut diag).unwrap_err();
        assert_eq!(err, want);
    }
}

#[test]
fn delete_removes_the_checkpoint_and_a_crashed_capture_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let ck = plant(&b, "ck1", SMALL, CH);
    let tmp = b.config.cache_dir.join("checkpoint-ck1.tmp");
    fs::create_dir_all(tmp.join("snap")).unwrap();
    let other = plant(&b, "ck2", SMALL, CH);
    b.remove_checkpoint("ck1").unwrap();
    assert!(!ck.exists() && !tmp.exists());
    assert!(other.exists(), "another checkpoint is untouched");
    b.remove_checkpoint("ck1").unwrap();
    b.remove_checkpoint("never-existed").unwrap();
}

#[test]
fn fork_of_an_unknown_checkpoint_says_so_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let err = b
        .fork_checkpoint(&desired("l2"), "ghost", &mut Vec::new())
        .unwrap_err();
    assert_eq!(err, "checkpoint not found: ghost");
    assert!(b.store.read("l2").unwrap().is_none());
    assert!(!b.config.runtime_dir.exists());
}

#[test]
fn fork_refuses_an_incomplete_checkpoint_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    let ck = plant(&b, "ck1", SMALL, CH);
    fs::remove_file(ck.join("snap/state.json")).unwrap();
    let err = b
        .fork_checkpoint(&desired("l2"), "ck1", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 is incomplete: it has no snap/state.json"
    );
    assert!(ck.exists());
}

#[test]
fn fork_refuses_a_checkpoint_taken_by_another_cloud_hypervisor() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    plant(&b, "ck1", SMALL, "cloud-hypervisor v51.0");
    let err = b
        .fork_checkpoint(&desired("l2"), "ck1", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 was taken by cloud-hypervisor v51.0, running cloud-hypervisor v52.0"
    );
}

#[test]
fn fork_refuses_a_class_that_is_not_the_checkpoints_size() {
    let dir = tempfile::tempdir().unwrap();
    let b = backend(dir.path());
    plant(&b, "ck1", SMALL, CH);
    let err = b
        .fork_checkpoint(&desired("l2"), "ck1", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 was taken at 1 cpus and 64 MiB; this lease would get 2 cpus and 4096 MiB"
    );
}

#[test]
fn dir_bytes_sums_allocated_blocks_of_every_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("snap")).unwrap();
    assert_eq!(dir_bytes(dir.path()).unwrap().allocated, 0);
    fs::write(dir.path().join("a"), vec![1u8; 100_000]).unwrap();
    fs::write(dir.path().join("snap/b"), vec![1u8; 100_000]).unwrap();
    let sparse = fs::File::create(dir.path().join("hole")).unwrap();
    sparse.set_len(1 << 30).unwrap();
    let got = dir_bytes(dir.path()).unwrap();
    assert!((200_000..400_000).contains(&got.allocated), "{got:?}");
    assert_eq!(got.apparent, 200_000 + (1 << 30));
}

/// A stand-in cloud-hypervisor API: pause, snapshot, resume and info, in any
/// order. Records `METHOD endpoint` per request and, on `vm.snapshot`, writes
/// the three snapshot files into the destination (or answers 500).
pub(in crate::vm) struct Api {
    log: Arc<Mutex<Vec<String>>>,
    thread: std::thread::JoinHandle<()>,
}

impl Api {
    pub(in crate::vm) fn start(sock: &Path, requests: usize, fail_snapshot: bool) -> Self {
        let listener = UnixListener::bind(sock).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let thread = std::thread::spawn(move || {
            let mut state = "Running";
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = text.lines().next().unwrap_or_default();
                let mut words = line.split_whitespace();
                let (method, path) = (words.next().unwrap(), words.next().unwrap());
                let endpoint = path.rsplit('/').next().unwrap();
                seen.lock().unwrap().push(format!("{method} {endpoint}"));
                let mut status = "204 No Content";
                let mut body = String::new();
                match endpoint {
                    "vm.pause" => state = "Paused",
                    "vm.resume" => state = "Running",
                    "vm.info" => {
                        status = "200 OK";
                        body = format!(r#"{{"state":"{state}"}}"#);
                    }
                    "vm.snapshot" if fail_snapshot => {
                        status = "500 Internal Server Error";
                        body = "disk full".to_owned();
                    }
                    "vm.snapshot" => {
                        let url = text.split("file://").nth(1).unwrap();
                        let dest = url.split('"').next().unwrap();
                        for f in ["config.json", "state.json", "memory-ranges"] {
                            fs::write(Path::new(dest).join(f), format!("{f} body")).unwrap();
                        }
                    }
                    other => {
                        seen.lock().unwrap().push(format!("unexpected {other}"));
                        status = "404 Not Found";
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        Self { log, thread }
    }

    pub(in crate::vm) fn finish(self) -> Vec<String> {
        self.thread.join().unwrap();
        let log = self.log.lock().unwrap().clone();
        log
    }
}

/// A backend and a running lease `a` with disk, key and host pin on disk.
pub(in crate::vm) fn running_lease(root: &Path) -> (RealBackend, LeaseRecord, FakeCh) {
    let b = backend(root);
    let ch = FakeCh::spawn();
    let mut rec = record("a");
    rec.run_dir = root.join("run-a");
    rec.size = Some(SMALL);
    rec.set_ch_pid(ch.child.id());
    fs::create_dir_all(&rec.run_dir).unwrap();
    b.store.write(&rec).unwrap();
    fs::write(b.store.disk_path("a"), vec![7u8; 65_536]).unwrap();
    fs::write(b.store.key_path("a"), "LEASE KEY\n").unwrap();
    fs::set_permissions(b.store.key_path("a"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(b.store.host_pub_path("a"), "ssh-ed25519 HOST root@x\n").unwrap();
    fs::write(b.store.known_hosts_path("a"), "stale pin\n").unwrap();
    (b, rec, ch)
}

/// Checks the files of checkpoint `ck1`.
fn assert_files(b: &RealBackend) {
    let dir = b.checkpoint_dir("ck1");
    for f in REQUIRED {
        assert!(dir.join(f).exists(), "missing {f}");
    }
    assert!(
        read_image(&dir.join("disks/root.qcow2")) == vec![7u8; 65_536],
        "the delta reads as the disk"
    );
    assert!(
        !dir.join("disks/root.frozen").exists(),
        "only the delta is kept"
    );
    assert_eq!(
        fs::read_to_string(dir.join("bootstrap")).unwrap(),
        "LEASE KEY\n"
    );
    let mode = fs::metadata(dir.join("bootstrap"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        fs::read_to_string(dir.join("known_hosts")).unwrap(),
        "crabbox-a ssh-ed25519 HOST\n",
        "the pin is rebuilt from the lease's host key under the lease's alias"
    );
}

/// Checks the meta of checkpoint `ck1` and the sizes `made` reports.
fn assert_meta(b: &RealBackend, made: &CheckpointMade) {
    let dir = b.checkpoint_dir("ck1");
    let meta: Meta =
        serde_json::from_slice(&fs::read(dir.join(super::META_FILE)).unwrap()).unwrap();
    assert_eq!(meta.checkpoint_id, "ck1");
    assert_eq!(meta.name, "after clone");
    assert_eq!(meta.lease_id, "a");
    assert_eq!(meta.ch_version, CH);
    assert_eq!(meta.size, SMALL);
    let bytes = dir_bytes(&dir).unwrap();
    assert_eq!(made.unique_bytes, bytes.allocated);
    assert_eq!(made.apparent_bytes, bytes.apparent);
    assert!(made.unique_bytes >= 65_536, "{made:?}");
}

#[test]
fn a_checkpoint_of_a_running_lease_holds_its_ram_disk_and_login_and_the_lease_resumes() {
    let root = tempfile::tempdir().unwrap();
    if !have_qemu_img() {
        return;
    }
    let (b, rec, _ch) = running_lease(root.path());
    let api = Api::start(&rec.api_socket(), 5, false);
    let mut diag = Vec::new();
    let made = b.take_whole(&rec, "ck1", "after clone", &mut diag).unwrap();
    assert_eq!(
        api.finish(),
        [
            "PUT vm.pause",
            "GET vm.info",
            "PUT vm.snapshot",
            "PUT vm.resume",
            "GET vm.info"
        ]
    );
    assert_files(&b);
    assert_meta(&b, &made);
    assert!(!b.config.cache_dir.join("checkpoint-ck1.tmp").exists());
    assert!(b.store.disk_path("a").exists(), "the lease keeps its disk");
    let diag = String::from_utf8(diag).unwrap();
    assert!(diag.contains("checkpoint ck1 written in "), "{diag}");
}

#[test]
fn the_guest_is_tidied_before_the_pause_and_a_failed_tidy_never_fails_the_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    if !have_qemu_img() {
        return;
    }
    let (b, rec, _ch) = running_lease(root.path());
    let api = Api::start(&rec.api_socket(), 5, false);
    let mut diag = Vec::new();
    b.take_whole(&rec, "ck1", "n", &mut diag).unwrap();
    api.finish();
    let diag = String::from_utf8(diag).unwrap();
    let tidy = diag
        .find("warn: guest not tidied before the snapshot")
        .unwrap();
    assert!(tidy < diag.find("vm paused").unwrap(), "{diag}");
    assert_files(&b);
}

#[test]
fn a_failed_snapshot_resumes_the_lease_and_leaves_no_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    let api = Api::start(&rec.api_socket(), 5, true);
    let err = b.take_whole(&rec, "ck1", "n", &mut Vec::new()).unwrap_err();
    assert!(err.contains("snapshot failed"), "{err}");
    assert!(err.contains("disk full"), "{err}");
    assert_eq!(
        api.finish()[3],
        "PUT vm.resume",
        "the lease must not stay paused"
    );
    assert!(!b.checkpoint_dir("ck1").exists());
    assert!(!b.config.cache_dir.join("checkpoint-ck1.tmp").exists());
}

#[test]
fn a_checkpoint_id_in_use_is_refused_and_the_old_checkpoint_kept() {
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    let old = plant(&b, "ck1", SMALL, CH);
    fs::write(old.join("kept"), "x").unwrap();
    let err = b
        .take_checkpoint(&rec, "ck1", "n", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint ck1 exists; delete it before taking it again"
    );
    assert!(old.join("kept").exists());
    assert!(!b.config.cache_dir.join("checkpoint-ck1.tmp").exists());
}

#[test]
fn a_lease_without_a_host_key_cannot_be_checkpointed_and_is_not_paused() {
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    fs::remove_file(b.store.host_pub_path("a")).unwrap();
    let err = b.take_whole(&rec, "ck1", "n", &mut Vec::new()).unwrap_err();
    assert!(err.contains("host key for a"), "{err}");
    assert!(!b.checkpoint_dir("ck1").exists());
}

#[test]
fn a_checkpoint_times_each_step_once_and_says_whether_the_lease_was_paused() {
    let root = tempfile::tempdir().unwrap();
    if !have_qemu_img() {
        return;
    }
    let (b, rec, _ch) = running_lease(root.path());
    let api = Api::start(&rec.api_socket(), 5, false);
    let mut diag = Vec::new();
    b.take_checkpoint(&rec, "ck1", "n", &mut diag).unwrap();
    api.finish();
    let mut job = Vec::new();
    super::layers::finish(&b.config.cache_dir, "ck1", &mut job).unwrap();
    let (diag, job) = (
        String::from_utf8(diag).unwrap(),
        String::from_utf8(job).unwrap(),
    );
    let steps = [
        ("lock-wait", "running"),
        ("tidy", "running"),
        ("pause", "paused"),
        ("memory-snapshot", "paused bytes="),
        ("dig-holes", "paused bytes="),
        ("hold-disk", "paused disk=root bytes="),
        ("resume", "paused"),
        ("checkpoint-total", ""),
    ];
    for (step, detail) in steps {
        let prefix = format!("timing {step} ms=");
        let lines: Vec<&str> = diag.lines().filter(|l| l.starts_with(&prefix)).collect();
        assert_eq!(
            lines.len(),
            1,
            "{step} in
{diag}"
        );
        assert!(
            lines.iter().all(|l| l.contains(detail)),
            "{step} in
{diag}"
        );
    }
    let total = diag.lines().last().unwrap_or_default();
    assert!(total.starts_with("timing checkpoint-total"), "{diag}");
    assert!(
        !diag.contains("write-layer"),
        "the caller does not wait for layers: {diag}"
    );
    assert_job_lines(&job);
}

/// Checks the layer job's timing lines in `job`.
fn assert_job_lines(job: &str) {
    let layer = "timing write-layer ms=";
    assert_eq!(
        job.lines().filter(|l| l.starts_with(layer)).count(),
        1,
        "{job}"
    );
    assert!(job.contains(" running disk=root bytes="), "{job}");
    assert!(job.contains(" method=rebase over=nothing"), "{job}");
    assert!(
        job.lines()
            .last()
            .unwrap_or_default()
            .starts_with("timing layers-total"),
        "{job}"
    );
}

impl RealBackend {
    /// [`Self::take_checkpoint`], then its layer job run here, as the detached
    /// job would; the bytes are what the whole checkpoint weighs.
    pub(in crate::vm) fn take_whole(
        &self,
        record: &LeaseRecord,
        id: &str,
        name: &str,
        diag: &mut dyn Write,
    ) -> Result<CheckpointMade, String> {
        let made = self.take_checkpoint(record, id, name, diag)?;
        super::layers::finish(&self.config.cache_dir, id, diag)?;
        let whole = dir_bytes(&self.checkpoint_dir(id))?;
        Ok(CheckpointMade {
            unique_bytes: whole.allocated,
            apparent_bytes: whole.apparent,
            ..made
        })
    }
}
