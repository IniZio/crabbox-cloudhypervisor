//! What the seed writes into the guest for cache disks: systemd units on the root disk.
//!
//! Units live on the root disk, so pause/resume and reboots keep the mounts.

use std::fmt::Write as _;

use super::{escape_path, Group, LOGIN_GID, LOGIN_HOME, LOGIN_UID, MOUNT_ROOT};
/// Docker's daemon config in the guest.
pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";
/// Script that points Docker's data root into the login home.
const DOCKER_SCRIPT: &str = "/usr/local/lib/crabbox/docker-data-root.py";
/// Script that creates volume directories and missing path parents.
const DIRS_SCRIPT: &str = "/usr/local/sbin/crabbox-cache-dirs";
/// Unit that runs [`DIRS_SCRIPT`] between the group mounts and the binds.
const DIRS_UNIT: &str = "crabbox-cache-dirs.service";
/// Directory for unit files.
const UNIT_DIR: &str = "/etc/systemd/system";
/// Services that read cache paths; every cache mount is up before them.
const CONSUMERS: &str = "docker.service containerd.service";

/// One file cloud-init writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFile {
    /// Absolute guest path.
    pub path: String,
    /// Octal mode string.
    pub permissions: &'static str,
    /// File body.
    pub content: String,
}

/// Files to write, then commands to run once on first boot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuestSetup {
    /// Files written by cloud-init.
    pub files: Vec<SeedFile>,
    /// Argument vectors run after `systemctl daemon-reload`.
    pub commands: Vec<Vec<String>>,
}

/// A group whose disk is attached with virtio serial `serial`.
#[derive(Debug, Clone, Copy)]
pub struct Attached<'a> {
    /// The group.
    pub group: &'a Group,
    /// virtio-blk serial, visible as `/dev/disk/by-id/virtio-<serial>`.
    pub serial: &'a str,
}

/// Mount unit name for a mount point.
#[must_use]
pub fn mount_unit_name(where_: &str) -> String {
    format!("{}.mount", escape_path(where_))
}

/// Guest mount point of a group disk.
#[must_use]
pub fn group_mount_point(group: &Group) -> String {
    format!("{MOUNT_ROOT}/{}", group.id.as_str())
}

/// Every prefix of an absolute path, shortest first: `/a/b` gives `/a`, `/a/b`.
fn prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push(acc.clone());
    }
    out
}

/// Body of the group disk's mount unit.
fn group_unit(mount_point: &str, serial: &str) -> String {
    format!(
        "[Unit]\nDescription=crabbox cache disk {serial}\nBefore={DIRS_UNIT} {CONSUMERS}\n\
         [Mount]\nWhat=/dev/disk/by-id/virtio-{serial}\nWhere={mount_point}\nType=ext4\n\
         Options=noatime,discard,nofail,noinit_itable\n[Install]\nWantedBy=local-fs.target\n"
    )
}

/// Body of one bind mount unit.
fn bind_unit(source: &str, target: &str) -> String {
    format!(
        "[Unit]\nDescription=crabbox cache {target}\nRequires={DIRS_UNIT}\nAfter={DIRS_UNIT}\n\
         Before={CONSUMERS}\n[Mount]\nWhat={source}\nWhere={target}\nType=none\n\
         Options=bind,nofail\n[Install]\nWantedBy=local-fs.target\n"
    )
}

/// Body of the directory service, ordered after every group mount.
fn dirs_unit(group_units: &[String]) -> String {
    format!(
        "[Unit]\nDescription=crabbox cache directories\nDefaultDependencies=no\n\
         After=local-fs-pre.target {}\n[Service]\nType=oneshot\nRemainAfterExit=yes\n\
         ExecStart={DIRS_SCRIPT}\n[Install]\nWantedBy=local-fs.target\n",
        group_units.join(" ")
    )
}

/// The cache-disk units and scripts for `attached`, plus the Docker data root.
#[must_use]
pub fn setup(attached: &[Attached<'_>]) -> GuestSetup {
    let mut out = GuestSetup::default();
    let docker_root = format!("{LOGIN_HOME}/docker");
    out.files.push(SeedFile {
        path: DOCKER_SCRIPT.to_owned(),
        permissions: "0644",
        content: docker_root_script(DAEMON_JSON, &docker_root),
    });
    out.commands.push(
        ["sh", "-c", &format!("python3 {DOCKER_SCRIPT} || true")]
            .map(str::to_owned)
            .to_vec(),
    );
    if attached.is_empty() {
        return out;
    }
    let mut group_units = Vec::new();
    let mut bind_units = Vec::new();
    let mut dirs = Vec::new();
    for a in attached {
        let mount_point = group_mount_point(a.group);
        let unit = mount_unit_name(&mount_point);
        out.files
            .push(unit_file(&unit, group_unit(&mount_point, a.serial)));
        group_units.push(unit);
        for volume in &a.group.volumes {
            let volume_dir = format!("{mount_point}/{}", volume.name.as_str());
            dirs.push(volume_dir.clone());
            for (path, dir) in volume.paths.iter().zip(volume.image_dirs()) {
                let target = path.resolve(LOGIN_HOME);
                let source = format!("{mount_point}{dir}");
                dirs.push(source.clone());
                dirs.extend(prefixes(&target));
                let unit = mount_unit_name(&target);
                out.files
                    .push(unit_file(&unit, bind_unit(&source, &target)));
                bind_units.push(unit);
            }
        }
    }
    out.files.push(SeedFile {
        path: DIRS_SCRIPT.to_owned(),
        permissions: "0755",
        content: dirs_script(&dirs),
    });
    out.files
        .push(unit_file(DIRS_UNIT, dirs_unit(&group_units)));
    let mut enable: Vec<String> = ["systemctl", "enable", "--now"].map(str::to_owned).to_vec();
    enable.extend(group_units);
    enable.push(DIRS_UNIT.to_owned());
    enable.extend(bind_units);
    out.commands.push(enable);
    out
}

/// A unit file under [`UNIT_DIR`].
fn unit_file(name: &str, content: String) -> SeedFile {
    SeedFile {
        path: format!("{UNIT_DIR}/{name}"),
        permissions: "0644",
        content,
    }
}

/// Shell script creating each missing directory, owned by the login user.
#[must_use]
pub fn dirs_script(dirs: &[String]) -> String {
    let mut out = format!(
        "#!/bin/sh\nmk() {{ [ -d \"$1\" ] || install -d -o {LOGIN_UID} -g {LOGIN_GID} \"$1\"; }}\n"
    );
    for dir in dirs {
        let _ = writeln!(out, "mk '{dir}'");
    }
    out
}

/// Python that sets `data-root` in `daemon_json`, keeping every other key.
#[must_use]
pub fn docker_root_script(daemon_json: &str, data_root: &str) -> String {
    format!(
        r#"import json, os
path, root = {daemon_json:?}, {data_root:?}
try:
    with open(path) as f:
        conf = json.load(f)
except FileNotFoundError:
    conf = {{}}
conf["data-root"] = root
os.makedirs(os.path.dirname(path), exist_ok=True)
with open(path + ".tmp", "w") as f:
    json.dump(conf, f, indent=2)
    f.write("\n")
os.replace(path + ".tmp", path)
"#
    )
}

#[cfg(test)]
mod tests;
