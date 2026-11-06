//! Saving caches at a sleep: a bounded guest sync before the pause, then a
//! detached process that publishes once the VM's memory is back with the host.
//!
//! `publish.lock` in the lease dir is the publisher's, not the lease lock, so a
//! wake never waits on it. Its children inherit the lock, so a free lock proves
//! that nothing of the publisher still runs.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rustix::process::{kill_process_group, Pid, Signal};
use serde::{Deserialize, Serialize};

use super::{describe, LeaseCache, RealBackend};
use crate::config::CacheConfig;
use crate::host::{self, Launch};
use crate::state::{self, LeaseRecord};
use crate::vm::spec;
use crate::volumes::guest::group_mount_point;
use crate::volumes::store::Store;

/// Why caches may publish, in the lease dir; a mid-work sleep leaves none.
const RECORD_FILE: &str = "pause-publish.json";
/// Lock the publisher and all its children hold, in the lease dir.
const LOCK_FILE: &str = "publish.lock";
/// Where the publisher's notes go, in the lease dir.
const LOG_FILE: &str = "publish.log";
/// Longest the guest sync may hold up a pause or a close.
const SYNC_LIMIT: Duration = Duration::from_secs(5);
/// Longest a close waits for a publisher to finish before killing it.
const CLOSE_WAIT: Duration = Duration::from_secs(120);
/// Longest a killed publisher may take to die; uninterruptible disk I/O can delay it.
const KILL_WAIT: Duration = Duration::from_secs(30);
/// Delay between looks at the lock.
const POLL: Duration = Duration::from_millis(50);

/// What a publisher needs, and which groups the guest flushed to disk. Only a
/// group whose sync succeeded publishes: an image without it can pass every
/// check and still hold broken files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublishRecord {
    /// Where the store is and what it may use.
    cache: CacheConfig,
    /// `qemu-img` binary.
    qemu_img: PathBuf,
    /// Base image hash, which tags the store.
    image_sha256: String,
    /// Whether each attached group synced, by group id.
    synced: BTreeMap<String, bool>,
}

/// Reads the record in `lease_dir`; `None` when absent or unreadable.
fn read_record(lease_dir: &Path) -> Option<PublishRecord> {
    let bytes = fs::read(lease_dir.join(RECORD_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Opens the lock in `lease_dir`, creating it so a starting publisher and a wake
/// always contend on one file; `None` when the lease dir is gone.
fn open_lock(lease_dir: &Path) -> Result<Option<File>, String> {
    let opened = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(lease_dir.join(LOCK_FILE));
    match opened {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("open publish lock: {}", e.kind())),
    }
}

/// True when `file` now holds its lock.
fn try_hold(file: &File) -> Result<bool, String> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(e)) => Err(format!("take publish lock: {}", e.kind())),
    }
}

/// Takes the lock for this process and its children; `None` when it is taken.
/// Records this process, which `setsid` made a group leader, for whoever must kill it.
fn lock_as_publisher(lease_dir: &Path) -> Result<Option<File>, String> {
    let Some(mut file) = open_lock(lease_dir)? else {
        return Ok(None);
    };
    if !try_hold(&file)? {
        return Ok(None);
    }
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())
        .map_err(|e| format!("share publish lock with children: {e}"))?;
    let pid = std::process::id();
    let start = host::proc_start(pid).unwrap_or(0);
    file.set_len(0)
        .and_then(|()| file.write_all(format!("{pid} {start}").as_bytes()))
        .map_err(|e| format!("write publish lock: {}", e.kind()))?;
    Ok(Some(file))
}

/// Kills the process group named in the lock, unless its pid now names another
/// process. A leader already gone still counts: its group exists while a child lives.
fn kill_holder(lease_dir: &Path) {
    let text = fs::read_to_string(lease_dir.join(LOCK_FILE)).unwrap_or_default();
    let Some((pid, start)) = text.split_once(' ') else {
        return;
    };
    let (Ok(pid), Ok(start)) = (pid.parse::<u32>(), start.parse::<u64>()) else {
        return;
    };
    if host::proc_start(pid).is_some_and(|now| now != start) {
        return;
    }
    let group = i32::try_from(pid).ok().and_then(Pid::from_raw);
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

/// Polls until `file` holds the lock or `wait` passes.
fn hold_within(file: &File, wait: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + wait;
    loop {
        if try_hold(file)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

/// Kills any publisher of `lease_dir`; the held lock proves all of it is dead.
fn kill_publisher(lease_dir: &Path, file: File) -> Result<File, String> {
    let deadline = Instant::now() + KILL_WAIT;
    while !try_hold(&file)? {
        if Instant::now() >= deadline {
            return Err(format!(
                "cache publisher still alive {}s after SIGKILL",
                KILL_WAIT.as_secs()
            ));
        }
        kill_holder(lease_dir);
        std::thread::sleep(POLL);
    }
    Ok(file)
}

/// Runs `publish <leaseId> --state-dir <dir>`: publishes each synced group that
/// grew enough, then collects store garbage. Nothing here touches a working copy.
pub fn run_publisher(id: &str, state_dir: &Path, diag: &mut dyn Write) -> Result<(), String> {
    let leases = state::Store::new(state_dir);
    let lease_dir = leases.lease_dir(id);
    let Some(_lock) = lock_as_publisher(&lease_dir)? else {
        return Ok(());
    };
    let Some(record) = read_record(&lease_dir) else {
        return Ok(());
    };
    let cache = LeaseCache::read(&lease_dir);
    let store = Store::new(
        &record.cache,
        &record.qemu_img,
        &record.image_sha256,
        cache.scope.as_deref(),
    );
    for (_, g) in cache.attached() {
        let group = g.group.id.as_str();
        let note = if record.synced.get(group) == Some(&true) {
            describe(store.publish_asleep(&g.group, &g.copy, id, SystemTime::now()))
        } else {
            "not synced; not published".to_owned()
        };
        let _ = writeln!(diag, "cache group {group}: {note}");
    }
    let live = |lease: &str| matches!(leases.read(lease), Ok(Some(_)));
    for error in store.gc(SystemTime::now(), &live, false).errors {
        let _ = writeln!(diag, "cache gc: {error}");
    }
    Ok(())
}

impl RealBackend {
    /// Flushes each attached group's filesystem from inside a running guest,
    /// each bounded on its own; whether each succeeded, by group id.
    pub(in crate::vm) fn sync_groups(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> BTreeMap<String, bool> {
        let c = &self.config;
        let id = record.lease_id.as_str();
        let probe = spec::guest_ssh(&c.ssh, &self.store, &self.exe, &c.state_dir, id);
        let cache = self.read_cache(id);
        let results: Vec<(String, Result<String, String>)> = std::thread::scope(|scope| {
            let mut running = Vec::new();
            for (_, g) in cache.attached() {
                let probe = &probe;
                let remote = format!(
                    "timeout {} sync -f {}",
                    SYNC_LIMIT.as_secs(),
                    spec::shell_quote(&group_mount_point(&g.group))
                );
                let group = g.group.id.as_str().to_owned();
                running.push(scope.spawn(move || (group, probe.output(&remote, SYNC_LIMIT))));
            }
            running.into_iter().filter_map(|t| t.join().ok()).collect()
        });
        let mut synced = BTreeMap::new();
        for (group, result) in results {
            if let Err(e) = &result {
                let _ = writeln!(diag, "cache group {group}: sync failed: {e}");
            }
            synced.insert(group, result.is_ok());
        }
        synced
    }

    /// Records which groups synced, atomically.
    pub(in crate::vm) fn write_publish_record(
        &self,
        id: &str,
        synced: BTreeMap<String, bool>,
    ) -> Result<(), String> {
        let c = &self.config;
        let record = PublishRecord {
            cache: c.cache.clone(),
            qemu_img: c.qemu_img.clone(),
            image_sha256: c.image.sha256.clone(),
            synced,
        };
        let json =
            serde_json::to_vec(&record).map_err(|e| format!("encode publish record: {e}"))?;
        let path = self.store.lease_dir(id).join(RECORD_FILE);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(|e| format!("write publish record: {}", e.kind()))
    }

    /// Whether each group synced, by id; empty without a record, as after a mid-work sleep.
    pub(in crate::vm) fn synced_groups(&self, id: &str) -> BTreeMap<String, bool> {
        read_record(&self.store.lease_dir(id)).map_or_else(BTreeMap::new, |r| r.synced)
    }

    /// Drops the record, so this lease publishes nothing until the next permitted sleep.
    pub(in crate::vm) fn forget_publish_record(&self, id: &str) {
        let _ = fs::remove_file(self.store.lease_dir(id).join(RECORD_FILE));
    }

    /// Records the sync results of a lease that just suspended and starts the
    /// publisher, lowest priority. Failures are only noted: the suspend is done.
    pub(in crate::vm) fn start_publisher(
        &self,
        id: &str,
        synced: BTreeMap<String, bool>,
        diag: &mut dyn Write,
    ) {
        if synced.is_empty() {
            return;
        }
        let any = synced.values().any(|ok| *ok);
        if let Err(e) = self.write_publish_record(id, synced) {
            let _ = writeln!(diag, "warn: nothing published: {e}");
            return;
        }
        if !any {
            let _ = writeln!(diag, "no cache group synced; nothing published");
            return;
        }
        let args: Vec<OsString> = ["-n19", "ionice", "-c3"]
            .into_iter()
            .map(OsString::from)
            .chain([
                self.exe.clone().into_os_string(),
                "publish".into(),
                id.into(),
                "--state-dir".into(),
                self.config.state_dir.clone().into_os_string(),
            ])
            .collect();
        let launch = Launch::plain(Path::new("nice"), &args);
        let log = self.store.lease_dir(id).join(LOG_FILE);
        match host::spawn_detached(&launch, &log) {
            Ok(_) => {
                let _ = writeln!(diag, "cache publisher started");
            }
            Err(e) => {
                let _ = writeln!(diag, "warn: nothing published: {e}");
            }
        }
    }

    /// Kills any publisher before a wake moves the disk, and returns only once
    /// it is dead with its record and flat copies gone. The wake loses that publish.
    pub(in crate::vm) fn cancel_publish(&self, id: &str) -> Result<(), String> {
        let lease_dir = self.store.lease_dir(id);
        let Some(file) = open_lock(&lease_dir)? else {
            return Ok(());
        };
        let _held = kill_publisher(&lease_dir, file)?;
        self.forget_publish_record(id);
        let cache = self.read_cache(id);
        self.volume_store(cache.scope.as_deref())
            .remove_flat_copies(id);
        Ok(())
    }

    /// Lets any publisher finish, for up to [`CLOSE_WAIT`], then kills it. The
    /// returned lock keeps a late starter out while the caller settles the caches.
    pub(in crate::vm) fn finish_publisher(&self, id: &str, diag: &mut dyn Write) -> Option<File> {
        let lease_dir = self.store.lease_dir(id);
        let waited = open_lock(&lease_dir).and_then(|file| {
            let Some(file) = file else {
                return Ok(None);
            };
            if hold_within(&file, CLOSE_WAIT)? {
                return Ok(Some(file));
            }
            kill_publisher(&lease_dir, file).map(Some)
        });
        waited.unwrap_or_else(|e| {
            let _ = writeln!(diag, "warn: {e}");
            None
        })
    }
}

#[cfg(test)]
mod tests;
