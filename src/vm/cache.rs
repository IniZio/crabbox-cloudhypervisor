//! Cache disks of the real backend: restore at create, attach at every boot,
//! publish or discard at release. A cache never fails a create or a release.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{wait_until, RealBackend};
use crate::backend::Power;
use crate::ch::ChClient;
use crate::protocol::{CacheVolumeEcho, CacheVolumeRequest};
use crate::state::LeaseRecord;
use crate::volumes::guest::{self, Attached, GuestSetup};
use crate::volumes::store::{Format, Publish, Store, WorkingCopy};
use crate::volumes::{self, Group, Outcome};

/// Record of a lease's cache disks, next to `lease.json`.
const CACHE_FILE: &str = "cache.json";
/// Longest wait for the guest to power off after the ACPI button.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(30);
/// Timeout of each API call while shutting down.
const API_TIMEOUT: Duration = Duration::from_secs(5);

/// One group of a lease: what it got and where its working copy is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordedGroup {
    /// The group as planned.
    group: Group,
    /// What restore did.
    outcome: Outcome,
    /// Working copy.
    #[serde(flatten)]
    copy: WorkingCopy,
}

/// A lease's cache disks; empty for a lease without cache volumes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LeaseCache {
    /// Groups in plan order.
    groups: Vec<RecordedGroup>,
    /// Requested names refused before any restore: (name, reason).
    refused: Vec<(String, String)>,
    /// Tenant scope the groups were restored under; publish goes back there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

impl LeaseCache {
    /// True when the lease has a cache disk attached.
    pub(super) fn has_disks(&self) -> bool {
        self.attached().next().is_some()
    }

    /// Groups that have a disk, with their disk index.
    fn attached(&self) -> impl Iterator<Item = (usize, &RecordedGroup)> {
        self.groups
            .iter()
            .filter(|g| !matches!(g.outcome, Outcome::Skipped { .. }))
            .enumerate()
    }

    /// virtio serial of disk `index`.
    fn serial(index: usize) -> String {
        format!("cbxcv{index}")
    }

    /// The echo for every requested name, groups first.
    pub(super) fn echo(&self) -> Vec<CacheVolumeEcho> {
        let mut out: Vec<CacheVolumeEcho> = self
            .groups
            .iter()
            .flat_map(|g| {
                g.group
                    .volumes
                    .iter()
                    .map(|v| g.outcome.echo(v.name.as_str()))
            })
            .collect();
        out.extend(self.refused.iter().map(|(name, reason)| {
            Outcome::Skipped {
                reason: reason.clone(),
            }
            .echo(name)
        }));
        out
    }

    /// Appends one disk per attached group to a `vm.create` body.
    pub(super) fn attach(&self, config: &mut Value) {
        if let Some(disks) = config.get_mut("disks").and_then(Value::as_array_mut) {
            for (i, g) in self.attached() {
                let mut disk = json!({
                    "id": format!("cv{i}"),
                    "path": g.copy.image,
                    "image_type": g.copy.format.image_type(),
                    "readonly": false,
                    "serial": Self::serial(i),
                });
                if let (Format::Qcow2, Some(fields)) = (g.copy.format, disk.as_object_mut()) {
                    fields.insert("backing_files".to_owned(), Value::Bool(true));
                }
                disks.push(disk);
            }
        }
    }

    /// Units and scripts the seed writes for the attached groups.
    pub(super) fn guest_setup(&self) -> GuestSetup {
        let serials: Vec<(String, &Group)> = self
            .attached()
            .map(|(i, g)| (Self::serial(i), &g.group))
            .collect();
        let attached: Vec<Attached<'_>> = serials
            .iter()
            .map(|(serial, group)| Attached { group, serial })
            .collect();
        guest::setup(&attached)
    }
}

impl RealBackend {
    /// The generation store of tenant `scope` for this config.
    pub(super) fn volume_store(&self, scope: Option<&str>) -> Store {
        Store::new(
            &self.config.cache,
            &self.config.qemu_img,
            &self.config.image.sha256,
            scope,
        )
    }

    /// Restores every requested group for lease `id` and records the result.
    pub(super) fn restore_cache(
        &self,
        id: &str,
        requests: &[CacheVolumeRequest],
        scope: Option<&str>,
    ) -> Result<LeaseCache, String> {
        if requests.is_empty() {
            return Ok(LeaseCache::default());
        }
        let plan = volumes::plan(requests, &self.config.cache);
        let store = self.volume_store(scope);
        let groups = plan
            .groups
            .into_iter()
            .map(|group| {
                let r = store.restore(&group, id);
                RecordedGroup {
                    group,
                    outcome: r.outcome,
                    copy: r.copy,
                }
            })
            .collect();
        let cache = LeaseCache {
            groups,
            refused: plan.skipped,
            scope: scope.map(str::to_owned),
        };
        self.write_cache(id, &cache)?;
        Ok(cache)
    }

    /// Records `cache` as lease `id`'s cache disks.
    fn write_cache(&self, id: &str, cache: &LeaseCache) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(cache).map_err(|e| format!("encode cache: {e}"))?;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.store.lease_dir(id).join(CACHE_FILE))
            .and_then(|mut f| f.write_all(&json))
            .map_err(|e| format!("write cache record: {e}"))
    }

    /// The lease's cache record; empty when it has none or it is unreadable.
    pub(super) fn read_cache(&self, id: &str) -> LeaseCache {
        fs::read(self.store.lease_dir(id).join(CACHE_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Collects store garbage before a lease with cache volumes restores, and
    /// after a release. Failures reach `diag` and never fail the caller.
    pub(super) fn collect_cache_garbage(&self, diag: &mut dyn Write) {
        let _ = self.gc_cache(false, diag);
    }

    /// Presses the ACPI power button and waits for the guest to power off.
    /// True only when it did, so its filesystems were unmounted.
    fn quiesce(record: &LeaseRecord, diag: &mut dyn Write) -> bool {
        let power = Self::power_of(record);
        if power != Power::Running {
            let _ = writeln!(diag, "cache: vm is {}; nothing published", power.status());
            return false;
        }
        let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
        if let Err(e) = client.power_button() {
            let _ = writeln!(diag, "cache: {e}; nothing published");
            return false;
        }
        let off = wait_until(SHUTDOWN_WAIT, || {
            !Self::ch_alive(record) || client.state().is_ok_and(|s| s == "Shutdown")
        });
        if !off {
            let _ = writeln!(
                diag,
                "cache: guest did not power off in time; nothing published"
            );
        }
        off
    }

    /// Shuts the guest down cleanly when it has cache disks; true when it did.
    pub(super) fn quiesce_for_cache(&self, record: &LeaseRecord, diag: &mut dyn Write) -> bool {
        let cache = self.read_cache(&record.lease_id);
        cache.attached().next().is_some() && Self::quiesce(record, diag)
    }

    /// Publishes each grown group when the guest shut down cleanly, then drops
    /// the working copies. Notes go to `diag`; nothing here fails a release.
    pub(super) fn settle_cache(&self, id: &str, quiesced: bool, diag: &mut dyn Write) {
        let cache = self.read_cache(id);
        let store = self.volume_store(cache.scope.as_deref());
        for (_, g) in cache.attached() {
            let group = g.group.id.as_str();
            let note = if quiesced {
                match store.publish(&g.group, &g.copy, id, SystemTime::now()) {
                    Publish::Published { generation } => {
                        format!("published gen {}", generation / 1_000_000_000)
                    }
                    Publish::Unchanged => "unchanged".to_owned(),
                    Publish::Rejected { reason } => format!("rejected: {reason}"),
                }
            } else {
                "discarded".to_owned()
            };
            let _ = writeln!(diag, "cache group {group}: {note}");
        }
        if let Err(e) = store.discard(id) {
            let _ = writeln!(diag, "cache: {e}");
        }
    }

    /// Store GC for `cleanup`; working copies of leases without a record go too.
    pub(super) fn gc_cache(&self, dry_run: bool, diag: &mut dyn Write) -> String {
        let live = |id: &str| matches!(self.store.read(id), Ok(Some(_)));
        let report = self
            .volume_store(None)
            .gc(SystemTime::now(), &live, dry_run);
        for path in &report.removed {
            let _ = writeln!(diag, "cache: remove {}", path.display());
        }
        for error in &report.errors {
            let _ = writeln!(diag, "cache gc: {error}");
        }
        match (report.removed.len(), dry_run) {
            (0, _) => String::new(),
            (n, true) => format!("would remove {n} cache path(s)"),
            (n, false) => format!("removed {n} cache path(s)"),
        }
    }
}

mod held;
#[cfg(test)]
mod tests;

pub(super) use held::CacheFrom;
