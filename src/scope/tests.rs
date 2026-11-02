use super::*;

fn strings(argv: &[OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn systemd(bus: &Path) -> Scopes {
    Scopes {
        mode: Cgroup::Systemd,
        bus: bus.to_path_buf(),
        user: "alice".to_owned(),
    }
}

#[test]
fn vm_scope_carries_quota_memory_and_unit_name() {
    let limits = Limits {
        cpus: 4,
        memory_bytes: (8192 + 512) * 1024 * 1024,
    };
    let argv = scope_argv("crabbox-lease_1.scope", Some(limits));
    assert_eq!(
        strings(&argv),
        [
            "systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "--unit=crabbox-lease_1.scope",
            "-p",
            "CPUQuota=400%",
            "-p",
            "MemoryMax=9126805504",
            "-p",
            "MemorySwapMax=0",
            "--",
        ]
    );
}

#[test]
fn helper_scopes_carry_no_limits() {
    let argv = scope_argv("crabbox-x-passt.scope", None);
    assert!(!strings(&argv)
        .iter()
        .any(|a| a.contains("Quota") || a.contains("Max")));
    assert_eq!(strings(&argv).last().map(String::as_str), Some("--"));
}

#[test]
fn unit_names_are_per_lease_and_role() {
    assert_eq!(unit("abc-1", Role::Vm), "crabbox-abc-1.scope");
    assert_eq!(unit("abc-1", Role::Passt), "crabbox-abc-1-passt.scope");
    assert_eq!(unit("abc-1", Role::Watch), "crabbox-abc-1-watch.scope");
    assert_eq!(unit("a.b/c d", Role::Vm), "crabbox-a_b_c_d.scope");
}

#[test]
fn off_mode_runs_the_program_directly() {
    let scopes = Scopes {
        mode: Cgroup::Off,
        bus: PathBuf::from("/nonexistent/bus"),
        user: "alice".to_owned(),
    };
    let launch = scopes
        .command(
            "id",
            Role::Vm,
            None,
            Path::new("/bin/ch"),
            &[OsString::from("-v")],
        )
        .unwrap();
    assert_eq!(launch.program, PathBuf::from("/bin/ch"));
    assert_eq!(strings(&launch.args), ["-v"]);
    assert!(launch.env.is_empty());
}

#[test]
fn systemd_mode_wraps_the_program_in_its_scope() {
    let dir = tempfile::tempdir().unwrap();
    let bus = dir.path().join("bus");
    std::os::unix::net::UnixListener::bind(&bus).unwrap();
    let limits = Limits {
        cpus: 2,
        memory_bytes: 1 << 30,
    };
    let launch = systemd(&bus)
        .command(
            "l1",
            Role::Vm,
            Some(limits),
            Path::new("/bin/ch"),
            &[OsString::from("-v")],
        )
        .unwrap();
    assert_eq!(launch.program, PathBuf::from("systemd-run"));
    assert_eq!(
        launch.env.first().map(|(_, v)| v.as_os_str()),
        Some(dir.path().as_os_str())
    );
    let args = strings(&launch.args);
    assert!(args.contains(&"--unit=crabbox-l1.scope".to_owned()));
    assert!(args.contains(&"CPUQuota=200%".to_owned()));
    assert!(args.ends_with(&["--".to_owned(), "/bin/ch".to_owned(), "-v".to_owned()]));
}

#[test]
fn systemd_mode_without_a_user_bus_refuses_and_names_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let err = systemd(&dir.path().join("bus"))
        .command("l1", Role::Vm, None, Path::new("/bin/ch"), &[])
        .unwrap_err();
    assert!(err.contains("loginctl enable-linger alice"), "{err}");
    assert!(err.contains("cgroup: off"), "{err}");
}

#[test]
fn oom_shows_in_unit_result_or_memory_events() {
    assert!(oom_killed("oom-kill\n", ""));
    assert!(oom_killed("success", "low 0\noom 1\noom_kill 1\n"));
    assert!(!oom_killed("success", "oom 0\noom_kill 0\n"));
    assert!(!oom_killed("", ""));
}

#[test]
fn vm_limits_add_overhead_to_class_memory() {
    let size = Size {
        cpus: 3,
        memory_mib: 4096,
    };
    let limits = Limits::for_vm(size, 512);
    assert_eq!(limits.cpus, 3);
    assert_eq!(limits.memory_bytes, 4608 * 1024 * 1024);
}

#[test]
fn memory_current_reads_bytes_and_rejects_unset() {
    assert_eq!(memory_current("1176502272\n"), Some(1_176_502_272));
    assert_eq!(memory_current("[not set]\n"), None);
    assert_eq!(memory_current(""), None);
    assert_eq!(memory_current("18446744073709551615"), None);
}
