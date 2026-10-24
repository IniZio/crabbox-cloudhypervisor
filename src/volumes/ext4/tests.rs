use super::*;
use crate::volumes::tests::have_e2fsprogs;

#[test]
fn parses_the_e2fsck_summary_line() {
    let line = "a.img: 11/1310720 files (0.0% non-contiguous), 126322/5242880 blocks";
    assert_eq!(
        parse_usage(line),
        Some(Usage {
            used: 126_322,
            total: 5_242_880
        })
    );
    assert_eq!(
        parse_usage("Pass 5: Checking group summary information"),
        None
    );
    assert_eq!(parse_usage("x: 1/2 files, a/2 blocks"), None);
}

#[test]
fn usage_full_at_the_percent_boundary() {
    assert!(Usage {
        used: 95,
        total: 100
    }
    .at_least(95));
    assert!(!Usage {
        used: 94,
        total: 100
    }
    .at_least(95));
}

#[test]
fn make_is_sparse_owned_by_the_login_user_and_checks_clean() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("g.img");
    make(&img, 4 << 30).unwrap();
    assert_eq!(std::fs::metadata(&img).unwrap().len(), 4 << 30);
    assert!(allocated(&img).unwrap() < 64 << 20, "sparse");
    let usage = check(&img).unwrap();
    assert_eq!(usage.total, (4 << 30) / 4096);
    assert!(!needs_recovery(&img).unwrap());
    let stat = Command::new("debugfs")
        .args(["-R", "stat /"])
        .arg(&img)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&stat.stdout);
    assert!(text.contains(&format!("User:  {LOGIN_UID}")), "{text}");
    assert!(text.contains(&format!("Group:  {LOGIN_GID}")), "{text}");
}

#[test]
fn grow_extends_and_never_shrinks() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("g.img");
    make(&img, 1 << 30).unwrap();
    grow(&img, 2 << 30).unwrap();
    assert_eq!(check(&img).unwrap().total, (2 << 30) / 4096);
    grow(&img, 1 << 30).unwrap();
    assert_eq!(std::fs::metadata(&img).unwrap().len(), 2 << 30);
}

#[test]
fn needs_recovery_reads_the_superblock_flag() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("g.img");
    make(&img, 1 << 30).unwrap();
    let out = Command::new("debugfs")
        .args(["-w", "-R", "feature needs_recovery"])
        .arg(&img)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(needs_recovery(&img).unwrap());
}

#[test]
fn has_file_tells_a_missing_file_from_an_unreadable_image() {
    if !have_e2fsprogs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("a.img");
    make(&image, 64 * 1024 * 1024).unwrap();
    assert_eq!(has_file(&image, "/a/.crabbox-sealed"), Ok(false));
    remove_file(&image, "/a/.crabbox-sealed").unwrap();
    let junk = dir.path().join("junk.img");
    std::fs::write(&junk, vec![0u8; 8192]).unwrap();
    assert!(has_file(&junk, "/a").is_err());
    assert!(remove_file(&junk, "/a").is_err());
}
