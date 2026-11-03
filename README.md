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
- `cloud-hypervisor` (tested with v52.0). Checkpoints need v52.0 with the
  vhost-user resume patch, [v52.0-moor.1](https://github.com/IniZio/cloud-hypervisor/releases/tag/v52.0-moor.1)
  (`cloud-hypervisor-static`). With stock v52.0 the guest network is dead after
  `checkpointCreate`.
- `passt` with `--vhost-user` support (tested with 2026_06_11).
- `qemu-img`, `ssh`, `ssh-keygen`, `curl`, `setsid`.
- For cache volumes: `mke2fs`, `e2fsck`, `resize2fs` and `debugfs` (e2fsprogs). Without
  them every cache volume is skipped and the Box starts cold.
- crabbox itself, with `rsync` and `git` for syncing.
- With `cgroup: systemd` (the default): a systemd user manager for the user that
  runs crabbox, with the `cpu` and `memory` controllers delegated to it. Stock
  systemd delegates them. A user without a login session has no user manager
  until `loginctl enable-linger <user>` is run once. See
  [Per-lease limits](#per-lease-limits).

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
| `memoryMiB` | `4096` | memory the guest boots with, at least 512 |
| `classes` | the ladder below | per-class `{cpus, memoryMiB}` that replaces a built-in class or adds one; a lease with a class boots at its size instead of `cpus`/`memoryMiB` |
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
| `cacheVolumeDir` | `<stateDir>/volumes` | cache volume store (absolute path) |
| `cacheStoreMaxGB` | `200` | store budget in GiB; least recently used groups go first above it |
| `cacheMaxVolumeGB` | `40` | largest group filesystem in GiB |
| `cacheKeyTtlDays` | `30` | `cleanup` removes groups unused this many days |
| `cgroup` | `systemd` | `systemd` runs each lease in systemd user scopes with kernel CPU and memory limits; `off` runs plain processes with no limits |
| `vmmOverheadMiB` | `512` | MiB a VM scope may use above the class memory, for cloud-hypervisor itself (at least 64) |
| `guestReclaimIntervalSecs` | `60` | seconds between guest file cache trims (at least 1) |
| `guestReclaimFloorMiB` | `512` | file cache a guest keeps; each trim asks for the rest back |
| `guestReclaimRefaultMiB` | `16` | file re-read from disk between trims, in MiB, above which... |
| `guestReclaimSkipRuns` | `10` | ...the next this many trims are skipped |
| `pageReportingOrder` | `2` | smallest free block, 2^order pages, the guest reports to the host (0-10) |
| `bootTemplate` | `false` | `true` restores new leases from a boot template snapshot instead of cold booting them; see [Boot template](#boot-template) |

Without the `XDG_*` variables, state and cache fall back to
`~/.local/state` and `~/.cache`.

### Lease classes

`crabbox warmup --class <name>` sizes the VM from the class. Without a class the VM
boots at `cpus` and `memoryMiB`.

| Class | vCPUs | Memory |
| --- | --- | --- |
| `standard` | 2 | 4 GiB |
| `fast` | 4 | 8 GiB |
| `large` | 8 | 16 GiB |
| `beast` | 16 | 32 GiB |

Override or add classes under `external.config`:

```yaml
classes:
  fast: {cpus: 6, memoryMiB: 12288}
  tiny: {cpus: 1, memoryMiB: 1024}
```

A class the host cannot hold fails the lease rather than booting a smaller VM, for
example `class beast needs 16 vCPUs; this host has 8` or
`class large needs 16384 MiB of memory; this host has 10000 MiB`. An unknown
class fails and lists the known ones. A cold boot of a stopped lease keeps its class
size.

A VM gets its class size at boot, and the host admits a class against its totals
(`MemTotal` and logical CPUs), not what is free at that moment. Guest memory is
backed lazily and freed pages are reported back, so only memory the guest has
touched is claimed on the host. Setting the removed keys `cpusMax` or
`memoryMiBMax` fails the config with a message saying so; delete them.

## Guest memory reclaim

A guest keeps file cache until it needs the memory, so after a big build the host
stays charged for pages the guest no longer uses. Each new lease runs a systemd timer
that writes to the guest's root `memory.reclaim`, asking for file cache above
`guestReclaimFloorMiB`. Freed pages go back to the host through free page reporting,
which the guest sets to report blocks of `pageReportingOrder` at boot (Linux
defaults to 2 MiB blocks; order 2 is 16 KiB).

- When more than `guestReclaimRefaultMiB` was read back from disk since the last
  trim, the cache was still in use: the next `guestReclaimSkipRuns` trims are
  skipped, about 10 minutes at the defaults.
- A guest with swap is left alone, since `memory.reclaim` would also swap out
  anonymous memory. Linux 6.8 has no `swappiness=` argument to stop that.
- The timer and setting are written by the seed when a lease is created, so leases
  created before an upgrade or a config change keep their old behaviour.

## Per-lease limits

With `cgroup: systemd`, each lease runs in three systemd user scopes:

| Scope | Holds | Limits |
| --- | --- | --- |
| `crabbox-<leaseId>.scope` | cloud-hypervisor | `CPUQuota=<cpus*100>%`, `MemoryMax=<class memory + vmmOverheadMiB>`, `MemorySwapMax=0` |
| `crabbox-<leaseId>-passt.scope` | passt | none |
| `crabbox-<leaseId>-watch.scope` | the growth watcher | none |

The VM's quota matches its class, so one busy guest cannot take CPU from the
others. The memory limit is a safety net above what the guest can use, not a
squeeze. A VMM that goes past it is killed by the kernel, and only that VM goes:
`crabbox resume` and `crabbox stop` print that the lease was OOM-killed, and so
does a failed readiness wait. The root disk is opened with `O_DIRECT`, so the
host page cache for the disk image is not charged to the scope.

passt and the watcher get their own scopes rather than the VM's, so the VM's
quota covers only the guest, and so all three outlive the process that started
them. A service manager that restarts crabbox's caller does not take the
lease's processes down with it. `crabbox stop` stops all three scopes; a scope
that is already gone is not an error.

With no user manager, the lease fails before anything is built, naming the fix:
`run loginctl enable-linger <user>`. It never falls back to running without
limits. Set `cgroup: off` to run that way on purpose.

If systemd-oomd watches `user@.service` with `ManagedOOMMemoryPressure=kill`
(the default on some distributions), it may kill a whole VM scope when the host
is under memory pressure, independently of these limits.

## Growth while a Box runs

Every lease runs a watcher process (`crabbox-cloudhypervisor watch <leaseId>
--state-dir <dir>`) next to its cloud-hypervisor. Every 10 seconds it reads the
guest over ssh (root fs size and free space) and grows the disk when it is
short, without a reboot:

- **Disk.** When free space on `/` falls below the floor, `vm.resize-disk` grows
  the image by `diskGrowStepGiB` (never past `diskMaxGiB`), then the guest runs
  `growpart` and `resize2fs` on the root partition. If that guest step fails, the
  watcher retries it before it grows the image again. A grow needs the host
  filesystem holding the image to have room for the whole step; otherwise the
  disk stays as it is, and `list` and `resolve` report why in the lease's
  `growthBlocked` field.

## Memory while a Box runs

The VM boots at its full class size with a balloon at 0. The watcher registers a
pressure trigger on the host's `/proc/pressure/memory` and waits on it, so
there is no polling for memory. Each time it fires the watcher inflates the
balloon by `balloonStepPercent` of the class memory (`vm.resize`), never past
class memory minus `balloonFloorMiB`; the guest frees page cache to fit. After
`balloonCooldownSecs` with no event it deflates by the same step, down to 0.
The guest can also take memory back from the balloon on its own when it runs out.

| Key | Default | Meaning |
| --- | --- | --- |
| `balloon` | `on` | `off` keeps guests at their full memory |
| `memoryPressureTrigger` | `some 500000 2000000` | `some\|full <stall µs> <window µs>`; the window must be a multiple of 2 s (2000000), at least 2 s, or the kernel refuses it for an unprivileged user |
| `balloonStepPercent` | `25` | percent of class memory per event (1 to 100) |
| `balloonFloorMiB` | `1024` | class memory the balloon never takes |
| `balloonCooldownSecs` | `60` | quiet seconds before each step down |

If the trigger cannot be registered (no PSI, EINVAL) the watcher says so once
in `grow.log`, keeps growing the disk, and the lease runs without a balloon;
`list` and `resolve` report why in the `balloonOff` field and `doctor` fails its
`memory_pressure` check. Every inflate and deflate goes to `grow.log`.

vCPUs and memory never change while a VM runs. A cold boot (`resume` of a
stopped lease) starts at the same size with the disk at its grown size. Every grow and
every failure goes to `<stateDir>/leases/<leaseId>/grow.log`, one timestamped
line each; a failure that repeats is written once, then again with its count
when it clears. The watcher skips paused VMs and exits when its cloud-hypervisor
exits; `release` and `stop` also stop it. `resume` starts a watcher when the
lease has none running, and never a second one.

## Cache volumes

With `capabilities.cacheVolumes: true` in the trusted crabbox user config,
`acquire` reads `desired.cacheVolumes`. Each entry names a `group`, a volume
`name`, a `key`, one or more guest `paths`, an optional `sizeGB` and an
optional `sealed`. With `CRABBOX_CACHE_SCOPE` set, crabbox also sends
`desired.cacheScope` (16 lowercase hex digits); any other value fails the
acquire. A path is `~/<rel>` (the login home `/home/crabbox`, never
`~/.ssh`), `/opt/<rel>`, `/var/cache/<rel>`, or `/var/lib/containerd` or
`/var/lib/containers` and below. Any other path is `skipped`.

- **One image per group.** A group is one sparse raw ext4 image, attached as an
  extra virtio disk and mounted at `/var/lib/crabbox-cache/<group>`. Each volume
  is a directory inside it, and each of its paths is bind-mounted from its own
  subdirectory. The units live on the root disk, so pause, resume and reboots
  keep the mounts. Missing path parents are created for the login user. Every
  cache mount is ordered before `docker.service` and `containerd.service`.
- **Size.** A group's filesystem is the sum of its declared `sizeGB`, at least
  20 GiB, at most `cacheMaxVolumeGB`. The image is sparse, so only used blocks
  take host space.
- **Restore.** A new lease gets a private copy of the group's newest
  generation (a reflink where the filesystem allows it, else a sparse copy of
  the used blocks), grown host-side to the current size. With no generation it
  gets a fresh empty filesystem. Two leases never share an image. Restore
  deletes `.crabbox-sealed` from each path directory of every `sealed` volume.
- **Publish.** `release` presses the ACPI power button and waits up to 30
  seconds for the guest to power off. Then a group publishes as the newest
  generation only if it grew by at least 32 MiB of allocated blocks, its
  journal is clean, every `sealed` volume has `.crabbox-sealed` in each of its
  path directories (else `unsealed: <name>`), `e2fsck -fn` passes and it is
  less than 95% full. Anything
  else is discarded and the previous generation stays. A VM that is stopped,
  paused or killed publishes nothing. A cache never fails a release.
- **Echo.** The acquire response lists each name with its outcome:
  `restored` (detail `gen <unix seconds>`), `empty` or `skipped` (detail says
  why; the path is then a plain directory of the root disk).
- **Store.** `<cacheVolumeDir>/<image sha prefix>-<arch>[-<scope>]/<group>/gen-<unix
  ns>-<lease>.img`, directories mode 0700. Each scope has its own
  generations, so one tenant never restores another's cache; leases without a
  scope share the unscoped ones. Release publishes back into the scope the
  lease was restored under. Working copies sit in
  `<cacheVolumeDir>/leases/<leaseId>/` on the same filesystem. The host runs
  only e2fsprogs on a guest-written image and never mounts it.
- **GC.** `cleanup` removes superseded generations an hour after a newer one
  appeared, groups unused for `cacheKeyTtlDays`, then least recently used
  groups until the store fits `cacheStoreMaxGB`, and working copies of leases
  that no longer exist. `dryRun` only reports.
- **Docker.** Every seed points Docker's `data-root` at `/home/crabbox/docker`
  (merged into an existing `/etc/docker/daemon.json`), so a cache volume on
  `~/docker` carries image layers.

`doctor` checks the four e2fsprogs tools, creates `cacheVolumeDir` mode 0700,
and warns when it cannot reflink, because then each restore copies the cache's
used bytes.

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

- `pause` always suspends to disk: `vm.pause`, then `vm.snapshot` into
  `<stateDir>/leases/<leaseId>/snap`, then it stops cloud-hypervisor and passt,
  so the guest's RAM goes back to the host. The lease status becomes
  `suspended`. The snapshot is written to `snap.tmp` and renamed only once
  complete, so a crash mid-snapshot never leaves one that `resume` would use.
  `pause` fails when the state filesystem has less free space than guest RAM
  plus 1 GiB, or when the snapshot fails; the VM is then resumed and keeps
  running. A lease paused in memory by an older adapter version is suspended to
  disk by `pause` and resumed in place by `resume`.
- `resume` resumes a paused VM in place. A suspended lease is restored: a new
  passt on the same socket, cloud-hypervisor `--restore` from `snap`, then
  `vm.resume`; guest processes continue where they were, but open TCP
  connections drop. The snapshot is deleted once the restored VM is paused and
  before it runs, because the guest's disk moves past it. A failed restore keeps
  the snapshot and returns the error; it never falls back to a cold boot. If the
  VM is dead with no snapshot (host reboot, killed process) it cold-boots from
  the lease's existing disk and seed.

Needs the crabbox fork (branch `moor/external-pause-resume`) and
`capabilities.pauseResume: true` in the trusted crabbox user config.

Lease status is `ready` (running), `paused`, `suspended` (VM processes gone,
memory in the lease's snapshot), `stopped` (VM dead) or
`unresponsive` (VM process alive but its API does not answer, for example
frozen with SIGSTOP). `resume` and non-`releaseOnly` `resolve` on an
`unresponsive` lease fail with
`lease <id>: cloud-hypervisor is alive but not responding` and never cold
boot; `release` kills the process and cleans up, and `cleanup` leaves it alone.
`resolve` on a paused, suspended or stopped lease returns the lease with its status, which
is what crabbox `status`, `extend` and heartbeat need; crabbox itself refuses to
open a session on it. `acquire` of such a lease fails with
`lease <id> is stopped; run crabbox resume`. crabbox does not know the
`unresponsive` status, so `resolve` keeps failing for it instead of returning it.

The lease record keeps each process's start time and the boot id next to its
pid. A pid that now belongs to another process, or that survives a reboot, is
never signalled. `list` and `cleanup` skip a lease that an operation currently
holds, so a create or resume in flight keeps its passt.

## Boot template

With `bootTemplate: true`, a new lease restores a snapshot of an already
provisioned guest instead of cold booting through cloud-init.

- The first lease of a size boots a template guest from the base image with
  the usual seed, but with a bootstrap key made for that template. Once it is
  ready and cloud-init is done, the adapter pauses it, snapshots it into
  `<cacheDir>/template-<key12>/snap`, stops it and keeps its disk beside the
  snapshot. Builds are serialised by `<cacheDir>/template.lock`.
- The template key is a SHA-256 over the image and firmware hashes, the
  rendered template seed, the lease size (vCPUs and memory), `diskGiB` and
  `cloud-hypervisor --version`. Changing any of them builds a new template, and
  the old template of the same size is removed. A template recorded under
  another cloud-hypervisor version is removed and refused.
- Each lease copies the template disk (reflink when the filesystem allows),
  hard-links the snapshot's `memory-ranges` and `state.json`, and writes its own
  `config.json` naming its disk, seed, serial log and sockets. It then restores
  with `--restore`, resumes, and hot-adds its cache volume disks.
- Identity rotation is one ssh call made with the bootstrap key. It steps the
  guest clock to host time and mixes host randomness into the guest RNG. It
  installs the lease's ssh host key, writes a new `/etc/machine-id` and sets the
  hostname. It writes the lease's seed files and runs its seed commands, and
  last it replaces `authorized_keys` with the lease key. The lease also gets its
  normal seed image, so a later cold boot gives it the same identity.
- The adapter then checks that the lease key logs in against the pinned new
  host key, and that the bootstrap key is refused. A lease where the bootstrap
  key still logs in is destroyed and `acquire` fails.
- When no template can be used, or the restore fails, the lease cold boots and
  `acquire` logs `template refused: <why>, cold booting`.

Measured on one host with a 2 vCPU, 4 GiB, 20 GiB lease, `acquire` until ready:

| Path | Seconds |
| --- | --- |
| Cold boot, `bootTemplate` off | 40.8 |
| First lease: template build, then restore | 45.2 |
| Later lease: restore and identity rotation | 3.6 |

The restore takes about 1.7 s and the identity rotation about 1.5 s. Guest RAM
is copied in full at restore, because `memory.shared` rules out restoring it on
demand.

## Checkpoints

A checkpoint is a snapshot of a running lease that new leases can start from.
Three operations manage them, in protocol v1 with the same envelope as
`acquire`. They need no config flag, and they share the snapshot, restore and
identity rotation code of the [boot template](#boot-template).

| Operation | Request adds | Response |
| --- | --- | --- |
| `checkpointCreate` | `leaseId`, `checkpointId`, `name` | `{"checkpointId", "bytes"}` |
| `checkpointFork` | the `acquire` request and `checkpointId` | the `acquire` response |
| `checkpointDelete` | `checkpointId` | `{}` |

- `checkpointId` is chosen by the caller: 1 to 64 characters of `A-Z a-z 0-9 _ -`.
  `name` is a label kept in the checkpoint's `checkpoint.json`.
- `checkpointCreate` needs a running lease. First it runs `sync`, drops the
  guest's caches and compacts its memory over the lease's ssh, then waits up to
  10 s for the guest's free pages to reach the host, so the snapshot holds less.
  If that fails, it is logged and the checkpoint goes ahead. It pauses the VM, writes its
  snapshot, reflinks its disk (a copy when the filesystem cannot), keeps the
  lease's login key and a host key pin, and resumes the VM in place. Guest
  processes, open TCP connections and ssh sessions carry on after a short
  pause, also when the checkpoint fails. Stock cloud-hypervisor v52.0 stops
  the passt rings to save their state and never restarts them, so the guest
  network stays dead. v52.0-moor.1 restarts them on resume. `bytes` is the
  space the checkpoint allocates on disk. A second create under the same id replaces the
  first. A lease with cache volumes attached is refused, because the snapshot
  would name their disks.
- The checkpoint lives in `<cacheDir>/checkpoint-<checkpointId>/`, laid out like
  a template: `snap/`, `disk.raw`, `bootstrap`, `known_hosts` and
  `checkpoint.json`. While it is written the dir is `checkpoint-<id>.tmp`.
  The `bootstrap` file is the source lease's private login key.
- `checkpointFork` starts a new lease from it. It copies the disk, restores the
  snapshot onto the new lease's files, and rotates identity through one ssh call
  made with the source lease's key. The new lease gets its own keys, host key,
  machine id and hostname. The adapter then checks that the source key is
  refused. A fork never cold boots. It fails if the checkpoint is missing
  (`checkpoint not found: <id>`), incomplete, taken by another
  `cloud-hypervisor --version`, or taken at another size than the request's
  class gives.
- `checkpointDelete` removes the dir and any half-written copy. An unknown id
  succeeds. A fork already running is not affected, because it holds its own
  files.
- A checkpoint is not removed by `release` of its source lease or by `cleanup`.
  Delete it with `checkpointDelete`.

## State and cache

- Leases: `<stateDir>/leases/<leaseId>/` (lease.json, disk, seed image, SSH key,
  console log, watch.json, grow.log).
- Sockets: `<runtimeDir>/<12-hex hash of the lease id>/` (`a`, `v`, `p`,
  `p.repair`).
- Downloads, the converted base image, boot templates and checkpoints: `<cacheDir>`.

`release` removes the lease directory. `cloud-hypervisor`, `passt` and the
growth watcher run detached per lease and are stopped on release. Only `release` destroys a
lease.

`cleanup` never deletes paused or suspended leases, nor stopped ones unless
`reapStoppedAfterHours` says so; `release` deletes a lease's snapshot with the
rest of its lease dir. It reaps orphan `passt`
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
runtime directories are writable, that the systemd user manager is reachable
and has the `cpu` and `memory` controllers (with `cgroup: systemd`, with the fix
when it is not), that socket paths fit the 108-byte limit
(`runtimeDir` at most 85 bytes), and whether the firmware and image are cached
and verified. Warnings and failures are also written to stderr, so crabbox
shows them, not just `doctor finished`.

## Limitations

- amd64 only.
- Disk growth assumes an ext4 root on a partition (`growpart` + `resize2fs`), as
  in the pinned Ubuntu image. Disks only grow; they never shrink.
- Leases are not stopped on a timer by the provider. Release them with
  `crabbox stop`.
- Pause writes a snapshot as large as guest RAM to the state filesystem.
- Pause/resume needs the crabbox fork (`moor/external-pause-resume`) with
  `capabilities.pauseResume: true` in trusted user config.
- Upstream crabbox ignores `ssh.hostKey` (see above).
