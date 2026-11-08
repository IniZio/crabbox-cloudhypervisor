//! The passt check: VMs reach the network through passt's vhost-user mode.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{Check, Level};
use crate::install::fix;

/// Fails when `passt --help` does not offer `--vhost-user`, as distro builds do not.
pub(super) fn vhost_user(passt: &Path) -> Check {
    let help = Command::new(passt)
        .arg("--help")
        .stdin(Stdio::null())
        .output();
    let offered = help.is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout).contains("--vhost-user")
            || String::from_utf8_lossy(&o.stderr).contains("--vhost-user")
    });
    if offered {
        Check::new(Level::Ok, "passt_vhost_user", "--vhost-user supported")
    } else {
        Check::new(
            Level::Fail,
            "passt_vhost_user",
            format!("passt lacks --vhost-user; {}", fix("passt")),
        )
    }
}
