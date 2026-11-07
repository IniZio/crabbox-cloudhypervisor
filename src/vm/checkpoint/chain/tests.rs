//! Checkpoint chains with real qemu-img and a stand-in cloud-hypervisor API.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::fs;
use std::path::{Path, PathBuf};

use crate::host::flatten;
use crate::protocol::CheckpointMade;
use crate::state::LeaseRecord;
use crate::vm::checkpoint::tests::{
    have_qemu_img, image_info, plant, plant_child, read_image, running_lease, Api, SMALL,
};
use crate::vm::spec::base_raw_path;
use crate::vm::tests::FakeCh;
use crate::vm::RealBackend;
use crate::volumes::tests::noise;

const MIB: usize = 1 << 20;
const CH: &str = "cloud-hypervisor v52.0";

/// Checkpoints the lease under `id` against a fresh stand-in API.
fn take(b: &RealBackend, rec: &LeaseRecord, id: &str) -> Result<CheckpointMade, String> {
    let _ = fs::remove_file(rec.api_socket());
    let api = Api::start(&rec.api_socket(), 5, false);
    let made = b.take_whole(rec, id, "n", &mut Vec::new());
    api.finish();
    made
}

/// Overwrites 64 KiB of the lease's disk at `at` with `byte`; returns the disk.
fn write_disk(b: &RealBackend, at: usize, byte: u8) -> Vec<u8> {
    let path = b.store.disk_path("a");
    let mut disk = fs::read(&path).unwrap();
    disk[at..at + 64 * 1024].fill(byte);
    fs::write(&path, &disk).unwrap();
    disk
}

/// A running lease `a` whose 2 MiB disk is the 1 MiB base image plus zeros.
fn lease_on_base(root: &Path) -> (RealBackend, LeaseRecord, FakeCh) {
    let (b, rec, ch) = running_lease(root);
    let base = noise(MIB as u64);
    fs::create_dir_all(&b.config.cache_dir).unwrap();
    fs::write(base_raw_path(&b.config), &base).unwrap();
    let mut disk = base;
    disk.resize(2 * MIB, 0);
    fs::write(b.store.disk_path("a"), disk).unwrap();
    (b, rec, ch)
}

/// The root disk layer of checkpoint `id`.
fn delta(b: &RealBackend, id: &str) -> PathBuf {
    b.checkpoint_dir(id).join("disks/root.qcow2")
}

/// The base image as a root layer over it names it: its link beside the layer.
const BASE_REF: &str = "root.base.img";

#[test]
fn each_checkpoint_of_a_lease_is_a_delta_over_its_last_and_the_first_over_the_base() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_on_base(root.path());
    write_disk(&b, MIB / 2, 1);
    let first = take(&b, &rec, "ck1").unwrap();
    assert_eq!(first.parent_checkpoint_id, None);
    assert_eq!(image_info(&delta(&b, "ck1"))["backing-filename"], BASE_REF);
    let disk = write_disk(&b, MIB + MIB / 2, 2);
    let second = take(&b, &rec, "ck2").unwrap();
    assert_eq!(second.parent_checkpoint_id.as_deref(), Some("ck1"));
    assert_eq!(
        image_info(&delta(&b, "ck2"))["backing-filename"],
        "../../checkpoint-ck1/disks/root.qcow2"
    );
    assert!(
        read_image(&delta(&b, "ck2")) == disk,
        "ck2 reads as the disk"
    );
    for made in [first, second] {
        assert!(made.unique_bytes < MIB as u64 / 2, "{made:?}");
        assert!(made.unique_bytes > 64 * 1024, "{made:?}");
    }
}

#[test]
fn a_disk_made_from_a_chain_is_raw_and_outlives_every_checkpoint_in_it() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_on_base(root.path());
    write_disk(&b, MIB / 2, 1);
    take(&b, &rec, "ck1").unwrap();
    let disk = write_disk(&b, MIB + MIB / 2, 2);
    take(&b, &rec, "ck2").unwrap();
    let fork = root.path().join("fork.raw");
    flatten(
        Path::new("qemu-img"),
        &delta(&b, "ck2"),
        &fork,
        4 * MIB as u64,
    )
    .unwrap();
    b.remove_checkpoint("ck2").unwrap();
    b.remove_checkpoint("ck1").unwrap();
    fs::remove_file(base_raw_path(&b.config)).unwrap();
    let info = image_info(&fork);
    assert_eq!(info["format"], "raw");
    assert!(info.get("backing-filename").is_none(), "{info}");
    let got = fs::read(&fork).unwrap();
    assert_eq!(got.len(), 4 * MIB);
    assert!(
        got[..2 * MIB] == disk[..],
        "the fork reads as the checkpoint"
    );
}

#[test]
fn a_checkpoint_others_are_deltas_over_cannot_be_deleted_until_they_are() {
    let root = tempfile::tempdir().unwrap();
    let (b, _rec, _ch) = running_lease(root.path());
    let parent = plant(&b, "ck1", SMALL, CH);
    plant_child(&b, "ck3", Some("ck1"), SMALL, CH);
    plant_child(&b, "ck2", Some("ck1"), SMALL, CH);
    assert_eq!(
        b.remove_checkpoint("ck1").unwrap_err(),
        "cannot delete checkpoint ck1: checkpoints ck2, ck3 are deltas over it; delete them first"
    );
    assert!(parent.exists());
    b.remove_checkpoint("ck2").unwrap();
    b.remove_checkpoint("ck3").unwrap();
    b.remove_checkpoint("ck1").unwrap();
    assert!(!parent.exists());
}

#[test]
fn a_checkpoint_others_are_deltas_over_is_not_replaced_and_the_lease_is_not_paused() {
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = running_lease(root.path());
    plant(&b, "ck1", SMALL, CH);
    plant_child(&b, "ck2", Some("ck1"), SMALL, CH);
    let err = b.take_whole(&rec, "ck1", "n", &mut Vec::new()).unwrap_err();
    assert_eq!(
        err,
        "cannot replace checkpoint ck1: checkpoints ck2 are deltas over it; delete them first"
    );
}

#[test]
fn retaking_the_last_checkpoint_keeps_it_over_the_one_before() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_on_base(root.path());
    take(&b, &rec, "ck1").unwrap();
    write_disk(&b, MIB / 2, 1);
    take(&b, &rec, "ck2").unwrap();
    let disk = write_disk(&b, MIB, 3);
    let again = take(&b, &rec, "ck2").unwrap();
    assert_eq!(again.parent_checkpoint_id.as_deref(), Some("ck1"));
    assert!(
        read_image(&delta(&b, "ck2")) == disk,
        "ck2 reads as the disk"
    );
}

#[test]
fn a_lease_whose_last_checkpoint_is_gone_checkpoints_over_the_base() {
    if !have_qemu_img() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (b, rec, _ch) = lease_on_base(root.path());
    take(&b, &rec, "ck1").unwrap();
    b.remove_checkpoint("ck1").unwrap();
    let disk = write_disk(&b, MIB / 2, 1);
    let made = take(&b, &rec, "ck2").unwrap();
    assert_eq!(made.parent_checkpoint_id, None);
    assert_eq!(image_info(&delta(&b, "ck2"))["backing-filename"], BASE_REF);
    assert!(
        read_image(&delta(&b, "ck2")) == disk,
        "ck2 reads as the disk"
    );
}
