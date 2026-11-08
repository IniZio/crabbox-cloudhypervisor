//! Host-side helpers: disk cloning, key generation, detached processes.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{kill_process, Pid, Signal};

/// Reflink-copies `base` to `dest`, mode 0600 whatever `base`'s mode, then
/// grows `dest` to `size_bytes` (never shrinks).
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
    fs::set_permissions(dest, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("disk clone: cannot make the copy private: {e}"))?;
    grow(dest, size_bytes).map_err(|e| format!("disk clone: {e}"))
}

/// Hard-links `from` to `to`, or copies it when they are on different filesystems.
pub fn link_or_copy(from: &Path, to: &Path) -> Result<(), String> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(_) => clone_disk(from, to, 0),
    }
}

/// Writes `dest`, a sparse raw copy of what qcow2 image `top` reads as through
/// its backing chain, grown to `size_bytes` (never shrunk). It refers to no
/// other file.
pub fn flatten(qemu_img: &Path, top: &Path, dest: &Path, size_bytes: u64) -> Result<(), String> {
    let output = Command::new(qemu_img)
        .args(["convert", "-q", "-f", "qcow2", "-O", "raw", "-S", "4k"])
        .arg(top)
        .arg(dest)
        .output()
        .map_err(|e| format!("disk flatten: cannot run qemu-img: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "disk flatten: qemu-img failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    grow(dest, size_bytes).map_err(|e| format!("disk flatten: {e}"))
}

/// Writes `dest`, a sparse raw copy of `top` as a `format` image reads through its chain.
///
/// It is new storage: qemu-img shares no extents without `-C`, so nothing written
/// to `dest` can reach `top`. It skips the page cache where the filesystem allows
/// that, so a big copy leaves other Boxes' memory alone.
pub fn flatten_private(
    qemu_img: &Path,
    format: &str,
    top: &Path,
    dest: &Path,
) -> Result<(), String> {
    let convert = |direct: &[&str]| {
        self::qemu_img(
            qemu_img,
            "private flatten",
            Command::new(qemu_img)
                .args(["convert", "-q"])
                .args(direct)
                .args(["-f", format, "-O", "raw", "-S", "4k"])
                .arg(top)
                .arg(dest),
        )
    };
    convert(&["-T", "none", "-t", "none"]).or_else(|_| convert(&[]))
}

/// Runs the qemu-img command `cmd`; `Err` carries what it said.
pub fn qemu_img(program: &Path, what: &str, cmd: &mut Command) -> Result<(), String> {
    let output = cmd
        .output()
        .map_err(|e| format!("{what}: cannot run {}: {e}", program.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{what}: qemu-img failed ({}): {}",
        output.status,
        said.trim()
    ))
}

/// Creates the empty qcow2 overlay `dest` over the image at absolute path
/// `backing`, of qemu-img format `format`; it reads as `backing` and stores
/// only what is written to it.
pub fn make_overlay(
    program: &Path,
    what: &str,
    backing: &Path,
    format: &str,
    dest: &Path,
) -> Result<(), String> {
    qemu_img(
        program,
        what,
        Command::new(program)
            .args(["create", "-q", "-f", "qcow2", "-F", format, "-b"])
            .arg(backing)
            .arg(dest),
    )
}

/// Grows `disk` to `size_bytes`; never shrinks it.
pub fn grow(disk: &Path, size_bytes: u64) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .open(disk)
        .map_err(|e| format!("cannot open disk: {e}"))?;
    let current = file
        .metadata()
        .map_err(|e| format!("cannot stat disk: {e}"))?
        .len();
    if current < size_bytes {
        file.set_len(size_bytes)
            .map_err(|e| format!("cannot grow disk: {e}"))?;
    }
    Ok(())
}

/// Bytes available to unprivileged users on the filesystem holding `path`:
/// statvfs `f_bavail * f_frsize`, read through `stat -f`.
pub fn free_bytes(path: &Path) -> Result<u64, String> {
    space(path).map(|(free, _)| free)
}

/// Free and total bytes of the filesystem holding `path`, read now.
pub fn space(path: &Path) -> Result<(u64, u64), String> {
    let output = Command::new("stat")
        .args(["-f", "-c", "%a %b %S", "--"])
        .arg(path)
        .output()
        .map_err(|e| format!("host free space: cannot run stat: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace().map(str::parse::<u64>);
    match (
        output.status.success(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        (true, Some(Ok(avail)), Some(Ok(total)), Some(Ok(size))) => {
            Ok((avail.saturating_mul(size), total.saturating_mul(size)))
        }
        _ => Err(format!(
            "host free space: stat failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Free bytes a filesystem of `total` bytes keeps so snapshots and other Boxes
/// still fit: a tenth of its size, and at least `min_gib`.
#[must_use]
pub const fn host_floor(total: u64, min_gib: u64) -> u64 {
    let percent = total / 100 * 10;
    let least = min_gib.saturating_mul(1024 * 1024 * 1024);
    if percent > least {
        percent
    } else {
        least
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

/// Util-linux tool that turns zero-filled ranges of a file into holes.
pub const DIG_HOLES_TOOL: &str = "fallocate";

/// Deallocates the zero-filled ranges of `file` in place with `fallocate --dig-holes`;
/// length and content are unchanged.
pub fn dig_holes(program: &Path, file: &Path) -> Result<(), String> {
    let output = Command::new(program)
        .arg("--dig-holes")
        .arg(file)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("dig holes: cannot run fallocate: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "dig holes: failed ({}): {}",
        output.status,
        stderr.trim()
    ))
}

/// A program to start detached: its path, arguments and extra environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Program path or name.
    pub program: PathBuf,
    /// Arguments after the program.
    pub args: Vec<OsString>,
    /// Variables added to the inherited environment.
    pub env: Vec<(OsString, OsString)>,
}

impl Launch {
    /// `program args` with the inherited environment.
    #[must_use]
    pub fn plain(program: &Path, args: &[OsString]) -> Self {
        Self {
            program: program.to_path_buf(),
            args: args.to_vec(),
            env: Vec::new(),
        }
    }
}

/// Starts a program in its own session, output appended to `log`; returns its pid.
///
/// Runs `setsid <program> <args>`. As a plain (non group-leader) child, setsid(1)
/// calls `setsid()` and execs in place, so the returned pid is the program's own
/// and it survives our exit and our process-group kill. The `Child` is dropped
/// without waiting: dropping does not kill it, and the brief zombie is reaped
/// when our short-lived op process exits. A `systemd-run --scope` launch also
/// execs in place, so its comm reads `setsid`, then `systemd-run`, before the
/// program's; callers wait for a socket or API before checking comm.
pub fn spawn_detached(launch: &Launch, log: &Path) -> Result<u32, String> {
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
        .arg(&launch.program)
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
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

/// Resident memory of `pid` in bytes, from `VmRSS` in `/proc/<pid>/status`.
#[must_use]
pub fn resident_bytes(pid: u32) -> Option<u64> {
    vm_rss_bytes(&fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
}

/// Bytes in the `VmRSS:   <n> kB` line of a `/proc/<pid>/status` text.
#[must_use]
pub fn vm_rss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
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
