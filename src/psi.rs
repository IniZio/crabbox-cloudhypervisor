//! The host's memory pressure feed: a trigger on `/proc/pressure/memory`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::OFlags;
use rustix::time::Timespec;

use crate::balloon::Trigger;

/// The host-wide memory pressure file.
pub const MEMORY_PRESSURE: &str = "/proc/pressure/memory";

/// A registered trigger; the kernel raises `POLLPRI` on it about once per window.
#[derive(Debug)]
pub struct Psi {
    /// The pressure file the trigger lives on; closing it removes the trigger.
    file: File,
}

impl Psi {
    /// Registers `trigger` on the pressure file at `path`.
    pub fn register(trigger: &Trigger, path: &Path) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
            .open(path)
            .map_err(|e| format!("open pressure file: {e}"))?;
        // One write including the NUL: the kernel parses a single buffer.
        let mut text = trigger.as_str().as_bytes().to_vec();
        text.push(0);
        file.write_all(&text)
            .map_err(|e| format!("register pressure trigger: {e}"))?;
        Ok(Self { file })
    }

    /// Waits up to `timeout`; true when the trigger fired.
    pub fn wait(&self, timeout: Duration) -> Result<bool, String> {
        let limit = Timespec {
            tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let mut fds = [PollFd::new(&self.file, PollFlags::PRI)];
        match poll(&mut fds, Some(&limit)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(false),
            Err(e) => return Err(format!("poll pressure file: {e}")),
        }
        let revents = fds.first().map_or(PollFlags::empty(), PollFd::revents);
        if revents.contains(PollFlags::ERR) {
            return Err("pressure trigger was dropped by the kernel".to_owned());
        }
        Ok(revents.contains(PollFlags::PRI))
    }
}

/// Whether `trigger` registers on the pressure file at `path`.
pub fn probe(trigger: &Trigger, path: &Path) -> Result<(), String> {
    Psi::register(trigger, path).map(drop)
}

#[cfg(test)]
mod tests;
