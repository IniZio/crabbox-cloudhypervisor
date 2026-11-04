//! Cache disks in checkpoints: what a checkpoint's lease had, matched to a
//! fork's cache plan, and the fork's copies put in its record and snapshot.

use serde_json::{json, Value};

use super::{LeaseCache, RealBackend};
use crate::protocol::Desired;
use crate::volumes::store::WorkingCopy;
use crate::volumes::{self, Group, Token};

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
    /// Tenant scope the groups were restored under.
    pub(in crate::vm) fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Each attached group's id and working copy, in disk order.
    pub(in crate::vm) fn copies(&self) -> impl Iterator<Item = (&Token, &WorkingCopy)> {
        self.attached().map(|(_, g)| (&g.group.id, &g.copy))
    }

    /// This record with `refused` and each attached group's copy made by `make`.
    pub(in crate::vm) fn with_copies(
        &self,
        refused: Vec<(String, String)>,
        mut make: impl FnMut(&Token) -> Result<WorkingCopy, String>,
    ) -> Result<Self, String> {
        let mut cache = self.clone();
        for g in &mut cache.groups {
            if !matches!(g.outcome, volumes::Outcome::Skipped { .. }) {
                g.copy = make(&g.group.id)?;
            }
        }
        cache.refused = refused;
        Ok(cache)
    }

    /// Points each cache disk a snapshot's `config.json` names at this lease's
    /// copy; returns the others, to hot-add once the VM runs.
    pub(in crate::vm) fn retarget(&self, config: &mut Value) -> Result<Vec<Value>, String> {
        let mut ours = json!({"disks": []});
        self.attach(&mut ours);
        let disks = config
            .get_mut("disks")
            .and_then(Value::as_array_mut)
            .ok_or("snapshot config.json has no disks")?;
        let mut rest = Vec::new();
        for disk in ours
            .get("disks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match disks.iter_mut().find(|d| d.get("id") == disk.get("id")) {
                Some(named) => named["path"] = disk["path"].clone(),
                None => rest.push(disk.clone()),
            }
        }
        Ok(rest)
    }
}

impl RealBackend {
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
}

#[cfg(test)]
mod tests;
