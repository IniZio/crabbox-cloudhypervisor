//! Template build teardown tests, with stand-in cloud-hypervisor and passt
//! scripts; no KVM.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::process::Signal;

use super::super::Meta;
use crate::host;
use crate::size::Size;
use crate::state;
use crate::vm::tests::config;
use crate::vm::{Assets, RealBackend};

const SIZE: Size = Size {
    cpus: 2,
    memory_mib: 4096,
};
/// Root of the build the signal target runs; unset in a normal test run.
const TARGET_ROOT: &str = "CBX_TEST_SIGNAL_TARGET_ROOT";

/// A backend whose cloud-hypervisor and passt are idle scripts, its assets,
/// and the run dir of its template build.
fn fixture(root: &Path) -> (RealBackend, Assets, PathBuf) {
    let mut cfg = config(root);
    cfg.boot_template = true;
    cfg.cloud_hypervisor = root.join("cloud-hypervisor");
    cfg.passt = root.join("passt");
    cfg.ssh_keygen = root.join("ssh-keygen");
    let backend = RealBackend::new(cfg.clone(), root.join("adapter"));
    let (seed, _) = backend.template_seed("<bootstrap key>", "<host key>", "<host key>");
    let (image, firmware) = (&cfg.image.sha256, &cfg.firmware.sha256);
    let ch = "cloud-hypervisor v52.0";
    let want = Meta::new(image, firmware, &seed, SIZE, cfg.disk_gib, ch);
    let run_dir = cfg.runtime_dir.join(state::run_dir_name(&want.guest_id()));
    let assets = Assets {
        firmware: root.join("fw"),
        base_raw: root.join("base"),
    };
    (backend, assets, run_dir)
}

/// Writes the stand-in tools and assets under `root`.
fn write_tools(root: &Path) {
    let idle = "while :; do sleep 1; done";
    let ch =
        format!("[ \"$1\" = --version ] && {{ echo 'cloud-hypervisor v52.0'; exit; }}\n{idle}");
    crate::test_exec::script(&root.join("cloud-hypervisor"), &ch);
    let passt = format!("for a; do sock=$a; done\n: >\"$sock\"\n{idle}");
    crate::test_exec::script(&root.join("passt"), &passt);
    let keygen =
        "for a; do k=$a; done\necho private >\"$k\"\necho 'ssh-ed25519 AAAA c' >\"$k.pub\"";
    crate::test_exec::script(&root.join("ssh-keygen"), keygen);
    crate::test_exec::script(&root.join("adapter"), "shift 3\nexec \"$@\"");
    fs::write(root.join("base"), "disk").unwrap();
    fs::write(root.join("fw"), "fw").unwrap();
}

/// Pids of live processes with an argument inside `dir`.
fn holders(dir: &Path) -> Vec<u32> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let inside = cmdline
            .split(|b| *b == 0)
            .any(|arg| Path::new(&*String::from_utf8_lossy(arg)).starts_with(dir));
        if inside && host::pid_alive(pid, "") {
            pids.push(pid);
        }
    }
    pids
}

/// SIGKILLs each pid still alive when dropped, so a failed test leaks nothing.
struct KillOnDrop(Vec<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for pid in &self.0 {
            let _ = host::kill(*pid, Signal::KILL);
        }
    }
}

/// A stand-in `name` from `root` serving `socket`; killed and reaped on drop.
struct Fake(Child);

impl Fake {
    fn spawn(root: &Path, name: &str, socket: &Path) -> Self {
        let flag = if name == "passt" {
            "-s"
        } else {
            "--api-socket"
        };
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        Self(
            Command::new(root.join(name))
                .arg(flag)
                .arg(socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_rebuild_stops_what_a_crashed_build_left_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_tools(root);
    let (mut backend, assets, run_dir) = fixture(root);
    backend.config.ssh_keygen = PathBuf::from("/bin/false");
    let mut sibling = run_dir.clone().into_os_string();
    sibling.push("x");
    let other = root.join("run").join("other");
    let mut stale = [
        Fake::spawn(root, "cloud-hypervisor", &run_dir.join("a")),
        Fake::spawn(root, "passt", &run_dir.join("p")),
    ];
    let mut kept = [
        Fake::spawn(root, "cloud-hypervisor", &other.join("a")),
        Fake::spawn(root, "passt", &other.join("p")),
        Fake::spawn(root, "cloud-hypervisor", &PathBuf::from(sibling).join("a")),
    ];
    std::thread::sleep(Duration::from_millis(200));
    let err = backend
        .ready_template(&assets, SIZE, &mut Vec::new())
        .err()
        .unwrap();
    assert!(err.contains("ssh keygen"), "build ran: {err}");
    for fake in &mut stale {
        assert!(!fake.running(), "stale process {} kept", fake.0.id());
    }
    for fake in &mut kept {
        assert!(fake.running(), "unrelated process {} stopped", fake.0.id());
    }
}

#[test]
fn a_signal_mid_build_stops_its_vm_and_passt() {
    for signal in ["INT", "TERM"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tools(root);
        let (_, _, run_dir) = fixture(root);
        let mut adapter = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "vm::template::reap::tests::signal_target"])
            .args(["--ignored", "--test-threads=1"])
            .env(TARGET_ROOT, root)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = KillOnDrop(Vec::new());
        while seen.0.len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            seen.0 = holders(&run_dir);
        }
        assert_eq!(seen.0.len(), 2, "build never started its vm and passt");
        let sig = if signal == "INT" {
            Signal::INT
        } else {
            Signal::TERM
        };
        host::kill(adapter.id(), sig).unwrap();
        let status = adapter.wait().unwrap();
        assert!(!status.success(), "SIG{signal}: adapter exited 0");
        let left = holders(&run_dir);
        assert!(left.is_empty(), "SIG{signal} left {left:?} running");
    }
}

/// The adapter that `a_signal_mid_build_stops_its_vm_and_passt` signals:
/// builds a template whose cloud-hypervisor never answers.
#[test]
#[ignore = "run by a_signal_mid_build_stops_its_vm_and_passt"]
fn signal_target() {
    let Some(root) = std::env::var_os(TARGET_ROOT) else {
        return;
    };
    let (backend, assets, _) = fixture(Path::new(&root));
    let _ = backend.ready_template(&assets, SIZE, &mut std::io::stderr());
}
