//! Restoring a lease from the boot template and rotating its identity.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::rotate::{self, Identity};
use super::{Template, BOOTSTRAP_KEY, DISK_FILE, KNOWN_HOSTS, SNAP_DIR};
use crate::backend::Backend;
use crate::ch::ChClient;
use crate::host::link_or_copy;
use crate::protocol::Desired;
use crate::ready::ReadyProbe;
use crate::seed;
use crate::size::Size;
use crate::state::{self, LeaseRecord};
use crate::vm::{mkdir_private, remove_tree, spec, DisksFrom, RealBackend};
use crate::volumes::guest::GuestSetup;

/// Restore source staged in a lease dir; removed once the VM is paused.
const STAGE_DIR: &str = "boot-snap";
/// Snapshot files a lease shares with its template; only `config.json` differs.
const SHARED_FILES: [&str; 2] = ["memory-ranges", "state.json"];
/// Timeout for each API call around the restore.
const API_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest one login check may take.
const CHECK_LIMIT: Duration = Duration::from_secs(30);
/// Longest the rotation script may take.
const ROTATE_LIMIT: Duration = Duration::from_secs(120);
/// Bytes of host randomness mixed into each clone.
const ENTROPY_BYTES: usize = 64;

/// Points a snapshot's `config.json` at one lease's disk, seed, serial and sockets.
pub(super) fn retarget(
    config: &mut Value,
    paths: spec::VmPaths<'_>,
    record: &LeaseRecord,
) -> Result<(), String> {
    let values = [
        ("/payload/firmware", json!(paths.firmware)),
        ("/disks/0/path", json!(paths.disk)),
        ("/disks/1/path", json!(paths.seed)),
        ("/serial/file", json!(paths.serial)),
        ("/net/0/vhost_socket", json!(record.passt_socket())),
        ("/vsock/socket", json!(record.vsock_socket())),
    ];
    for (pointer, value) in values {
        *config
            .pointer_mut(pointer)
            .ok_or_else(|| format!("template config.json has no {pointer}"))? = value;
    }
    Ok(())
}

/// A dir laid out as a template: `snap/`, disks, the bootstrap key and the
/// host pin.
pub(in crate::vm) struct Origin<'a> {
    /// The dir.
    pub dir: &'a Path,
    /// What the log calls it.
    pub label: &'a str,
    /// Where the lease's disks come from.
    pub disks: DisksFrom<'a>,
}

impl RealBackend {
    /// Gives a new lease its files, stages the template snapshot for it and
    /// restores it running, with its cache disks hot-added.
    pub(super) fn restore_template(
        &self,
        desired: &Desired,
        run_dir: &Path,
        t: &Template,
        size: Size,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        let label = format!("template {}", t.meta.key12());
        let disk = t.dir.join(DISK_FILE);
        let origin = Origin {
            dir: &t.dir,
            label: &label,
            disks: DisksFrom::Image(&disk),
        };
        self.restore_from(desired, run_dir, &origin, size, diag)
    }

    /// Like [`Self::restore_template`] for any dir laid out as a template. Cache
    /// disks its snapshot names are pointed at the lease's copies; the rest
    /// are hot-added.
    pub(in crate::vm) fn restore_from(
        &self,
        desired: &Desired,
        run_dir: &Path,
        origin: &Origin<'_>,
        size: Size,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        let started = Instant::now();
        let id = desired.lease_id.as_str();
        let mut record = spec::new_record(id, &desired.slug, &desired.name, run_dir, size);
        remove_tree(run_dir)?;
        self.store.write(&record)?;
        mkdir_private(run_dir)?;
        self.prepare_guest(&record, desired, origin.disks, diag)?;
        let stage = self.store.lease_dir(id).join(STAGE_DIR);
        let hot = self.stage_snapshot(&origin.dir.join(SNAP_DIR), &stage, &record)?;
        let client = ChClient::new(&record.api_socket()).with_timeout(API_TIMEOUT);
        let restored = self
            .start_processes(&mut record, size, Some(&stage), diag)
            .and_then(|()| self.wait_state(&client, "Paused"));
        let _ = remove_tree(&stage);
        restored?;
        client.resume()?;
        self.wait_state(&client, "Running")?;
        let secs = started.elapsed().as_secs_f64();
        let _ = writeln!(diag, "restored from {} in {secs:.1}s", origin.label);
        for disk in &hot {
            client.add_disk(disk)?;
        }
        self.start_watcher(&mut record, diag)?;
        Ok(record)
    }

    /// Links the shared snapshot files into `stage` and writes its retargeted
    /// config; returns the cache disks it does not name.
    fn stage_snapshot(
        &self,
        snap: &Path,
        stage: &Path,
        record: &LeaseRecord,
    ) -> Result<Vec<Value>, String> {
        remove_tree(stage)?;
        mkdir_private(stage)?;
        for name in SHARED_FILES {
            link_or_copy(&snap.join(name), &stage.join(name))?;
        }
        let text = fs::read(snap.join("config.json"))
            .map_err(|e| format!("read template config.json: {e}"))?;
        let mut config: Value = serde_json::from_slice(&text)
            .map_err(|e| format!("parse template config.json: {e}"))?;
        let id = record.lease_id.as_str();
        let paths = spec::VmPaths {
            firmware: &spec::firmware_cache_path(&self.config),
            disk: &self.store.disk_path(id),
            seed: &self.store.seed_path(id),
            serial: &self.store.serial_log(id),
        };
        retarget(&mut config, paths, record)?;
        let hot = self.read_cache(id).retarget(&mut config)?;
        fs::write(stage.join("config.json"), config.to_string())
            .map_err(|e| format!("write config.json: {e}"))?;
        Ok(hot)
    }

    /// Rotates the clone's identity; a lease that fails it is destroyed.
    pub(super) fn take_identity(
        &self,
        record: LeaseRecord,
        t: &Template,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        let alias = format!("crabbox-{}", t.meta.guest_id());
        self.take_identity_from(record, &t.dir, alias, diag)
    }

    /// Like [`Self::take_identity`] for any template-shaped `source` dir whose
    /// guest was pinned under host key alias `alias`.
    pub(in crate::vm) fn take_identity_from(
        &self,
        record: LeaseRecord,
        source: &Path,
        alias: String,
        diag: &mut dyn Write,
    ) -> Result<LeaseRecord, String> {
        match self.rotate_from(&record, source, alias, diag) {
            Ok(()) => Ok(record),
            Err(e) => Err(match self.destroy(&record.lease_id, diag) {
                Ok(()) => format!("{e}; lease destroyed"),
                Err(d) => format!("{e}; destroying the lease also failed: {d}"),
            }),
        }
    }

    /// The lease's identity as its cold-boot seed would give it.
    fn identity(&self, record: &LeaseRecord) -> Result<Identity, String> {
        let id = record.lease_id.as_str();
        let read =
            |p: PathBuf| fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()));
        let mut lease_pub = self.store.key_path(id).into_os_string();
        lease_pub.push(".pub");
        let mut entropy = vec![0; ENTROPY_BYTES];
        File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut entropy))
            .map_err(|e| format!("read host entropy: {e}"))?;
        let mut guest: GuestSetup = self.read_cache(id).guest_setup();
        crate::reclaim::add_to(&mut guest, &self.config.reclaim);
        Ok(Identity {
            epoch: state::now_unix(),
            entropy,
            hostname: seed::hostname_for(&record.name),
            host_private_key: read(self.store.host_key_path(id))?,
            host_public_key: self.read_host_key(id)?,
            lease_public_key: read(PathBuf::from(lease_pub))?,
            guest,
        })
    }

    /// Logs in with the bootstrap key, rotates, then proves the lease key logs
    /// in against the new host key and the bootstrap key no longer does.
    /// Rotation against the guest in `source`, which logs in with its bootstrap
    /// key and answers under host key alias `alias`.
    fn rotate_from(
        &self,
        record: &LeaseRecord,
        source: &Path,
        alias: String,
        diag: &mut dyn Write,
    ) -> Result<(), String> {
        let started = Instant::now();
        let c = &self.config;
        let id = record.lease_id.as_str();
        let lease = spec::guest_ssh(&c.ssh, &self.store, &self.exe, &c.state_dir, id);
        let bootstrap = |known_hosts: PathBuf, host_key_alias: String| ReadyProbe {
            key: source.join(BOOTSTRAP_KEY),
            known_hosts,
            host_key_alias,
            ..spec::guest_ssh(&c.ssh, &self.store, &self.exe, &c.state_dir, id)
        };
        let old = bootstrap(source.join(KNOWN_HOSTS), alias);
        old.wait(
            c.ready_timeout,
            Duration::from_millis(200),
            &mut || self.alive(record),
            diag,
        )?;
        let script = rotate::script(&self.identity(record)?);
        old.output(
            &format!("sudo sh -c {}", spec::shell_quote(&script)),
            ROTATE_LIMIT,
        )
        .map_err(|e| format!("identity rotation failed: {e}"))?;
        lease
            .output("true", CHECK_LIMIT)
            .map_err(|e| format!("lease key cannot log in after identity rotation: {e}"))?;
        let stale = bootstrap(lease.known_hosts, lease.host_key_alias);
        match stale.output("true", CHECK_LIMIT) {
            Ok(_) => Err("bootstrap key still logs in after identity rotation".to_owned()),
            Err(e) if e.contains("Permission denied") => {
                let _ = writeln!(diag, "bootstrap key refused: {e}");
                let secs = started.elapsed().as_secs_f64();
                let _ = writeln!(diag, "identity rotated in {secs:.1}s");
                Ok(())
            }
            Err(e) => Err(format!("cannot confirm the bootstrap key is refused: {e}")),
        }
    }
}
