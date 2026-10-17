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
fn dead_existing_is_destroyed_then_recreated() {
    let fake = Fake::default();
    fake.add("a1", false);
    let lease = run_acquire(&fake, &acquire_req("a1")).unwrap();
    assert_eq!(lease.status, "ready");
    let calls = fake.calls.borrow();
    let destroy = calls.iter().position(|c| c == "destroy:a1").unwrap();
    let create = calls.iter().position(|c| c == "create").unwrap();
    assert!(destroy < create);
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
fn resolve_found_ready_and_stopped() {
    let fake = Fake::default();
    fake.add("a1", true);
    fake.add("b2", false);
    let ready = run_resolve(&fake, &lease_request(Operation::Resolve, "a1")).unwrap();
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.ssh.key, "/keys/a1/id_ed25519");
    let stopped = run_resolve(&fake, &lease_request(Operation::Resolve, "b2")).unwrap();
    assert_eq!(stopped.status, "stopped");
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
fn status_of_maps_alive() {
    assert_eq!(status_of(true), "ready");
    assert_eq!(status_of(false), "stopped");
}
