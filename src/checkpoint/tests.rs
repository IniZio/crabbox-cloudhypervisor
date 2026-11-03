//! Checkpoint operations against the fake host, through the wire.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use serde_json::{json, Value};

use crate::fake::Fake;
use crate::run_with;

/// Runs `request` and returns the exit code and the parsed stdout.
fn call(fake: &Fake, request: &Value) -> (u8, Value, String) {
    let mut diag = Vec::new();
    let out = run_with(request.to_string().as_bytes(), fake, &mut diag);
    let body = serde_json::from_str(&out.stdout).unwrap();
    (out.exit_code, body, String::from_utf8(diag).unwrap())
}

fn create_request(lease: &str, checkpoint: &str) -> Value {
    json!({"protocolVersion":1,"operation":"checkpointCreate",
        "leaseId":lease,"checkpointId":checkpoint,"name":"after clone"})
}

fn fork_request(lease: &str, checkpoint: &str) -> Value {
    json!({"protocolVersion":1,"operation":"checkpointFork","checkpointId":checkpoint,
        "desired":{"leaseId":lease,"slug":format!("slug-{lease}"),"name":format!("name-{lease}")}})
}

fn delete_request(checkpoint: &str) -> Value {
    json!({"protocolVersion":1,"operation":"checkpointDelete","checkpointId":checkpoint})
}

#[test]
fn create_snapshots_the_running_lease_and_reports_its_parent_and_bytes() {
    let fake = Fake::default();
    fake.add("l1", true);
    let (code, body, _) = call(&fake, &create_request("l1", "ck1"));
    assert_eq!(code, 0, "{body}");
    assert_eq!(
        body,
        json!({"protocolVersion":1,"checkpointId":"ck1","parentCheckpointId":"ck0",
            "bytes":4096,"uniqueBytes":4096,"apparentBytes":8192})
    );
    assert!(fake.called("lock:l1"));
    assert!(fake.called("checkpoint_create l1 ck1 after clone"));
    assert!(
        fake.alive_set.borrow().contains("l1"),
        "the lease keeps running"
    );
}

#[test]
fn create_refuses_a_lease_that_is_not_running() {
    let fake = Fake::default();
    fake.add("paused", true);
    fake.pause_set("paused");
    fake.add("stopped", false);
    fake.add("hung", true);
    fake.unresponsive_set("hung");
    for (lease, want) in [
        ("paused", "lease paused is paused; run crabbox resume"),
        ("stopped", "lease stopped is stopped; run crabbox resume"),
        (
            "hung",
            "lease hung: cloud-hypervisor is alive but not responding",
        ),
        ("nope", "lease nope not found"),
    ] {
        let (code, body, _) = call(&fake, &create_request(lease, "ck1"));
        assert_eq!(code, 1);
        assert_eq!(body["error"], want);
    }
    assert!(!fake
        .calls
        .borrow()
        .iter()
        .any(|c| c.starts_with("checkpoint")));
}

#[test]
fn create_needs_a_lease_id() {
    let fake = Fake::default();
    let request = json!({"protocolVersion":1,"operation":"checkpointCreate","checkpointId":"ck1"});
    let (code, body, _) = call(&fake, &request);
    assert_eq!(code, 1);
    assert_eq!(body["error"], "request names no lease");
}

#[test]
fn a_failed_create_is_an_error_and_the_lease_stays() {
    let fake = Fake::default();
    fake.add("l1", true);
    fake.checkpoint_fails.set(true);
    let (code, body, _) = call(&fake, &create_request("l1", "ck1"));
    assert_eq!(code, 1);
    assert_eq!(body["error"], "checkpoint failed");
    assert!(fake.alive_set.borrow().contains("l1"));
}

#[test]
fn fork_starts_a_new_lease_from_the_checkpoint_and_waits_for_it() {
    let fake = Fake::default();
    fake.add("l1", true);
    assert_eq!(call(&fake, &create_request("l1", "ck1")).0, 0);
    let (code, body, _) = call(&fake, &fork_request("l2", "ck1"));
    assert_eq!(code, 0, "{body}");
    let lease = &body["lease"];
    assert_eq!(lease["leaseId"], "l2");
    assert_eq!(lease["status"], "ready");
    assert_eq!(lease["ssh"]["host"], "l2");
    assert!(fake.called("checkpoint_fork l2 ck1"));
    assert!(fake.called("ready:l2"));
    assert!(
        fake.alive_set.borrow().contains("l1"),
        "the source is untouched"
    );
}

#[test]
fn fork_of_an_unknown_checkpoint_fails_with_checkpoint_not_found() {
    let fake = Fake::default();
    let (code, body, _) = call(&fake, &fork_request("l2", "ghost"));
    assert_eq!(code, 1);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("checkpoint not found"),
        "{body}"
    );
    assert!(fake.records.borrow().is_empty());
}

#[test]
fn a_fork_that_never_gets_ready_is_removed() {
    let fake = Fake::default();
    fake.add("l1", true);
    call(&fake, &create_request("l1", "ck1"));
    fake.ready_fails.set(true);
    let (code, body, _) = call(&fake, &fork_request("l2", "ck1"));
    assert_eq!(code, 1);
    assert_eq!(body["error"], "not ready; vm removed");
    assert!(fake.called("destroy:l2"));
}

#[test]
fn fork_of_a_lease_that_already_runs_returns_it_like_acquire() {
    let fake = Fake::default();
    fake.add("l1", true);
    call(&fake, &create_request("l1", "ck1"));
    let (code, body, _) = call(&fake, &fork_request("l1", "ck1"));
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["lease"]["leaseId"], "l1");
    assert!(!fake.called("checkpoint_fork l1 ck1"));
}

#[test]
fn fork_needs_desired() {
    let fake = Fake::default();
    let request = json!({"protocolVersion":1,"operation":"checkpointFork","checkpointId":"ck1"});
    let (code, body, _) = call(&fake, &request);
    assert_eq!(code, 1);
    assert_eq!(body["error"], "checkpointFork requires desired");
}

#[test]
fn delete_removes_the_checkpoint_and_an_unknown_id_succeeds() {
    let fake = Fake::default();
    fake.add("l1", true);
    call(&fake, &create_request("l1", "ck1"));
    for _ in 0..2 {
        let (code, body, _) = call(&fake, &delete_request("ck1"));
        assert_eq!(code, 0, "{body}");
        assert_eq!(body, json!({"protocolVersion":1}));
    }
    assert!(fake.checkpoints.borrow().is_empty());
    let (code, _, _) = call(&fake, &delete_request("never-existed"));
    assert_eq!(code, 0);
    assert_eq!(call(&fake, &fork_request("l2", "ck1")).0, 1);
}
