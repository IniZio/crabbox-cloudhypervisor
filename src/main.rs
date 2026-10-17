//! Reads one protocol request on stdin and writes one response on stdout.

use std::io::{Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut stderr = std::io::stderr();
    let mut input = Vec::new();
    if let Err(error) = std::io::stdin().read_to_end(&mut input) {
        let _ = writeln!(stderr, "crabbox-cloudhypervisor: reading stdin: {error}");
    }
    let outcome = crabbox_cloudhypervisor::run(&input, &mut stderr);
    let mut stdout = std::io::stdout();
    if writeln!(stdout, "{}", outcome.stdout).is_err() {
        return ExitCode::from(1);
    }
    ExitCode::from(outcome.exit_code)
}
