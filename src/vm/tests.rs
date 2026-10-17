//! Unit tests for the VM backend; no KVM and no network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use super::spec::{self, mac_for, shell_quote};
use super::RealBackend;
use crate::backend::Backend;
use crate::config::{Asset, Config};
use crate::protocol::Desired;
use crate::state::LeaseRecord;

/// A config rooted in `root` that points every tool at `/bin/false`.
fn config(root: &Path) -> Config {
    let tool = PathBuf::from("/bin/false");
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
        ready_timeout: Duration::from_secs(1),
    }
}

/// A record for lease `id` with its run dir in `/r/<id>`.
fn record(id: &str) -> LeaseRecord {
    LeaseRecord {
        lease_id: id.to_owned(),
        slug: "slug".to_owned(),
        name: "name".to_owned(),
        cloud_id: format!("ch-{id}"),
        created_at: 1,
        ch_pid: None,
        passt_pid: None,
        run_dir: PathBuf::from("/r").join(id),
    }
}

#[test]
fn vm_config_has_the_spike_shape() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let rec = record("abc");
    let got = spec::vm_config(
        &cfg,
        Path::new("/fw.fd"),
        Path::new("/d.raw"),
        Path::new("/s.img"),
        Path::new("/serial.log"),
        &rec,
    );
    let want = json!({
        "cpus": {"boot_vcpus": 2, "max_vcpus": 2},
        "memory": {"size": 4_294_967_296_u64, "shared": true},
        "payload": {"firmware": "/fw.fd"},
        "disks": [
            {"path": "/d.raw", "image_type": "Raw", "readonly": false},
            {"path": "/s.img", "image_type": "Raw", "readonly": true},
        ],
        "net": [{
            "vhost_user": true,
            "vhost_socket": "/r/abc/passt.sock",
            "mac": mac_for("abc"),
            "num_queues": 2,
        }],
        "vsock": {"cid": 3, "socket": "/r/abc/vsock.sock"},
        "serial": {"mode": "File", "file": "/serial.log"},
        "console": {"mode": "Off"},
    });
    assert_eq!(got, want);
}

#[test]
fn mac_is_deterministic_and_locally_administered() {
    let mac = mac_for("lease-1");
    assert_eq!(mac, mac_for("lease-1"));
    assert_ne!(mac, mac_for("lease-2"));
    assert!(mac.starts_with("52:54:00:"));
    let parts: Vec<&str> = mac.split(':').collect();
    assert_eq!(parts.len(), 6);
    assert!(parts
        .iter()
        .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit())));
}

#[test]
fn shell_quote_cases() {
    assert_eq!(
        shell_quote("/usr/bin/x-1.2_3:=@%+"),
        "/usr/bin/x-1.2_3:=@%+"
    );
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
    assert_eq!(shell_quote("$HOME;ls"), "'$HOME;ls'");
}

#[test]
fn proxy_command_quotes_each_argument() {
    let got = spec::proxy_command(Path::new("/bin/cb"), "id-1", Path::new("/my state"));
    assert_eq!(got, "/bin/cb proxy id-1 --state-dir '/my state'");
}

#[test]
fn cache_paths_use_hash_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let cache = &cfg.cache_dir;
    assert_eq!(
        spec::firmware_cache_path(&cfg),
        cache.join("firmware-fedcba987654.fd")
    );
    assert_eq!(
        spec::image_cache_path(&cfg),
        cache.join("image-0123456789ab.qcow2")
    );
    assert_eq!(
        spec::base_raw_path(&cfg),
        cache.join("image-0123456789ab.raw")
    );
}

#[test]
fn create_rejects_overlong_runtime_dir_before_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.runtime_dir = dir.path().join("r".repeat(120));
    let backend = RealBackend::new(cfg.clone(), PathBuf::from("/bin/cb"));
    let desired = Desired {
        lease_id: "lease1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
    };
    let err = backend.create(&desired, &mut Vec::new()).unwrap_err();
    assert!(err.contains("too long"), "{err}");
    assert!(!cfg.runtime_dir.exists());
    assert!(!cfg.state_dir.exists());
    assert!(!cfg.cache_dir.exists());
}

#[test]
fn destroy_missing_lease_is_ok() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/cb"));
    backend.destroy("nope", &mut Vec::new()).unwrap();
    assert!(backend.destroy("bad/id", &mut Vec::new()).is_err());
}

#[test]
fn destroy_orphan_removes_both_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let lease_dir = cfg.state_dir.join("leases").join("orph");
    let run_dir = cfg.runtime_dir.join("orph");
    std::fs::create_dir_all(&lease_dir).unwrap();
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(lease_dir.join("lease.json"), "not json").unwrap();
    std::fs::write(run_dir.join("passt.sock.repair"), "").unwrap();
    let backend = RealBackend::new(cfg, PathBuf::from("/bin/cb"));
    backend.destroy("orph", &mut Vec::new()).unwrap();
    assert!(!lease_dir.exists());
    assert!(!run_dir.exists());
}

#[test]
fn alive_is_false_without_a_live_cloud_hypervisor() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RealBackend::new(config(dir.path()), PathBuf::from("/bin/cb"));
    let mut rec = record("a");
    assert!(!backend.alive(&rec));
    rec.ch_pid = Some(u32::MAX - 1);
    assert!(!backend.alive(&rec));
    rec.ch_pid = Some(std::process::id());
    assert!(!backend.alive(&rec));
}
