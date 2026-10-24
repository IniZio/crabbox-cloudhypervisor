//! Host-side helpers: disk cloning, key generation, detached processes.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{kill_process, Pid, Signal};

/// Reflink-copies `base` to `dest`, then grows `dest` to `size_bytes` (never shrinks).
pub fn clone_disk(base: &Path, dest: &Path, size_bytes: u64) -> Result<(), String> {
    let output = Command::new("cp")
        .arg("--reflink=auto")
        .arg("--sparse=always")
        .arg(base)
        .arg(dest)
        .output()
        .map_err(|e| format!("disk clone: cannot run cp: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "disk clone: cp failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let file = OpenOptions::new()
        .write(true)
        .open(dest)
        .map_err(|e| format!("disk clone: cannot open clone: {e}"))?;
    let current = file
        .metadata()
        .map_err(|e| format!("disk clone: cannot stat clone: {e}"))?
        .len();
    if current < size_bytes {
        file.set_len(size_bytes)
            .map_err(|e| format!("disk clone: cannot grow clone: {e}"))?;
    }
    Ok(())
}

/// Bytes available to unprivileged users on the filesystem holding `path`:
/// statvfs `f_bavail * f_frsize`, read through `stat -f`.
pub fn free_bytes(path: &Path) -> Result<u64, String> {
    let output = Command::new("stat")
        .args(["-f", "-c", "%a %S", "--"])
        .arg(path)
        .output()
        .map_err(|e| format!("host free space: cannot run stat: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace().map(str::parse::<u64>);
    match (output.status.success(), fields.next(), fields.next()) {
        (true, Some(Ok(blocks)), Some(Ok(size))) => Ok(blocks.saturating_mul(size)),
        _ => Err(format!(
            "host free space: stat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Generates an ed25519 keypair at `key` and returns the trimmed public key line.
pub fn ssh_keygen(program: &Path, key: &Path, comment: &str) -> Result<String, String> {
    let mut pubkey = key.as_os_str().to_owned();
    pubkey.push(".pub");
    for stale in [key.as_os_str(), pubkey.as_os_str()] {
        match fs::remove_file(stale) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("ssh keygen: cannot remove stale key: {e}")),
        }
    }
    let output = Command::new(program)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(key)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh keygen: cannot run ssh-keygen: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ssh keygen: failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    fs::set_permissions(key, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("ssh keygen: cannot set key mode: {e}"))?;
    let text = fs::read_to_string(&pubkey)
        .map_err(|e| format!("ssh keygen: cannot read public key: {e}"))?;
    Ok(text.trim().to_owned())
}

/// Starts `program` in its own session, output appended to `log`; returns its pid.
///
/// Runs `setsid <program> <args>`. As a plain (non group-leader) child, setsid(1)
/// calls `setsid()` and execs in place, so the returned pid is the program's own
/// and it survives our exit and our process-group kill. The `Child` is dropped
/// without waiting: dropping does not kill it, and the brief zombie is reaped
/// when our short-lived op process exits.
pub fn spawn_detached(program: &Path, args: &[OsString], log: &Path) -> Result<u32, String> {
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .map_err(|e| format!("spawn: cannot open log file: {e}"))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("spawn: cannot duplicate log handle: {e}"))?;
    let child = Command::new("setsid")
        .arg(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("spawn: cannot run setsid: {e}"))?;
    Ok(child.id())
}

/// True iff `pid` exists, is not a zombie/dead, and its comm starts with `comm_prefix`.
#[must_use]
pub fn pid_alive(pid: u32, comm_prefix: &str) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    let state = rest.split_whitespace().next();
    if matches!(state, None | Some("Z" | "X")) {
        return false;
    }
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim_end().starts_with(comm_prefix))
}

/// Start time of `pid` in clock ticks since boot (field 22 of `/proc/<pid>/stat`).
#[must_use]
pub fn proc_start(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Identifier of the current boot; a pid means nothing across boots.
#[must_use]
pub fn boot_id() -> Option<String> {
    let id = fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(id.trim().to_owned())
}

/// True iff `pid` is the process recorded as `start` in boot `boot`.
///
/// A record without a start time or boot id never matches, so its pid is never
/// signalled.
#[must_use]
pub fn owns(pid: u32, comm_prefix: &str, start: Option<u64>, boot: Option<&str>) -> bool {
    let (Some(start), Some(boot)) = (start, boot) else {
        return false;
    };
    pid_alive(pid, comm_prefix)
        && proc_start(pid) == Some(start)
        && boot_id().as_deref() == Some(boot)
}

/// Sends `signal` to the one process `pid`; a missing process counts as success.
pub fn kill(pid: u32, signal: Signal) -> Result<(), String> {
    let target = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| format!("kill: {pid} is not a process id"))?;
    match kill_process(target, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        Err(e) => Err(format!("kill {pid}: {e}")),
    }
}

/// Polls every 100ms until the process is gone; true if gone before `timeout`.
///
/// A zombie child of ours counts as gone.
#[must_use]
pub fn wait_gone(pid: u32, comm_prefix: &str, timeout: Duration) -> bool {
    wait_until_dead(|| pid_alive(pid, comm_prefix), timeout)
}

/// Polls every 100ms until `alive` turns false; true if that happens before `timeout`.
#[must_use]
pub fn wait_until_dead(alive: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !alive() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests;
