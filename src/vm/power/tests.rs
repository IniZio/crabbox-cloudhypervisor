use std::cell::Cell;
use std::process::{Child, Command};
use std::time::Duration;

use super::terminate;

/// A child that `terminate` may signal; reaped on drop.
struct Sleeper(Child);

impl Sleeper {
    fn spawn() -> Self {
        Self(Command::new("sleep").arg("30").spawn().unwrap())
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn terminate_returns_only_once_the_process_is_gone() {
    let child = Sleeper::spawn();
    let checks = Cell::new(0);
    let alive = || {
        checks.set(checks.get() + 1);
        checks.get() <= 5
    };
    terminate(child.0.id(), alive, Duration::ZERO, &mut Vec::new()).unwrap();
    assert!(checks.get() > 5, "returned while still alive");
}

#[test]
fn terminate_errors_when_the_process_outlives_kill() {
    let child = Sleeper::spawn();
    let err = terminate(child.0.id(), || true, Duration::ZERO, &mut Vec::new()).unwrap_err();
    assert!(err.contains("still alive"), "{err}");
}

mod clock {
    #![allow(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use std::path::{Path, PathBuf};

    use crate::backend::Backend;
    use crate::state::LeaseRecord;
    use crate::vm::tests::{config, info, live_fixture, serve, FakeCh};
    use crate::vm::RealBackend;

    const OK: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

    /// Fixture whose remote shell client logs the command of every call and
    /// fails the clock step when `step_fails`.
    fn fixture(dir: &Path, ch: &FakeCh, step_fails: bool) -> (RealBackend, LeaseRecord, PathBuf) {
        let (mut backend, mut rec) = live_fixture(dir, Some(ch));
        let me = std::process::id();
        rec.boot_id = crate::host::boot_id();
        rec.watch_pid = Some(me);
        rec.watch_start = crate::host::proc_start(me);
        let log = dir.join("calls.log");
        let client = dir.join("client");
        let fail = if step_fails {
            "case \"$last\" in *date*) exit 1;; esac"
        } else {
            ""
        };
        let body = format!(
            "for a; do last=$a; done; echo \"$last\" >> '{}'; {fail}",
            log.display()
        );
        crate::test_exec::script(&client, &body);
        let mut cfg = config(dir);
        cfg.ssh = client;
        backend.config = cfg;
        (backend, rec, log)
    }

    fn resume(backend: &RealBackend, rec: &LeaseRecord) -> (Result<(), String>, String) {
        let mut diag = Vec::new();
        let result = backend.resume(rec, &mut diag);
        (result, String::from_utf8(diag).unwrap())
    }

    fn paused_replies() -> Vec<String> {
        vec![info("Paused"), OK.to_owned(), info("Running")]
    }

    #[test]
    fn resuming_a_paused_vm_steps_the_guest_clock() {
        let dir = tempfile::tempdir().unwrap();
        let ch = FakeCh::spawn();
        let (backend, rec, log) = fixture(dir.path(), &ch, false);
        let _ = serve(&rec.api_socket(), paused_replies());
        let (result, diag) = resume(&backend, &rec);
        result.unwrap();
        assert!(diag.contains("guest clock stepped to host time"), "{diag}");
        let calls = std::fs::read_to_string(log).unwrap();
        assert!(calls.contains("sudo date -s @"), "{calls}");
    }

    #[test]
    fn resuming_a_running_vm_leaves_the_guest_clock() {
        let dir = tempfile::tempdir().unwrap();
        let ch = FakeCh::spawn();
        let (backend, rec, log) = fixture(dir.path(), &ch, false);
        let _ = serve(&rec.api_socket(), vec![info("Running")]);
        let (result, diag) = resume(&backend, &rec);
        result.unwrap();
        assert!(!diag.contains("guest clock"), "{diag}");
        assert!(!log.exists());
    }

    #[test]
    fn a_failed_clock_step_does_not_fail_the_resume() {
        let dir = tempfile::tempdir().unwrap();
        let ch = FakeCh::spawn();
        let (backend, rec, _) = fixture(dir.path(), &ch, true);
        let _ = serve(&rec.api_socket(), paused_replies());
        let (result, diag) = resume(&backend, &rec);
        result.unwrap();
        assert!(diag.contains("guest clock step failed:"), "{diag}");
    }
}
