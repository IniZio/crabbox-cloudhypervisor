//! Unit tests for `Power` and the dead-VM error strings.

use super::{paused_error, stopped_error, Power};

#[test]
fn power_status_strings() {
    assert_eq!(Power::Running.status(), "ready");
    assert_eq!(Power::Paused.status(), "paused");
    assert_eq!(Power::Stopped.status(), "stopped");
}

#[test]
fn error_strings_are_exact() {
    assert_eq!(
        stopped_error("L1"),
        "lease L1 is stopped; run crabbox resume"
    );
    assert_eq!(paused_error("L1"), "lease L1 is paused; run crabbox resume");
}
