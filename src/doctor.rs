//! Host readiness checks for `doctor`: devices, tools, directories, caches.
//!
//! Details never echo configured paths or URLs; they name the check only.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::assets::{cache_state, CacheState};
use crate::config::{Asset, Config};
use crate::state::{LONGEST_SOCKET_NAME, RUN_DIR_NAME_LEN};
use crate::vm::spec::{base_raw_path, firmware_cache_path, image_cache_path};

#[cfg(test)]
mod tests;

/// Longest unix socket path the kernel accepts, including the NUL.
const SOCKET_PATH_MAX: usize = 108;

/// Device nodes probed by the doctor; injectable for tests.
#[derive(Debug, Clone)]
struct Probes {
    /// KVM device node.
    kvm: PathBuf,
    /// vhost-vsock device node.
    vhost_vsock: PathBuf,
}

impl Probes {
    /// The real host device nodes.
    fn host() -> Self {
        Self {
            kvm: PathBuf::from("/dev/kvm"),
            vhost_vsock: PathBuf::from("/dev/vhost-vsock"),
        }
    }
}

/// Outcome level of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Check passed.
    Ok,
    /// Degraded but usable.
    Warn,
    /// Provider cannot work.
    Fail,
}

/// One report line.
#[derive(Debug)]
struct Check {
    /// Severity.
    level: Level,
    /// Short check name.
    name: &'static str,
    /// Human detail without configured paths.
    detail: String,
}

impl Check {
    /// Builds a check result.
    fn new(level: Level, name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level,
            name,
            detail: detail.into(),
        }
    }
}

/// Runs every host check and returns the report; `Err` when any check failed.
pub fn run(config: &Config, diag: &mut dyn Write) -> Result<String, String> {
    run_with(config, &Probes::host(), diag)
}

/// Like [`run`] with injectable device paths.
fn run_with(config: &Config, probes: &Probes, diag: &mut dyn Write) -> Result<String, String> {
    let mut checks = vec![
        device("kvm", &probes.kvm, Level::Fail),
        device("vhost-vsock", &probes.vhost_vsock, Level::Warn),
    ];
    checks.extend(tool_checks(config));
    checks.push(writable("runtime_dir", &config.runtime_dir));
    checks.push(writable("state_dir", &config.state_dir));
    checks.push(socket_room(&config.runtime_dir));
    checks.push(cache(
        "firmware",
        &firmware_cache_path(config),
        &config.firmware,
    ));
    checks.push(cache("image", &image_cache_path(config), &config.image));
    checks.push(raw_base(config));
    for check in checks.iter().filter(|c| c.level != Level::Ok) {
        let _ = writeln!(diag, "{}", line(check));
    }
    let report = render(&checks);
    let _ = writeln!(diag, "doctor finished");
    if checks.iter().any(|c| c.level == Level::Fail) {
        Err(report)
    } else {
        Ok(report)
    }
}

/// One report line: `<tag> <name>: <detail>`.
fn line(c: &Check) -> String {
    let tag = match c.level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "FAIL",
    };
    format!("{tag} {}: {}", c.name, c.detail)
}

/// Formats the checks plus the summary line.
fn render(checks: &[Check]) -> String {
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    let mut lines: Vec<String> = checks.iter().map(line).collect();
    lines.push(format!(
        "doctor: {} ok, {} warn, {} fail",
        count(Level::Ok),
        count(Level::Warn),
        count(Level::Fail)
    ));
    lines.join("\n")
}

/// Checks that a device node opens read+write; `missing` is the level when it does not.
fn device(name: &'static str, path: &Path, missing: Level) -> Check {
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Check::new(Level::Ok, name, "readable and writable"),
        Err(e) => Check::new(
            missing,
            name,
            format!("cannot open read+write: {}", e.kind()),
        ),
    }
}

/// Version probes for every external tool.
fn tool_checks(config: &Config) -> Vec<Check> {
    let strict: [(&'static str, &Path, &str); 5] = [
        ("cloud-hypervisor", &config.cloud_hypervisor, "--version"),
        ("passt", &config.passt, "--version"),
        ("qemu-img", &config.qemu_img, "--version"),
        ("ssh", &config.ssh, "-V"),
        ("curl", &config.curl, "--version"),
    ];
    let mut checks: Vec<Check> = strict
        .iter()
        .map(|(name, bin, flag)| tool(name, bin, flag, true))
        .collect();
    checks.push(tool("ssh-keygen", &config.ssh_keygen, "-?", false));
    checks.push(tool("setsid", Path::new("setsid"), "--version", false));
    checks
}

/// First line of the tool output (stdout, else stderr), trimmed.
fn first_line(stdout: &[u8], stderr: &[u8]) -> String {
    let text = if stdout.is_empty() { stderr } else { stdout };
    String::from_utf8_lossy(text)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Runs `bin flag`; `need_success` makes a non-zero exit a failure.
fn tool(name: &'static str, bin: &Path, flag: &str, need_success: bool) -> Check {
    let output = Command::new(bin).arg(flag).stdin(Stdio::null()).output();
    match output {
        Ok(out) if out.status.success() || !need_success => {
            let version = first_line(&out.stdout, &out.stderr);
            if version.is_empty() || !need_success {
                Check::new(Level::Ok, name, "runnable")
            } else {
                Check::new(Level::Ok, name, version)
            }
        }
        _ => Check::new(
            Level::Fail,
            name,
            format!("configured {name} binary not runnable"),
        ),
    }
}

/// Creates `dir` and a probe file inside it.
fn writable(name: &'static str, dir: &Path) -> Check {
    let probe = dir.join(format!(".doctor-probe-{}", std::process::id()));
    let result = fs::create_dir_all(dir)
        .and_then(|()| fs::write(&probe, b"probe"))
        .and_then(|()| fs::remove_file(&probe));
    match result {
        Ok(()) => Check::new(Level::Ok, name, "writable"),
        Err(e) => Check::new(Level::Fail, name, format!("not writable: {}", e.kind())),
    }
}

/// Checks the longest socket path under the runtime dir fits `sun_path`.
fn socket_room(runtime_dir: &Path) -> Check {
    let fixed = 1 + RUN_DIR_NAME_LEN + 1 + LONGEST_SOCKET_NAME.len();
    let longest = runtime_dir.as_os_str().len() + fixed;
    let max_dir = SOCKET_PATH_MAX - 1 - fixed;
    if longest < SOCKET_PATH_MAX {
        Check::new(
            Level::Ok,
            "socket_path",
            format!("{longest} of {SOCKET_PATH_MAX} bytes"),
        )
    } else {
        Check::new(
            Level::Fail,
            "socket_path",
            format!(
                "runtime dir too long: sockets need {longest} bytes, limit {SOCKET_PATH_MAX}; runtimeDir must be at most {max_dir} bytes"
            ),
        )
    }
}

/// Reports the cache state of one downloaded asset.
fn cache(name: &'static str, path: &Path, asset: &Asset) -> Check {
    match cache_state(path, &asset.sha256) {
        CacheState::Verified => Check::new(Level::Ok, name, "verified"),
        CacheState::Present => Check::new(Level::Warn, name, "present but unverified"),
        CacheState::Missing => {
            Check::new(Level::Warn, name, "missing, will download on first acquire")
        }
    }
}

/// Reports whether the raw base image has been converted yet.
fn raw_base(config: &Config) -> Check {
    if base_raw_path(config).exists() {
        Check::new(Level::Ok, "image_raw", "present")
    } else {
        Check::new(
            Level::Warn,
            "image_raw",
            "missing, will convert on first acquire",
        )
    }
}
