//! Decides when a running guest gets more disk.
//!
//! Pure: it reads samples and the VMM's current disk size and returns actions.

use std::collections::BTreeSet;

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

/// One cache filesystem in the guest. Its disk is sized up front, so it grows
/// inside the disk without touching the VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheFs {
    /// Guest mount point.
    pub mount: String,
    /// Size of the filesystem, bytes.
    pub fs_size: u64,
    /// Space available to users, bytes.
    pub fs_avail: u64,
    /// Size of the disk holding it, bytes.
    pub disk_bytes: u64,
}

/// One change to a cache filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheAction {
    /// Grow the filesystem at `mount` to `to` bytes.
    Grow {
        /// Guest mount point.
        mount: String,
        /// Size before, bytes.
        from: u64,
        /// Size after, bytes.
        to: u64,
    },
    /// The filesystem at `mount` is low on space and fills its disk.
    AtDisk {
        /// Guest mount point.
        mount: String,
        /// Space available, bytes.
        avail: u64,
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
    /// Cache mounts whose `AtDisk` was reported for the current low-space spell.
    caches_noted: BTreeSet<String>,
}

impl Planner {
    /// A planner that first makes sure the filesystem fills the disk.
    #[must_use]
    pub const fn new(growth: Growth) -> Self {
        Self {
            growth,
            fs_pending: true,
            max_noted: false,
            caches_noted: BTreeSet::new(),
        }
    }

    /// Actions for one sample.
    pub fn plan(&mut self, sample: &Sample, current: Current) -> Vec<Action> {
        self.disk(sample, current)
    }

    /// Actions for the cache filesystems: each low one grows by the same rule
    /// as the root disk, up to the size of its disk.
    pub fn plan_caches(&mut self, caches: &[CacheFs]) -> Vec<CacheAction> {
        let mut out = Vec::new();
        for c in caches {
            let floor = self.floor(c.fs_size);
            if c.fs_avail >= floor {
                self.caches_noted.remove(&c.mount);
                continue;
            }
            if c.fs_size >= c.disk_bytes {
                if self.caches_noted.insert(c.mount.clone()) {
                    out.push(CacheAction::AtDisk {
                        mount: c.mount.clone(),
                        avail: c.fs_avail,
                    });
                }
                continue;
            }
            let to = c
                .fs_size
                .saturating_add(self.step(floor - c.fs_avail))
                .min(c.disk_bytes);
            out.push(CacheAction::Grow {
                mount: c.mount.clone(),
                from: c.fs_size,
                to,
            });
        }
        out
    }

    /// Free space below which a filesystem of `fs_size` bytes grows.
    const fn floor(&self, fs_size: u64) -> u64 {
        let percent_floor = fs_size / 100 * self.growth.disk_free_percent;
        let gib_floor = self.growth.disk_free_gib.saturating_mul(GIB);
        if percent_floor > gib_floor {
            percent_floor
        } else {
            gib_floor
        }
    }

    /// Bytes one grow adds to cover `shortfall`. Free space is checked up front
    /// by tools such as emulators, so one resize must reach the floor instead of
    /// creeping up a step per tick.
    fn step(&self, shortfall: u64) -> u64 {
        self.growth
            .disk_step_gib
            .saturating_mul(GIB)
            .max(shortfall.div_ceil(GIB).saturating_mul(GIB))
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
        let floor = self.floor(sample.fs_size);
        if sample.fs_avail >= floor {
            self.max_noted = false;
            return Vec::new();
        }
        let max = self.growth.disk_max_gib.map(|g| g.saturating_mul(GIB));
        if let Some(max) = max.filter(|m| current.disk_bytes >= *m) {
            if self.max_noted {
                return Vec::new();
            }
            self.max_noted = true;
            return vec![Action::DiskAtMax { max }];
        }
        let step = self.step(floor - sample.fs_avail);
        let to = current
            .disk_bytes
            .saturating_add(step)
            .min(max.unwrap_or(u64::MAX));
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
