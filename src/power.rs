//! Pause and resume: freeze a running VM in RAM, or bring a paused or dead VM back.

use std::io::Write;

use crate::backend::{Backend, Power};
use crate::lease::{build_lease, check_agreement, target_id};
use crate::protocol::{Lease, Request};
use crate::state::LeaseRecord;

#[cfg(test)]
mod tests;

/// Loads the named record, checks agreement and reaps a dead VM's leftovers.
fn load(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<(String, LeaseRecord), String> {
    let id = target_id(request)?;
    let record = backend
        .find(&id)?
        .ok_or_else(|| format!("lease {id} not found"))?;
    check_agreement(request, &record)?;
    backend.reap(&record, diag);
    Ok((id, record))
}

/// Pauses a running VM; paused and stopped leases are reported, not changed.
pub(crate) fn pause(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<String, String> {
    let id = target_id(request)?;
    let _lock = backend.lock(&id)?;
    let (id, record) = load(request, backend, diag)?;
    match backend.power(&record) {
        Power::Running => {
            backend.pause(&record, diag)?;
            Ok(format!("paused {id}"))
        }
        Power::Paused => Ok(format!("{id} already paused")),
        Power::Stopped => Ok(format!("{id} already stopped")),
    }
}

/// Brings a paused or stopped VM back to ready; a running one is returned as is.
pub(crate) fn resume(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let id = target_id(request)?;
    let _lock = backend.lock(&id)?;
    let (id, record) = load(request, backend, diag)?;
    if backend.power(&record) == Power::Running {
        return Ok(build_lease(&record, Power::Running.status(), backend));
    }
    backend.resume(&record, diag)?;
    let record = backend
        .find(&id)?
        .ok_or_else(|| format!("lease {id} not found"))?;
    Ok(build_lease(&record, Power::Running.status(), backend))
}
