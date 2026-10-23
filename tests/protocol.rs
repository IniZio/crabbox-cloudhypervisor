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

/// Runs `doctor` with every directory in `root` and every tool `/bin/true`,
/// except for the `overrides` object merged into the config.
fn doctor(root: &std::path::Path, overrides: &Value) -> (Value, i32) {
    let mut config = json!({
        "stateDir": root.join("state"),
        "runtimeDir": root.join("run"),
        "cacheDir": root.join("cache"),
        "cloudHypervisor": "/bin/true",
        "passt": "/bin/true",
        "qemuImg": "/bin/true",
        "ssh": "/bin/true",
        "sshKeygen": "/bin/true",
        "curl": "/bin/true",
    });
    for (k, v) in overrides.as_object().unwrap() {
        config[k] = v.clone();
    }
    let request = json!({"protocolVersion": 1, "operation": "doctor", "config": config});
    call(&request.to_string())
}

/// The report text and the expected fail count for a doctor response.
fn report_of(out: &Value, code: i32) -> (&str, usize) {
    if code == 0 {
        assert_eq!(out["protocolVersion"], 1);
        (out["message"].as_str().unwrap(), 0)
    } else {
        assert_eq!(code, 1);
        assert!(out.get("message").is_none(), "{out}");
        (out["error"].as_str().unwrap(), 1)
    }
}

/// Only the host's /dev/kvm is outside the request's control, so with healthy
/// tools and directories kvm is the one check allowed to decide the exit code.
#[test]
fn doctor_reports_healthy_config() {
    let root = tempfile::tempdir().unwrap();
    let (out, code) = doctor(root.path(), &json!({}));
    let (report, fails) = report_of(&out, code);
    let failed: Vec<&str> = report.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert_eq!(failed.len(), fails, "{report}");
    assert!(
        failed.iter().all(|l| l.starts_with("FAIL kvm:")),
        "{report}"
    );
    assert!(
        report.ends_with(&format!("{fails} fail")),
        "summary line: {report}"
    );
    for tool in ["cloud-hypervisor", "passt", "qemu-img", "ssh", "curl"] {
        assert!(report.contains(&format!("ok {tool}:")), "{tool}: {report}");
    }
    for dir in ["runtime_dir", "state_dir"] {
        assert!(report.contains(&format!("ok {dir}: writable")), "{report}");
    }
}

#[test]
fn doctor_fails_naming_missing_tool() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("no-such-cloud-hypervisor");
    let (out, code) = doctor(root.path(), &json!({"cloudHypervisor": missing}));
    assert_eq!(code, 1);
    let report = out["error"].as_str().unwrap();
    assert!(
        report.contains("FAIL cloud-hypervisor: configured cloud-hypervisor binary not runnable"),
        "{report}"
    );
    assert!(!report.contains("no-such-cloud-hypervisor"), "{report}");
    assert!(report.contains("ok passt:"), "{report}");
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
