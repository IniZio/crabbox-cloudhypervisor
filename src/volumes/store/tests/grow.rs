//! Working copies whose disk is larger than their filesystem, so the guest can grow it.

use super::*;

/// A store whose cache disks are `disk_gib` GiB.
fn growable(root: &Path, disk_gib: u64) -> Store {
    let mut cfg = limits(40);
    cfg.volume_dir = root.to_path_buf();
    cfg.grow_max_gib = disk_gib;
    Store::new(&cfg, Path::new("qemu-img"), "abcdef0123456789", None)
}

/// Size of the filesystem the guest sees in `copy`, bytes.
fn fs_bytes(copy: &WorkingCopy) -> u64 {
    ext4::check(&view(copy)).unwrap().total * 4096
}

/// Size of the disk the guest sees for `copy`, bytes.
fn disk_bytes(copy: &WorkingCopy) -> u64 {
    info(&copy.image)["virtual-size"].as_u64().unwrap()
}

#[test]
fn a_fresh_copy_is_a_ceiling_sized_disk_holding_a_group_sized_filesystem() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let r = growable(dir.path(), 2).restore(&group("g", GIB), "a");
    assert_eq!(r.outcome, Outcome::Empty);
    assert_eq!(disk_bytes(&r.copy), 2 * GIB);
    assert_eq!(fs_bytes(&r.copy), GIB);
    assert!(r.copy.baseline < 32 * MIB, "sparse: {}", r.copy.baseline);
}

#[test]
fn a_generation_spans_the_whole_disk_so_the_next_overlay_can_grow() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = growable(dir.path(), 2);
    let g = group("g", GIB);
    let got = publish_one(&s, &g, "a", "pkg", 10);
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    let r = s.restore(&g, "b");
    assert_eq!(r.copy.format, Format::Qcow2);
    assert_eq!(disk_bytes(&r.copy), 2 * GIB);
    let base = PathBuf::from(info(&r.copy.image)["backing-filename"].as_str().unwrap());
    assert_eq!(
        fs::metadata(base).unwrap().len(),
        2 * GIB,
        "the chain stays within its base"
    );
    assert_eq!(fs_bytes(&r.copy), GIB);
    assert_eq!(guest_size(&r.copy, "pkg"), Some(40 * MIB));
}

#[test]
fn an_older_smaller_generation_restores_at_its_size_and_publishes_full_size() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let g = group("g", GIB);
    let got = publish_one(&growable(dir.path(), 0), &g, "a", "pkg", 10);
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    let s = growable(dir.path(), 2);
    let r = s.restore(&g, "b");
    assert_eq!(disk_bytes(&r.copy), GIB);
    guest_writes(&r.copy, "more", 40 * MIB);
    let got = s.publish(&g, &r.copy, "b", at(20));
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    assert_eq!(disk_bytes(&s.restore(&g, "c").copy), 2 * GIB);
}

#[test]
fn an_overlay_is_never_smaller_than_its_generation() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let g = group("g", GIB);
    let got = publish_one(&growable(dir.path(), 3), &g, "a", "pkg", 10);
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    let r = growable(dir.path(), 2).restore(&g, "b");
    assert_eq!(r.copy.format, Format::Qcow2);
    assert_eq!(disk_bytes(&r.copy), 3 * GIB);
    assert_eq!(guest_size(&r.copy, "pkg"), Some(40 * MIB));
}
