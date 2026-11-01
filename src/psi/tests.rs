use std::fs;
use std::time::{Duration, Instant};

use super::*;

#[test]
fn registering_writes_the_trigger_with_a_trailing_nul() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory");
    fs::write(&path, "").unwrap();
    let trigger = Trigger::parse("some 500000 2000000").unwrap();
    let _psi = Psi::register(&trigger, &path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"some 500000 2000000\0");
}

#[test]
fn registering_fails_with_the_reason_when_the_file_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let trigger = Trigger::parse("some 500000 2000000").unwrap();
    let e = Psi::register(&trigger, &dir.path().join("none"))
        .err()
        .unwrap();
    assert!(e.starts_with("open pressure file"), "{e}");
}

#[test]
fn waiting_without_pressure_returns_false_after_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory");
    fs::write(&path, "").unwrap();
    let trigger = Trigger::parse("some 500000 2000000").unwrap();
    let psi = Psi::register(&trigger, &path).unwrap();
    let start = Instant::now();
    assert_eq!(psi.wait(Duration::from_millis(50)), Ok(false));
    assert!(start.elapsed() >= Duration::from_millis(40));
}
