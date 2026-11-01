#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use super::*;

/// The owner's defaults for a 10 GiB guest disk.
const GROWTH: Growth = Growth {
    disk_max_gib: 30,
    disk_step_gib: 10,
    disk_free_percent: 15,
    disk_free_gib: 4,
};

/// A calm guest with plenty of space.
fn calm(_tick: u64) -> Sample {
    Sample {
        fs_size: 9 * GIB,
        fs_avail: 7 * GIB,
    }
}

const NOW: Current = Current {
    disk_bytes: 10 * GIB,
};

/// A planner past its first filesystem check.
fn settled() -> Planner {
    let mut p = Planner::new(GROWTH);
    assert_eq!(p.plan(&calm(0), NOW), vec![Action::GrowFs]);
    p.fs_grown(true);
    p
}

#[test]
fn first_sample_grows_the_filesystem_to_fill_the_disk() {
    let mut p = Planner::new(GROWTH);
    assert_eq!(p.plan(&calm(0), NOW), vec![Action::GrowFs]);
    p.fs_grown(true);
    assert!(p.plan(&calm(1), NOW).is_empty());
}

#[test]
fn low_space_grows_the_disk_by_one_step_then_the_filesystem() {
    let mut p = settled();
    let low = Sample {
        fs_size: 9 * GIB,
        fs_avail: 3 * GIB,
    };
    assert_eq!(
        p.plan(&low, NOW),
        vec![
            Action::GrowDisk {
                from: 10 * GIB,
                to: 20 * GIB
            },
            Action::GrowFs
        ]
    );
}

#[test]
fn the_free_space_floor_is_the_larger_of_percent_and_gib() {
    let mut p = settled();
    let big = Sample {
        fs_size: 100 * GIB,
        fs_avail: 14 * GIB,
    };
    assert!(
        matches!(
            p.plan(&big, NOW)[..],
            [Action::GrowDisk { .. }, Action::GrowFs]
        ),
        "14 GiB free of 100 GiB is under 15%"
    );
    let mut p = settled();
    let enough = Sample {
        fs_size: 9 * GIB,
        fs_avail: 4 * GIB,
    };
    assert!(
        p.plan(&enough, NOW).is_empty(),
        "4 GiB free meets the floor"
    );
}

#[test]
fn a_failed_filesystem_grow_is_retried_before_the_disk_grows_again() {
    let mut p = settled();
    let low = Sample {
        fs_size: 9 * GIB,
        fs_avail: GIB,
    };
    assert_eq!(p.plan(&low, NOW).len(), 2);
    p.fs_grown(false);
    let grown = Current {
        disk_bytes: 20 * GIB,
    };
    assert_eq!(p.plan(&low, grown), vec![Action::GrowFs]);
    p.fs_grown(true);
    assert!(matches!(p.plan(&low, grown)[0], Action::GrowDisk { .. }));
}

#[test]
fn the_disk_stops_at_its_cap_and_says_so_once() {
    let mut p = settled();
    let low = Sample {
        fs_size: 9 * GIB,
        fs_avail: GIB,
    };
    let near = Current {
        disk_bytes: 25 * GIB,
    };
    assert_eq!(
        p.plan(&low, near)[0],
        Action::GrowDisk {
            from: 25 * GIB,
            to: 30 * GIB
        }
    );
    p.fs_grown(true);
    let full = Current {
        disk_bytes: 30 * GIB,
    };
    assert_eq!(
        p.plan(&low, full),
        vec![Action::DiskAtMax { max: 30 * GIB }]
    );
    assert!(p.plan(&low, full).is_empty());
}
