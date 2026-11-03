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
fn clone_disk_of_a_read_only_base_is_a_private_writable_copy() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    let dest = dir.path().join("dest");
    fs::write(&base, b"gen").unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o444)).unwrap();
    clone_disk(&base, &dest, 1 << 20).unwrap();
    assert_eq!(
        fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&base).unwrap().permissions().mode() & 0o777,
        0o444
    );
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
    let pid = spawn_detached(
        &Launch::plain(Path::new("sleep"), &[OsString::from("30")]),
        &log,
    )
    .unwrap();
    assert!(wait_alive(pid));
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = stat.rsplit_once(')').unwrap().1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After comm: state ppid pgrp session ...
    assert_eq!(fields.get(3).copied(), Some(pid.to_string().as_str()));
    assert!(!pid_alive(pid, "nomatch"));
    kill(pid, Signal::KILL).unwrap();
    assert!(wait_gone(pid, "sleep", Duration::from_secs(5)));
    assert!(!pid_alive(pid, "sleep"));
}

#[test]
fn pid_alive_false_for_missing_pid() {
    assert!(!pid_alive(u32::MAX, "sleep"));
}

#[test]
fn kill_missing_pid_is_ok() {
    kill(i32::MAX.unsigned_abs(), Signal::TERM).unwrap();
}

#[test]
fn kill_refuses_a_pid_that_is_not_one_process() {
    for pid in [0, u32::MAX - 1] {
        let err = kill(pid, Signal::TERM).unwrap_err();
        assert!(err.contains("not a process id"), "{err}");
    }
}

#[test]
fn wait_gone_times_out_for_live_process() {
    assert!(!wait_gone(
        std::process::id(),
        "",
        Duration::from_millis(250)
    ));
}

#[test]
fn free_bytes_matches_df_for_the_same_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let ours = free_bytes(dir.path()).unwrap();
    let out = Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(dir.path())
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let df: u64 = text.lines().last().unwrap().trim().parse().unwrap();
    let slack = 64 * 1024 * 1024;
    assert!(ours.abs_diff(df) < slack, "stat {ours} vs df {df}");
    assert!(free_bytes(Path::new("/no/such/dir")).is_err());
}

#[test]
fn dig_holes_frees_zero_blocks_and_keeps_content() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mem");
    let mut data = vec![0u8; 64 << 20];
    for at in [0usize, 7 << 20, 40 << 20, (64 << 20) - 4096] {
        data.get_mut(at..at + 4096).unwrap().fill(0xab);
    }
    fs::write(&file, &data).unwrap();
    dig_holes(Path::new(DIG_HOLES_TOOL), &file).unwrap();
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.len(), 64 << 20);
    assert!(
        meta.blocks() * 512 < 1 << 20,
        "{} bytes",
        meta.blocks() * 512
    );
    assert!(fs::read(&file).unwrap() == data, "content changed");
}

#[test]
fn dig_holes_reports_a_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("mem");
    fs::write(&file, b"x").unwrap();
    let err = dig_holes(Path::new("/no/such/fallocate"), &file).unwrap_err();
    assert!(err.contains("cannot run fallocate"), "{err}");
}

#[test]
fn vm_rss_bytes_reads_status_line() {
    let status = "Name:\tcloud-hyperviso\nVmHWM:\t 1200000 kB\nVmRSS:\t 1148928 kB\nThreads:\t8\n";
    assert_eq!(vm_rss_bytes(status), Some(1_148_928 * 1024));
    assert_eq!(vm_rss_bytes("Name:\tx\n"), None);
}

#[test]
fn resident_bytes_of_this_process_is_nonzero() {
    assert!(resident_bytes(std::process::id()).is_some_and(|b| b > 0));
    assert_eq!(resident_bytes(u32::MAX), None);
}
