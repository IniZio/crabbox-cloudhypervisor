//! The watcher against a live cloud-hypervisor, host and guest.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::{parse_sample, watch_loop, Log, Machine, WatchSpec, GROW_FS_SCRIPT, INTERVAL};
use super::{SAMPLE_SCRIPT, SETTLE};
use crate::ch::ChClient;
use crate::grow::{Current, Planner, Sample};
use crate::ready::ReadyProbe;
use crate::state::{self, LeaseRecord, Store};
use crate::vm::spec;

/// Longest a sample may take.
const SAMPLE_LIMIT: Duration = Duration::from_secs(30);
/// Longest a partition and filesystem grow may take.
const GROW_FS_LIMIT: Duration = Duration::from_secs(120);

/// The live VMM, host and guest of one lease.
struct RealMachine {
    /// The lease, for its process identity.
    record: LeaseRecord,
    /// cloud-hypervisor API.
    client: ChClient,
    /// ssh into the guest.
    ssh: ReadyProbe,
    /// Root disk image.
    disk: PathBuf,
    /// Where a blocked disk grow is recorded for the lease status.
    blocked: PathBuf,
}

impl Machine for RealMachine {
    fn alive(&self) -> bool {
        crate::vm::RealBackend::ch_alive(&self.record)
    }

    fn running(&self) -> bool {
        let client = self.client.clone().with_timeout(Duration::from_secs(5));
        client.state().as_deref() == Ok("Running")
    }

    fn current(&self) -> Result<Current, String> {
        let info = self.client.info()?;
        let field = |path: &str| {
            info.pointer(path)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("cloud-hypervisor vm.info: missing {path}"))
        };
        Ok(Current {
            disk_bytes: fs::metadata(&self.disk)
                .map_err(|e| format!("stat root disk: {e}"))?
                .len(),
            vcpus: u32::try_from(field("/config/cpus/boot_vcpus")?)
                .map_err(|_| "cloud-hypervisor vm.info: vcpus out of range".to_owned())?,
            memory_bytes: field("/config/memory/size")?,
        })
    }

    fn sample(&self) -> Result<Sample, String> {
        parse_sample(&self.ssh.output(SAMPLE_SCRIPT, SAMPLE_LIMIT)?)
    }

    fn host_free(&self) -> Result<u64, String> {
        let dir = self.disk.parent().unwrap_or_else(|| Path::new("/"));
        crate::host::free_bytes(dir)
    }

    fn set_blocked(&self, reason: Option<&str>) {
        let _ = reason.map_or_else(
            || fs::remove_file(&self.blocked),
            |text| fs::write(&self.blocked, text),
        );
    }

    fn grow_disk(&self, bytes: u64) -> Result<(), String> {
        self.client.resize_disk(spec::ROOT_DISK_ID, bytes)
    }

    fn grow_fs(&self) -> Result<u64, String> {
        let out = self.ssh.output(GROW_FS_SCRIPT, GROW_FS_LIMIT)?;
        out.trim()
            .parse()
            .map_err(|_| "grow fs: cannot read the new size".to_owned())
    }

    fn resize(&self, vcpus: Option<u32>, ram_bytes: Option<u64>) -> Result<(), String> {
        self.client.resize(vcpus, ram_bytes)
    }
}

/// Runs the watcher for `lease_id` until its cloud-hypervisor exits.
pub fn run(lease_id: &str, state_dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    state::validate_lease_id(lease_id)?;
    let store = Store::new(state_dir);
    let record = store
        .read(lease_id)?
        .ok_or_else(|| format!("lease {lease_id} not found"))?;
    let text =
        fs::read(store.watch_spec_path(lease_id)).map_err(|e| format!("read watch spec: {e}"))?;
    let watch: WatchSpec =
        serde_json::from_slice(&text).map_err(|e| format!("parse watch spec: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate adapter binary: {e}"))?;
    let machine = RealMachine {
        client: ChClient::new(&record.api_socket()),
        ssh: spec::guest_ssh(&watch.ssh, &store, &exe, state_dir, lease_id),
        disk: store.disk_path(lease_id),
        blocked: store.grow_blocked_path(lease_id),
        record,
    };
    let g = watch.growth;
    let mut log = Log::new(out);
    log.note(&format!(
        "watching {lease_id}: disk up to {} GiB, vCPUs up to {}, memory up to {} MiB",
        g.disk_max_gib, g.cpus_max, g.memory_mib_max
    ));
    watch_loop(&machine, &mut Planner::new(g), INTERVAL, SETTLE, &mut log);
    Ok(())
}
