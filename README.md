# crabbox-cloudhypervisor

A [crabbox](https://github.com/openclaw/crabbox) external provider that leases
local, rootless [cloud-hypervisor](https://www.cloudhypervisor.org/) microVMs.
Each lease is a fresh Ubuntu 24.04 guest reachable over SSH, with outbound
network access. It speaks crabbox external provider protocol v1 (JSON on
stdin/stdout) and supports `doctor`, `acquire`, `resolve`, `list`, `release`, `pause`,
`resume`, `touch` and `cleanup`.

## Host prerequisites

- Linux x86_64. No root needed.
- Read/write access to `/dev/kvm` (usually membership of the `kvm` group).
  `/dev/vhost-vsock` should also be accessible; `doctor` warns if it is not.
- `cloud-hypervisor` (tested with v52.0).
- `passt` with `--vhost-user` support (tested with 2026_06_11).
- `qemu-img`, `ssh`, `ssh-keygen`, `curl`, `setsid`.
- crabbox itself, with `rsync` and `git` for syncing.

## Install

```sh
cargo install --path .
# or
cargo build --release   # target/release/crabbox-cloudhypervisor
```

## crabbox config

Put this in your trusted user config (not the repository config). Use the
absolute path of the binary.

```yaml
provider: external
target: linux
external:
  command: /home/you/.cargo/bin/crabbox-cloudhypervisor
  capabilities:
    idempotentLeaseId: true
  connection:
    ssh:
      trustProviderOutput: true
  workRoot: /home/crabbox/work
  config:
    cpus: 2
    memoryMiB: 4096
```

The guest user is `crabbox`, so `workRoot` should be under `/home/crabbox`.
Then use crabbox as usual:

```sh
crabbox warmup
crabbox run --id <slug> -- uname -a
crabbox list
crabbox stop <slug>
```

## Provider config keys

All keys go under `external.config`. All are optional.

| Key | Default | Meaning |
| --- | --- | --- |
| `cpus` | `2` | vCPUs the guest boots with (1-64) |
| `cpusMax` | `2 × cpus`, at most 64 | most vCPUs the guest is raised to under sustained CPU pressure (`cpus`-64) |
| `memoryMiB` | `4096` | memory the guest boots with, at least 512. Memory the guest frees is returned to the host |
| `memoryMiBMax` | `2 × memoryMiB` | most memory the guest is raised to under sustained memory pressure (at least `memoryMiB`) |
| `diskGiB` | `20` | guest disk size at create, at least 8 |
| `diskMaxGiB` | `100`, or `diskGiB` if larger | largest the disk grows to (at least `diskGiB`) |
| `diskGrowStepGiB` | `10` | GiB added per disk grow (at least 1) |
| `diskGrowFreePercent` | `15` | grow when free space on `/` falls below this percent of its size (1-90)... |
| `diskGrowFreeGiB` | `4` | ...or below this many GiB, whichever is larger |
| `readyTimeoutSeconds` | `300` | wait for the guest to become ready (1-3600) |
| `stateDir` | `$XDG_STATE_HOME/crabbox-cloudhypervisor` | per-lease state (absolute path) |
| `runtimeDir` | `$XDG_RUNTIME_DIR/crabbox-cloudhypervisor`, else `<stateDir>/run` | unix sockets (absolute path, at most 85 bytes) |
| `cacheDir` | `$XDG_CACHE_HOME/crabbox-cloudhypervisor` | downloaded assets (absolute path) |
| `image` | pinned Ubuntu image | `{url, sha256}` override |
| `firmware` | pinned CLOUDHV.fd | `{url, sha256}` override |
| `cloudHypervisor`, `passt`, `qemuImg`, `ssh`, `sshKeygen`, `curl` | looked up on `PATH` | binary paths |

Without the `XDG_*` variables, state and cache fall back to
`~/.local/state` and `~/.cache`.

## Growth while a Box runs

Every lease runs a watcher process (`crabbox-cloudhypervisor watch <leaseId>
--state-dir <dir>`) next to its cloud-hypervisor. Every 10 seconds it reads the
guest over ssh (root fs size and free space, `/proc/stat`, `/proc/meminfo`,
`nproc`) and grows what is short, without a reboot:

- **Disk.** When free space on `/` falls below the floor, `vm.resize-disk` grows
  the image by `diskGrowStepGiB` (never past `diskMaxGiB`), then the guest runs
  `growpart` and `resize2fs` on the root partition. If that guest step fails, the
  watcher retries it before it grows the image again. A grow needs the host
  filesystem holding the image to have room for the whole step; otherwise the
  disk stays as it is, and `list` and `resolve` report why in the lease's
  `growthBlocked` field.
- **vCPUs.** The VM boots with `max_vcpus = cpusMax`. After 3 samples in a row
  (30 seconds) above 90% CPU busy, `vm.resize` doubles the vCPU count, up to
  `cpusMax`. A udev rule from the seed brings the new vCPUs online.
- **Memory.** The VM boots with `hotplug_size` = `memoryMiBMax - memoryMiB`
  (ACPI hotplug). After 2 samples in a row above 80% memory in use (from
  `MemAvailable`), `vm.resize` doubles memory in whole 128 MiB blocks, up to
  `memoryMiBMax`. The trigger is earlier than CPU's because the guest's OOM
  killer acts before memory is full. ACPI-added memory stays until the VM stops.

Raises last until the VM stops; a cold boot (`resume` of a stopped lease) starts
at `cpus` and `memoryMiB` again, with the disk at its grown size. Every grow and
every failure goes to `<stateDir>/leases/<leaseId>/grow.log`, one timestamped
line each; a failure that repeats is written once, then again with its count
when it clears. The watcher skips paused VMs and exits when its cloud-hypervisor
exits; `release` and `stop` also stop it. `resume` starts a watcher when the
lease has none running, and never a second one.

## What is pinned

- Image: Ubuntu noble cloud image `20260926` (amd64), SHA-256 checked.
- Firmware: cloud-hypervisor edk2 `CLOUDHV.fd` (`ch-97eeb7b09`), SHA-256 checked.

The first `acquire` downloads both (about 625 MB) and converts the image to a
raw base once. Later leases copy that base (reflink when the filesystem
supports it) into a private disk.

## How SSH works

Guests have no routable address. Each lease gets a vsock device, and the guest
runs sshd on vsock port 22. The lease tells crabbox to connect through a
`ProxyCommand`:

```
crabbox-cloudhypervisor proxy <leaseId> --state-dir <stateDir>
```

The `proxy` subcommand connects to the lease's cloud-hypervisor vsock socket
and relays stdin/stdout to guest port 22. Each lease has its own SSH key.
Outbound traffic goes through passt (NAT and DNS, no inbound).

The provider generates a per-lease ed25519 host key and injects it through
cloud-init (sshd offers only that key). Every `acquire`, `resolve` and `resume`
returns it as `ssh.hostKey` (`ssh-ed25519 AAAA...`, no hostname or comment), so
it never rotates. The provider uses it for its own readiness check. The crabbox
fork (branch `moor/main`) pins `ssh.hostKey` for the lease and keeps enforcing an
earlier pin when a response omits it; upstream crabbox ignores it.

## Pause and resume

- `pause` runs cloud-hypervisor `vm.pause`. RAM stays allocated.
- `resume` resumes a paused VM in place. If the VM is dead (host reboot, killed
  process) it cold-boots from the lease's existing disk and seed.
- Snapshot to disk is future work.

Needs the crabbox fork (branch `moor/external-pause-resume`) and
`capabilities.pauseResume: true` in the trusted crabbox user config.

Lease status is `ready` (running), `paused`, `stopped` (VM dead) or
`unresponsive` (VM process alive but its API does not answer, for example
frozen with SIGSTOP). `resume` and non-`releaseOnly` `resolve` on an
`unresponsive` lease fail with
`lease <id>: cloud-hypervisor is alive but not responding` and never cold
boot; `release` kills the process and cleans up, and `cleanup` leaves it alone.
`resolve` on a paused or stopped lease returns the lease with its status, which
is what crabbox `status`, `extend` and heartbeat need; crabbox itself refuses to
open a session on it. `acquire` of such a lease fails with
`lease <id> is stopped; run crabbox resume`. crabbox does not know the
`unresponsive` status, so `resolve` keeps failing for it instead of returning it.

The lease record keeps each process's start time and the boot id next to its
pid. A pid that now belongs to another process, or that survives a reboot, is
never signalled. `list` and `cleanup` skip a lease that an operation currently
holds, so a create or resume in flight keeps its passt.

## State and cache

- Leases: `<stateDir>/leases/<leaseId>/` (lease.json, disk, seed image, SSH key,
  console log, watch.json, grow.log).
- Sockets: `<runtimeDir>/<12-hex hash of the lease id>/` (`a`, `v`, `p`,
  `p.repair`).
- Downloads and the converted base image: `<cacheDir>`.

`release` removes the lease directory. `cloud-hypervisor`, `passt` and the
growth watcher run detached per lease and are stopped on release. Only `release` destroys a
lease.

`cleanup` never deletes stopped or paused leases. It reaps orphan `passt`
processes (VM dead, passt alive) and removes broken lease directories (no
readable `lease.json`, for example from a crashed acquire). `dryRun` reports
what it would remove without deleting.

Opt-in: set `reapStoppedAfterHours` (integer >= 1; absent = off) in the
provider config and `cleanup` also deletes stopped leases whose age exceeds
that many hours. Age is `now - createdAt` from `lease.json`, so a long-lived
lease that stops is eligible at once.

## Doctor

`crabbox doctor` calls the provider's `doctor` op. It checks `/dev/kvm`,
`/dev/vhost-vsock`, the host tools and their versions, that the state and
runtime directories are writable, that socket paths fit the 108-byte limit
(`runtimeDir` at most 85 bytes), and whether the firmware and image are cached
and verified. Warnings and failures are also written to stderr, so crabbox
shows them, not just `doctor finished`.

## Limitations

- amd64 only.
- Disk growth assumes an ext4 root on a partition (`growpart` + `resize2fs`), as
  in the pinned Ubuntu image. Disks and vCPUs only grow; they never shrink.
- Leases are not stopped on a timer by the provider. Release them with
  `crabbox stop`.
- Pause keeps the guest RAM allocated on the host (cloud-hypervisor
  `vm.pause`); snapshot-to-disk is future work.
- Pause/resume needs the crabbox fork (`moor/external-pause-resume`) with
  `capabilities.pauseResume: true` in trusted user config.
- Upstream crabbox ignores `ssh.hostKey` (see above).
