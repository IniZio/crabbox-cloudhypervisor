//! Reads one protocol request on stdin and writes one response on stdout.
//! `proxy <leaseId> --state-dir <dir>` runs the vsock SSH proxy instead, and
//! `watch <leaseId> --state-dir <dir>` the lease's growth watcher, and
//! `publish <leaseId> --state-dir <dir>` the cache publisher of a suspended lease,
//! and `tied <parentPid> -- <program> <args>` a program that dies with its parent.
//! `install` fetches the pinned cloud-hypervisor and passt; `doctor` checks the host.

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
        Some((cmd, rest)) if cmd == "publish" => {
            if let Err(error) = crabbox_cloudhypervisor::run_publish(rest, &mut stderr) {
                let _ = writeln!(stderr, "crabbox-cloudhypervisor: publish: {error}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Some((cmd, rest)) if cmd == "layers" => {
            if let Err(error) = crabbox_cloudhypervisor::run_layers(rest, &mut stderr) {
                let _ = writeln!(stderr, "crabbox-cloudhypervisor: layers: {error}");
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Some((cmd, _)) if cmd == "install" || cmd == "doctor" => host_command(cmd),
        Some((cmd, rest)) if cmd == "tied" => {
            let error = crabbox_cloudhypervisor::tied::run(rest);
            let _ = writeln!(stderr, "crabbox-cloudhypervisor: {error}");
            ExitCode::from(1)
        }
        Some((cmd, _)) => {
            let _ = writeln!(
                stderr,
                "crabbox-cloudhypervisor: unknown subcommand {cmd:?}; usage: crabbox-cloudhypervisor [install|doctor|proxy|watch|publish <leaseId> --state-dir <dir>]"
            );
            ExitCode::from(2)
        }
        None => protocol_mode(&mut stderr),
    }
}

/// `install` or `doctor` for a person at a terminal: the report on stdout.
fn host_command(cmd: &str) -> ExitCode {
    let env = |k: &str| std::env::var(k).ok();
    let result = if cmd == "install" {
        crabbox_cloudhypervisor::install::run(&env)
    } else {
        crabbox_cloudhypervisor::config::Config::from_request(&serde_json::Value::Null, &env)
            .and_then(|config| {
                crabbox_cloudhypervisor::doctor::run_host(&config, &env, &mut std::io::sink())
            })
    };
    let (report, code) = match result {
        Ok(report) => (report, ExitCode::SUCCESS),
        Err(report) => (report, ExitCode::from(1)),
    };
    let _ = writeln!(std::io::stdout(), "{report}");
    code
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
