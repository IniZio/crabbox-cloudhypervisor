//! Decides when a running guest gets more disk.
//!
//! Pure: it reads samples and the VMM's current disk size and returns actions.

use crate::config::Growth;

/// Bytes per GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// One reading of the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Size of the root filesystem, bytes.
    pub fs_size: u64,
    /// Space on the root filesystem available to users, bytes.
    pub fs_avail: u64,
}

/// What the VMM currently gives the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    /// Root disk image size, bytes.
    pub disk_bytes: u64,
}

/// One change to make, in the order returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Grow the root disk image.
    GrowDisk {
        /// Size before, bytes.
        from: u64,
        /// Size after, bytes.
        to: u64,
    },
    /// Grow the root partition and filesystem to fill the disk (idempotent).
    GrowFs,
    /// The root filesystem is low on space and the disk is at its cap.
    DiskAtMax {
        /// The cap, bytes.
        max: u64,
    },
}

/// Remembers what decisions need across samples.
#[derive(Debug, Clone)]
pub struct Planner {
    /// Limits and triggers.
    growth: Growth,
    /// The filesystem may not fill the disk yet; grow it before the disk again.
    fs_pending: bool,
    /// `DiskAtMax` was already reported for the current low-space spell.
    max_noted: bool,
}

impl Planner {
    /// A planner that first makes sure the filesystem fills the disk.
    #[must_use]
    pub const fn new(growth: Growth) -> Self {
        Self {
            growth,
            fs_pending: true,
            max_noted: false,
        }
    }

    /// Actions for one sample.
    pub fn plan(&mut self, sample: &Sample, current: Current) -> Vec<Action> {
        self.disk(sample, current)
    }

    /// Records whether the last `GrowFs` succeeded.
    pub const fn fs_grown(&mut self, ok: bool) {
        self.fs_pending = !ok;
    }

    /// Grows the filesystem while it lags the disk, else the disk when space is low.
    fn disk(&mut self, sample: &Sample, current: Current) -> Vec<Action> {
        if self.fs_pending {
            return vec![Action::GrowFs];
        }
        let percent_floor = sample.fs_size / 100 * self.growth.disk_free_percent;
        let floor = percent_floor.max(self.growth.disk_free_gib.saturating_mul(GIB));
        if sample.fs_avail >= floor {
            self.max_noted = false;
            return Vec::new();
        }
        let max = self.growth.disk_max_gib.saturating_mul(GIB);
        if current.disk_bytes >= max {
            if self.max_noted {
                return Vec::new();
            }
            self.max_noted = true;
            return vec![Action::DiskAtMax { max }];
        }
        let step = self.growth.disk_step_gib.saturating_mul(GIB);
        let to = current.disk_bytes.saturating_add(step).min(max);
        self.fs_pending = true;
        vec![
            Action::GrowDisk {
                from: current.disk_bytes,
                to,
            },
            Action::GrowFs,
        ]
    }
}

#[cfg(test)]
mod tests;
