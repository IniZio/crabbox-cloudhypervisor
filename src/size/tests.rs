//! Class sizing: the ladder, config overrides and host refusal.

#![allow(clippy::indexing_slicing, reason = "tests index known-key fixtures")]

use serde_json::json;

use super::{available_mib, classes, default_classes, resolve, Host, Size};

const DEFAULT: Size = Size {
    cpus: 2,
    memory_mib: 4096,
};
const BIG_HOST: Host = Host {
    cpus: 64,
    free_memory_mib: 1 << 20,
};

fn size(cpus: u32, memory_mib: u64) -> Size {
    Size { cpus, memory_mib }
}

#[test]
fn ladder_maps_each_class() {
    let table = default_classes();
    for (class, want) in [
        ("standard", size(2, 4096)),
        ("fast", size(4, 8192)),
        ("large", size(8, 16384)),
        ("beast", size(16, 32768)),
    ] {
        assert_eq!(resolve(&table, DEFAULT, Some(class), &BIG_HOST), Ok(want));
    }
}

#[test]
fn no_class_uses_the_default_even_on_a_tiny_host() {
    let tiny = Host {
        cpus: 1,
        free_memory_mib: 100,
    };
    assert_eq!(
        resolve(&default_classes(), DEFAULT, None, &tiny),
        Ok(DEFAULT)
    );
}

#[test]
fn config_overrides_one_class_and_keeps_the_rest() {
    let table = classes(&json!({"classes": {"fast": {"cpus": 6, "memoryMiB": 12288}}})).unwrap();
    assert_eq!(table["fast"], size(6, 12288));
    assert_eq!(table["beast"], size(16, 32768));
}

#[test]
fn config_can_add_a_class() {
    let table = classes(&json!({"classes": {"tiny": {"cpus": 1, "memoryMiB": 1024}}})).unwrap();
    assert_eq!(
        resolve(&table, DEFAULT, Some("tiny"), &BIG_HOST),
        Ok(size(1, 1024))
    );
}

#[test]
fn absent_or_null_classes_is_the_ladder() {
    assert_eq!(classes(&json!({})).unwrap(), default_classes());
    assert_eq!(
        classes(&json!({"classes": null})).unwrap(),
        default_classes()
    );
}

#[test]
fn bad_class_entries_are_named() {
    for (cfg, want) in [
        (json!({"classes": []}), "config.classes must be an object"),
        (
            json!({"classes": {"fast": 4}}),
            "config.classes.fast must be an object",
        ),
        (
            json!({"classes": {"fast": {"memoryMiB": 8192}}}),
            "config.classes.fast.cpus",
        ),
        (
            json!({"classes": {"fast": {"cpus": 65, "memoryMiB": 8192}}}),
            "config.classes.fast.cpus",
        ),
        (
            json!({"classes": {"fast": {"cpus": 4, "memoryMiB": 64}}}),
            "config.classes.fast.memoryMiB",
        ),
    ] {
        let err = classes(&cfg).unwrap_err();
        assert!(err.contains(want), "{err}");
    }
}

#[test]
fn too_many_vcpus_is_refused_not_shrunk() {
    let host = Host {
        cpus: 8,
        free_memory_mib: 1 << 20,
    };
    assert_eq!(
        resolve(&default_classes(), DEFAULT, Some("beast"), &host),
        Err("class beast needs 16 vCPUs; this host has 8".to_owned())
    );
}

#[test]
fn too_much_memory_is_refused_not_shrunk() {
    let host = Host {
        cpus: 64,
        free_memory_mib: 10_000,
    };
    assert_eq!(
        resolve(&default_classes(), DEFAULT, Some("large"), &host),
        Err("class large needs 16384 MiB of memory; this host has 10000 MiB free".to_owned())
    );
}

#[test]
fn unknown_class_lists_the_known_ones() {
    let err = resolve(&default_classes(), DEFAULT, Some("huge"), &BIG_HOST).unwrap_err();
    assert_eq!(
        err,
        "unknown class huge; known classes: beast, fast, large, standard"
    );
}

#[test]
fn reads_mem_available_in_mib() {
    let text = "MemTotal:       32000000 kB\nMemAvailable:    2097152 kB\n";
    assert_eq!(available_mib(text), Ok(2048));
    assert!(available_mib("MemTotal: 1 kB\n").is_err());
}
