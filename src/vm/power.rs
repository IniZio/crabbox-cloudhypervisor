//! Power control, host keys and process teardown for the real backend.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

use super::{mkdir_private, remove_tree, spec, RealBackend, CH_COMM, PASST_COMM};
use crate::backend::{Backend, Power};
use crate::ch::ChClient;
use crate::host;
use crate::ready;
use crate::state::LeaseRecord;

/// Timeout for each API call while stopping a VM.
const STOP_API_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for each state query or power call.
const POWER_API_TIMEOUT: Duration = Duration::from_secs(2);
/// Delay between state polls.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// One cloud-hypervisor API call used to stop a VM.
type Step = fn(&ChClient) -> Result<(), String>;

/// Sends TERM, then KILL if the process outlives `grace`; `alive` says whether
/// `pid` is still the recorded process.
pub(super) fn terminate(pid: u32, alive: impl Fn() -> bool, grace: Duration, diag: &mut dyn Write) {
    if !alive() {
        return;
    }
    if let Err(e) = host::kill(pid, "TERM") {
        let _ = writeln!(diag, "warn: {e}");
    }
    if !host::wait_until_dead(&alive, grace) {
        if let Err(e) = host::kill(pid, "KILL") {
            let _ = writeln!(diag, "warn: {e}");
        }
    }
}

/// First two whitespace fields of a public key line.
fn key_fields(text: &str) -> Result<String, String> {
    let mut fields = text.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(kind), Some(blob)) => Ok(format!("{kind} {blob}")),
        _ => Err("host public key is malformed".to_owned()),
    }
}

impl RealBackend {
    /// Asks cloud-hypervisor to shut down over its API; failures only warn.
    /// Returns false when the first call fails, so a hung process is not retried.
    fn stop_via_api(record: &LeaseRecord, diag: &mut dyn Write) -> bool {
        let client = ChClient::new(&record.api_socket()).with_timeout(STOP_API_TIMEOUT);
        let steps: [(&str, Step); 3] = [
            ("vm.shutdown", ChClient::shutdown),
            ("vm.delete", ChClient::delete),
            ("vmm.shutdown", ChClient::vmm_shutdown),
        ];
        for (n, (name, step)) in steps.into_iter().enumerate() {
            if let Err(e) = step(&client) {
                let _ = writeln!(diag, "warn: {name}: {e}");
                if n == 0 {
                    return false;
                }
            }
        }
        true
    }

    /// Stops cloud-hypervisor and passt for a record.
    pub(super) fn stop_processes(record: &LeaseRecord, diag: &mut dyn Write) {
        Self::note_unstamped(record, diag);
        Self::stop_watcher(record, diag);
        if let Some(pid) = record.ch_pid {
            let alive = || Self::ch_alive(record);
            if record.api_socket().exists() && alive() && Self::stop_via_api(record, diag) {
                let _ = host::wait_until_dead(alive, Duration::from_secs(5));
            }
            terminate(pid, alive, Duration::from_secs(2), diag);
        }
        if let Some(pid) = record.passt_pid {
            terminate(
                pid,
                || Self::passt_alive(record),
                Duration::from_secs(2),
                diag,
            );
        }
    }

    /// Makes the guest ssh host key and the known-hosts file; returns (private, public).
    pub(super) fn make_host_key(&self, id: &str) -> Result<(String, String), String> {
        let path = self.store.host_key_path(id);
        let line = host::ssh_keygen(&self.config.ssh_keygen, &path, "")?;
        let public = key_fields(&line)?;
        let private = fs::read_to_string(&path).map_err(|e| format!("read host key: {e}"))?;
        let known = ready::known_hosts_line(&format!("crabbox-{id}"), &public);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.store.known_hosts_path(id))
            .map_err(|e| format!("write known_hosts: {e}"))?;
        file.write_all(known.as_bytes())
            .map_err(|e| format!("write known_hosts: {e}"))?;
        Ok((private, public))
    }

    /// Reads the guest host public key as `type blob`.
    pub(super) fn read_host_key(&self, id: &str) -> Result<String, String> {
        let text = fs::read_to_string(self.store.host_pub_path(id))
            .map_err(|e| format!("host key for {id}: {e}"))?;
        key_fields(&text)
    }

    /// Live power state of a record.
    pub(super) fn power_of(record: &LeaseRecord) -> Power {
        if !Self::ch_alive(record) {
            return Power::Stopped;
        }
        let client = ChClient::new(&record.api_socket()).with_timeout(POWER_API_TIMEOUT);
        match client.state().as_deref() {
            Ok("Paused") => Power::Paused,
            Ok(_) => Power::Running,
            Err(_) => Power::Unresponsive,
        }
    }

    /// Polls vm.info until it reports `want`, or errors naming the last state.
    fn wait_state(&self, client: &ChClient, want: &str) -> Result<(), String> {
        let deadline = Instant::now() + self.config.ready_timeout;
        loop {
            let last = client.state();
            if last.as_deref() == Ok(want) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "vm did not reach {want}; last state: {}",
                    last.unwrap_or_else(|e| format!("error ({e})"))
                ));
            }
            std::thread::sleep(POLL_EVERY);
        }
    }

    /// Pauses a running VM and waits until it reports Paused.
    pub(super) fn pause_vm(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let client = ChClient::new(&record.api_socket()).with_timeout(POWER_API_TIMEOUT);
        client.pause()?;
        self.wait_state(&client, "Paused")?;
        let _ = writeln!(diag, "vm paused");
        Ok(())
    }

    /// True iff the record's cloud-hypervisor process is alive.
    pub(crate) fn ch_alive(record: &LeaseRecord) -> bool {
        record
            .ch_pid
            .is_some_and(|pid| host::owns(pid, CH_COMM, record.ch_start, record.boot_id.as_deref()))
    }

    /// Notes that a record names pids it cannot verify, so none is signalled.
    fn note_unstamped(record: &LeaseRecord, diag: &mut dyn Write) {
        let unverifiable = (record.ch_pid.is_some() && record.ch_start.is_none())
            || (record.passt_pid.is_some() && record.passt_start.is_none())
            || record.boot_id.is_none();
        if unverifiable && (record.ch_pid.is_some() || record.passt_pid.is_some()) {
            let _ = writeln!(
                diag,
                "lease {}: record has no process start time or boot id; treating it as another boot",
                record.lease_id
            );
        }
    }

    /// True iff the record's passt process is alive and is the one recorded.
    fn passt_alive(record: &LeaseRecord) -> bool {
        record.passt_pid.is_some_and(|pid| {
            host::owns(
                pid,
                PASST_COMM,
                record.passt_start,
                record.boot_id.as_deref(),
            )
        })
    }

    /// Resumes a paused VM, or cold boots a dead one, then waits for ssh.
    pub(super) fn resume_vm(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let current = self.store.read(&record.lease_id)?;
        let record = current.as_ref().unwrap_or(record);
        let changed = self.bring_up(record, diag)?;
        let now = self.store.read(&record.lease_id)?;
        let now = now.as_ref().unwrap_or(record);
        self.ensure_watcher(now, diag)?;
        if changed {
            self.wait_ready(now, diag)?;
        }
        Ok(())
    }

    /// Resumes or cold boots; false means the VM already ran and nothing changed.
    ///
    /// A live cloud-hypervisor is never cold booted, even when it does not answer.
    pub(super) fn bring_up(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<bool, String> {
        if !Self::ch_alive(record) {
            self.cold_boot(record, diag)?;
            return Ok(true);
        }
        let client = ChClient::new(&record.api_socket()).with_timeout(POWER_API_TIMEOUT);
        match client.state().as_deref() {
            Ok("Paused") => {
                client.resume()?;
                self.wait_state(&client, "Running")?;
                let _ = writeln!(diag, "vm resumed");
                Ok(true)
            }
            Ok(_) => Ok(false),
            Err(e) => Err(format!(
                "lease {}: cloud-hypervisor is alive but not responding: {e}",
                record.lease_id
            )),
        }
    }

    /// Boots a dead VM again from its existing disk, seed and host key.
    pub(super) fn cold_boot(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        if Self::ch_alive(record) {
            return Err(format!(
                "lease {}: refusing to cold boot while cloud-hypervisor is alive",
                record.lease_id
            ));
        }
        Self::reap_orphan_passt(record, diag);
        let mut record = record.clone();
        remove_tree(&record.run_dir)?;
        mkdir_private(&record.run_dir)?;
        record.clear_pids();
        self.store.write(&record)?;
        let firmware = spec::firmware_cache_path(&self.config);
        let started = self
            .start_processes(&mut record, diag)
            .and_then(|()| self.boot_vm(&record, &firmware, diag))
            .and_then(|()| self.start_watcher(&mut record, diag));
        if started.is_err() {
            Self::stop_processes(&record, diag);
        }
        started
    }

    /// Kills a passt left behind by a dead cloud-hypervisor and removes the
    /// lease's stale sockets. Never touches a lease that never had a CH pid.
    pub(super) fn reap_orphan_passt(record: &LeaseRecord, diag: &mut dyn Write) {
        if record.ch_pid.is_none() || Self::ch_alive(record) {
            return;
        }
        Self::note_unstamped(record, diag);
        for name in ["a", "p", "p.repair", "v"] {
            let _ = fs::remove_file(record.run_dir.join(name));
        }
        if let Some(pid) = record.passt_pid.filter(|_| Self::passt_alive(record)) {
            terminate(
                pid,
                || Self::passt_alive(record),
                Duration::from_secs(2),
                diag,
            );
            let _ = writeln!(diag, "reaped orphan passt {pid} for {}", record.lease_id);
        }
    }
}

#[cfg(test)]
mod tests;
