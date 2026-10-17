//! In-process `Backend` for lease and inventory tests.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;

use crate::backend::{cloud_id, Backend, Power};
use crate::protocol::{Desired, Expected, LeaseRef, Operation, Request};
use crate::state::{LeaseLock, LeaseRecord};

/// Fake host: records, liveness, a call log and failure knobs.
#[derive(Default)]
pub(crate) struct Fake {
    /// Persisted records by lease id.
    pub(crate) records: RefCell<BTreeMap<String, LeaseRecord>>,
    /// Lease ids whose VM is running.
    pub(crate) alive_set: RefCell<BTreeSet<String>>,
    /// Orphaned directory names.
    pub(crate) orphan_ids: RefCell<Vec<String>>,
    /// Backend calls in order.
    pub(crate) calls: RefCell<Vec<String>>,
    /// `create` fails.
    pub(crate) create_fails: Cell<bool>,
    /// `wait_ready` fails.
    pub(crate) ready_fails: Cell<bool>,
    /// Lease ids whose `destroy` fails.
    pub(crate) destroy_fails: RefCell<BTreeSet<String>>,
    /// Alive lease ids whose VM is paused.
    pub(crate) paused: RefCell<BTreeSet<String>>,
    /// Lease ids with a live passt (orphaned when the VM is dead).
    pub(crate) passt_alive: RefCell<BTreeSet<String>>,
    /// `resume` fails.
    pub(crate) resume_fails: Cell<bool>,
}

impl Fake {
    /// Marks an alive lease as paused.
    pub(crate) fn pause_set(&self, id: &str) {
        self.paused.borrow_mut().insert(id.to_owned());
    }

    /// Marks a lease's passt as alive.
    pub(crate) fn passt_set(&self, id: &str) {
        self.passt_alive.borrow_mut().insert(id.to_owned());
    }

    /// Adds a record, running or not.
    pub(crate) fn add(&self, id: &str, alive: bool) {
        self.records.borrow_mut().insert(id.to_owned(), record(id));
        if alive {
            self.alive_set.borrow_mut().insert(id.to_owned());
        }
    }

    /// True when `call` was logged.
    pub(crate) fn called(&self, call: &str) -> bool {
        self.calls.borrow().iter().any(|c| c == call)
    }

    /// True when any destroy was logged.
    pub(crate) fn destroyed_any(&self) -> bool {
        self.calls
            .borrow()
            .iter()
            .any(|c| c.starts_with("destroy:"))
    }
}

/// A record with slug `slug-<id>` and name `name-<id>`.
pub(crate) fn record(id: &str) -> LeaseRecord {
    LeaseRecord {
        lease_id: id.to_owned(),
        slug: format!("slug-{id}"),
        name: format!("name-{id}"),
        cloud_id: cloud_id(id),
        created_at: 1,
        ch_pid: Some(1),
        passt_pid: None,
        run_dir: PathBuf::from("/run/x"),
    }
}

/// A request for `operation` with every optional part unset.
pub(crate) fn request(operation: Operation) -> Request {
    Request {
        operation,
        config: serde_json::Value::Null,
        desired: None,
        expected: None,
        keep: false,
        lease: None,
        dry_run: false,
        release_only: false,
        id: None,
        state: None,
        reclaim: false,
        force: false,
        all: false,
        refresh: false,
    }
}

/// A request naming a lease by `lease.leaseId`.
pub(crate) fn lease_request(operation: Operation, id: &str) -> Request {
    let mut req = request(operation);
    req.lease = Some(LeaseRef {
        lease_id: Some(id.to_owned()),
        ..LeaseRef::default()
    });
    req
}

/// Sets `expected` on a request.
pub(crate) fn with_expected(mut req: Request, expected: Expected) -> Request {
    req.expected = Some(expected);
    req
}

/// A desired lease matching `record(id)`.
pub(crate) fn desired(id: &str) -> Desired {
    Desired {
        lease_id: id.to_owned(),
        slug: format!("slug-{id}"),
        name: format!("name-{id}"),
        class: None,
    }
}

impl Backend for Fake {
    fn find(&self, lease_id: &str) -> Result<Option<LeaseRecord>, String> {
        Ok(self.records.borrow().get(lease_id).cloned())
    }

    fn list(&self) -> Result<Vec<LeaseRecord>, String> {
        Ok(self.records.borrow().values().cloned().collect())
    }

    fn orphans(&self) -> Result<Vec<String>, String> {
        Ok(self.orphan_ids.borrow().clone())
    }

    fn alive(&self, record: &LeaseRecord) -> bool {
        self.alive_set.borrow().contains(&record.lease_id)
    }

    fn lock(&self, lease_id: &str) -> Result<LeaseLock, String> {
        self.calls.borrow_mut().push(format!("lock:{lease_id}"));
        Ok(LeaseLock::none())
    }

    fn create(&self, desired: &Desired, _diag: &mut dyn Write) -> Result<LeaseRecord, String> {
        self.calls.borrow_mut().push("create".to_owned());
        if self.create_fails.get() {
            return Err("create failed".to_owned());
        }
        let mut rec = record(&desired.lease_id);
        rec.slug.clone_from(&desired.slug);
        rec.name.clone_from(&desired.name);
        self.records
            .borrow_mut()
            .insert(rec.lease_id.clone(), rec.clone());
        self.alive_set.borrow_mut().insert(rec.lease_id.clone());
        Ok(rec)
    }

    fn wait_ready(&self, record: &LeaseRecord, _diag: &mut dyn Write) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("ready:{}", record.lease_id));
        if self.ready_fails.get() {
            return Err("not ready".to_owned());
        }
        Ok(())
    }

    fn destroy(&self, lease_id: &str, _diag: &mut dyn Write) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("destroy:{lease_id}"));
        if self.destroy_fails.borrow().contains(lease_id) {
            return Err("destroy failed".to_owned());
        }
        self.records.borrow_mut().remove(lease_id);
        self.alive_set.borrow_mut().remove(lease_id);
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
        self.calls
            .borrow_mut()
            .push(format!("pause {}", record.lease_id));
        self.pause_set(&record.lease_id);
        Ok(())
    }

    fn resume(&self, record: &LeaseRecord, _diag: &mut dyn Write) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("resume {}", record.lease_id));
        if self.resume_fails.get() {
            return Err("resume failed".to_owned());
        }
        self.paused.borrow_mut().remove(&record.lease_id);
        self.alive_set.borrow_mut().insert(record.lease_id.clone());
        Ok(())
    }

    fn host_key(&self, lease_id: &str) -> Result<String, String> {
        if self.records.borrow().contains_key(lease_id) {
            Ok(format!("ssh-ed25519 AAAAFAKE{lease_id}"))
        } else {
            Err(format!("no host key for {lease_id}"))
        }
    }

    fn reap(&self, record: &LeaseRecord, _diag: &mut dyn Write) {
        if !self.alive(record) && self.passt_alive.borrow_mut().remove(&record.lease_id) {
            self.calls
                .borrow_mut()
                .push(format!("reap {}", record.lease_id));
        }
    }
}
