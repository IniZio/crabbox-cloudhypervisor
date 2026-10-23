//! Provider configuration parsed from `request.config`.
//!
//! Error text names the offending key and never echoes the provided value.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pinned guest image URL (Ubuntu noble cloud image).
pub const IMAGE_URL: &str =
    "https://cloud-images.ubuntu.com/noble/20260926/noble-server-cloudimg-amd64.img";
/// SHA-256 of the pinned guest image.
pub const IMAGE_SHA256: &str = "6a81c37564db9b1ee84e141922625e1d7c5b389b99bb3c572e0243607d5bb4d2";
/// Pinned UEFI firmware URL.
pub const FIRMWARE_URL: &str =
    "https://github.com/cloud-hypervisor/edk2/releases/download/ch-97eeb7b09/CLOUDHV.fd";
/// SHA-256 of the pinned firmware.
pub const FIRMWARE_SHA256: &str =
    "dc2fc8f0e43b96712d9fccc52a3a590769606412b3e1bc911d217addd3bef624";

/// Name of the per-user subdirectory used under XDG/HOME bases.
const APP_DIR: &str = "crabbox-cloudhypervisor";

/// A downloadable, checksum-pinned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Download URL.
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
}

/// Limits and triggers for growing a running guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Growth {
    /// Highest vCPU count the guest may be raised to.
    pub cpus_max: u32,
    /// Highest memory, in MiB, the guest may be raised to.
    pub memory_mib_max: u64,
    /// Largest root disk, in GiB.
    pub disk_max_gib: u64,
    /// GiB added to the disk per grow.
    pub disk_step_gib: u64,
    /// The disk grows when free space falls below this percent of the root fs...
    pub disk_free_percent: u64,
    /// ...or below this many GiB, whichever is larger.
    pub disk_free_gib: u64,
}

/// Validated provider configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Directory holding per-lease state.
    pub state_dir: PathBuf,
    /// Directory holding unix sockets (kept short).
    pub runtime_dir: PathBuf,
    /// Directory holding downloaded assets.
    pub cache_dir: PathBuf,
    /// Guest vCPU count.
    pub cpus: u32,
    /// Guest memory in MiB.
    pub memory_mib: u64,
    /// Guest disk size in GiB.
    pub disk_gib: u64,
    /// Runtime growth limits.
    pub growth: Growth,
    /// Guest base image.
    pub image: Asset,
    /// UEFI firmware.
    pub firmware: Asset,
    /// `cloud-hypervisor` binary.
    pub cloud_hypervisor: PathBuf,
    /// `passt` binary.
    pub passt: PathBuf,
    /// `qemu-img` binary.
    pub qemu_img: PathBuf,
    /// `ssh` binary.
    pub ssh: PathBuf,
    /// `ssh-keygen` binary.
    pub ssh_keygen: PathBuf,
    /// `curl` binary.
    pub curl: PathBuf,
    /// How long to wait for the guest to become ready.
    pub ready_timeout: Duration,
    /// Cleanup deletes stopped leases older than this many hours; `None` = never.
    pub reap_stopped_after_hours: Option<u64>,
}

/// Environment lookup used for default directories.
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

impl Config {
    /// Parses `request.config` (object or null); defaults come from `env`.
    pub fn from_request(config: &Value, env: Env<'_>) -> Result<Self, String> {
        if !config.is_object() && !config.is_null() {
            return Err("config must be an object".to_owned());
        }
        let state_dir = match path_key(config, "stateDir")? {
            Some(p) => p,
            None => default_state_dir(env)?,
        };
        let runtime_dir =
            path_key(config, "runtimeDir")?.unwrap_or_else(|| default_runtime_dir(env, &state_dir));
        let cache_dir = match path_key(config, "cacheDir")? {
            Some(p) => p,
            None => default_cache_dir(env)?,
        };
        let cpus = u32::try_from(int_key(config, "cpus", 2, 1, 64)?)
            .map_err(|_| "config.cpus must be an integer between 1 and 64".to_owned())?;
        let memory_mib = int_key(config, "memoryMiB", 4096, 512, u64::MAX)?;
        let disk_gib = int_key(config, "diskGiB", 20, 8, u64::MAX)?;
        Ok(Self {
            state_dir,
            runtime_dir,
            cache_dir,
            cpus,
            memory_mib,
            disk_gib,
            growth: growth(config, cpus, memory_mib, disk_gib)?,
            image: asset_key(config, "image", IMAGE_URL, IMAGE_SHA256)?,
            firmware: asset_key(config, "firmware", FIRMWARE_URL, FIRMWARE_SHA256)?,
            cloud_hypervisor: bin_key(config, "cloudHypervisor", "cloud-hypervisor")?,
            passt: bin_key(config, "passt", "passt")?,
            qemu_img: bin_key(config, "qemuImg", "qemu-img")?,
            ssh: bin_key(config, "ssh", "ssh")?,
            ssh_keygen: bin_key(config, "sshKeygen", "ssh-keygen")?,
            curl: bin_key(config, "curl", "curl")?,
            ready_timeout: Duration::from_secs(int_key(
                config,
                "readyTimeoutSeconds",
                300,
                1,
                3600,
            )?),
            reap_stopped_after_hours: reap_stopped_after_hours(config)?,
        })
    }
}

/// Reads the growth keys; each cap defaults to headroom above the boot size.
fn growth(config: &Value, cpus: u32, memory_mib: u64, disk_gib: u64) -> Result<Growth, String> {
    let cpus_max = int_key(
        config,
        "cpusMax",
        (u64::from(cpus) * 2).min(64),
        u64::from(cpus),
        64,
    )?;
    Ok(Growth {
        cpus_max: u32::try_from(cpus_max)
            .map_err(|_| "config.cpusMax must be an integer between cpus and 64".to_owned())?,
        memory_mib_max: int_key(
            config,
            "memoryMiBMax",
            memory_mib.saturating_mul(2),
            memory_mib,
            u64::MAX,
        )?,
        disk_max_gib: int_key(config, "diskMaxGiB", disk_gib.max(100), disk_gib, u64::MAX)?,
        disk_step_gib: int_key(config, "diskGrowStepGiB", 10, 1, u64::MAX)?,
        disk_free_percent: int_key(config, "diskGrowFreePercent", 15, 1, 90)?,
        disk_free_gib: int_key(config, "diskGrowFreeGiB", 4, 0, u64::MAX)?,
    })
}

/// Reads the optional `reapStoppedAfterHours` (integer >= 1; absent = off).
pub fn reap_stopped_after_hours(config: &Value) -> Result<Option<u64>, String> {
    if get(config, "reapStoppedAfterHours").is_none() {
        return Ok(None);
    }
    int_key(config, "reapStoppedAfterHours", 0, 1, u64::MAX).map(Some)
}

/// Returns the value at `key`, treating absent and null alike.
fn get<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    config.get(key).filter(|v| !v.is_null())
}

/// Reads an optional absolute path.
fn path_key(config: &Value, key: &str) -> Result<Option<PathBuf>, String> {
    let Some(value) = get(config, key) else {
        return Ok(None);
    };
    match value.as_str() {
        Some(s) if Path::new(s).is_absolute() => Ok(Some(PathBuf::from(s))),
        _ => Err(format!("config.{key} must be an absolute path")),
    }
}

/// Reads an optional non-empty binary path or name.
fn bin_key(config: &Value, key: &str, default: &str) -> Result<PathBuf, String> {
    let Some(value) = get(config, key) else {
        return Ok(PathBuf::from(default));
    };
    match value.as_str() {
        Some(s) if !s.is_empty() => Ok(PathBuf::from(s)),
        _ => Err(format!("config.{key} must be a non-empty string")),
    }
}

/// Reads an optional integer in `min..=max`.
fn int_key(config: &Value, key: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
    let Some(value) = get(config, key) else {
        return Ok(default);
    };
    match value.as_u64() {
        Some(n) if (min..=max).contains(&n) => Ok(n),
        _ if max == u64::MAX => Err(format!("config.{key} must be an integer >= {min}")),
        _ => Err(format!(
            "config.{key} must be an integer between {min} and {max}"
        )),
    }
}

/// Reads an optional `{url, sha256}` asset.
fn asset_key(config: &Value, key: &str, url: &str, sha256: &str) -> Result<Asset, String> {
    let Some(value) = get(config, key) else {
        return Ok(Asset {
            url: url.to_owned(),
            sha256: sha256.to_owned(),
        });
    };
    if !value.is_object() {
        return Err(format!(
            "config.{key} must be an object with url and sha256"
        ));
    }
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("config.{key}.url must be a non-empty string"))?;
    let sha = value
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|s| is_sha256(s))
        .ok_or_else(|| format!("config.{key}.sha256 must be 64 lowercase hex characters"))?;
    Ok(Asset {
        url: url.to_owned(),
        sha256: sha.to_owned(),
    })
}

/// True for 64 lowercase hex characters.
fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns an env var only when it is an absolute path (XDG rule).
fn abs_env(env: Env<'_>, name: &str) -> Option<PathBuf> {
    env(name).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// `$XDG_*` base if set, else `$HOME/<fallback>`, then the app subdirectory.
fn xdg_dir(env: Env<'_>, xdg: &str, fallback: &str) -> Result<PathBuf, String> {
    if let Some(base) = abs_env(env, xdg) {
        return Ok(base.join(APP_DIR));
    }
    let home = abs_env(env, "HOME")
        .ok_or_else(|| format!("config: set an absolute path key or HOME (${xdg} unset)"))?;
    Ok(home.join(fallback).join(APP_DIR))
}

/// Default state directory.
fn default_state_dir(env: Env<'_>) -> Result<PathBuf, String> {
    xdg_dir(env, "XDG_STATE_HOME", ".local/state")
}

/// Default cache directory.
fn default_cache_dir(env: Env<'_>) -> Result<PathBuf, String> {
    xdg_dir(env, "XDG_CACHE_HOME", ".cache")
}

/// Default runtime directory (short, holds unix sockets).
fn default_runtime_dir(env: Env<'_>, state_dir: &Path) -> PathBuf {
    abs_env(env, "XDG_RUNTIME_DIR").map_or_else(|| state_dir.join("run"), |base| base.join(APP_DIR))
}

#[cfg(test)]
mod tests;
