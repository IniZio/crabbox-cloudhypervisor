//! Sizing a lease from its class or, with none, from the host.

use super::RealBackend;
use crate::size::{self, Size};
use crate::state::LeaseRecord;

impl RealBackend {
    /// Sizes a lease before any download or state exists. The host is read
    /// here, once per boot, so a resized host sizes the next VM.
    pub(super) fn lease_size(&self, class: Option<&str>) -> Result<Size, String> {
        let c = &self.config;
        let host = size::host()?;
        class.map_or_else(
            || size::no_class_size(&host, c.cpus, c.memory_mib),
            |name| size::resolve(&c.classes, name, &host),
        )
    }

    /// The size a lease boots or restores at: the one it was created with.
    /// A record from before sizes were stored is sized like a new no-class lease.
    pub(super) fn record_size(&self, record: &LeaseRecord) -> Result<Size, String> {
        record.size.map_or_else(|| self.lease_size(None), Ok)
    }
}
