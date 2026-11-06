//! Restores as qcow2 overlays over a generation, and publishes that flatten them.

use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::*;

/// A store whose group `g` has one generation, published at 10 s with 40 MiB in `pkg`.
fn published(root: &Path) -> (Store, Group) {
    let s = store(root);
    let g = group("g", GIB);
    let got = publish_one(&s, &g, "first", "pkg", 10);
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
    (s, g)
}

/// The newest generation of `g`.
fn newest(s: &Store, g: &Group) -> Generation {
    generations(&s.group_dir(&g.id)).pop().unwrap()
}

/// `copy` raw as a generation is, for reading with the image helpers.
fn as_raw(path: &Path) -> WorkingCopy {
    WorkingCopy {
        image: path.to_path_buf(),
        format: Format::Raw,
        baseline: 0,
    }
}

#[test]
fn a_restore_is_an_overlay_over_the_newest_generation_that_starts_near_empty() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (s, g) = published(dir.path());
    let generation = newest(&s, &g);

    let r = s.restore(&g, "second");
    assert_eq!(
        r.outcome,
        Outcome::Restored {
            generation: generation.ns
        }
    );
    assert_eq!(r.copy.format, Format::Qcow2);
    let allocated = ext4::allocated(&r.copy.image).unwrap();
    assert!(allocated < MIB, "overlay allocates {allocated} bytes");
    assert_eq!(r.copy.baseline, allocated);
    assert_backed_by(&r.copy, &s.work_dir("second"), &generation.path);
    assert_eq!(guest_size(&r.copy, "pkg"), Some(40 * MIB));
}

/// `copy` is a 1 GiB qcow2 whose backing file, in `work_dir`, is `generation` itself.
fn assert_backed_by(copy: &WorkingCopy, work_dir: &Path, generation: &Path) {
    let meta = info(&copy.image);
    assert_eq!(meta["format"], "qcow2");
    assert_eq!(meta["virtual-size"], GIB);
    let backing = PathBuf::from(meta["backing-filename"].as_str().unwrap());
    assert!(backing.starts_with(fs::canonicalize(work_dir).unwrap()));
    let ino = |p: &Path| fs::metadata(p).unwrap().ino();
    assert_eq!(ino(&backing), ino(generation), "a link, not a copy");
}

/// `image` is raw with no backing file and nothing may write it.
fn assert_self_contained(image: &Path) {
    let meta = info(image);
    assert_eq!(meta["format"], "raw");
    assert!(meta.get("backing-filename").is_none(), "{meta}");
    let mode = fs::metadata(image).unwrap().permissions().mode();
    assert_eq!(mode & 0o222, 0, "a generation is read-only, mode {mode:o}");
}

#[test]
fn publish_flattens_an_overlay_into_a_self_contained_read_only_generation() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (s, g) = published(dir.path());
    let first = newest(&s, &g);
    let first_bytes = fs::read(&first.path).unwrap();

    let r = s.restore(&g, "second");
    guest_writes(&r.copy, "more", 40 * MIB);
    let got = s.publish(&g, &r.copy, "second", at(20));
    assert_eq!(
        got,
        Publish::Published {
            generation: 20_000_000_000
        }
    );

    let second = newest(&s, &g);
    assert_self_contained(&second.path);
    assert_eq!(guest_size(&as_raw(&second.path), "pkg"), Some(40 * MIB));
    assert_eq!(guest_size(&as_raw(&second.path), "more"), Some(40 * MIB));
    assert!(
        fs::read(&first.path).unwrap() == first_bytes,
        "writing through an overlay never changes its generation"
    );

    let third = s.restore(&g, "third");
    assert_eq!(third.copy.format, Format::Qcow2);
    assert_eq!(guest_size(&third.copy, "more"), Some(40 * MIB));
}

#[test]
fn an_overlay_still_reads_its_generation_after_gc_drops_it_from_the_store() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (s, g) = published(dir.path());
    let old = newest(&s, &g);
    let reader = s.restore(&g, "reader");
    let got = publish_one(&s, &g, "writer", "new", 20);
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");

    let report = s.gc(at(20 + GRACE.as_secs() + 1), &|_: &str| true, false);
    assert!(report.removed.contains(&old.path), "{report:?}");
    assert!(!old.path.exists());
    assert_eq!(guest_size(&reader.copy, "pkg"), Some(40 * MIB));

    guest_writes(&reader.copy, "late", 80 * MIB);
    let got = s.publish(&g, &reader.copy, "reader", at(30));
    assert!(matches!(got, Publish::Published { .. }), "{got:?}");
}

#[test]
fn discarding_one_lease_leaves_another_lease_on_the_same_generation_intact() {
    if !have_tools() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (s, g) = published(dir.path());
    let a = s.restore(&g, "a");
    let b = s.restore(&g, "b");
    assert_eq!(
        (a.copy.format, b.copy.format),
        (Format::Qcow2, Format::Qcow2)
    );
    s.discard("a").unwrap();
    assert!(!s.work_dir("a").exists());
    assert!(newest(&s, &g).path.exists());
    assert_eq!(guest_size(&b.copy, "pkg"), Some(40 * MIB));
}
