use super::*;
use std::io::Read;
use std::process::Command;

const KEY: &str = "ssh-ed25519 AAAAC3Nza test@host";

fn params() -> SeedParams<'static> {
    SeedParams {
        instance_id: "i-123",
        hostname: "box-1",
        ssh_public_key: KEY,
    }
}

#[test]
fn user_data_has_expected_parts() {
    let u = user_data(&params());
    assert!(u.starts_with("#cloud-config\n"));
    assert!(u.contains(&format!("      - {KEY}\n")));
    assert!(u.contains("ListenStream=vsock::22"));
    assert!(u.contains("/usr/local/bin/crabbox-ready"));
    assert!(u.contains("install -y -qq git rsync curl jq python3 tar"));
    assert!(u.contains("lock_passwd: true"));
}

#[test]
fn user_data_flattens_key_newlines() {
    let p = SeedParams {
        ssh_public_key: "ssh-ed25519 A\r\nruncmd: x\n",
        ..params()
    };
    let u = user_data(&p);
    assert!(u.contains("      - ssh-ed25519 A  runcmd: x\n"));
}

#[test]
fn meta_data_exact() {
    assert_eq!(
        meta_data(&params()),
        "instance-id: i-123\nlocal-hostname: box-1\n"
    );
}

#[test]
fn hostname_cases() {
    assert_eq!(hostname_for("Crabbox_Lease 42"), "crabbox-lease-42");
    assert_eq!(hostname_for("--a..b--"), "a-b");
    assert_eq!(hostname_for("___"), "crabbox");
    assert_eq!(hostname_for(""), "crabbox");
    assert_eq!(hostname_for("é"), "crabbox");
    let long = "a".repeat(100);
    assert_eq!(hostname_for(&long).len(), 63);
    let edge = format!("{}-b", "a".repeat(62));
    assert_eq!(hostname_for(&edge), "a".repeat(62));
}

#[test]
fn write_seed_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("seed.img");
    let u = user_data(&params());
    let m = meta_data(&params());
    write_seed(&img, &u, &m).unwrap();
    assert!(!dir.path().join("seed.img.tmp").exists());

    let file = fs::File::open(&img).unwrap();
    let fsys = FileSystem::new(&file, FsOptions::new()).unwrap();
    assert_eq!(fsys.volume_label(), "CIDATA");
    for (name, want) in [("user-data", &u), ("meta-data", &m)] {
        let mut s = String::new();
        fsys.root_dir()
            .open_file(name)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(&s, want);
    }
    drop(fsys);

    if let Ok(out) = Command::new("blkid")
        .args(["-o", "value", "-s", "LABEL"])
        .arg(&img)
        .output()
    {
        if out.status.success() {
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "CIDATA");
        }
    }
}

#[test]
fn write_seed_overwrites_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("seed.img");
    write_seed(&img, "a", "b").unwrap();
    write_seed(&img, "c", "d").unwrap();
    assert!(write_seed(&dir.path().join("no/such/seed.img"), "a", "b").is_err());
}
