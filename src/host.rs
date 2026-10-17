//! Host-side helpers: disk cloning, key generation, detached processes.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Sends `signal` (e.g. "TERM") to `pid`; a missing process counts as success.
pub fn kill(pid: u32, signal: &str) -> Result<(), String> {
    let output = Command::new("kill")
        .arg("-s")
        .arg(signal)
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("kill: cannot run kill: {e}"))?;
    if output.status.success() || !Path::new(&format!("/proc/{pid}")).exists() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "kill: failed ({}): {}",
        output.status,
        stderr.trim()
    ))
}

/// Polls every 100ms until the process is gone; true if gone before `timeout`.
///
/// A zombie child of ours counts as gone.
#[must_use]
pub fn wait_gone(pid: u32, comm_prefix: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pid_alive(pid, comm_prefix) {
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
