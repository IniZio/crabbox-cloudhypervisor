//! Growing the filesystems of cache disks.

use super::*;

/// A cache mount of group `g`.
const MOUNT: &str = "/var/lib/crabbox-cache/g";

/// A 20 GiB cache filesystem with `avail_gib` free on a `disk_gib` disk.
fn cache(avail_gib: u64, disk_gib: u64) -> CacheFs {
    CacheFs {
        mount: MOUNT.to_owned(),
        fs_size: 20 * GIB,
        fs_avail: avail_gib * GIB,
        disk_bytes: disk_gib * GIB,
    }
}

/// A guest whose root is calm and whose one cache is `c`.
fn with_cache(c: CacheFs) -> Fake {
    let fake = Fake::new(calm());
    *fake.caches.borrow_mut() = vec![c];
    fake
}

/// The cache grows the planner asked for.
fn cache_grows(fake: &Fake) -> Vec<String> {
    changes(fake)
        .into_iter()
        .filter(|c| c.starts_with("grow_cache"))
        .collect()
}

#[test]
fn parses_a_cache_sample_line() {
    let got =
        parse_cache_samples(&format!("{MOUNT}  5242880  4096 1073741824 274877906944\n")).unwrap();
    assert_eq!(got, [cache(1, 256)]);
}

#[test]
fn parses_a_cache_sample_captured_from_a_live_noble_guest() {
    let live =
        "/var/lib/crabbox-cache/g               262144                4096 35008512 3221225472\n";
    let got = parse_cache_samples(live).unwrap();
    assert_eq!(got[0].fs_size, GIB);
    assert_eq!((got[0].fs_avail, got[0].disk_bytes), (35_008_512, 3 * GIB));
}

#[test]
fn a_cache_sample_outside_the_cache_root_is_refused() {
    let err = parse_cache_samples("/etc 1 4096 1 1\n").unwrap_err();
    assert!(err.contains("/etc"), "{err}");
    let err = parse_cache_samples("/var/lib/crabbox-cache/a;b 1 4096 1 1\n").unwrap_err();
    assert!(err.contains("a;b"), "{err}");
}

#[test]
fn a_cache_low_on_space_grows_its_filesystem_once_and_logs_it() {
    let fake = with_cache(cache(1, 256));
    let log = ticks(&mut Planner::new(GROWTH), &fake, 3);
    assert_eq!(cache_grows(&fake), [format!("grow_cache {MOUNT} 30")]);
    assert!(
        log.contains(&format!(
            "grew cache filesystem {MOUNT} 20.0 GiB -> 30.0 GiB"
        )),
        "{log}"
    );
}

#[test]
fn a_cache_with_room_is_left_alone() {
    let fake = with_cache(cache(10, 256));
    ticks(&mut Planner::new(GROWTH), &fake, 2);
    assert!(cache_grows(&fake).is_empty());
}

#[test]
fn a_cache_grows_no_further_than_its_disk() {
    let fake = with_cache(cache(1, 25));
    ticks(&mut Planner::new(GROWTH), &fake, 3);
    assert_eq!(cache_grows(&fake), [format!("grow_cache {MOUNT} 25")]);
}

#[test]
fn a_full_cache_that_fills_its_disk_is_logged_once() {
    let fake = with_cache(cache(1, 20));
    let log = ticks(&mut Planner::new(GROWTH), &fake, 4);
    assert!(cache_grows(&fake).is_empty());
    let full = format!("cache filesystem {MOUNT} low on space (1.0 GiB free) and fills its disk");
    assert_eq!(log.matches(&full).count(), 1, "{log}");
}

#[test]
fn a_cache_grow_that_would_cross_the_host_floor_is_refused_and_logged_once() {
    let fake = Fake {
        host_free: 25 * GIB,
        host_total: 100 * GIB,
        ..with_cache(cache(1, 256))
    };
    let log = ticks(&mut Planner::new(GROWTH), &fake, 3);
    assert!(cache_grows(&fake).is_empty());
    let why = format!("host filesystem holding cache {MOUNT} has 25.0 GiB free");
    assert_eq!(log.matches(&why).count(), 1, "{log}");
}

#[test]
fn the_grow_script_resizes_the_mounts_device_to_whole_blocks() {
    let script = grow_cache_script(MOUNT, 30 * GIB + 100);
    assert!(
        script.contains(&format!("findmnt -no SOURCE '{MOUNT}'")),
        "{script}"
    );
    assert!(
        script.contains(&format!("resize2fs \"$src\" {}K", 30 * GIB / 1024)),
        "{script}"
    );
}
