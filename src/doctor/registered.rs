//! The `doctor` check that crabbox's user config selects this adapter.

use std::io::Write;

use super::{run_checks, Check, Level, Probes};
use crate::config::{Config, Env};
use crate::install::adapter_path;
use crate::register;

/// [`super::run`] plus whether crabbox's user config selects this adapter.
pub fn run_host(config: &Config, env: Env<'_>, diag: &mut dyn Write) -> Result<String, String> {
    let registered = registration(env);
    run_checks(config, &Probes::host(config.cgroup), vec![registered], diag)
}

/// Whether crabbox's user config selects the adapter that is running.
pub(super) fn registration(env: Env<'_>) -> Check {
    let fix = "run `crabbox-cloudhypervisor install`";
    let status = adapter_path().and_then(|exe| register::status(env, &exe));
    match status {
        Ok(register::Status::Registered) => {
            Check::new(Level::Ok, "crabbox_config", "selects this adapter")
        }
        Ok(register::Status::Unregistered) => Check::new(
            Level::Fail,
            "crabbox_config",
            format!("does not select this adapter; {fix}"),
        ),
        Ok(register::Status::Other(change)) => Check::new(Level::Fail, "crabbox_config", change),
        Err(e) => Check::new(Level::Fail, "crabbox_config", format!("{e}; {fix}")),
    }
}
