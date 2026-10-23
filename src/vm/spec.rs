//! Pure builders: VM config JSON, MAC address, shell quoting, cache paths.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::backend::SSH_USER;
use crate::config::Config;
use crate::ready::ReadyProbe;
use crate::state::{LeaseRecord, Store};

/// Guest vsock context id (any value >= 3 works; one VM per vsock socket).
const GUEST_CID: u32 = 3;
/// Disk id of the root disk, named in `vm.resize-disk`.
pub const ROOT_DISK_ID: &str = "root";
/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;
/// Memory hotplug granularity the guest kernel needs (128 MiB).
pub const MEMORY_BLOCK: u64 = 128 * MIB;
/// Hex characters of an asset hash used in cache file names.
const HASH_PREFIX: usize = 12;

/// Builds the `vm.create` request body for one lease.
#[must_use]
pub fn vm_config(
    config: &Config,
    firmware: &Path,
    disk: &Path,
    seed: &Path,
    serial: &Path,
    record: &LeaseRecord,
) -> Value {
    let boot = config.memory_mib * MIB;
    let headroom = (config.growth.memory_mib_max * MIB).saturating_sub(boot);
    let memory = if headroom > 0 {
        let hotplug = headroom.div_ceil(MEMORY_BLOCK) * MEMORY_BLOCK;
        json!({"size": boot, "shared": true, "hotplug_size": hotplug})
    } else {
        json!({"size": boot, "shared": true})
    };
    json!({
        "cpus": {"boot_vcpus": config.cpus, "max_vcpus": config.growth.cpus_max},
        "memory": memory,
        "balloon": {"size": 0, "deflate_on_oom": true, "free_page_reporting": true},
        "payload": {"firmware": firmware},
        "disks": [
            {"id": ROOT_DISK_ID, "path": disk, "image_type": "Raw", "readonly": false},
            {"path": seed, "image_type": "Raw", "readonly": true},
        ],
        "net": [{
            "vhost_user": true,
            "vhost_socket": record.passt_socket(),
            "mac": mac_for(&record.lease_id),
            "num_queues": 2,
        }],
        "vsock": {"cid": GUEST_CID, "socket": record.vsock_socket()},
        "serial": {"mode": "File", "file": serial},
        "console": {"mode": "Off"},
    })
}

/// Deterministic locally administered MAC (`52:54:00:xx:xx:xx`) for a lease.
#[must_use]
pub fn mac_for(lease_id: &str) -> String {
    let digest = Sha256::digest(lease_id.as_bytes());
    let tail: Vec<String> = digest.iter().take(3).map(|b| format!("{b:02x}")).collect();
    format!("52:54:00:{}", tail.join(":"))
}

/// Quotes `s` for a POSIX shell only when it has characters outside a safe set.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=@%+-".contains(c));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// The ssh `ProxyCommand` that bridges to the lease's vsock socket.
#[must_use]
pub fn proxy_command(exe: &Path, lease_id: &str, state_dir: &Path) -> String {
    format!(
        "{} proxy {} --state-dir {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(lease_id),
        shell_quote(&state_dir.to_string_lossy()),
    )
}

/// How to ssh into a lease's guest through its vsock proxy.
#[must_use]
pub fn guest_ssh(ssh: &Path, store: &Store, exe: &Path, state_dir: &Path, id: &str) -> ReadyProbe {
    ReadyProbe {
        ssh: ssh.to_path_buf(),
        key: store.key_path(id),
        proxy_command: proxy_command(exe, id, state_dir),
        user: SSH_USER.to_owned(),
        host: id.to_owned(),
        known_hosts: store.known_hosts_path(id),
        host_key_alias: format!("crabbox-{id}"),
    }
}

/// `<cache_dir>/<stem>-<hash prefix>.<ext>`.
fn cache_file(config: &Config, stem: &str, sha256: &str, ext: &str) -> PathBuf {
    let prefix: String = sha256.chars().take(HASH_PREFIX).collect();
    config.cache_dir.join(format!("{stem}-{prefix}.{ext}"))
}

/// Cached firmware download.
#[must_use]
pub fn firmware_cache_path(config: &Config) -> PathBuf {
    cache_file(config, "firmware", &config.firmware.sha256, "fd")
}

/// Cached qcow2 image download.
#[must_use]
pub fn image_cache_path(config: &Config) -> PathBuf {
    cache_file(config, "image", &config.image.sha256, "qcow2")
}

/// Raw conversion of the cached image, the base for per-lease disks.
#[must_use]
pub fn base_raw_path(config: &Config) -> PathBuf {
    cache_file(config, "image", &config.image.sha256, "raw")
}
