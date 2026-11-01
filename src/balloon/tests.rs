use super::*;

const MIB: u64 = 1024 * 1024;

fn cfg() -> BalloonConfig {
    BalloonConfig::default()
}

#[test]
fn trigger_accepts_a_window_that_is_a_multiple_of_two_seconds() {
    let t = Trigger::parse("some 500000 2000000").unwrap();
    assert_eq!(t.as_str(), "some 500000 2000000");
    assert!(Trigger::parse("full 100000 4000000").is_ok());
}

#[test]
fn trigger_rejects_a_window_under_or_off_the_two_second_grid() {
    for bad in [
        "some 150000 1000000",
        "some 150000 3000000",
        "some 150000 0",
        "some 150000 2500000",
    ] {
        let e = Trigger::parse(bad).unwrap_err();
        assert!(e.contains("multiple of 2 s"), "{bad}: {e}");
    }
}

#[test]
fn trigger_rejects_malformed_text() {
    for bad in [
        "",
        "some",
        "some 1 2 3",
        "any 500000 2000000",
        "some x 2000000",
        "some 0 2000000",
        "some 3000000 2000000",
    ] {
        assert!(Trigger::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn an_event_raises_the_target_one_step_of_class_memory() {
    let mut b = Inflator::new(&cfg(), 4096);
    assert_eq!(b.event(0), Some(1024 * MIB));
    assert_eq!(b.event(2), Some(2048 * MIB));
}

#[test]
fn the_target_stops_at_class_memory_minus_the_floor() {
    let mut b = Inflator::new(&cfg(), 4096);
    assert_eq!(b.event(0), Some(1024 * MIB));
    assert_eq!(b.event(2), Some(2048 * MIB));
    assert_eq!(b.event(4), Some(3072 * MIB));
    assert_eq!(b.event(6), None);
    assert_eq!(b.target(), 3072 * MIB);
}

#[test]
fn a_floor_at_or_above_class_memory_never_inflates() {
    let mut b = Inflator::new(&cfg(), 1024);
    assert_eq!(b.event(0), None);
    assert_eq!(b.target(), 0);
}

#[test]
fn quiet_before_the_cooldown_changes_nothing() {
    let mut b = Inflator::new(&cfg(), 4096);
    b.event(10);
    assert_eq!(b.quiet(69), None);
    assert_eq!(b.target(), 1024 * MIB);
}

#[test]
fn quiet_after_the_cooldown_steps_down_once_per_cooldown() {
    let mut b = Inflator::new(&cfg(), 4096);
    b.event(0);
    b.event(2);
    assert_eq!(b.quiet(61), None);
    assert_eq!(b.quiet(62), Some(1024 * MIB));
    assert_eq!(b.quiet(100), None);
    assert_eq!(b.quiet(122), Some(0));
    assert_eq!(b.quiet(500), None);
}

#[test]
fn an_event_restarts_the_cooldown() {
    let mut b = Inflator::new(&cfg(), 4096);
    b.event(0);
    b.event(50);
    assert_eq!(b.quiet(109), None);
    assert_eq!(b.quiet(110), Some(1024 * MIB));
}

#[test]
fn revert_puts_back_a_target_the_vmm_refused() {
    let mut b = Inflator::new(&cfg(), 4096);
    let before = b.target();
    b.event(0);
    b.revert(before);
    assert_eq!(b.target(), 0);
}
