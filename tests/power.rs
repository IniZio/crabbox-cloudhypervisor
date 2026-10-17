//! End-to-end pause/resume/dead-VM tests through `run_with`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

#[path = "support/fake_backend.rs"]
mod fake_backend;

use crabbox_cloudhypervisor::run_with;
use fake_backend::FakeBackend;
use serde_json::{json, Value};

const DEAD: &str = "cbx_dead01";
const PAUSED: &str = "cbx_paus01";
const LIVE: &str = "cbx_live01";

/// Runs one request; returns parsed stdout and exit code.
fn call(backend: &FakeBackend, request: &Value) -> (Value, u8) {
    let outcome = run_with(
        request.to_string().as_bytes(),
        backend,
        &mut std::io::sink(),
    );
    (
        serde_json::from_str(&outcome.stdout).unwrap(),
        outcome.exit_code,
    )
}

/// A request for `operation` with extra top-level fields merged in.
fn req(operation: &str, extra: &Value) -> Value {
    let mut base = json!({"protocolVersion": 1, "operation": operation});
    for (k, v) in extra.as_object().unwrap() {
        base[k] = v.clone();
    }
    base
}

/// A request naming lease `id` through `lease`.
fn on_lease(operation: &str, id: &str) -> Value {
    req(operation, &json!({"lease": {"leaseId": id}}))
}

/// Request shaped exactly as the crabbox fork sends pause/resume.
fn fork_req(operation: &str, id: &str) -> Value {
    json!({
        "protocolVersion": 1,
        "operation": operation,
        "config": {},
        "lease": {
            "leaseId": id, "slug": "s", "name": "n", "cloudId": format!("ch-{id}"),
            "status": "ready", "serverType": "x", "labels": {}
        }
    })
}

/// Resolve with `releaseOnly` set.
fn resolve_release_only(id: &str) -> Value {
    req("resolve", &json!({"id": id, "releaseOnly": true}))
}

/// Backend holding one dead, one paused and one running lease.
fn fixture() -> FakeBackend {
    let backend = FakeBackend::default();
    backend.add(DEAD, false);
    backend.add(PAUSED, true);
    backend.paused.borrow_mut().insert(PAUSED.to_owned());
    backend.add(LIVE, true);
    for (record, _) in backend.records.borrow_mut().values_mut() {
        "s".clone_into(&mut record.slug);
        "n".clone_into(&mut record.name);
    }
    backend
}

#[test]
fn resolve_dead_errors_with_resume_hint() {
    let backend = fixture();
    let (out, code) = call(&backend, &req("resolve", &json!({"id": DEAD})));
    assert_eq!(code, 1);
    assert_eq!(
        out,
        json!({"error": format!("lease {DEAD} is stopped; run crabbox resume")})
    );
}

#[test]
fn resolve_paused_errors_with_resume_hint() {
    let backend = fixture();
    let (out, code) = call(&backend, &req("resolve", &json!({"id": PAUSED})));
    assert_eq!(code, 1);
    assert_eq!(
        out,
        json!({"error": format!("lease {PAUSED} is paused; run crabbox resume")})
    );
}

#[test]
fn resolve_release_only_succeeds_with_status() {
    let backend = fixture();
    for (id, status) in [(DEAD, "stopped"), (PAUSED, "paused"), (LIVE, "ready")] {
        let (out, code) = call(&backend, &resolve_release_only(id));
        assert_eq!(code, 0, "{id}");
        assert_eq!(out["lease"]["status"], status, "{id}");
    }
}

#[test]
fn release_force_succeeds_on_dead_and_paused() {
    let backend = fixture();
    for id in [DEAD, PAUSED] {
        let (_, code) = call(&backend, &resolve_release_only(id));
        assert_eq!(code, 0, "{id}");
        let (out, code) = call(&backend, &req("release", &json!({"id": id, "force": true})));
        assert_eq!(code, 0, "{id}: {out}");
        assert!(!backend.has(id), "{id} not destroyed");
    }
}

#[test]
fn pause_running_then_idempotent() {
    let backend = fixture();
    let (out, code) = call(&backend, &fork_req("pause", LIVE));
    assert_eq!(code, 0, "{out}");
    assert!(out["message"].is_string(), "{out}");
    assert!(backend.paused.borrow().contains(LIVE));
    let (out, code) = call(&backend, &fork_req("pause", LIVE));
    assert_eq!(code, 0, "{out}");
    assert!(out["message"].is_string(), "{out}");
}

#[test]
fn resume_paused_and_dead_return_ready_lease() {
    let backend = fixture();
    for id in [PAUSED, DEAD] {
        let (out, code) = call(&backend, &fork_req("resume", id));
        assert_eq!(code, 0, "{id}: {out}");
        assert_eq!(out["lease"]["status"], "ready", "{id}");
        assert_eq!(
            out["lease"]["ssh"]["hostKey"],
            format!("ssh-ed25519 AAAAFAKE{id}"),
            "{id}"
        );
        assert!(backend.resumed.borrow().contains(&id.to_owned()));
    }
}

#[test]
fn resume_running_skips_backend_resume() {
    let backend = fixture();
    let (out, code) = call(&backend, &fork_req("resume", LIVE));
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["lease"]["status"], "ready");
    assert!(backend.resumed.borrow().is_empty());
}

#[test]
fn resume_failure_is_an_error() {
    let backend = fixture();
    backend.resume_fails.set(true);
    let (out, code) = call(&backend, &fork_req("resume", DEAD));
    assert_eq!(code, 1, "{out}");
    assert!(out["error"].is_string());
}

#[test]
fn acquire_and_resolve_include_host_key() {
    let backend = FakeBackend::default();
    let acquire = req(
        "acquire",
        &json!({"desired": {"leaseId": "cbx_abc123", "slug": "s", "name": "n"}}),
    );
    let (out, code) = call(&backend, &acquire);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["lease"]["ssh"]["hostKey"],
        "ssh-ed25519 AAAAFAKEcbx_abc123"
    );
    let (out, code) = call(&backend, &on_lease("resolve", "cbx_abc123"));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        out["lease"]["ssh"]["hostKey"],
        "ssh-ed25519 AAAAFAKEcbx_abc123"
    );
}

#[test]
fn list_shows_each_power_state() {
    let backend = fixture();
    let (out, code) = call(&backend, &req("list", &json!({})));
    assert_eq!(code, 0, "{out}");
    let status_of = |id: &str| {
        out["leases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["leaseId"] == id)
            .map(|l| l["status"].clone())
    };
    assert_eq!(status_of(LIVE), Some(json!("ready")));
    assert_eq!(status_of(PAUSED), Some(json!("paused")));
    assert_eq!(status_of(DEAD), Some(json!("stopped")));
}
