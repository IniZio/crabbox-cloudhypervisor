//! Per-lease state on disk: records, paths, listing and locking.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

/// Name of the directory holding lock files, next to the lease dirs.
const LOCKS_DIR: &str = ".locks";
/// Record file inside a lease dir.
const RECORD_FILE: &str = "lease.json";
/// Longest accepted lease id.
const MAX_ID_LEN: usize = 48;
/// Longest socket file name created in a run dir (passt's repair socket).
pub const LONGEST_SOCKET_NAME: &str = "p.repair";
/// Length of a run dir name.
pub const RUN_DIR_NAME_LEN: usize = 12;

/// Run dir name for a lease: first 12 lowercase hex chars of sha256 of the id.
#[must_use]
pub fn run_dir_name(lease_id: &str) -> String {
    let mut hex = String::with_capacity(RUN_DIR_NAME_LEN);
    for byte in Sha256::digest(lease_id.as_bytes()) {
        for nibble in [byte >> 4, byte & 0x0f] {
            hex.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    hex.truncate(RUN_DIR_NAME_LEN);
    hex
}

/// Persisted description of one lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRecord {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Provider machine id.
    pub cloud_id: String,
    /// Creation time, unix seconds.
    pub created_at: u64,
    /// Pid of cloud-hypervisor, once started.
    pub ch_pid: Option<u32>,
    /// Pid of passt, once started.
    pub passt_pid: Option<u32>,
    /// Runtime directory holding the sockets.
    pub run_dir: PathBuf,
}

impl LeaseRecord {
    /// cloud-hypervisor API socket.
    #[must_use]
    pub fn api_socket(&self) -> PathBuf {
        self.run_dir.join("a")
    }

    /// Guest vsock socket.
    #[must_use]
    pub fn vsock_socket(&self) -> PathBuf {
        self.run_dir.join("v")
    }

    /// passt socket.
    #[must_use]
    pub fn passt_socket(&self) -> PathBuf {
        self.run_dir.join("p")
    }
}

/// Handle on the lease state tree.
#[derive(Debug, Clone)]
pub struct Store {
    /// `<state_dir>/leases`.
    root: PathBuf,
}

/// Exclusive lock on one lease, released on drop.
#[derive(Debug)]
pub struct LeaseLock {
    /// Locked file; dropping it releases the lock.
    file: Option<File>,
}

impl LeaseLock {
    /// A lock that guards nothing, for test fakes.
    #[must_use]
    pub const fn none() -> Self {
        Self { file: None }
    }

    /// True when a file is held.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        self.file.is_some()
    }
}

/// Checks a lease id is 1..=48 chars of `[A-Za-z0-9_-]`.
pub fn validate_lease_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err("invalid lease id".to_owned())
    }
}

/// Current unix time in seconds; 0 if the clock is before the epoch.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Creates a directory (and parents) with mode 0700.
fn mkdir_private(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create state dir: {e}"))
}

impl Store {
    /// Store rooted at `<state_dir>/leases`.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join("leases"),
        }
    }

    /// Directory holding one lease's files.
    #[must_use]
    pub fn lease_dir(&self, lease_id: &str) -> PathBuf {
        self.root.join(lease_id)
    }

    /// Private SSH key of the lease.
    #[must_use]
    pub fn key_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("id_ed25519")
    }

    /// Private SSH host key of the guest.
    #[must_use]
    pub fn host_key_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("ssh_host_ed25519_key")
    }

    /// Public SSH host key of the guest.
    #[must_use]
    pub fn host_pub_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("ssh_host_ed25519_key.pub")
    }

    /// Pinned `known_hosts` file for the guest.
    #[must_use]
    pub fn known_hosts_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("known_hosts")
    }

    /// Root disk image of the lease.
    #[must_use]
    pub fn disk_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("disk.raw")
    }

    /// Cloud-init seed image of the lease.
    #[must_use]
    pub fn seed_path(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("seed.img")
    }

    /// Guest serial console log.
    #[must_use]
    pub fn serial_log(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("serial.log")
    }

    /// Log of the VMM process.
    #[must_use]
    pub fn process_log(&self, id: &str) -> PathBuf {
        self.lease_dir(id).join("vmm.log")
    }

    /// Reads a record; `Ok(None)` when the dir or file is missing.
    pub fn read(&self, lease_id: &str) -> Result<Option<LeaseRecord>, String> {
        validate_lease_id(lease_id)?;
        let path = self.lease_dir(lease_id).join(RECORD_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("read lease {lease_id}: {e}")),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("parse lease {lease_id}: {e}"))
    }

    /// Writes a record atomically (tmp file then rename), mode 0600.
    pub fn write(&self, record: &LeaseRecord) -> Result<(), String> {
        let id = &record.lease_id;
        validate_lease_id(id)?;
        let dir = self.lease_dir(id);
        mkdir_private(&dir)?;
        let tmp = dir.join("lease.json.tmp");
        let json = serde_json::to_vec_pretty(record).map_err(|e| format!("encode lease: {e}"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("write lease {id}: {e}"))?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("write lease {id}: {e}"))?;
        fs::rename(&tmp, dir.join(RECORD_FILE)).map_err(|e| format!("commit lease {id}: {e}"))
    }

    /// Names of lease dirs (not `.locks`), sorted; empty when root is missing.
    fn lease_names(&self) -> Result<Vec<String>, String> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("list leases: {e}")),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("list leases: {e}"))?;
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if let (true, Ok(name)) = (is_dir, entry.file_name().into_string()) {
                if name != LOCKS_DIR {
                    names.push(name);
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// All readable records sorted by lease id; skips dirs without a record.
    pub fn list(&self) -> Result<Vec<LeaseRecord>, String> {
        let mut records = Vec::new();
        for name in self.lease_names()? {
            if let Ok(Some(record)) = self.read(&name) {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Lease dirs whose record is missing or unparsable, sorted.
    pub fn orphans(&self) -> Result<Vec<String>, String> {
        Ok(self
            .lease_names()?
            .into_iter()
            .filter(|name| !matches!(self.read(name), Ok(Some(_))))
            .collect())
    }

    /// Removes the lease dir; a missing one is fine.
    pub fn remove(&self, lease_id: &str) -> Result<(), String> {
        validate_lease_id(lease_id)?;
        match fs::remove_dir_all(self.lease_dir(lease_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("remove lease {lease_id}: {e}")),
        }
    }

    /// Blocks until it holds the exclusive lock for the lease.
    pub fn lock(&self, lease_id: &str) -> Result<LeaseLock, String> {
        validate_lease_id(lease_id)?;
        let dir = self.root.join(LOCKS_DIR);
        mkdir_private(&dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(format!("{lease_id}.lock")))
            .map_err(|e| format!("open lock {lease_id}: {e}"))?;
        file.lock()
            .map_err(|e| format!("lock lease {lease_id}: {e}"))?;
        Ok(LeaseLock { file: Some(file) })
    }
}
