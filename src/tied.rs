//! `tied <parentPid> -- <program> <args>`: runs a program that the kernel
//! kills with SIGKILL when the adapter that started it exits, even by SIGKILL.

use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::process::{getppid, set_parent_process_death_signal, Signal};

/// Usage of the `tied` subcommand.
const USAGE: &str = "usage: crabbox-cloudhypervisor tied <parentPid> -- <program> [args...]";

/// Sets SIGKILL as this process's parent-death signal, then execs the program.
/// Returns only on failure.
pub fn run(args: &[String]) -> String {
    let [parent, dashes, program, rest @ ..] = args else {
        return USAGE.to_owned();
    };
    let Ok(parent) = parent.parse::<i32>() else {
        return USAGE.to_owned();
    };
    if dashes != "--" {
        return USAGE.to_owned();
    }
    if let Err(e) = set_parent_process_death_signal(Some(Signal::KILL)) {
        return format!("tied: set parent-death signal: {e}");
    }
    if getppid().map(rustix::process::Pid::as_raw_nonzero) != std::num::NonZeroI32::new(parent) {
        return format!("tied: parent {parent} is gone");
    }
    let e = Command::new(program).args(rest).exec();
    format!("tied: exec {program}: {e}")
}
