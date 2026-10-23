//! The watcher's log: grows logged as they happen, each distinct failure
//! logged once and its repeats counted.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use crate::state;

/// Writes timestamped lines and folds repeated failures into a count.
pub struct Log<'a> {
    /// Where lines go.
    out: &'a mut dyn Write,
    /// Failures still recurring, with how often each was seen.
    failing: BTreeMap<String, u32>,
    /// Failures seen since the last `end_tick`.
    this_tick: BTreeSet<String>,
}

impl<'a> Log<'a> {
    /// A log writing to `out`.
    pub fn new(out: &'a mut dyn Write) -> Self {
        Self {
            out,
            failing: BTreeMap::new(),
            this_tick: BTreeSet::new(),
        }
    }

    /// Writes one timestamped line.
    pub fn note(&mut self, message: &str) {
        let _ = writeln!(self.out, "{} {message}", state::now_unix());
    }

    /// Records a failure; only its first occurrence is written.
    pub fn fail(&mut self, message: String) {
        let seen = self.failing.entry(message.clone()).or_insert(0);
        *seen = seen.saturating_add(1);
        if *seen == 1 {
            self.note(&message);
        }
        self.this_tick.insert(message);
    }

    /// Ends a tick that read the guest: failures that did not recur are
    /// written as cleared, with their count.
    pub fn end_tick(&mut self) {
        let cleared: Vec<String> = self
            .failing
            .keys()
            .filter(|m| !self.this_tick.contains(*m))
            .cloned()
            .collect();
        for message in cleared {
            let count = self.failing.remove(&message).unwrap_or(0);
            self.note(&format!("cleared after {count} times: {message}"));
        }
        self.this_tick.clear();
    }

    /// Writes every failure still recurring, with its count.
    pub fn finish(&mut self) {
        for (message, count) in std::mem::take(&mut self.failing) {
            self.note(&format!("still failing after {count} times: {message}"));
        }
    }
}
