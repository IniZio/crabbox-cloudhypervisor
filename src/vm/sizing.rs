//! Sizing a lease from its class.

use super::RealBackend;
use crate::size::{self, Size};

impl RealBackend {
    /// The size of a lease with no class.
    pub(super) const fn default_size(&self) -> Size {
        Size {
            cpus: self.config.cpus,
            memory_mib: self.config.memory_mib,
        }
    }

    /// Sizes a lease from its class before any download or state exists.
    pub(super) fn lease_size(&self, class: Option<&str>) -> Result<Size, String> {
        let c = &self.config;
        let default = self.default_size();
        let host = if class.is_some() {
            size::host()?
        } else {
            size::Host {
                total_cpus: c.cpus,
                total_memory_mib: c.memory_mib,
            }
        };
        size::resolve(&c.classes, default, class, &host)
    }
}
