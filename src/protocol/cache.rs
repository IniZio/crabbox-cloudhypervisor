//! `desired.cacheVolumes`: shape checks only; `volumes::plan` validates the values.

use serde_json::Value;

use super::opt_str;

/// One `desired.cacheVolumes` entry, checked for shape only; `volumes::plan` validates it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheVolumeRequest {
    /// Store image the volume lives in.
    pub group: String,
    /// Directory name inside the group.
    pub name: String,
    /// `<group>-<name>`.
    pub key: String,
    /// Guest paths bound to the volume.
    pub paths: Vec<String>,
    /// Declared size in GiB; 0 means the adapter default.
    pub size_gb: u64,
}

/// Parses `desired.cacheVolumes`; absent or null is empty.
pub(super) fn parse_cache_volumes(
    desired: &serde_json::Map<String, Value>,
) -> Result<Vec<CacheVolumeRequest>, String> {
    const SHAPE: &str = "desired.cacheVolumes must be an array of objects";
    let entries = match desired.get("cacheVolumes") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(SHAPE.to_owned()),
    };
    entries
        .iter()
        .map(|entry| {
            let o = entry.as_object().ok_or(SHAPE)?;
            let text = |key: &str| opt_str(o, key).unwrap_or_default();
            let paths = match o.get("paths") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|p| p.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("desired.cacheVolumes[].paths must be strings")?,
                Some(_) => return Err("desired.cacheVolumes[].paths must be an array".to_owned()),
            };
            let size_gb = match o.get("sizeGB") {
                None | Some(Value::Null) => 0,
                Some(v) => v
                    .as_u64()
                    .ok_or("desired.cacheVolumes[].sizeGB must be an integer >= 0")?,
            };
            Ok(CacheVolumeRequest {
                group: text("group"),
                name: text("name"),
                key: text("key"),
                paths,
                size_gb,
            })
        })
        .collect()
}

/// Parses `desired.cacheScope`; absent or null is none. It names a store
/// directory, so anything but 16 lowercase hex digits is refused.
pub(super) fn parse_cache_scope(
    desired: &serde_json::Map<String, Value>,
) -> Result<Option<String>, String> {
    match desired.get("cacheScope") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if s.len() == 16 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            Ok(Some(s.clone()))
        }
        Some(_) => Err("desired.cacheScope must be 16 lowercase hex digits".to_owned()),
    }
}
