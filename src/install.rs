//! `install`: puts the pinned cloud-hypervisor and passt in the user's data
//! dir, where the config finds them without any keys set.

use std::ffi::OsStr;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::assets::sha256_file;
use crate::config::{is_sha256, xdg_dir, Env};

#[cfg(test)]
mod tests;

/// Release of the patched cloud-hypervisor that checkpoints need.
const CH_VERSION: &str = "v52.0-moor.1";
/// SHA-256 of that release's `x86_64` static binary.
const CH_SHA256: &str = "f0052b204ea9e22b9428845ff6f22b6f7738d1b49e29e00b7e3a2856ab1f098c";
/// Where this adapter's releases, and the passt built with each, are published.
const RELEASES: &str = "https://github.com/IniZio/crabbox-cloudhypervisor/releases/download";

/// Host tools and the Debian/Ubuntu package that ships each.
const PACKAGES: [(&str, &str); 10] = [
    ("qemu-img", "qemu-utils"),
    ("mke2fs", "e2fsprogs"),
    ("e2fsck", "e2fsprogs"),
    ("resize2fs", "e2fsprogs"),
    ("ssh", "openssh-client"),
    ("ssh-keygen", "openssh-client"),
    ("rsync", "rsync"),
    ("git", "git"),
    ("curl", "curl"),
    ("setsid", "util-linux"),
];

/// How a download's expected SHA-256 is known.
#[derive(Debug, Clone)]
pub enum Checksum {
    /// Fixed in this binary.
    Pinned(String),
    /// Read from a `.sha256` file published beside the download.
    Published(String),
}

/// One binary `install` puts in place.
#[derive(Debug, Clone)]
pub struct Pin {
    /// File name in the bin dir, which is also the config default.
    pub name: &'static str,
    /// Version shown in the report.
    pub version: String,
    /// Download URL.
    pub url: String,
    /// Expected digest.
    pub sha256: Checksum,
}

/// The bin dir `install` writes and the config defaults read.
pub fn bin_dir(env: Env<'_>) -> Result<PathBuf, String> {
    Ok(xdg_dir(env, "XDG_DATA_HOME", ".local/share")?.join("bin"))
}

/// The binaries this adapter version installs on `arch`.
pub fn pins(arch: &str) -> Result<Vec<Pin>, String> {
    if arch != "x86_64" {
        return Err(format!(
            "cloud-hypervisor {CH_VERSION} has no {arch} build; install cloud-hypervisor and \
             passt yourself and set cloudHypervisor and passt in the provider config"
        ));
    }
    let release = format!("v{}", env!("CARGO_PKG_VERSION"));
    let passt = format!("{RELEASES}/{release}/passt-{arch}-linux");
    Ok(vec![
        Pin {
            name: "cloud-hypervisor",
            version: CH_VERSION.to_owned(),
            url: format!(
                "https://github.com/IniZio/cloud-hypervisor/releases/download/{CH_VERSION}/cloud-hypervisor-static"
            ),
            sha256: Checksum::Pinned(CH_SHA256.to_owned()),
        },
        Pin {
            name: "passt",
            version: release,
            sha256: Checksum::Published(format!("{passt}.sha256")),
            url: passt,
        },
    ])
}

/// Installs every pin into `dir`, skipping those already there and intact.
pub fn install(pins: &[Pin], dir: &Path, curl: &Path) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|e| format!("create the bin dir: {e}"))?;
    let mut lines = Vec::new();
    for pin in pins {
        match install_one(pin, dir, curl) {
            Ok(line) => lines.push(line),
            Err(e) => {
                lines.push(e);
                return Err(lines.join("\n"));
            }
        }
    }
    Ok(lines.join("\n"))
}

/// Installs one pin; a failed download or mismatch leaves nothing behind.
fn install_one(pin: &Pin, dir: &Path, curl: &Path) -> Result<String, String> {
    let label = format!("{} {}", pin.name, pin.version);
    let part = dir.join(format!("{}.part", pin.name));
    let want = match &pin.sha256 {
        Checksum::Pinned(sha) => sha.clone(),
        Checksum::Published(url) => published(url, &part, curl, &label)?,
    };
    let dest = dir.join(pin.name);
    if sha256_file(&dest).is_ok_and(|have| have == want) {
        return Ok(format!("{label} already installed"));
    }
    let placed = download(curl, &pin.url, &part, &label).and_then(|()| {
        let got = sha256_file(&part)?;
        if got != want {
            return Err(format!(
                "{label}: checksum mismatch: expected {want}, got {got}"
            ));
        }
        fs::set_permissions(&part, Permissions::from_mode(0o755))
            .and_then(|()| fs::rename(&part, &dest))
            .map_err(|e| format!("{label}: put in place: {e}"))
    });
    if placed.is_err() {
        let _ = fs::remove_file(&part);
    }
    placed.map(|()| format!("installed {label}"))
}

/// Fetches a published `.sha256` file and returns its digest.
fn published(url: &str, part: &Path, curl: &Path, label: &str) -> Result<String, String> {
    let fetched = download(curl, url, part, label).and_then(|()| {
        fs::read_to_string(part).map_err(|e| format!("{label}: read checksum: {e}"))
    });
    let _ = fs::remove_file(part);
    let text = fetched?;
    let sha = text.split_whitespace().next().unwrap_or_default();
    if is_sha256(sha) {
        Ok(sha.to_owned())
    } else {
        Err(format!("{label}: {url} does not hold a SHA-256"))
    }
}

/// Downloads `url` to `dest` with curl, telling an unpublished asset apart.
fn download(curl: &Path, url: &str, dest: &Path, label: &str) -> Result<(), String> {
    let out = Command::new(curl)
        .args(["-sSL", "--proto", "=https,http", "--max-time", "600"])
        .args(["-w", "%{http_code}", "-o"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{label}: cannot run curl ({e}); {}", fix("curl")))?;
    let code = String::from_utf8_lossy(&out.stdout);
    match (out.status.success(), code.trim()) {
        (true, "200") => Ok(()),
        (_, "404") => Err(format!("{label} is not published at {url}")),
        _ => Err(format!(
            "{label}: download of {url} failed (HTTP {code}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Host tools missing from every directory of `path`, with their package.
#[must_use]
pub fn missing_packages(path: &OsStr) -> Vec<(&'static str, &'static str)> {
    PACKAGES
        .into_iter()
        .filter(|(tool, _)| !std::env::split_paths(path).any(|d| d.join(tool).is_file()))
        .collect()
}

/// What to do about a missing or broken tool.
#[must_use]
pub fn fix(tool: &str) -> String {
    if matches!(tool, "cloud-hypervisor" | "passt") {
        return "run `crabbox-cloudhypervisor install`".to_owned();
    }
    PACKAGES.iter().find(|(t, _)| *t == tool).map_or_else(
        || format!("install {tool} or set its path in the provider config"),
        |(_, package)| format!("install the {package} package"),
    )
}

/// `crabbox-cloudhypervisor install`: the pinned binaries, then what the host still lacks.
pub fn run(env: Env<'_>) -> Result<String, String> {
    let dir = bin_dir(env)?;
    let mut lines = vec![
        install(&pins(std::env::consts::ARCH)?, &dir, Path::new("curl"))?,
        format!("binaries in {}", dir.display()),
    ];
    for (tool, _) in missing_packages(OsStr::new(&env("PATH").unwrap_or_default())) {
        lines.push(format!("missing {tool}: {}", fix(tool)));
    }
    lines.push("next: crabbox-cloudhypervisor doctor".to_owned());
    Ok(lines.join("\n"))
}
