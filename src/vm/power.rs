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

/// Sends TERM, then KILL if the process outlives `grace`.
fn terminate(pid: u32, comm: &str, grace: Duration, diag: &mut dyn Write) {
    if !host::pid_alive(pid, comm) {
        return;
    }
    if let Err(e) = host::kill(pid, "TERM") {
        let _ = writeln!(diag, "warn: {e}");
    }
    if !host::wait_gone(pid, comm, grace) {
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
    fn stop_via_api(record: &LeaseRecord, diag: &mut dyn Write) {
        let client = ChClient::new(&record.api_socket()).with_timeout(STOP_API_TIMEOUT);
        let steps = [
            ("vm.shutdown", client.shutdown()),
            ("vm.delete", client.delete()),
            ("vmm.shutdown", client.vmm_shutdown()),
        ];
        for (name, result) in steps {
            if let Err(e) = result {
                let _ = writeln!(diag, "warn: {name}: {e}");
            }
        }
    }

    /// Stops cloud-hypervisor and passt for a record.
    pub(super) fn stop_processes(record: &LeaseRecord, diag: &mut dyn Write) {
        if let Some(pid) = record.ch_pid {
            if record.api_socket().exists() && host::pid_alive(pid, CH_COMM) {
                Self::stop_via_api(record, diag);
                let _ = host::wait_gone(pid, CH_COMM, Duration::from_secs(5));
            }
            terminate(pid, CH_COMM, Duration::from_secs(2), diag);
        }
        if let Some(pid) = record.passt_pid {
            terminate(pid, PASST_COMM, Duration::from_secs(2), diag);
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
        let alive = record
            .ch_pid
            .is_some_and(|pid| host::pid_alive(pid, CH_COMM));
        if !alive {
            return Power::Stopped;
        }
        let client = ChClient::new(&record.api_socket()).with_timeout(POWER_API_TIMEOUT);
        match client.state().as_deref() {
            Ok("Paused") => Power::Paused,
            _ => Power::Running,
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

    /// Resumes a paused VM, or cold boots a stopped one, then waits for ssh.
    pub(super) fn resume_vm(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let current = self.store.read(&record.lease_id)?;
        let record = current.as_ref().unwrap_or(record);
        if Self::power_of(record) == Power::Paused {
            let client = ChClient::new(&record.api_socket()).with_timeout(POWER_API_TIMEOUT);
            client.resume()?;
            self.wait_state(&client, "Running")?;
            let _ = writeln!(diag, "vm resumed");
        } else {
            self.cold_boot(record, diag)?;
        }
        let booted = self.store.read(&record.lease_id)?;
        self.wait_ready(booted.as_ref().unwrap_or(record), diag)
    }

    /// Boots a dead VM again from its existing disk, seed and host key.
    pub(super) fn cold_boot(
        &self,
        record: &LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        Self::reap_orphan_passt(record, diag);
        let mut record = record.clone();
        remove_tree(&record.run_dir)?;
        mkdir_private(&record.run_dir)?;
        record.ch_pid = None;
        record.passt_pid = None;
        self.store.write(&record)?;
        let firmware = spec::firmware_cache_path(&self.config);
        let started = self
            .start_processes(&mut record, diag)
            .and_then(|()| self.boot_vm(&record, &firmware, diag));
        if started.is_err() {
            Self::stop_processes(&record, diag);
        }
        started
    }

    /// Kills a passt left behind by a dead cloud-hypervisor.
    pub(super) fn reap_orphan_passt(record: &LeaseRecord, diag: &mut dyn Write) {
        let ch_alive = record
            .ch_pid
            .is_some_and(|pid| host::pid_alive(pid, CH_COMM));
        if ch_alive {
            return;
        }
        if let Some(pid) = record.passt_pid.filter(|p| host::pid_alive(*p, PASST_COMM)) {
            terminate(pid, PASST_COMM, Duration::from_secs(2), diag);
            let _ = writeln!(diag, "reaped orphan passt {pid} for {}", record.lease_id);
        }
    }
}
