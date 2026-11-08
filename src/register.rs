//! Selects this adapter in crabbox's trusted user config, so crabbox core
//! never has to know about cloud-hypervisor.

use std::fs::{self, DirBuilder, Permissions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::config::Env;

#[cfg(test)]
mod tests;

/// Where a config stands relative to this adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Selects this adapter with every key it needs.
    Registered,
    /// Selects no provider, or this adapter with keys missing.
    Unregistered,
    /// Selects something else; the text says what to change.
    Other(String),
}

/// Keys under `external.capabilities` that this adapter needs on.
const CAPABILITIES: [&str; 2] = ["idempotentLeaseId", "checkpoints"];

/// The file crabbox reads as its user config: `$CRABBOX_CONFIG`, else
/// `$XDG_CONFIG_HOME/crabbox/config.yaml`, else `$HOME/.config/crabbox/config.yaml`.
pub fn config_path(env: Env<'_>) -> Result<PathBuf, String> {
    if let Some(explicit) = env("CRABBOX_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    let base = abs("XDG_CONFIG_HOME")
        .or_else(|| abs("HOME").map(|h| h.join(".config")))
        .ok_or("crabbox config: set CRABBOX_CONFIG, XDG_CONFIG_HOME or HOME to an absolute path")?;
    Ok(base.join("crabbox").join("config.yaml"))
}

/// Reads the config as a mapping; a missing or empty file is an empty one.
fn load(path: &Path) -> Result<Hash, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let docs =
        YamlLoader::load_from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    match docs.into_iter().next() {
        None | Some(Yaml::Null) => Ok(Hash::new()),
        Some(Yaml::Hash(h)) => Ok(h),
        Some(_) => Err(format!("{} is not a YAML mapping", path.display())),
    }
}

/// The value at `keys` below `map`.
fn dig<'a>(map: &'a Hash, keys: &[&str]) -> Option<&'a Yaml> {
    let (first, rest) = keys.split_first()?;
    let value = map.get(&key(first))?;
    if rest.is_empty() {
        return Some(value);
    }
    match value {
        Yaml::Hash(inner) => dig(inner, rest),
        _ => None,
    }
}

/// A string key.
fn key(name: &str) -> Yaml {
    Yaml::String(name.to_owned())
}

/// Classifies `map` against the adapter at `exe`.
fn classify(map: &Hash, exe: &str) -> Status {
    let provider = dig(map, &["provider"]).and_then(Yaml::as_str);
    let command = dig(map, &["external", "command"]).and_then(Yaml::as_str);
    let ours = command == Some(exe);
    let change = format!(
        "change `provider: {}` to `provider: external` and set `external.command: {exe}`",
        provider.unwrap_or("external")
    );
    if provider.is_some_and(|p| p != "external") || command.is_some_and(|c| c != exe) {
        return Status::Other(change);
    }
    let on = |keys: &[&str]| dig(map, keys).and_then(Yaml::as_bool) == Some(true);
    let complete = provider.is_some()
        && ours
        && CAPABILITIES
            .iter()
            .all(|c| on(&["external", "capabilities", c]))
        && on(&["external", "connection", "ssh", "trustProviderOutput"]);
    if complete {
        Status::Registered
    } else {
        Status::Unregistered
    }
}

/// Where the user's crabbox config stands for the adapter at `exe`.
pub fn status(env: Env<'_>, exe: &Path) -> Result<Status, String> {
    let exe = exe.to_str().ok_or("adapter path is not UTF-8")?;
    Ok(classify(&load(&config_path(env)?)?, exe))
}

/// The mapping under `name`, created when absent.
fn section<'a>(map: &'a mut Hash, name: &str) -> Result<&'a mut Hash, String> {
    match map
        .entry(key(name))
        .or_insert_with(|| Yaml::Hash(Hash::new()))
    {
        Yaml::Hash(inner) => Ok(inner),
        _ => Err(format!(
            "crabbox config: `{name}` is not a mapping; fix it by hand"
        )),
    }
}

/// Sets the adapter's keys, leaving every other key as it was.
fn apply(map: &mut Hash, exe: &str) -> Result<(), String> {
    map.insert(key("provider"), key("external"));
    map.entry(key("target")).or_insert_with(|| key("linux"));
    let external = section(map, "external")?;
    external.insert(key("command"), key(exe));
    let capabilities = section(external, "capabilities")?;
    for name in CAPABILITIES {
        capabilities.insert(key(name), Yaml::Boolean(true));
    }
    let ssh = section(section(external, "connection")?, "ssh")?;
    ssh.insert(key("trustProviderOutput"), Yaml::Boolean(true));
    Ok(())
}

/// Renders the mapping as one YAML document.
fn render(map: Hash) -> Result<String, String> {
    let mut text = String::new();
    YamlEmitter::new(&mut text)
        .dump(&Yaml::Hash(map))
        .map_err(|e| format!("render crabbox config: {e}"))?;
    text.push('\n');
    Ok(text
        .strip_prefix("---\n")
        .map(str::to_owned)
        .unwrap_or(text))
}

/// Writes `text` to `path` with mode 0600 through a rename, in a parent that only
/// its owner can write when this call creates it.
fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or("crabbox config has no parent directory")?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| format!("create {}: {e}", parent.display()))?;
    let part = path.with_extension("yaml.part");
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&part)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .and_then(|()| fs::set_permissions(&part, Permissions::from_mode(0o600)))
        .and_then(|()| fs::rename(&part, path));
    if written.is_err() {
        let _ = fs::remove_file(&part);
    }
    written.map_err(|e| format!("write {}: {e}", path.display()))
}

/// Registers the adapter at `exe` in crabbox's user config. Refuses, naming the
/// one change to make, when the config selects another provider.
pub fn register(env: Env<'_>, exe: &Path) -> Result<String, String> {
    let path = config_path(env)?;
    let exe = exe.to_str().ok_or("adapter path is not UTF-8")?;
    let mut map = load(&path)?;
    match classify(&map, exe) {
        Status::Registered => Ok(format!("already registered in {}", path.display())),
        Status::Other(change) => Err(format!(
            "{} selects another provider: {change}",
            path.display()
        )),
        Status::Unregistered => {
            apply(&mut map, exe)?;
            write_private(&path, &render(map)?)?;
            Ok(format!("registered in {}", path.display()))
        }
    }
}
