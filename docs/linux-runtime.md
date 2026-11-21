<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Running a workload on Linux: FluxVM container or microVM

On Linux you choose how an image runs, behind one front door:

```bash
fluxctl sandbox run alpine:3.22 --runtime container --trust dev -- echo hi   # FluxVM's own container runtime
fluxctl sandbox run alpine:3.22 --runtime vm        -- echo hi               # a microVM (see "What is not there yet")
fluxctl sandbox run alpine:3.22 --runtime auto --trust agent -- echo hi      # auto: the isolation follows the trust level
```

`container` is **FluxVM's own** runtime: no Docker, no runc, nothing to install besides FluxVM. It is shaped like systemd services and
systemd-nspawn, not like a Docker clone: every container is a transient systemd unit, and the profile is written in systemd's
hardening vocabulary. `vm` is a microVM with its own kernel. The default `auto` picks by trust and always says what it picked.

## What already existed (read before this change)

| Piece | What it is | Linux host runtime? |
|---|---|---|
| [oci-sandboxes.md](oci-sandboxes.md), `fluxvm-oci-init`, `oci_sandbox.rs` | one microVM per container image, on **vz (macOS) only**; refused elsewhere (`OCI sandboxes run on the vz backend`) | no |
| `docker.sock` subset ([docker-engine.md](docker-engine.md)) | Docker Engine API subset, each container a microVM (macOS); `fluxctl docker` = a real `dockerd` in one managed VM | no: both run in VMs |
| `fluxvm-container-agent` / `-client` / `-protocol` / `-containerd-shim` | in-guest OCI supervisor (pivot_root, namespaces, cgroup v2, seccomp, capabilities) for containerd/Kubernetes Pod VMs. It runs **inside a guest**, over vsock | no: guest side only, not a host runtime |
| `fluxvm-procbox`, `procbox_sandbox.rs` ([procbox.md](procbox.md)) | rootless Landlock + seccomp **process** sandbox on the host kernel: no image, no rootfs, no cgroups | partly: a process tier, not a container |
| QEMU, Cloud Hypervisor, Firecracker, the in-tree KVM engine, the jailer, `fluxvm-cgroup`, `spec_lint` | microVM backends on Linux; per-VM cgroup v2 | yes, for **VM sandboxes** (a Debian image), not for OCI images |
| `fluxvm-image` `Puller`/`BlobStore`, `fluxvm-oci-init` `unpack` | native registry pull and layer unpacking (diff_id check, whiteouts, symlink-safe) | reused here |
| [linux-containers-research.md](linux-containers-research.md) | recommends both tiers behind one front door | this document implements the front door for Linux |

So Linux had microVMs for VM sandboxes and a weak process tier, but **no way to run an OCI image** on the host.

## The decision

- **Both runtimes, one front door** (`runtime` on the sandbox API, `--runtime` in `fluxctl`, `sandbox.container.default_runtime` in config).
- **`container` is FluxVM's own native runtime**: `fluxvm-ctr` (this change). Pull and unpack reuse the existing image code; the launcher
  `fluxvm-container-run` does namespaces, cgroup v2 (through systemd), seccomp and capability drop. It is built on the pieces already
  in the repo (procbox's seccomp compiler, oci-init's unpacker), not on Docker or runc.
- **systemd runs the container**, as in nspawn: lifecycle, restart policy, cgroup delegation and limits, ordering and the lifetime come from
  a transient unit, not from FluxVM re-implementing a supervisor. Without systemd as PID 1 the same runtime runs with the daemon managing
  the cgroup directly.
- **Safe by default**: `auto` gives `dev`/`trusted` workloads a container and `untrusted`/`agent` workloads a microVM; `container` for
  untrusted code is **refused** unless the operator allows it **and** the caller asks for it. `auto` never falls from `vm` back to `container`.
- **Not Docker API compatible, by design.** Docker API clients keep using the `docker.sock` subset or `fluxctl docker` (see the matrix).

## Support matrix

| | `container` (native, this change) | `vm`, one microVM per container | shared-VM engine (`fluxctl docker`) |
|---|---|---|---|
| Linux host | yes (needs the host checks below) | **not yet for OCI images** (vz/macOS only today); VM sandboxes without an image work | yes (a real `dockerd` in one VM) |
| Isolation | shares the host kernel: namespaces, cgroup v2, seccomp, dropped caps. A kernel bug reachable through an allowed syscall is an escape | own kernel behind KVM; an escape needs a hypervisor bug | containers share the engine VM's kernel; the VM is the boundary to the host |
| Start time | tens of ms to start the process once the image is unpacked (first use pays pull + unpack) | about a second on vz; seconds for a cold Linux VM boot (snapshot restore is the fast path, see below) | one VM start (minutes the first time, it installs Docker), then container-fast |
| Docker API | no | the `docker.sock` subset (macOS) | full |
| Rootless | yes, with an unprivileged user namespace (uid 0 inside = your uid; see AppArmor note) | needs `/dev/kvm` access, no root | engine VM needs `/dev/kvm` |
| Kernel features | cgroup v2, seccomp, mount/pid/ipc/uts/net namespaces; user namespaces for rootless; systemd >= 240 for `append:` logs | `/dev/kvm` (KVM), a backend (QEMU/Cloud Hypervisor/Firecracker) | `/dev/kvm` |
| Limits | memory, cpu, pids, io weight (cgroup v2 via systemd or direct) | vcpus, memory | engine VM |
| Network | none (loopback) by default, or the host namespace for trusted code. **No published ports, no egress policy yet** | NIC with the egress policy (vz) | docker networks |
| exec / files / logs / TTL / delete | yes (same sandbox API) | yes | docker CLI |

### Which one should I pick

- Your own build and test loops, CI steps, dev tools: `container` (fast, rootless, systemd-managed).
- Code you did not write, anything an agent chooses to run, multi-tenant: `vm`.
- You need `docker build`, Compose, networks, the Docker API: `fluxctl docker`.

## One front door

```bash
fluxctl sandbox run IMAGE [--runtime auto|container|vm] [--trust dev|trusted|untrusted|agent]
                    [--allow-container-untrusted] [--memory-mib N] [--cpu-percent N] [--pids N] [--io-weight N]
                    [--net none|host] [--writable-root] [--restart on-failure] [--print-unit] [-- CMD...]
fluxctl sandbox inspect ID [--unit]     # the runtime decision, the container as a systemd unit, its state
fluxctl sandbox runtimes                # what this host supports, with fix hints (also in `fluxctl doctor`)
fluxctl sandbox logs ID                 # console tail and the exit code
```

The same fields exist on `POST /v1/sandboxes` (`runtime`, `trust`, `allow_container_untrusted`, `container`), on the MCP
`sandbox_create` tool, and as defaults in config:

```toml
[sandbox.container]
default_runtime = "auto"       # auto | container | vm
default_trust = "untrusted"    # a request that does not say gets the safe default
allow_untrusted = false        # lets callers pass allow_container_untrusted
```

`auto` explains itself in every output (stderr of `sandbox run`, the labels `fluxvm.runtime` and `fluxvm.runtime.reason` on the record):

```
runtime: container (auto: trust=dev gets a native container (fast start, shares the host kernel))
container: the workload shares the host kernel. Namespaces, cgroup v2, seccomp and dropped capabilities confine it, ...
```

Refusal for untrusted code (HTTP 403 on the API):

```
refusing runtime=container for untrusted code. container: the workload shares the host kernel... vm: the workload runs in its
own kernel behind hardware virtualization... Use --runtime vm, or have the operator set sandbox.container.allow_untrusted = true
and then pass --allow-container-untrusted
```

### `--print-unit`: the container is transparent

`fluxctl sandbox run alpine --runtime container --trust dev --memory-mib 256 --print-unit` runs nothing and prints the unit the daemon would
start, plus the equivalent `systemd-run` command. `fluxctl sandbox inspect ID --unit` prints the real one. Example:

```ini
[Unit]
Description=FluxVM container fx-1a2b3c4d (alpine:3.22)

[Service]
Type=exec
Delegate=yes
MemoryMax=256M
MemorySwapMax=0
TasksMax=512
Restart=no
KillMode=control-group
NoNewPrivileges=yes
StandardOutput=append:/var/lib/fluxvm/instances/<id>/console.log
ExecStart=/usr/bin/fluxvm-container-run run --spec /var/lib/fluxvm/instances/<id>/spec.json

# Enforced by fluxvm-container-run inside the container, in systemd's own vocabulary.
[X-FluxVM-Container]
RootDirectory=...   ReadOnlyPaths=/   PrivateTmp=yes   PrivateDevices=yes   PrivateNetwork=yes   PrivateUsers=yes
CapabilityBoundingSet=CAP_CHOWN CAP_DAC_OVERRIDE ...   SystemCallFilter=~ptrace mount umount2 pivot_root ... unshare setns
```

The `[Service]` section is real systemd configuration (cgroup delegation and limits, restart, lifetime, logs, `NoNewPrivileges`).
The `[X-FluxVM-Container]` section uses the same directive names for what the launcher enforces inside the container; systemd ignores
`X-` sections, so the file loads as is, and a hardening directive can be lifted from it into your own units. (`RootDirectory=` and
`PrivateUsers=` cannot be given to systemd directly: it would need its own pivot into a rootfs it did not prepare, and the user manager
refuses it, see "Verified".)

## How the native container runs

1. `oci_images::pull` fetches the image (existing registry code, per-digest blob store). `ensure_cache` unpacks the layers once per
   manifest digest into `ctr/rootfs/<digest>-<user|root>` (diff_ids verified, whiteouts honoured, no path escapes), with the mountpoints
   a read-only root needs. Read-only root (the default) shares that directory between containers; `--writable-root` takes a private copy
   (`cp --reflink=auto`: a reflink on btrfs/XFS, a plain copy elsewhere).
2. The spec (`spec.json`) is derived by one pure function (`build_spec`): image process rules (Entrypoint/Cmd, env, workdir, user) as in
   the vz path, limits against the operator caps, refusals for what is not implemented.
3. systemd starts `fluxvm-container-run run` as a transient unit (`systemd-run`, `--user` when the daemon is not root). The launcher
   unshares mount, pid, ipc, uts and net (and user when unprivileged), bind-mounts the rootfs (read-only on request), mounts `/proc`,
   a minimal `/dev` (`null zero full random urandom tty`), `/tmp`, `/run`, `pivot_root`s, drops capabilities from the bounding set,
   sets `no_new_privs`, installs the seccomp filter (the procbox denylist plus `unshare`/`setns`; shown in the unit as
   `SystemCallFilter=`), and `exec`s the entrypoint as PID 1.
4. `exec` joins the container's cgroup and namespaces (`setns`) and runs the command with the same restrictions; `fs read/write` join
   the mount namespace and do the file I/O there, so symlinks resolve inside the container. `logs` read the console file and the exit
   code. `delete` stops the unit, removes the cgroup and the workspace. TTL becomes `RuntimeMaxSec=` and the sandbox TTL reaper.
5. Limits are never silently dropped: a limit that cannot be enforced fails the start.

### Cgroup delegation and the "failed to delegate cgroup controllers" warning

The daemon used to warn when it was not given a delegated subtree. `packaging/systemd/fluxvm.service` now sets `Delegate=yes`, the
warning explains the fix, and `fluxctl doctor` reports which controllers are available. Containers do not depend on the daemon's
delegation: each is its own unit with `Delegate=yes`.

### Without systemd as PID 1 (Alpine/OpenRC, containers, WSL)

The same runtime, with `manager = "direct"` (chosen automatically): the daemon starts the launcher itself (new process group, console
file as stdout), the launcher creates `<daemon cgroup>/fx-XXXX`, writes the cgroup v2 files (`memory.max`, `memory.swap.max`,
`memory.oom.group`, `cpu.max`, `pids.max`, `io.weight`), and moves itself in. Restart policy and `RuntimeMaxSec` come from the daemon's
TTL instead of systemd. `fluxctl doctor` reports this as `manager: no systemd manager available`.

### Rootless, and the AppArmor catch on Ubuntu

Rootless containers map your uid to root inside a user namespace (`PrivateUsers=yes`); other uids need a root daemon (or `newuidmap`,
not implemented). **Ubuntu 23.10 and later restrict unprivileged user namespaces with AppArmor**
(`kernel.apparmor_restrict_unprivileged_userns=1`): a program without an AppArmor profile that says `userns,` gets a namespace without
capabilities, and the launcher cannot mount or pivot. Fix, either:

- install `deploy/apparmor/fluxvm-container-run` (`sudo cp` into `/etc/apparmor.d/`, `apparmor_parser -r`), the way Ubuntu does for runc,
  crun and podman; or
- run the daemon as root (rootful containers need no user namespace); or
- `sysctl kernel.apparmor_restrict_unprivileged_userns=0` (a system-wide relaxation, not recommended).

`fluxctl doctor` and `fluxctl sandbox runtimes` detect this and print the fix. The launcher itself refuses with the same hint instead of
failing obscurely.

## What is not there yet (refused, never faked)

- **`vm` for an OCI image on Linux.** The one-VM-per-container flow needs the vz kernel/initramfs and a virtiofs meta share. Extending it
  to QEMU/Cloud Hypervisor/Firecracker needs an OCI boot image (kernel, initramfs with `fluxvm-oci-init`), a block device built from the
  image (the same unpack, into ext4) and the vsock agent. The request is refused with that reason; `runtime: vm` without an image creates
  the usual VM sandbox. Next step: build the ext4 rootfs from the shared unpack cache and boot it with the existing Linux backends.
- **Published ports, private networks, the egress allow-list, volumes, health checks, secrets** for containers: refused with a reason.
  `--offline` (the default) is a network namespace with only loopback. Networking plan: veth + bridge only when the daemon is root and
  configured; rootless user-mode networking (slirp4netns style; `fluxvm-usernet` is the likely base) otherwise; the egress policy
  then applies at the bridge.
- Non-root image users under a rootless daemon; user *names* are resolved from the image's `/etc/passwd` for a root daemon.
- Stacks (`after`/`requires` exist on the spec and in the unit; the stack file format does not drive them yet).
- A status update when a container exits (the record stays `running`; `sandbox logs` reports the exit code and `stopped`).
- Overlay rootfs (copy or shared read-only today; overlayfs is available on the test host).

## Next tier: faster starts (design only, not in the first slice)

The rootfs layer is a function `image -> directory` (`ensure_cache`) and the runtime only needs a rootfs path, so these slot in without
changing the runtime API (`spec.rootfs` stays a directory or, later, a mount the launcher is given).

- **Lazy pulling.** A FAST '26 paper (https://www.usenix.org/system/files/fast26-wang-li.pdf) is reported to find that image pull is about 76% of
  container start time while only about 6.4% of that data is read. *Not verified here: I could fetch the PDF but not read its text.*
  Candidates: **Nydus** (on-demand chunk download; can serve through in-kernel EROFS), **SOCI** (keeps the original OCI image and adds a
  separate index artifact in the registry, so existing images work).
- **EROFS layers.** containerd 2.3.0 (30 April 2026) release notes list "Add EROFS layer media type", "Support zstd-wrapped EROFS layers",
  "Add os.features support for EROFS native container images" and "Add dmverity support to the erofs snapshotter" (verified against the
  release page; the claim that layers are received straight from a registry is implied by the media type, not stated, and I did not test
  it). No composefs-with-containerd-2.x source was found.
- **Pre-baked, snapshot-restored microVMs** are the fast path for `vm`: FluxVM already has snapshots and vm-templates; restore replaces
  cold boot (about two seconds against about eight on a Mac with the warm pool).
- **Needs real hardware or kernels to verify.** The test host's kernel is 7.0.0-43 (Ubuntu): `CONFIG_EROFS_FS=m` with zstd, lzma, xattr and
  file-backed mounts, `CONFIG_FS_VERITY=y`, overlayfs built as a module and loaded. The erofs module exists but is **not loaded**, and loading it
  or mounting needs root, so nothing EROFS was exercised. EROFS on-demand (fscache) support, dm-verity, and the lazy-pull candidates are
  untested here.

## Verification status

See the end of the change description in the CHANGELOG entry and the list below, which separates what ran live from what is only
unit-tested.

VERIFIED_PLACEHOLDER
