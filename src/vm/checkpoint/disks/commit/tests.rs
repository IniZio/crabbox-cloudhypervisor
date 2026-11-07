//! A fork's cache overlay, with real qemu-img and qemu-io; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::{commit_chain, ends_in, read_chain};
use crate::vm::checkpoint::tests::{have_qemu_img, image_info};
use crate::volumes::tests::noise;

const MIB: u64 = 1 << 20;
const QEMU_IMG: &str = "qemu-img";

/// Runs `qemu-io` on `image`.
fn io(image: &Path, command: &str) {
    let out = Command::new("qemu-io")
        .args(["-f", "qcow2", "-c", command])
        .arg(image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn have_tools() -> bool {
    have_qemu_img() && Command::new("qemu-io").arg("--version").output().is_ok()
}

/// A base in `root` and `layers` qcow2 layers named as a checkpoint chain names
/// them, each writing its own 1 MiB and 64 KiB of a shared place; the top also
/// zeroes 1 MiB of what the base holds.
struct Chain {
    /// The base's link in the bottom layer's dir.
    base: PathBuf,
    /// Every layer, bottom first.
    layers: Vec<PathBuf>,
}

impl Chain {
    fn new(root: &Path, layers: usize) -> Self {
        let dir = |i: usize| root.join(format!("checkpoint-c{i}/disks"));
        let base = root.join("generation");
        fs::write(&base, noise(8 * MIB)).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&base).unwrap();
        file.set_len(64 * MIB).unwrap();
        let mut made = Vec::new();
        for i in 0..layers {
            fs::create_dir_all(dir(i)).unwrap();
            fs::hard_link(&base, dir(i).join("g.base.img")).unwrap();
            let top = dir(i).join("g.qcow2");
            let (format, backing) = if i == 0 {
                ("raw", "g.base.img".to_owned())
            } else {
                (
                    "qcow2",
                    format!("../../checkpoint-c{}/disks/g.qcow2", i - 1),
                )
            };
            let status = Command::new(QEMU_IMG)
                .args(["create", "-q", "-f", "qcow2", "-F", format, "-b", &backing])
                .arg(&top)
                .status()
                .unwrap();
            assert!(status.success());
            let at = 16 * MIB + (i as u64) * 2 * MIB;
            io(&top, &format!("write -P {} {at} 1M", i + 1));
            io(&top, &format!("write -P {} 0 64k", i + 1));
            made.push(top);
        }
        let top = made.last().unwrap();
        io(top, &format!("write -z {} 1M", 2 * MIB));
        Self {
            base: dir(0).join("g.base.img"),
            layers: made,
        }
    }

    fn top(&self) -> &Path {
        self.layers.last().unwrap()
    }
}

/// Makes `image`, an overlay over a link of the chain's base, from the chain.
fn commit(qemu_img: &Path, chain: &Chain, image: &Path) -> Result<(), String> {
    let base = image.with_extension("base.img");
    fs::hard_link(&chain.base, &base).unwrap();
    let found = read_chain(Path::new(QEMU_IMG), chain.top()).unwrap();
    assert!(ends_in(&found, &chain.base).unwrap());
    commit_chain(qemu_img, &found, &fs::canonicalize(&base).unwrap(), image)
}

fn compare(a: &Path, b: &Path) -> bool {
    Command::new(QEMU_IMG)
        .args(["compare", "-q", "-f", "qcow2", "-F", "qcow2"])
        .arg(a)
        .arg(b)
        .status()
        .unwrap()
        .success()
}

/// The sha256, mtime and size of each file.
fn fingerprints(files: &[PathBuf]) -> Vec<(Vec<u8>, i64, i64, u64)> {
    files
        .iter()
        .map(|f| {
            let meta = fs::metadata(f).unwrap();
            let sum = Sha256::digest(fs::read(f).unwrap()).to_vec();
            (sum, meta.mtime(), meta.mtime_nsec(), meta.size())
        })
        .collect()
}

#[test]
fn the_overlay_reads_as_the_chains_top_over_its_own_link_of_the_base() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 17);
    let image = root.path().join("fork.qcow2");
    commit(Path::new(QEMU_IMG), &chain, &image).unwrap();
    assert!(compare(&image, chain.top()));
    let info = image_info(&image);
    let base = fs::canonicalize(image.with_extension("base.img")).unwrap();
    assert_eq!(info["backing-filename"], base.to_str().unwrap());
    let data = fs::metadata(&image).unwrap().blocks() * 512;
    assert!(data < 32 * MIB, "only what the chain allocated: {data}");
    assert!(
        !image.with_extension("top.qcow2").exists(),
        "the throwaway top is gone"
    );
}

#[test]
fn zeroed_clusters_are_data_so_they_cannot_read_through_to_the_base() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 3);
    let image = root.path().join("fork.qcow2");
    commit(Path::new(QEMU_IMG), &chain, &image).unwrap();
    assert_eq!(
        image_info(&image)["format-specific"]["data"]["compat"],
        "0.10"
    );
    let map = Command::new(QEMU_IMG)
        .args(["map", "--output=json"])
        .arg(&image)
        .output()
        .unwrap();
    let extents: Vec<serde_json::Value> = serde_json::from_slice(&map.stdout).unwrap();
    let at = 2 * MIB + 64 * 1024;
    let covering = extents
        .iter()
        .find(|e| {
            let start = e["start"].as_u64().unwrap();
            start <= at && at < start + e["length"].as_u64().unwrap()
        })
        .unwrap();
    assert_eq!(covering["depth"], 0, "{covering}");
    assert_eq!(covering["data"], true, "{covering}");
    io(&image, &format!("read -P 0 {at} 64k"));
}

#[test]
fn layers_stay_the_same_and_may_be_read_only() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 4);
    for layer in &chain.layers {
        fs::set_permissions(layer, fs::Permissions::from_mode(0o444)).unwrap();
    }
    let before = fingerprints(&chain.layers);
    let image = root.path().join("fork.qcow2");
    commit(Path::new(QEMU_IMG), &chain, &image).unwrap();
    assert_eq!(fingerprints(&chain.layers), before);
    assert!(compare(&image, chain.top()));
}

#[test]
fn forks_of_one_chain_can_run_at_once() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 8);
    let images: Vec<PathBuf> = (0..4)
        .map(|i| root.path().join(format!("fork{i}.qcow2")))
        .collect();
    std::thread::scope(|s| {
        let running: Vec<_> = images
            .iter()
            .map(|image| s.spawn(|| commit(Path::new(QEMU_IMG), &chain, image)))
            .collect();
        for run in running {
            run.join().unwrap().unwrap();
        }
    });
    for image in &images {
        assert!(compare(image, chain.top()));
    }
}

#[test]
fn a_chain_grown_past_its_base_keeps_its_size() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 2);
    let grown = 96 * MIB;
    let status = Command::new(QEMU_IMG)
        .args(["resize", "-q"])
        .arg(chain.top())
        .arg(grown.to_string())
        .status()
        .unwrap();
    assert!(status.success());
    io(chain.top(), &format!("write -P 9 {} 1M", 80 * MIB));
    let image = root.path().join("fork.qcow2");
    commit(Path::new(QEMU_IMG), &chain, &image).unwrap();
    assert_eq!(image_info(&image)["virtual-size"], grown);
    assert!(compare(&image, chain.top()));
}

#[test]
fn a_failed_step_says_why_and_leaves_no_file() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 2);
    let tool = root.path().join("qemu-img-fails");
    crate::test_exec::script(
        &tool,
        "case \"$1\" in commit) echo 'no space left' >&2; exit 1 ;; esac\nexec qemu-img \"$@\"",
    );
    let image = root.path().join("fork.qcow2");
    let err = commit(&tool, &chain, &image).unwrap_err();
    assert!(err.contains("no space left"), "{err}");
    assert!(!image.exists());
    assert!(!image.with_extension("top.qcow2").exists());
}

#[test]
fn a_chain_over_another_file_does_not_end_in_the_base() {
    if !have_tools() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let chain = Chain::new(root.path(), 2);
    let copy = root.path().join("copy.img");
    fs::copy(&chain.base, &copy).unwrap();
    let found = read_chain(Path::new(QEMU_IMG), chain.top()).unwrap();
    assert!(!ends_in(&found, &copy).unwrap());
    assert!(read_chain(Path::new(QEMU_IMG), &root.path().join("gone")).is_err());
}
