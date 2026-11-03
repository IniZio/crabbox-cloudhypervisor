//! Cache disks a checkpoint holds: copied in while its lease is paused, and
//! copied out to each fork, whose snapshot already names them.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{LeaseCache, RealBackend};
use crate::protocol::Desired;
use crate::volumes::store::{copy_working, BackingName};
use crate::volumes::{self, Group};

/// The checkpoint's record of its cache disks; absent when it holds none.
const HELD_FILE: &str = "cache.json";
/// Dir of the held images inside a checkpoint dir.
const HELD_DIR: &str = "cache";

/// Where a new lease's cache disks come from.
#[derive(Debug, Clone, Copy)]
pub(in crate::vm) enum CacheFrom<'a> {
    /// The generation store; disks are attached at boot or hot-added after a restore.
    Store,
    /// Copies of what checkpoint dir `.0` holds; its snapshot names the disks already.
    Checkpoint(&'a Path),
}

/// `group ids @ scope` for an error message.
fn describe(groups: &[&Group], scope: Option<&str>) -> String {
    let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    let ids = if ids.is_empty() {
        "no cache disks".to_owned()
    } else {
        format!("cache groups [{}]", ids.join(", "))
    };
    scope.map_or_else(|| ids.clone(), |s| format!("{ids} in scope {s}"))
}

impl LeaseCache {
    /// Points the cache disks a snapshot's `config.json` names at this lease's copies.
    pub(in crate::vm) fn retarget(&self, config: &mut Value) -> Result<(), String> {
        let disks = config
            .get_mut("disks")
            .and_then(Value::as_array_mut)
            .ok_or("snapshot config.json has no disks")?;
        for (i, g) in self.attached() {
            let id = format!("cv{i}");
            let disk = disks
                .iter_mut()
                .find(|d| d.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .ok_or_else(|| format!("snapshot config.json has no cache disk {id}"))?;
            disk["path"] = json!(g.copy.image);
        }
        Ok(())
    }
}

impl RealBackend {
    /// Copies lease `id`'s cache disks into checkpoint dir `dir`; the lease must be paused.
    pub(in crate::vm) fn hold_cache(&self, id: &str, dir: &Path) -> Result<(), String> {
        let mut held = self.read_cache(id);
        if !held.has_disks() {
            return Ok(());
        }
        let into = dir.join(HELD_DIR);
        for g in held
            .groups
            .iter_mut()
            .filter(|g| !matches!(g.outcome, volumes::Outcome::Skipped { .. }))
        {
            let copy = copy_working(
                &self.config.qemu_img,
                &g.group.id,
                &g.copy,
                &into,
                BackingName::Relative,
            )
            .map_err(|e| format!("keep cache group {}: {e}", g.group.id.as_str()))?;
            let name = copy
                .image
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_default();
            g.copy = super::WorkingCopy {
                image: name,
                ..copy
            };
        }
        let json = serde_json::to_vec_pretty(&held).map_err(|e| format!("encode cache: {e}"))?;
        fs::write(dir.join(HELD_FILE), json).map_err(|e| format!("write {HELD_FILE}: {e}"))
    }

    /// The cache disks checkpoint dir `dir` holds; none for a checkpoint without.
    pub(in crate::vm) fn held_cache(dir: &Path) -> Result<LeaseCache, String> {
        match fs::read(dir.join(HELD_FILE)) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("its {HELD_FILE} is bad: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LeaseCache::default()),
            Err(e) => Err(format!("read its {HELD_FILE}: {e}")),
        }
    }

    /// Refuses a fork whose cache plan is not the one the checkpoint was taken
    /// under: its snapshot names those disks and its guest has them mounted.
    pub(in crate::vm) fn check_held(
        &self,
        held: &LeaseCache,
        desired: &Desired,
    ) -> Result<(), String> {
        let plan = volumes::plan(&desired.cache_volumes, &self.config.cache);
        let scope = desired
            .cache_scope
            .as_deref()
            .filter(|_| !desired.cache_volumes.is_empty());
        let held_groups: Vec<&Group> = held.groups.iter().map(|g| &g.group).collect();
        let wants: Vec<&Group> = plan.groups.iter().collect();
        if held_groups == wants && held.scope.as_deref() == scope {
            return Ok(());
        }
        Err(format!(
            "holds {}; this lease asks for {}",
            describe(&held_groups, held.scope.as_deref()),
            describe(&wants, scope)
        ))
    }

    /// Gives lease `id` its own copies of the cache disks checkpoint dir `dir`
    /// holds, and records them as the lease's.
    pub(in crate::vm) fn adopt_cache(
        &self,
        id: &str,
        dir: &Path,
        desired: &Desired,
    ) -> Result<LeaseCache, String> {
        let mut cache = Self::held_cache(dir)?;
        let store = self.volume_store(cache.scope.as_deref());
        let from = dir.join(HELD_DIR);
        let into = store.work_dir(id);
        for g in &mut cache.groups {
            if matches!(g.outcome, volumes::Outcome::Skipped { .. }) {
                continue;
            }
            let held = super::WorkingCopy {
                image: from.join(&g.copy.image),
                ..g.copy.clone()
            };
            g.copy = copy_working(
                &self.config.qemu_img,
                &g.group.id,
                &held,
                &into,
                BackingName::Absolute,
            )
            .map_err(|e| format!("copy cache group {}: {e}", g.group.id.as_str()))?;
        }
        cache.refused = volumes::plan(&desired.cache_volumes, &self.config.cache).skipped;
        self.write_cache(id, &cache)?;
        Ok(cache)
    }
}

#[cfg(test)]
mod tests;
