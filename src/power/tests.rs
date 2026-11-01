use super::*;
use crate::fake::{lease_request, Fake};
use crate::protocol::Operation;

fn run_pause(fake: &Fake, id: &str) -> Result<String, String> {
    pause(&lease_request(Operation::Pause, id), fake, &mut Vec::new())
}

fn run_resume(fake: &Fake, id: &str) -> Result<Lease, String> {
    resume(&lease_request(Operation::Resume, id), fake, &mut Vec::new())
}

#[test]
fn pause_stopped_is_idempotent() {
    let fake = Fake::default();
    fake.add("p1", false);
    assert_eq!(run_pause(&fake, "p1").unwrap(), "p1 already stopped");
    assert!(!fake.called("pause p1"));
}

#[test]
fn pause_missing_lease_errors() {
    let fake = Fake::default();
    assert_eq!(
        run_pause(&fake, "nope").unwrap_err(),
        "lease nope not found"
    );
}

#[test]
fn resume_paused_and_stopped_return_ready_lease() {
    for paused in [true, false] {
        let fake = Fake::default();
        fake.add("r1", paused);
        if paused {
            fake.pause_set("r1");
        }
        let lease = run_resume(&fake, "r1").unwrap();
        assert!(fake.called("resume r1"));
        assert_eq!(lease.status, "ready");
        assert!(!lease.ssh.host_key.is_empty());
    }
}

#[test]
fn resume_running_skips_backend() {
    let fake = Fake::default();
    fake.add("r1", true);
    let lease = run_resume(&fake, "r1").unwrap();
    assert_eq!(lease.status, "ready");
    assert!(!fake.called("resume r1"));
}

#[test]
fn resume_failure_propagates() {
    let fake = Fake::default();
    fake.add("r1", false);
    fake.resume_fails.set(true);
    assert!(run_resume(&fake, "r1").is_err());
}

#[test]
fn unresponsive_pause_and_resume_error_without_acting() {
    let fake = Fake::default();
    fake.add("u1", true);
    fake.unresponsive_set("u1");
    let want = "lease u1: cloud-hypervisor is alive but not responding";
    assert_eq!(run_pause(&fake, "u1").unwrap_err(), want);
    assert_eq!(run_resume(&fake, "u1").unwrap_err(), want);
    assert!(!fake.called("pause u1"));
    assert!(!fake.called("resume u1"));
}

/// A fake with running lease `s1`.
fn disk_fake() -> Fake {
    let fake = Fake::default();
    fake.add("s1", true);
    fake
}

#[test]
fn disk_pause_suspends_running_and_memory_paused_vms() {
    for paused in [false, true] {
        let fake = disk_fake();
        if paused {
            fake.pause_set("s1");
        }
        assert_eq!(run_pause(&fake, "s1").unwrap(), "suspended s1");
        assert!(fake.called("pause s1"));
        assert_eq!(
            fake.power(&fake.find("s1").unwrap().unwrap()),
            Power::Suspended
        );
    }
}

#[test]
fn pause_suspended_is_idempotent() {
    let fake = disk_fake();
    run_pause(&fake, "s1").unwrap();
    fake.calls.borrow_mut().clear();
    assert_eq!(run_pause(&fake, "s1").unwrap(), "s1 already suspended");
    assert!(!fake.called("pause s1"));
}

#[test]
fn resume_suspended_returns_ready_lease() {
    let fake = disk_fake();
    run_pause(&fake, "s1").unwrap();
    let lease = run_resume(&fake, "s1").unwrap();
    assert!(fake.called("resume s1"));
    assert_eq!(lease.status, "ready");
}

#[test]
fn failed_restore_surfaces_and_stays_suspended() {
    let fake = disk_fake();
    run_pause(&fake, "s1").unwrap();
    fake.resume_fails.set(true);
    assert_eq!(run_resume(&fake, "s1").unwrap_err(), "resume failed");
    assert_eq!(
        fake.power(&fake.find("s1").unwrap().unwrap()),
        Power::Suspended
    );
}

#[test]
fn suspended_lease_resolves_with_status_and_refuses_acquire() {
    let fake = disk_fake();
    run_pause(&fake, "s1").unwrap();
    let req = lease_request(Operation::Resolve, "s1");
    let lease = crate::lease::resolve(&req, &fake, &mut Vec::new()).unwrap();
    assert_eq!(lease.status, "suspended");
    let mut acquire = crate::fake::request(Operation::Acquire);
    acquire.desired = Some(crate::fake::desired("s1"));
    let err = crate::lease::acquire(&acquire, &fake, &mut Vec::new()).unwrap_err();
    assert_eq!(err, "lease s1 is suspended; run crabbox resume");
}

#[test]
fn cleanup_never_deletes_a_suspended_lease() {
    let fake = disk_fake();
    run_pause(&fake, "s1").unwrap();
    let mut req = crate::fake::request(Operation::Cleanup);
    req.config = serde_json::json!({"reapStoppedAfterHours": 1});
    crate::inventory::cleanup(&req, &fake, &mut Vec::new()).unwrap();
    assert!(!fake.destroyed_any());
    assert!(fake.find("s1").unwrap().is_some());
}
