//! A spare root: a raw copy of a checkpoint's root, flattened in the
//! background after a fork, so the next fork takes it by rename instead of
//! copying every byte of the root while the caller waits. Only checkpoints
//! that Boxes fork from carry one, which bounds the disk it costs.

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustix::process::{kill_process_group, Pid, Signal};

use super::disks::HeldDisk;
use crate::host::{self, Launch};
use crate::vm::RealBackend;

/// The rate the background flatten reads and writes at, so the forks and
/// Boxes beside it keep the disk. I/O priority classes do nothing on hosts
/// whose disks run without an I/O scheduler.
const RATE: &str = "200M";
/// How long a delete waits for a killed job to go.
const STOP_WAIT: Duration = Duration::from_secs(5);
/// Flattens the layer, grows the copy to the lease disk size, then publishes
/// it under its final name, so a fork never sees a partial spare.
const JOB: &str = r#"set -e
qemu_img=$1 layer=$2 tmp=$3 spare=$4 size=$5 job=$6
nice -n 19 "$qemu_img" convert -q -r "$RATE" -f qcow2 -O raw -S 4k "$layer" "$tmp"
truncate -s ">$size" "$tmp"
mv -T "$tmp" "$spare"
rm -f "$job""#;

/// The files of a checkpoint's spare root, in its `disks` dir.
pub(super) struct Spare {
    /// The finished spare.
    pub(super) file: PathBuf,
    /// The copy a job is writing.
    tmp: PathBuf,
    /// `pid start boot` of the running job.
    job: PathBuf,
    /// The job's output.
    log: PathBuf,
}

impl Spare {
    /// The spare of `root`, the root disk of checkpoint dir `dir`.
    pub(super) fn of(dir: &Path, root: &HeldDisk) -> Self {
        let layer = root.layer(dir);
        Self {
            file: layer.with_extension("spare.raw"),
            tmp: layer.with_extension("spare.raw.tmp"),
            job: layer.with_extension("spare.job"),
            log: layer.with_extension("spare.log"),
        }
    }

    /// The job's pid when the job recorded is still that process.
    fn running(&self) -> Option<u32> {
        let text = fs::read_to_string(&self.job).ok()?;
        let mut words = text.split_whitespace();
        let pid = words.next()?.parse().ok()?;
        let start = words.next().and_then(|s| s.parse().ok());
        host::owns(pid, "", start, words.next()).then_some(pid)
    }

    /// Kills a running job and everything it started; true when none is left.
    pub(super) fn stop(&self) -> bool {
        let Some(pid) = self.running() else {
            return true;
        };
        kill_group(pid);
        host::wait_until_dead(|| host::pid_alive(pid, ""), STOP_WAIT)
    }
}

/// Kills every process in the group `leader` leads: the job's shell, nice
/// and qemu-img alike.
fn kill_group(leader: u32) {
    if let Some(group) = i32::try_from(leader).ok().and_then(Pid::from_raw) {
        // A group already gone is fine; one that will not die shows in the wait after.
        let _ = kill_process_group(group, Signal::KILL);
    }
}

/// Moves `spare` to `image` when it is exactly `size` bytes long; true when
/// it did. Of forks racing for one spare, one rename wins and the rest copy.
pub(super) fn take(spare: &Path, image: &Path, size: u64) -> Result<bool, String> {
    match fs::metadata(spare) {
        Ok(m) if m.len() == size => {}
        Ok(_) => {
            let _ = fs::remove_file(spare);
            return Ok(false);
        }
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat the spare root: {e}")),
    }
    match fs::rename(spare, image) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("take the spare root: {e}")),
    }
}

/// Whether writing `bytes` leaves a filesystem with `free` of `total` bytes
/// above its floor of `min_gib`; a floor of 0 is off.
pub(super) const fn room_for(free: u64, total: u64, bytes: u64, min_gib: u64) -> bool {
    min_gib == 0 || free.saturating_sub(bytes) >= host::host_floor(total, min_gib)
}

impl RealBackend {
    /// The size a flatten of `layer` gives: its own, grown to the lease disk size.
    pub(super) fn root_size(&self, layer: &Path) -> Result<u64, String> {
        let out = Command::new(&self.config.qemu_img)
            .args(["info", "--output=json", "-U"])
            .arg(layer)
            .output()
            .map_err(|e| format!("read the root layer: {e}"))?;
        let info: serde_json::Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("read the root layer: {e}"))?;
        let own = info
            .get("virtual-size")
            .and_then(serde_json::Value::as_u64)
            .ok_or("read the root layer: no virtual size")?;
        Ok(own.max(self.config.disk_gib * super::disks::GIB))
    }

    /// Starts flattening a spare root for checkpoint dir `dir` unless one is
    /// there or being made, the host would fall below its floor for `bytes`,
    /// or the spare could not be renamed into the state dir.
    pub(super) fn refill_spare(
        &self,
        dir: &Path,
        root: &HeldDisk,
        bytes: u64,
        diag: &mut dyn Write,
    ) {
        let spare = Spare::of(dir, root);
        if spare.running().is_some() {
            return;
        }
        let _ = fs::remove_file(&spare.tmp);
        if spare.file.exists() {
            return;
        }
        if let Err(why) = self.spare_allowed(dir, bytes) {
            let _ = writeln!(diag, "no spare root: {why}");
            return;
        }
        if let Err(e) = self.start_spare(dir, root, &spare) {
            let _ = writeln!(diag, "no spare root: {e}");
        }
    }

    /// Why no spare of `bytes` may be made for `dir`, if none may.
    fn spare_allowed(&self, dir: &Path, bytes: u64) -> Result<(), String> {
        let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).map_err(|e| e.to_string());
        if dev(dir)? != dev(&self.config.state_dir)? {
            return Err("checkpoints and leases are on different filesystems".to_owned());
        }
        let (free, total) = host::space(dir)?;
        let floor = self.config.cache.host_floor_gib;
        if room_for(free, total, bytes, floor) {
            Ok(())
        } else {
            Err(format!(
                "{bytes} bytes would take the host below its free-space floor"
            ))
        }
    }

    /// Launches the job and records it, so a delete can stop it.
    fn start_spare(&self, dir: &Path, root: &HeldDisk, spare: &Spare) -> Result<(), String> {
        let size = self.root_size(&root.layer(dir))?;
        let args: Vec<OsString> = vec![
            "-c".into(),
            JOB.replace("$RATE", RATE).into(),
            "spare-root".into(),
            self.config.qemu_img.clone().into(),
            root.layer(dir).into(),
            spare.tmp.clone().into(),
            spare.file.clone().into(),
            size.to_string().into(),
            spare.job.clone().into(),
        ];
        let pid = host::spawn_detached(&Launch::plain(Path::new("/bin/sh"), &args), &spare.log)?;
        let start = host::proc_start(pid).unwrap_or_default();
        let boot = host::boot_id().unwrap_or_default();
        fs::write(&spare.job, format!("{pid} {start} {boot}"))
            .map_err(|e| format!("record the spare job: {e}"))
    }
}

#[cfg(test)]
mod tests;
