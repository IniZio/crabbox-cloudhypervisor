//! The checkpoint check: the host's cloud-hypervisor must be the patched build.

use std::path::Path;
use std::process::{Command, Stdio};

use super::{first_line, Check, Level};

/// Commit of the vhost-user resume patch; the `v52.0-moor.1` build prints it in
/// its version (`v52.0-1-g5a781e3`).
const PATCHED_COMMIT: &str = "g5a781e3";

/// Patched cloud-hypervisor a checkpointing host needs.
const PATCHED_VERSION: &str = "v52.0-moor.1";

/// Stock cloud-hypervisor leaves a guest's network dead after a checkpoint, so a
/// host that serves checkpoints needs the patched build.
pub(super) fn hypervisor(bin: &Path) -> Check {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .output();
    let version = out
        .map(|o| first_line(&o.stdout, &o.stderr))
        .unwrap_or_default();
    if version.contains(PATCHED_COMMIT) {
        Check::new(Level::Ok, "checkpoints", version)
    } else {
        let seen = if version.is_empty() {
            "version unreadable".to_owned()
        } else {
            format!("found {version}")
        };
        Check::new(
            Level::Fail,
            "checkpoints",
            format!(
                "cloud-hypervisor {PATCHED_VERSION} is required, a guest's network is dead after a checkpoint on stock builds ({seen})"
            ),
        )
    }
}
