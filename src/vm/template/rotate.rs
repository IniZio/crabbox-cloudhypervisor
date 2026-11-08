//! The one guest script that turns a restored template clone into its lease:
//! clock, entropy, host key, machine id, hostname, per-lease files and
//! commands, and finally the login key.

use std::fmt::Write as _;

use crate::backend::SSH_USER;
use crate::seed::APT_JOBS;
use crate::vm::spec::shell_quote;
use crate::volumes::guest::GuestSetup;

/// Base64 alphabet (RFC 4648).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Shell function writing base64 `$3` to `$1` with mode `$2` via rename.
const PUT: &str = r#"put() { t="$1.crabbox-tmp"; : >"$t"; chmod "$2" "$t"; printf %s "$3" | base64 -d >"$t"; mv -f "$t" "$1"; }"#;

/// Everything that makes a clone this lease's guest and no other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Identity {
    /// Host time, in unix seconds.
    pub epoch: u64,
    /// Fresh host randomness, so clones do not share their RNG state.
    pub entropy: Vec<u8>,
    /// Guest hostname.
    pub hostname: String,
    /// Lease ssh host private key (OpenSSH PEM).
    pub host_private_key: String,
    /// Lease ssh host public key line.
    pub host_public_key: String,
    /// The one key `authorized_keys` keeps.
    pub lease_public_key: String,
    /// Files and commands the cold-boot seed would apply.
    pub guest: GuestSetup,
}

/// Standard padded base64.
#[must_use]
pub(super) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [0, 1, 2].map(|i| chunk.get(i).copied().unwrap_or(0));
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let index = (n >> (18 - 6 * i)) & 63;
                let c = ALPHABET.get(index as usize).copied().unwrap_or(b'=');
                out.push(char::from(c));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One `put` line for `content` at `path`.
fn put(path: &str, mode: &str, content: &str) -> String {
    format!(
        "put {} {mode} {}\n",
        shell_quote(path),
        base64(content.as_bytes())
    )
}

/// The rotation script, run as root. Identity steps stop on the first error;
/// per-lease commands, like cloud-init's runcmd, only report theirs. The apt
/// jobs are masked before the clock steps forward, which would fire their
/// missed runs, so a snapshot taken before they were masked is covered too.
#[must_use]
pub(super) fn script(id: &Identity) -> String {
    let host = shell_quote(&id.hostname);
    let mut s = format!(
        "set -eu\n{PUT}\nsystemctl mask --now {} || echo 'crabbox: failed: masking apt jobs' >&2\ndate -s @{} >/dev/null\nprintf %s {} | base64 -d >/dev/urandom\n",
        APT_JOBS.join(" "),
        id.epoch,
        base64(&id.entropy)
    );
    s.push_str(&put(
        "/etc/ssh/ssh_host_ed25519_key",
        "0600",
        &id.host_private_key,
    ));
    s.push_str(&put(
        "/etc/ssh/ssh_host_ed25519_key.pub",
        "0644",
        &format!("{}\n", id.host_public_key.trim()),
    ));
    s.push_str("tr -d - </proc/sys/kernel/random/uuid >/etc/machine-id\n");
    let _ = writeln!(
        s,
        "hostnamectl set-hostname {host} 2>/dev/null || {{ echo {host} >/etc/hostname; hostname {host}; }}"
    );
    for f in &id.guest.files {
        let _ = writeln!(s, "mkdir -p \"$(dirname {})\"", shell_quote(&f.path));
        s.push_str(&put(&f.path, f.permissions, &f.content));
    }
    s.push_str("systemctl daemon-reload\n");
    for argv in &id.guest.commands {
        let line: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
        let line = line.join(" ");
        let _ = writeln!(
            s,
            "{line} || echo {} >&2",
            shell_quote(&format!("crabbox: failed: {line}"))
        );
    }
    let _ = writeln!(
        s,
        "home=$(getent passwd {SSH_USER} | cut -d: -f6)\ninstall -d -m 0700 -o {SSH_USER} -g {SSH_USER} \"$home/.ssh\""
    );
    let _ = writeln!(
        s,
        "t=\"$home/.ssh/authorized_keys.crabbox-tmp\"\n: >\"$t\"; chmod 0600 \"$t\"\nprintf %s {} | base64 -d >\"$t\"",
        base64(format!("{}\n", id.lease_public_key.trim()).as_bytes())
    );
    let _ = writeln!(
        s,
        "chown {SSH_USER}:{SSH_USER} \"$t\"; mv -f \"$t\" \"$home/.ssh/authorized_keys\""
    );
    s.push_str("systemctl try-restart ssh.service 2>/dev/null || true\n");
    s
}
