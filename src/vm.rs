//! The real backend: boots and destroys cloud-hypervisor microVMs.

mod cache;
mod power;
pub mod spec;
mod watcher;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::assets;
use crate::backend::{self, Backend, Power};
use crate::ch::ChClient;
use crate::config::Config;
use crate::host;
use crate::protocol::{CacheVolumeEcho, Desired};
use crate::seed::{self, SeedParams};
use crate::state::{self, LeaseLock, LeaseRecord, Store};

/// Process name prefix of cloud-hypervisor (`comm` is cut to 15 characters).
const CH_COMM: &str = "cloud-hyperviso";
/// Process name of passt.
const PASST_COMM: &str = "passt";
/// Longest allowed socket path (`sun_path` is 108 bytes including the NUL).
const SOCKET_PATH_LIMIT: usize = 108;
/// Longest wait for passt to create its socket.
const PASST_WAIT: Duration = Duration::from_secs(5);
/// Longest wait for the cloud-hypervisor API to answer.
const CH_WAIT: Duration = Duration::from_secs(10);
/// Serial log lines shown when readiness fails.
const SERIAL_TAIL_LINES: usize = 20;

/// Leases real microVMs on this host.
pub struct RealBackend {
    /// Resolved request configuration.
    config: Config,
    /// Lease state on disk.
    store: Store,
    /// This executable, used as the ssh `ProxyCommand`.
    exe: PathBuf,
}

impl RealBackend {
    /// Creates a backend whose state lives under `config.state_dir`.
    #[must_use]
    pub fn new(config: Config, exe: PathBuf) -> Self {
        let store = Store::new(&config.state_dir);
        Self { config, store, exe }
    }
}

/// Creates `path` (and parents) private to the user.
fn mkdir_private(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create dir: {e}"))
}

/// Removes a directory tree; a missing one is fine.
fn remove_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove run dir: {e}")),
    }
}

/// Polls `done` every 100 ms until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Last lines of the guest serial log, for failure diagnostics.
fn serial_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len().saturating_sub(SERIAL_TAIL_LINES);
    lines
        .iter()
        .skip(skip)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fails when the longest socket path in `run_dir` would not fit `sun_path`.
fn check_socket_len(run_dir: &Path) -> Result<(), String> {
    let longest = run_dir.join(state::LONGEST_SOCKET_NAME);
    if longest.as_os_str().len() >= SOCKET_PATH_LIMIT {
        return Err(format!(
            "runtime dir path is too long for unix sockets (limit {} bytes with lease id)",
            SOCKET_PATH_LIMIT - 1
        ));
    }
    Ok(())
}

/// Where a create gets its downloaded inputs.
struct Assets {
    /// Firmware file.
    firmware: PathBuf,
    /// Raw base image.
    base_raw: PathBuf,
}

impl RealBackend {
    /// Downloads (if needed) the firmware and image, and converts the image.
    fn ensure_assets(&self, diag: &mut dyn Write) -> Result<Assets, String> {
        let c = &self.config;
        let firmware = spec::firmware_cache_path(c);
        let image = spec::image_cache_path(c);
        let base_raw = spec::base_raw_path(c);
        mkdir_private(&c.cache_dir)?;
        let fw = &c.firmware;
        assets::ensure_download(&c.curl, &fw.url, &fw.sha256, &firmware, diag)?;
        let img = &c.image;
        assets::ensure_download(&c.curl, &img.url, &img.sha256, &image, diag)?;
        assets::ensure_raw(&c.qemu_img, &image, &base_raw, diag)?;
        Ok(Assets { firmware, base_raw })
    }

    /// Makes the key, disk and seed image for a new lease.
    fn prepare_guest(
        &self,
        record: &LeaseRecord,
        desired: &Desired,
        base_raw: &Path,
    ) -> Result<(), String> {
        let id = record.lease_id.as_str();
        let comment = format!("crabbox-{id}");
        let pubkey = host::ssh_keygen(&self.config.ssh_keygen, &self.store.key_path(id), &comment)?;
        let (host_private_key, host_public_key) = self.make_host_key(id)?;
        let size = self.config.disk_gib * 1024 * 1024 * 1024;
        host::clone_disk(base_raw, &self.store.disk_path(id), size)?;
        let instance_id = format!("{}-{}", record.cloud_id, record.created_at);
        let hostname = seed::hostname_for(&desired.name);
        let guest = self
            .restore_cache(id, &desired.cache_volumes)?
            .guest_setup();
        let params = SeedParams {
            instance_id: &instance_id,
            hostname: &hostname,
            ssh_public_key: &pubkey,
            host_private_key: &host_private_key,
            host_public_key: &host_public_key,
            files: &guest.files,
            commands: &guest.commands,
        };
        let (user_data, meta_data) = (seed::user_data(&params), seed::meta_data(&params));
        seed::write_seed(&self.store.seed_path(id), &user_data, &meta_data)
    }

    /// Starts passt then cloud-hypervisor, saving each pid as soon as it exists.
    fn start_processes(
        &self,
        record: &mut LeaseRecord,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let id = record.lease_id.clone();
        let log = self.store.process_log(&id);
        let passt_sock = record.passt_socket();
        let args = ["--vhost-user", "-f", "-s"].map(OsString::from);
        let args = [&args[..], &[passt_sock.clone().into_os_string()]].concat();
        let passt_pid = host::spawn_detached(&self.config.passt, &args, &log)?;
        record.set_passt_pid(passt_pid);
        self.store.write(record)?;
        if !wait_until(PASST_WAIT, || passt_sock.exists()) {
            return Err("passt did not create its socket".to_owned());
        }
        let _ = writeln!(diag, "passt ready");
        let args = [
            OsString::from("--api-socket"),
            record.api_socket().into_os_string(),
        ];
        let pid = host::spawn_detached(&self.config.cloud_hypervisor, &args, &log)?;
        record.set_ch_pid(pid);
        self.store.write(record)?;
        let client = ChClient::new(&record.api_socket());
        if !wait_until(CH_WAIT, || client.ping().is_ok()) {
            return Err("cloud-hypervisor API did not come up".to_owned());
        }
        let _ = writeln!(diag, "cloud-hypervisor ready");
        Ok(())
    }

    /// Creates and boots the VM on already-running processes.
    fn boot_vm(
        &self,
        record: &LeaseRecord,
        firmware: &Path,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let id = record.lease_id.as_str();
        let mut config = spec::vm_config(
            &self.config,
            firmware,
            &self.store.disk_path(id),
            &self.store.seed_path(id),
            &self.store.serial_log(id),
            record,
        );
        self.read_cache(id).attach(&mut config);
        let client = ChClient::new(&record.api_socket());
        client.create(&config)?;
        client.boot()?;
        let _ = writeln!(diag, "vm booted");
        Ok(())
    }

    /// Builds everything for a lease and boots it; the caller cleans up on error.
    fn provision(
        &self,
        desired: &Desired,
        run_dir: &Path,
        assets: &Assets,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        let id = desired.lease_id.as_str();
        let mut record = LeaseRecord {
            lease_id: id.to_owned(),
            slug: desired.slug.clone(),
            name: desired.name.clone(),
            cloud_id: backend::cloud_id(id),
            created_at: state::now_unix(),
            ch_pid: None,
            passt_pid: None,
            run_dir: run_dir.to_path_buf(),
            boot_id: None,
            ch_start: None,
            passt_start: None,
            watch_pid: None,
            watch_start: None,
        };
        remove_tree(run_dir)?;
        self.store.write(&record)?;
        mkdir_private(run_dir)?;
        self.prepare_guest(&record, desired, &assets.base_raw)?;
        let _ = writeln!(diag, "guest files ready");
        self.start_processes(&mut record, diag)?;
        self.boot_vm(&record, &assets.firmware, diag)?;
        self.start_watcher(&mut record, diag)?;
        Ok(record)
    }
}

impl Backend for RealBackend {
    fn find(&self, lease_id: &str) -> Result<Option<LeaseRecord>, String> {
        self.store.read(lease_id)
    }

    fn list(&self) -> Result<Vec<LeaseRecord>, String> {
        self.store.list()
    }

    fn orphans(&self) -> Result<Vec<String>, String> {
        self.store.orphans()
    }

    fn alive(&self, record: &LeaseRecord) -> bool {
        Self::ch_alive(record)
    }

    fn power(&self, record: &LeaseRecord) -> Power {
        Self::power_of(record)
    }

    fn pause(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        self.pause_vm(record, diag)
    }

    fn resume(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        self.resume_vm(record, diag)
    }

    fn host_key(&self, lease_id: &str) -> Result<String, String> {
        self.read_host_key(lease_id)
    }

    fn growth_blocked(&self, lease_id: &str) -> String {
        fs::read_to_string(self.store.grow_blocked_path(lease_id)).unwrap_or_default()
    }

    fn reap(&self, record: &LeaseRecord, diag: &mut dyn Write) {
        if let Err(e) = Self::reap_orphan_passt(record, diag) {
            let _ = writeln!(diag, "warn: {e}");
        }
    }

    fn cache_echo(&self, lease_id: &str) -> Vec<CacheVolumeEcho> {
        self.read_cache(lease_id).echo()
    }

    fn cache_gc(&self, dry_run: bool, diag: &mut dyn Write) -> String {
        self.gc_cache(dry_run, diag)
    }

    fn try_lock(&self, lease_id: &str) -> Result<Option<LeaseLock>, String> {
        self.store.try_lock(lease_id)
    }

    fn lock(&self, lease_id: &str) -> Result<LeaseLock, String> {
        self.store.lock(lease_id)
    }

    fn create(&self, desired: &Desired, diag: &mut dyn Write) -> Result<LeaseRecord, String> {
        let id = desired.lease_id.as_str();
        state::validate_lease_id(id)?;
        let run_dir = self.config.runtime_dir.join(state::run_dir_name(id));
        check_socket_len(&run_dir)?;
        let assets = self.ensure_assets(diag)?;
        match self.provision(desired, &run_dir, &assets, diag) {
            Ok(record) => Ok(record),
            Err(error) => {
                if let Err(e) = self.destroy(id, diag) {
                    let _ = writeln!(diag, "cleanup after failed create also failed: {e}");
                }
                Err(error)
            }
        }
    }

    fn wait_ready(&self, record: &LeaseRecord, diag: &mut dyn Write) -> Result<(), String> {
        let id = record.lease_id.as_str();
        let c = &self.config;
        let probe = spec::guest_ssh(&c.ssh, &self.store, &self.exe, &c.state_dir, id);
        let result = probe.wait(
            self.config.ready_timeout,
            Duration::from_secs(1),
            &mut || self.alive(record),
            diag,
        );
        if result.is_err() {
            let _ = writeln!(
                diag,
                "serial log tail:\n{}",
                serial_tail(&self.store.serial_log(id))
            );
        }
        result
    }

    fn destroy(&self, lease_id: &str, diag: &mut dyn Write) -> Result<(), String> {
        state::validate_lease_id(lease_id)?;
        let record = self.store.read(lease_id).unwrap_or(None);
        let run_dir = record.as_ref().map_or_else(
            || self.config.runtime_dir.join(lease_id),
            |r| r.run_dir.clone(),
        );
        let quiesced = record
            .as_ref()
            .is_some_and(|r| self.quiesce_for_cache(r, diag));
        if let Some(record) = &record {
            Self::stop_processes(record, diag)?;
        }
        self.settle_cache(lease_id, quiesced, diag);
        remove_tree(&run_dir)?;
        self.store.remove(lease_id)
    }

    fn ssh_key(&self, lease_id: &str) -> PathBuf {
        self.store.key_path(lease_id)
    }

    fn proxy_command(&self, lease_id: &str) -> String {
        spec::proxy_command(&self.exe, lease_id, &self.config.state_dir)
    }
}

#[cfg(test)]
mod tests;
