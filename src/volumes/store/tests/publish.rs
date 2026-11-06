//! One publish path: the working copy is only ever read, and only checks and
//! guards decide what reaches the store.

use std::os::unix::fs::PermissionsExt;

use super::*;

const CAP: u64 = 256 * MIB;
const REPLAYED: u8 = b'J';

fn raw_and_overlay(s: &Store, g: &Group) -> [Restored; 2] {
    let got = publish_one(s, g, "seed", "pkg", 10);
    assert_published(&got);
    s.discard("seed").unwrap();
    let overlay = s.restore(g, "overlay");
    assert_eq!(overlay.copy.format, Format::Qcow2);
    let empty = group("other", CAP);
    let raw = s.restore(&empty, "raw");
    assert_eq!(raw.copy.format, Format::Raw);
    [raw, overlay]
}

fn assert_published(got: &Publish) {
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
}

fn owner(r: &Restored) -> Group {
    match r.copy.format {
        Format::Raw => group("other", CAP),
        Format::Qcow2 => group("g", CAP),
    }
}

const fn lease(r: &Restored) -> &'static str {
    match r.copy.format {
        Format::Raw => "raw",
        Format::Qcow2 => "overlay",
    }
}

/// Leaves a real unreplayed transaction in the journal of `image`: `/victim`
/// reads as noise until the journal is replayed, and as [`REPLAYED`] after.
fn dirty_journal(image: &Path) {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old");
    let new = dir.path().join("new");
    fs::write(&old, noise(4096)).unwrap();
    fs::write(&new, [REPLAYED; 4096]).unwrap();
    debugfs_raw(image, &format!("write {} victim", old.display()));
    let block = debugfs_out(image, "bmap victim 0");
    let script = dir.path().join("cmds");
    let cmds = format!("jo\njw -b {} {}\njc\n", block.trim(), new.display());
    fs::write(&script, cmds).unwrap();
    let out = Command::new("debugfs")
        .args(["-w", "-f"])
        .arg(&script)
        .arg(image)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(ext4::needs_recovery(image).unwrap());
}

fn debugfs_out(image: &Path, request: &str) -> String {
    let out = Command::new("debugfs")
        .args(["-R", request])
        .arg(image)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn newest_gen(s: &Store, g: &Group) -> Generation {
    generations(&s.group_dir(&g.id)).pop().unwrap()
}

fn disk_bytes(s: &Store, r: &Restored) -> Vec<Vec<u8>> {
    let (_, base) = s.work_paths(lease(r), &owner(r).id, r.copy.format);
    let mut files = vec![fs::read(&r.copy.image).unwrap()];
    files.extend(base.map(|b| fs::read(b).unwrap()));
    files
}

fn no_flat_left(s: &Store, lease: &str) {
    let left: Vec<_> = fs::read_dir(s.work_dir(lease))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".flat.tmp"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_publish_leaves_raw_and_overlay_working_copies_byte_identical() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    for r in raw_and_overlay(&s, &group("g", CAP)) {
        guest_writes(&r.copy, "more", 40 * MIB);
        let before = disk_bytes(&s, &r);
        let mode = fs::metadata(&r.copy.image).unwrap().permissions();
        let got = s.publish(&owner(&r), &r.copy, lease(&r), at(20));
        assert_published(&got);
        assert!(disk_bytes(&s, &r) == before, "{:?}", r.copy.format);
        assert_eq!(fs::metadata(&r.copy.image).unwrap().permissions(), mode);
        let generation = newest_gen(&s, &owner(&r));
        assert_eq!(guest_size(&as_gen(&generation), "more"), Some(40 * MIB));
        no_flat_left(&s, lease(&r));
    }
}

fn as_gen(generation: &Generation) -> WorkingCopy {
    WorkingCopy {
        image: generation.path.clone(),
        format: Format::Raw,
        baseline: 0,
    }
}

#[test]
fn a_lease_keeps_using_its_raw_disk_after_a_publish_and_the_generation_does_not_follow() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", CAP);
    let got = publish_one(&s, &g, "a", "pkg", 10);
    assert_published(&got);
    let generation = newest_gen(&s, &g);
    let frozen = fs::read(&generation.path).unwrap();
    let copy = s.work_paths("a", &g.id, Format::Raw).0;
    write_raw(&copy, "later", 40 * MIB);
    assert!(
        fs::read(&generation.path).unwrap() == frozen,
        "no shared storage"
    );
    assert_eq!(guest_size(&as_gen(&generation), "later"), None);
}

#[test]
fn a_dirty_journal_replays_on_the_private_copy_and_publishes() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    for r in raw_and_overlay(&s, &group("g", CAP)) {
        guest_writes(&r.copy, "more", 40 * MIB);
        in_guest(&r.copy, dirty_journal);
        let before = disk_bytes(&s, &r);
        let got = s.publish(&owner(&r), &r.copy, lease(&r), at(20));
        assert_published(&got);
        let generation = newest_gen(&s, &owner(&r));
        assert!(!ext4::needs_recovery(&generation.path).unwrap());
        let replayed = debugfs_out(&generation.path, "cat victim");
        assert!(replayed.bytes().all(|b| b == REPLAYED), "not replayed");
        assert!(disk_bytes(&s, &r) == before, "replay stays on the copy");
        assert!(ext4::needs_recovery(&view(&r.copy)).unwrap());
    }
}

#[test]
fn damage_e2fsck_repairs_unattended_is_repaired_on_the_copy_and_publishes() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", CAP);
    let r = written_in(&s, &g, "a", 40 * MIB);
    debugfs_w(&r.copy, "set_bg 0 free_blocks_count 1");
    let got = s.publish(&g, &r.copy, "a", at(1));
    assert_published(&got);
    ext4::check(&newest_gen(&s, &g).path).unwrap();
}

#[test]
fn a_failed_check_leaves_the_store_and_the_working_copy_untouched() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", CAP);
    let [raw, overlay] = raw_and_overlay(&s, &g);
    let held = gens(&s, "g");
    for r in [raw, overlay] {
        guest_writes(&r.copy, "more", 40 * MIB);
        debugfs_w(&r.copy, "set_inode_field npm mode 0100644");
        let before = disk_bytes(&s, &r);
        let got = s.publish(&owner(&r), &r.copy, lease(&r), at(20));
        assert!(
            matches!(&got, Publish::Rejected { reason } if reason.starts_with("fsck")),
            "{got:?}"
        );
        assert!(disk_bytes(&s, &r) == before, "working copy unchanged");
        no_flat_left(&s, lease(&r));
    }
    assert_eq!(gens(&s, "g"), held);
    assert!(gens(&s, "other").is_empty());
}

#[test]
fn a_damaged_overlay_is_rejected_and_a_raw_copy_is_never_asked_for_a_qemu_check() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", CAP);
    let [raw, overlay] = raw_and_overlay(&s, &g);
    guest_writes(&overlay.copy, "more", 40 * MIB);
    let mut bytes = fs::read(&overlay.copy.image).unwrap();
    let refcounts = 2 * 64 * 1024;
    bytes[refcounts..refcounts + 4096].fill(0);
    fs::write(&overlay.copy.image, bytes).unwrap();
    let held = gens(&s, "g");
    let got = s.publish(&g, &overlay.copy, "overlay", at(20));
    assert!(
        matches!(&got, Publish::Rejected { reason } if reason.contains("overlay check")),
        "{got:?}"
    );
    assert_eq!(gens(&s, "g"), held);

    guest_writes(&raw.copy, "more", 40 * MIB);
    let got = s.publish(&owner(&raw), &raw.copy, "raw", at(21));
    assert_published(&got);
}

#[test]
fn growth_is_measured_from_the_last_publish_with_a_bigger_bar_at_sleep() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", GIB);
    let r = s.restore(&g, "a");
    guest_writes(&r.copy, "small", 40 * MIB);
    assert_eq!(
        s.publish_asleep(&g, &r.copy, "a", at(1)),
        Publish::Unchanged
    );
    assert!(gens(&s, "g").is_empty());

    guest_writes(&r.copy, "big", 260 * MIB);
    let got = s.publish_asleep(&g, &r.copy, "a", at(2));
    assert_published(&got);
    assert_eq!(s.publish(&g, &r.copy, "a", at(3)), Publish::Unchanged);

    guest_writes(&r.copy, "more", 40 * MIB);
    assert_eq!(
        s.publish_asleep(&g, &r.copy, "a", at(4)),
        Publish::Unchanged
    );
    let got = s.publish(&g, &r.copy, "a", at(5));
    assert_published(&got);
    assert_eq!(gens(&s, "g").len(), 2);
}

#[test]
fn a_smaller_copy_skips_a_newer_generation_from_another_lease() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", GIB);
    let small = written_in(&s, &g, "small", 40 * MIB);
    let large = written_in(&s, &g, "large", 80 * MIB);
    let got = s.publish(&g, &large.copy, "large", at(1));
    assert_published(&got);

    let got = s.publish(&g, &small.copy, "small", at(2));
    assert!(matches!(got, Publish::Skipped { .. }), "{got:?}");
    assert_eq!(gens(&s, "g").len(), 1);
    no_flat_left(&s, "small");

    guest_writes(&small.copy, "more", 80 * MIB);
    let got = s.publish(&g, &small.copy, "small", at(3));
    assert_published(&got);

    guest_writes(&small.copy, "own", 40 * MIB);
    let got = s.publish(&g, &small.copy, "small", at(4));
    assert_published(&got);
}

fn written_in(s: &Store, g: &Group, lease: &str, bytes: u64) -> Restored {
    let r = s.restore(g, lease);
    guest_writes(&r.copy, lease, bytes);
    r
}

fn guest_rewrites_pkg(copy: &WorkingCopy) {
    in_guest(copy, |image| {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        let mut bytes = noise(40 * MIB);
        bytes.reverse();
        fs::write(&blob, bytes).unwrap();
        debugfs_raw(image, "rm npm/pkg");
        debugfs_raw(image, &format!("write {} npm/pkg", blob.display()));
    });
}

#[test]
fn the_regression_guard_also_compares_the_flat_size_an_overlay_turned_out_to_be() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", GIB);
    let got = publish_one(&s, &g, "seed", "pkg", 1);
    assert_published(&got);
    let ours = s.restore(&g, "ours");
    guest_rewrites_pkg(&ours.copy);
    let theirs = written_in(&s, &g, "theirs", 34 * MIB);
    assert_published(&s.publish(&g, &theirs.copy, "theirs", at(2)));
    let (_, base) = s.work_paths("ours", &g.id, Format::Qcow2);
    let most =
        ext4::allocated(&ours.copy.image).unwrap() + ext4::allocated(&base.unwrap()).unwrap();
    assert!(
        most > newest_gen(&s, &g).bytes,
        "the bound alone passes this"
    );

    let got = s.publish(&g, &ours.copy, "ours", at(3));
    assert!(matches!(got, Publish::Skipped { .. }), "{got:?}");
    no_flat_left(&s, "ours");
}

#[test]
fn a_publish_that_would_cross_the_host_floor_skips() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = limits(40);
    cfg.volume_dir = dir.path().to_path_buf();
    cfg.host_floor_gib = 1 << 40;
    let s = Store::new(&cfg, Path::new("qemu-img"), "abcdef0123456789", None);
    let g = group("g", GIB);
    let r = written_in(&s, &g, "a", 40 * MIB);
    let got = s.publish(&g, &r.copy, "a", at(1));
    assert!(
        matches!(&got, Publish::Skipped { reason } if reason.contains("floor")),
        "{got:?}"
    );
    assert!(gens(&s, "g").is_empty());
    no_flat_left(&s, "a");
}

#[test]
fn a_generation_with_an_old_seal_marker_restores_as_an_overlay() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let g = group("g", CAP);
    let first = s.restore(&g, "old");
    write_raw(&first.copy.image, "pkg", 40 * MIB);
    let marker = "write /dev/null npm/.crabbox-sealed";
    debugfs_raw(&first.copy.image, marker);
    let legacy = s.group_dir(&g.id).join("gen-0000000000000000009-old.img");
    fs::rename(&first.copy.image, &legacy).unwrap();
    let before = fs::read(&legacy).unwrap();

    let r = s.restore(&g, "new");
    assert_eq!(r.outcome, Outcome::Restored { generation: 9 });
    assert_eq!(r.copy.format, Format::Qcow2);
    assert_eq!(guest_size(&r.copy, "pkg"), Some(40 * MIB));
    assert!(fs::read(&legacy).unwrap() == before, "generation unchanged");
    guest_writes(&r.copy, "more", 40 * MIB);
    let got = s.publish(&g, &r.copy, "new", at(20));
    assert_published(&got);
}

fn logging_qemu_img(dir: &Path, refuse: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("args");
    let stub = dir.join("qemu-img");
    let body = format!(
        "echo \"$*\" >> {}\ncase \"$*\" in *'{refuse}'*) exit 1;; esac\nexec qemu-img \"$@\"",
        log.display()
    );
    crate::test_exec::script(&stub, &body);
    (stub, log)
}

fn store_with(root: &Path, program: &Path) -> Store {
    let mut cfg = limits(40);
    cfg.volume_dir = root.to_path_buf();
    Store::new(&cfg, program, "abcdef0123456789", None)
}

#[test]
fn a_flatten_bypasses_the_page_cache_and_never_offloads_the_copy() {
    if !have_tools() {
        return;
    }
    let tools = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (stub, log) = logging_qemu_img(tools.path(), "no-such-argument");
    let s = store_with(dir.path(), &stub);
    let got = publish_one(&s, &group("g", CAP), "a", "pkg", 1);
    assert_published(&got);
    let args = fs::read_to_string(&log).unwrap();
    let convert = args.lines().find(|l| l.starts_with("convert")).unwrap();
    assert!(convert.contains("-T none -t none"), "{convert}");
    assert!(!convert.contains("-C"), "{convert}");
    assert!(convert.contains("-f raw"), "{convert}");
}

#[test]
fn a_copy_plainly_smaller_than_a_newer_generation_skips_without_copying_anything() {
    if !have_tools() {
        return;
    }
    let tools = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (stub, log) = logging_qemu_img(tools.path(), "no-such-argument");
    let s = store_with(dir.path(), &stub);
    let g = group("g", CAP);
    let small = written_in(&s, &g, "small", 40 * MIB);
    let large = written_in(&s, &g, "large", 80 * MIB);
    assert_published(&s.publish(&g, &large.copy, "large", at(1)));
    fs::remove_file(&log).unwrap();
    let got = s.publish(&g, &small.copy, "small", at(2));
    assert!(matches!(got, Publish::Skipped { .. }), "{got:?}");
    assert!(!log.exists(), "qemu-img never ran");
}

#[test]
fn a_filesystem_without_direct_io_still_publishes() {
    if !have_tools() {
        return;
    }
    let tools = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (stub, _) = logging_qemu_img(tools.path(), "-t none");
    let s = store_with(dir.path(), &stub);
    let got = publish_one(&s, &group("g", CAP), "a", "pkg", 1);
    assert_published(&got);
}

#[test]
fn a_stale_read_only_flat_copy_does_not_stop_a_publish_and_cleanup_spares_every_disk() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let tricky = group("x.flat", CAP);
    let r = written_in(&s, &tricky, "a", 40 * MIB);
    let stale = r.copy.image.with_extension("flat.tmp");
    fs::write(&stale, b"left by a killed publisher").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o444)).unwrap();
    s.remove_flat_copies("a");
    assert!(!stale.exists());
    assert!(r.copy.image.exists(), "a working copy is never a flat copy");

    fs::write(&stale, b"left again").unwrap();
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o444)).unwrap();
    let got = s.publish(&tricky, &r.copy, "a", at(1));
    assert_published(&got);
    no_flat_left(&s, "a");
}
