# crabbox-cloudhypervisor

A [crabbox](https://github.com/openclaw/crabbox) external provider that leases
local, rootless [cloud-hypervisor](https://www.cloudhypervisor.org/) microVMs.
Each lease is a fresh Ubuntu 24.04 guest reachable over SSH, with outbound
network access. It speaks crabbox external provider protocol v1 (JSON on
stdin/stdout) and supports `doctor`, `acquire`, `resolve`, `list`, `release`,
`touch` and `cleanup`.

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
| `cpus` | `2` | vCPUs (1-64) |
| `memoryMiB` | `4096` | guest memory, at least 512 |
| `diskGiB` | `20` | guest disk size, at least 8 |
| `readyTimeoutSeconds` | `300` | wait for the guest to become ready (1-3600) |
| `stateDir` | `$XDG_STATE_HOME/crabbox-cloudhypervisor` | per-lease state (absolute path) |
| `runtimeDir` | `$XDG_RUNTIME_DIR/crabbox-cloudhypervisor`, else `<stateDir>/run` | unix sockets (absolute path) |
| `cacheDir` | `$XDG_CACHE_HOME/crabbox-cloudhypervisor` | downloaded assets (absolute path) |
| `image` | pinned Ubuntu image | `{url, sha256}` override |
| `firmware` | pinned CLOUDHV.fd | `{url, sha256}` override |
| `cloudHypervisor`, `passt`, `qemuImg`, `ssh`, `sshKeygen`, `curl` | looked up on `PATH` | binary paths |

Without the `XDG_*` variables, state and cache fall back to
`~/.local/state` and `~/.cache`.

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

## State and cache

- Leases: `<stateDir>/leases/<leaseId>/` (lease.json, disk, seed image, SSH key,
  console log).
- Sockets: `<runtimeDir>`.
- Downloads and the converted base image: `<cacheDir>`.

`release` removes the lease directory. `cloud-hypervisor` and `passt` processes
run detached per lease and are stopped on release. `cleanup` removes dead or
orphaned lease directories.

## Doctor

`crabbox doctor` calls the provider's `doctor` op. It checks `/dev/kvm`,
`/dev/vhost-vsock`, the host tools and their versions, that the state and
runtime directories are writable, that socket paths fit the 108-byte limit, and
whether the firmware and image are cached and verified.

## Limitations

- No pause/resume and no `hostKey` yet.
- amd64 only.
- The guest host key is not pinned. SSH runs with `StrictHostKeyChecking=no`,
  which is acceptable only because the connection is a local vsock proxy.
- Leases are not stopped on a timer by the provider. Release them with
  `crabbox stop`.
