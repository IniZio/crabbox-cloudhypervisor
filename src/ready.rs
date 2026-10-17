//! Readiness probe: runs the guest's ready check over ssh until it exits 0.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Guest-side script that exits 0 once provisioning has finished.
const READY_REMOTE: &str = "/usr/local/bin/crabbox-ready";
/// Longest a single ssh attempt may run before it is killed.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(30);
/// How often a running attempt is polled.
const POLL: Duration = Duration::from_millis(100);
/// Longest stderr excerpt kept in an error message.
const STDERR_MAX: usize = 200;
/// A progress note is written every this many attempts.
const NOTE_EVERY: u32 = 10;

/// How to reach one guest over ssh.
pub struct ReadyProbe {
    /// ssh client binary.
    pub ssh: PathBuf,
    /// Private key used to log in.
    pub key: PathBuf,
    /// `ProxyCommand` that reaches the guest.
    pub proxy_command: String,
    /// Login user.
    pub user: String,
    /// Host name passed to ssh.
    pub host: String,
    /// `known_hosts` file pinning the guest's host key.
    pub known_hosts: PathBuf,
    /// Host key alias the pinned key is filed under.
    pub host_key_alias: String,
}

/// One `known_hosts` line pinning `host_key` under `alias`.
#[must_use]
pub fn known_hosts_line(alias: &str, host_key: &str) -> String {
    format!("{alias} {host_key}\n")
}

impl ReadyProbe {
    /// Builds the ssh argument vector that runs `remote` on the guest.
    #[must_use]
    pub fn args(&self, remote: &str) -> Vec<String> {
        let mut args: Vec<String> = ["-F", "/dev/null", "-i"]
            .iter()
            .map(ToString::to_string)
            .collect();
        args.push(self.key.to_string_lossy().into_owned());
        for option in [
            "IdentitiesOnly=yes",
            "BatchMode=yes",
            "ConnectTimeout=10",
            "StrictHostKeyChecking=yes",
            "UpdateHostKeys=no",
            "VerifyHostKeyDNS=no",
            "LogLevel=ERROR",
        ] {
            args.push("-o".to_owned());
            args.push(option.to_owned());
        }
        args.push("-o".to_owned());
        args.push(format!("UserKnownHostsFile={}", self.known_hosts.display()));
        args.push("-o".to_owned());
        args.push(format!("HostKeyAlias={}", self.host_key_alias));
        args.push("-o".to_owned());
        args.push(format!("ProxyCommand={}", self.proxy_command));
        args.push("-p".to_owned());
        args.push("22".to_owned());
        args.push(format!("{}@{}", self.user, self.host));
        args.push(remote.to_owned());
        args
    }

    /// Polls the ready check until it passes, the VM dies, or `timeout` passes.
    pub fn wait(
        &self,
        timeout: Duration,
        interval: Duration,
        alive: &mut dyn FnMut() -> bool,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let _ = writeln!(
            diag,
            "waiting for guest readiness (up to {}s)",
            timeout.as_secs()
        );
        let mut attempts: u32 = 0;
        let mut last_error = String::new();
        while Instant::now() < deadline {
            if !alive() {
                return Err("vm exited before it became ready".to_owned());
            }
            attempts += 1;
            if attempts.is_multiple_of(NOTE_EVERY) {
                let _ = writeln!(
                    diag,
                    "still waiting for guest readiness (attempt {attempts})"
                );
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (ok, stderr) = self.attempt(remaining.min(ATTEMPT_LIMIT));
            if ok {
                return Ok(());
            }
            last_error = stderr;
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(interval.min(remaining));
        }
        let mut message = format!("vm not ready after {}s", timeout.as_secs());
        if !last_error.is_empty() {
            message.push_str(": ");
            message.push_str(&last_error);
        }
        Err(message)
    }

    /// Runs one ssh attempt; returns success and the last stderr line.
    fn attempt(&self, limit: Duration) -> (bool, String) {
        let spawned = Command::new(&self.ssh)
            .args(self.args(READY_REMOTE))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return (false, format!("cannot run ssh: {}", error.kind())),
        };
        let (sender, receiver) = mpsc::channel();
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                let _ = sender.send(text);
            });
        }
        let started = Instant::now();
        let success = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) if started.elapsed() < limit => std::thread::sleep(POLL),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        let text = receiver
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_default();
        (success, last_line(&text))
    }
}

/// Last non-empty line, trimmed and cut to `STDERR_MAX` characters.
fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(|line| line.chars().take(STDERR_MAX).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
