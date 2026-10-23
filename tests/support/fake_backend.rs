//! In-memory `Backend` for the e2e tests (the crate's own fake is `cfg(test)`).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

use crabbox_cloudhypervisor::backend::{cloud_id, Backend, Power};
use crabbox_cloudhypervisor::protocol::Desired;
use crabbox_cloudhypervisor::state::{LeaseLock, LeaseRecord};

/// Fake host: records with liveness and failure knobs.
#[derive(Default)]
pub(crate) struct FakeBackend {
    /// Records by lease id, with whether the VM is running.
    pub records: RefCell<BTreeMap<String, (LeaseRecord, bool)>>,
    /// `wait_ready` fails.
    pub ready_fails: Cell<bool>,
    /// Orphan directory names.
    pub orphan_ids: RefCell<Vec<String>>,
    /// Lease ids whose VM is paused (only meaningful while alive).
    pub paused: RefCell<BTreeSet<String>>,
    /// `resume` fails.
    pub resume_fails: Cell<bool>,
    /// Lease ids whose dead VM left a passt orphan; `reap` clears them.
    pub passt_orphans: RefCell<BTreeSet<String>>,
    /// Lease ids that `resume` was called for.
    pub resumed: RefCell<Vec<String>>,
}

impl FakeBackend {
    /// Adds a record for `id`, running or not.
    pub(crate) fn add(&self, id: &str, alive: bool) {
        let record = LeaseRecord {
            lease_id: id.to_owned(),
            slug: format!("slug-{id}"),
            name: format!("name-{id}"),
            cloud_id: cloud_id(id),
            created_at: 1,
            ch_pid: Some(1),
            passt_pid: None,
            run_dir: PathBuf::from("/run/x"),
            boot_id: None,
            ch_start: None,
            passt_start: None,
        };
        self.records
            .borrow_mut()
            .insert(id.to_owned(), (record, alive));
    }

    /// True when a record for `id` exists.
    pub(crate) fn has(&self, id: &str) -> bool {
        self.records.borrow().contains_key(id)
    }
}

impl Backend for FakeBackend {
    fn find(&self, lease_id: &str) -> Result<Option<LeaseRecord>, String> {
        Ok(self.records.borrow().get(lease_id).map(|(r, _)| r.clone()))
    }

    fn list(&self) -> Result<Vec<LeaseRecord>, String> {
        Ok(self
            .records
            .borrow()
            .values()
            .map(|(r, _)| r.clone())
            .collect())
    }

    fn orphans(&self) -> Result<Vec<String>, String> {
        Ok(self.orphan_ids.borrow().clone())
    }

    fn alive(&self, record: &LeaseRecord) -> bool {
        self.records
            .borrow()
            .get(&record.lease_id)
            .is_some_and(|(_, alive)| *alive)
    }

    fn try_lock(&self, _lease_id: &str) -> Result<Option<LeaseLock>, String> {
        Ok(Some(LeaseLock::none()))
    }

    fn lock(&self, _lease_id: &str) -> Result<LeaseLock, String> {
        Ok(LeaseLock::none())
    }

    fn create(&self, desired: &Desired, _diag: &mut dyn Write) -> Result<LeaseRecord, String> {
        self.add(&desired.lease_id, true);
        let mut records = self.records.borrow_mut();
        let entry = records
            .get_mut(&desired.lease_id)
            .ok_or("fake: record vanished")?;
        entry.0.slug.clone_from(&desired.slug);
        entry.0.name.clone_from(&desired.name);
        Ok(entry.0.clone())
    }

    fn wait_ready(&self, _record: &LeaseRecord, _diag: &mut dyn Write) -> Result<(), String> {
        if self.ready_fails.get() {
            return Err("not ready".to_owned());
        }
        Ok(())
    }

    fn destroy(&self, lease_id: &str, _diag: &mut dyn Write) -> Result<(), String> {
        self.records.borrow_mut().remove(lease_id);
        self.orphan_ids.borrow_mut().retain(|o| o != lease_id);
        Ok(())
    }

    fn ssh_key(&self, lease_id: &str) -> PathBuf {
        PathBuf::from(format!("/keys/{lease_id}/id_ed25519"))
    }

    fn proxy_command(&self, lease_id: &str) -> String {
        format!("proxy {lease_id}")
    }

    fn power(&self, record: &LeaseRecord) -> Power {
        if !self.alive(record) {
            Power::Stopped
        } else if self.paused.borrow().contains(&record.lease_id) {
            Power::Paused
        } else {
            Power::Running
        }
    }

    fn pause(&self, record: &LeaseRecord, _diag: &mut dyn Write) -> Result<(), String> {
        self.paused.borrow_mut().insert(record.lease_id.clone());
        Ok(())
    }

    fn resume(&self, record: &LeaseRecord, _diag: &mut dyn Write) -> Result<(), String> {
        if self.resume_fails.get() {
            return Err("resume failed".to_owned());
        }
        self.resumed.borrow_mut().push(record.lease_id.clone());
        self.paused.borrow_mut().remove(&record.lease_id);
        if let Some(entry) = self.records.borrow_mut().get_mut(&record.lease_id) {
            entry.1 = true;
        }
        Ok(())
    }

    fn host_key(&self, lease_id: &str) -> Result<String, String> {
        Ok(format!("ssh-ed25519 AAAAFAKE{lease_id}"))
    }

    fn reap(&self, record: &LeaseRecord, _diag: &mut dyn Write) {
        if !self.alive(record) {
            self.passt_orphans.borrow_mut().remove(&record.lease_id);
        }
    }
}
