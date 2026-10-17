use super::*;
use crate::fake::{lease_request, request, with_expected, Fake};
use crate::protocol::{Expected, Operation};
use crate::state::now_unix;
use serde_json::json;

fn run_cleanup(fake: &Fake, dry_run: bool) -> Result<String, String> {
    run_cleanup_with(fake, dry_run, serde_json::Value::Null)
}

fn run_cleanup_with(
    fake: &Fake,
    dry_run: bool,
    config: serde_json::Value,
) -> Result<String, String> {
    let mut req = request(Operation::Cleanup);
    req.dry_run = dry_run;
    req.config = config;
    cleanup(&req, fake, &mut Vec::new())
}

fn run_release(fake: &Fake, req: &Request) -> Result<(), String> {
    release(req, fake, &mut Vec::new())
}

#[test]
fn list_sorted_with_status() {
    let fake = Fake::default();
    fake.add("c3", false);
    fake.add("a1", true);
    fake.add("b2", true);
    fake.pause_set("b2");
    fake.passt_set("c3");
    let rows = list(&request(Operation::List), &fake, &mut std::io::sink()).unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.lease_id.as_str()).collect();
    assert_eq!(ids, ["a1", "b2", "c3"]);
    let statuses: Vec<&str> = rows.iter().map(|r| r.status.as_str()).collect();
    assert_eq!(statuses, ["ready", "paused", "stopped"]);
    assert!(fake.called("reap c3"));
    assert_eq!(rows.first().unwrap().cloud_id, "ch-a1");
}

#[test]
fn list_empty() {
    let fake = Fake::default();
    assert!(list(&request(Operation::List), &fake, &mut std::io::sink())
        .unwrap()
        .is_empty());
}

#[test]
fn release_missing_is_ok() {
    let fake = Fake::default();
    run_release(&fake, &lease_request(Operation::Release, "a1")).unwrap();
    assert!(!fake.destroyed_any());
}

#[test]
fn release_mismatch_errors_without_destroy() {
    let fake = Fake::default();
    fake.add("a1", true);
    let expected = Expected {
        cloud_id: Some("ch-other".to_owned()),
        ..Expected::default()
    };
    let req = with_expected(lease_request(Operation::Release, "a1"), expected);
    let err = run_release(&fake, &req).unwrap_err();
    assert!(err.contains("expected.cloudId"), "{err}");
    assert!(!fake.destroyed_any());
}

#[test]
fn release_destroys() {
    let fake = Fake::default();
    fake.add("a1", true);
    run_release(&fake, &lease_request(Operation::Release, "a1")).unwrap();
    assert!(fake.called("destroy:a1"));
    assert!(fake.records.borrow().is_empty());
}

#[test]
fn cleanup_dry_run_lists_without_destroying() {
    let fake = Fake::default();
    fake.add("a1", true);
    fake.add("b2", false);
    fake.orphan_ids.borrow_mut().push("o3".to_owned());
    let cfg = json!({"reapStoppedAfterHours": 1});
    let msg = run_cleanup_with(&fake, true, cfg).unwrap();
    assert_eq!(msg, "would remove 2 lease(s): b2, o3");
    assert!(!fake.destroyed_any());
}

#[test]
fn cleanup_keeps_stopped_and_paused_removes_broken() {
    let fake = Fake::default();
    fake.add("a1", true);
    fake.add("p2", true);
    fake.pause_set("p2");
    fake.add("s3", false);
    fake.passt_set("s3");
    fake.orphan_ids.borrow_mut().push("o4".to_owned());
    let msg = run_cleanup(&fake, false).unwrap();
    assert_eq!(msg, "removed 1 lease(s): o4");
    assert!(fake.called("reap s3"));
    assert!(fake.called("destroy:o4"));
    let records = fake.records.borrow();
    assert!(["a1", "p2", "s3"]
        .iter()
        .all(|id| records.contains_key(*id)));
}

#[test]
fn cleanup_reap_key_removes_only_old_stopped() {
    let fake = Fake::default();
    fake.add("old", false);
    fake.add("new", false);
    fake.add("run", true);
    fake.records.borrow_mut().get_mut("new").unwrap().created_at = now_unix();
    let cfg = json!({"reapStoppedAfterHours": 1});
    let msg = run_cleanup_with(&fake, false, cfg).unwrap();
    assert_eq!(msg, "removed 1 lease(s): old");
    let records = fake.records.borrow();
    assert!(records.contains_key("new") && records.contains_key("run"));
}

#[test]
fn cleanup_rejects_bad_reap_key() {
    let fake = Fake::default();
    let err = run_cleanup_with(&fake, false, json!({"reapStoppedAfterHours": 0})).unwrap_err();
    assert!(err.contains("config.reapStoppedAfterHours"), "{err}");
}

#[test]
fn cleanup_nothing_to_clean() {
    let fake = Fake::default();
    fake.add("a1", true);
    assert_eq!(run_cleanup(&fake, false).unwrap(), "nothing to clean up");
    assert_eq!(run_cleanup(&fake, true).unwrap(), "nothing to clean up");
}

#[test]
fn cleanup_destroy_failure_continues_then_errors() {
    let fake = Fake::default();
    fake.add("b2", false);
    fake.add("c3", false);
    fake.destroy_fails.borrow_mut().insert("b2".to_owned());
    let cfg = json!({"reapStoppedAfterHours": 1});
    let err = run_cleanup_with(&fake, false, cfg).unwrap_err();
    assert!(err.contains("b2: destroy failed"), "{err}");
    assert!(fake.called("destroy:c3"));
    assert!(!fake.records.borrow().contains_key("c3"));
}
