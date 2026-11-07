//! Checkpoints: snapshot a running lease under a caller-chosen id, start new
//! leases from it, and delete it.

use std::io::Write;
use std::time::Instant;

use crate::backend::{Backend, Power};
use crate::lease::{build_lease, check_agreement, require_running, target_id};
use crate::protocol::{CheckpointMade, Lease, Request};
use crate::timing;

#[cfg(test)]
mod tests;

/// The request's checkpoint id; parsing already required it.
fn checkpoint_id(request: &Request) -> Result<&str, String> {
    request
        .checkpoint_id
        .as_deref()
        .ok_or_else(|| "checkpointId must be a non-empty string".to_owned())
}

/// Snapshots a running lease and reports the checkpoint. The lease keeps running.
pub(crate) fn create(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<CheckpointMade, String> {
    let checkpoint = checkpoint_id(request)?;
    let id = target_id(request)?;
    let _lock = backend.lock(&id)?;
    let record = backend
        .find(&id)?
        .ok_or_else(|| format!("lease {id} not found"))?;
    check_agreement(request, &record)?;
    backend.reap(&record, diag);
    require_running(backend.power(&record), &id)?;
    let name = request.name.as_deref().unwrap_or_default();
    let made = backend.checkpoint_create(&record, checkpoint, name, diag)?;
    let parent = made.parent_checkpoint_id.as_deref().unwrap_or("none");
    let _ = writeln!(
        diag,
        "checkpoint {checkpoint} of {id}: {} bytes of its own, parent {parent}",
        made.unique_bytes
    );
    Ok(made)
}

/// Starts the lease in `request.desired` from a checkpoint and waits until it
/// is ready; a lease that already runs is returned as acquire would.
pub(crate) fn fork(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let desired = request
        .desired
        .as_ref()
        .ok_or("checkpointFork requires desired")?;
    let checkpoint = checkpoint_id(request)?;
    let id = target_id(request)?;
    let started = Instant::now();
    let _lock = backend.lock(&id)?;
    if let Some(existing) = backend.find(&id)? {
        check_agreement(request, &existing)?;
        backend.reap(&existing, diag);
        require_running(backend.power(&existing), &id)?;
        backend.wait_ready(&existing, diag)?;
        return Ok(build_lease(&existing, Power::Running.status(), backend));
    }
    let record = backend.checkpoint_fork(desired, checkpoint, diag)?;
    let waiting = Instant::now();
    let _ = writeln!(diag, "fork: created {} from {checkpoint}", record.lease_id);
    if let Err(err) = backend.wait_ready(&record, diag) {
        return Err(match backend.destroy(&record.lease_id, diag) {
            Ok(()) => format!("{err}; vm removed"),
            Err(e2) => format!("{err}; removing vm also failed: {e2}"),
        });
    }
    timing::step(diag, "wait-ready", waiting.elapsed(), "");
    timing::step(diag, "fork-total", started.elapsed(), "");
    Ok(build_lease(&record, Power::Running.status(), backend))
}

/// Removes a checkpoint's files; an unknown id is fine, one that others are
/// deltas over is refused.
pub(crate) fn delete(request: &Request, backend: &dyn Backend) -> Result<(), String> {
    backend.checkpoint_delete(checkpoint_id(request)?)
}
