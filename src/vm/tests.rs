//! Unit tests for the VM backend; no KVM and no network.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

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
        reap_stopped_after_hours: None,
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
        "balloon": {"size": 0, "deflate_on_oom": true, "free_page_reporting": true},
        "payload": {"firmware": "/fw.fd"},
        "disks": [
            {"path": "/d.raw", "image_type": "Raw", "readonly": false},
            {"path": "/s.img", "image_type": "Raw", "readonly": true},
        ],
        "net": [{
            "vhost_user": true,
            "vhost_socket": "/r/abc/p",
            "mac": mac_for("abc"),
            "num_queues": 2,
        }],
        "vsock": {"cid": 3, "socket": "/r/abc/v"},
        "serial": {"mode": "File", "file": "/serial.log"},
        "console": {"mode": "Off"},
    });
    assert_eq!(got, want);
}

#[test]
fn vm_config_returns_freed_guest_memory_to_the_host() {
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
    assert_eq!(got.pointer("/balloon/size"), Some(&json!(0)));
    assert_eq!(got.pointer("/balloon/deflate_on_oom"), Some(&json!(true)));
    assert_eq!(
        got.pointer("/balloon/free_page_reporting"),
        Some(&json!(true))
    );
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

/// A stand-in process whose comm reads as cloud-hypervisor; killed on drop.
struct FakeCh {
    /// Keeps the renamed binary.
    _dir: tempfile::TempDir,
    /// The running child.
    child: std::process::Child,
}

impl FakeCh {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("cloud-hypervisor");
        std::fs::copy("/bin/sleep", &bin).unwrap();
        let child = std::process::Command::new(&bin).arg("30").spawn().unwrap();
        let ch = Self { _dir: dir, child };
        let pid = ch.child.id();
        for _ in 0..100 {
            if crate::host::pid_alive(pid, super::CH_COMM) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ch
    }
}

impl Drop for FakeCh {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Serves one canned reply per connection on `sock`; yields request lines.
fn serve(sock: &Path, replies: Vec<String>) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{Read, Write};
    let listener = std::os::unix::net::UnixListener::bind(sock).unwrap();
    std::thread::spawn(move || {
        replies
            .into_iter()
            .map(|reply| {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                text.lines().next().unwrap_or_default().to_owned()
            })
            .collect()
    })
}

/// A canned 200 vm.info reply.
fn info(state: &str) -> String {
    let body = format!(r#"{{"state":"{state}"}}"#);
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// A backend plus a record whose run dir holds a marker file.
fn live_fixture(dir: &Path, ch: Option<&FakeCh>) -> (RealBackend, LeaseRecord) {
    let backend = RealBackend::new(config(dir), PathBuf::from("/bin/cb"));
    let mut rec = record("a");
    rec.run_dir = dir.join("r");
    std::fs::create_dir_all(&rec.run_dir).unwrap();
    std::fs::write(rec.run_dir.join("marker"), "").unwrap();
    rec.ch_pid = ch.map(|c| c.child.id());
    (backend, rec)
}

#[test]
fn alive_but_unresponsive_errors_without_cold_boot() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = live_fixture(dir.path(), Some(&ch));
    let err = backend.bring_up(&rec, &mut Vec::new()).unwrap_err();
    assert!(err.contains("lease a"), "{err}");
    assert!(err.contains("alive but not responding"), "{err}");
    assert!(rec.run_dir.join("marker").exists());
    assert!(crate::host::pid_alive(ch.child.id(), super::CH_COMM));
}

#[test]
fn alive_and_paused_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = live_fixture(dir.path(), Some(&ch));
    let ok = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_owned();
    let seen = serve(&rec.api_socket(), vec![info("Paused"), ok, info("Running")]);
    assert!(backend.bring_up(&rec, &mut Vec::new()).unwrap());
    let lines = seen.join().unwrap();
    assert!(lines[1].contains("vm.resume"), "{lines:?}");
    assert!(rec.run_dir.join("marker").exists());
}

#[test]
fn alive_and_running_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = live_fixture(dir.path(), Some(&ch));
    let seen = serve(&rec.api_socket(), vec![info("Running")]);
    assert!(!backend.bring_up(&rec, &mut Vec::new()).unwrap());
    assert_eq!(seen.join().unwrap().len(), 1);
    assert!(rec.run_dir.join("marker").exists());
}

#[test]
fn dead_cloud_hypervisor_cold_boots() {
    let dir = tempfile::tempdir().unwrap();
    let (backend, rec) = live_fixture(dir.path(), None);
    let err = backend.bring_up(&rec, &mut Vec::new()).unwrap_err();
    assert!(!err.contains("alive"), "{err}");
    assert!(!rec.run_dir.join("marker").exists());
}

#[test]
fn cold_boot_refuses_while_alive() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = live_fixture(dir.path(), Some(&ch));
    let err = backend.cold_boot(&rec, &mut Vec::new()).unwrap_err();
    assert!(err.contains("refusing to cold boot"), "{err}");
    assert!(rec.run_dir.join("marker").exists());
    assert!(crate::host::pid_alive(ch.child.id(), super::CH_COMM));
}

#[test]
fn power_of_maps_api_answers_and_failures() {
    use crate::backend::Power;
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (_, rec) = live_fixture(dir.path(), Some(&ch));
    assert_eq!(
        RealBackend::power_of(&rec),
        Power::Unresponsive,
        "no socket"
    );
    let seen = serve(&rec.api_socket(), vec![info("Running")]);
    assert_eq!(RealBackend::power_of(&rec), Power::Running);
    seen.join().unwrap();
    std::fs::remove_file(rec.api_socket()).unwrap();
    let seen = serve(&rec.api_socket(), vec![info("Paused")]);
    assert_eq!(RealBackend::power_of(&rec), Power::Paused);
    seen.join().unwrap();
    std::fs::remove_file(rec.api_socket()).unwrap();
    let _silent = std::os::unix::net::UnixListener::bind(rec.api_socket()).unwrap();
    assert_eq!(RealBackend::power_of(&rec), Power::Unresponsive, "silent");
    let (_, dead) = live_fixture(dir.path(), None);
    assert_eq!(RealBackend::power_of(&dead), Power::Stopped);
}

#[test]
fn destroy_kills_a_stopped_cloud_hypervisor() {
    let dir = tempfile::tempdir().unwrap();
    let ch = FakeCh::spawn();
    let (backend, rec) = live_fixture(dir.path(), Some(&ch));
    backend.store.write(&rec).unwrap();
    let _silent = std::os::unix::net::UnixListener::bind(rec.api_socket()).unwrap();
    let pid = ch.child.id().to_string();
    assert!(std::process::Command::new("kill")
        .args(["-s", "STOP", &pid])
        .status()
        .unwrap()
        .success());
    backend.destroy("a", &mut Vec::new()).unwrap();
    assert!(!crate::host::pid_alive(ch.child.id(), super::CH_COMM));
    assert!(!rec.run_dir.exists());
}

#[test]
fn reap_removes_stale_sockets_only_when_ch_is_dead() {
    let dir = tempfile::tempdir().unwrap();
    let names = ["a", "p", "p.repair", "v"];
    let ch = FakeCh::spawn();
    let (_, mut rec) = live_fixture(dir.path(), Some(&ch));
    for n in names {
        std::fs::write(rec.run_dir.join(n), "").unwrap();
    }
    RealBackend::reap_orphan_passt(&rec, &mut Vec::new());
    assert!(names.iter().all(|n| rec.run_dir.join(n).exists()), "alive");
    rec.ch_pid = Some(u32::MAX);
    RealBackend::reap_orphan_passt(&rec, &mut Vec::new());
    assert!(names.iter().all(|n| !rec.run_dir.join(n).exists()), "dead");
    assert!(rec.run_dir.join("marker").exists());
}
