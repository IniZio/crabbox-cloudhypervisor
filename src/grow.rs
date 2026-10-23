//! Decides when a running guest gets more disk, vCPUs or memory.
//!
//! Pure: it reads samples and the VMM's current sizes and returns actions.

use crate::config::Growth;
use crate::vm::spec::MEMORY_BLOCK;

/// Bytes per GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// Bytes per MiB.
pub const MIB: u64 = 1024 * 1024;
/// CPU or memory use above this percent counts as pressure.
pub const HOT_PERCENT: u64 = 90;
/// Consecutive samples under pressure before a raise.
pub const SUSTAINED_SAMPLES: u32 = 3;

/// Cumulative CPU time from the `cpu` line of `/proc/stat`, in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Ticks spent neither idle nor waiting on I/O.
    pub busy: u64,
    /// All ticks.
    pub total: u64,
}

/// One reading of the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Size of the root filesystem, bytes.
    pub fs_size: u64,
    /// Space on the root filesystem available to users, bytes.
    pub fs_avail: u64,
    /// CPU time counters.
    pub cpu: CpuTimes,
    /// `MemTotal`, bytes.
    pub mem_total: u64,
    /// `MemAvailable`, bytes.
    pub mem_avail: u64,
    /// Online CPUs.
    pub online_cpus: u32,
}

/// What the VMM currently gives the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Current {
    /// Root disk image size, bytes.
    pub disk_bytes: u64,
    /// vCPUs.
    pub vcpus: u32,
    /// Memory, bytes.
    pub memory_bytes: u64,
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
    /// Raise the vCPU count.
    RaiseCpus {
        /// Count before.
        from: u32,
        /// Count after.
        to: u32,
    },
    /// Raise guest memory.
    RaiseMemory {
        /// Bytes before.
        from: u64,
        /// Bytes after.
        to: u64,
    },
}

/// Remembers what decisions need across samples.
#[derive(Debug, Clone)]
pub struct Planner {
    /// Limits and triggers.
    growth: Growth,
    /// CPU counters of the previous sample.
    prev_cpu: Option<CpuTimes>,
    /// Consecutive samples with CPU pressure.
    hot_cpu: u32,
    /// Consecutive samples with memory pressure.
    hot_mem: u32,
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
            prev_cpu: None,
            hot_cpu: 0,
            hot_mem: 0,
            fs_pending: true,
            max_noted: false,
        }
    }

    /// Actions for one sample.
    pub fn plan(&mut self, sample: &Sample, current: &Current) -> Vec<Action> {
        let mut actions = self.disk(sample, current);
        actions.extend(self.cpus(sample, current));
        actions.extend(self.memory(sample, current));
        actions
    }

    /// Records whether the last `GrowFs` succeeded.
    pub const fn fs_grown(&mut self, ok: bool) {
        self.fs_pending = !ok;
    }

    /// Grows the filesystem while it lags the disk, else the disk when space is low.
    fn disk(&mut self, sample: &Sample, current: &Current) -> Vec<Action> {
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

    /// Doubles vCPUs (up to the cap) after sustained CPU pressure.
    fn cpus(&mut self, sample: &Sample, current: &Current) -> Option<Action> {
        let hot = self.prev_cpu.replace(sample.cpu).is_some_and(|prev| {
            let total = sample.cpu.total.saturating_sub(prev.total);
            let busy = sample.cpu.busy.saturating_sub(prev.busy);
            total > 0 && busy.saturating_mul(100) > total.saturating_mul(HOT_PERCENT)
        });
        self.hot_cpu = if hot {
            self.hot_cpu.saturating_add(1)
        } else {
            0
        };
        let cap = self.growth.cpus_max;
        if self.hot_cpu < SUSTAINED_SAMPLES || current.vcpus >= cap {
            return None;
        }
        self.hot_cpu = 0;
        Some(Action::RaiseCpus {
            from: current.vcpus,
            to: current.vcpus.saturating_mul(2).min(cap),
        })
    }

    /// Doubles memory (up to the cap, in whole hotplug blocks) after sustained pressure.
    fn memory(&mut self, sample: &Sample, current: &Current) -> Option<Action> {
        let used = sample.mem_total.saturating_sub(sample.mem_avail);
        let hot = sample.mem_total > 0
            && used.saturating_mul(100) > sample.mem_total.saturating_mul(HOT_PERCENT);
        self.hot_mem = if hot {
            self.hot_mem.saturating_add(1)
        } else {
            0
        };
        let room = self
            .growth
            .memory_mib_max
            .saturating_mul(MIB)
            .saturating_sub(current.memory_bytes);
        let add = current.memory_bytes.min(room) / MEMORY_BLOCK * MEMORY_BLOCK;
        if self.hot_mem < SUSTAINED_SAMPLES || add == 0 {
            return None;
        }
        self.hot_mem = 0;
        Some(Action::RaiseMemory {
            from: current.memory_bytes,
            to: current.memory_bytes + add,
        })
    }
}

#[cfg(test)]
mod tests;
