//! Boot template tests: cache keying, refusal, retargeting and identity
//! rotation, with stand-in ssh and cloud-hypervisor; no KVM.

#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::restore::retarget;
use super::rotate::{base64, script, Identity};
use super::{ensure, Meta, Template, BOOTSTRAP_KEY, KNOWN_HOSTS, META_FILE, REQUIRED};
use crate::protocol::Desired;
use crate::size::Size;
use crate::vm::spec::{self, VmPaths};
use crate::vm::tests::{config, record, FakeCh};
use crate::vm::{Assets, RealBackend};
use crate::volumes::guest::{GuestSetup, SeedFile};

const SIZE: Size = Size {
    cpus: 2,
    memory_mib: 4096,
};
const CH: &str = "cloud-hypervisor v52.0";

/// Inputs for a template of `size` and `disk_gib` under cloud-hypervisor `ch`.
fn meta(size: Size, disk_gib: u64, ch: &str) -> Meta {
    Meta::new("img", "fw", "#cloud-config", size, disk_gib, ch)
}

/// A build that writes every file a usable template has and counts its runs.
fn counting_build(
    runs: &Cell<u32>,
) -> impl FnOnce(&Path, &mut dyn std::io::Write) -> Result<(), String> + '_ {
    move |dir, _| {
        runs.set(runs.get() + 1);
        for f in REQUIRED {
            let path = dir.join(f);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        Ok(())
    }
}

#[test]
fn a_template_is_built_once_per_key_and_reused() {
    let dir = tempfile::tempdir().unwrap();
    let want = meta(SIZE, 20, CH);
    let runs = Cell::new(0);
    let mut diag = Vec::new();
    let first = ensure(dir.path(), &want, &mut diag, counting_build(&runs)).unwrap();
    let second = ensure(dir.path(), &want, &mut diag, counting_build(&runs)).unwrap();
    assert_eq!(runs.get(), 1);
    assert_eq!(first.dir, second.dir);
    let named = dir.path().join(format!("template-{}", want.key12()));
    assert_eq!(first.dir, named);
    let diag = String::from_utf8(diag).unwrap();
    assert_eq!(diag.matches(" built in ").count(), 1, "{diag}");
    let line = format!("template {} built in ", want.key12());
    assert!(diag.starts_with(&line), "{diag}");
}

#[test]
fn a_changed_key_input_builds_a_new_template_and_drops_the_old_one_of_that_size() {
    let dir = tempfile::tempdir().unwrap();
    let runs = Cell::new(0);
    let mut diag = Vec::new();
    let mut build = |m: Meta| ensure(dir.path(), &m, &mut diag, counting_build(&runs)).unwrap();
    let old = build(meta(SIZE, 20, CH));
    let new = build(meta(SIZE, 30, CH));
    assert_ne!(old.dir, new.dir);
    assert!(
        !old.dir.exists(),
        "superseded template of the same size kept"
    );
    let big = Size {
        cpus: 4,
        memory_mib: 8192,
    };
    let other = build(meta(big, 30, CH));
    assert_eq!(runs.get(), 3);
    assert!(
        new.dir.exists() && other.dir.exists(),
        "one template per size"
    );
}

#[test]
fn a_template_from_another_cloud_hypervisor_is_refused_removed_and_not_built() {
    let dir = tempfile::tempdir().unwrap();
    let want = meta(SIZE, 20, CH);
    let runs = Cell::new(0);
    let built = ensure(dir.path(), &want, &mut Vec::new(), counting_build(&runs)).unwrap();
    let mut recorded = want.clone();
    recorded.ch_version = "cloud-hypervisor v51.0".to_owned();
    let json = serde_json::to_vec(&recorded).unwrap();
    fs::write(built.dir.join(META_FILE), json).unwrap();
    let err = ensure(dir.path(), &want, &mut Vec::new(), counting_build(&runs))
        .err()
        .unwrap();
    assert_eq!(
        err,
        "built by cloud-hypervisor v51.0, running cloud-hypervisor v52.0"
    );
    assert_eq!(runs.get(), 1);
    assert!(!built.dir.exists());
}

#[test]
fn a_refused_template_falls_back_to_a_cold_boot() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.boot_template = true;
    cfg.cloud_hypervisor = dir.path().join("cloud-hypervisor");
    crate::test_exec::script(&cfg.cloud_hypervisor, &format!("echo '{CH}'"));
    let backend = RealBackend::new(cfg.clone(), PathBuf::from("/bin/cb"));
    let (seed, _) = backend.template_seed("<bootstrap key>", "<host key>", "<host key>");
    let (image, firmware) = (&cfg.image.sha256, &cfg.firmware.sha256);
    let mut recorded = Meta::new(image, firmware, &seed, SIZE, cfg.disk_gib, CH);
    let planted = cfg.cache_dir.join(format!("template-{}", recorded.key12()));
    fs::create_dir_all(&planted).unwrap();
    recorded.ch_version = "cloud-hypervisor v51.0".to_owned();
    let json = serde_json::to_vec(&recorded).unwrap();
    fs::write(planted.join(META_FILE), json).unwrap();
    let desired = Desired {
        lease_id: "lease1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: None,
        cache_volumes: Vec::new(),
        cache_scope: None,
    };
    let assets = Assets {
        firmware: dir.path().join("fw"),
        base_raw: dir.path().join("base"),
    };
    let mut diag = Vec::new();
    let run_dir = cfg.runtime_dir.join("lease1");
    let err = backend
        .provision_lease(&desired, &run_dir, &assets, SIZE, &mut diag)
        .unwrap_err();
    let diag = String::from_utf8(diag).unwrap();
    let note = "template refused: built by cloud-hypervisor v51.0, \
                running cloud-hypervisor v52.0, cold booting";
    assert!(diag.contains(note), "{diag}");
    assert!(err.starts_with("ssh keygen"), "cold boot path ran: {err}");
    assert!(!planted.exists());
}

/// VM paths under `/<root>`.
fn paths_at(root: &str) -> [PathBuf; 4] {
    ["fw", "disk", "seed", "serial"].map(|n| PathBuf::from(format!("/{root}/{n}")))
}

/// [`VmPaths`] borrowing `p`.
fn vm_paths(p: &[PathBuf; 4]) -> VmPaths<'_> {
    VmPaths {
        firmware: &p[0],
        disk: &p[1],
        seed: &p[2],
        serial: &p[3],
    }
}

#[test]
fn retarget_points_the_template_config_at_the_lease() {
    let (tpl, lease) = (paths_at("tpl"), paths_at("lease"));
    let mut got = spec::vm_config(SIZE, vm_paths(&tpl), &record("tpl-1"));
    let mut want = spec::vm_config(SIZE, vm_paths(&lease), &record("lease1"));
    retarget(&mut got, vm_paths(&lease), &record("lease1")).unwrap();
    want["net"][0]["mac"] = got["net"][0]["mac"].clone();
    assert_eq!(got, want);
    let mut bare = json!({"disks": []});
    let err = retarget(&mut bare, vm_paths(&lease), &record("lease1")).unwrap_err();
    assert!(err.contains("has no /payload/firmware"), "{err}");
}

#[test]
fn base64_matches_known_vectors() {
    for (raw, enc) in [
        ("", ""),
        ("f", "Zg=="),
        ("ab", "YWI="),
        ("foo", "Zm9v"),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(raw.as_bytes()), enc);
    }
}

#[test]
fn the_rotation_script_sets_every_identity_piece_and_the_key_last() {
    let enable = ["systemctl", "enable", "a b"].map(str::to_owned).to_vec();
    let id = Identity {
        epoch: 1_700_000_000,
        entropy: vec![7; 4],
        hostname: "box-1".to_owned(),
        host_private_key: "PRIVATE".to_owned(),
        host_public_key: "ssh-ed25519 HOST".to_owned(),
        lease_public_key: "ssh-ed25519 LEASE".to_owned(),
        guest: GuestSetup {
            files: vec![SeedFile {
                path: "/etc/x y".to_owned(),
                permissions: "0644",
                content: "body".to_owned(),
            }],
            commands: vec![enable],
        },
    };
    let s = script(&id);
    let lease_key = base64(b"ssh-ed25519 LEASE\n");
    for want in [
        "date -s @1700000000".to_owned(),
        format!(
            "put /etc/ssh/ssh_host_ed25519_key 0600 {}",
            base64(b"PRIVATE")
        ),
        "/etc/machine-id".to_owned(),
        "hostnamectl set-hostname box-1".to_owned(),
        format!("put '/etc/x y' 0644 {}", base64(b"body")),
        "systemctl enable 'a b' || echo".to_owned(),
        lease_key.clone(),
    ] {
        assert!(s.contains(&want), "missing {want}:\n{s}");
    }
    assert!(
        !s.contains("PRIVATE") && !s.contains("LEASE"),
        "keys travel encoded"
    );
    let key_at = s.find(&lease_key).unwrap();
    assert!(
        key_at > s.find("systemctl enable").unwrap(),
        "login key changes last"
    );
}

/// A lease with a live stand-in cloud-hypervisor and key files, a template
/// dir, and an ssh script that logs argv and, once `deny` exists, refuses the
/// bootstrap key on `true`.
struct Rotation {
    /// Holds every file.
    root: tempfile::TempDir,
    /// Backend whose ssh is the script.
    backend: RealBackend,
    /// The lease.
    record: crate::state::LeaseRecord,
    /// The template it came from.
    template: Template,
    /// Its cloud-hypervisor.
    _ch: FakeCh,
}

impl Rotation {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut cfg = config(root.path());
        cfg.ssh = root.path().join("ssh");
        let (log, deny) = (root.path().join("ssh.log"), root.path().join("deny"));
        let body = format!(
            "printf '%s' \"$*\" | tr '\\n' '|' >>{0}; echo >>{0}\ncase \"$*\" in *{BOOTSTRAP_KEY}*' true') [ -f {1} ] && \
             {{ echo 'crabbox@x: Permission denied (publickey).' >&2; exit 255; }} ;; esac\nexit 0",
            log.display(),
            deny.display()
        );
        crate::test_exec::script(&cfg.ssh, &body);
        let backend = RealBackend::new(cfg.clone(), PathBuf::from("/bin/cb"));
        let ch = FakeCh::spawn();
        let pid = ch.child.id();
        let mut rec = record("lease1");
        rec.run_dir = cfg.runtime_dir.join("lease1");
        rec.ch_pid = Some(pid);
        rec.ch_start = crate::host::proc_start(pid);
        rec.boot_id = crate::host::boot_id();
        let store = &backend.store;
        store.write(&rec).unwrap();
        fs::write(store.host_key_path("lease1"), "PRIVATE\n").unwrap();
        fs::write(store.host_pub_path("lease1"), "ssh-ed25519 HOST\n").unwrap();
        let lease_pub = store.lease_dir("lease1").join("id_ed25519.pub");
        fs::write(lease_pub, "ssh-ed25519 LEASE\n").unwrap();
        let dir = root.path().join("tpl");
        fs::create_dir_all(&dir).unwrap();
        for f in [BOOTSTRAP_KEY, KNOWN_HOSTS] {
            fs::write(dir.join(f), "x").unwrap();
        }
        let template = Template {
            dir,
            meta: meta(SIZE, 20, CH),
        };
        Self {
            root,
            backend,
            record: rec,
            template,
            _ch: ch,
        }
    }

    /// ssh argv lines in call order.
    fn calls(&self) -> Vec<String> {
        let text = fs::read_to_string(self.root.path().join("ssh.log")).unwrap_or_default();
        text.lines().map(str::to_owned).collect()
    }
}

#[test]
fn rotation_logs_in_with_the_bootstrap_key_then_proves_the_lease_key() {
    let r = Rotation::new();
    fs::write(r.root.path().join("deny"), "").unwrap();
    let mut diag = Vec::new();
    let kept = r
        .backend
        .take_identity(r.record.clone(), &r.template, &mut diag)
        .unwrap();
    assert_eq!(kept.lease_id, "lease1");
    let calls = r.calls();
    let boot = format!("-i {} ", r.template.dir.join(BOOTSTRAP_KEY).display());
    let lease = format!("-i {} ", r.backend.store.key_path("lease1").display());
    let pin = r.backend.store.known_hosts_path("lease1");
    let pin = pin.display().to_string();
    assert_eq!(calls.len(), 4, "{calls:#?}");
    assert!(calls[0].contains(&boot) && calls[0].ends_with("/usr/local/bin/crabbox-ready"));
    assert!(calls[1].contains(&boot) && calls[1].contains("sudo sh -c"));
    assert!(calls[1].contains("date -s @") && calls[1].contains("HostKeyAlias=crabbox-tpl-"));
    assert!(calls[2].contains(&lease) && calls[2].contains(&pin) && calls[2].ends_with(" true"));
    assert!(calls[3].contains(&boot) && calls[3].contains(&pin) && calls[3].ends_with(" true"));
    let diag = String::from_utf8(diag).unwrap();
    let refused = "bootstrap key refused: crabbox@x: Permission denied (publickey).";
    assert!(diag.contains(refused), "{diag}");
    assert!(diag.contains("identity rotated in "), "{diag}");
}

#[test]
fn a_surviving_bootstrap_login_destroys_the_lease() {
    let r = Rotation::new();
    let err = r
        .backend
        .take_identity(r.record.clone(), &r.template, &mut Vec::new())
        .unwrap_err();
    assert_eq!(
        err,
        "bootstrap key still logs in after identity rotation; lease destroyed"
    );
    assert!(r.backend.store.read("lease1").unwrap().is_none());
    let pid = r.record.ch_pid.unwrap();
    assert!(!crate::host::pid_alive(pid, crate::vm::CH_COMM));
}
