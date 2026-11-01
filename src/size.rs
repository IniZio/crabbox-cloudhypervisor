//! Lease sizing: maps a crabbox class to guest vCPUs and memory.
//!
//! A class is admitted against the host's totals: a guest is backed lazily, so
//! only the memory it touches is claimed on the host.

use std::collections::BTreeMap;
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest vCPU count cloud-hypervisor is asked for.
const MAX_CPUS: u64 = 64;
/// Smallest guest memory in MiB.
const MIN_MEMORY_MIB: u64 = 512;

/// What a guest boots with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Boot vCPUs.
    pub cpus: u32,
    /// Boot memory in MiB.
    pub memory_mib: u64,
}

/// What this host has in total, which a class must fit within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    /// Logical CPUs.
    pub total_cpus: u32,
    /// `MemTotal`, in MiB.
    pub total_memory_mib: u64,
}

/// The built-in ladder, small to large.
const LADDER: [(&str, u32, u64); 4] = [
    ("standard", 2, 4096),
    ("fast", 4, 8192),
    ("large", 8, 16384),
    ("beast", 16, 32768),
];

/// Class name to size, the built-in ladder before any override.
#[must_use]
pub fn default_classes() -> BTreeMap<String, Size> {
    LADDER
        .iter()
        .map(|&(name, cpus, memory_mib)| (name.to_owned(), Size { cpus, memory_mib }))
        .collect()
}

/// Reads `config.classes` over the built-in ladder; an entry replaces its class whole.
pub fn classes(config: &Value) -> Result<BTreeMap<String, Size>, String> {
    let mut out = default_classes();
    let Some(raw) = config.get("classes").filter(|v| !v.is_null()) else {
        return Ok(out);
    };
    let entries = raw
        .as_object()
        .ok_or("config.classes must be an object of class name to {cpus, memoryMiB}")?;
    for (name, entry) in entries {
        out.insert(name.clone(), entry_size(name, entry)?);
    }
    Ok(out)
}

/// One `classes.<name>` entry.
fn entry_size(name: &str, entry: &Value) -> Result<Size, String> {
    let key = format!("config.classes.{name}");
    let object = entry
        .as_object()
        .ok_or_else(|| format!("{key} must be an object with cpus and memoryMiB"))?;
    let cpus = object
        .get("cpus")
        .and_then(Value::as_u64)
        .filter(|n| (1..=MAX_CPUS).contains(n))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("{key}.cpus must be an integer between 1 and {MAX_CPUS}"))?;
    let memory_mib = object
        .get("memoryMiB")
        .and_then(Value::as_u64)
        .filter(|n| *n >= MIN_MEMORY_MIB)
        .ok_or_else(|| format!("{key}.memoryMiB must be an integer >= {MIN_MEMORY_MIB}"))?;
    Ok(Size { cpus, memory_mib })
}

/// The size for `class`: `default` when absent, else the class checked against `host`.
///
/// A class the host cannot hold fails the lease; shrinking it would hand the
/// caller a smaller machine than it asked for.
pub fn resolve(
    classes: &BTreeMap<String, Size>,
    default: Size,
    class: Option<&str>,
    host: &Host,
) -> Result<Size, String> {
    let Some(name) = class else {
        return Ok(default);
    };
    let Some(&size) = classes.get(name) else {
        let known: Vec<&str> = classes.keys().map(String::as_str).collect();
        return Err(format!(
            "unknown class {name}; known classes: {}",
            known.join(", ")
        ));
    };
    if size.cpus > host.total_cpus {
        return Err(format!(
            "class {name} needs {} vCPUs; this host has {}",
            size.cpus, host.total_cpus
        ));
    }
    if size.memory_mib > host.total_memory_mib {
        return Err(format!(
            "class {name} needs {} MiB of memory; this host has {} MiB",
            size.memory_mib, host.total_memory_mib
        ));
    }
    Ok(size)
}

/// Reads this host's CPU count and total memory.
pub fn host() -> Result<Host, String> {
    let cpus = std::thread::available_parallelism()
        .map_err(|e| format!("read host cpu count: {e}"))?
        .get();
    let meminfo =
        fs::read_to_string("/proc/meminfo").map_err(|e| format!("read /proc/meminfo: {e}"))?;
    Ok(Host {
        total_cpus: u32::try_from(cpus).unwrap_or(u32::MAX),
        total_memory_mib: total_mib(&meminfo)?,
    })
}

/// `MemTotal` from `/proc/meminfo` text, in MiB.
fn total_mib(meminfo: &str) -> Result<u64, String> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.trim().strip_suffix("kB"))
        .and_then(|kib| kib.trim().parse::<u64>().ok())
        .map(|kib| kib / 1024)
        .ok_or_else(|| "read MemTotal from /proc/meminfo".to_owned())
}

#[cfg(test)]
mod tests;
