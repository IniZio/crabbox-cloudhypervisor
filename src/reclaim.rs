//! Guest page-cache reclaim: a systemd timer that trims file cache, and a lower
//! free page reporting order, so memory a guest stops using returns to the host.
//!
//! Written by the seed, so only leases created after a change pick it up.

use crate::volumes::guest::{GuestSetup, SeedFile};

/// Guest path of the reclaim script.
const SCRIPT_PATH: &str = "/usr/local/sbin/crabbox-reclaim";
/// Script body with `@KEY@` placeholders.
const SCRIPT: &str = include_str!("reclaim/crabbox-reclaim.sh");
/// Timer unit that runs the script.
const TIMER: &str = "crabbox-reclaim.timer";
/// tmpfiles rule that sets the page reporting order on every boot.
const TMPFILES_PATH: &str = "/etc/tmpfiles.d/crabbox-page-reporting.conf";
/// Kernel parameter for the smallest free block the guest reports to the host.
const ORDER_PARAM: &str = "/sys/module/page_reporting/parameters/page_reporting_order";

/// How the guest trims its file cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReclaimConfig {
    /// Seconds between reclaim runs.
    pub interval_secs: u64,
    /// File cache the guest keeps, in MiB.
    pub floor_mib: u64,
    /// File re-reads from disk between runs, in MiB, that pause reclaim...
    pub refault_mib: u64,
    /// ...for this many runs.
    pub skip_runs: u64,
    /// Smallest free block (2^order pages) the guest reports to the host.
    pub page_reporting_order: u64,
}

impl Default for ReclaimConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            floor_mib: 512,
            refault_mib: 16,
            skip_runs: 10,
            page_reporting_order: 2,
        }
    }
}

/// The reclaim script with `config` substituted.
#[must_use]
pub fn script(config: &ReclaimConfig) -> String {
    SCRIPT
        .replace("@REFAULT_MIB@", &config.refault_mib.to_string())
        .replace("@SKIP_RUNS@", &config.skip_runs.to_string())
        .replace("@FLOOR_MIB@", &config.floor_mib.to_string())
}

/// Adds the script, its timer, the page reporting rule and the commands that start them.
pub fn add_to(setup: &mut GuestSetup, config: &ReclaimConfig) {
    let interval = config.interval_secs;
    let service = format!(
        "[Unit]\nDescription=Drop guest file cache above a floor\n\
         [Service]\nType=oneshot\nExecStart={SCRIPT_PATH}\n"
    );
    let timer = format!(
        "[Unit]\nDescription=Drop guest file cache every {interval}s\n[Timer]\n\
         OnBootSec={interval}s\nOnUnitActiveSec={interval}s\nAccuracySec=1s\n\
         [Install]\nWantedBy=timers.target\n"
    );
    setup.files.extend([
        file(SCRIPT_PATH, "0755", script(config)),
        file("/etc/systemd/system/crabbox-reclaim.service", "0644", service),
        file(&format!("/etc/systemd/system/{TIMER}"), "0644", timer),
        file(
            TMPFILES_PATH,
            "0644",
            format!("w {ORDER_PARAM} - - - - {}\n", config.page_reporting_order),
        ),
    ]);
    setup.commands.extend([
        argv(&["systemd-tmpfiles", "--create", TMPFILES_PATH]),
        argv(&["systemctl", "enable", "--now", TIMER]),
    ]);
}

/// A file cloud-init writes.
fn file(path: &str, permissions: &'static str, content: String) -> SeedFile {
    SeedFile {
        path: path.to_owned(),
        permissions,
        content,
    }
}

/// An owned argument vector.
fn argv(args: &[&str]) -> Vec<String> {
    args.iter().copied().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests;
