//! Protocol v1 framing: one JSON request in, one JSON response out.

use serde::Serialize;
use serde_json::Value;

mod cache;
mod wire;
use cache::parse_cache_volumes;
pub use cache::CacheVolumeRequest;
pub use wire::{CacheVolumeEcho, Lease, LeaseRow, Ssh};

/// The only protocol version this adapter speaks.
const VERSION: u64 = 1;

/// An operation named by the crabbox external provider protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Readiness check.
    Doctor,
    /// Create a lease.
    Acquire,
    /// Look up an existing lease.
    Resolve,
    /// Enumerate leases.
    List,
    /// Release a lease.
    Release,
    /// Refresh a lease.
    Touch,
    /// Remove stale resources.
    Cleanup,
    /// Pause a lease's VM.
    Pause,
    /// Resume a paused or stopped lease.
    Resume,
}

impl Operation {
    /// Wire name of the operation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Acquire => "acquire",
            Self::Resolve => "resolve",
            Self::List => "list",
            Self::Release => "release",
            Self::Touch => "touch",
            Self::Cleanup => "cleanup",
            Self::Pause => "pause",
            Self::Resume => "resume",
        }
    }

    /// Inverse of [`Operation::as_str`].
    fn from_wire(name: &str) -> Option<Self> {
        [
            Self::Doctor,
            Self::Acquire,
            Self::Resolve,
            Self::List,
            Self::Release,
            Self::Touch,
            Self::Cleanup,
            Self::Pause,
            Self::Resume,
        ]
        .into_iter()
        .find(|op| op.as_str() == name)
    }
}

/// The lease crabbox wants created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Optional machine class.
    pub class: Option<String>,
    /// Cache volumes to attach, as sent.
    pub cache_volumes: Vec<CacheVolumeRequest>,
}

/// Identifiers crabbox expects an existing lease to carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// Crabbox lease id.
    pub lease_id: Option<String>,
    /// Lease id of the acquire attempt.
    pub attempt_lease_id: Option<String>,
    /// Short lease slug.
    pub slug: Option<String>,
    /// Provider-side instance id.
    pub cloud_id: Option<String>,
}

/// Identifiers of an existing lease, as sent for release and touch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseRef {
    /// Crabbox lease id.
    pub lease_id: Option<String>,
    /// Short lease slug.
    pub slug: Option<String>,
    /// Lease name.
    pub name: Option<String>,
    /// Provider-side instance id.
    pub cloud_id: Option<String>,
}

/// A parsed request. Fields not yet needed are ignored.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the wire request, which carries independent boolean flags"
)]
pub struct Request {
    /// Requested operation.
    pub operation: Operation,
    /// Provider config object, or `Value::Null` when absent.
    pub config: Value,
    /// Lease to create, for acquire.
    pub desired: Option<Desired>,
    /// Identifiers to match, for resolve and others.
    pub expected: Option<Expected>,
    /// Keep the lease after the command finishes.
    pub keep: bool,
    /// Existing lease, for release and touch.
    pub lease: Option<LeaseRef>,
    /// Report what cleanup would remove without removing it.
    pub dry_run: bool,
    /// Resolve without claiming the lease for use.
    pub release_only: bool,
    /// Lease id, slug or name to look up, for resolve.
    pub id: Option<String>,
    /// Lifecycle state filter, for list and cleanup.
    pub state: Option<String>,
    /// Take over a lease claimed elsewhere.
    pub reclaim: bool,
    /// Act even when the lease looks in use.
    pub force: bool,
    /// Apply to every matching lease.
    pub all: bool,
    /// Refresh provider state before answering.
    pub refresh: bool,
}

/// Non-empty string field, else `None`.
fn opt_str(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse an optional `key` object with `parse`; `null` counts as absent.
fn optional_object<T>(
    root: &serde_json::Map<String, Value>,
    key: &str,
    parse: impl FnOnce(&serde_json::Map<String, Value>) -> Result<T, String>,
) -> Result<Option<T>, String> {
    match root.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) => parse(object).map(Some),
        Some(_) => Err(format!("{key} must be an object")),
    }
}

/// Optional boolean field; absent means false.
fn flag(object: &serde_json::Map<String, Value>, key: &str) -> Result<bool, String> {
    object.get(key).map_or(Ok(false), |v| {
        v.as_bool()
            .ok_or_else(|| format!("{key} must be a boolean"))
    })
}

/// Reject requests whose protocol version this adapter does not speak.
fn check_version(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    match object.get("protocolVersion").and_then(Value::as_u64) {
        Some(VERSION) => {}
        Some(other) => {
            return Err(format!(
                "unsupported protocol version {other}; this adapter speaks version {VERSION}"
            ));
        }
        None => {
            return Err(format!(
                "missing or invalid protocolVersion; this adapter speaks version {VERSION}"
            ));
        }
    }
    Ok(())
}

/// Parse the optional `desired` object.
fn parse_desired(object: &serde_json::Map<String, Value>) -> Result<Option<Desired>, String> {
    optional_object(object, "desired", |o| {
        let required = |key: &str| {
            opt_str(o, key).ok_or_else(|| format!("desired.{key} must be a non-empty string"))
        };
        Ok(Desired {
            lease_id: required("leaseId")?,
            slug: required("slug")?,
            name: required("name")?,
            class: opt_str(o, "class"),
            cache_volumes: parse_cache_volumes(o)?,
        })
    })
}

/// Parse the optional `expected` object.
fn parse_expected(object: &serde_json::Map<String, Value>) -> Result<Option<Expected>, String> {
    optional_object(object, "expected", |o| {
        Ok(Expected {
            lease_id: opt_str(o, "leaseId"),
            attempt_lease_id: opt_str(o, "attemptLeaseId"),
            slug: opt_str(o, "slug"),
            cloud_id: opt_str(o, "cloudId"),
        })
    })
}

/// Parse the optional `lease` object.
fn parse_lease(object: &serde_json::Map<String, Value>) -> Result<Option<LeaseRef>, String> {
    optional_object(object, "lease", |o| {
        Ok(LeaseRef {
            lease_id: opt_str(o, "leaseId"),
            slug: opt_str(o, "slug"),
            name: opt_str(o, "name"),
            cloud_id: opt_str(o, "cloudId"),
        })
    })
}

/// Parse a single request object.
///
/// # Errors
///
/// Returns a human-readable message for malformed input. Messages never
/// include config values, which may carry secrets.
pub fn parse_request(input: &[u8]) -> Result<Request, String> {
    let value: Value =
        serde_json::from_slice(input).map_err(|e| format!("invalid request JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "request must be a JSON object".to_owned())?;

    check_version(object)?;

    let name = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing operation".to_owned())?;
    let operation = Operation::from_wire(name)
        .ok_or_else(|| format!("unknown operation {}", Value::String(name.to_owned())))?;

    let config = match object.get("config") {
        None | Some(Value::Null) => Value::Null,
        Some(config @ Value::Object(_)) => config.clone(),
        Some(_) => return Err("config must be an object".to_owned()),
    };

    let desired = parse_desired(object)?;
    let expected = parse_expected(object)?;
    let keep = flag(object, "keep")?;
    let lease = parse_lease(object)?;
    let dry_run = flag(object, "dryRun")?;
    let release_only = flag(object, "releaseOnly")?;
    let id = opt_str(object, "id");
    let state = opt_str(object, "state");
    let reclaim = flag(object, "reclaim")?;
    let force = flag(object, "force")?;
    let all = flag(object, "all")?;
    let refresh = flag(object, "refresh")?;

    Ok(Request {
        operation,
        config,
        desired,
        expected,
        keep,
        lease,
        dry_run,
        release_only,
        id,
        state,
        reclaim,
        force,
        all,
        refresh,
    })
}

/// Success response carrying a message.
#[must_use]
pub fn message_response(message: &str) -> String {
    format!(
        r#"{{"protocolVersion":{VERSION},"message":{}}}"#,
        Value::String(message.to_owned())
    )
}

/// Error response; crabbox reads `error` before the version.
#[must_use]
pub fn error_response(error: &str) -> String {
    format!(r#"{{"error":{}}}"#, Value::String(error.to_owned()))
}

/// Success response carrying inventory rows; always a JSON array.
#[must_use]
pub fn leases_response(rows: &[LeaseRow]) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope<'a> {
        protocol_version: u64,
        leases: &'a [LeaseRow],
    }
    let envelope = Envelope {
        protocol_version: VERSION,
        leases: rows,
    };
    serde_json::to_string(&envelope).unwrap_or_default()
}

/// Success response for operations that return nothing.
#[must_use]
pub fn ok_response() -> String {
    format!(r#"{{"protocolVersion":{VERSION}}}"#)
}

/// Success response carrying a lease.
#[must_use]
pub fn lease_response(lease: &Lease) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope<'a> {
        protocol_version: u64,
        lease: &'a Lease,
    }
    let envelope = Envelope {
        protocol_version: VERSION,
        lease,
    };
    serde_json::to_string(&envelope).unwrap_or_default()
}

#[cfg(test)]
mod tests;
