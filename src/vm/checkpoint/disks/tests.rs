//! Every disk of a checkpoint, root and cache alike: held as a delta over the
//! parent's, forked into files that name no checkpoint. Needs qemu-img and
//! e2fsprogs; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{DiskOf, HeldDisk};
use crate::protocol::{CacheVolumeRequest, CheckpointMade, Desired};
use crate::state::LeaseRecord;
use crate::vm::cache::LeaseCache;
use crate::vm::checkpoint::read_meta;
use crate::vm::checkpoint::tests::{have_qemu_img, image_info, running_lease, Api};
use crate::vm::spec::base_raw_path;
use crate::vm::tests::{record, FakeCh};
use crate::vm::RealBackend;
use crate::volumes::store::{Format, WorkingCopy};
use crate::volumes::tests::{have_e2fsprogs, noise, req};
use crate::volumes::Token;

const MIB: u64 = 1 << 20;

pub(in crate::vm) fn npm() -> Vec<CacheVolumeRequest> {
    vec![req("g1", "npm", &["~/.npm"], 1)]
}

pub(in crate::vm) fn desired(id: &str, cache_volumes: Vec<CacheVolumeRequest>) -> Desired {
    Desired {
        lease_id: id.to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
        cache_volumes,
        cache_scope: None,
    }
}

fn tools() -> bool {
    have_qemu_img() && have_e2fsprogs()
}

fn g1() -> Token {
    Token::parse("g1").unwrap()
}

/// Writes `len` bytes of `byte` at `at` into `image` through its format, as a guest would.
fn scribble(image: &Path, format: Format, at: u64, len: u64, byte: u8) {
    let fmt = match format {
        Format::Raw => "raw",
        Format::Qcow2 => "qcow2",
    };
    let out = Command::new("qemu-io")
        .args(["-f", fmt, "-c", &format!("write -P {byte} {at} {len}")])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Publishes a generation of group g1 from a throwaway lease.
fn publish_generation(b: &RealBackend) {
    fs::create_dir_all(b.store.lease_dir("p")).unwrap();
    let cache = b.restore_cache("p", &npm(), None).unwrap();
    let blob = tempfile::NamedTempFile::new().unwrap();
    fs::write(blob.path(), noise(40 * MIB)).unwrap();
    let wrote = Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!("write {} blob", blob.path().display()))
        .arg(&first(&cache).image)
        .output()
        .unwrap();
    assert!(wrote.status.success());
    b.settle_cache("p", true, &mut Vec::new());
}

/// A running lease `a` on a 1 MiB base image with cache group g1: an overlay
/// over a generation, or a raw disk when the store has none.
pub(in crate::vm) fn lease(root: &Path, format: Format) -> (RealBackend, LeaseRecord, FakeCh) {
    let (b, rec, ch) = running_lease(root);
    let base = noise(MIB);
    fs::create_dir_all(&b.config.cache_dir).unwrap();
    fs::write(base_raw_path(&b.config), &base).unwrap();
    let mut disk = base;
    disk.resize(2 << 20, 0);
    fs::write(b.store.disk_path("a"), disk).unwrap();
    if format == Format::Qcow2 {
        publish_generation(&b);
    }
    let cache = b.restore_cache("a", &npm(), None).unwrap();
    assert_eq!(first(&cache).format, format);
    (b, rec, ch)
}

/// Checkpoints lease `a` under `id` against a fresh stand-in API.
pub(in crate::vm) fn take(b: &RealBackend, rec: &LeaseRecord, id: &str) -> CheckpointMade {
    let _ = fs::remove_file(rec.api_socket());
    let api = Api::start(&rec.api_socket(), 5, false);
    let made = b.take_whole(rec, id, "n", &mut Vec::new()).unwrap();
    api.finish();
    made
}

/// True when the guest reads the same bytes from images `a` and `b`.
fn same_content(a: &Path, b: &Path) -> bool {
    Command::new("qemu-img")
        .args(["compare", "-q"])
        .arg(a)
        .arg(b)
        .status()
        .unwrap()
        .success()
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

fn allocated(path: &Path) -> u64 {
    fs::metadata(path).unwrap().blocks() * 512
}

fn first(cache: &LeaseCache) -> &WorkingCopy {
    cache.copies().next().unwrap().1
}

fn cache_image(b: &RealBackend, lease: &str) -> PathBuf {
    first(&b.read_cache(lease)).image.clone()
}

/// A raw copy of what `image` reads as, at `to`.
fn flatten_to(image: &Path, to: &Path) {
    let ok = Command::new("qemu-img")
        .args(["convert", "-q", "-O", "raw", "-S", "4k"])
        .arg(image)
        .arg(to)
        .status()
        .unwrap();
    assert!(ok.success());
}

#[test]
fn a_checkpoint_holds_root_and_cache_disks_the_same_way_root_first() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let image = cache_image(&b, "a");
    let made = take(&b, &rec, "ck1");
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    assert_eq!(
        meta.disks,
        [
            HeldDisk {
                of: DiskOf::Root,
                format: Format::Raw,
            },
            HeldDisk {
                of: DiskOf::Cache(g1()),
                format: Format::Qcow2,
            },
        ]
    );
    let disks = dir.join("disks");
    for (stem, live, base) in [
        ("root", b.store.disk_path("a"), base_raw_path(&b.config)),
        (
            "cache-g1",
            image.clone(),
            image.with_file_name("g1.base.img"),
        ),
    ] {
        let layer = disks.join(format!("{stem}.qcow2"));
        assert_eq!(
            image_info(&layer)["backing-filename"],
            format!("{stem}.base.img")
        );
        assert_eq!(
            inode(&disks.join(format!("{stem}.base.img"))),
            inode(&base),
            "{stem}: what the disk started from is linked, not copied"
        );
        assert!(same_content(&layer, &live), "{stem} reads as the lease's");
    }
    assert!(
        made.unique_bytes < 4 * MIB,
        "linked bases are not this checkpoint's: {made:?}"
    );
}

#[test]
fn every_later_layer_of_a_disk_holds_only_what_changed_since_the_parent() {
    if !tools() {
        return;
    }
    for format in [Format::Raw, Format::Qcow2] {
        let root = tempfile::tempdir().unwrap();
        let (b, rec, _ch) = lease(root.path(), format);
        let image = cache_image(&b, "a");
        scribble(&image, format, 64 * MIB, 32 * MIB, 1);
        take(&b, &rec, "ck1");
        scribble(&image, format, 200 * MIB, MIB, 2);
        let made = take(&b, &rec, "ck2");
        assert_eq!(made.parent_checkpoint_id.as_deref(), Some("ck1"));
        let layer = b.checkpoint_dir("ck2").join("disks/cache-g1.qcow2");
        assert_eq!(
            image_info(&layer)["backing-filename"],
            "../../checkpoint-ck1/disks/cache-g1.qcow2"
        );
        assert!(
            same_content(&layer, &image),
            "{format:?}: ck2 reads as the disk"
        );
        assert!(
            allocated(&layer) < 4 * MIB,
            "{format:?}: a 1 MiB change makes a {} byte layer",
            allocated(&layer)
        );
        assert!(made.unique_bytes < 8 * MIB, "{format:?}: {made:?}");
    }
}

/// Checks lease `f`'s root disk starts with `want` and has the configured size.
fn assert_root_of_f(b: &RealBackend, want: &[u8]) {
    let mut got = vec![0; want.len()];
    let mut disk = fs::File::open(b.store.disk_path("f")).unwrap();
    disk.read_exact(&mut got).unwrap();
    assert_eq!(disk.metadata().unwrap().len(), b.config.disk_gib << 30);
    assert!(got == want, "the root reads as the lease's");
}

/// Deletes checkpoints ck2 and ck1, the base image and every generation of g1.
fn evict_chain_and_bases(b: &RealBackend) {
    b.remove_checkpoint("ck2").unwrap();
    b.remove_checkpoint("ck1").unwrap();
    fs::remove_file(base_raw_path(&b.config)).unwrap();
    for gen in fs::read_dir(b.volume_store(None).group_dir(&g1()))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("gen-"))
    {
        fs::remove_file(gen.path()).unwrap();
    }
}

#[test]
fn a_fork_runs_on_its_own_files_and_outlives_the_chain_and_the_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let image = cache_image(&b, "a");
    scribble(&image, Format::Qcow2, 64 * MIB, 8 * MIB, 1);
    take(&b, &rec, "ck1");
    scribble(&image, Format::Qcow2, 100 * MIB, MIB, 2);
    take(&b, &rec, "ck2");
    let want_cache = root.path().join("want-cache.img");
    flatten_to(&image, &want_cache);
    let want_root = fs::read(b.store.disk_path("a")).unwrap();

    let dir = b.checkpoint_dir("ck2");
    let meta = read_meta(&dir).unwrap();
    b.store.write(&record("f")).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut Vec::new())
        .unwrap();
    let copy = first(&cache);
    let work = b.volume_store(None).work_dir("f");
    assert_eq!(copy.format, Format::Qcow2);
    assert_eq!(copy.image, work.join("g1.qcow2"));
    let base = fs::canonicalize(work.join("g1.base.img")).unwrap();
    assert_eq!(
        image_info(&copy.image)["backing-filename"],
        base.to_str().unwrap(),
        "the fork's overlay reads its generation through its own link"
    );
    assert_eq!(
        b.read_cache("f"),
        cache,
        "the fork's record names its copies"
    );
    let info = image_info(&b.store.disk_path("f"));
    assert_eq!(info["format"], "raw");
    assert!(info.get("backing-filename").is_none(), "{info}");

    evict_chain_and_bases(&b);
    assert!(same_content(&copy.image, &want_cache));
    assert_root_of_f(&b, &want_root);
}

#[test]
fn a_fork_of_a_raw_cache_disk_runs_a_raw_disk_of_its_own() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Raw);
    let image = cache_image(&b, "a");
    scribble(&image, Format::Raw, 64 * MIB, MIB, 1);
    take(&b, &rec, "ck1");
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    b.store.write(&record("f")).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut Vec::new())
        .unwrap();
    let copy = first(&cache);
    assert_eq!(copy.format, Format::Raw);
    assert_eq!(
        copy.image,
        b.volume_store(None).work_dir("f").join("g1.img")
    );
    assert!(image_info(&copy.image).get("backing-filename").is_none());
    b.remove_checkpoint("ck1").unwrap();
    assert!(same_content(&copy.image, &image));
}

/// A lease on a generation, checkpointed once, its record for fork `f` written.
fn checkpointed(root: &Path) -> (RealBackend, LeaseRecord, FakeCh, PathBuf) {
    let (b, rec, ch) = lease(root, Format::Qcow2);
    let image = cache_image(&b, "a");
    scribble(&image, Format::Qcow2, 64 * MIB, MIB, 1);
    take(&b, &rec, "ck1");
    b.store.write(&record("f")).unwrap();
    (b, rec, ch, image)
}

#[test]
fn a_fork_commits_its_chain_and_says_so() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, _rec, _ch, image) = checkpointed(root.path());
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    let mut diag = Vec::new();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut diag)
        .unwrap();
    let diag = String::from_utf8(diag).unwrap();
    assert!(diag.contains(" group=g1 bytes="), "{diag}");
    assert!(diag.contains(" method=commit"), "{diag}");
    assert!(!diag.contains("rebasing"), "{diag}");
    assert!(same_content(&first(&cache).image, &image));
}

#[test]
fn a_chain_over_another_copy_of_the_base_falls_back_to_a_rebase_and_says_so() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, _rec, _ch, image) = checkpointed(root.path());
    let dir = b.checkpoint_dir("ck1");
    let held = dir.join("disks/cache-g1.base.img");
    let other = held.with_extension("img.other");
    fs::copy(&held, &other).unwrap();
    let status = Command::new("qemu-img")
        .args(["rebase", "-q", "-u", "-f", "qcow2", "-F", "raw", "-b"])
        .arg("cache-g1.base.img.other")
        .arg(dir.join("disks/cache-g1.qcow2"))
        .status()
        .unwrap();
    assert!(status.success());
    let meta = read_meta(&dir).unwrap();
    let mut diag = Vec::new();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut diag)
        .unwrap();
    let diag = String::from_utf8(diag).unwrap();
    assert!(
        diag.contains("does not end in the checkpoint's base; rebasing"),
        "{diag}"
    );
    assert!(diag.contains(" method=rebase"), "{diag}");
    assert!(same_content(&first(&cache).image, &image));
}

#[test]
fn a_committed_fork_publishes_and_the_generation_restores() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, _rec, _ch, _image) = checkpointed(root.path());
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut Vec::new())
        .unwrap();
    let forked = first(&cache).image.clone();
    scribble(&forked, Format::Qcow2, 128 * MIB, 40 * MIB, 3);
    let mut diag = Vec::new();
    b.settle_cache("f", true, &mut diag);
    let diag = String::from_utf8(diag).unwrap();
    assert!(!diag.contains("failed"), "{diag}");
    fs::create_dir_all(b.store.lease_dir("r")).unwrap();
    let restored = b.restore_cache("r", &npm(), None).unwrap();
    let image = &first(&restored).image;
    assert!(diag.contains("published gen"), "{diag}");
    for (at, len, byte) in [(64, 1, 1), (128, 40, 3)] {
        let command = format!("read -P {byte} {at}M {len}M");
        let out = Command::new("qemu-io")
            .args(["-f", "qcow2", "-c", &command])
            .arg(image)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(!said.contains("failed"), "{command}: {said}");
        assert!(said.contains("read "), "{command}: {said}");
    }
}

#[test]
fn a_checkpoint_of_a_committed_fork_chains_on_its_parent() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, _rec, _ch, _image) = checkpointed(root.path());
    let dir = b.checkpoint_dir("ck1");
    let meta = read_meta(&dir).unwrap();
    let mut f = record("f");
    f.run_dir = root.path().join("run-f");
    f.size = Some(crate::vm::checkpoint::tests::SMALL);
    let ch = FakeCh::spawn();
    f.set_ch_pid(ch.child.id());
    fs::create_dir_all(&f.run_dir).unwrap();
    b.store.write(&f).unwrap();
    let cache = b
        .fork_disks("f", &dir, &meta, &desired("f", npm()), &mut Vec::new())
        .unwrap();
    fs::write(b.store.disk_path("f"), vec![7u8; 65_536]).unwrap();
    fs::write(b.store.key_path("f"), "LEASE KEY\n").unwrap();
    fs::write(b.store.host_pub_path("f"), "ssh-ed25519 HOST root@x\n").unwrap();
    b.set_parent("f", "ck1", &mut Vec::new());
    let forked = first(&cache).image.clone();
    scribble(&forked, Format::Qcow2, 70 * MIB, MIB, 5);
    let made = take(&b, &f, "ck2");
    assert_eq!(made.parent_checkpoint_id.as_deref(), Some("ck1"));
    let layer = b.checkpoint_dir("ck2").join("disks/cache-g1.qcow2");
    assert_eq!(
        image_info(&layer)["backing-filename"],
        "../../checkpoint-ck1/disks/cache-g1.qcow2"
    );
    assert!(same_content(&layer, &forked));
}

#[test]
fn removing_a_checkpoint_drops_its_link_to_the_generation() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    let base = cache_image(&b, "a").with_file_name("g1.base.img");
    let before = fs::metadata(&base).unwrap().nlink();
    take(&b, &rec, "ck1");
    assert_eq!(fs::metadata(&base).unwrap().nlink(), before + 1);
    b.remove_checkpoint("ck1").unwrap();
    assert_eq!(fs::metadata(&base).unwrap().nlink(), before);
}

#[test]
fn a_checkpoint_in_the_layout_before_disks_is_refused_so_the_caller_starts_cold() {
    let root = tempfile::tempdir().unwrap();
    let b = crate::vm::checkpoint::tests::backend(root.path());
    let dir = b.checkpoint_dir("old");
    fs::create_dir_all(&dir).unwrap();
    let old = r#"{"checkpointId":"old","name":"n","leaseId":"src","chVersion":"cloud-hypervisor v52.0",
        "size":{"cpus":2,"memory_mib":4096},"createdAt":1}"#;
    fs::write(dir.join("checkpoint.json"), old).unwrap();
    fs::write(dir.join("disk.qcow2"), "x").unwrap();
    let err = b
        .fork_checkpoint(&desired("f", Vec::new()), "old", &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "checkpoint old is unreadable: its checkpoint.json is bad or from an older layout"
    );
    assert!(b.store.read("f").unwrap().is_none());
}

/// Answers a restoring cloud-hypervisor's API at `sock` once its dir exists.
fn serve_restored_vm(sock: PathBuf) {
    std::thread::spawn(move || {
        while !sock.parent().unwrap().is_dir() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let mut state = "Paused";
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            if text.contains("vm.resume") {
                state = "Running";
            }
            let body = format!(r#"{{"state":"{state}"}}"#);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
}

/// Steps every fork with a hot-added cache disk times.
const FORK_STEPS: [&str; 13] = [
    "lock-wait",
    "cache-gc",
    "root-flatten",
    "cache-disk",
    "stage-snapshot",
    "process-start",
    "memory-restore",
    "resume",
    "add-disk",
    "identity-ssh-wait",
    "identity-rotation",
    "wait-ready",
    "fork-total",
];

#[test]
fn a_fork_logs_each_step_once() {
    if !tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease(root.path(), Format::Qcow2);
    take(&b, &rec, "ck1");
    let config = r#"{"payload":{"firmware":""},"disks":[{"path":""},{"path":""}],
        "serial":{"file":""},"net":[{"vhost_socket":""}],"vsock":{"socket":""}}"#;
    fs::write(b.checkpoint_dir("ck1").join("snap/config.json"), config).unwrap();

    let mut cfg = b.config;
    cfg.cpus = Some(1);
    cfg.memory_mib = Some(64);
    cfg.passt = root.path().join("passt");
    cfg.ssh = root.path().join("ssh");
    cfg.ssh_keygen = PathBuf::from("ssh-keygen");
    let exe = root.path().join("watch");
    let write = crate::test_exec::script;
    let version = "cloud-hypervisor v52.0";
    let ch = format!("case \"$1\" in --version) echo '{version}' ;; *) sleep 20 & wait ;; esac");
    write(&cfg.cloud_hypervisor, &ch);
    write(&cfg.passt, ": >\"$4\"; sleep 20 & wait");
    write(&exe, "sleep 20 & wait");
    let refused = "echo 'crabbox@x: Permission denied (publickey).' >&2; exit 255";
    let ssh = format!("case \"$*\" in *bootstrap*' true') {refused} ;; esac; exit 0");
    write(&cfg.ssh, &ssh);
    let run_dir = cfg.runtime_dir.join(crate::state::run_dir_name("f"));
    let b = RealBackend::new(cfg, exe);
    let mut fork = record("f");
    fork.run_dir = run_dir;
    serve_restored_vm(fork.api_socket());

    let request = r#"{"protocolVersion":1,"operation":"checkpointFork","checkpointId":"ck1",
        "config":{},"desired":{"leaseId":"f","slug":"s","name":"n","cacheVolumes":[
        {"group":"g1","name":"npm","key":"g1-npm","paths":["~/.npm"],"sizeGB":1}]}}"#;
    let mut diag = Vec::new();
    let outcome = crate::run_with(request.as_bytes(), &b, &mut diag);
    let diag = String::from_utf8(diag).unwrap();
    if let Ok(Some(forked)) = b.store.read("f") {
        let _ = RealBackend::stop_processes(&forked, &mut Vec::new());
    }
    assert_eq!(outcome.exit_code, 0, "{}\n{diag}", outcome.stdout);

    for step in FORK_STEPS {
        let prefix = format!("timing {step} ms=");
        let lines = diag.lines().filter(|l| l.starts_with(&prefix)).count();
        assert_eq!(lines, 1, "{step} in\n{diag}");
    }
    assert!(diag.contains(" group=g1 bytes="), "{diag}");
    assert!(diag.contains("restored from checkpoint ck1 in "), "{diag}");
}
