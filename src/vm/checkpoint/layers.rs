//! Writing a checkpoint's layers after its lease resumes. A create returns
//! once the VM runs again and a detached job turns the frozen disk copies into
//! layers, then renames `checkpoint-<id>.tmp` to `checkpoint-<id>`, so the
//! final dir exists only when every layer is whole.
//!
//! `layers.lock` in the `.tmp` dir is held from the create's first step and
//! handed to the job as its stdin, which its own children inherit, so it is
//! never free while the checkpoint is being made. A free lock beside no final
//! dir means the job died: such a checkpoint is never ready.

use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{kill_process_group, Pid, Signal};
use serde::{Deserialize, Serialize};

use super::{disks, read_meta, DIR_PREFIX};
use crate::host;
use crate::timing;
use crate::vm::{remove_tree, RealBackend};

/// Held for as long as the checkpoint is being made.
const LOCK_FILE: &str = "layers.lock";
/// What the job needs that the checkpoint's meta does not say.
const RECORD_FILE: &str = "layers.json";
/// The job's output; it moves into the final dir with the rest.
const LOG_FILE: &str = "layers.log";
/// Longest a fork waits for a checkpoint, or a job for its parent.
pub(super) const READY_WAIT: Duration = Duration::from_secs(600);
/// Longest a delete waits for a killed job to let go of the lock.
const KILL_WAIT: Duration = Duration::from_secs(30);
/// How often a waiter looks at the lock again.
const POLL: Duration = Duration::from_millis(100);

/// The job's settings.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayersRecord {
    /// qemu-img to write the layers with.
    qemu_img: PathBuf,
}

/// Where checkpoint `id` of cache dir `cache` is: final, and while it is made.
fn dirs(cache: &Path, id: &str) -> (PathBuf, PathBuf) {
    let done = cache.join(format!("{DIR_PREFIX}{id}"));
    let tmp = cache.join(format!("{DIR_PREFIX}{id}.tmp"));
    (done, tmp)
}

/// The layers lock in `tmp`, made if missing; none when `tmp` is gone.
fn open_lock(tmp: &Path) -> Result<Option<File>, String> {
    let opened = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(tmp.join(LOCK_FILE));
    match opened {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("open the layers lock: {}", e.kind())),
    }
}

/// Whether the lock was free and is now held, shared or not.
fn try_hold(file: &File, shared: bool) -> Result<bool, String> {
    let held = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match held {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(format!("take the layers lock: {}", e.kind())),
    }
}

/// Takes the lock of the checkpoint being made in `tmp`; the caller keeps it
/// until it is handed to the job.
pub(super) fn begin(tmp: &Path) -> Result<File, String> {
    let file = open_lock(tmp)?.ok_or("the checkpoint dir is gone")?;
    if try_hold(&file, false)? {
        Ok(file)
    } else {
        Err("another create holds this checkpoint".to_owned())
    }
}

/// Whether a checkpoint is being made in `tmp`, by a create or its job.
pub(super) fn in_progress(tmp: &Path) -> bool {
    match open_lock(tmp) {
        Ok(Some(file)) => !try_hold(&file, true).unwrap_or(false),
        _ => false,
    }
}

/// Waits up to `wait` for checkpoint `id` to be whole. Fine when it is, or
/// when nothing of it exists, which a fork then reports as not found.
pub(super) fn wait_ready(cache: &Path, id: &str, wait: Duration) -> Result<(), String> {
    let (done, tmp) = dirs(cache, id);
    let deadline = Instant::now() + wait;
    loop {
        if done.is_dir() {
            return Ok(());
        }
        let Some(file) = open_lock(&tmp)? else {
            if done.is_dir() || !tmp.is_dir() {
                return Ok(());
            }
            return Err(format!("checkpoint {id} was never finished; delete it"));
        };
        if try_hold(&file, true)? {
            drop(file);
            if done.is_dir() {
                return Ok(());
            }
            return Err(format!("checkpoint {id} was never finished; delete it"));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "checkpoint {id} is still being written after {}s",
                wait.as_secs()
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Kills whatever holds the lock in `tmp`, and returns once it is free.
pub(super) fn stop(tmp: &Path) -> Result<(), String> {
    let Some(file) = open_lock(tmp)? else {
        return Ok(());
    };
    let deadline = Instant::now() + KILL_WAIT;
    while !try_hold(&file, false)? {
        if Instant::now() >= deadline {
            return Err(format!(
                "its layer job is still alive {}s after SIGKILL",
                KILL_WAIT.as_secs()
            ));
        }
        kill_holder(tmp);
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// Kills the process group named in the lock file, if it is still that process.
fn kill_holder(tmp: &Path) {
    let text = fs::read_to_string(tmp.join(LOCK_FILE)).unwrap_or_default();
    let Some((pid, start)) = text.split_once(' ') else {
        return;
    };
    let (Ok(pid), Ok(start)) = (pid.parse::<u32>(), start.trim().parse::<u64>()) else {
        return;
    };
    if host::proc_start(pid) != Some(start) {
        return;
    }
    if let Some(group) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

impl RealBackend {
    /// Hands the checkpoint in `tmp`, and its held `lock`, to a detached job.
    pub(super) fn start_layers(&self, tmp: &Path, id: &str, mut lock: File) -> Result<(), String> {
        let record = LayersRecord {
            qemu_img: self.config.qemu_img.clone(),
        };
        let json = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        fs::write(tmp.join(RECORD_FILE), json).map_err(|e| format!("write {RECORD_FILE}: {e}"))?;
        let args: Vec<OsString> = vec![
            "nice".into(),
            "-n19".into(),
            self.exe.clone().into_os_string(),
            "layers".into(),
            id.into(),
            "--cache-dir".into(),
            self.config.cache_dir.clone().into_os_string(),
        ];
        let held = lock
            .try_clone()
            .map_err(|e| format!("hand the layers lock to the job: {}", e.kind()))?;
        let pid = spawn_holding(&args, &tmp.join(LOG_FILE), held)?;
        let start = host::proc_start(pid).unwrap_or_default();
        lock.set_len(0)
            .and_then(|()| lock.write_all(format!("{pid} {start}").as_bytes()))
            .map_err(|e| format!("record the layer job: {}", e.kind()))
    }
}

/// Starts `args` in its own session with `lock` as its stdin, the only fd it
/// gets beyond its log, so no other process can come to hold the lock.
fn spawn_holding(args: &[OsString], log: &Path, lock: File) -> Result<u32, String> {
    let out = File::options()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .map_err(|e| format!("open the layers log: {}", e.kind()))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("open the layers log: {}", e.kind()))?;
    let child = Command::new("setsid")
        .args(args)
        .stdin(Stdio::from(lock))
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| format!("start the layer job: {e}"))?;
    Ok(child.id())
}

/// The job: waits for the parent's layers, writes this checkpoint's, then
/// makes it whole. It runs holding the lock it inherited.
pub fn finish(cache: &Path, id: &str, diag: &mut dyn Write) -> Result<(), String> {
    super::check_id(id)?;
    let started = Instant::now();
    let (done, tmp) = dirs(cache, id);
    let meta = read_meta(&tmp).ok_or("its checkpoint.json is unreadable")?;
    let record: LayersRecord = fs::read(tmp.join(RECORD_FILE))
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .map_err(|e| format!("read {RECORD_FILE}: {e}"))?;
    if let Some(parent) = &meta.parent {
        let waiting = Instant::now();
        wait_ready(cache, parent, READY_WAIT).map_err(|e| format!("its parent: {e}"))?;
        timing::step(diag, "parent-wait", waiting.elapsed(), "");
    }
    disks::write_layers(
        &record.qemu_img,
        cache,
        &tmp,
        &meta.disks,
        meta.parent.as_deref(),
        diag,
    )?;
    remove_tree(&done)?;
    fs::rename(&tmp, &done).map_err(|e| format!("publish checkpoint: {e}"))?;
    timing::step(diag, "layers-total", started.elapsed(), "");
    Ok(())
}

#[cfg(test)]
mod tests;
