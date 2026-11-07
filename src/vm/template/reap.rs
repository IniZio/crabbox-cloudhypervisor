//! Stops the processes of a template build: those a crashed adapter left
//! behind, and those of an in-flight build when the adapter is told to stop.

#[cfg(test)]
mod tests;

use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use signal_hook::low_level::emulate_default_handler;

use super::super::power::terminate;
use super::super::{CH_COMM, PASST_COMM};
use crate::host;

/// Names a build's processes carry, including the launchers that exec into them.
const COMMS: [&str; 4] = [CH_COMM, PASST_COMM, "setsid", "systemd-run"];
/// How long a process may take to exit after SIGTERM.
const GRACE: Duration = Duration::from_secs(2);

/// Run dir of the template build in flight in this process.
static BUILDING: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Whether the SIGINT/SIGTERM thread is running.
static HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

/// Stops every process of ours that a build launched with a socket inside
/// `run_dir`; errors name the first one still alive.
pub(super) fn reap(run_dir: &Path, diag: &mut dyn Write) -> Result<(), String> {
    let stopped: Vec<Result<(), String>> = launched_in(run_dir)
        .into_iter()
        .map(|(pid, comm)| {
            let start = host::proc_start(pid);
            let alive = || host::pid_alive(pid, "") && host::proc_start(pid) == start;
            let _ = writeln!(
                diag,
                "template: stopping {comm} {pid} left in {}",
                run_dir.display()
            );
            terminate(pid, alive, GRACE, diag)
        })
        .collect();
    stopped.into_iter().collect()
}

/// Pids and comms of this user's build processes with an argument inside `run_dir`.
fn launched_in(run_dir: &Path) -> Vec<(u32, String)> {
    let uid = rustix::process::getuid().as_raw();
    let me = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let ours = entry.metadata().is_ok_and(|m| m.uid() == uid);
            let comm = fs::read_to_string(entry.path().join("comm")).ok()?;
            let comm = comm.trim_end();
            if pid == me || !ours || !COMMS.iter().any(|c| comm.starts_with(c)) {
                return None;
            }
            let cmdline = fs::read(entry.path().join("cmdline")).ok()?;
            cmdline
                .split(|b| *b == 0)
                .any(|arg| Path::new(&*String::from_utf8_lossy(arg)).starts_with(run_dir))
                .then(|| (pid, comm.to_owned()))
        })
        .collect()
}

/// Clears the in-flight build when dropped.
pub(super) struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        *building() = None;
    }
}

/// Marks `run_dir` as the in-flight build, so SIGINT or SIGTERM stops its
/// processes before the adapter dies of the signal.
pub(super) fn in_flight(run_dir: &Path) -> Result<InFlight, String> {
    HANDLER.get_or_init(start_handler).clone()?;
    *building() = Some(run_dir.to_path_buf());
    Ok(InFlight)
}

/// The in-flight build, kept usable after a panic elsewhere.
fn building() -> MutexGuard<'static, Option<PathBuf>> {
    BUILDING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the thread that reaps the in-flight build on SIGINT or SIGTERM and
/// then dies of that signal.
fn start_handler() -> Result<(), String> {
    let mut signals =
        Signals::new([SIGINT, SIGTERM]).map_err(|e| format!("signal handler: {e}"))?;
    std::thread::Builder::new()
        .name("template-signals".to_owned())
        .spawn(move || {
            for signal in signals.forever() {
                let dir = building().clone();
                if let Some(dir) = dir {
                    let mut stderr = std::io::stderr();
                    if let Err(e) = reap(&dir, &mut stderr) {
                        let _ = writeln!(stderr, "warn: {e}");
                    }
                }
                let _ = emulate_default_handler(signal);
            }
        })
        .map(drop)
        .map_err(|e| format!("signal handler: {e}"))
}
