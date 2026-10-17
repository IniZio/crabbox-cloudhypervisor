//! crabbox external provider that leases local cloud-hypervisor microVMs.
//!
//! crabbox runs the binary once per operation: one request object on stdin, one
//! response object on stdout, diagnostics on stderr.

pub mod protocol;

use std::io::Write;

/// What one invocation writes to stdout and the process exit code.
pub struct Outcome {
    /// One JSON response object, without a trailing newline.
    pub stdout: String,
    /// 0 on success, 1 on any error response.
    pub exit_code: u8,
}

/// Handles one request.
pub fn run(input: &[u8], diag: &mut dyn Write) -> Outcome {
    match handle(input, diag) {
        Ok(stdout) => Outcome {
            stdout,
            exit_code: 0,
        },
        Err(error) => Outcome {
            stdout: protocol::error_response(&error),
            exit_code: 1,
        },
    }
}

/// Dispatches a parsed request to its operation; returns the stdout JSON.
fn handle(input: &[u8], _diag: &mut dyn Write) -> Result<String, String> {
    let request = protocol::parse_request(input)?;
    match request.operation {
        protocol::Operation::Doctor => Ok(protocol::message_response(
            "doctor: backend checks not implemented yet",
        )),
        protocol::Operation::Touch => Ok(protocol::ok_response()),
        op => Err(format!("operation {} not implemented yet", op.as_str())),
    }
}
