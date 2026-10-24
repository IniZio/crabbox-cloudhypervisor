#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::process::Command;

use super::*;
use crate::volumes::plan;
use crate::volumes::tests::{limits, req};

#[test]
fn escape_path_matches_systemd_escape() {
    assert_eq!(
        escape_path("/var/lib/crabbox-cache/7f3a"),
        r"var-lib-crabbox\x2dcache-7f3a"
    );
    assert_eq!(
        escape_path("/home/crabbox/.gradle/caches"),
        "home-crabbox-.gradle-caches"
    );
    assert_eq!(escape_path("/.hidden/a_b+c@d"), r"\x2ehidden-a_b\x2bc\x40d");
    assert_eq!(escape_path("/"), "-");
}

fn file<'a>(setup: &'a GuestSetup, path: &str) -> &'a SeedFile {
    setup
        .files
        .iter()
        .find(|f| f.path == path)
        .expect("seed file present")
}

/// Setup for one group: gradle (two home paths) and an sdk under /opt.
fn gradle_and_sdk() -> GuestSetup {
    let reqs = [
        req(
            "g7",
            "gradle",
            &["~/.gradle/caches", "~/.gradle/wrapper"],
            0,
        ),
        req("g7", "sdk", &["/opt/android-sdk"], 0),
    ];
    let plan = plan(&reqs, &limits(40));
    setup(&[Attached {
        group: &plan.groups[0],
        serial: "cbxcv0",
    }])
}

#[test]
fn the_group_disk_mounts_by_serial_before_docker() {
    let setup = gradle_and_sdk();
    let disk = file(
        &setup,
        r"/etc/systemd/system/var-lib-crabbox\x2dcache-g7.mount",
    );
    assert!(disk.content.contains(
        "What=/dev/disk/by-id/virtio-cbxcv0\nWhere=/var/lib/crabbox-cache/g7\nType=ext4\n"
    ));
    assert!(disk
        .content
        .contains("Options=noatime,discard,nofail,noinit_itable\n"));
    assert!(disk
        .content
        .contains("Before=crabbox-cache-dirs.service docker.service containerd.service\n"));
    let unit = file(&setup, "/etc/systemd/system/crabbox-cache-dirs.service");
    assert!(unit.content.contains(
        "DefaultDependencies=no\nAfter=local-fs-pre.target var-lib-crabbox\\x2dcache-g7.mount\n"
    ));
}

#[test]
fn each_path_binds_its_own_directory_of_the_volume() {
    let setup = gradle_and_sdk();
    let caches = file(
        &setup,
        "/etc/systemd/system/home-crabbox-.gradle-caches.mount",
    );
    assert!(caches.content.contains(
        "What=/var/lib/crabbox-cache/g7/gradle/home-crabbox-.gradle-caches\nWhere=/home/crabbox/.gradle/caches\nType=none\nOptions=bind,nofail\n"
    ));
    assert!(caches
        .content
        .contains("Before=docker.service containerd.service\n"));
    assert!(caches.content.contains("WantedBy=local-fs.target\n"));
    let wrapper = file(
        &setup,
        "/etc/systemd/system/home-crabbox-.gradle-wrapper.mount",
    );
    assert!(wrapper
        .content
        .contains("What=/var/lib/crabbox-cache/g7/gradle/home-crabbox-.gradle-wrapper\n"));
    let sdk = file(&setup, r"/etc/systemd/system/opt-android\x2dsdk.mount");
    assert!(sdk.content.contains("Where=/opt/android-sdk\n"));
}

#[test]
fn every_cache_mount_is_ordered_before_containerd_and_docker() {
    let mut docker = req("g7x", "docker", &["~/docker", "/var/lib/containerd"], 0);
    docker.sealed = true;
    let reqs = [req("g7", "npm", &["~/.npm"], 0), docker];
    let plan = plan(&reqs, &limits(40));
    let setup = setup(&[
        Attached {
            group: &plan.groups[0],
            serial: "cbxcv0",
        },
        Attached {
            group: &plan.groups[1],
            serial: "cbxcv1",
        },
    ]);
    let mounts: Vec<&SeedFile> = setup
        .files
        .iter()
        .filter(|f| std::path::Path::new(&f.path).extension() == Some("mount".as_ref()))
        .collect();
    let targets: Vec<&str> = mounts
        .iter()
        .filter_map(|f| f.content.split("Where=").nth(1)?.lines().next())
        .collect();
    assert_eq!(
        targets,
        [
            "/var/lib/crabbox-cache/g7",
            "/home/crabbox/.npm",
            "/var/lib/crabbox-cache/g7x",
            "/home/crabbox/docker",
            "/var/lib/containerd",
        ]
    );
    for unit in mounts {
        let before = unit
            .content
            .lines()
            .find_map(|l| l.strip_prefix("Before="))
            .unwrap_or_default();
        let services: Vec<&str> = before.split(' ').collect();
        assert!(
            services.contains(&"docker.service") && services.contains(&"containerd.service"),
            "{}: Before={before}",
            unit.path
        );
    }
    let containerd = file(&setup, "/etc/systemd/system/var-lib-containerd.mount");
    assert!(containerd
        .content
        .contains("What=/var/lib/crabbox-cache/g7x/docker/var-lib-containerd\n"));
}

#[test]
fn missing_directories_are_made_for_the_login_user() {
    let setup = gradle_and_sdk();
    let dirs = file(&setup, "/usr/local/sbin/crabbox-cache-dirs");
    assert_eq!(dirs.permissions, "0755");
    assert!(dirs.content.contains("install -d -o 1000 -g 1000"));
    for line in [
        "mk '/var/lib/crabbox-cache/g7/gradle'\n",
        "mk '/var/lib/crabbox-cache/g7/gradle/home-crabbox-.gradle-caches'\n",
        "mk '/home/crabbox/.gradle'\nmk '/home/crabbox/.gradle/caches'\n",
        "mk '/opt'\nmk '/opt/android-sdk'\n",
    ] {
        assert!(dirs.content.contains(line), "{line} in {}", dirs.content);
    }
}

#[test]
fn first_boot_enables_disk_then_dirs_then_binds() {
    let setup = gradle_and_sdk();
    assert_eq!(
        setup.commands.last().unwrap(),
        &[
            "systemctl",
            "enable",
            "--now",
            r"var-lib-crabbox\x2dcache-g7.mount",
            "crabbox-cache-dirs.service",
            "home-crabbox-.gradle-caches.mount",
            "home-crabbox-.gradle-wrapper.mount",
            r"opt-android\x2dsdk.mount",
        ]
    );
}

#[test]
fn without_groups_only_the_docker_root_is_set() {
    let setup = setup(&[]);
    let paths: Vec<&str> = setup.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["/usr/local/lib/crabbox/docker-data-root.py"]);
    assert_eq!(
        setup.commands,
        [[
            "sh",
            "-c",
            "python3 /usr/local/lib/crabbox/docker-data-root.py || true"
        ]]
    );
}

fn python_merges(existing: Option<&str>) -> Option<serde_json::Value> {
    if Command::new("python3").arg("-V").output().is_err() {
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let daemon = dir.path().join("docker/daemon.json");
    if let Some(text) = existing {
        std::fs::create_dir_all(daemon.parent().unwrap()).unwrap();
        std::fs::write(&daemon, text).unwrap();
    }
    let script = docker_root_script(daemon.to_str().unwrap(), "/home/crabbox/docker");
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(serde_json::from_str(&std::fs::read_to_string(&daemon).unwrap()).unwrap())
}

#[test]
fn docker_data_root_is_written_or_merged() {
    let Some(fresh) = python_merges(None) else {
        return;
    };
    assert_eq!(
        fresh,
        serde_json::json!({"data-root": "/home/crabbox/docker"})
    );
    let merged = python_merges(Some(
        r#"{"data-root":"/var/lib/docker","log-driver":"local"}"#,
    ));
    assert_eq!(
        merged,
        Some(serde_json::json!({"data-root": "/home/crabbox/docker", "log-driver": "local"}))
    );
}
