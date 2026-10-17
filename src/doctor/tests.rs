//! Doctor tests: devices and tools are temp files or `true`/`false`, never the host's.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{run, run_with, Probes};
use crate::config::{Asset, Config};
use crate::vm::spec::{firmware_cache_path, image_cache_path};

/// A config rooted in `root` whose tools all succeed.
fn config(root: &Path) -> Config {
    let tool = PathBuf::from("/bin/true");
    Config {
        state_dir: root.join("state"),
        runtime_dir: root.join("run"),
        cache_dir: root.join("cache"),
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 20,
        image: Asset {
            url: "https://example.invalid/i.img".to_owned(),
            sha256: "0123456789abcdef0123".to_owned(),
        },
        firmware: Asset {
            url: "https://example.invalid/f.fd".to_owned(),
            sha256: "fedcba9876543210fedc".to_owned(),
        },
        cloud_hypervisor: tool.clone(),
        passt: tool.clone(),
        qemu_img: tool.clone(),
        ssh: tool.clone(),
        ssh_keygen: tool.clone(),
        curl: tool,
        ready_timeout: Duration::from_secs(5),
    }
}

/// Probes where both devices are plain writable files.
fn probes(root: &Path) -> Probes {
    let kvm = root.join("kvm");
    let vsock = root.join("vsock");
    std::fs::write(&kvm, b"").unwrap();
    std::fs::write(&vsock, b"").unwrap();
    Probes {
        kvm,
        vhost_vsock: vsock,
    }
}

/// Runs the doctor and returns (passed, report).
fn report(config: &Config, probes: &Probes) -> (bool, String) {
    match run_with(config, probes, &mut Vec::new()) {
        Ok(r) => (true, r),
        Err(r) => (false, r),
    }
}

#[test]
fn kvm_missing_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = probes(dir.path());
    p.kvm = dir.path().join("absent");
    let (ok, r) = report(&config(dir.path()), &p);
    assert!(!ok);
    assert!(r.contains("FAIL kvm:"), "{r}");
}

#[test]
fn kvm_writable_file_is_ok_and_nothing_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, r) = report(&config(dir.path()), &probes(dir.path()));
    assert!(ok, "{r}");
    assert!(r.contains("ok kvm: readable and writable"), "{r}");
    assert!(r.contains("ok vhost-vsock:"), "{r}");
    assert!(r.contains("ok runtime_dir: writable"), "{r}");
    assert!(r.lines().last().unwrap().ends_with("0 fail"), "{r}");
}

#[test]
fn vsock_missing_only_warns() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = probes(dir.path());
    p.vhost_vsock = dir.path().join("absent");
    let (ok, r) = report(&config(dir.path()), &p);
    assert!(ok, "{r}");
    assert!(r.contains("warn vhost-vsock:"), "{r}");
}

#[test]
fn missing_cloud_hypervisor_fails_without_echoing_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    c.cloud_hypervisor = PathBuf::from("/nonexistent/secret-ch");
    c.passt = PathBuf::from("/bin/false");
    let (ok, r) = report(&c, &probes(dir.path()));
    assert!(!ok);
    assert!(r.contains("FAIL cloud-hypervisor: configured cloud-hypervisor binary not runnable"));
    assert!(r.contains("FAIL passt:"), "{r}");
    assert!(!r.contains("secret-ch"), "{r}");
    assert!(!r.contains("/nonexistent"), "{r}");
}

#[test]
fn missing_cache_warns_then_verified_after_marker() {
    let dir = tempfile::tempdir().unwrap();
    let c = config(dir.path());
    let (_, r) = report(&c, &probes(dir.path()));
    assert!(
        r.contains("warn firmware: missing, will download on first acquire"),
        "{r}"
    );
    assert!(
        r.contains("warn image: missing, will download on first acquire"),
        "{r}"
    );

    std::fs::create_dir_all(&c.cache_dir).unwrap();
    let fw = firmware_cache_path(&c);
    std::fs::write(&fw, b"fw").unwrap();
    let mut marker = fw.into_os_string();
    marker.push(".sha256");
    std::fs::write(marker, &c.firmware.sha256).unwrap();
    std::fs::write(image_cache_path(&c), b"img").unwrap();
    let (_, r) = report(&c, &probes(dir.path()));
    assert!(r.contains("ok firmware: verified"), "{r}");
    assert!(r.contains("warn image: present but unverified"), "{r}");
}

#[test]
fn overlong_runtime_dir_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = config(dir.path());
    c.runtime_dir = dir.path().join("r".repeat(90));
    let (ok, r) = report(&c, &probes(dir.path()));
    assert!(!ok);
    assert!(r.contains("FAIL socket_path:"), "{r}");
    assert!(r.contains("at most 85 bytes"), "{r}");
}

#[test]
fn socket_room_boundary_is_85_bytes() {
    use super::socket_room;
    let ok = socket_room(Path::new(&"r".repeat(85)));
    assert!(ok.detail.contains("107 of 108"), "{}", ok.detail);
    assert!(socket_room(Path::new(&"r".repeat(86)))
        .detail
        .contains("too long"));
}

#[test]
fn failures_and_warnings_go_to_diag() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = probes(dir.path());
    p.kvm = dir.path().join("absent");
    let mut diag = Vec::new();
    let _ = run_with(&config(dir.path()), &p, &mut diag);
    let d = String::from_utf8(diag).unwrap();
    assert!(d.contains("FAIL kvm: cannot open"), "{d}");
    assert!(d.contains("warn firmware:"), "{d}");
    assert!(!d.contains("ok runtime_dir"), "{d}");
    assert!(d.trim_end().ends_with("doctor finished"), "{d}");
}

#[test]
fn summary_counts_ok_lines() {
    let dir = tempfile::tempdir().unwrap();
    let (_, r) = report(&config(dir.path()), &probes(dir.path()));
    let oks = r.lines().filter(|l| l.starts_with("ok ")).count();
    let last = r.lines().last().unwrap();
    assert!(last.starts_with(&format!("doctor: {oks} ok")), "{r}");
}

#[test]
#[ignore = "writes the real host report to $DOCTOR_OUT"]
fn real_host_report() {
    let config =
        Config::from_request(&serde_json::Value::Null, &|k| std::env::var(k).ok()).unwrap();
    let r = run(&config, &mut Vec::new()).unwrap_or_else(|e| e);
    std::fs::write(std::env::var("DOCTOR_OUT").unwrap(), r).unwrap();
}
