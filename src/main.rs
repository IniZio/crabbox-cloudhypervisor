//! Reads one protocol request on stdin and writes one response on stdout.
//! `proxy <leaseId> --state-dir <dir>` runs the vsock SSH proxy instead, and
//! `watch <leaseId> --state-dir <dir>` the lease's growth watcher.

use std::io::{Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stderr = std::io::stderr();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((cmd, rest)) if cmd == "proxy" => {
            if let Err(error) = crabbox_cloudhypervisor::run_proxy(rest, &mut stderr) {
                let _ = writeln!(stderr, "crabbox-cloudhypervisor: proxy: {error}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Some((cmd, rest)) if cmd == "watch" => {
            if let Err(error) = crabbox_cloudhypervisor::run_watch(rest, &mut stderr) {
                let _ = writeln!(stderr, "crabbox-cloudhypervisor: watch: {error}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Some((cmd, _)) => {
            let _ = writeln!(
                stderr,
                "crabbox-cloudhypervisor: unknown subcommand {cmd:?}; usage: crabbox-cloudhypervisor [proxy|watch <leaseId> --state-dir <dir>]"
            );
            ExitCode::from(2)
        }
        None => protocol_mode(&mut stderr),
    }
}

/// Serves one protocol request from stdin.
fn protocol_mode(stderr: &mut std::io::Stderr) -> ExitCode {
    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut input) {
        let _ = writeln!(stderr, "crabbox-cloudhypervisor: reading stdin: {error}");
    }
    let outcome = crabbox_cloudhypervisor::run(&input, stderr);
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{}", outcome.stdout).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::from(outcome.exit_code)
}
