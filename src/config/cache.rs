//! Settings of the cache volume store.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where cache volume generations live and how much they may use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheConfig {
    /// Store root (`cacheVolumeDir`).
    pub volume_dir: PathBuf,
    /// Whole store budget in GiB; least recently used groups go first above it.
    pub store_max_gib: u64,
    /// Largest group filesystem in GiB.
    pub max_volume_gib: u64,
    /// Groups unused for this many days are removed.
    pub key_ttl_days: u64,
    /// Publish leaves the host at least this free, GiB; 0 turns the floor off.
    pub host_floor_gib: u64,
}
