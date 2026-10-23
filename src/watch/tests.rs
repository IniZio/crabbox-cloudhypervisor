#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use super::*;
use crate::grow::GIB;

/// Growth limits for a 10 GiB, 2 vCPU, 2 GiB guest.
const GROWTH: Growth = Growth {
    cpus_max: 4,
    memory_mib_max: 4096,
    disk_max_gib: 30,
    disk_step_gib: 10,
    disk_free_percent: 15,
    disk_free_gib: 4,
};

/// Output of `SAMPLE_SCRIPT` captured from a live noble guest.
const LIVE_SAMPLE: &str = "11362635776 9346433024
cpu  1802 0 1306 87043 336 0 49 0 0 0
MemTotal:        2010632 kB
MemAvailable:    1678940 kB
2
";

/// A scripted VMM and guest that records every call.
struct Fake {
    /// Current sizes.
    current: RefCell<Current>,
    /// Samples returned in order; the last one repeats.
    samples: RefCell<Vec<Sample>>,
    /// Error for `grow_disk`, if any.
    disk_error: Option<String>,
    /// Results for `grow_fs`, in order; the last one repeats.
    fs_results: RefCell<Vec<Result<u64, String>>>,
    /// Error for `resize`, if any.
    resize_error: Option<String>,
    /// Online CPUs a sample reports after a vCPU raise.
    onlined: Option<u32>,
    /// Calls made, as text.
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn new(sample: Sample) -> Self {
        Self {
            current: RefCell::new(Current {
                disk_bytes: 10 * GIB,
                vcpus: 2,
                memory_bytes: 2048 * MIB,
            }),
            samples: RefCell::new(vec![sample]),
            disk_error: None,
            fs_results: RefCell::new(vec![Ok(9 * GIB)]),
            resize_error: None,
            onlined: None,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Machine for Fake {
    fn current(&self) -> Result<Current, String> {
        Ok(*self.current.borrow())
    }

    fn sample(&self) -> Result<Sample, String> {
        let mut samples = self.samples.borrow_mut();
        let mut s = if samples.len() > 1 {
            samples.remove(0)
        } else {
            samples[0]
        };
        if let Some(n) = self.onlined {
            s.online_cpus = n.min(self.current.borrow().vcpus);
        }
        Ok(s)
    }

    fn grow_disk(&self, bytes: u64) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("grow_disk {}", bytes / GIB));
        if let Some(e) = &self.disk_error {
            return Err(e.clone());
        }
        self.current.borrow_mut().disk_bytes = bytes;
        Ok(())
    }

    fn grow_fs(&self) -> Result<u64, String> {
        self.calls.borrow_mut().push("grow_fs".to_owned());
        let mut results = self.fs_results.borrow_mut();
        if results.len() > 1 {
            results.remove(0)
        } else {
            results[0].clone()
        }
    }

    fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("resize {vcpus:?} {:?}", ram_bytes.map(|b| b / MIB)));
        if let Some(e) = &self.resize_error {
            return Err(e.clone());
        }
        let mut c = self.current.borrow_mut();
        if let Some(n) = vcpus {
            c.vcpus = n;
        }
        if let Some(n) = ram_bytes {
            c.memory_bytes = n;
        }
        Ok(())
    }
}

fn calm() -> Sample {
    parse_sample(LIVE_SAMPLE).unwrap()
}

fn low_space() -> Sample {
    Sample {
        fs_avail: GIB,
        ..calm()
    }
}

/// Runs `n` ticks; returns the log.
fn ticks(planner: &mut Planner, fake: &Fake, n: usize) -> String {
    let mut log = Vec::new();
    for _ in 0..n {
        tick(planner, fake, Duration::ZERO, &mut log).unwrap();
    }
    String::from_utf8(log).unwrap()
}

#[test]
fn parses_a_live_guest_sample() {
    let s = parse_sample(LIVE_SAMPLE).unwrap();
    assert_eq!(s.fs_size, 11_362_635_776);
    assert_eq!(s.fs_avail, 9_346_433_024);
    assert_eq!(s.cpu.total, 1802 + 1306 + 87043 + 336 + 49);
    assert_eq!(s.cpu.busy, 1802 + 1306 + 49);
    assert_eq!(s.mem_total, 2_010_632 * 1024);
    assert_eq!(s.mem_avail, 1_678_940 * 1024);
    assert_eq!(s.online_cpus, 2);
}

#[test]
fn a_garbled_sample_names_what_is_missing() {
    let err = parse_sample("guest not ready\n").unwrap_err();
    assert!(err.contains("root filesystem size"), "{err}");
    let no_mem = LIVE_SAMPLE.replace("MemAvailable", "MemFree");
    assert!(parse_sample(&no_mem).unwrap_err().contains("MemAvailable"));
}

#[test]
fn low_space_grows_the_disk_then_the_filesystem_and_logs_both() {
    let fake = Fake::new(low_space());
    let mut p = Planner::new(GROWTH);
    let log = ticks(&mut p, &fake, 2);
    assert_eq!(fake.calls(), ["grow_fs", "grow_disk 20", "grow_fs"]);
    assert!(log.contains("grew disk 10.0 GiB -> 20.0 GiB"), "{log}");
    assert!(
        log.contains("root filesystem fills the disk: 9.0 GiB"),
        "{log}"
    );
}

#[test]
fn a_failed_disk_grow_is_logged() {
    let fake = Fake {
        disk_error: Some("HTTP 500: no space".to_owned()),
        ..Fake::new(low_space())
    };
    let log = ticks(&mut Planner::new(GROWTH), &fake, 2);
    assert!(
        log.contains("grow failed: disk 10.0 GiB -> 20.0 GiB: HTTP 500: no space"),
        "{log}"
    );
}

#[test]
fn a_failed_filesystem_grow_is_logged_and_retried_without_growing_the_disk() {
    let fake = Fake::new(low_space());
    *fake.fs_results.borrow_mut() =
        vec![Ok(9 * GIB), Err("growpart: boom".to_owned()), Ok(19 * GIB)];
    let log = ticks(&mut Planner::new(GROWTH), &fake, 3);
    assert_eq!(
        fake.calls(),
        ["grow_fs", "grow_disk 20", "grow_fs", "grow_fs"]
    );
    assert!(
        log.contains("grow failed: root filesystem: growpart: boom"),
        "{log}"
    );
}

#[test]
fn a_full_disk_at_its_cap_is_logged_once() {
    let fake = Fake::new(low_space());
    fake.current.borrow_mut().disk_bytes = 30 * GIB;
    let log = ticks(&mut Planner::new(GROWTH), &fake, 4);
    assert_eq!(
        log.matches("disk at diskMaxGiB (30.0 GiB)").count(),
        1,
        "{log}"
    );
    assert!(!fake.calls().iter().any(|c| c.starts_with("grow_disk")));
}

/// Samples with the CPU fully busy between each.
fn busy_samples(n: u64) -> Vec<Sample> {
    (1..=n)
        .map(|t| Sample {
            cpu: CpuTimes {
                busy: t * 100,
                total: t * 100,
            },
            ..calm()
        })
        .collect()
}

#[test]
fn a_vcpu_raise_is_logged_with_what_the_guest_sees() {
    let fake = Fake {
        onlined: Some(u32::MAX),
        ..Fake::new(calm())
    };
    *fake.samples.borrow_mut() = busy_samples(10);
    let log = ticks(&mut Planner::new(GROWTH), &fake, 4);
    assert!(fake.calls().contains(&"resize Some(4) None".to_owned()));
    assert!(log.contains("raised vCPUs 2 -> 4; guest sees 4"), "{log}");
}

#[test]
fn vcpus_the_guest_leaves_offline_are_a_logged_failure() {
    let fake = Fake {
        onlined: Some(2),
        ..Fake::new(calm())
    };
    *fake.samples.borrow_mut() = busy_samples(10);
    let log = ticks(&mut Planner::new(GROWTH), &fake, 4);
    assert!(
        log.contains("grow failed: raised vCPUs 2 -> 4 but the guest has 2 online"),
        "{log}"
    );
}

#[test]
fn a_rejected_memory_raise_is_logged() {
    let fake = Fake {
        resize_error: Some("HTTP 500: no hotplug".to_owned()),
        ..Fake::new(Sample {
            mem_avail: 0,
            ..calm()
        })
    };
    let log = ticks(&mut Planner::new(GROWTH), &fake, 3);
    assert!(fake.calls().contains(&"resize None Some(4096)".to_owned()));
    assert!(
        log.contains("grow failed: memory 2048 -> 4096 MiB: HTTP 500: no hotplug"),
        "{log}"
    );
}

/// Writes an executable shell script `name` into `dir`.
fn fake_tool(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs `GROW_FS_SCRIPT` on the host against fake guest tools.
fn run_grow_fs(growpart: &str) -> (bool, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path();
    fake_tool(bin, "sudo", "exec \"$@\"");
    fake_tool(bin, "apt-get", "echo apt-get must not run >&2; exit 99");
    fake_tool(bin, "findmnt", "echo /dev/vda1");
    fake_tool(
        bin,
        "lsblk",
        "case \"$2\" in PKNAME) echo vda ;; PARTN) echo '    1' ;; *) exit 9 ;; esac",
    );
    fake_tool(bin, "growpart", growpart);
    fake_tool(
        bin,
        "resize2fs",
        "echo \"resize2fs $*\" >> \"$(dirname \"$0\")/calls\"",
    );
    fake_tool(bin, "df", "printf '1B-blocks\\n 20401094656\\n'");
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let out = Command::new("sh")
        .arg("-c")
        .arg(GROW_FS_SCRIPT)
        .env("PATH", path)
        .output()
        .unwrap();
    let calls = fs::read_to_string(bin.join("calls")).unwrap_or_default();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        calls + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn grow_fs_script_grows_the_root_partition_then_its_filesystem() {
    let (ok, stdout, calls) = run_grow_fs("echo \"growpart $*\" >> \"$(dirname \"$0\")/calls\"");
    assert!(ok, "{calls}");
    assert_eq!(stdout, "20401094656");
    assert_eq!(calls, "growpart /dev/vda 1\nresize2fs /dev/vda1\n");
}

#[test]
fn grow_fs_script_treats_nochange_as_success() {
    let (ok, _, calls) = run_grow_fs("echo 'NOCHANGE: partition 1 is size 100'; exit 1");
    assert!(ok, "{calls}");
    assert!(calls.contains("resize2fs /dev/vda1"));
}

#[test]
fn grow_fs_script_fails_on_a_real_growpart_error() {
    let (ok, _, calls) = run_grow_fs("echo 'FAILED: disk too small' >&2; exit 2");
    assert!(!ok);
    assert!(
        calls.contains("growpart: FAILED: disk too small"),
        "{calls}"
    );
    assert!(!calls.contains("resize2fs"));
}
