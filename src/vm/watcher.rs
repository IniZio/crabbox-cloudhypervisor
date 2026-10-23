//! Starts and stops the per-lease growth watcher.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

use super::power::terminate;
use super::RealBackend;
use crate::host;
use crate::state::LeaseRecord;
use crate::watch::WatchSpec;

impl RealBackend {
    /// Writes the watch spec and spawns `<exe> watch <id> --state-dir <dir>`.
    pub(super) fn start_watcher(
        &self,
        record: &mut LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let id = record.lease_id.clone();
        let watch = WatchSpec {
            growth: self.config.growth,
            ssh: self.config.ssh.clone(),
        };
        let json = serde_json::to_vec(&watch).map_err(|e| format!("encode watch spec: {e}"))?;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.store.watch_spec_path(&id))
            .and_then(|mut f| f.write_all(&json))
            .map_err(|e| format!("write watch spec: {e}"))?;
        let args = [
            OsString::from("watch"),
            OsString::from(&id),
            OsString::from("--state-dir"),
            self.config.state_dir.clone().into_os_string(),
        ];
        let pid = host::spawn_detached(&self.exe, &args, &self.store.grow_log(&id))?;
        record.set_watch_pid(pid);
        self.store.write(record)?;
        let _ = writeln!(diag, "growth watcher started");
        Ok(())
    }

    /// Stops the record's watcher if it is still the process recorded.
    pub(super) fn stop_watcher(record: &LeaseRecord, diag: &mut dyn Write) {
        if let Some(pid) = record.watch_pid {
            terminate(
                pid,
                || Self::watch_alive(record),
                Duration::from_secs(2),
                diag,
            );
        }
    }

    /// Starts a watcher unless the record's own is still running, so a lease
    /// never has none and never two.
    pub(super) fn ensure_watcher(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        if Self::watch_alive(record) {
            return Ok(());
        }
        self.start_watcher(&mut record.clone(), diag)
    }

    /// True iff the record's watcher is alive and is the one recorded: same
    /// pid, start time and boot, whatever the binary is named.
    pub(super) fn watch_alive(record: &LeaseRecord) -> bool {
        record
            .watch_pid
            .is_some_and(|pid| host::owns(pid, "", record.watch_start, record.boot_id.as_deref()))
    }
}

#[cfg(test)]
mod tests;
