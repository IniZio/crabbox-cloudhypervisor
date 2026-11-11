//! Readers for the `cgroup` and `cgroupSlice` keys.

use serde_json::Value;

use crate::scope::{Cgroup, ScopeSlice};

/// Reads `cgroup`: `systemd` (default) or `off`.
pub(super) fn mode(config: &Value) -> Result<Cgroup, String> {
    match super::get(config, "cgroup").map(Value::as_str) {
        None | Some(Some("systemd")) => Ok(Cgroup::Systemd),
        Some(Some("off")) => Ok(Cgroup::Off),
        Some(_) => Err("config.cgroup must be \"systemd\" or \"off\"".to_owned()),
    }
}

/// Reads `cgroupSlice`; it needs `cgroup: systemd`, as `off` starts no scopes.
pub(super) fn slice(config: &Value) -> Result<Option<ScopeSlice>, String> {
    let Some(value) = super::get(config, "cgroupSlice") else {
        return Ok(None);
    };
    if mode(config)? == Cgroup::Off {
        return Err("config.cgroupSlice cannot be set with cgroup: off".to_owned());
    }
    let name = value
        .as_str()
        .ok_or("config.cgroupSlice must be a string")?;
    ScopeSlice::parse(name)
        .map(Some)
        .map_err(|e| format!("config.cgroupSlice: {e}"))
}
