//! Tests for the readiness probe.

use super::{known_hosts_line, ReadyProbe};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Probe whose ssh is the given script.
fn probe(ssh: &Path) -> ReadyProbe {
    ReadyProbe {
        ssh: ssh.to_path_buf(),
        key: "/k/id".into(),
        proxy_command: "pc %h".into(),
        user: "crabbox".into(),
        host: "ch-x".into(),
        known_hosts: "/k/known_hosts".into(),
        host_key_alias: "crabbox-x".into(),
    }
}

/// Writes an executable shell script.
fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Waits with a fixed 50ms interval and an always-alive VM.
fn run(p: &ReadyProbe, timeout: Duration) -> Result<(), String> {
    p.wait(
        timeout,
        Duration::from_millis(50),
        &mut || true,
        &mut Vec::new(),
    )
}

#[test]
fn args_exact() {
    let args = probe(Path::new("ssh")).args("true");
    let expected: Vec<String> = [
        "-F",
        "/dev/null",
        "-i",
        "/k/id",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "UpdateHostKeys=no",
        "-o",
        "VerifyHostKeyDNS=no",
        "-o",
        "LogLevel=ERROR",
        "-o",
        "UserKnownHostsFile=/k/known_hosts",
        "-o",
        "HostKeyAlias=crabbox-x",
        "-o",
        "ProxyCommand=pc %h",
        "-p",
        "22",
        "crabbox@ch-x",
        "true",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(args, expected);
}

#[test]
fn succeeds_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = script(dir.path(), "ssh", "exit 0");
    assert_eq!(run(&probe(&ssh), Duration::from_secs(5)), Ok(()));
}

#[test]
fn failure_reports_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = script(dir.path(), "ssh", "echo boom >&2; exit 255");
    let error = run(&probe(&ssh), Duration::from_millis(400)).unwrap_err();
    assert!(error.contains("not ready"), "{error}");
    assert!(error.contains("boom"), "{error}");
}

#[test]
fn succeeds_on_third_call() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("count");
    let body = format!(
        "echo x >> '{0}'; [ \"$(wc -l < '{0}')\" -ge 3 ]",
        counter.display()
    );
    let ssh = script(dir.path(), "ssh", &body);
    assert_eq!(run(&probe(&ssh), Duration::from_secs(10)), Ok(()));
}

#[test]
fn dead_vm_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = script(dir.path(), "ssh", "exit 255");
    let error = probe(&ssh)
        .wait(
            Duration::from_secs(5),
            Duration::from_millis(50),
            &mut || false,
            &mut Vec::new(),
        )
        .unwrap_err();
    assert!(error.contains("exited"), "{error}");
}

#[test]
fn hanging_attempt_respects_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = script(dir.path(), "ssh", "exec sleep 60");
    let start = Instant::now();
    let error = run(&probe(&ssh), Duration::from_secs(1)).unwrap_err();
    assert!(error.contains("not ready"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn known_hosts_line_format() {
    assert_eq!(
        known_hosts_line("crabbox-x", "ssh-ed25519 AAAA"),
        "crabbox-x ssh-ed25519 AAAA\n"
    );
}
