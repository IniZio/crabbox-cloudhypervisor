use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

fn record(id: &str) -> LeaseRecord {
    LeaseRecord {
        lease_id: id.to_owned(),
        slug: "slug".to_owned(),
        name: "name".to_owned(),
        cloud_id: format!("ch-{id}"),
        created_at: 7,
        ch_pid: Some(1),
        passt_pid: None,
        run_dir: PathBuf::from("/run/x"),
        boot_id: None,
        ch_start: None,
        passt_start: None,
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn write_read_roundtrip_mode_0600() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    let rec = record("cbx_000000000001");
    store.write(&rec).unwrap();
    assert_eq!(store.read(&rec.lease_id).unwrap(), Some(rec.clone()));
    let dir = store.lease_dir(&rec.lease_id);
    assert_eq!(mode(&dir.join("lease.json")), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(rec.api_socket(), PathBuf::from("/run/x/a"));
}

#[test]
fn read_missing_is_none_and_garbage_is_err() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    assert_eq!(store.read("nope").unwrap(), None);
    fs::create_dir_all(store.lease_dir("bad")).unwrap();
    assert_eq!(store.read("bad").unwrap(), None);
    fs::write(store.lease_dir("bad").join("lease.json"), "{").unwrap();
    assert!(store.read("bad").is_err());
}

#[test]
fn list_sorted_and_skips_junk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    assert!(store.list().unwrap().is_empty());
    store.write(&record("b")).unwrap();
    store.write(&record("a")).unwrap();
    fs::create_dir_all(store.lease_dir("empty")).unwrap();
    drop(store.lock("a").unwrap());
    let ids: Vec<_> = store
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.lease_id)
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn orphans_lists_missing_and_bad_records() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    store.write(&record("good")).unwrap();
    fs::create_dir_all(store.lease_dir("z_empty")).unwrap();
    fs::create_dir_all(store.lease_dir("m_bad")).unwrap();
    fs::write(store.lease_dir("m_bad").join("lease.json"), "x").unwrap();
    drop(store.lock("good").unwrap());
    assert_eq!(store.orphans().unwrap(), ["m_bad", "z_empty"]);
}

#[test]
fn remove_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    store.write(&record("a")).unwrap();
    store.remove("a").unwrap();
    store.remove("a").unwrap();
    assert_eq!(store.read("a").unwrap(), None);
    assert!(store.remove("../x").is_err());
}

#[test]
fn lock_blocks_second_locker() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::new(tmp.path());
    let first = store.lock("a").unwrap();
    assert!(first.is_held());
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _second = store.lock("a").unwrap();
        tx.send(()).unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    drop(first);
    assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    handle.join().unwrap();
    assert!(!LeaseLock::none().is_held());
}

#[test]
fn lease_id_validation() {
    assert!(validate_lease_id("cbx_000000000001").is_ok());
    assert!(validate_lease_id(&"a".repeat(48)).is_ok());
    for bad in ["../x", "", "a/b", ".x"] {
        assert!(validate_lease_id(bad).is_err(), "{bad}");
    }
    assert!(validate_lease_id(&"a".repeat(49)).is_err());
    assert!(now_unix() > 0);
}

#[test]
fn run_dir_name_is_12_hex_and_deterministic() {
    let name = run_dir_name("cbx_000000000001");
    assert_eq!(name.len(), RUN_DIR_NAME_LEN);
    assert!(name
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    assert_eq!(name, run_dir_name("cbx_000000000001"));
    assert_ne!(name, run_dir_name("cbx_000000000002"));
}

#[test]
fn host_key_paths_and_short_sockets() {
    let store = Store::new(Path::new("/s"));
    let dir = store.lease_dir("x");
    assert_eq!(store.host_key_path("x"), dir.join("ssh_host_ed25519_key"));
    assert_eq!(
        store.host_pub_path("x"),
        dir.join("ssh_host_ed25519_key.pub")
    );
    assert_eq!(store.known_hosts_path("x"), dir.join("known_hosts"));
    let rec = record("x");
    assert_eq!(rec.vsock_socket(), PathBuf::from("/run/x/v"));
    assert_eq!(rec.passt_socket(), PathBuf::from("/run/x/p"));
    assert_eq!(LONGEST_SOCKET_NAME, "p.repair");
}
