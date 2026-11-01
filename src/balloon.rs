//! Decides how far to inflate a guest's balloon as the host runs short of memory.
//!
//! Pure: it reads pressure events and the clock and returns the balloon size
//! to ask the VMM for. The host's memory pressure feed lives in [`crate::psi`].

use serde::{Deserialize, Serialize};

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;
/// The kernel only accepts a pressure window that is a multiple of this.
const WINDOW_UNIT_US: u64 = 2_000_000;

/// A validated pressure trigger: `some|full <stall µs> <window µs>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Trigger(String);

impl Trigger {
    /// Parses and validates trigger text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || "must look like \"some 500000 2000000\" (some|full, stall µs, window µs)";
        let words: Vec<&str> = text.split_whitespace().collect();
        let [kind, stall, window] = words.as_slice() else {
            return Err(bad().to_owned());
        };
        let (Ok(stall), Ok(window)) = (stall.parse::<u64>(), window.parse::<u64>()) else {
            return Err(bad().to_owned());
        };
        if !matches!(*kind, "some" | "full") || stall == 0 {
            return Err(bad().to_owned());
        }
        if window == 0 || window % WINDOW_UNIT_US != 0 {
            return Err(
                "window must be a multiple of 2 s (2000000 µs) and at least 2 s: an \
                 unprivileged trigger is rejected otherwise"
                    .to_owned(),
            );
        }
        if stall > window {
            return Err("stall must not exceed the window".to_owned());
        }
        Ok(Self(text.split_whitespace().collect::<Vec<_>>().join(" ")))
    }

    /// The trigger text as the kernel takes it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Balloon settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BalloonConfig {
    /// Whether leases balloon at all.
    pub enabled: bool,
    /// Host memory pressure that counts as an event.
    pub trigger: Trigger,
    /// Percent of class memory added to the balloon per event.
    pub step_percent: u64,
    /// MiB of class memory the balloon never takes.
    pub floor_mib: u64,
    /// Quiet seconds before the balloon steps back down.
    pub cooldown_secs: u64,
}

/// The trigger used unless `memoryPressureTrigger` is set.
pub const DEFAULT_TRIGGER: &str = "some 500000 2000000";

impl Default for BalloonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: Trigger(DEFAULT_TRIGGER.to_owned()),
            step_percent: 25,
            floor_mib: 1024,
            cooldown_secs: 60,
        }
    }
}

/// The balloon size to ask for, given pressure events and quiet time.
#[derive(Debug, Clone)]
pub struct Inflator {
    /// Bytes added or removed per step.
    step: u64,
    /// Largest balloon, bytes.
    max: u64,
    /// Quiet seconds before a step down.
    cooldown: u64,
    /// Balloon size last asked for, bytes.
    target: u64,
    /// Second of the last event or step down.
    last: u64,
}

impl Inflator {
    /// An inflator for a guest of `memory_mib`, balloon empty.
    #[must_use]
    pub const fn new(config: &BalloonConfig, memory_mib: u64) -> Self {
        Self {
            step: memory_mib.saturating_mul(config.step_percent) / 100 * MIB,
            max: memory_mib.saturating_sub(config.floor_mib) * MIB,
            cooldown: config.cooldown_secs,
            target: 0,
            last: 0,
        }
    }

    /// Balloon size last asked for, bytes.
    #[must_use]
    pub const fn target(&self) -> u64 {
        self.target
    }

    /// A pressure event at second `now`; the new target when it changed.
    pub fn event(&mut self, now: u64) -> Option<u64> {
        self.last = now;
        let next = self.target.saturating_add(self.step).min(self.max);
        self.move_to(next)
    }

    /// A tick with no event at second `now`; the new target when it changed.
    pub fn quiet(&mut self, now: u64) -> Option<u64> {
        if self.target == 0 || now < self.last.saturating_add(self.cooldown) {
            return None;
        }
        self.last = now;
        self.move_to(self.target.saturating_sub(self.step))
    }

    /// Puts back `target` after the VMM refused the last change.
    pub const fn revert(&mut self, target: u64) {
        self.target = target;
    }

    /// Sets the target; `Some` only when it differs.
    fn move_to(&mut self, next: u64) -> Option<u64> {
        let changed = next != self.target;
        self.target = next;
        changed.then_some(next)
    }
}

#[cfg(test)]
mod tests;
