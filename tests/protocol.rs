//! End-to-end tests: spawn the binary, pipe one request, check stdout and exit.

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

/// Runs the binary with `input` on stdin; returns parsed stdout and exit code.
fn call(input: &str) -> (Value, i32) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabbox-cloudhypervisor"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let value = serde_json::from_slice(&out.stdout).unwrap();
    (value, out.status.code().unwrap())
}

#[test]
fn doctor_returns_message() {
    let (out, code) = call(r#"{"protocolVersion":1,"operation":"doctor"}"#);
    assert_eq!(code, 0);
    assert_eq!(out["protocolVersion"], 1);
    assert!(out["message"].is_string());
}

#[test]
fn touch_is_ok() {
    let (out, code) = call(r#"{"protocolVersion":1,"operation":"touch","lease":{"leaseId":"l"}}"#);
    assert_eq!(code, 0);
    assert_eq!(out, json!({"protocolVersion":1}));
}

#[test]
fn bad_version_fails() {
    let (out, code) = call(r#"{"protocolVersion":2,"operation":"doctor"}"#);
    assert_eq!(code, 1);
    assert_eq!(
        out["error"],
        "unsupported protocol version 2; this adapter speaks version 1"
    );
}

#[test]
fn unknown_operation_fails() {
    let (out, code) = call(r#"{"protocolVersion":1,"operation":"frobnicate"}"#);
    assert_eq!(code, 1);
    assert_eq!(out["error"], "unknown operation \"frobnicate\"");
}

#[test]
fn unimplemented_operation_fails() {
    let (out, code) = call(r#"{"protocolVersion":1,"operation":"list"}"#);
    assert_eq!(code, 1);
    assert_eq!(out["error"], "operation list not implemented yet");
}
