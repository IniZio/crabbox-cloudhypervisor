//! crabbox external provider that leases local cloud-hypervisor microVMs.
//!
//! crabbox runs the binary once per operation: one request object on stdin, one
//! response object on stdout, diagnostics on stderr.

pub mod assets;
pub mod backend;
pub mod ch;
pub mod config;
pub mod doctor;
pub mod host;
pub mod inventory;
pub mod lease;
/// Pause and resume operations.
mod power;
pub mod protocol;
pub mod proxy;
pub mod ready;
pub mod seed;
pub mod state;
pub mod vm;

#[cfg(test)]
mod fake;

use std::io::Write;

use protocol::{Operation, Request};

/// What one invocation writes to stdout and the process exit code.
pub struct Outcome {
    /// One JSON response object, without a trailing newline.
    pub stdout: String,
    /// 0 on success, 1 on any error response.
    pub exit_code: u8,
}

/// Usage text for the `proxy` subcommand.
const PROXY_USAGE: &str = "usage: proxy <leaseId> --state-dir <dir>";

/// Builds the config from a request and the process environment.
fn config_of(request: &Request) -> Result<config::Config, String> {
    config::Config::from_request(&request.config, &|k| std::env::var(k).ok())
}

/// Wraps a handled result in an [`Outcome`].
fn outcome(result: Result<String, String>) -> Outcome {
    match result {
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

/// Handles one request against the real host.
pub fn run(input: &[u8], diag: &mut dyn Write) -> Outcome {
    outcome(handle(input, None, diag))
}

/// Handles one request against `backend`; doctor still reads the request config.
pub fn run_with(input: &[u8], backend: &dyn backend::Backend, diag: &mut dyn Write) -> Outcome {
    outcome(handle(input, Some(backend), diag))
}

/// Dispatches a parsed request; `injected` replaces the real backend.
fn handle(
    input: &[u8],
    injected: Option<&dyn backend::Backend>,
    diag: &mut dyn Write,
) -> Result<String, String> {
    let request = protocol::parse_request(input)?;
    match request.operation {
        Operation::Doctor => {
            doctor::run(&config_of(&request)?, diag).map(|m| protocol::message_response(&m))
        }
        Operation::Touch => Ok(protocol::ok_response()),
        _ => {
            if let Some(backend) = injected {
                return with_backend(&request, backend, diag);
            }
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot locate adapter binary: {e}"))?;
            let real = vm::RealBackend::new(config_of(&request)?, exe);
            with_backend(&request, &real, diag)
        }
    }
}

/// Runs an operation that needs the backend.
fn with_backend(
    request: &Request,
    backend: &dyn backend::Backend,
    diag: &mut dyn Write,
) -> Result<String, String> {
    match request.operation {
        Operation::Acquire => {
            lease::acquire(request, backend, diag).map(|l| protocol::lease_response(&l))
        }
        Operation::Resolve => {
            lease::resolve(request, backend, diag).map(|l| protocol::lease_response(&l))
        }
        Operation::List => {
            inventory::list(request, backend, diag).map(|rows| protocol::leases_response(&rows))
        }
        Operation::Release => {
            inventory::release(request, backend, diag).map(|()| protocol::ok_response())
        }
        Operation::Cleanup => {
            inventory::cleanup(request, backend, diag).map(|m| protocol::message_response(&m))
        }
        Operation::Pause => {
            power::pause(request, backend, diag).map(|m| protocol::message_response(&m))
        }
        Operation::Resume => {
            power::resume(request, backend, diag).map(|l| protocol::lease_response(&l))
        }
        Operation::Doctor | Operation::Touch => Ok(protocol::ok_response()),
    }
}

/// Value of `--state-dir` in either `--state-dir X` or `--state-dir=X` form.
fn state_dir_arg(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--state-dir" {
            return iter.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix("--state-dir=") {
            return Some(value);
        }
    }
    None
}

/// Runs `proxy <leaseId> --state-dir <dir>` (`args` are those after `proxy`).
pub fn run_proxy(args: &[String], _diag: &mut dyn Write) -> Result<(), String> {
    let dir = state_dir_arg(args).ok_or(PROXY_USAGE)?;
    let id = args
        .iter()
        .find(|a| !a.starts_with("--") && a.as_str() != dir)
        .ok_or(PROXY_USAGE)?;
    state::validate_lease_id(id)?;
    let record = state::Store::new(std::path::Path::new(dir))
        .read(id)?
        .ok_or_else(|| format!("lease {id} not found"))?;
    proxy::proxy(
        &record.vsock_socket(),
        22,
        std::io::stdin(),
        std::io::stdout(),
    )
}
