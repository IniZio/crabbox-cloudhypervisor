//! The seam between lease operations and the VM machinery; tests fake it.

use std::io::Write;
use std::path::PathBuf;

use crate::protocol::Desired;
use crate::state::{LeaseLock, LeaseRecord};

/// Login user inside every guest.
pub const SSH_USER: &str = "crabbox";

/// Guest command that exits 0 once the guest is ready for work.
pub const READY_CHECK: &str = "/usr/local/bin/crabbox-ready";

/// Provider-side machine id for a lease.
#[must_use]
pub fn cloud_id(lease_id: &str) -> String {
    format!("ch-{lease_id}")
}

/// Everything the operations need from the host; the real impl lives in `vm`.
pub trait Backend {
    /// Reads the record for a lease; `Ok(None)` when it does not exist.
    fn find(&self, lease_id: &str) -> Result<Option<LeaseRecord>, String>;

    /// All readable lease records, sorted by lease id.
    fn list(&self) -> Result<Vec<LeaseRecord>, String>;

    /// Names of lease directories whose record is missing or unreadable.
    fn orphans(&self) -> Result<Vec<String>, String>;

    /// True when the lease's VM process is still running.
    fn alive(&self, record: &LeaseRecord) -> bool;

    /// Takes an exclusive blocking lock for the lease; released on drop.
    fn lock(&self, lease_id: &str) -> Result<LeaseLock, String>;

    /// Creates and boots a VM; persists the record before returning.
    /// On its own failure it removes everything it made.
    fn create(&self, desired: &Desired, diag: &mut dyn Write) -> Result<LeaseRecord, String>;

    /// Blocks until the guest accepts SSH and passes [`READY_CHECK`], or errors.
    fn wait_ready(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String>;

    /// Stops the VM if running and removes state and run dirs.
    /// A missing lease is `Ok`.
    fn destroy(&self, lease_id: &str, diag: &mut dyn Write) -> Result<(), String>;

    /// Path of the lease's private SSH key.
    fn ssh_key(&self, lease_id: &str) -> PathBuf;

    /// SSH `ProxyCommand` that reaches the lease's guest.
    fn proxy_command(&self, lease_id: &str) -> String;
}
