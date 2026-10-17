use super::*;
use std::fs::File;
use std::io::Write;

#[test]
fn clone_disk_grows_and_preserves() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let dest = dir.path().join("dest");
    File::create(&base).unwrap().write_all(b"hello").unwrap();
    clone_disk(&base, &dest, 1 << 20).unwrap();
    assert_eq!(fs::metadata(&dest).unwrap().len(), 1 << 20);
    assert!(fs::read(&dest).unwrap().starts_with(b"hello"));
}

#[test]
fn clone_disk_never_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let dest = dir.path().join("dest");
    fs::write(&base, vec![1u8; 4096]).unwrap();
    clone_disk(&base, &dest, 10).unwrap();
    assert_eq!(fs::metadata(&dest).unwrap().len(), 4096);
}

#[test]
fn clone_disk_missing_base_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = clone_disk(&dir.path().join("nope"), &dir.path().join("d"), 10).unwrap_err();
    assert!(err.starts_with("disk clone"));
}

#[test]
fn keygen_makes_private_key_and_pubkey() {
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("id");
    fs::write(&key, "stale").unwrap();
    let publine = ssh_keygen(Path::new("ssh-keygen"), &key, "crabbox-test").unwrap();
    assert!(publine.starts_with("ssh-ed25519 "));
    assert!(publine.ends_with("crabbox-test"));
    let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// Waits briefly for the setsid exec to land on the target program.
fn wait_alive(pid: u32) -> bool {
    for _ in 0..50 {
        if pid_alive(pid, "sleep") {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn spawn_detached_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let pid = spawn_detached(Path::new("sleep"), &[OsString::from("30")], &log).unwrap();
    assert!(wait_alive(pid));
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = stat.rsplit_once(')').unwrap().1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After comm: state ppid pgrp session ...
    assert_eq!(fields.get(3).copied(), Some(pid.to_string().as_str()));
    assert!(!pid_alive(pid, "nomatch"));
    kill(pid, "KILL").unwrap();
    assert!(wait_gone(pid, "sleep", Duration::from_secs(5)));
    assert!(!pid_alive(pid, "sleep"));
}

#[test]
fn pid_alive_false_for_missing_pid() {
    assert!(!pid_alive(u32::MAX, "sleep"));
}

#[test]
fn kill_missing_pid_is_ok() {
    kill(u32::MAX - 1, "TERM").unwrap();
}

#[test]
fn wait_gone_times_out_for_live_process() {
    assert!(!wait_gone(
        std::process::id(),
        "",
        Duration::from_millis(250)
    ));
}
