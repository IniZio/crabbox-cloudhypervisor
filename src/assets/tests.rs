//! Tests for cached asset handling.

use super::*;
use std::os::unix::fs::PermissionsExt;

const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn curl() -> Option<PathBuf> {
    Command::new("curl")
        .arg("--version")
        .output()
        .ok()
        .map(|_| PathBuf::from("curl"))
}

fn url(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[test]
fn sha256_known_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a");
    fs::write(&f, b"abc").unwrap();
    assert_eq!(sha256_file(&f).unwrap(), ABC);
    assert!(sha256_file(&dir.path().join("none")).is_err());
}

#[test]
fn cache_state_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a");
    assert_eq!(cache_state(&f, ABC), CacheState::Missing);
    fs::write(&f, b"abc").unwrap();
    assert_eq!(cache_state(&f, ABC), CacheState::Present);
    fs::write(with_suffix(&f, ".sha256"), format!("{ABC}\n")).unwrap();
    assert_eq!(cache_state(&f, ABC), CacheState::Verified);
    assert_eq!(cache_state(&f, "other"), CacheState::Present);
}

#[test]
fn download_match_then_noop() {
    let Some(curl) = curl() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::write(&src, b"abc").unwrap();
    let dest = dir.path().join("cache/out");
    let mut diag = Vec::new();
    ensure_download(&curl, &url(&src), ABC, &dest, &mut diag).unwrap();
    assert_eq!(cache_state(&dest, ABC), CacheState::Verified);
    assert!(!diag.is_empty());
    fs::remove_file(&src).unwrap();
    ensure_download(&curl, &url(&src), ABC, &dest, &mut diag).unwrap();
}

#[test]
fn download_mismatch_leaves_nothing() {
    let Some(curl) = curl() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::write(&src, b"xyz").unwrap();
    let dest = dir.path().join("out");
    let err = ensure_download(&curl, &url(&src), ABC, &dest, &mut Vec::new()).unwrap_err();
    assert!(
        err.starts_with("checksum mismatch for out: expected "),
        "{err}"
    );
    assert!(!err.contains(dir.path().to_str().unwrap()));
    assert!(!dest.exists());
    assert!(!with_suffix(&dest, ".part").exists());
}

#[test]
fn download_failure_reports_exit_code() {
    let Some(curl) = curl() else { return };
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let dest = dir.path().join("out");
    let err = ensure_download(&curl, &url(&missing), ABC, &dest, &mut Vec::new()).unwrap_err();
    assert!(err.contains("curl exit"), "{err}");
    assert!(!err.contains("file://"));
}

#[test]
fn present_without_marker_is_adopted_or_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out");
    fs::write(&dest, b"abc").unwrap();
    // Unused curl path: adoption must not download.
    ensure_download(Path::new("/nonexistent"), "x", ABC, &dest, &mut Vec::new()).unwrap();
    assert_eq!(cache_state(&dest, ABC), CacheState::Verified);
    let bad = dir.path().join("bad");
    fs::write(&bad, b"zzz").unwrap();
    assert!(ensure_download(Path::new("/nonexistent"), "x", ABC, &bad, &mut Vec::new()).is_err());
    assert!(!bad.exists());
}

/// Writes a fake qemu-img script into `dir`.
fn fake_qemu(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("qemu-img");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn raw_converts_once() {
    let dir = tempfile::tempdir().unwrap();
    let qemu = fake_qemu(dir.path(), "cp \"$4\" \"$5\"");
    let src = dir.path().join("src");
    fs::write(&src, b"data").unwrap();
    let dest = dir.path().join("sub/raw");
    ensure_raw(&qemu, &src, &dest, &mut Vec::new()).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), b"data");
    assert!(!with_suffix(&dest, ".part").exists());
    fs::remove_file(&src).unwrap();
    ensure_raw(&qemu, &src, &dest, &mut Vec::new()).unwrap();
}

#[test]
fn raw_failure_includes_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let qemu = fake_qemu(dir.path(), "echo boom >&2; exit 1");
    let dest = dir.path().join("raw");
    let err = ensure_raw(&qemu, &dir.path().join("s"), &dest, &mut Vec::new()).unwrap_err();
    assert!(err.contains("boom"), "{err}");
    assert!(!dest.exists());
}
