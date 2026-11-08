#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]
//! Install tests: downloads come from a local HTTP server, never the network.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{fix, install, missing_packages, pins, Checksum, Pin};
use crate::config::Config;

/// Serves `files` by path; records every requested path.
struct Server {
    /// `http://127.0.0.1:<port>`.
    base: String,
    /// Paths requested so far.
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start(files: HashMap<String, Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request.split(' ').nth(1).unwrap_or("").to_owned();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = files
                    .get(&path)
                    .map_or(("404 Not Found", &b""[..]), |b| ("200 OK", b.as_slice()));
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        Self { base, hits }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
}

const CH: &[u8] = b"#!/bin/sh\necho cloud-hypervisor v52.0-moor.1\n";
const PASST: &[u8] = b"#!/bin/sh\necho passt test\n";

fn release() -> Server {
    let mut files = HashMap::new();
    files.insert("/ch".to_owned(), CH.to_vec());
    files.insert("/passt".to_owned(), PASST.to_vec());
    files.insert(
        "/passt.sha256".to_owned(),
        format!("{}  passt-x86_64-linux\n", sha(PASST)).into_bytes(),
    );
    Server::start(files)
}

fn test_pins(server: &Server) -> Vec<Pin> {
    vec![
        Pin {
            name: "cloud-hypervisor",
            version: "v52.0-moor.1".to_owned(),
            url: server.url("/ch"),
            sha256: Checksum::Pinned(sha(CH)),
        },
        Pin {
            name: "passt",
            version: "v0.0.0".to_owned(),
            url: server.url("/passt"),
            sha256: Checksum::Published(server.url("/passt.sha256")),
        },
    ]
}

fn curl() -> PathBuf {
    PathBuf::from("curl")
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| Path::new(n).extension().is_some_and(|e| e == "part"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn installs_the_pinned_binaries_executable() {
    let server = release();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");

    let first = install(&test_pins(&server), &bin, &curl()).unwrap();
    assert!(
        first.contains("installed cloud-hypervisor v52.0-moor.1"),
        "{first}"
    );
    assert!(first.contains("installed passt v0.0.0"), "{first}");
    assert_eq!(fs::read(bin.join("cloud-hypervisor")).unwrap(), CH);
    assert_eq!(fs::read(bin.join("passt")).unwrap(), PASST);
    let mode = fs::metadata(bin.join("passt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn a_rerun_downloads_nothing() {
    let server = release();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    install(&test_pins(&server), &bin, &curl()).unwrap();

    let second = install(&test_pins(&server), &bin, &curl()).unwrap();
    assert!(
        second.contains("cloud-hypervisor v52.0-moor.1 already installed"),
        "{second}"
    );
    assert!(
        second.contains("passt v0.0.0 already installed"),
        "{second}"
    );
    assert_eq!(server.count("/ch"), 1);
    assert_eq!(server.count("/passt"), 1);
    assert!(leftovers(&bin).is_empty());
}

#[test]
fn a_tampered_install_is_replaced_on_rerun() {
    let server = release();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    install(&test_pins(&server), &bin, &curl()).unwrap();
    fs::write(bin.join("cloud-hypervisor"), b"stale").unwrap();

    let report = install(&test_pins(&server), &bin, &curl()).unwrap();
    assert!(
        report.contains("installed cloud-hypervisor v52.0-moor.1"),
        "{report}"
    );
    assert_eq!(fs::read(bin.join("cloud-hypervisor")).unwrap(), CH);
}

#[test]
fn a_checksum_mismatch_installs_nothing_and_says_so() {
    let server = release();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    let mut pins = test_pins(&server);
    pins[0].sha256 = Checksum::Pinned(sha(b"something else"));

    let err = install(&pins, &bin, &curl()).unwrap_err();
    assert!(err.contains("cloud-hypervisor"), "{err}");
    assert!(err.contains("checksum mismatch"), "{err}");
    assert!(!bin.join("cloud-hypervisor").exists());
    assert!(leftovers(&bin).is_empty());
}

#[test]
fn a_published_checksum_mismatch_is_refused() {
    let mut files = HashMap::new();
    files.insert("/passt".to_owned(), PASST.to_vec());
    files.insert("/passt.sha256".to_owned(), sha(b"other").into_bytes());
    let server = Server::start(files);
    let dir = tempfile::tempdir().unwrap();
    let pins = &test_pins(&server)[1..];

    let err = install(pins, dir.path(), &curl()).unwrap_err();
    assert!(
        err.contains("passt") && err.contains("checksum mismatch"),
        "{err}"
    );
    assert!(!dir.path().join("passt").exists());
}

#[test]
fn an_unpublished_asset_names_the_missing_url() {
    let server = Server::start(HashMap::new());
    let dir = tempfile::tempdir().unwrap();
    let pins = &test_pins(&server)[1..];

    let err = install(pins, dir.path(), &curl()).unwrap_err();
    assert!(err.contains("passt v0.0.0 is not published"), "{err}");
    assert!(err.contains(&server.url("/passt.sha256")), "{err}");
}

#[test]
fn release_pins_cover_x86_64_and_name_the_gap_elsewhere() {
    let x86 = pins("x86_64").unwrap();
    let names: Vec<&str> = x86.iter().map(|p| p.name).collect();
    assert_eq!(names, ["cloud-hypervisor", "passt"]);
    assert!(x86[0]
        .url
        .contains("IniZio/cloud-hypervisor/releases/download/v52.0-moor.1/"));
    assert!(x86[1].url.ends_with(&format!(
        "/v{}/passt-x86_64-linux",
        env!("CARGO_PKG_VERSION")
    )));

    let err = pins("aarch64").map(|_| ()).unwrap_err();
    assert!(err.contains("aarch64"), "{err}");
}

#[test]
fn installed_binaries_become_the_config_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap().to_owned();
    let bin = dir.path().join("crabbox-cloudhypervisor/bin");
    let env = |k: &str| match k {
        "HOME" => Some("/home/u".to_owned()),
        "XDG_DATA_HOME" => Some(data.clone()),
        _ => None,
    };

    let before = Config::from_request(&Value::Null, &env).unwrap();
    assert_eq!(before.passt, PathBuf::from("passt"));

    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("passt"), PASST).unwrap();
    fs::write(bin.join("cloud-hypervisor"), CH).unwrap();
    let after = Config::from_request(&Value::Null, &env).unwrap();
    assert_eq!(after.passt, bin.join("passt"));
    assert_eq!(after.cloud_hypervisor, bin.join("cloud-hypervisor"));

    let explicit = serde_json::json!({ "passt": "/opt/passt" });
    let pinned = Config::from_request(&explicit, &env).unwrap();
    assert_eq!(pinned.passt, PathBuf::from("/opt/passt"));
}

#[test]
fn missing_host_tools_name_their_package() {
    let empty = tempfile::tempdir().unwrap();
    let missing = missing_packages(empty.path().as_os_str());
    assert!(missing.contains(&("qemu-img", "qemu-utils")), "{missing:?}");
    assert!(missing.contains(&("rsync", "rsync")), "{missing:?}");
    assert!(missing.contains(&("resize2fs", "e2fsprogs")), "{missing:?}");
    assert!(fix("passt").contains("crabbox-cloudhypervisor install"));
    assert!(fix("git").contains("git package"));
}
