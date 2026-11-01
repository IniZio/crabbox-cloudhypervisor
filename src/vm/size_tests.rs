//! Class sizing in the VM config and at create.

#![allow(clippy::indexing_slicing, reason = "tests index known-key fixtures")]

use std::path::{Path, PathBuf};

use serde_json::json;

use super::spec;
use super::tests::{config, record};
use super::RealBackend;
use crate::backend::Backend;
use crate::config::Config;
use crate::protocol::Desired;
use crate::size::Size;

/// The size a lease with no class boots at.
pub(super) fn size_of(cfg: &Config) -> Size {
    Size {
        cpus: cfg.cpus,
        memory_mib: cfg.memory_mib,
    }
}

#[test]
fn class_size_replaces_config_size() {
    let p = Path::new("/x");
    let big = Size {
        cpus: 16,
        memory_mib: 32768,
    };
    let got = spec::vm_config(
        big,
        spec::VmPaths {
            firmware: p,
            disk: p,
            seed: p,
            serial: p,
        },
        &record("abc"),
    );
    assert_eq!(got["cpus"], json!({"boot_vcpus": 16, "max_vcpus": 16}));
    assert_eq!(
        got["memory"],
        json!({"size": 32768_u64 * 1024 * 1024, "shared": true})
    );
}

#[test]
fn create_refuses_an_unknown_class_before_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let backend = RealBackend::new(cfg.clone(), PathBuf::from("/bin/cb"));
    let desired = Desired {
        lease_id: "lease1".to_owned(),
        slug: "s".to_owned(),
        name: "n".to_owned(),
        class: Some("huge".to_owned()),
        cache_volumes: Vec::new(),
    };
    let err = backend.create(&desired, &mut Vec::new()).unwrap_err();
    assert!(
        err.contains("unknown class huge") && err.contains("beast"),
        "{err}"
    );
    assert!(!cfg.state_dir.exists() && !cfg.cache_dir.exists());
}
