//! Binary-level tests with temp directories; no KVM or network needed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail loudly on setup errors"
)]

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};
use tempfile::TempDir;

/// Path of the binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_crabbox-cloudhypervisor");

/// Runs the binary in protocol mode; returns parsed stdout and exit code.
fn call(dir: &TempDir, operation: &str, extra: &Value) -> (Value, i32) {
    let root = dir.path();
    let mut request = json!({
        "protocolVersion": 1,
        "operation": operation,
        "config": {
            "stateDir": root.join("state"),
            "runtimeDir": root.join("run"),
            "cacheDir": root.join("cache"),
        },
    });
    for (k, v) in extra.as_object().unwrap() {
        if k == "config" {
            for (ck, cv) in v.as_object().unwrap() {
                request["config"][ck] = cv.clone();
            }
        } else {
            request[k] = v.clone();
        }
    }
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        serde_json::from_slice(&out.stdout).unwrap(),
        out.status.code().unwrap(),
    )
}

#[test]
fn list_on_empty_state() {
    let dir = TempDir::new().unwrap();
    let (out, code) = call(&dir, "list", &json!({}));
    assert_eq!(code, 0);
    assert_eq!(out, json!({"protocolVersion": 1, "leases": []}));
}

#[test]
fn release_missing_is_ok() {
    let dir = TempDir::new().unwrap();
    let (out, code) = call(
        &dir,
        "release",
        &json!({"lease": {"leaseId": "cbx_missing"}}),
    );
    assert_eq!((out, code), (json!({"protocolVersion": 1}), 0));
}

#[test]
fn touch_is_ok() {
    let dir = TempDir::new().unwrap();
    let (out, code) = call(&dir, "touch", &json!({"lease": {"leaseId": "cbx_x"}}));
    assert_eq!((out, code), (json!({"protocolVersion": 1}), 0));
}

#[test]
fn resolve_missing_fails() {
    let dir = TempDir::new().unwrap();
    let (out, code) = call(
        &dir,
        "resolve",
        &json!({"lease": {"leaseId": "cbx_missing"}}),
    );
    assert_eq!(code, 1);
    assert_eq!(out["error"], "lease cbx_missing not found");
}

#[test]
fn cleanup_with_nothing() {
    let dir = TempDir::new().unwrap();
    let (out, code) = call(&dir, "cleanup", &json!({}));
    assert_eq!(code, 0);
    assert_eq!(out["message"], "nothing to clean up");
}

#[test]
fn acquire_with_bad_image_leaves_nothing() {
    let dir = TempDir::new().unwrap();
    let extra = json!({
        "config": {"image": {"url": "file:///nonexistent", "sha256": "0".repeat(64)}},
        "desired": {"leaseId": "cbx_1", "slug": "s", "name": "n"},
    });
    let (out, code) = call(&dir, "acquire", &extra);
    assert_eq!(code, 1);
    assert!(out["error"].is_string());
    let leases = dir.path().join("state").join("leases").join("cbx_1");
    assert!(!leases.exists(), "lease dir left behind");
}

#[test]
fn proxy_missing_lease_fails() {
    let dir = TempDir::new().unwrap();
    let status = Command::new(BIN)
        .args(["proxy", "cbx_missing", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn unknown_subcommand_exits_2() {
    let status = Command::new(BIN)
        .arg("frobnicate")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}
