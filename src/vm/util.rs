//! Small helpers shared by the VM backend.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

/// Serial log lines shown when readiness fails.
const SERIAL_TAIL_LINES: usize = 20;

/// Polls `done` every 100 ms until it holds or `timeout` passes.
pub(super) fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Last lines of the guest serial log, for failure diagnostics.
pub(super) fn serial_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(SERIAL_TAIL_LINES);
    lines.get(skip..).unwrap_or_default().join("\n")
}
