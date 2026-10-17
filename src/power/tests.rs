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
fn pause_running_pauses_backend() {
    let fake = Fake::default();
    fake.add("p1", true);
    assert_eq!(run_pause(&fake, "p1").unwrap(), "paused p1");
    assert!(fake.called("pause p1"));
}

#[test]
fn pause_paused_is_idempotent() {
    let fake = Fake::default();
    fake.add("p1", true);
    fake.pause_set("p1");
    assert_eq!(run_pause(&fake, "p1").unwrap(), "p1 already paused");
    assert!(!fake.called("pause p1"));
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
