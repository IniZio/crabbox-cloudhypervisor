//! Host readiness checks for `doctor`: devices, tools, directories, caches.
//!
//! Details never echo configured paths or URLs; they name the check only.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::assets::{cache_state, CacheState};
use crate::config::{Asset, Config};
use crate::install::fix;
use crate::scope::{Cgroup, Scopes};
use crate::state::{LONGEST_SOCKET_NAME, RUN_DIR_NAME_LEN};
use crate::vm::spec::{base_raw_path, firmware_cache_path, image_cache_path};

mod checkpoints;
mod passt;
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
    /// This user's systemd manager, as scopes reach it.
    scopes: Scopes,
    /// `cgroup.controllers` of this user's `user@<uid>.service`.
    controllers: PathBuf,
    /// Host memory pressure file the balloon trigger registers on.
    pressure: PathBuf,
}

impl Probes {
    /// The real host device nodes.
    fn host(cgroup: Cgroup) -> Self {
        let uid = rustix::process::getuid().as_raw();
        Self {
            kvm: PathBuf::from("/dev/kvm"),
            vhost_vsock: PathBuf::from("/dev/vhost-vsock"),
            scopes: Scopes::current(cgroup),
            controllers: PathBuf::from(format!(
                "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
            )),
            pressure: PathBuf::from(crate::psi::MEMORY_PRESSURE),
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
    run_with(config, &Probes::host(config.cgroup), diag)
}

/// Like [`run`] with injectable device paths.
fn run_with(config: &Config, probes: &Probes, diag: &mut dyn Write) -> Result<String, String> {
    let mut checks = vec![
        kvm(&probes.kvm),
        device("vhost-vsock", &probes.vhost_vsock, Level::Warn),
    ];
    checks.extend(tool_checks(config));
    checks.push(passt::vhost_user(&config.passt));
    if config.checkpoints {
        checks.push(checkpoints::hypervisor(&config.cloud_hypervisor));
    }
    checks.extend(cgroup_checks(probes));
    checks.push(memory_pressure(config, probes));
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
    checks.extend(cache_checks(&config.cache.volume_dir));
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

/// Checks that VM scopes can be created and can carry CPU and memory limits.
fn cgroup_checks(probes: &Probes) -> Vec<Check> {
    if probes.scopes.mode == Cgroup::Off {
        return vec![Check::new(
            Level::Warn,
            "cgroup",
            "off: VMs run without CPU or memory limits",
        )];
    }
    let bus = match probes.scopes.check_bus() {
        Ok(()) => Check::new(Level::Ok, "user_bus", "systemd user manager reachable"),
        Err(e) => Check::new(Level::Fail, "user_bus", e),
    };
    let text = fs::read_to_string(&probes.controllers).unwrap_or_default();
    let missing: Vec<&str> = ["cpu", "memory"]
        .into_iter()
        .filter(|c| !text.split_whitespace().any(|have| have == *c))
        .collect();
    let delegated = if missing.is_empty() {
        Check::new(Level::Ok, "cgroup_delegation", "cpu and memory delegated")
    } else {
        Check::new(
            Level::Fail,
            "cgroup_delegation",
            format!(
                "{} not delegated to the user manager; add Delegate=cpu memory pids to a \
                 user@.service drop-in, or set cgroup: off",
                missing.join(" and ")
            ),
        )
    };
    vec![bus, delegated]
}

/// Checks that the configured memory pressure trigger registers, since the
/// balloon runs on it.
fn memory_pressure(config: &Config, probes: &Probes) -> Check {
    if !config.balloon.enabled {
        return Check::new(
            Level::Warn,
            "memory_pressure",
            "balloon off: guests keep their memory when the host runs short",
        );
    }
    match crate::psi::probe(&config.balloon.trigger, &probes.pressure) {
        Ok(()) => Check::new(Level::Ok, "memory_pressure", "pressure trigger registers"),
        Err(e) => Check::new(
            Level::Fail,
            "memory_pressure",
            format!("{e}; leases will not balloon and guests keep their memory"),
        ),
    }
}

/// `/dev/kvm`, with the group fix when this user cannot open it.
fn kvm(path: &Path) -> Check {
    let mut check = device("kvm", path, Level::Fail);
    if check.level == Level::Fail {
        check.detail +=
            "; add your user to the kvm group (sudo usermod -aG kvm $USER) and log in again";
    }
    check
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
    for name in ["rsync", "git"] {
        checks.push(optional(name, "--version", "crabbox cannot sync work"));
    }
    checks.push(dig_holes());
    checks
}

/// Without `fallocate --dig-holes` suspended snapshots stay dense; never a failure.
fn dig_holes() -> Check {
    let probe = Command::new(crate::host::DIG_HOLES_TOOL)
        .arg("--help")
        .stdin(Stdio::null())
        .output();
    let supported = probe.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--dig-holes"));
    if supported {
        Check::new(Level::Ok, "dig_holes", "fallocate --dig-holes available")
    } else {
        Check::new(
            Level::Warn,
            "dig_holes",
            "fallocate --dig-holes missing; suspended snapshots stay dense",
        )
    }
}

/// Cache volume tools and the store dir; a missing piece only degrades caching.
fn cache_checks(volume_dir: &Path) -> Vec<Check> {
    let mut checks: Vec<Check> = crate::volumes::ext4::TOOLS
        .iter()
        .map(|name| optional(name, "-V", "cache volumes are skipped"))
        .collect();
    checks.push(cache_store(volume_dir));
    checks
}

/// A tool on PATH whose absence only degrades `lost`.
fn optional(name: &'static str, flag: &str, lost: &str) -> Check {
    let mut c = tool(name, Path::new(name), flag, false);
    if c.level == Level::Fail {
        c.level = Level::Warn;
        c.detail = format!("{name} not runnable; {lost}; {}", fix(name));
    }
    c
}

/// Probes that the cache store can hard-link a generation into a lease's work
/// dir, which every overlay restore does.
fn cache_store(dir: &Path) -> Check {
    let src = dir.join(format!(".doctor-link-{}", std::process::id()));
    let mut dst = src.clone().into_os_string();
    dst.push(".link");
    let linked = crate::volumes::store::mkdir_private(dir)
        .and_then(|()| fs::write(&src, b"probe").map_err(|e| e.to_string()))
        .and_then(|()| fs::hard_link(&src, &dst).map_err(|e| e.to_string()));
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    match linked {
        Ok(()) => Check::new(Level::Ok, "cache_store", "hard links work"),
        Err(e) => Check::new(
            Level::Warn,
            "cache_store",
            format!("cacheVolumeDir cannot hard-link ({e}); cache volumes are skipped"),
        ),
    }
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
            format!("configured {name} binary not runnable; {}", fix(name)),
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
