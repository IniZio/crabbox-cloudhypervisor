#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use std::path::PathBuf;

use super::*;

pub(crate) fn limits(max_volume_gib: u64) -> CacheConfig {
    CacheConfig {
        volume_dir: PathBuf::from("/v"),
        store_max_gib: 200,
        max_volume_gib,
        key_ttl_days: 30,
    }
}

pub(crate) fn req(group: &str, name: &str, paths: &[&str], size_gb: u64) -> CacheVolumeRequest {
    CacheVolumeRequest {
        group: group.to_owned(),
        name: name.to_owned(),
        key: format!("{group}-{name}"),
        paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        size_gb,
        sealed: false,
    }
}

#[test]
fn token_accepts_the_key_charset_up_to_96() {
    for ok in ["a", "7f3a9c1e04b2d655-gradle", "A.b_c-9", &"x".repeat(96)] {
        assert_eq!(Token::parse(ok).unwrap().as_str(), ok);
    }
    for bad in ["", ".", "..", "a/b", "a b", "ä", "~x", &"x".repeat(97)] {
        assert!(Token::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn guest_paths_resolve_home_and_absolute() {
    let home = GuestPath::parse("~/.gradle/caches").unwrap();
    assert_eq!(home, GuestPath::Home(".gradle/caches".to_owned()));
    assert_eq!(
        home.resolve("/home/crabbox"),
        "/home/crabbox/.gradle/caches"
    );
    let abs = GuestPath::parse("/opt/android-sdk").unwrap();
    assert_eq!(abs.resolve("/home/crabbox"), "/opt/android-sdk");
}

#[test]
fn guest_paths_accept_only_the_cacheable_roots() {
    for ok in [
        "~/.npm",
        "~/docker",
        "~/.sshx",
        "~/a/.ssh",
        "/opt/android-sdk",
        "/opt/a/b",
        "/var/cache/apt",
        "/var/lib/containerd",
        "/var/lib/containerd/io.containerd.content.v1.content",
        "/var/lib/containers",
        "/var/lib/containers/storage",
    ] {
        assert!(GuestPath::parse(ok).is_ok(), "{ok:?}");
    }
    for bad in [
        "~/.ssh",
        "~/.ssh/authorized_keys",
        "/etc",
        "/etc/docker",
        "/opt",
        "/var/cache",
        "/var/lib",
        "/var/lib/docker",
        "/var/lib/containerdx",
        "/var/lib/containersx",
        "/home/crabbox/.npm",
        "/root/.npm",
        "/usr/local",
        "/var/lib/crabbox-cache",
        "/var/lib/crabbox-cache/g",
        "",
        "~",
        "~/",
        "/",
        "rel/x",
        "~/a/../b",
        "/opt/a/./b",
        "/opt/a//b",
        "/opt/a/",
        "/opt/a b",
        "/opt/a'b",
        "~/a\nb",
    ] {
        assert!(GuestPath::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_refused_path_skips_only_its_volume() {
    let reqs = [
        req("g", "keys", &["~/.ssh"], 0),
        req("g", "etc", &["~/.npm", "/etc/docker"], 0),
        req("g", "npm", &["~/.npm"], 0),
    ];
    let plan = plan(&reqs, &limits(40));
    let names: Vec<&str> = plan.groups[0]
        .volumes
        .iter()
        .map(|v| v.name.as_str())
        .collect();
    assert_eq!(names, ["npm"]);
    let skipped: Vec<&str> = plan.skipped.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(skipped, ["keys", "etc"]);
    assert!(plan.skipped.iter().all(|(_, r)| r.starts_with("path ")));
}

#[test]
fn plan_keeps_sealed_per_volume() {
    let mut docker = req("gx", "docker", &["~/docker", "/var/lib/containerd"], 0);
    docker.sealed = true;
    let reqs = [req("g", "npm", &["~/.npm"], 0), docker];
    let plan = plan(&reqs, &limits(40));
    let sealed: Vec<(&str, &str, bool)> = plan
        .groups
        .iter()
        .flat_map(|g| {
            g.volumes
                .iter()
                .map(move |v| (g.id.as_str(), v.name.as_str(), v.sealed))
        })
        .collect();
    assert_eq!(
        sealed,
        [("g", "npm", false), ("gx", "docker", true)],
        "two groups per scope, sealed only where declared"
    );
}

#[test]
fn capacity_is_the_declared_sum_or_default_clamped_to_the_max() {
    assert_eq!(capacity(&[0, 0], 40), DEFAULT_GROUP_GIB * GIB);
    assert_eq!(capacity(&[12, 15], 40), 27 * GIB);
    assert_eq!(capacity(&[30, 30], 40), 40 * GIB);
    assert_eq!(capacity(&[], 8), 8 * GIB);
    assert_eq!(capacity(&[u64::MAX, 1], 40), 40 * GIB);
}

#[test]
fn plan_groups_names_and_sums_sizes() {
    let reqs = [
        req(
            "g1",
            "gradle",
            &["~/.gradle/caches", "~/.gradle/wrapper"],
            0,
        ),
        req("g2", "cargo", &["~/.cargo/registry"], 0),
        req("g1", "sdk", &["/opt/sdk"], 25),
        req("g1", "ndk", &["/opt/ndk"], 25),
    ];
    let plan = plan(&reqs, &limits(40));
    assert!(plan.skipped.is_empty());
    let ids: Vec<&str> = plan.groups.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["g1", "g2"]);
    let g1 = &plan.groups[0];
    let names: Vec<&str> = g1.volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["gradle", "sdk", "ndk"]);
    assert_eq!(g1.volumes[0].paths.len(), 2);
    assert_eq!(g1.capacity, 40 * GIB);
    assert_eq!(plan.groups[1].capacity, DEFAULT_GROUP_GIB * GIB);
}

#[test]
fn plan_skips_bad_entries_without_failing_the_rest() {
    let mut bad_key = req("g", "k", &["~/k"], 0);
    bad_key.key = "a/b".to_owned();
    let reqs = [
        req("g", "ok", &["~/ok"], 0),
        req("g/x", "bad-group", &["~/a"], 0),
        req("g", "..", &["~/b"], 0),
        bad_key,
        req("g", "nopaths", &[], 0),
        req("g", "badpath", &["~/a/../b"], 0),
        req("g", "ok", &["~/ok2"], 0),
        req("g2", "again", &["~/ok"], 0),
    ];
    let plan = plan(&reqs, &limits(40));
    assert_eq!(plan.groups.len(), 1);
    assert_eq!(plan.groups[0].volumes.len(), 1);
    let skipped: Vec<(&str, &str)> = plan
        .skipped
        .iter()
        .map(|(n, r)| (n.as_str(), r.split(' ').next().unwrap_or_default()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("bad-group", "group"),
            ("..", "name"),
            ("k", "key"),
            ("nopaths", "no"),
            ("badpath", "path"),
            ("ok", "duplicate"),
            ("again", "path"),
        ]
    );
}

#[test]
fn echo_names_each_outcome() {
    let echo = |o: &Outcome| {
        let e = o.echo("gradle");
        (e.name, e.outcome, e.detail)
    };
    let restored = Outcome::Restored {
        generation: 1_729_781_203_123_456_789,
    };
    assert_eq!(
        echo(&restored),
        ("gradle".into(), "restored".into(), "gen 1729781203".into())
    );
    assert_eq!(
        echo(&Outcome::Empty),
        ("gradle".into(), "empty".into(), "no generation yet".into())
    );
    let skipped = Outcome::Skipped {
        reason: "mke2fs exited 1".to_owned(),
    };
    assert_eq!(
        echo(&skipped),
        ("gradle".into(), "skipped".into(), "mke2fs exited 1".into())
    );
}
