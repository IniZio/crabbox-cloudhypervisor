//! Runs each lease's processes in systemd user scopes, the VM with kernel limits.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::host::Launch;
use crate::size::Size;

/// Bytes per MiB.
const MIB: u64 = 1024 * 1024;
/// `systemctl` exit status for a unit that is not loaded.
const NOT_LOADED: i32 = 5;

/// How lease processes are confined (`config.cgroup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cgroup {
    /// One systemd user scope per process, the VM's with CPU and memory limits.
    Systemd,
    /// Plain child processes with no limits.
    Off,
}

/// Which lease process a scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// cloud-hypervisor, the only scope with limits.
    Vm,
    /// passt.
    Passt,
    /// The growth watcher.
    Watch,
}

impl Role {
    /// Every role, in the order scopes are stopped.
    pub const ALL: [Self; 3] = [Self::Watch, Self::Vm, Self::Passt];
}

/// Kernel limits for a VM scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// CPUs of quota (`CPUQuota=<cpus*100>%`).
    pub cpus: u32,
    /// `MemoryMax`, in bytes.
    pub memory_bytes: u64,
}

impl Limits {
    /// Limits for a VM of `size`, with `overhead_mib` on top for the VMM itself.
    #[must_use]
    pub const fn for_vm(size: Size, overhead_mib: u64) -> Self {
        Self {
            cpus: size.cpus,
            memory_bytes: (size.memory_mib + overhead_mib) * MIB,
        }
    }
}

/// Unit name of a lease's scope for `role`.
#[must_use]
pub fn unit(lease_id: &str, role: Role) -> String {
    let id: String = lease_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let suffix = match role {
        Role::Vm => "",
        Role::Passt => "-passt",
        Role::Watch => "-watch",
    };
    format!("crabbox-{id}{suffix}.scope")
}

/// `systemd-run` argv that runs the command after it inside scope `unit`.
///
/// No `--collect`: a scope the OOM killer stopped stays loaded as failed, so
/// its `Result=oom-kill` can still be read until the lease is stopped.
#[must_use]
pub fn scope_argv(unit: &str, limits: Option<Limits>) -> Vec<OsString> {
    let mut argv: Vec<String> = ["systemd-run", "--user", "--scope", "--quiet"]
        .map(str::to_owned)
        .to_vec();
    argv.push(format!("--unit={unit}"));
    if let Some(l) = limits {
        for p in [
            format!("CPUQuota={}%", u64::from(l.cpus) * 100),
            format!("MemoryMax={}", l.memory_bytes),
            "MemorySwapMax=0".to_owned(),
        ] {
            argv.extend(["-p".to_owned(), p]);
        }
    }
    argv.push("--".to_owned());
    argv.into_iter().map(OsString::from).collect()
}

/// True when the unit result or its `memory.events` records an OOM kill.
#[must_use]
pub fn oom_killed(result: &str, memory_events: &str) -> bool {
    result.trim() == "oom-kill"
        || memory_events.lines().any(|l| {
            l.strip_prefix("oom_kill ")
                .and_then(|n| n.trim().parse::<u64>().ok())
                .is_some_and(|n| n > 0)
        })
}

/// Bytes in `systemctl show -p MemoryCurrent --value` output; `None` for `[not set]`.
#[must_use]
pub fn memory_current(value: &str) -> Option<u64> {
    value.trim().parse().ok().filter(|&n| n != u64::MAX)
}

/// The confinement mode plus where this user's systemd manager listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scopes {
    /// Configured mode.
    pub mode: Cgroup,
    /// The user manager's bus socket.
    pub bus: PathBuf,
    /// User name for the fix hint.
    pub user: String,
}

impl Scopes {
    /// Scopes for the current user; the bus lives in `XDG_RUNTIME_DIR`, else `/run/user/<uid>`.
    #[must_use]
    pub fn current(mode: Cgroup) -> Self {
        let uid = rustix::process::getuid().as_raw();
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);
        let user = std::env::var("USER").unwrap_or_else(|_| uid.to_string());
        Self {
            mode,
            bus: runtime.join("bus"),
            user,
        }
    }

    /// Fails with the fix when scopes are on and the user manager's bus is not there.
    pub fn check_bus(&self) -> Result<(), String> {
        let is_socket = fs::metadata(&self.bus).is_ok_and(|m| m.file_type().is_socket());
        if self.mode == Cgroup::Off || is_socket {
            return Ok(());
        }
        Err(format!(
            "cgroup: systemd needs a systemd user manager for {user}, but its bus {} is missing; \
             run `loginctl enable-linger {user}` (or set cgroup: off to run VMs without limits)",
            self.bus.display(),
            user = self.user,
        ))
    }

    /// The command that runs `program` for a lease; wrapped in its scope unless off.
    pub fn command(
        &self,
        lease_id: &str,
        role: Role,
        limits: Option<Limits>,
        program: &Path,
        args: &[OsString],
    ) -> Result<Launch, String> {
        if self.mode == Cgroup::Off {
            return Ok(Launch::plain(program, args));
        }
        self.check_bus()?;
        let mut wrapped = scope_argv(&unit(lease_id, role), limits);
        let runner = wrapped.remove(0);
        wrapped.push(program.as_os_str().to_owned());
        wrapped.extend_from_slice(args);
        // A service started without a login session lacks XDG_RUNTIME_DIR, and
        // systemd-run finds the user bus only through it.
        Ok(Launch {
            program: PathBuf::from(runner),
            args: wrapped,
            env: self.env(),
        })
    }

    /// Environment that points systemctl and systemd-run at the user manager.
    fn env(&self) -> Vec<(OsString, OsString)> {
        self.bus.parent().map_or_else(Vec::new, |dir| {
            vec![(
                OsString::from("XDG_RUNTIME_DIR"),
                dir.as_os_str().to_owned(),
            )]
        })
    }

    /// Runs `systemctl --user <args>`; returns (exit code, stdout, stderr).
    fn systemctl(&self, args: &[&str]) -> Result<(Option<i32>, String, String), String> {
        let out = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .envs(self.env())
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        Ok((
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        ))
    }

    /// True when the kernel OOM killer stopped the lease's VM scope.
    #[must_use]
    pub fn vm_oom_killed(&self, lease_id: &str) -> bool {
        if self.mode == Cgroup::Off {
            return false;
        }
        let name = unit(lease_id, Role::Vm);
        let show = |prop: &str| {
            self.systemctl(&["show", "-p", prop, "--value", &name])
                .map(|(_, out, _)| out.trim().to_owned())
                .unwrap_or_default()
        };
        let cgroup = show("ControlGroup");
        let events = if cgroup.is_empty() {
            String::new()
        } else {
            fs::read_to_string(format!("/sys/fs/cgroup{cgroup}/memory.events")).unwrap_or_default()
        };
        oom_killed(&show("Result"), &events)
    }

    /// The VM scope's `MemoryCurrent` in bytes; `None` when off or unreadable.
    #[must_use]
    pub fn vm_memory_bytes(&self, lease_id: &str) -> Option<u64> {
        if self.mode == Cgroup::Off {
            return None;
        }
        let name = unit(lease_id, Role::Vm);
        match self.systemctl(&["show", "-p", "MemoryCurrent", "--value", &name]) {
            Ok((Some(0), out, _)) => memory_current(&out),
            _ => None,
        }
    }

    /// Stops a lease's scopes and forgets failed ones; a scope already gone is fine.
    pub fn stop(&self, lease_id: &str, diag: &mut dyn Write) -> Result<(), String> {
        if self.mode == Cgroup::Off || self.check_bus().is_err() {
            return Ok(());
        }
        if self.vm_oom_killed(lease_id) {
            let _ = writeln!(diag, "{}", oom_note(lease_id));
        }
        let mut first_error = Ok(());
        for role in Role::ALL {
            let name = unit(lease_id, role);
            match self.systemctl(&["stop", &name])? {
                (Some(0 | NOT_LOADED), _, _) => {}
                (code, _, err) => {
                    let e = format!("stop scope {name}: exit {code:?}: {err}");
                    let _ = writeln!(diag, "warn: {e}");
                    first_error = first_error.and(Err(e));
                }
            }
            let _ = self.systemctl(&["reset-failed", &name]);
        }
        first_error
    }
}

/// The plain message for a VM the kernel killed at its memory limit.
#[must_use]
pub fn oom_note(lease_id: &str) -> String {
    format!(
        "lease {lease_id}: the VM was killed by the kernel OOM killer at its scope memory limit \
         (class memory + vmmOverheadMiB); raise vmmOverheadMiB or the class memory"
    )
}

#[cfg(test)]
mod tests;
