//! Class sizing: the ladder, config overrides and host refusal.

#![allow(clippy::indexing_slicing, reason = "tests index known-key fixtures")]

use serde_json::json;

use super::{classes, default_classes, no_class_size, resolve, total_mib, Host, Size};

const BIG_HOST: Host = Host {
    total_cpus: 64,
    total_memory_mib: 1 << 20,
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
        assert_eq!(resolve(&table, class, &BIG_HOST), Ok(want));
    }
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
    assert_eq!(resolve(&table, "tiny", &BIG_HOST), Ok(size(1, 1024)));
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
        total_cpus: 8,
        total_memory_mib: 1 << 20,
    };
    assert_eq!(
        resolve(&default_classes(), "beast", &host),
        Err("class beast needs 16 vCPUs; this host has 8".to_owned())
    );
}

#[test]
fn too_much_memory_is_refused_not_shrunk() {
    let host = Host {
        total_cpus: 64,
        total_memory_mib: 10_000,
    };
    assert_eq!(
        resolve(&default_classes(), "large", &host),
        Err("class large needs 16384 MiB of memory; this host has 10000 MiB".to_owned())
    );
}

#[test]
fn unknown_class_lists_the_known_ones() {
    let err = resolve(&default_classes(), "huge", &BIG_HOST).unwrap_err();
    assert_eq!(
        err,
        "unknown class huge; known classes: beast, fast, large, standard"
    );
}

#[test]
fn reads_mem_total_in_mib_not_what_is_currently_available() {
    let text = "MemTotal:       16179200 kB\nMemAvailable:    2097152 kB\n";
    assert_eq!(total_mib(text), Ok(15800));
    assert!(total_mib("MemAvailable: 1 kB\n").is_err());
}

#[test]
fn admission_uses_host_totals_so_a_busy_host_still_admits() {
    let host = Host {
        total_cpus: 8,
        total_memory_mib: 16384,
    };
    assert_eq!(
        resolve(&default_classes(), "large", &host),
        Ok(size(8, 16384))
    );
}

fn host(cpus: u32, total_memory_mib: u64) -> Host {
    Host {
        total_cpus: cpus,
        total_memory_mib,
    }
}

#[test]
fn no_class_takes_host_cores_and_memory_less_the_reserve() {
    // 64 GiB host: reserve is 4 GiB plus 6553 MiB.
    let got = no_class_size(&host(16, 65536), None, None).unwrap();
    assert_eq!(got, size(16, 65536 - 4096 - 6553));
}

#[test]
fn no_class_refuses_a_host_too_small_to_hold_a_vm() {
    let err = no_class_size(&host(4, 4096), None, None).unwrap_err();
    assert!(err.contains("4096 MiB") && err.contains("reserve"), "{err}");
    let err = no_class_size(&host(4, 5000), None, None).unwrap_err();
    assert!(err.contains("5000 MiB"), "{err}");
}

#[test]
fn no_class_operator_override_wins_per_field() {
    let h = host(16, 65536);
    assert_eq!(
        no_class_size(&h, Some(3), None),
        Ok(size(3, 65536 - 4096 - 6553))
    );
    assert_eq!(no_class_size(&h, None, Some(2048)), Ok(size(16, 2048)));
    let tiny = host(1, 100);
    assert_eq!(no_class_size(&tiny, Some(2), Some(4096)), Ok(size(2, 4096)));
}

#[test]
fn a_class_ignores_the_host_ceiling() {
    let table = default_classes();
    assert_eq!(
        resolve(&table, "standard", &host(16, 65536)),
        Ok(size(2, 4096))
    );
}

#[test]
fn scope_limit_follows_the_ceiling() {
    let ceiling = no_class_size(&host(8, 32768), None, None).unwrap();
    let limits = crate::scope::Limits::for_vm(ceiling, 512);
    assert_eq!(limits.cpus, 8);
    assert_eq!(
        limits.memory_bytes,
        (32768 - 4096 - 3276 + 512) * 1024 * 1024
    );
}
