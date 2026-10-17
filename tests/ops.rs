//! End-to-end operation tests through `run_with` and an in-memory backend.

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

/// An acquire request for lease `id`.
fn acquire(id: &str) -> Value {
    req(
        "acquire",
        &json!({"desired": {"leaseId": id, "slug": format!("slug-{id}"), "name": format!("name-{id}")}}),
    )
}

/// A request naming lease `id` through `lease`.
fn on_lease(operation: &str, id: &str) -> Value {
    req(operation, &json!({"lease": {"leaseId": id}}))
}

#[test]
fn acquire_returns_full_lease() {
    let backend = FakeBackend::default();
    let (out, code) = call(&backend, &acquire("cbx_1"));
    assert_eq!(code, 0);
    assert_eq!(out["protocolVersion"], 1);
    let lease = &out["lease"];
    let expected = [
        ("/leaseId", "cbx_1"),
        ("/slug", "slug-cbx_1"),
        ("/name", "name-cbx_1"),
        ("/cloudId", "ch-cbx_1"),
        ("/status", "ready"),
        ("/ssh/user", "crabbox"),
        ("/ssh/host", "cbx_1"),
        ("/ssh/port", "22"),
        ("/ssh/key", "/keys/cbx_1/id_ed25519"),
        ("/ssh/proxyCommand", "proxy cbx_1"),
        ("/ssh/readyCheck", "/usr/local/bin/crabbox-ready"),
    ];
    for (pointer, want) in expected {
        assert_eq!(lease.pointer(pointer), Some(&json!(want)), "{pointer}");
    }
}

#[test]
fn acquire_is_idempotent() {
    let backend = FakeBackend::default();
    let (first, _) = call(&backend, &acquire("cbx_1"));
    let (second, code) = call(&backend, &acquire("cbx_1"));
    assert_eq!(code, 0);
    assert_eq!(first, second);
    assert_eq!(backend.records.borrow().len(), 1);
}

#[test]
fn failed_acquire_rolls_back() {
    let backend = FakeBackend::default();
    backend.ready_fails.set(true);
    let (out, code) = call(&backend, &acquire("cbx_1"));
    assert_eq!(code, 1);
    assert!(out["error"].as_str().unwrap().contains("not ready"));
    assert!(!backend.has("cbx_1"));
}

#[test]
fn resolve_existing_and_missing() {
    let backend = FakeBackend::default();
    backend.add("cbx_1", true);
    let (out, code) = call(&backend, &on_lease("resolve", "cbx_1"));
    assert_eq!(code, 0);
    assert_eq!(out["lease"]["status"], "ready");
    let (out, code) = call(&backend, &on_lease("resolve", "cbx_2"));
    assert_eq!(code, 1);
    assert_eq!(out["error"], "lease cbx_2 not found");
}

#[test]
fn list_empty_and_populated() {
    let backend = FakeBackend::default();
    let (out, code) = call(&backend, &req("list", &json!({})));
    assert_eq!(code, 0);
    assert_eq!(out, json!({"protocolVersion": 1, "leases": []}));
    backend.add("cbx_b", false);
    backend.add("cbx_a", true);
    let (out, _) = call(&backend, &req("list", &json!({})));
    let rows = out["leases"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["leaseId"], "cbx_a");
    assert_eq!(rows[0]["status"], "ready");
    assert_eq!(rows[1]["leaseId"], "cbx_b");
    assert_eq!(rows[1]["status"], "stopped");
}

#[test]
fn release_missing_and_existing() {
    let backend = FakeBackend::default();
    let (out, code) = call(&backend, &on_lease("release", "cbx_1"));
    assert_eq!((out, code), (json!({"protocolVersion": 1}), 0));
    backend.add("cbx_1", true);
    let (out, code) = call(&backend, &on_lease("release", "cbx_1"));
    assert_eq!((out, code), (json!({"protocolVersion": 1}), 0));
    assert!(!backend.has("cbx_1"));
}

#[test]
fn touch_is_ok() {
    let backend = FakeBackend::default();
    let (out, code) = call(&backend, &on_lease("touch", "cbx_1"));
    assert_eq!((out, code), (json!({"protocolVersion": 1}), 0));
}

#[test]
fn cleanup_dry_run_then_real() {
    let backend = FakeBackend::default();
    backend.add("cbx_dead", false);
    backend.add("cbx_live", true);
    let reap = json!({"reapStoppedAfterHours": 1});
    let (out, _) = call(&backend, &req("cleanup", &json!({})));
    assert_eq!(out["message"], "nothing to clean up");
    assert!(backend.has("cbx_dead"));
    let dry = json!({"dryRun": true, "config": reap});
    let (out, _) = call(&backend, &req("cleanup", &dry));
    assert_eq!(out["message"], "would remove 1 lease(s): cbx_dead");
    assert!(backend.has("cbx_dead"));
    let (out, code) = call(&backend, &req("cleanup", &json!({"config": reap})));
    assert_eq!(code, 0);
    assert_eq!(out["message"], "removed 1 lease(s): cbx_dead");
    assert!(!backend.has("cbx_dead"));
    assert!(backend.has("cbx_live"));
    let (out, _) = call(&backend, &req("cleanup", &json!({"config": reap})));
    assert_eq!(out["message"], "nothing to clean up");
}

#[test]
fn doctor_yields_message_or_error() {
    let backend = FakeBackend::default();
    let (out, code) = call(&backend, &req("doctor", &json!({})));
    assert!(out["message"].is_string() || out["error"].is_string());
    assert_eq!(code == 0, out["message"].is_string());
}

#[test]
fn malformed_request_is_error() {
    let backend = FakeBackend::default();
    let outcome = run_with(b"not json", &backend, &mut std::io::sink());
    assert_eq!(outcome.exit_code, 1);
    let out: Value = serde_json::from_str(&outcome.stdout).unwrap();
    assert!(out["error"].is_string());
}

#[test]
fn errors_never_echo_config_values() {
    let backend = FakeBackend::default();
    let secret = "SECRET-VALUE-123";
    let request = req(
        "doctor",
        &json!({"config": {"stateDir": secret, "cpus": secret}}),
    );
    let (out, code) = call(&backend, &request);
    assert_eq!(code, 1);
    assert!(!out["error"].as_str().unwrap().contains(secret));
    let request = req(
        "resolve",
        &json!({"config": {"image": secret}, "lease": {"leaseId": "cbx_9"}}),
    );
    let (out, code) = call(&backend, &request);
    assert_eq!(code, 1);
    assert!(!out.to_string().contains(secret));
}
