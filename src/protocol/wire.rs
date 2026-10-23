//! Wire response types: lease, SSH details and inventory rows.

use serde::Serialize;

/// SSH connection details of a lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ssh {
    /// Login user.
    pub user: String,
    /// Host name.
    pub host: String,
    /// Port, as the protocol's string.
    pub port: String,
    /// Absolute path of the private key crabbox must use.
    pub key: String,
    /// `ProxyCommand` line.
    pub proxy_command: String,
    /// Shell command that proves readiness.
    pub ready_check: String,
    /// Guest's ed25519 host key as one OpenSSH line; omitted when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_key: String,
}

/// A lease as reported back to crabbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Provider-side instance id.
    pub cloud_id: String,
    /// Lease status.
    pub status: String,
    /// SSH connection details.
    pub ssh: Ssh,
    /// Why the disk cannot grow, when something blocks it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub growth_blocked: String,
}

/// One inventory row reported by list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRow {
    /// Crabbox lease id.
    pub lease_id: String,
    /// Short lease slug.
    pub slug: String,
    /// Lease name.
    pub name: String,
    /// Provider-side instance id.
    pub cloud_id: String,
    /// Lease status.
    pub status: String,
    /// Why the disk cannot grow, when something blocks it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub growth_blocked: String,
}
