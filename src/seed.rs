//! `NoCloud` seed image: cloud-init user-data/meta-data in a FAT volume labelled `CIDATA`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use fatfs::{format_volume, FileSystem, FormatVolumeOptions, FsOptions};

/// Image size: small, but large enough for FAT16 (needs >= 4085 clusters).
const IMAGE_BYTES: u64 = 2 * 1024 * 1024;
/// Volume label that cloud-init `NoCloud` looks for.
const LABEL: [u8; 11] = *b"CIDATA     ";
/// Longest DNS label.
const MAX_HOSTNAME: usize = 63;

/// Indented body of `/usr/local/bin/crabbox-ready`. If cloud-init finished but the
/// marker never reached disk (VMM killed), it recreates the marker instead of hanging.
const READY_SCRIPT: &str = "      #!/bin/sh
      test -f /var/lib/crabbox-ready && exit 0
      cloud-init status 2>/dev/null | grep -q 'status: done' || exit 1
      touch /var/lib/crabbox-ready && sync
";

/// Values substituted into the cloud-init documents.
pub struct SeedParams<'a> {
    /// cloud-init instance id (new id per lease regenerates host keys).
    pub instance_id: &'a str,
    /// Guest hostname.
    pub hostname: &'a str,
    /// One-line OpenSSH public key authorised for user `crabbox`.
    pub ssh_public_key: &'a str,
    /// Full OpenSSH ed25519 host private key (multi-line PEM).
    pub host_private_key: &'a str,
    /// One-line OpenSSH ed25519 host public key.
    pub host_public_key: &'a str,
}

/// Renders `#cloud-config` user-data. A multi-line key is flattened to one line.
#[must_use]
pub fn user_data(params: &SeedParams<'_>) -> String {
    let key = params.ssh_public_key.trim().replace(['\r', '\n'], " ");
    let host_pub = params.host_public_key.trim().replace(['\r', '\n'], " ");
    let host_priv = params
        .host_private_key
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r"#cloud-config
users:
  - name: crabbox
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
    ssh_authorized_keys:
      - {key}
ssh_pwauth: false
ssh_deletekeys: true
ssh_genkeytypes: []
ssh_keys:
  ed25519_private: |
{host_priv}
  ed25519_public: {host_pub}
write_files:
  - path: /etc/ssh/sshd_config.d/00-crabbox-hostkey.conf
    content: |
      HostKey /etc/ssh/ssh_host_ed25519_key
  - path: /etc/systemd/system/crabbox-ssh-vsock.socket
    content: |
      [Unit]
      Description=sshd over vsock (port 22)
      [Socket]
      ListenStream=vsock::22
      Accept=yes
      [Install]
      WantedBy=sockets.target
  - path: /etc/systemd/system/crabbox-ssh-vsock@.service
    content: |
      [Unit]
      Description=sshd -i for vsock connection
      [Service]
      ExecStartPre=/usr/bin/mkdir -p /run/sshd
      ExecStart=-/usr/sbin/sshd -i
      StandardInput=socket
  - path: /usr/local/bin/crabbox-ready
    permissions: '0755'
    content: |
{READY_SCRIPT}runcmd:
  - [systemctl, daemon-reload]
  - [systemctl, enable, --now, crabbox-ssh-vsock.socket]
  - [sh, -c, 'for i in 1 2 3 4 5; do apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq git rsync curl jq python3 tar && break; sleep 3; done']
  - [sh, -c, 'touch /var/lib/crabbox-ready && sync']
"
    )
}

/// Renders `NoCloud` `meta-data`.
#[must_use]
pub fn meta_data(params: &SeedParams<'_>) -> String {
    format!(
        "instance-id: {}\nlocal-hostname: {}\n",
        params.instance_id, params.hostname
    )
}

/// Derives a valid hostname: `[a-z0-9-]`, no edge or repeated dashes, <= 63 chars.
#[must_use]
pub fn hostname_for(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    out.truncate(MAX_HOSTNAME);
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "crabbox".to_owned()
    } else {
        out.to_owned()
    }
}

/// Writes a FAT16 image labelled `CIDATA` holding `user-data` and `meta-data`.
/// The file appears atomically at `path` (temp file + rename).
pub fn write_seed(path: &Path, user_data: &str, meta_data: &str) -> Result<(), String> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let result = build(&tmp, user_data, meta_data)
        .and_then(|()| fs::rename(&tmp, path).map_err(|e| e.to_string()));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("write seed {}: {e}", path.display()))
}

/// Creates and fills the image at `path`.
fn build(path: &Path, user_data: &str, meta_data: &str) -> Result<(), String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.set_len(IMAGE_BYTES).map_err(|e| e.to_string())?;
    format_volume(
        &file,
        FormatVolumeOptions::new()
            .fat_type(fatfs::FatType::Fat16)
            .volume_label(LABEL),
    )
    .map_err(|e| e.to_string())?;
    let fs = FileSystem::new(&file, FsOptions::new()).map_err(|e| e.to_string())?;
    let root = fs.root_dir();
    for (name, body) in [("user-data", user_data), ("meta-data", meta_data)] {
        let mut f = root.create_file(name).map_err(|e| e.to_string())?;
        f.truncate().map_err(|e| e.to_string())?;
        f.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())?;
    }
    drop(root);
    fs.unmount().map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
