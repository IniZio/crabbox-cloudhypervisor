//! Small helpers shared by the VM backend.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
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

/// Creates `path` (and parents) private to the user.
pub(super) fn mkdir_private(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create dir: {e}"))
}

/// Removes a directory tree; a missing one is fine.
pub(super) fn remove_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove run dir: {e}")),
    }
}
