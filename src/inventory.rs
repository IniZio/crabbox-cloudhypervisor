//! List, release and cleanup over the leases on this host.

use std::collections::BTreeSet;
use std::io::Write;

use crate::backend::Backend;
use crate::lease::{check_agreement, status_of, target_id};
use crate::protocol::{LeaseRow, Request};

#[cfg(test)]
mod tests;

/// Every lease on the host with its status, sorted by lease id.
pub fn list(_request: &Request, backend: &dyn Backend) -> Result<Vec<LeaseRow>, String> {
    let mut rows: Vec<LeaseRow> = backend
        .list()?
        .into_iter()
        .map(|r| LeaseRow {
            status: status_of(backend.alive(&r)).to_owned(),
            lease_id: r.lease_id,
            slug: r.slug,
            name: r.name,
            cloud_id: r.cloud_id,
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

/// Removes dead leases and orphaned directories; live leases stay untouched.
pub fn cleanup(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<String, String> {
    let mut ids: BTreeSet<String> = backend.orphans()?.into_iter().collect();
    for record in backend.list()? {
        if !backend.alive(&record) {
            ids.insert(record.lease_id);
        }
    }
    if ids.is_empty() {
        return Ok("nothing to clean up".to_owned());
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
        return Ok("nothing to clean up".to_owned());
    }
    Ok(summary("removed", removed.into_iter()))
}

/// `"<verb> N lease(s): a, b"`.
fn summary<'a>(verb: &str, ids: impl Iterator<Item = &'a str>) -> String {
    let ids: Vec<&str> = ids.collect();
    format!("{verb} {} lease(s): {}", ids.len(), ids.join(", "))
}
