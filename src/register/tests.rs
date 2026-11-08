#![allow(clippy::indexing_slicing, reason = "tests index known-shape YAML")]
//! Registration tests: every config lives in a temp XDG dir.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

use super::{config_path, register, status, Status};

const EXE: &str = "/opt/bin/crabbox-cloudhypervisor";

fn env_of(root: &Path) -> impl Fn(&str) -> Option<String> {
    let root = root.to_str().unwrap().to_owned();
    move |k| match k {
        "XDG_CONFIG_HOME" => Some(format!("{root}/config")),
        "HOME" => Some(root.clone()),
        _ => None,
    }
}

fn config_file(root: &Path) -> PathBuf {
    root.join("config/crabbox/config.yaml")
}

fn load(root: &Path) -> Yaml {
    let text = fs::read_to_string(config_file(root)).unwrap();
    YamlLoader::load_from_str(&text).unwrap().remove(0)
}

fn seed(root: &Path, text: &str) {
    let file = config_file(root);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn a_fresh_config_selects_the_adapter_privately() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_of(dir.path());
    let report = register(&env, Path::new(EXE)).unwrap();
    assert!(report.contains("registered"), "{report}");
    let doc = load(dir.path());
    assert_eq!(doc["provider"].as_str(), Some("external"));
    assert_eq!(doc["target"].as_str(), Some("linux"));
    assert_eq!(doc["external"]["command"].as_str(), Some(EXE));
    assert_eq!(
        doc["external"]["capabilities"]["idempotentLeaseId"].as_bool(),
        Some(true)
    );
    assert_eq!(
        doc["external"]["capabilities"]["checkpoints"].as_bool(),
        Some(true)
    );
    assert_eq!(
        doc["external"]["connection"]["ssh"]["trustProviderOutput"].as_bool(),
        Some(true)
    );
    assert_eq!(status(&env, Path::new(EXE)).unwrap(), Status::Registered);
}

#[test]
fn the_file_is_private_and_its_parent_not_group_writable() {
    let dir = tempfile::tempdir().unwrap();
    register(&env_of(dir.path()), Path::new(EXE)).unwrap();
    let file = config_file(dir.path());
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()) & 0o022, 0);
}

#[test]
fn unrelated_keys_survive() {
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        "broker:\n  url: https://example.test\nexternal:\n  workRoot: /work\n  capabilities:\n    pauseResume: true\n  config:\n    cpus: 2\n",
    );
    register(&env_of(dir.path()), Path::new(EXE)).unwrap();
    let doc = load(dir.path());
    assert_eq!(doc["broker"]["url"].as_str(), Some("https://example.test"));
    assert_eq!(doc["external"]["workRoot"].as_str(), Some("/work"));
    assert_eq!(
        doc["external"]["capabilities"]["pauseResume"].as_bool(),
        Some(true)
    );
    assert_eq!(doc["external"]["config"]["cpus"].as_i64(), Some(2));
    assert_eq!(doc["external"]["command"].as_str(), Some(EXE));
}

#[test]
fn a_second_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_of(dir.path());
    register(&env, Path::new(EXE)).unwrap();
    let file = config_file(dir.path());
    fs::write(
        &file,
        format!("# mine\n{}", fs::read_to_string(&file).unwrap()),
    )
    .unwrap();
    let before = fs::read(&file).unwrap();
    let report = register(&env, Path::new(EXE)).unwrap();
    assert!(report.contains("already"), "{report}");
    assert_eq!(fs::read(&file).unwrap(), before);
}

#[test]
fn another_provider_is_never_overwritten() {
    for text in [
        "provider: aws\n",
        "provider: external\nexternal:\n  command: /usr/bin/other\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), text);
        let err = register(&env_of(dir.path()), Path::new(EXE)).unwrap_err();
        assert!(err.contains("provider"), "{err}");
        assert!(err.contains(EXE), "{err}");
        assert_eq!(fs::read_to_string(config_file(dir.path())).unwrap(), text);
    }
}

#[test]
fn crabbox_config_names_the_file() {
    let env = |k: &str| match k {
        "CRABBOX_CONFIG" => Some("/etc/x/c.yaml".to_owned()),
        "XDG_CONFIG_HOME" => Some("/ignored".to_owned()),
        _ => None,
    };
    assert_eq!(config_path(&env).unwrap(), PathBuf::from("/etc/x/c.yaml"));
}

#[test]
fn status_tells_unset_from_other() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_of(dir.path());
    assert_eq!(status(&env, Path::new(EXE)).unwrap(), Status::Unregistered);
    seed(dir.path(), "provider: hetzner\n");
    assert!(matches!(
        status(&env, Path::new(EXE)).unwrap(),
        Status::Other(_)
    ));
}
