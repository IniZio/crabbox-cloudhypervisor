//! Acquire and resolve: find or create the VM for a lease and describe it.

use std::io::Write;

use crate::backend::{self, Backend, Power};
use crate::protocol::{Lease, Request, Ssh};
use crate::state::{validate_lease_id, LeaseRecord};

#[cfg(test)]
mod tests;

/// The non-empty value, else `None`.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Cloud id with the `ch-` prefix removed.
fn strip_cloud(cloud_id: &str) -> &str {
    cloud_id.strip_prefix("ch-").unwrap_or(cloud_id)
}

/// Lease id the request names: first non-empty of desired, lease, expected ids,
/// then cloud ids, then the bare `id`. Validated.
pub fn target_id(request: &Request) -> Result<String, String> {
    let desired = request.desired.as_ref();
    let lease = request.lease.as_ref();
    let expected = request.expected.as_ref();
    let direct = [
        desired.map(|d| d.lease_id.as_str()),
        lease.and_then(|l| l.lease_id.as_deref()),
        expected.and_then(|e| e.lease_id.as_deref()),
        expected.and_then(|e| e.attempt_lease_id.as_deref()),
    ];
    let cloud = [
        lease.and_then(|l| l.cloud_id.as_deref()),
        expected.and_then(|e| e.cloud_id.as_deref()),
        request.id.as_deref(),
    ]
    .map(|c| non_empty(c).map(strip_cloud));
    let id = direct
        .into_iter()
        .map(non_empty)
        .chain(cloud)
        .flatten()
        .next()
        .ok_or("request names no lease")?;
    validate_lease_id(id)?;
    Ok(id.to_owned())
}

/// Fails, naming the field, when any non-empty identity field in the request
/// disagrees with the record.
pub fn check_agreement(request: &Request, record: &LeaseRecord) -> Result<(), String> {
    let desired = request.desired.as_ref();
    let lease = request.lease.as_ref();
    let expected = request.expected.as_ref();
    let (rid, slug, name, cloud) = (
        &record.lease_id,
        &record.slug,
        &record.name,
        &record.cloud_id,
    );
    let checks = [
        (
            "expected.leaseId",
            expected.and_then(|e| e.lease_id.as_deref()),
            rid,
        ),
        (
            "expected.attemptLeaseId",
            expected.and_then(|e| e.attempt_lease_id.as_deref()),
            rid,
        ),
        (
            "expected.slug",
            expected.and_then(|e| e.slug.as_deref()),
            slug,
        ),
        (
            "expected.cloudId",
            expected.and_then(|e| e.cloud_id.as_deref()),
            cloud,
        ),
        (
            "lease.leaseId",
            lease.and_then(|l| l.lease_id.as_deref()),
            rid,
        ),
        ("lease.slug", lease.and_then(|l| l.slug.as_deref()), slug),
        ("lease.name", lease.and_then(|l| l.name.as_deref()), name),
        (
            "lease.cloudId",
            lease.and_then(|l| l.cloud_id.as_deref()),
            cloud,
        ),
        ("desired.leaseId", desired.map(|d| d.lease_id.as_str()), rid),
        ("desired.slug", desired.map(|d| d.slug.as_str()), slug),
        ("desired.name", desired.map(|d| d.name.as_str()), name),
    ];
    for (field, want, actual) in checks {
        if non_empty(want).is_some_and(|w| w != actual) {
            return Err(format!("{field} does not match lease {rid}"));
        }
    }
    Ok(())
}

/// The protocol lease for a record with the given status.
#[must_use]
pub fn build_lease(record: &LeaseRecord, status: &str, backend: &dyn Backend) -> Lease {
    let id = &record.lease_id;
    Lease {
        lease_id: id.clone(),
        slug: record.slug.clone(),
        name: record.name.clone(),
        cloud_id: record.cloud_id.clone(),
        status: status.to_owned(),
        ssh: Ssh {
            user: backend::SSH_USER.to_owned(),
            host: id.clone(),
            port: "22".to_owned(),
            key: backend.ssh_key(id).to_string_lossy().into_owned(),
            proxy_command: backend.proxy_command(id),
            ready_check: backend::READY_CHECK.to_owned(),
            host_key: backend.host_key(id).unwrap_or_default(),
        },
        growth_blocked: backend.growth_blocked(id),
        cache_volumes: Vec::new(),
    }
}

/// Creates the VM and waits for it; removes it again when it never gets ready.
fn create_ready(
    desired: &crate::protocol::Desired,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<LeaseRecord, String> {
    let record = backend.create(desired, diag)?;
    let _ = writeln!(diag, "acquire: created {}", record.lease_id);
    match backend.wait_ready(&record, diag) {
        Ok(()) => Ok(record),
        Err(err) => Err(match backend.destroy(&record.lease_id, diag) {
            Ok(()) => format!("{err}; vm removed"),
            Err(e2) => format!("{err}; removing vm also failed: {e2}"),
        }),
    }
}

/// Finds or creates the VM for `request.desired` and waits until it is ready.
pub fn acquire(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let desired = request.desired.as_ref().ok_or("acquire requires desired")?;
    if desired.lease_id.is_empty() {
        return Err("acquire requires desired.leaseId".to_owned());
    }
    let id = target_id(request)?;
    let _lock = backend.lock(&id)?;
    if let Some(existing) = backend.find(&id)? {
        check_agreement(request, &existing)?;
        backend.reap(&existing, diag);
        require_running(backend.power(&existing), &id)?;
        backend.wait_ready(&existing, diag)?;
        return Ok(build_lease(&existing, Power::Running.status(), backend));
    }
    let record = create_ready(desired, backend, diag)?;
    Ok(build_lease(&record, Power::Running.status(), backend))
}

/// Ok when the VM runs; else the paused, stopped or unresponsive error naming `id`.
fn require_running(power: Power, id: &str) -> Result<(), String> {
    match power {
        Power::Running => Ok(()),
        Power::Paused => Err(backend::paused_error(id)),
        Power::Stopped => Err(backend::stopped_error(id)),
        Power::Unresponsive => Err(backend::unresponsive_error(id)),
    }
}

/// Reaps a lease's orphan passt only when no operation holds its lock.
///
/// A create or resume in flight has a passt pid recorded before cloud-hypervisor
/// starts; reaping then would kill the new passt.
pub fn reap_if_idle(backend: &dyn Backend, record: &LeaseRecord, diag: &mut dyn Write) {
    if let Ok(Some(_lock)) = backend.try_lock(&record.lease_id) {
        backend.reap(record, diag);
    }
}

/// Describes the lease the request names; never creates or destroys.
///
/// A paused or stopped lease is returned with its status, because crabbox
/// status, extend and heartbeat resolve it and decide for themselves. An
/// unresponsive lease is an error unless the request is `releaseOnly`: crabbox
/// does not know that status and would wait on SSH that never answers.
pub fn resolve(
    request: &Request,
    backend: &dyn Backend,
    diag: &mut dyn Write,
) -> Result<Lease, String> {
    let id = target_id(request)?;
    let record = backend
        .find(&id)?
        .ok_or_else(|| format!("lease {id} not found"))?;
    check_agreement(request, &record)?;
    reap_if_idle(backend, &record, diag);
    let power = backend.power(&record);
    if !request.release_only && power == Power::Unresponsive {
        return Err(backend::unresponsive_error(&id));
    }
    Ok(build_lease(&record, power.status(), backend))
}
