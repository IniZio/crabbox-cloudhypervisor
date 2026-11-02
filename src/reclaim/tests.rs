use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::*;

/// The rendered script with every kernel and runtime path moved under a temp dir.
struct Guest {
    /// Holds the fake `/proc`, `/run` and cgroup files.
    dir: tempfile::TempDir,
}

impl Guest {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["proc", "run", "cgroup"] {
            fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let root = dir.path().display().to_string();
        let body = script(&ReclaimConfig::default())
            .replace("/proc/", &format!("{root}/proc/"))
            .replace("/run/", &format!("{root}/run/"))
            .replace("/sys/fs/cgroup/", &format!("{root}/cgroup/"));
        fs::write(dir.path().join("reclaim.sh"), body).unwrap();
        let guest = Self { dir };
        guest.write("proc/swaps", "Filename Type Size Used Priority\n");
        guest
    }

    fn write(&self, name: &str, body: &str) {
        fs::write(self.dir.path().join(name), body).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// One run with this file cache (KiB) and refault count; returns what was asked to reclaim.
    fn run(&self, file_kib: u64, refaults: u64) -> Option<String> {
        let half = file_kib / 2;
        self.write(
            "proc/meminfo",
            &format!(
                "MemTotal: 4000000 kB\nActive(file): {half} kB\nInactive(file): {} kB\n",
                file_kib - half
            ),
        );
        self.write(
            "proc/vmstat",
            &format!("nr_free_pages 1\nworkingset_refault_file {refaults}\n"),
        );
        let reclaim = self.path("cgroup/memory.reclaim");
        let _ = fs::remove_file(&reclaim);
        let status = Command::new("sh")
            .arg(self.path("reclaim.sh"))
            .status()
            .unwrap();
        assert!(status.success());
        read(&reclaim)
    }
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn body<'a>(setup: &'a GuestSetup, path: &str) -> &'a str {
    &setup
        .files
        .iter()
        .find(|f| f.path == path)
        .expect("seed file present")
        .content
}

const GIB_KIB: u64 = 1024 * 1024;

#[test]
fn the_script_has_every_placeholder_filled() {
    let s = script(&ReclaimConfig {
        floor_mib: 300,
        refault_mib: 7,
        skip_runs: 4,
        ..ReclaimConfig::default()
    });
    assert!(!s.contains('@'), "{s}");
    assert!(s.contains("$((7 * 1024))"));
    assert!(s.contains("skip=4\n"));
    assert!(s.contains("300 * 1024"));
}

#[test]
fn reclaims_file_cache_above_the_floor() {
    let g = Guest::new();
    assert_eq!(g.run(3 * GIB_KIB / 2, 0).as_deref(), Some("1048576K\n"));
    assert_eq!(g.run(GIB_KIB / 4, 0), None);
}

#[test]
fn re_reads_above_the_limit_skip_the_next_runs() {
    let g = Guest::new();
    let mut refaults = 0;
    assert!(g.run(GIB_KIB, refaults).is_some());
    refaults += 4000;
    assert!(g.run(GIB_KIB, refaults).is_some(), "15.6 MiB is under 16");
    refaults += 4097;
    assert_eq!(g.run(GIB_KIB, refaults), None);
    for run in 0..10 {
        assert_eq!(g.run(GIB_KIB, refaults), None, "skipped run {run}");
    }
    assert!(g.run(GIB_KIB, refaults).is_some());
}

#[test]
fn does_nothing_with_swap() {
    let g = Guest::new();
    g.write(
        "proc/swaps",
        "Filename Type Size Used Priority\n/swap.img file 1048572 0 -2\n",
    );
    assert_eq!(g.run(2 * GIB_KIB, 0), None);
}

#[test]
fn adds_the_timer_and_the_page_reporting_order() {
    let mut setup = GuestSetup::default();
    add_to(
        &mut setup,
        &ReclaimConfig {
            interval_secs: 30,
            page_reporting_order: 3,
            ..ReclaimConfig::default()
        },
    );
    let timer = body(&setup, "/etc/systemd/system/crabbox-reclaim.timer");
    assert!(timer.contains("OnUnitActiveSec=30s\n"), "{timer}");
    let service = body(&setup, "/etc/systemd/system/crabbox-reclaim.service");
    assert!(service.contains("ExecStart=/usr/local/sbin/crabbox-reclaim\n"));
    assert_eq!(
        body(&setup, TMPFILES_PATH),
        "w /sys/module/page_reporting/parameters/page_reporting_order - - - - 3\n"
    );
    assert_eq!(
        setup.commands,
        [
            argv(&["systemd-tmpfiles", "--create", TMPFILES_PATH]),
            argv(&["systemctl", "enable", "--now", "crabbox-reclaim.timer"]),
        ]
    );
}
