//! Cache volumes: which groups a lease gets, and what happened to each name.
//!
//! A group is one ext4 image in the store; each volume `name` is a directory
//! inside it, bind-mounted to every path the volume declares.

pub mod ext4;
pub mod store;

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::CacheConfig;
use crate::protocol::{CacheVolumeEcho, CacheVolumeRequest};

#[cfg(test)]
pub(crate) mod tests;

/// Where group disks are mounted in the guest.
pub const MOUNT_ROOT: &str = "/var/lib/crabbox-cache";
/// The login user's home; `~/` paths resolve here.
pub const LOGIN_HOME: &str = "/home/crabbox";
/// Uid of the login user (the first user cloud-init creates).
pub const LOGIN_UID: u32 = 1000;
/// Gid of the login user.
pub const LOGIN_GID: u32 = 1000;
/// File the guest leaves in each path of a sealed volume once it sealed it.
pub const SEAL_MARKER: &str = ".crabbox-sealed";
/// Bytes per GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;
/// Group size when no volume of the group declares one.
pub const DEFAULT_GROUP_GIB: u64 = 20;
/// Longest group, name or key.
const MAX_TOKEN: usize = 96;
/// Characters allowed in a path component besides ASCII alphanumerics.
const PATH_PUNCT: &str = "._@+-";
/// Absolute roots a volume may sit strictly below.
const BELOW_ROOTS: [&str; 2] = ["/opt", "/var/cache"];
/// Absolute roots a volume may be, or sit below.
const AT_ROOTS: [&str; 2] = ["/var/lib/containerd", "/var/lib/containers"];
/// Home directories never cached: they hold credentials.
const HOME_REFUSED: [&str; 1] = [".ssh"];

/// A group id, volume name or key: `[A-Za-z0-9._-]{1,96}`, never `.` or `..`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    /// Validates `s`; the only way a request string reaches a file name.
    pub fn parse(s: &str) -> Result<Self, String> {
        let charset = s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if s.is_empty() || s.len() > MAX_TOKEN || !charset || s == "." || s == ".." {
            return Err("must be 1 to 96 characters of [A-Za-z0-9._-]".to_owned());
        }
        Ok(Self(s.to_owned()))
    }

    /// The token text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A guest path: `~/<rel>` stays symbolic until the login home is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestPath {
    /// Relative to the login user's home.
    Home(String),
    /// Absolute.
    Abs(String),
}

impl GuestPath {
    /// Parses `~/<rel>` (not `~/.ssh`), `/opt/<rel>`, `/var/cache/<rel>`, or
    /// `/var/lib/containerd` and `/var/lib/containers` and below. Components
    /// are `[A-Za-z0-9._@+-]`, never `.` or `..`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let (path, rest) = match s.strip_prefix("~/") {
            Some(rel) => (Self::Home(rel.to_owned()), rel),
            None => match s.strip_prefix('/') {
                Some(rel) => (Self::Abs(s.to_owned()), rel),
                None => return Err("must start with ~/ or /".to_owned()),
            },
        };
        let component_ok = |c: &str| {
            !c.is_empty()
                && c != "."
                && c != ".."
                && c.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || PATH_PUNCT.contains(ch))
        };
        if !rest.split('/').all(component_ok) {
            return Err("has an empty, dot or unsupported component".to_owned());
        }
        let below = |root: &str, p: &str| {
            p.strip_prefix(root)
                .is_some_and(|r| r.len() > 1 && r.starts_with('/'))
        };
        let allowed = match &path {
            Self::Home(rel) => !HOME_REFUSED.iter().any(|r| rel == r || below(r, rel)),
            Self::Abs(abs) => {
                BELOW_ROOTS.iter().any(|r| below(r, abs))
                    || AT_ROOTS.iter().any(|r| abs == r || below(r, abs))
            }
        };
        if !allowed {
            return Err(
                "must be under ~/ (not ~/.ssh), /opt/, /var/cache/, /var/lib/containerd or /var/lib/containers"
                    .to_owned(),
            );
        }
        Ok(path)
    }

    /// The absolute guest path, with `~/` resolved against `home`.
    #[must_use]
    pub fn resolve(&self, home: &str) -> String {
        match self {
            Self::Home(rel) => format!("{home}/{rel}"),
            Self::Abs(abs) => abs.clone(),
        }
    }
}

/// One declared volume of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// Directory name inside the group.
    pub name: Token,
    /// Guest paths bound to it, at least one.
    pub paths: Vec<GuestPath>,
    /// The guest seals it at close and leaves a marker in each path.
    pub sealed: bool,
}

impl Volume {
    /// Directory of each path inside the group filesystem: `/<name>/<escaped target>`.
    #[must_use]
    pub fn image_dirs(&self) -> Vec<String> {
        self.paths
            .iter()
            .map(|p| {
                format!(
                    "/{}/{}",
                    self.name.as_str(),
                    escape_path(&p.resolve(LOGIN_HOME))
                )
            })
            .collect()
    }
}

/// `systemd-escape --path`: `/` becomes `-`, other bytes outside `[A-Za-z0-9:_.]`
/// (and a leading `.`) become `\xNN`.
#[must_use]
pub fn escape_path(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return "-".to_owned();
    }
    let mut out = String::new();
    for (i, b) in trimmed.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i > 0 => out.push('.'),
            b if b.is_ascii_alphanumeric() || b == b':' || b == b'_' => out.push(char::from(b)),
            b => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

/// One store image and the volumes inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Store key of the image.
    pub id: Token,
    /// Volumes in request order.
    pub volumes: Vec<Volume>,
    /// Filesystem size in bytes.
    pub capacity: u64,
}

impl Group {
    /// `(volume name, marker path in the image)` for each path of each sealed volume.
    #[must_use]
    pub fn seal_markers(&self) -> Vec<(&str, String)> {
        self.volumes
            .iter()
            .filter(|v| v.sealed)
            .flat_map(|v| {
                v.image_dirs()
                    .into_iter()
                    .map(|d| (v.name.as_str(), format!("{d}/{SEAL_MARKER}")))
            })
            .collect()
    }
}

/// What acquire did for a group; every name of the group shares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum Outcome {
    /// A private copy of a published generation.
    Restored {
        /// Generation timestamp, unix nanoseconds.
        generation: u64,
    },
    /// A fresh empty filesystem.
    Empty,
    /// No disk; the paths are plain directories of the root disk.
    Skipped {
        /// Why.
        reason: String,
    },
}

impl Outcome {
    /// The wire echo for volume `name`.
    #[must_use]
    pub fn echo(&self, name: &str) -> CacheVolumeEcho {
        let (outcome, detail) = match self {
            Self::Restored { generation } => {
                ("restored", format!("gen {}", generation / 1_000_000_000))
            }
            Self::Empty => ("empty", "no generation yet".to_owned()),
            Self::Skipped { reason } => ("skipped", reason.clone()),
        };
        CacheVolumeEcho {
            name: name.to_owned(),
            outcome: outcome.to_owned(),
            detail,
        }
    }
}

/// The groups a lease gets, plus the requested names that were refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Valid groups in first-seen order.
    pub groups: Vec<Group>,
    /// `(name as sent, reason)` for refused volumes.
    pub skipped: Vec<(String, String)>,
}

/// Group size: the larger of the declared sum and the default, clamped to the max.
#[must_use]
pub fn capacity(declared_gib: &[u64], max_gib: u64) -> u64 {
    let sum = declared_gib.iter().fold(0u64, |a, b| a.saturating_add(*b));
    sum.max(DEFAULT_GROUP_GIB).min(max_gib).saturating_mul(GIB)
}

/// A request entry as a typed volume, or why it is refused.
fn parse_entry(req: &CacheVolumeRequest) -> Result<(Token, Volume), String> {
    let group = Token::parse(&req.group).map_err(|e| format!("group {e}"))?;
    let name = Token::parse(&req.name).map_err(|e| format!("name {e}"))?;
    Token::parse(&req.key).map_err(|e| format!("key {e}"))?;
    if req.paths.is_empty() {
        return Err("no paths".to_owned());
    }
    let paths = req
        .paths
        .iter()
        .map(|p| GuestPath::parse(p).map_err(|e| format!("path {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        group,
        Volume {
            name,
            paths,
            sealed: req.sealed,
        },
    ))
}

/// Groups the request's volumes; a bad or duplicate entry is skipped, never an error.
#[must_use]
pub fn plan(requests: &[CacheVolumeRequest], limits: &CacheConfig) -> Plan {
    let mut out = Plan::default();
    let mut declared: Vec<Vec<u64>> = Vec::new();
    let mut seen_paths: Vec<GuestPath> = Vec::new();
    for req in requests {
        let (group, volume) = match parse_entry(req) {
            Ok(entry) => entry,
            Err(reason) => {
                out.skipped.push((req.name.clone(), reason));
                continue;
            }
        };
        let index = out.groups.iter().position(|g| g.id == group);
        let taken = index.is_some_and(|i| {
            out.groups
                .get(i)
                .is_some_and(|g| g.volumes.iter().any(|v| v.name == volume.name))
        });
        if taken {
            out.skipped
                .push((req.name.clone(), "duplicate name in group".to_owned()));
            continue;
        }
        if volume.paths.iter().any(|p| seen_paths.contains(p)) {
            out.skipped
                .push((req.name.clone(), "path already cached".to_owned()));
            continue;
        }
        seen_paths.extend(volume.paths.iter().cloned());
        let i = index.unwrap_or_else(|| {
            out.groups.push(Group {
                id: group,
                volumes: Vec::new(),
                capacity: 0,
            });
            declared.push(Vec::new());
            out.groups.len() - 1
        });
        if let (Some(g), Some(d)) = (out.groups.get_mut(i), declared.get_mut(i)) {
            g.volumes.push(volume);
            d.push(req.size_gb);
        }
    }
    for (g, d) in out.groups.iter_mut().zip(&declared) {
        g.capacity = capacity(d, limits.max_volume_gib);
    }
    out
}
