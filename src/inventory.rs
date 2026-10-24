//! List, release and cleanup over the leases on this host.

use std::collections::BTreeSet;
use std::io::Write;

use crate::backend::{Backend, Power};
use crate::config::reap_stopped_after_hours;
use crate::lease::{check_agreement, reap_if_idle, target_id};
use crate::protocol::{LeaseRow, Request};
use crate::state::now_unix;

#[cfg(test)]
mod tests;

/// Every lease on the host with its status, sorted by lease id.
pub fn list(
    _request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<Vec<LeaseRow>, String> {
    let mut rows: Vec<LeaseRow> = backend
        .list()?
        .into_iter()
        .map(|r| {
            reap_if_idle(backend, &r, diag);
            LeaseRow {
                status: backend.power(&r).status().to_owned(),
                growth_blocked: backend.growth_blocked(&r.lease_id),
                lease_id: r.lease_id,
                slug: r.slug,
                name: r.name,
                cloud_id: r.cloud_id,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.lease_id.cmp(&b.lease_id));
    Ok(rows)
}

/// Destroys the named lease after checking it matches the request; missing is `Ok`.
pub fn release(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<(), String> {
    let id = target_id(request)?;
    let _lock = backend.lock(&id)?;
    match backend.find(&id)? {
        Some(record) => {
            check_agreement(request, &record)?;
            backend.destroy(&id, diag)
        }
        None => Ok(()),
    }
}

/// Destroys one cleanup candidate unless it came alive; true when destroyed.
fn remove_one(backend: &dyn Backend, id: &str, diag: &mut dyn Write) -> Result<bool, String> {
    let _lock = backend.lock(id)?;
    // An orphan's record may be unreadable; it then counts as not alive.
    if let Ok(Some(record)) = backend.find(id) {
        if backend.alive(&record) {
            return Ok(false);
        }
    }
    backend.destroy(id, diag)?;
    Ok(true)
}

/// Removes broken lease dirs, then stale cache generations.
///
/// Stopped leases go only past `reapStoppedAfterHours`. Running, paused and
/// (by default) stopped leases stay untouched; only `release` destroys them.
/// Age is `now - createdAt` from lease.json.
pub fn cleanup(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<String, String> {
    let leases = cleanup_leases(request, backend, diag)?;
    let cache = backend.cache_gc(request.dry_run, diag);
    Ok(match (leases.as_str(), cache.is_empty()) {
        (_, true) => leases,
        (NOTHING, false) => cache,
        (_, false) => format!("{leases}; {cache}"),
    })
}

/// Message when cleanup finds no lease to remove.
const NOTHING: &str = "nothing to clean up";

/// The lease half of [`cleanup`].
fn cleanup_leases(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<String, String> {
    let max_age = reap_stopped_after_hours(&request.config)?.map(|h| h.saturating_mul(3600));
    let now = now_unix();
    let mut ids: BTreeSet<String> = backend.orphans()?.into_iter().collect();
    for record in backend.list()? {
        reap_if_idle(backend, &record, diag);
        let old = max_age.is_some_and(|max| now.saturating_sub(record.created_at) > max);
        if old && backend.power(&record) == Power::Stopped {
            ids.insert(record.lease_id);
        }
    }
    if ids.is_empty() {
        return Ok(NOTHING.to_owned());
    }
    if request.dry_run {
        return Ok(summary("would remove", ids.iter().map(String::as_str)));
    }
    let mut removed = Vec::new();
    let mut failures = Vec::new();
    for id in &ids {
        match remove_one(backend, id, diag) {
            Ok(true) => removed.push(id.as_str()),
            Ok(false) => {}
            Err(e) => failures.push(format!("{id}: {e}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!("cleanup failed for {}", failures.join("; ")));
    }
    if removed.is_empty() {
        return Ok(NOTHING.to_owned());
    }
    Ok(summary("removed", removed.into_iter()))
}

/// `"<verb> N lease(s): a, b"`.
fn summary<'a>(verb: &str, ids: impl Iterator<Item = &'a str>) -> String {
    let ids: Vec<&str> = ids.collect();
    format!("{verb} {} lease(s): {}", ids.len(), ids.join(", "))
}
