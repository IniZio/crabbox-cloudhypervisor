//! Boot template: a snapshot of a freshly provisioned guest that new leases
//! restore instead of cold booting, before they take their own identity.

mod reap;
pub(super) mod restore;
mod rotate;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{mkdir_private, remove_tree, spec, Assets, RealBackend};
use crate::backend::Backend;
use crate::ch::ChClient;
use crate::host;
use crate::protocol::Desired;
use crate::seed::{self, SeedParams};
use crate::size::Size;
use crate::state::{self, LeaseRecord};

/// What the template was built from, inside its dir.
const META_FILE: &str = "template.json";
/// Snapshot dir inside a template.
pub(super) const SNAP_DIR: &str = "snap";
/// Root disk of the template guest.
pub(super) const DISK_FILE: &str = "disk.raw";
/// Private key the template guest authorizes until a lease rotates it out.
pub(super) const BOOTSTRAP_KEY: &str = "bootstrap";
/// Pin of the template guest's host key.
pub(super) const KNOWN_HOSTS: &str = "known_hosts";
/// Serialises template builds across adapter processes.
const LOCK_FILE: &str = "template.lock";
/// Files a usable template has.
pub(super) const REQUIRED: [&str; 6] = [
    "snap/config.json",
    "snap/state.json",
    "snap/memory-ranges",
    DISK_FILE,
    BOOTSTRAP_KEY,
    KNOWN_HOSTS,
];
/// Hex characters of the key in names.
const KEY12: usize = 12;
/// cloud-init instance id of the template guest; leases seed another.
const INSTANCE_ID: &str = "crabbox-template";
/// Waits out cloud-init and lets writes settle before the snapshot.
const SETTLE: &str = "cloud-init status --wait >/dev/null 2>&1; sync; sleep 3; sync";
/// Longest settle.
const SETTLE_LIMIT: Duration = Duration::from_secs(300);
/// Timeout for `vm.snapshot`, which writes all of guest RAM.
pub(super) const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);
/// Bytes per GiB.
const GIB: u64 = 1024 * 1024 * 1024;

/// Everything a template depends on; `key` hashes the rest and the seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Meta {
    /// SHA-256 over every input.
    pub(super) key: String,
    /// `cloud-hypervisor --version` of the binary that took the snapshot.
    pub(super) ch_version: String,
    /// Guest size.
    pub(super) size: Size,
    /// Root disk size in GiB.
    pub(super) disk_gib: u64,
}

impl Meta {
    /// Builds the inputs and their key.
    pub(super) fn new(
        image: &str,
        firmware: &str,
        seed: &str,
        size: Size,
        disk_gib: u64,
        ch: &str,
    ) -> Self {
        let text = format!(
            "image {image}\nfirmware {firmware}\ncpus {}\nmemory {}\ndisk {disk_gib}\nch {ch}\nseed\n{seed}",
            size.cpus, size.memory_mib
        );
        let key = Sha256::digest(text.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, b| {
                let _ = write!(hex, "{b:02x}");
                hex
            });
        Self {
            key,
            ch_version: ch.to_owned(),
            size,
            disk_gib,
        }
    }

    /// First hex characters of the key.
    pub(super) fn key12(&self) -> &str {
        self.key.get(..KEY12).unwrap_or(&self.key)
    }

    /// Lease id of the template guest while it is built.
    fn guest_id(&self) -> String {
        format!("tpl-{}", self.key12())
    }
}

/// A built template on disk.
pub(super) struct Template {
    /// `<cacheDir>/template-<key12>`.
    pub(super) dir: PathBuf,
    /// What it was built from.
    pub(super) meta: Meta,
}

/// Why an existing template dir cannot be used; `Ok(false)` when there is none.
fn check(dir: &Path, want: &Meta) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(false);
    }
    let meta: Meta = fs::read(dir.join(META_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("its template.json is unreadable")?;
    if meta.ch_version != want.ch_version {
        return Err(format!(
            "built by {}, running {}",
            meta.ch_version, want.ch_version
        ));
    }
    if meta != *want {
        return Err("its recorded inputs differ from this config".to_owned());
    }
    REQUIRED
        .iter()
        .find(|f| !dir.join(f).exists())
        .map_or(Ok(true), |f| Err(format!("it has no {f}")))
}

/// Removes other templates of the same size and crashed builds.
fn remove_superseded(cache_dir: &Path, want: &Meta, diag: &mut dyn Write) {
    let Ok(entries) = fs::read_dir(cache_dir) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with("template-") || !path.is_dir() {
            continue;
        }
        let meta: Option<Meta> = fs::read(path.join(META_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let stale = meta.is_none_or(|m| m.size == want.size && m.key != want.key);
        if stale && remove_tree(&path).is_ok() {
            let _ = writeln!(diag, "template: removed {name}");
        }
    }
}

/// The ready template for `want`, built by `build` into a scratch dir when absent.
/// A template that cannot be used is removed and refused.
pub(super) fn ensure(
    cache_dir: &Path,
    want: &Meta,
    diag: &mut dyn Write,
    build: impl FnOnce(&Path, &mut dyn Write) -> Result<(), String>,
) -> Result<Template, String> {
    mkdir_private(cache_dir)?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache_dir.join(LOCK_FILE))
        .and_then(|f| f.lock().map(|()| f))
        .map_err(|e| format!("lock template: {e}"))?;
    let dir = cache_dir.join(format!("template-{}", want.key12()));
    let template = || Template {
        dir: dir.clone(),
        meta: want.clone(),
    };
    match check(&dir, want) {
        Ok(true) => return Ok(template()),
        Ok(false) => {}
        Err(why) => {
            remove_tree(&dir)?;
            return Err(why);
        }
    }
    let started = Instant::now();
    let tmp = cache_dir.join(format!("template-{}.tmp", want.key12()));
    remove_tree(&tmp)?;
    mkdir_private(&tmp)?;
    let made = build(&tmp, diag).and_then(|()| {
        let json = serde_json::to_vec_pretty(want).map_err(|e| e.to_string())?;
        fs::write(tmp.join(META_FILE), json).map_err(|e| e.to_string())
    });
    if let Err(e) = made {
        let _ = remove_tree(&tmp);
        return Err(format!("build failed: {e}"));
    }
    fs::rename(&tmp, &dir).map_err(|e| format!("publish template: {e}"))?;
    remove_superseded(cache_dir, want, diag);
    drop(lock);
    let secs = started.elapsed().as_secs_f64();
    let _ = writeln!(diag, "template {} built in {secs:.1}s", want.key12());
    Ok(template())
}

/// `cloud-hypervisor --version`, first line.
pub(super) fn ch_version(program: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run cloud-hypervisor --version: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().next().map(str::trim) {
        Some(v) if out.status.success() && !v.is_empty() => Ok(v.to_owned()),
        _ => Err("cloud-hypervisor --version printed no version".to_owned()),
    }
}

impl RealBackend {
    /// Boots a new lease: from the template when `bootTemplate` is on and one is
    /// usable, else cold, with the reason in `diag`.
    pub(super) fn provision_lease(
        &self,
        desired: &Desired,
        run_dir: &Path,
        assets: &Assets,
        size: Size,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        if !self.config.boot_template {
            return self.provision(desired, run_dir, assets, size, diag);
        }
        let why = match self.ready_template(assets, size, diag) {
            Ok(t) => match self.restore_template(desired, run_dir, &t, size, diag) {
                Ok(record) => return self.take_identity(record, &t, diag),
                Err(e) => {
                    if let Err(d) = self.destroy(&desired.lease_id, diag) {
                        let _ = writeln!(diag, "warn: {d}");
                    }
                    format!("restore failed: {e}")
                }
            },
            Err(why) => why,
        };
        let _ = writeln!(diag, "template refused: {why}, cold booting");
        self.provision(desired, run_dir, assets, size, diag)
    }

    /// cloud-init documents of the template guest.
    fn template_seed(
        &self,
        ssh_key: &str,
        host_private: &str,
        host_public: &str,
    ) -> (String, String) {
        let mut guest = crate::volumes::guest::setup(&[]);
        crate::reclaim::add_to(&mut guest, &self.config.reclaim);
        let params = SeedParams {
            instance_id: INSTANCE_ID,
            hostname: INSTANCE_ID,
            ssh_public_key: ssh_key,
            host_private_key: host_private,
            host_public_key: host_public,
            files: &guest.files,
            commands: &guest.commands,
        };
        (seed::user_data(&params), seed::meta_data(&params))
    }

    /// The template for this config and `size`, built when missing.
    fn ready_template(
        &self,
        assets: &Assets,
        size: Size,
        diag: &mut dyn Write,
    ) -> Result<Template, String> {
        let c = &self.config;
        let ch = ch_version(&c.cloud_hypervisor)?;
        let (seed, _) = self.template_seed("<bootstrap key>", "<host key>", "<host key>");
        let want = Meta::new(
            &c.image.sha256,
            &c.firmware.sha256,
            &seed,
            size,
            c.disk_gib,
            &ch,
        );
        ensure(&c.cache_dir, &want, diag, |dir, diag| {
            self.build_template(dir, &want, assets, diag)
        })
    }

    /// Boots the template guest under its own state dir in `dir`, snapshots it,
    /// stops it and keeps its disk, bootstrap key and host key pin.
    fn build_template(
        &self,
        dir: &Path,
        want: &Meta,
        assets: &Assets,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let id = want.guest_id();
        let mut config = self.config.clone();
        config.state_dir = dir.join("state");
        let builder = Self::new(config, self.exe.clone());
        let run_dir = self.config.runtime_dir.join(state::run_dir_name(&id));
        reap::reap(&run_dir, diag)?;
        let _building = reap::in_flight(&run_dir)?;
        let mut record = spec::new_record(&id, "template", "template", &run_dir, want.size);
        let built = builder.boot_template(&mut record, assets, dir, diag);
        let stopped = Self::stop_processes(&record, diag);
        let unscoped = builder.scopes().stop(&id, diag);
        let _ = remove_tree(&run_dir);
        built.and(stopped).and(unscoped)?;
        let store = &builder.store;
        for (from, to) in [
            (store.disk_path(&id), DISK_FILE),
            (store.key_path(&id), BOOTSTRAP_KEY),
            (store.known_hosts_path(&id), KNOWN_HOSTS),
        ] {
            fs::rename(&from, dir.join(to)).map_err(|e| format!("keep {to}: {e}"))?;
        }
        remove_tree(&dir.join("state"))
    }

    /// Cold boots the template guest, waits until it is quiet, pauses it and
    /// writes its snapshot to `dir/snap`.
    fn boot_template(
        &self,
        record: &mut LeaseRecord,
        assets: &Assets,
        dir: &Path,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let id = record.lease_id.clone();
        let size = self.record_size(record)?;
        remove_tree(&record.run_dir)?;
        self.store.write(record)?;
        mkdir_private(&record.run_dir)?;
        let c = &self.config;
        let ssh_key = host::ssh_keygen(
            &c.ssh_keygen,
            &self.store.key_path(&id),
            "crabbox-bootstrap",
        )?;
        let (private, public) = self.make_host_key(&id)?;
        host::clone_disk(
            &assets.base_raw,
            &self.store.disk_path(&id),
            c.disk_gib * GIB,
        )?;
        let (user_data, meta_data) = self.template_seed(&ssh_key, &private, &public);
        seed::write_seed(&self.store.seed_path(&id), &user_data, &meta_data)?;
        self.start_processes(record, size, None, diag)?;
        self.boot_vm(record, size, &assets.firmware, diag)?;
        self.wait_ready(record, diag)?;
        spec::guest_ssh(&c.ssh, &self.store, &self.exe, &c.state_dir, &id)
            .output(SETTLE, SETTLE_LIMIT)
            .map_err(|e| format!("template guest did not settle: {e}"))?;
        self.pause_vm(record, diag)?;
        let snap = dir.join(SNAP_DIR);
        mkdir_private(&snap)?;
        ChClient::new(&record.api_socket())
            .with_timeout(SNAPSHOT_TIMEOUT)
            .snapshot(&snap)?;
        super::suspend::dig_snapshot_holes(Path::new(host::DIG_HOLES_TOOL), &snap, diag);
        Ok(())
    }
}
