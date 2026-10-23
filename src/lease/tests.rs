use super::*;
use crate::fake::{desired, lease_request, request, with_expected, Fake};
use crate::protocol::{Expected, Operation};

fn acquire_req(id: &str) -> Request {
    let mut req = request(Operation::Acquire);
    req.desired = Some(desired(id));
    req
}

fn run_acquire(fake: &Fake, req: &Request) -> Result<Lease, String> {
    acquire(req, fake, &mut Vec::new())
}

fn run_resolve(fake: &Fake, req: &Request) -> Result<Lease, String> {
    resolve(req, fake, &mut Vec::new())
}

#[test]
fn acquire_creates_and_returns_full_lease() {
    let fake = Fake::default();
    let lease = run_acquire(&fake, &acquire_req("a1")).unwrap();
    assert_eq!(lease.lease_id, "a1");
    assert_eq!(lease.cloud_id, "ch-a1");
    assert_eq!(lease.slug, "slug-a1");
    assert_eq!(lease.name, "name-a1");
    assert_eq!(lease.status, "ready");
    let ssh = Ssh {
        user: "crabbox".to_owned(),
        host: "a1".to_owned(),
        port: "22".to_owned(),
        key: "/keys/a1/id_ed25519".to_owned(),
        proxy_command: "proxy a1".to_owned(),
        ready_check: "/usr/local/bin/crabbox-ready".to_owned(),
        host_key: "ssh-ed25519 AAAAFAKEa1".to_owned(),
    };
    assert_eq!(lease.ssh, ssh);
    assert!(fake.called("lock:a1"));
}

#[test]
fn acquire_is_idempotent() {
    let fake = Fake::default();
    let first = run_acquire(&fake, &acquire_req("a1")).unwrap();
    let second = run_acquire(&fake, &acquire_req("a1")).unwrap();
    assert_eq!(first, second);
    let creates = fake
        .calls
        .borrow()
        .iter()
        .filter(|c| *c == "create")
        .count();
    assert_eq!(creates, 1);
}

#[test]
fn existing_alive_not_ready_is_kept() {
    let fake = Fake::default();
    fake.add("a1", true);
    fake.ready_fails.set(true);
    let err = run_acquire(&fake, &acquire_req("a1")).unwrap_err();
    assert_eq!(err, "not ready");
    assert!(!fake.destroyed_any());
    assert!(fake.records.borrow().contains_key("a1"));
}

#[test]
fn created_not_ready_is_destroyed() {
    let fake = Fake::default();
    fake.ready_fails.set(true);
    let err = run_acquire(&fake, &acquire_req("a1")).unwrap_err();
    assert_eq!(err, "not ready; vm removed");
    assert!(fake.called("destroy:a1"));
}

#[test]
fn created_not_ready_and_destroy_fails_says_so() {
    let fake = Fake::default();
    fake.ready_fails.set(true);
    fake.destroy_fails.borrow_mut().insert("a1".to_owned());
    let err = run_acquire(&fake, &acquire_req("a1")).unwrap_err();
    assert_eq!(err, "not ready; removing vm also failed: destroy failed");
}

#[test]
fn create_failure_propagates() {
    let fake = Fake::default();
    fake.create_fails.set(true);
    let err = run_acquire(&fake, &acquire_req("a1")).unwrap_err();
    assert_eq!(err, "create failed");
    assert!(!fake.destroyed_any());
}

#[test]
fn acquire_existing_stopped_or_paused_errors_without_destroy() {
    let fake = Fake::default();
    fake.add("a1", false);
    let err = run_acquire(&fake, &acquire_req("a1")).unwrap_err();
    assert_eq!(err, "lease a1 is stopped; run crabbox resume");
    fake.add("b2", true);
    fake.pause_set("b2");
    let err = run_acquire(&fake, &acquire_req("b2")).unwrap_err();
    assert_eq!(err, "lease b2 is paused; run crabbox resume");
    assert!(!fake.destroyed_any());
    assert!(!fake.called("create"));
}

#[test]
fn identity_mismatch_errors_without_destroy() {
    let fake = Fake::default();
    fake.add("a1", true);
    let mut req = acquire_req("a1");
    req.desired.as_mut().unwrap().slug = "other".to_owned();
    let err = run_acquire(&fake, &req).unwrap_err();
    assert!(err.contains("desired.slug"), "{err}");
    let mut req = acquire_req("a1");
    req.desired.as_mut().unwrap().name = "other".to_owned();
    assert!(run_acquire(&fake, &req)
        .unwrap_err()
        .contains("desired.name"));
    assert!(!fake.destroyed_any());
    assert!(!fake.called("create"));
}

#[test]
fn acquire_without_desired_errors() {
    let fake = Fake::default();
    let err = run_acquire(&fake, &request(Operation::Acquire)).unwrap_err();
    assert_eq!(err, "acquire requires desired");
}

#[test]
fn resolve_ready_returns_lease() {
    let fake = Fake::default();
    fake.add("a1", true);
    let ready = run_resolve(&fake, &lease_request(Operation::Resolve, "a1")).unwrap();
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.ssh.key, "/keys/a1/id_ed25519");
    assert_eq!(ready.ssh.host_key, "ssh-ed25519 AAAAFAKEa1");
}

#[test]
fn resolve_returns_stopped_and_paused_leases_with_their_status() {
    let fake = Fake::default();
    fake.add("b2", false);
    fake.add("c3", true);
    fake.pause_set("c3");
    for release_only in [false, true] {
        let mut req = lease_request(Operation::Resolve, "b2");
        req.release_only = release_only;
        assert_eq!(run_resolve(&fake, &req).unwrap().status, "stopped");
        let mut req = lease_request(Operation::Resolve, "c3");
        req.release_only = release_only;
        assert_eq!(run_resolve(&fake, &req).unwrap().status, "paused");
    }
    assert!(!fake.destroyed_any());
}

#[test]
fn resolve_reaps_orphan_passt() {
    let fake = Fake::default();
    fake.add("b2", false);
    fake.passt_set("b2");
    let mut req = lease_request(Operation::Resolve, "b2");
    req.release_only = true;
    run_resolve(&fake, &req).unwrap();
    assert!(fake.called("reap b2"));
}

#[test]
fn resolve_missing_errors_and_never_creates() {
    let fake = Fake::default();
    let err = run_resolve(&fake, &lease_request(Operation::Resolve, "zz")).unwrap_err();
    assert_eq!(err, "lease zz not found");
    assert!(!fake.called("create"));
    assert!(!fake.destroyed_any());
}

#[test]
fn resolve_by_expected_cloud_id() {
    let fake = Fake::default();
    fake.add("a1", true);
    let expected = Expected {
        cloud_id: Some("ch-a1".to_owned()),
        ..Expected::default()
    };
    let req = with_expected(request(Operation::Resolve), expected);
    assert_eq!(run_resolve(&fake, &req).unwrap().lease_id, "a1");
}

#[test]
fn resolve_expected_mismatch_errors() {
    let fake = Fake::default();
    fake.add("a1", true);
    let expected = Expected {
        slug: Some("wrong".to_owned()),
        ..Expected::default()
    };
    let req = with_expected(lease_request(Operation::Resolve, "a1"), expected);
    let err = run_resolve(&fake, &req).unwrap_err();
    assert!(err.contains("expected.slug"), "{err}");
}

#[test]
fn no_target_and_bad_id_error() {
    let fake = Fake::default();
    let err = run_resolve(&fake, &request(Operation::Resolve)).unwrap_err();
    assert_eq!(err, "request names no lease");
    assert!(run_resolve(&fake, &lease_request(Operation::Resolve, "a/b")).is_err());
}

#[test]
fn unresponsive_errors_on_acquire_and_resolve_but_not_release_only() {
    let fake = Fake::default();
    fake.add("u1", true);
    fake.unresponsive_set("u1");
    let want = "lease u1: cloud-hypervisor is alive but not responding";
    assert_eq!(run_acquire(&fake, &acquire_req("u1")).unwrap_err(), want);
    let req = lease_request(Operation::Resolve, "u1");
    assert_eq!(run_resolve(&fake, &req).unwrap_err(), want);
    let mut req = req;
    req.release_only = true;
    assert_eq!(run_resolve(&fake, &req).unwrap().status, "unresponsive");
    assert!(!fake.destroyed_any());
}
