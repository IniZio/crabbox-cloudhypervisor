//! Guest hygiene: the settle rule, and the command sent to the guest.

use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use super::{settle, Settled, CAP, GUEST_SCRIPT, STEADY_BYTES, STEADY_RUN};
use crate::vm::tests::{config, record};
use crate::vm::RealBackend;

const MIB: u64 = 1024 * 1024;
const NO_WAIT: Duration = Duration::from_millis(1);

/// Yields `values` in order, then repeats the last.
fn series(values: &[u64]) -> impl FnMut() -> Option<u64> + '_ {
    let at = Cell::new(0);
    move || {
        let i = at.get();
        at.set(i + 1);
        values.get(i).or_else(|| values.last()).copied()
    }
}

#[test]
fn the_wait_ends_once_memory_has_held_still_for_the_steady_run() {
    let got = settle(CAP, NO_WAIT, series(&[900 * MIB, 500 * MIB, 300 * MIB]));
    assert_eq!(
        got,
        Some(Settled {
            first: 900 * MIB,
            last: 300 * MIB,
            samples: 3 + STEADY_RUN,
            capped: false
        })
    );
}

#[test]
fn a_pause_between_bursts_does_not_end_the_wait() {
    let small = STEADY_BYTES - 1;
    let mut values = vec![1000 * MIB];
    values.extend(std::iter::repeat_n(
        1000 * MIB - small,
        (STEADY_RUN - 1) as usize,
    ));
    values.push(500 * MIB);
    values.push(500 * MIB - small);
    let got = settle(CAP, NO_WAIT, series(&values)).unwrap();
    assert_eq!(got.last, 500 * MIB - small, "{got:?}");
    assert_eq!(got.samples, 1 + 2 * STEADY_RUN, "{got:?}");
    assert!(!got.capped);
}

#[test]
fn memory_that_keeps_falling_is_cut_off_at_the_cap() {
    let falling = Cell::new(100_000 * MIB);
    let got = settle(Duration::from_millis(50), NO_WAIT, || {
        falling.set(falling.get() - 100 * MIB);
        Some(falling.get())
    })
    .unwrap();
    assert!(got.capped);
    assert_eq!(
        got.samples, 51,
        "the first sample plus one per poll in the cap"
    );
}

#[test]
fn unreadable_memory_ends_the_wait_without_a_result() {
    assert_eq!(settle(CAP, NO_WAIT, || None), None);
    let n = Cell::new(0);
    let got = settle(CAP, NO_WAIT, || {
        n.set(n.get() + 1);
        (n.get() < 3).then_some(MIB)
    });
    assert_eq!(got, None);
}

#[test]
fn the_guest_script_flushes_drops_caches_then_compacts_in_that_order() {
    let at = |needle: &str| GUEST_SCRIPT.find(needle).unwrap();
    assert!(at("sync;") < at("drop_caches"));
    assert!(at("drop_caches") < at("compact_memory"));
    assert!(
        GUEST_SCRIPT.starts_with("sudo "),
        "writing /proc/sys needs root"
    );
}

/// A backend whose ssh is `body`, and a running lease `a` on this process.
fn backend_with_ssh(root: &std::path::Path, body: &str) -> RealBackend {
    let mut cfg = config(root);
    cfg.ssh = root.join("ssh");
    crate::test_exec::script(&cfg.ssh, body);
    RealBackend::new(cfg, PathBuf::from("/bin/cb"))
}

#[test]
fn the_script_runs_over_the_leases_own_ssh_and_memory_is_logged() {
    let root = tempfile::tempdir().unwrap();
    let seen = root.path().join("seen");
    let b = backend_with_ssh(
        root.path(),
        &format!(
            r#"for a; do last=$a; done; printf '%s\n' "$last" >> {}"#,
            seen.display()
        ),
    );
    let mut rec = record("a");
    rec.ch_pid = Some(std::process::id());
    let mut diag = Vec::new();
    b.tidy_guest(&rec, &mut diag);
    assert_eq!(
        fs::read_to_string(&seen).unwrap(),
        format!("{GUEST_SCRIPT}\n")
    );
    let diag = String::from_utf8(diag).unwrap();
    assert!(diag.contains("guest tidied: vm memory "), "{diag}");
}

#[test]
fn a_guest_that_cannot_be_tidied_is_logged_and_not_waited_on() {
    let root = tempfile::tempdir().unwrap();
    let b = backend_with_ssh(root.path(), "echo 'permission denied' >&2; exit 255");
    let mut rec = record("a");
    rec.ch_pid = Some(std::process::id());
    let mut diag = Vec::new();
    b.tidy_guest(&rec, &mut diag);
    let diag = String::from_utf8(diag).unwrap();
    assert_eq!(
        diag,
        "warn: guest not tidied before the checkpoint: permission denied\n"
    );
}

#[test]
fn a_vm_whose_memory_cannot_be_read_is_not_waited_on() {
    let root = tempfile::tempdir().unwrap();
    let b = backend_with_ssh(root.path(), "exit 0");
    let mut diag = Vec::new();
    b.tidy_guest(&record("a"), &mut diag);
    let diag = String::from_utf8(diag).unwrap();
    assert!(diag.contains("vm memory unreadable"), "{diag}");
}
