use super::*;
use serde_json::json;
use std::collections::HashMap;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    move |k| map.get(k).cloned()
}

fn parse(config: &Value, pairs: &[(&str, &str)]) -> Result<Config, String> {
    Config::from_request(config, &env_of(pairs))
}

const HOME: &[(&str, &str)] = &[("HOME", "/home/u")];

/// Builds an expected config; binaries are bare names unless overridden.
fn expected(state: &str, runtime: &str, cache: &str) -> Config {
    Config {
        state_dir: PathBuf::from(state),
        runtime_dir: PathBuf::from(runtime),
        cache_dir: PathBuf::from(cache),
        cpus: 2,
        memory_mib: 4096,
        disk_gib: 20,
        image: Asset {
            url: IMAGE_URL.to_owned(),
            sha256: IMAGE_SHA256.to_owned(),
        },
        firmware: Asset {
            url: FIRMWARE_URL.to_owned(),
            sha256: FIRMWARE_SHA256.to_owned(),
        },
        cloud_hypervisor: PathBuf::from("cloud-hypervisor"),
        passt: PathBuf::from("passt"),
        qemu_img: PathBuf::from("qemu-img"),
        ssh: PathBuf::from("ssh"),
        ssh_keygen: PathBuf::from("ssh-keygen"),
        curl: PathBuf::from("curl"),
        ready_timeout: Duration::from_secs(300),
    }
}

#[test]
fn defaults_with_home() {
    let state = "/home/u/.local/state/crabbox-cloudhypervisor";
    let want = expected(
        state,
        "/home/u/.local/state/crabbox-cloudhypervisor/run",
        "/home/u/.cache/crabbox-cloudhypervisor",
    );
    assert_eq!(parse(&Value::Null, HOME).unwrap(), want);
}

#[test]
fn empty_object_equals_null() {
    assert_eq!(
        parse(&json!({}), HOME).unwrap(),
        parse(&Value::Null, HOME).unwrap()
    );
}

#[test]
fn xdg_dirs_win_over_home() {
    let env = [
        ("HOME", "/home/u"),
        ("XDG_STATE_HOME", "/x/state"),
        ("XDG_CACHE_HOME", "/x/cache"),
        ("XDG_RUNTIME_DIR", "/run/user/1"),
    ];
    let c = parse(&Value::Null, &env).unwrap();
    assert_eq!(
        c.state_dir,
        PathBuf::from("/x/state/crabbox-cloudhypervisor")
    );
    assert_eq!(
        c.cache_dir,
        PathBuf::from("/x/cache/crabbox-cloudhypervisor")
    );
    assert_eq!(
        c.runtime_dir,
        PathBuf::from("/run/user/1/crabbox-cloudhypervisor")
    );
}

#[test]
fn relative_xdg_ignored() {
    let env = [
        ("HOME", "/home/u"),
        ("XDG_STATE_HOME", "rel"),
        ("XDG_CACHE_HOME", ""),
        ("XDG_RUNTIME_DIR", "rel"),
    ];
    let c = parse(&Value::Null, &env).unwrap();
    assert_eq!(
        c.state_dir,
        PathBuf::from("/home/u/.local/state/crabbox-cloudhypervisor")
    );
    assert_eq!(
        c.cache_dir,
        PathBuf::from("/home/u/.cache/crabbox-cloudhypervisor")
    );
    assert_eq!(c.runtime_dir, c.state_dir.join("run"));
}

#[test]
fn missing_home_errors_only_when_needed() {
    assert!(parse(&Value::Null, &[]).is_err());
    let cfg = json!({"stateDir": "/s", "cacheDir": "/c"});
    let c = parse(&cfg, &[]).unwrap();
    assert_eq!(c.runtime_dir, PathBuf::from("/s/run"));
    assert!(parse(&json!({"stateDir": "/s"}), &[]).is_err());
}

#[test]
fn overrides_apply() {
    let sha = "a".repeat(64);
    let cfg = json!({
        "stateDir": "/s", "runtimeDir": "/r", "cacheDir": "/c",
        "cpus": 8, "memoryMiB": 1024, "diskGiB": 40,
        "image": {"url": "https://i/x", "sha256": sha},
        "firmware": {"url": "https://f/x", "sha256": sha},
        "cloudHypervisor": "/b/ch", "passt": "/b/passt", "qemuImg": "/b/qi",
        "ssh": "/b/ssh", "sshKeygen": "/b/sk", "curl": "/b/curl",
        "readyTimeoutSeconds": 10, "unknown": [1]
    });
    let asset = |url: &str| Asset {
        url: url.to_owned(),
        sha256: sha.clone(),
    };
    let want = Config {
        cpus: 8,
        memory_mib: 1024,
        disk_gib: 40,
        image: asset("https://i/x"),
        firmware: asset("https://f/x"),
        cloud_hypervisor: PathBuf::from("/b/ch"),
        passt: PathBuf::from("/b/passt"),
        qemu_img: PathBuf::from("/b/qi"),
        ssh: PathBuf::from("/b/ssh"),
        ssh_keygen: PathBuf::from("/b/sk"),
        curl: PathBuf::from("/b/curl"),
        ready_timeout: Duration::from_secs(10),
        ..expected("/s", "/r", "/c")
    };
    assert_eq!(parse(&cfg, &[]).unwrap(), want);
}

#[test]
fn boundaries_accepted() {
    let cfg = json!({"cpus": 64, "memoryMiB": 512, "diskGiB": 8, "readyTimeoutSeconds": 3600});
    assert!(parse(&cfg, HOME).is_ok());
    let cfg = json!({"cpus": 1, "readyTimeoutSeconds": 1});
    assert!(parse(&cfg, HOME).is_ok());
}

fn err_for(cfg: &Value) -> String {
    parse(cfg, HOME).unwrap_err()
}

#[test]
fn config_must_be_object() {
    assert!(err_for(&json!([1])).contains("config"));
    assert!(err_for(&json!("x")).contains("config"));
}

#[test]
fn integer_errors_name_key() {
    let cases = [
        ("cpus", json!(0)),
        ("cpus", json!(65)),
        ("cpus", json!(-1)),
        ("cpus", json!(1.5)),
        ("cpus", json!("two")),
        ("memoryMiB", json!(511)),
        ("memoryMiB", json!("big")),
        ("diskGiB", json!(7)),
        ("diskGiB", json!(true)),
        ("readyTimeoutSeconds", json!(0)),
        ("readyTimeoutSeconds", json!(3601)),
    ];
    for (key, value) in cases {
        let e = err_for(&json!({ key: value }));
        assert!(
            e.starts_with(&format!("config.{key} must be an integer")),
            "{e}"
        );
    }
    assert_eq!(
        err_for(&json!({"cpus": 0})),
        "config.cpus must be an integer between 1 and 64"
    );
}

#[test]
fn path_errors_name_key() {
    for key in ["stateDir", "runtimeDir", "cacheDir"] {
        for value in [json!("relative/p"), json!(5), json!("")] {
            let e = err_for(&json!({ key: value }));
            assert_eq!(e, format!("config.{key} must be an absolute path"));
        }
    }
}

#[test]
fn binary_errors_name_key() {
    for key in [
        "cloudHypervisor",
        "passt",
        "qemuImg",
        "ssh",
        "sshKeygen",
        "curl",
    ] {
        for value in [json!(""), json!(3)] {
            let e = err_for(&json!({ key: value }));
            assert!(e.starts_with(&format!("config.{key} ")), "{e}");
        }
    }
}

#[test]
fn asset_errors_name_key() {
    let sha = "b".repeat(64);
    for key in ["image", "firmware"] {
        assert!(err_for(&json!({ key: "str" })).contains(&format!("config.{key}")));
        assert!(err_for(&json!({ key: {"sha256": sha} })).contains(&format!("config.{key}.url")));
        assert!(err_for(&json!({ key: {"url": "u"} })).contains(&format!("config.{key}.sha256")));
        assert!(err_for(&json!({ key: {"url": 1, "sha256": sha} })).contains("url"));
    }
}

#[test]
fn sha256_must_be_lowercase_hex_64() {
    for bad in [
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        String::new(),
    ] {
        let e = err_for(&json!({"image": {"url": "u", "sha256": bad}}));
        assert!(e.contains("config.image.sha256"), "{e}");
    }
}

#[test]
fn errors_do_not_echo_values() {
    let secret = "SECRET-VALUE-123";
    let cases = [
        json!({"stateDir": secret}),
        json!({"runtimeDir": secret}),
        json!({"cacheDir": secret}),
        json!({"cpus": secret}),
        json!({"memoryMiB": secret}),
        json!({"diskGiB": secret}),
        json!({"readyTimeoutSeconds": secret}),
        json!({"image": secret}),
        json!({"image": {"url": secret, "sha256": secret}}),
        json!({"firmware": {"url": "u", "sha256": secret}}),
        json!({"curl": 99999}),
    ];
    for cfg in cases {
        let e = err_for(&cfg);
        assert!(!e.contains(secret) && !e.contains("99999"), "{e}");
    }
    let e = err_for(&json!({"cpus": 987_654}));
    assert!(!e.contains("987654"), "{e}");
}

#[test]
fn null_key_uses_default() {
    let c = parse(&json!({"cpus": null, "stateDir": null}), HOME).unwrap();
    assert_eq!(c.cpus, 2);
}
