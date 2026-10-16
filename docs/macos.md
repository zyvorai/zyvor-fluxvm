# FluxVM on macOS (Apple silicon)

FluxVM's control plane (daemon, REST API, `fluxctl`, scheduler) builds and runs natively on macOS, with a new
**`vz` backend** that runs VMs on Apple's Virtualization.framework. It is a separate, smaller feature set than the
Linux backends: no KVM, TAP, eBPF, cgroups or network namespaces.

For how the pieces fit together (runner per VM, control socket, vsock, warm pool, networking), see [macos-architecture.md](macos-architecture.md).
To run several Mac minis and Mac Studios as one VM API, see [mac-cloud.md](mac-cloud.md).

## What is verified

On an Apple M4 running macOS 27.2 (Xcode 27, Rust 1.98):

| Check | Result |
| --- | --- |
| `cargo build -p fluxctl`, `cargo build -p fluxvm-apple` | Builds; `fluxctl --version` runs |
| `cargo test -p fluxvm-core --lib` / `-p fluxvm-scheduler --lib` / `-p fluxvm-api --lib` | core 79 and scheduler 204 passed on the M4 (api 56 when the backend landed); CI runs them on every change |
| `cargo test -p fluxvm-network --lib`, `-p fluxvm-storage --lib`, `-p fluxvm-guest-protocol --lib` | all passed (one Linux-only test is gated) |
| `cargo test -p fluxvm-apple` / `-p fluxvm-agent --lib` | fluxvm-apple 49 unit tests (capability matrix and egress validation, control protocol, SSH helpers, snapshot files, host-capabilities decode) plus 1 `backend` test against a fake runner and 6 source-contract tests (they only grep Swift and C source, they run nothing); fluxvm-agent 55; all pass, and `cargo clippy -p fluxvm-apple -p fluxvm-agent -- -D warnings` is clean |
| `fluxvm-vz-runner host-capabilities` | Printed the M4's real capabilities (macOS 27.2, 10 CPUs, 16 GiB, nested virtualization, vmnet, custom Virtio, USB API, bridged `en5` and `en0`); the Rust decode and the node heartbeat used it |
| Fleet placement on loopback | `fluxvm-agent central` with one real `node` (heartbeat carried its `apple` caps) and two fake nodes: `vz` requests (plain, `custom_virtio`, `nested_virtualization`) landed on the real node, the fake nodes without those features or without Apple caps were excluded; a named shared vmnet request went to the only vmnet-capable node |
| Custom Virtio guest bus | Debian 13 guest (kernel 6.12), `apple.custom_virtio: true`, device id `0x3F`: the driver binds, `/dev/fluxvm` appears, `fluxvm_virtioctl ping`, `echo`, `stats` and `capabilities` work, and all four bulk operations (fill, zero, copy, crc32; up to 1 MiB) pass. See "Mac Studio options" for what is not exercised |
| `scripts/macos-live-test.sh` | **PASS**, end to end on a real Debian 13 guest: create through the API, address, SSH, TCP forwards (host and guest-to-guest), shared folders, pause/resume, stop/start, snapshot and restore, named images, `fluxctl run` (cold and warm), a two-service stack, sandboxes (exec, files, TTL, warm pool and its refresh after an image update, concurrent creates, offline, allow-listed egress, speculate and changesets), nothing left running |
| `scripts/oci-live-test.sh` | **PASS** with the `oci-boot` CI artifacts: container exit codes, uid, read-only root, exec, distroless argv exec, offline, allow-list, a private network between two sandboxes, `linux/amd64` under Rosetta, a warm-pool claim, TTL |
| `scripts/vz-devices-live-test.sh` | **PASS** on a Debian 13 guest: extra disks on virtio, NVMe and USB, a host block device, an NBD export, USB hot-attach and detach, a console port both ways, a 1600x900 Linux display |
| `scripts/macos-guest-live-test.sh` | Clones a prepared macOS guest template through the API, checks the reported address and SSH, deletes it; needs your own template (`FLUXVM_MACOS_TEMPLATE`), so CI does not run it |

**Verified on hardware (macOS 27.0.1 guest on an M4 host):** `POST /v1/vms` with `image` set to the cached IPSW, `apple.install` and `provision_*` end to end (installed, provisioned and reachable over SSH as the provisioned user with no one at the console), exec through `/v1/vms/{id}/agent`, a memory snapshot (3-9 s), a stop/start of the provisioned VM, and a memory restore of a running VM (a file written after the snapshot was gone again after `POST /v1/vms/{id}/restore`). A memory restore needs an unlocked login session on the host: with the screen locked it fails with "An error occurred while restoring the virtual machine". **Not verified:** multi-Mac clusters (placement was only checked on loopback with one real Mac and fake nodes), the shared vmnet broker, physical USB passthrough, Linux-only crates (`fluxvm-procbox`,
`fluxvm-container-*`, `fluxvm-microvm`, `fluxvm-kube`, the eBPF agent), and any Intel Mac. The hosted CI jobs for the `vz` pull requests
have not all been green; see the pull requests for their state.

## Quick start

**Install with Homebrew** (Apple silicon, macOS 14 or later):

```bash
brew install zyvorai/tap/fluxvm      # step by step, with the MCP client setup: install-macos.md
fluxctl run                      # a throwaway Debian VM and a shell
brew services start fluxvm       # the daemon in the background, API on 127.0.0.1:7788
```

The package is built from each version tag (see [`packaging/homebrew`](https://github.com/zyvorai/zyvor-fluxvm/blob/main/packaging/homebrew/README.md)) and contains `fluxctl`, the
signed `fluxvm-vz-runner`, `fluxvm-vz-switch`, `fluxvm-blockd` and `fluxvm-usernet` side by side, which is where the daemon looks for
them. Or build from source:

```bash
xcode-select --install          # compiler for the Swift runner
brew install hivex              # linked by guestkit's registry support
cargo build -p fluxctl          # also builds and ad-hoc signs target/*/build/fluxvm-apple-*/out/fluxvm-vz-runner
./scripts/macos-live-test.sh    # boots a real Debian VM through the API (downloads the built-in debian-13, about 300 MB)
```

Or skip the API: `fluxctl run` gives you a throwaway VM and a shell, and everything you did in it is gone when you leave.

```bash
fluxctl run                                  # built-in debian-13, a shell as your own user name
fluxctl run -v ~/src:/mnt/src -p 8080:80     # share a folder, forward 127.0.0.1:8080 to guest port 80
fluxctl run ubuntu-24.04 -- 'uname -a'       # run one command; its exit code is yours
fluxctl run --keep --name dev                # keep the VM; `fluxctl ssh dev` (as your user) or `fluxctl delete dev` later
fluxctl run --no-warm                        # always cold-boot instead of restoring the warm snapshot
```

It uses your first `~/.ssh/id_*.pub` key (or creates `~/.ssh/fluxvm_ed25519`).

**Warm starts.** The first run for a given setup (image, size, ports, volumes, user, key) cold-boots a VM, waits for cloud-init to
finish, snapshots it as a stopped template named `warm-<hash>`, and runs your session on it. Every later run restores that snapshot
(about 2 s on an M4 against about 10 s for a cold boot) and stops the VM afterwards, so each run starts pristine. If the template is
in use by another run, or the restore fails (for example on a locked screen, see Snapshots), `fluxctl run` boots a separate VM
instead. Remove a template with `fluxctl delete warm-<hash>`, for example after a new image version.

Measured on an Apple M4: cold boot about 10 s, warm run 2 to 3 s, snapshot 0.7 s, restore to SSH 1.4 s, sandbox create 8 s cold and
about 2 s from the warm pool.

A project that needs several VMs can describe them in a `fluxvm.toml` and use `fluxctl up` / `down`; see [stacks](macos-stacks.md).

An AI agent can get a disposable VM through the sandbox API or MCP; see [sandboxes](macos-sandboxes.md). A container image can be
the sandbox too, one lightweight VM per container: `fluxctl sandbox run alpine:3.22 --rm -- echo hi`; see
[container sandboxes](oci-sandboxes.md).

Run the daemon yourself:

```bash
fluxctl serve                   # state in ~/Library/Application Support/FluxVM, API on 127.0.0.1:7788
curl -X POST localhost:7788/v1/vms -H 'Content-Type: application/json' -d '{
  "name": "demo", "backend": "vz", "image": "/path/to/arm64-debian.raw",
  "vcpus": 2, "memory_mib": 2048, "network": {"mode": "user"},
  "cloud_init": {"hostname": "demo", "user": "velora", "ssh_authorized_keys": ["ssh-ed25519 AAAA…"]}
}'
curl localhost:7788/v1/vms/<id>        # status, and guest_ip once the guest reports it
```

On a Mac, `"backend": "auto"` resolves to `vz`.

## How it works

The daemon never links Virtualization.framework. `fluxvm-apple` supervises one signed helper process per VM,
`fluxvm-vz-runner` (`crates/fluxvm-apple/runner/Runner.swift`), over a unix control socket (one JSON line per request:
`status`, `pause`, `resume`, `shutdown`, `stop`, `save`). The runner holds the `VZVirtualMachine`, writes the guest's serial console
to the VM log, and records the guest's NAT address.

- **Disk:** raw images are cloned instantly (APFS `cp -c`). A qcow2 image is converted to raw while cloning, which needs
  `qemu-img` (`brew install qemu`).
- **Named images:** `"image": "debian-13"` (also `debian-12`, `ubuntu-24.04`, `ubuntu-26.04`, `fedora-44`, `centos-stream-10`, `almalinux-10`, `rocky-10`, `kali`) downloads the ARM64 cloud image over HTTPS, checks it against
  the vendor's published SHA file, and caches it under `<state_dir>/images`. qcow2 downloads are converted to raw once by
  FluxVM itself (no qemu-img needed), and every image is kept sparse. `kali` follows Kali's current rolling build. Vendor root
  disks are small (Ubuntu's is about 3.5 GiB), so set `disk_size_gib` for real work. A file of the same name, or a catalog entry, wins. Named images are not catalog images, so `policy.require_catalog_names` rejects them.
- **Cloud-init:** a NoCloud ISO built with `hdiutil`, with a MAC-based DHCP identity so the address stays stable.
- **Offline injection (Linux guests, private raw clone, never a running VM):** the guest agent token is written straight into the
  root disk's ext4 filesystem by `fluxvm-diskinject` (no seed ISO needed for it), and the request's SSH public keys
  (`cloud_init.ssh_authorized_keys`) are appended to `/root/.ssh/authorized_keys` the same way. Both are skipped, and the
  cloud-init copy stays the only one, for OCI sandboxes, macOS guests, ASIF overlays, a read-only root, non-default storage and
  non-raw disks. A failed injection also keeps the cloud-init copy. Keys are injected only when `cloud_init.user` is explicitly `root` (then the cloud-init copy is dropped once every key is on the
  disk). With the user unset or any other account the keys belong to that account, so nothing is written to root's file: the offline
  writer can only touch root's `authorized_keys`, and writing them there would grant a root login that cloud-init alone does not. **Verified on a booted Debian 13 guest on an M4** (`scripts/vz-usernet-live-test.sh`: root login with the key and no cloud-init copy); the decision logic is also unit-tested, and an
  ext4 round trip in `fluxvm-diskinject`; booted once on Debian 13 with a clean ext4 (see the hardware log).
- **Guest address:** macOS offers no usable DHCP-lease or ARP view to a spawned process, so FluxVM adds a small systemd
  service to the cloud-init (when a `cloud_init` is given) that prints `VELORA-IP <addr>` on the serial console; the runner
  parses it and `GET /v1/vms/{id}` reports it as `guest_ip`. It also turns off OpenSSH's per-source penalties inside the
  guest so the managing host is never throttled.
- **Networking:** Virtualization.framework NAT only (`network.mode = "user"`), or `"none"`, which attaches no network card at all (see [sandboxes](macos-sandboxes.md)). The guest is reachable at its address
  from the Mac. TCP `forwards` listen on `127.0.0.1:<host_port>` (ports 1024 and up) and relay to the guest's address. Guests on this NAT cannot reach
  each other; a forward with `"guests": true` also listens on the NAT gateway address (the Mac, as guests see it) so other guests can use it
  (that is how [stacks](macos-stacks.md) connect services).
- **Shared folders:** `shared_folders` become virtiofs shares tagged `fs0`, `fs1`, …; with `cloud_init` (implied if you give shares)
  the guest mounts them at `guest_path` via `/etc/fstab`, so they survive stop/start. `read_only` is enforced by the host.
- **vsock:** every VM has a vsock proxy socket (`CONNECT <port>` over a unix socket). The daemon uses it to reach an offline sandbox's sshd
  (guest port 22), and the guest reaches a host-side egress proxy over it (guest to host, port 3128). See [sandboxes](macos-sandboxes.md).
- **Direct kernel boot:** a Linux guest with `kernel` (and optionally `initrd`, `kernel_args`) boots through `VZLinuxBootLoader`
  instead of EFI from the disk; both paths keep the generic platform, so snapshots work. `apple.root_read_only` attaches the root
  disk read-only, and `apple.tagged_shares` adds virtiofs shares with fixed tags (other than `fsN` and `rosetta`). Container sandboxes
  use all three ([oci-sandboxes.md](oci-sandboxes.md)).
- **Signing:** the runner is ad-hoc signed with `com.apple.security.virtualization` by `build.rs`.
  Set `FLUXVM_VZ_RUNNER` to use another binary; `FLUXVM_SKIP_VZ_RUNNER=1` skips building it. The runner needs the macOS 27 SDK
  (Xcode 27). If Swift is installed but the runner does not compile or sign, the build fails and shows the swiftc errors; with no
  Swift toolchain it only warns.

## Snapshots

`POST /v1/vms/{id}/snapshot {"tag": "s1"}` saves a running (or paused) guest: the runner pauses it, writes its memory and device
state with Virtualization.framework (`saveMachineStateTo`), FluxVM clones the disk and EFI variables beside it with APFS
`cp -c` while it is still paused, and the guest continues. Everything lives in `<vm workspace>/snapshots/<tag>/`.
`GET /v1/vms/{id}/snapshots` lists them and `DELETE /v1/vms/{id}/snapshots/{tag}` removes one.

`POST /v1/vms/{id}/restore {"tag": "s1"}` on a **stopped** VM puts the cloned disk back and resumes the saved state: processes that
were running keep running, nothing reboots, and the guest address is reported again. A running VM must be stopped first.

- Needs macOS 14 or later, Apple silicon, and Linux guests.
- **Restoring needs an unlocked login session.** Virtualization.framework encrypts the saved state with a Secure Enclave key that is
  unusable while the Mac's screen is locked: restore then fails with "permission denied". Saving still works. Keep a headless Mac
  mini logged in (auto-login) if you restore on it.
- A saved state is only valid for the configuration it was saved with, and each VM now keeps one machine identifier
  (`generic-id.bin`) so that holds across launches.
- Whether a restore survives a FluxVM runner upgrade has not been tested.

### macOS warm pool (`fluxctl macos-pool`)

A macOS guest cannot be forked or branched (its saved state is bound to one machine identifier and auxiliary storage),
and Apple allows two to run at once. The pool is therefore at most two *slots*: stopped macOS VMs, each with a `warm`
snapshot taken while it was logged in. Slots are reused, not consumed.

1. Set a guest up by hand (Setup Assistant, auto-login), leave it running and `fluxctl macos-pool enroll <id>`. This saves the
   `warm` snapshot, stops the VM and marks it idle. A third enrolment is refused.
2. `fluxctl macos-pool checkout` restores a free slot: the snapshot's disk and auxiliary storage go back first, then memory.
   409 when every slot is checked out; 503 when the restore fails and the Mac's login session is why (locked screen, nobody
   logged in): the message says so and the slot stays idle. Any other failed restore is an error; there is no cold-boot fallback.
3. `fluxctl macos-pool return <id>` stops the slot. Whatever the guest wrote is discarded by the next checkout.

Restore needs an unlocked login session on the Mac (see Snapshots above). `fluxctl macos-pool status` (REST
`GET /v1/macos-pool/status`) says whether a checkout can restore right now (`login_session`: `unlocked`, `locked`,
`no_session` or `unknown`, with `hint` when it will fail) and how many slots are free. Not verified on hardware; the enroll, checkout
and return flow has only been unit-tested for slot selection.

### Branches (`fluxctl branch`)

`POST /v1/vms/{id}/branches {"name": "checkpoint-a"}` saves the running guest's state and disk as a durable, named
checkpoint outside the VM's workspace; `POST /v1/branches/{id}/start {"count": 4}` starts new VMs from it, with everything
fork does (strict identity, rotated token, parallel restore, hooks, readiness). It survives the source VM's deletion and
a daemon restart, can be started repeatedly, and a VM started from a branch can be branched again (`parent_branch`).
Details in [fork-and-restore.md](fork-and-restore.md#branches).

### Fork (`POST /v1/vms/{id}/fork`)

`{"count": 2, "name_prefix": "job"}` turns one running Linux guest into `count` running children (up to 32), named
`job-1`, `job-2`. One runner `save` pauses the parent for a moment and clones its disk; each child gets APFS clones of that
state and disk, the parent's machine identifier and MAC (the saved device state is bound to them), is restored through the same
path as `restore`, and carries the label `fluxvm.dev/forked-from`. The parent keeps running. It is all-or-nothing: if one child
fails, the others are deleted.

- **Each child needs a network of its own**, because it resumes with the parent's MAC and address: `network.mode = none`, or
  `mode = user` with `apple.usernet` (a private userspace NAT per VM) and no port forwards. The Mac's NAT, vmnet, bridged and
  private-network guests are refused; so are `apple.usernet.port_forwards` (every child would bind the same host port), extra
  disks, and macOS guests.
- A child's MAC is marked shared and is never returned to the MAC pool when the child is deleted.
- **Identity** is reset per child by the guest agent and is *strict by default* (`identity: unique`): the fork fails, and no
  child is left, unless the guest confirmed hostname, machine-id, entropy, SSH host keys, cloud-init, DHCP and network state;
  the agent token is rotated too. A guest without the agent is reset over SSH when sshd is reachable (a usernet guest with no
  forwards is not), else refused; pass `identity: best-effort` or `preserve` to fork it anyway. Details in
  [fork-and-restore.md](fork-and-restore.md#child-identity-reset).
- **Children are restored in parallel**, up to `parallelism` at a time (default `min(count, 8)`). The batch is admitted once, before
  the parent is paused. The response carries per-child `prepare_ms`, `restore_ms` and identity timings and `parent_snapshot_ms`.
- A usernet child's address is the gateway's lease (10.0.2.15) and is reported at once.
- A parent on the Mac's NAT cannot be forked: restoring its saved state onto a usernet NIC was tried on hardware and the child
  runs but its NIC comes up disconnected ("Internal Network Error"). Create the guest with `apple.usernet` to make it forkable.
- Needs the same unlocked login session as `restore`.

## macOS guests

Verified by hand on an Apple M4 running macOS 27.2 with macOS 27.0.1 (26A434) as the guest. What works:

- **Install.** `fluxvm-vz-runner install --config <json>` (with `guest_os: "macos"`, `media` = the IPSW, `disk` = a sparse raw file of at
  least 40 GB, 4 CPUs, 8 GiB) installs from an IPSW and writes `hardware.bin`, `identity.bin` and `auxiliary.bin` beside the disk. The
  installer reached 100% and the guest then booted. The IPSW is 26.6 GB; Apple's public catalog
  (`https://mesu.apple.com/assets/macos/com_apple_macOSIPSW/com_apple_macOSIPSW.xml`) lists it, since
  `VZMacOSRestoreImage.fetchLatestSupported` failed to load its catalog on that machine. The install is not exposed through the API yet.
- **Prepare once, by hand.** The guest stops at Setup Assistant. Boot it with `window: true` from a terminal (the window needs a
  foreground app), create a user, turn on **Remote Login** (System Settings, General, Sharing; if the toggle does not stick, give
  Terminal Full Disk Access and run `sudo systemsetup -setremotelogin on`), install your SSH key, and shut down. That disk is the template.
  Doing this offline from the host needs root and was not attempted.
- **Clone.** `POST /v1/vms` with `backend: "vz"`, `apple: {guest_os: "macos"}` and `image` = the template's `disk.raw` clones the disk
  (APFS `cp -c`, instant) and takes `hardware.bin` and `auxiliary.bin` from beside it. The runner gives every clone its own machine
  identifier, so a clone is a separate machine (it has its own serial number). The clone's state directory must be on the same APFS volume as
  the template, or the "clone" is a full copy of a 20+ GB disk.
- **Address.** A macOS guest has no systemd, so the runner reads the Mac's DHCP leases (`/var/db/dhcpd_leases`) for the MAC the guest's
  card was given and reports it as `guest_ip`. (The ARP table is not an option: macOS shows it empty to a spawned process.)
- **Key login on a fresh clone: turn FileVault off in the template.** With FileVault on, a clone boots with its volume locked and sshd
  refuses every key ("Permission denied (publickey,...)") until one password login unlocks it ("System successfully unlocked. You may
  now use SSH to authenticate normally."). Moving the key outside the home directory (`AuthorizedKeysFile` in `/etc/ssh/`) does not help,
  because the whole Data volume is locked. Fix it once, in the template: boot it, log in, and run
  `sudo fdesetup disable` (give it the user's name and password), then wait for decryption to finish (`fdesetup status` says "FileVault is
  Off"; about 25 minutes for a 50 GB disk on an Apple M4), shut the guest down cleanly and use that disk as the template. Verified on an
  Apple M4: a fresh clone of such a template accepted the key on its first login, no password needed. If encryption is still running
  when you try, `fdesetup disable` fails with error -69573; wait for it to reach 100% first. The guest's disk is a file on your Mac, so
  turning FileVault off there only matters if the template file itself needs to stay secret.
- **First boot of a clone** is slow on a USB drive (about 5 minutes to SSH at 37 MB/s) and restarts sshd once, so retry SSH for a minute.

### As an agent session

`POST /v1/agents/sessions` with `runtime.template` names a template directory under `sandbox.templates_dir`
(`<name>/spec.json` plus `disk.raw`, `hardware.bin`, `identity.bin`, `auxiliary.bin`). `scripts/macos-session-template.sh` builds
one from the IPSW (install, provision, authorize the daemon's key, disable sleep and the screen lock, stop), and
`scripts/vz-macos-session-live-test.sh` exercises it end to end. What a macOS session can and cannot do is the table in
[agent-sessions-guide.md](agent-sessions-guide.md#22-a-macos-guest-as-a-session). **Verified on an M4 host with a macOS 27.0.1 guest:** the template build, an offline policy refused,
session create (`guest_os: macos`, `engine: ssh`), exec, the workspace under the user's home, a shell job keeping its exit code,
a screenshot, fork refused with the reason, a memory checkpoint and restore returning the earlier file state, and delete.
**Not verified live:** the two-guest limit (the third create is 409; unit-level only). Shell jobs use `setsid` where it exists;
macOS has none, so a killed job loses its direct children but not necessarily deeper descendants.

### Installing through the API

`apple.install: true` installs from an IPSW as part of `POST /v1/vms` (run against a real 27.0.1 IPSW: install, `provision_*` and SSH
exec worked unattended; the sandbox SSH user for a macOS guest is `apple.provision_username`, not `sandbox`, which is a system account there):

```bash
curl -X POST localhost:7788/v1/vms -H 'Content-Type: application/json' -d @examples/macos-install.json
```

- The IPSW is `apple.media`, or `image` when `media` is unset. Use a local absolute path, or the name `macos`: FluxVM asks Apple for
  the newest restore image this Mac supports, downloads it once (26.6 GB for 27.0.1, so keep about 55 GB free: the image, then about 25 GB for the guest disk; cached as `images/macos-<build>.ipsw`, older ones removed)
  and installs from it. `fluxctl vz ipsw` shows which build that is and whether it is cached; `--download` fetches it ahead of time.
  URLs are still refused. `fluxctl create --name mac --image macos --guest macos --install` is the one-line form.
- FluxVM creates a sparse disk of `disk_size_gib` (default 64, minimum 40), runs `fluxvm-vz-runner install` to completion (progress
  events land in `vz-runner.log` in the VM workspace; the request returns only after the install, up to 3 hours), then boots the guest.
  A workspace marker (`macos-installed`) stops a restart from installing again; a failed install leaves no marker and is retried.
- The installed guest stops at Setup Assistant unless you use the macOS 27 `provision_*` options (see "Mac Studio options"). Stop the
  VM afterwards and use its `root.raw` as a template for clones.
- **Growing the disk later.** Stop the VM, then `fluxctl disk resize <id> root --size-gib N` (absolute, grow-only; refused with
  `apple.asif_overlay`). This extends the file only. The guest's APFS container still has to be grown from inside
  (`diskutil apfs resizeContainer`); FluxVM does not do that, and it is not verified on hardware whether the container can take the
  new space, since the recovery partition may sit after it.

### First-boot keys

`apple.firstboot: {"ssh_public_keys": ["ssh-ed25519 …"], "enable_remote_login": true}` shares a read-only folder into the guest at
`/Volumes/My Shared Files/firstboot` with `authorized_keys` and `firstboot.sh` (also in [`scripts/macos-firstboot-helper.sh`](../scripts/macos-firstboot-helper.sh)).
Run it once in the template as an admin, or from a LaunchDaemon you bake in: it installs the keys as a root-owned
`/etc/ssh/fluxvm_authorized_keys` (via `sshd_config.d`), which sshd reads before any home directory is unlocked, so fresh clones accept
the key on first boot, and turns on Remote Login. FluxVM cannot run it inside the guest for you.

### Snapshots of macOS guests

The same `snapshot` / `restore` endpoints work for macOS guests (macOS 14+ host); the snapshot also keeps the guest's NVRAM
(`auxiliary.bin`). Earlier runners ignored the saved state of a macOS guest and cold-booted it; the runner now resumes it the same
way as for Linux guests. Exercised once on an Apple M4 (macOS 27.2 host) with a clone of an installed macOS 27.0.1 guest
(4 vCPUs, 4 GiB, `usb_controller`): `POST /v1/vms/{id}/snapshot` returned 200 after 99 s on a USB-attached APFS drive, the VM stopped,
and `POST /v1/vms/{id}/restore` returned 200 after 15 s with the VM running and its address reported again. Not checked: that processes
inside the guest kept running across the restore (no SSH login was set up in that template).

Exec over vsock for macOS guests is **experimental and verified once** on one M4 and macOS 27.0.1 (next section). The `macos` image name looks up and downloads the
IPSW; that download path has not been run end to end (only Apple's metadata lookup was checked on an Apple silicon Mac). Apple
allows two macOS VMs at a time per Mac.

### Guest agent over vsock (experimental; verified on one M4 and macOS 27.0.1, including the host's choice of it)

By default a macOS guest has no vsock device and no FluxVM agent, and is reached over SSH only. `"apple": {"macos_vsock": true}` adds a
`VZVirtioSocketDeviceConfiguration` to the macOS guest. **Verified on an M4 with a macOS 27.0.1 guest:** a guest program can create
and bind an `AF_VSOCK` socket (family 40, Darwin's `sockaddr_vm`: length, family, 16 reserved bits, port, CID) with a stock `perl`,
and the host reaches it through the runner's `CONNECT <port>` proxy (`OK 1024`, then the guest's bytes). A port with no listener
returns nothing. **The agent was run too:** `crates/fluxvm-guest-agent/macos/install.sh` put it in the guest as a LaunchDaemon (root, token
at `/etc/fluxvm-guest-agent.token`, mode 0600), and from the host through the proxy `ping` answered `pong`, a wrong token was
refused, `exec` ran as root (`Darwin arm64`, `27.0.1`), `put-file` and `get-file` round-tripped, and an exec policy was refused
with "policy could not be enforced, command not run". Uninstalling removed the daemon, binary and token. The rest of the protocol was run too: `open-shell` gave a root PTY (a command
ran, `stty size` reported the `Resize` frame's 120x40); `reset-identity` applied the hostname (it sets `HostName`,
`LocalHostName` and `ComputerName`); `shutdown` powered the guest off and the runner reported `stopped`; and after that power
cycle the LaunchDaemon started by itself and answered `ping` with no reinstall. Not run: `regenerate_machine_id` and
`reseed_entropy` (reported as failures on macOS by design) and a guest with FileVault or a locked screen.

- **Guest side.** `fluxvm-guest-agent` builds for macOS (`cargo build -p fluxvm-guest-agent`). The vsock listener, exec, file
  transfer, shell (PTY), shutdown and hostname reset are ported; per-exec policies (Landlock/seccomp) are refused outright because
  they need Linux, and machine-id and entropy reseeding are reported as failures. Install it as a LaunchDaemon with
  `crates/fluxvm-guest-agent/macos/install.sh <binary> [token]` (plist: `dev.zyvor.fluxvm-guest-agent.plist`). A macOS guest has no
  cloud-init. Install the agent **once, without a token**, in the template: each clone then gets its own token from the host through
  the read-only `firstboot` share (see "Host side"). With no token anywhere the agent refuses everything. Passing a token to
  `install.sh` still works and always wins: the agent never replaces a token it already has with the share's.
- **Token delivery.** For a macOS guest with `apple.macos_vsock: true` and `agent.enabled`, the daemon writes the VM's agent token
  (generated when you gave none) to `agent-token`, mode 0600, in the guest's `firstboot` share (`/Volumes/My Shared Files/firstboot`,
  read-only) and attaches that share even when `apple.firstboot` is unset. The agent's LaunchDaemon passes `--token-seed` pointing
  there, tries it on a request while it has no token (the share may mount after the daemon starts), and keeps what it found in
  `/etc/fluxvm-guest-agent.token`. **Run on an M4** (`scripts/vz-macos-agent-seed-live-test.sh`, a macOS 26 guest installed from
  `macos-26A434.ipsw`): the template's agent had no token and refused, a clone's token file then equalled the VM's token, its log said
  it took the token from the share, and `POST /v1/vms/{id}/agent` ran `uname -sm` (`Darwin arm64`); a clone with the agent off got
  no share and no token. **Caveats:** a template that already holds a token keeps it (so the host's differs and requests are
  refused until you reset it); any process in the guest that can read the share can read the token, as with the other firstboot
  files, so this is for single-tenant sandbox guests; after a token rotation the share still holds the old token, which is only
  read when the guest has none. Not run: a macOS 27 guest, FileVault, or a fresh clone whose share mounts after login.
- **Host side.** Create the VM with `apple.macos_vsock: true` **and** `agent: {"enabled": true, "token": "<the token in the guest>"}`
  (or leave `token` out and use the delivery above)
  (admission used to refuse `agent.enabled` for every macOS guest, so the host could never choose the agent; it now needs
  `macos_vsock`). `vz_guest` pings the agent over the runner's vsock proxy (3 s) before each request and uses SSH when it does not
  answer. Verified: a VM created from a template with the daemon installed answered `exec` through the daemon's `/v1/vms/{id}/agent`
  as `root` and `fluxvm_vz_agent_calls_total` went up; while the guest's daemon was not yet listening every request fell back to
  SSH (`fluxvm_vz_agent_ssh_fallbacks_total`). On this guest the daemon started several minutes into boot. Exec policies and argv
  exec need the agent.
- **Checked:** the flag validates (macOS guests only), the runner config carries it, the Swift runner compiles, the agent's
  macOS build passes its unit tests (file transfer and PTY shell run against a local socketpair, the `sockaddr_vm` layout), and the
  fallback decision is unit-tested. **Checked on hardware:** that a macOS guest boots with the device, that `bind` on `AF_VSOCK`
  succeeds there, that the host's `connect` reaches it, and the agent's `ping`, token check, `exec`, file transfer and policy refusal.

## Display, audio, sharing and USB options

`request.apple` takes these first-party Virtualization.framework options:

```json
{"backend": "vz", "apple": {"guest_os": "macos", "window": true, "display_width": 5120, "display_height": 2880,
 "display_ppi": 220, "audio_output": true, "microphone": false, "usb_controller": true}}
```

The same options as flags, without a spec file:

```sh
fluxctl create --name mac --image ~/ipsw/UniversalMac.ipsw --guest macos --install \
  --display 5120x2880@220 --window --usb-controller
fluxctl create --name dev --image debian-13 --rosetta --clipboard --mute --vcpus 4
```

`--spec FILE` can be combined with these flags; a flag overrides the file's field. `--provision-full-name`,
`--provision-username` and `--provision-password-file` set the macOS 27 first-boot account.

- **Display:** `display_width` 800 to 5120 (default 2560), `display_height` 600 to 2880 (default 1600), `display_ppi` 72 to 300 (default 220).
  With `window: true` the guest follows the window as it is resized. Linux guests use `display_width` and `display_height` for
  their virtio-gpu scanout too (they used a fixed 1280x800 before).
- **Audio:** output to the host's default device is on by default. `microphone` is off by default; turning it on makes macOS ask for
  microphone access.
- **Shared folders on macOS guests** (macOS 13+ host): all entries share one automount device, so they appear under
  `/Volumes/My Shared Files`; two folders with the same name get a `-2` suffix. Linux guests keep the `fs0`, `fs1`, … tags.
- **Linux only:** `rosetta: true` adds a Rosetta share (the guest still mounts it and registers binfmt); `nested_virtualization: true`
  needs macOS 15 and an M3 or later, and fails clearly otherwise. Both are refused for macOS guests.
- **USB:** `usb_controller: true` adds an XHCI controller (macOS 15+). Attaching a disk image to a running guest is covered under
  "Extra disks" and "Mac Studio options"; a physical device can be attached through Accessory Access (see "Mac Studio options"; not verified).
- **Console log:** a Linux guest's serial console goes to the VM's `console.log`. Past `serial_log_max_mib` (default 16; a key
  of the `[apple]` table in `fluxvm.toml`, not a request field) the runner moves it to `console.log.1`, replacing the previous one, and starts a new file, so a chatty guest cannot fill the
  disk. `sandbox logs` reads both files; `GET /v1/vms/{id}/serial` follows the current one.

### Agent screen and input

An agent can see and drive the display of any running `vz` VM, Linux or macOS guest, with no console window and no Screen
Recording or Accessibility permission on the host: the runner renders the display into a hidden window of its own and sends
keyboard and mouse events to it.

```sh
fluxctl screenshot web --out screen.png --max-width 1280
fluxctl input web '{"action":"click","x":600,"y":400}' --text 'ls -la
'
fluxctl input web '{"action":"key","key":"c","modifiers":["control"]}'
```

The REST routes are `GET /v1/vms/{id}/screenshot` and `POST /v1/vms/{id}/input` (admin only, see [api.md](api.md)); the MCP
tools are `vm_screenshot` and `vm_input` ([mcp.md](mcp.md)).

- **Size:** a display 1920 pixels or wider is captured at half its pixels (a 2560x1600 display gives 1280x800, the size macOS
  shows it at on a Retina screen); a smaller one at its own size. `max_width` scales down further.
- **Coordinates** are screenshot pixels from the top left. After scaling with `max_width`, pass the same number as
  `screen_width`.
- **Typing** uses a US layout and covers printable ASCII, newline and tab. Keys: a character, `enter`, `tab`, `escape`,
  `backspace`, `delete`, arrows, `home`, `end`, `page_up`, `page_down`, `f1`-`f12`; modifiers `shift`, `control`, `option`,
  `command`.
- **Pointer:** the guest gets absolute positions (a Linux guest's USB screen-coordinate digitizer, a macOS guest's trackpad), so a
  click lands where the screenshot shows it; `right_click` and `middle_click` send those buttons, and `scroll` moves by wheel
  notches.
- **Verified** on macOS 27.2 (M4) with a Debian 13 guest: console login by typing, Ctrl+C, and left/right/middle clicks, drag and
  scroll checked with evdev in the guest, before and after a guest reboot and a stop/start.

### Guest self-control

With `"apple": {"self_control": true}` (Linux guests), software inside the VM, such as a coding agent, can snapshot,
restore and restart the VM it runs in through an MCP server at `http://127.0.0.1:7790/mcp` in the guest:

### Extra guest→host vsock services

`apple.vsock_services` opens more vsock ports from the guest to the host, and only the ports it lists (Linux guests; the guest
reaches the host at CID 2). Each entry is `{"port": N, "builtin": "metadata" | "telemetry"}`, `{"port": N, "socket": "name"}` or `{"port": N, "tcp": "127.0.0.1:PORT"}`:

- `metadata`: the guest connects and reads one JSON line (`id`, `name`, `vcpus`, `memory_mib`); the runner then closes.
- `telemetry`: the guest writes lines and the runner appends them to `<workspace>/telemetry.log`, up to 8 MiB, then drops the rest.
- `tcp`: `{"port": N, "tcp": "127.0.0.1:PORT"}` relays the guest's connections to a TCP server on this Mac. Loopback only:
  the host must be `127.0.0.1` (or `localhost`) and `PORT` 1024-65535; anything else is refused at create time, and the runner
  receives only a port and connects to the constant `127.0.0.1`. **Verified on an M4 with a stand-in model server**
  (`examples/agent-sandbox-demo/local-model.sh`: a sandbox with only `lo` reached `/v1/models` and a chat completion on
  the Mac's loopback and still could not reach the internet), and again against a real `llama-server` serving
  `SmolLM2-135M-Instruct-Q8_0.gguf`. Not run against MLX (`mlx_lm.server`) or a larger model.
  See "Local model server" below.
- `socket`: connections are relayed to `/tmp/fluxvm-<uid>/<vm id>.svc-<name>`. The host operator runs whatever listens there. The
  name is `[a-z0-9_-]{1,32}`, so a spec cannot point the guest at another host path (such as the daemon's own socket).

#### Local model server (host inference over vsock)

To give a guest an OpenAI-compatible endpoint backed by the Mac's GPU, run MLX (`mlx_lm.server --port 8080`) or llama.cpp
(`llama-server --port 8080`) on the host, bound to `127.0.0.1`, and create the VM with
`fluxctl create --vsock-service 5100=tcp:127.0.0.1:8080` (no network card needed). Inside the guest, forward a local port to
the vsock service (CID 2, port 5100) and point the client at it:

```sh
# inside the guest; the tcp service is allow-listed, so only this one host port is reachable
python3 - <<'PY' &
import socket, threading
def pump(a, b):
    try:
        while (d := a.recv(65536)): b.sendall(d)
    finally: a.close(); b.close()
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 8080)); srv.listen(16)
while True:
    c, _ = srv.accept()
    v = socket.socket(socket.AF_VSOCK); v.connect((2, 5100))
    threading.Thread(target=pump, args=(c, v), daemon=True).start()
    threading.Thread(target=pump, args=(v, c), daemon=True).start()
PY
curl http://127.0.0.1:8080/v1/models     # OPENAI_BASE_URL=http://127.0.0.1:8080/v1 for SDKs
```

The host server sees every request from the guest, so run it with the model and prompts you are willing to expose to that VM;
the relay adds no authentication.

Ports must be 1024-65535, not 3128 (egress proxy), 7790 (self-control), 7791 or 22, at most 16 per VM. Create with
`fluxctl create --vsock-service 5001=metadata --vsock-service 5003=socket:echo --vsock-service 5100=tcp:127.0.0.1:8080`. From a guest with python3:
`s=socket.socket(socket.AF_VSOCK); s.connect((2, 5001)); print(s.recv(4096))`.
Verified on a Debian 13 guest by `scripts/vz-vsock-services-live-test.sh`.

```sh
# inside the guest
claude mcp add --transport http fluxvm http://127.0.0.1:7790/mcp
```

| Tool | Does |
| --- | --- |
| `self_info` | id, name, status, vCPUs, memory, snapshots, and the outcome of the last snapshot, restore or restart |
| `self_snapshot_list` | snapshots (tag, time, size) |
| `self_snapshot` | saves memory and disk under `tag`; the VM pauses briefly |
| `self_snapshot_restore` | rewinds the VM, including the calling program, to `tag` |
| `self_snapshot_delete` | removes `tag` |
| `self_restart` | stops and starts the VM |

cloud-init installs a small relay (`fluxvm-self.service`) from that port to vsock; the runner passes each connection to the
daemon and names the VM itself, so a guest can only act on its own VM, with no network card, token or host port involved.
Snapshot, restore and restart start about a second after they answer, so the reply reaches the guest before it is paused,
rewound or rebooted; `self_info` then shows whether it worked, and a second one is refused while the first is still running.
Verified on macOS 27.2 with a Debian 13 guest: a file changed after `self_snapshot` was back to its old content after
`self_snapshot_restore` (same boot id: memory was restored, not rebooted), and `self_restart` gave a new boot id.

### Control preconditions, display resize and per-feature minimum macOS

- The runner checks `canStart`, `canPause`, `canResume`, `canRequestStop` and `canStop` before `pause`, `resume`, `shutdown` and
  `stop`, and answers with a plain reason (`cannot pause: the VM is paused; pause needs a running VM`) instead of a Virtualization
  error from the middle of a transition. **Partly verified on hardware:** a second `pause` on a paused VM came back as `cannot pause: the VM is paused; pause needs a running VM`; the other checks are compiled against the macOS 27 SDK only.
- `display-resize` on the runner control socket calls `VZGraphicsDisplay.reconfigure(configuration:)` (macOS 14+) on a running or
  paused VM. From Rust: `fluxvm_apple::display_resize(&vm, width, height, display, ppi)`; `ppi` only applies to macOS guests.
  REST: `POST /v1/vms/{id}/vz/display` with `{"width": 1920, "height": 1080, "display": 0, "ppi": 220}` (`display` and `ppi`
  optional; admin role; tenant-scoped like the other `vz` routes; the size is validated before the VM is looked up), CLI:
  `fluxctl vz display resize <vm> --width 1920 --height 1080 [--display N] [--ppi N]`. The route is covered by API tests against a
  stopped VM (validation, tenant and role checks).
- **What a resize does, measured on the M4 with a Debian 13 guest (partial).** The call succeeds and the DRM connector's
  *preferred* mode in the guest changes to the new size. The *active* mode does not: fbcon's `virtual_size` and the console
  resolution stay as they were. A headless guest has no KMS client, and switching the active mode takes one (a compositor, or a
  tool that sets a mode through the DRM/KMS API); without one the guest keeps its mode and what a later modeset picks is not
  deterministic. So the reply means "the host asked for this size", not "the guest now runs at it": the REST and CLI reply carry
  `"requested": true`, `"applied_to_guest": "not confirmed"` and a `note` saying so, and `ok` only reports that the runner
  accepted the call. To get a guest to the new size, run a compositor or set the mode from inside the guest after the resize.
  macOS guests follow through their own window server and were not measured.
- Every capability-table entry has a `min_macos` (`None` means the runner floor, macOS 14); for example USB controller and nested
  virtualization 15, vmnet 26, custom Virtio, Secure Boot, ASIF overlay and macOS provisioning 27. When the daemon has probed the
  host (it does so once, before the first `vz` create), `fluxvm_apple::validate_request` refuses with the real requirement
  (`apple.vmnet needs macOS 26 or later; this Mac runs macOS 15 (...)`). Without a probe only the static table applies.
- **MTU.** Virtualization has no MTU property on `VZNATNetworkDeviceAttachment` (the macOS 27 SDK has `maximumTransmissionUnit` only on
  `VZFileHandleNetworkDeviceAttachment`, range 1500 to 65535), so the host's NAT always passes 1500. What is configurable:
  `apple.nat_mtu` (576 to 1500, Linux guests on the NAT or a bridge) lowers the MTU the *guest* sets on its primary card through the
  seed's network-config, for a path that cannot carry 1500; it cannot raise it. `apple.usernet.mtu` (576 to 9000) is advertised by
  the gateway's DHCP and, above 1500, now also sets the attachment's `maximumTransmissionUnit`; before that the guest kept 1500
  and every jumbo frame was dropped. Private networks (`apple.networks[].mtu`, 1280 to 9000) carry jumbo frames too: see
  "Private networks between guests".

### Host-side accounting and memory pressure

- Virtualization.framework reports nothing per VM, and the runner process is not the guest: on macOS the guest's vCPUs and
  memory live in a separate `com.apple.Virtualization.VirtualMachine` XPC service (parent launchd; its responsible pid is the
  user's shell, so neither the parent pid nor `responsibility_get_pid_responsible_for_pid` links it to a runner). Measured on an
  M4 with 2 vCPUs pinned: runner 0.0% CPU and 73 MB RSS against the service at 200% and 710 MB.
- FluxVM therefore sums the runner and its service. The link is the open files, read with public libproc calls: the service
  holds the VM's root disk and `efi.bin` open, and those live in the VM's own workspace, so
  `fluxvm_apple::vm_usage(&[(runner_pid, workspace)])` lists the Virtualization service processes (`proc_listallpids`,
  `proc_pidpath`, `PROC_PIDLISTFDS`, `PROC_PIDFDVNODEPATHINFO`) and attributes each to the workspace its files sit in (a service
  matching two workspaces is attributed to none). `/metrics` exposes the result for running or paused `vz` VMs as
  `fluxvm_vm_runner_rss_bytes{vm,name}` (gauge) and `fluxvm_vm_runner_cpu_seconds_total{vm,name}` (counter), plus
  `fluxvm_vm_runner_service_attributed{vm,name}` (1 when the service is included, 0 when the numbers are the runner process
  only and the guest's CPU and memory are not in them). The metric names keep the `runner` prefix for dashboard compatibility.
  These are host-side numbers, not what the guest sees. Verified against a live Debian 13 guest (service pid found by its
  open `root.raw`, 542 MB and 7 s CPU against the runner's 58 MB and 0.1 s). Not covered: a guest whose disk is not a file in
  the workspace and has no `efi.bin` (it falls back to the runner only, flagged by the 0), and a different user's VMs.
- Memory pressure was already wired before this change: `kern.memorystatus_vm_pressure_level` is sampled (normal, warn,
  critical; also `fluxvm_host_memory_pressure_level` in `/metrics`), and `fluxvm_scheduler::density::reclaim_after_secs` shortens
  the idle time before a sandbox's balloon is inflated to 60 s under warn or critical pressure. Normal pressure leaves the
  configured `sandbox.idle_balloon_secs` unchanged.

### Boot attestation (a host claim, not hardware attestation)

For a VM with a `metadata` vsock service, the document the guest reads gains an `attestation` field: a manifest signed with an
ed25519 key the daemon keeps in its state directory (`vz-attestation.key`, mode 0600, created on first use; the public half is
`vz-attestation.pub`). The manifest lists the kernel, initrd, command line (sha256), root disk, runner binary and the `apple`
spec digest, plus the VM id and the issue time. A root disk or other file over 256 MiB is not hashed; it carries
`"digest": "skipped"` with its size and mtime. The signature covers the exact `payload` text, so verify it as received:

```rust
let manifest = fluxvm_apple::verify_attestation_hex(&attestation_json, trusted_public_key_hex)?;
```

What it does and does not prove:

- It records what the host says it started, signed by a key a file on the host holds. A guest or operator that already trusts
  that public key can detect a different host key, or a document altered in transit or at rest.
- It is **not** a hardware root of trust. Virtualization.framework has no TPM and no measured boot, the key is not in the Secure
  Enclave, and a compromised host (or anyone who can read the key file) can sign anything. Nothing proves the host booted the
  bytes it listed, and files over the hashing limit (typically the root disk) are only identified by size and mtime.
- The public key must reach the guest or verifier out of band; the `public_key` field inside the document is informational and
  is never used to verify.
- If the manifest cannot be produced (for example an unreadable file) the boot continues and the metadata has no `attestation`
  field; a verifier must treat a missing field as a failure.
- Unit-tested (sign, verify, tamper, key file mode); **not exercised** end to end on a booted VM.

**Delivery to an out-of-band verifier.** Each launch also keeps the attestation object in the VM's workspace
(`vz-attestation.json`; a launch that produced none removes the previous file). `GET /v1/vms/{id}/vz/attestation` (any role,
tenant-scoped, any VM state once it has launched; 400 before the first launch) returns
`{"vm_id", "claim", "issued_at_unix", "attestation": {alg, payload, signature, public_key}, "manifest": {...}, "public_key": "<hex>"}`;
`manifest` is `payload` decoded for convenience, and `public_key` is the daemon's key from `vz-attestation.pub` (read, never
created by the request). Verify it with:

```sh
fluxctl vz attestation verify --vm web                       # against the host-served key: a consistency check only
fluxctl vz attestation verify --vm web --trusted-key <hex>   # against a key you obtained out of band
```

The command uses `fluxvm_apple::verify_attestation_hex`, prints the manifest on success and exits non-zero when the signature does
not match. Without `--trusted-key` the host vouches for itself, so the output carries a warning; only a key you already trusted
gives the check any meaning. Everything above about what the signature does not prove still applies. The route and CLI are tested
against synthetic attestations (API handler, tenant scoping, sign/verify round trip); **not exercised** against a booted VM.

### Stored sign-in

`fluxctl signin` keeps a guest username and password in the host's login Keychain (service `dev.zyvor.fluxvm.vm-signin`,
account: the VM id) and types them into the display, so you, a script or an agent (MCP `vm_sign_in`) can get past a login
screen without handling the password:

```sh
fluxctl signin set web --user velora        # prompts for the password with echo off, or reads it from stdin
fluxctl signin type web --mode username     # username, Enter, password, Enter: a text console login
fluxctl signin type web                     # just the password, e.g. a macOS lock screen
fluxctl signin status web                   # {"configured": true, "username": "velora"}
fluxctl signin clear web
```

`--mode username_tab` types the username, Tab and the password for a graphical form; `--no-submit` leaves out the final Enter.
The password is never returned by the API, the CLI or MCP, and deleting the VM removes the item. Verified on macOS 27.2 with a
Debian 13 guest's tty1 login.

### Named console ports

`apple.console_ports` (Linux guests, up to 8) adds virtio console ports for a byte stream between host and guest that needs no
network and no agent, for example a debug shell, a log pipe or a custom control channel:

```json
"apple": {"console_ports": ["agent", "debug"]}
```

- **Guest:** each port appears as `/dev/virtio-ports/<name>` (udev creates the link; without udev, find the name in
  `/sys/class/virtio-ports/vport*/name`).
- **Host:** the runner bridges each port to a unix socket, `/tmp/fluxvm-<uid>/<vm id>.port-<name>` (mode 0600). One client at a
  time; a new client replaces the old one. Guest output while no client is connected is dropped, and so is host input while
  no guest process has the port open: have one side announce itself before the other sends (the guest can keep one
  descriptor for both directions, e.g. `exec 3<>/dev/virtio-ports/<name>`).
- **Clients:** `fluxctl port-connect <vm> <name>` connects stdin and stdout (Ctrl-] detaches), locally or with `--server`. Over
  REST, the websocket `GET /v1/vms/{id}/ports/{name}` (admin) carries raw bytes both ways.
- **Names:** 1-32 of `a-z`, `0-9`, `.`, `-` and `_`, starting with a letter or digit. Ports are fixed when the VM starts.

## Private networks between guests

On the Mac's NAT, Virtualization.framework keeps guests apart: each reaches only the Mac. `apple.networks` adds a second network card
on a private layer-2 network that only the guests joining it share (Linux guests):

```json
{"backend": "vz", "apple": {"networks": [{"name": "shop"}, {"name": "lab", "address": "10.89.7.20/24"}]}}
```

- **Addresses:** each network name gets its own `10.89.N.0/24`. A guest gets the lowest free host address (`.2` and up) and a random
  locally administered MAC, unless it asks for `address` or `mac`. Both are kept in the VM record, so they survive restarts. A
  requested address must be in the network's subnet and free. At most 4 networks per guest; names are `a-z`, `0-9` and `-`.
- **MTU:** `"networks": [{"name": "lab", "mtu": 9000}]` (1280 to 9000, default 1500) is a property of the whole network. The first
  member defines it, a later member that omits `mtu` inherits it, and one that names another value is refused. The runner raises
  that card's `maximumTransmissionUnit` and the guest sets the same MTU (a `[Link] MTUBytes=` line in its networkd unit, `mtu` in
  the fallback script and the seed's network-config), and the switch accepts frames up to 9216 bytes. `GET /v1/vznets` reports the
  network's `mtu` when it is not 1500. Container sandboxes (`oci.networks`) are not told the MTU and stay at 1500.
- **In the guest:** container sandboxes (`oci.networks`) get the address from init, matched by MAC, with no route and no DHCP on that
  card. Full VMs get it through cloud-init: a systemd-networkd unit per card, and a `fluxvm-vznet.service` oneshot that sets the
  same address with `ip` on guests without networkd.
- **The switch:** each network is one `fluxvm-vz-switch` process (installed next to `fluxctl`, or `FLUXVM_VZ_SWITCH`), started on
  demand and listening on `/tmp/fluxvm-<uid>/vznet-<name>.sock` (mode 0600). The runner hands it one end of a datagram socket pair
  (`VZFileHandleNetworkDeviceAttachment`). The switch forwards by the MAC each port registered with, drops frames with any other
  source MAC, floods broadcast and multicast, and drops unicast to unknown MACs. It exits 30 s after the last guest leaves; a runner
  that loses its switch starts it again and reconnects. A plain build of the switch includes the `netobs` frame export
  (`--netobs-socket PATH [--network NAME]`, see [netobs](netobs.md)); `--no-default-features` builds one without it.
- **Isolation:** guests on different networks, or not on one, cannot reach each other. A network belongs to the tenant of the VMs
  already on it. `networks` works with `network.mode = "none"` (an isolated cluster) but not with `egress_allow`, since another guest
  could relay around the proxy.
- **Listing:** `fluxctl vznet ls` and `GET /v1/vznets` show each network's subnet, members and whether its switch is running.

## Extra disks

A Linux guest takes extra disks in `apple.extra_disks`, or with `fluxctl disk attach` (`POST /v1/vms/{id}/disks`):

```json
"apple": {"extra_disks": [
  {"name": "scratch", "path": "/Volumes/Fast/scratch.raw", "caching": "uncached", "sync": "none", "controller": "nvme"},
  {"name": "raw", "kind": "block", "path": "/dev/disk4", "read_only": true},
  {"name": "shared", "kind": "nbd", "url": "nbd://10.0.0.5:10809/vol1"}
]}
```

| Field | Values |
|-------|--------|
| `kind` | `image` (a raw file, default), `block` (a host device, `/dev/diskN`), `nbd` (an NBD export in `url`), `overlay` (copy-on-write overlay on a base image, see below; verified once on an Apple M4, see the hardware log) |
| `caching` | `automatic` (default), `cached`, `uncached`; image files only |
| `sync` | `full` (default), `fsync` (image files only), `none` (fastest; data can be lost if the host crashes) |
| `controller` | `virtio` (default, `/dev/vdX`), `nvme` (`/dev/nvmeXn1`), `usb` (`/dev/sdX`) |

- `block`, `nbd` and `nvme` need macOS 14. A block device must be readable (and, unless `read_only`, writable) by the user the
  daemon runs as; FluxVM never changes device permissions. Image files and devices go through the same
  `policy.allowed_image_dirs` check as QEMU data disks.
- NBD URLs are `nbd://host:port/export`, `nbds://…` (TLS), or `nbd+unix:///export?socket=/path`. Virtualization.framework
  reconnects when the server drops; a missing server fails the VM's start.
- `overlay` (verified once against Virtualization.framework's NBD client, see the hardware log): `{"kind": "overlay", "path": "/images/base.raw",
  "overlay": "/images/vm1.ovl", "read_only": false, "iops": 500, "bps": 10485760}` serves `overlay` (created on top of `path` when
  missing; `path` is never written) through a `fluxvm-blockd serve` the daemon starts on 127.0.0.1 with an ephemeral port before the
  runner and stops when the runner exits. The runner sees an ordinary `nbd` disk (`nbd://127.0.0.1:<port>/<disk name>`), so the
  same macOS 14 requirement and start-time attachment apply. `fluxvm-blockd` is found next to `fluxctl` or through
  `FLUXVM_BLOCKD`; its output goes to `<workspace>/blockd.log`. An overlay file belongs to exactly one VM record: create, `disk
  attach` and start refuse an overlay that is another record's overlay, base image, extra image or root disk (running or stopped),
  that one VM names twice, or whose base is an overlay another VM still writes (a chain parent in use). A clone of a VM with
  overlay disks is refused for the same reason. Paths are compared after resolving symlinks and `..`; the check covers FluxVM's own
  records, not other processes. Paths must be absolute and contain no comma. Encryption, lazy HTTP bases (both verified once, see the hardware log), `branch`, `rebase` and
  `resize` are `fluxvm-blockd` CLI features, not request fields; see [flux-disk.md](flux-disk.md).
- Virtualization.framework fixes a VM's devices when it starts, so `fluxctl disk attach` and `detach` apply at the next start. The
  exception is a USB image disk on a running guest: attach plugs it in at once (macOS 15), and detach unplugs it at once as long
  as the VM has not restarted since. A new image made with `--size-gib` is deleted on detach once nothing uses it.

```bash
fluxctl disk attach web scratch --size-gib 20 --controller nvme --caching uncached   # a new sparse image in the VM's workspace
fluxctl disk attach web shared --url nbd://10.0.0.5:10809/vol1 --read-only
fluxctl disk ls web
fluxctl disk detach web scratch
```

## Mac Studio options

These compile against the macOS 27 SDK and are validated at admission. Except for USB disk hot-attach and the share swap, which
`scripts/vz-devices-live-test.sh` and `scripts/oci-live-test.sh` exercise, they have **not been verified on hardware** yet, except where an item below says otherwise (host capabilities, fleet placement and the custom Virtio control channel).

- **Several displays (macOS guests):** `display_count` 1 to 8 (default 1), each `display_width` x `display_height`.
- **Clipboard (Linux guests):** `clipboard: true` adds a SPICE agent port; the guest needs `spice-vdagent`. Refused for macOS guests.
- **Bridged networking:** `bridge_interface: "en0"` puts the guest on the host's LAN (for example a Mac Studio's 10GbE) instead of NAT.
  Needs `network.mode = "user"` with no `forwards`, and a runner signed with `com.apple.vm.networking`: build with
  `FLUXVM_VZ_BRIDGE=1 cargo build -p fluxvm-apple` (ad-hoc signing of that entitlement only works with SIP/AMFI relaxed or a
  provisioning profile).
- **vmnet networks (macOS 26+):** `vmnet: {"mode": "shared" | "host-only", "subnet": "192.168.105.0", "mask": "255.255.255.0",
  "reserved_ip": "192.168.105.10", "forwards": [{"protocol": "tcp", "host_port": 8080, "guest_port": 80, "guest_ip": "192.168.105.10"}]}`
  gives the VM its own network with a stable DHCP reservation for its MAC and TCP/UDP host forwards. Admission checks that the mask is
  contiguous (/30 or wider) and that `reserved_ip` and each forward's `guest_ip` are inside the subnet. The SDK has no DHCP pool setter,
  so the whole subnet is served and `dhcp_start`/`dhcp_end` are refused. The other vmnet settings are passed through:
  `ipv6_prefix` (`"fd00:1::/64"`), `mtu` (1280-9000), `external_interface` (shared mode), and the switches `disable_dhcp`
  (refused with `reserved_ip`), `disable_dns_proxy`, `disable_nat44`, `disable_nat66` and `disable_router_advertisement`; a named
  network's fingerprint includes them. Each runner owns its network unless the VMs name it: `vmnet.name` (macOS 26+) makes them share one network through the `fluxvm-vmnetd` broker over XPC
  ([VMNET_BROKER.md](VMNET_BROKER.md)). **Not verified, and now explained:** on the test Mac the ad-hoc signed broker was killed at launch because it claimed the restricted `com.apple.vm.networking`; a runner without that entitlement also cannot use a vmnet network at all (see the hardware log), so two runners on one name cannot run on a Mac without Apple's provisioning profile; with no broker a named VM fails with "vmnetd XPC connection failed". Exclusive with `bridge_interface`. IP discovery and the 127.0.0.1 `network.forwards`
  relay read the NAT lease file, so use `vmnet.forwards` instead.
- **Userspace gateway (`apple.usernet`, Linux guests, PARTLY verified on hardware, see the hardware log):** replaces the NAT card with a file-handle NIC whose
  peer is a supervised `fluxvm-usernet` process (ARP, DHCP `10.0.2.15/24`, DNS forwarder, TCP/UDP NAT, egress policy, port forwards).
  Spec: `"usernet": {"allow_cidrs": ["93.184.216.0/24"], "deny_cidrs": [], "allow_ports": [], "deny_ports": [25],
  "dns_allow": ["example.com"], "dns_deny": [], "allow_private": false, "mtu": 1500,
  "port_forwards": [{"protocol": "tcp", "host_addr": "127.0.0.1", "host_port": 8080, "guest_port": 80}]}`. Any `allow_cidrs` or
  `dns_allow` entry makes egress a strict allow-list; the default is the public internet without private ranges. Admission refuses it for
  macOS guests and together with `network.mode = "none"`, `bridge_interface`, `vmnet` or `network.forwards`; CIDRs, ports (1-65535), host
  ports (0 or 1024-65535), forward protocol/address and MTU (576-9000) are checked. The daemon creates the NIC socketpair, the runner
  and the gateway each inherit an end (runner fd 3, `usernet_fd` in the runner config; gateway `--fd 3`), and the daemon restarts a
  gateway that dies for as long as the runner lives; the gateway also exits when the runner is gone. The gateway needs `fluxvm-usernet`
  next to `fluxctl` (or `FLUXVM_USERNET`). With network observability on, the gateway copies each frame to the daemon (`--tap-fd`),
  so `/v1/netobs/*` sees real frames for the VM (the gateway runs with tap version 2, so a frame it denied by policy arrives with a `denied` verdict and one it dropped for another
  reason, such as a spoofed source, as `dropped`; the details are in the gateway log, `<workspace>/usernet.log`). IPv6 is opt-in:
  `"ipv6": true` serves a ULA /64 by SLAAC with the same egress policy, `allow_cidrs6` / `deny_cidrs6` take IPv6 CIDRs, and a forward
  can set `"guest_family": "v6"` (with an optional `"guest_addr"` inside the /64; otherwise the last address the guest used). Frames reach the observer only when the VM is launched by the `fluxctl serve` daemon. IP discovery and
  the legacy 127.0.0.1 `network.forwards` relay read the NAT lease file and do not work: use `usernet.port_forwards`. Booted and verified on an
  M4 (see the hardware log); the file-handle attachment path is the same one private networks use.
- **Custom Virtio (macOS 27+, Linux guests):** `custom_virtio: true` adds a vendor Virtio device (id `0x3F`, PCI `1af4:107f`; an earlier id, `0xFF00`, was outside the range Linux binds) with a host-side provider. Queue 0 is a bounded (1 MiB) versioned JSON control channel (`ping`, `echo`, `capabilities`, `stats`, `map-probe`); queue 1 is the bulk queue (`bulk_zero`, `fill`, `copy`, `crc32`, up to 64 MiB). The guest driver is in `guest/virtio-flux` (`/dev/fluxvm`, `/dev/fluxvm-bulk`, `fluxvm_virtioctl`). **Verified** on the M4 with a Debian 13 guest: `ping`, `echo`, `stats` and `capabilities`. `fluxvm_virtioctl bulk-test`, `bulk-zero-test`, `bulk-copy-test` and `bulk-crc-test` (queue 1, 1 B to 1 MiB) pass after the driver stopped using a stack buffer for its request; the 64 MiB host cap was not tried. See [macos-vz27-full-stack.md](macos-vz27-full-stack.md).
  The device also handles DRIVER_OK, stop, pause, resume and reset, and saves its counters with a VM snapshot (the configuration sets
  `supportsSaveRestore`; without it Virtualization refuses to save a VM with the device). **Verified** on the M4: snapshot, stop and
  `start-from-snapshot` resumed the Debian guest without a reboot, `ping` and `echo` worked, and the counters continued. The control socket takes
  `{"cmd":"virtio-status"}` (driver state and counters) and `{"cmd":"virtio-reset"}` (host-initiated reset: the device sets DEVICE_NEEDS_RESET, which Linux ignores unless the
  driver handles it; `virtio_flux` resets the device, aborts requests still queued with `EIO`, and sets the queues up again. Needs
  `CONFIG_PM_SLEEP`. **Verified** on the M4: after two resets in a row, `ping` and `bulk-test` worked);
  `fluxvm_apple::vz27` wraps both.
- **EFI Secure Boot (macOS 27+, Linux EFI guests):** top-level `secure_boot: true` enrolls Microsoft's KEK, UEFI CA and revocation list
  and enables Secure Boot with Apple's platform key, so Microsoft-signed shims boot. `apple.efi_secure_boot` customises it:
  `platform_key` (X.509, DER or PEM), `kek`/`db`/`dbx` (certificates, SHA-256 hashes or EFI signature lists),
  `default_signatures: false` and `reset: true` (clear previously enrolled keys first). `secure_boot: false` turns it off and keeps the
  keys; leaving it out does not touch `efi.bin`. Refused with direct kernel boot and for macOS guests. `{"cmd":"secure-boot-status"}` reports
  the state and signature counts. **Verified** on the M4: enable with default keys (2 KEK, 2 db, 26 dbx), status, disable, and a stock Debian 13 cloud image booting through the API with `secure_boot: true` (guest efivars: `SecureBoot=1`, `SetupMode=0`). The guest kernel reports "Secure boot enabled" and runs in lockdown (integrity), so it loads only signed modules: an out-of-tree
  module such as `virtio_flux.ko` must be signed with an enrolled key (for example a MOK) to load under Secure Boot. While the VM runs,
  Virtualization keeps `efi.bin` locked, so `secure-boot-status` returns the state read just before start, marked `"as_of": "boot"`;
  with the VM stopped it reads the store.
- **Recovery (macOS guests):** `recovery: true` starts the guest in macOS Recovery.
- **Rosetta cache (Linux guests):** with `rosetta: true`, `rosetta_cache` is `"default"`, a guest socket path or an abstract socket name for
  `rosettad`'s AOT cache. A Mac without Rosetta fails at boot with a pointer to `fluxvm-vz-runner install-rosetta`.
- **Disk serials:** an `extra_disks` entry on the virtio controller can set `block_device_id` (1-20 ASCII), shown in the guest as
  `/dev/disk/by-id/virtio-<id>`.
- **VM label (macOS 27+):** the runner sets `VZVirtualMachineConfiguration.label` to the VM's name (trimmed to 64 characters) so system
  services show it; the console window uses `VZVirtualMachineViewAdaptor`.
- **Events:** the runner log gets `network-disconnected` (a bridged or vmnet attachment went away), `nbd-connected` / `nbd-failed`,
  `usb-passthrough-disconnected` (the host took a device back) and `secure-boot` lines.
- **Memory balloon:** `GET /v1/vms/{id}/balloon` and `POST /v1/vms/{id}/balloon {"balloon_mib": N}` work for `vz` VMs too (the runner sets
  the balloon target), and idle reclaim inflates idle `vz` sandboxes the same way as KVM ones.
- **USB disk hotplug:** with `usb_controller: true`, the runner's control socket accepts
- **USB buses:** `apple.usb_controllers` (1-4, implies `usb_controller`) creates that many XHCI controllers. Give a hot-attached
  USB disk `usb_bus` (`fluxctl disk attach --usb-bus N`) to choose its bus; the default is bus 0. Disks present at boot are placed by
  Virtualization.framework, and physical USB passthrough uses bus 0. The guest shows one more XHCI controller than configured.
  `{"cmd":"usb-attach","path":"/path/disk.img","read_only":false}` (returns a `uuid`) and `{"cmd":"usb-detach","uuid":"…"}` (macOS 15+).
  `{"cmd":"usb-list"}` lists what is attached now. A snapshot is refused while a hot-attached USB disk is still attached: macOS 27 can
  crash restoring such a state without the disk (177528319). Passed-through devices are detached before saving instead (174267926), and
  the `save` reply lists them in `usb_detached`.
- **Physical USB passthrough (macOS 27):** the runner's control socket also takes `{"cmd":"usb-physical-list"}` and
  `{"cmd":"usb-physical-attach","registry_id":…}`. It goes through Apple's Accessory Access consent, so it needs the `FluxVMUSBAccess` app
  signed with the `com.apple.developer.accessory-access.usb` entitlement (an Apple entitlement). There is no REST route or `fluxctl`
  command. **Not verified, and not fixable by ad-hoc signing:** the broker app claims `com.apple.developer.accessory-access.usb`, a restricted entitlement that macOS only honours with an Apple-issued provisioning profile; on the test Mac the ad-hoc signed app was killed at launch (the same kernel check as for `com.apple.vm.networking`, see the hardware log) and no USB device was attached. A Developer ID build with the profile is required.
- **Share swap:** `{"cmd":"share-set","tag":"fluxvm-vol0","path":"/dir","read_only":false}` points a running guest's virtiofs share at
  another directory. The container sandbox warm pool ([oci-sandboxes.md](oci-sandboxes.md#warm-pool)) uses it with USB hot-attach
  to hand a pre-booted VM its rootfs and volumes.
- **ASIF overlay (macOS 27+):** `asif_overlay: true` keeps the base disk read-only and writes to a sparse `disk-overlay.asif`, which
  snapshots include. Verified on a Linux guest by `scripts/vz-asif-overlay-live-test.sh` (guest writes grow the overlay, the base is
  byte-identical afterwards); not run with a macOS guest. A plain APFS clone (`cp -c`) is already instant and copy-on-write, so
  layering several overlays over a shared base is not built: it would save little over what `clone` does today.
- **Unattended first boot (macOS 27 guests):** `provision_full_name`, `provision_username`, `provision_password_file` (a one-shot file the
  runner deletes after reading), `provision_auto_login`, `provision_remote_login` create the user and turn on Remote Login without Setup
  Assistant.
- **Host capabilities:** `fluxvm-vz-runner host-capabilities` prints the host's OS version, CPU and memory, `maximumVmCPUs`, nested
  virtualization, vmnet, custom-Virtio and USB API support and bridgeable interfaces; `fluxvm_apple::host_capabilities` decodes it.
  (`{"cmd":"capabilities"}` on a runner's control socket reports a running VM's view.) **Verified** on the M4.
- **Fleet placement for `vz`:** `fluxvm-agent node` puts these capabilities, with free CPU and memory and the macOS guest count, in its
  heartbeat (`apple`), and `fluxvm-agent central` scores nodes with `fluxvm_scheduler::apple_placement` for `backend: vz` requests. Nodes
  without Apple capabilities are excluded for `vz`; among nodes that fit, the tightest fit wins. **Verified** only on loopback: one real
  M4 node plus two fake nodes registered by hand. No second Mac was used; central's `vz` filtering is unit-tested (request parsing, nodes without Apple capabilities, feature requirements, the two-macOS-guest limit).

## Capability matrix

| Supported | Not supported |
| --- | --- |
| vCPUs, memory, raw disk, cloud-init, direct kernel boot (Linux guests) | tap / macvtap / netns / eBPF networking, UDP port forwards |
| NAT networking, TCP port forwards, no-network mode, serial console | NUMA, hugepages, cpuset, VFIO / GPU passthrough |
| shared folders (virtiofs), VM snapshots (memory + disk, Linux and macOS guests), pause / resume, graceful shutdown, force stop, EFI Secure Boot (macOS 27+, Linux) | TPM, confidential profiles; virtio-input devices, a GDB debug stub, a guest battery and macOS guest clipboard (no public API in the macOS 27 SDK) |
| guest agent over vsock (proxied like Firecracker; needs the agent in the image) | hotplug of CPU, memory and NICs; cdroms; firmware overrides |
| macOS guests (installed from an IPSW through the API or by hand, then cloned; see above) | live migration, in-place restore of a running VM, direct kernel boot of macOS guests |
| `apple.usernet` userspace gateway for Linux guests (egress/DNS policy, UDP/TCP forwards, netobs frames; DHCP, DNS, ICMP, IPv6 SLAAC and deny verdicts verified on hardware, TCP egress, TCP and UDP forwards and live netobs re-verified (hardware log)) | |
| network observability: `/v1/netobs/*`, `fluxctl netobs`, `/metrics` series, private-network switches bridged into the daemon (see [netobs.md](netobs.md); unit/integration tested with synthetic frames, verified with live guest traffic from a usernet guest on an M4: flows and DNS records, `scripts/vz-usernet-live-test.sh`) | packet payload retention without an explicit, filtered, expiring capture |
| `POST /v1/vms/{id}/vz/display` and `fluxctl vz display resize` (macOS 14+; **partial on hardware**: the connector's preferred mode changes, the active mode needs a KMS client in the guest) | |
| `GET /v1/vms/{id}/vz/attestation` and `fluxctl vz attestation verify`: signed boot manifest, host claim only (**not exercised on a booted VM**) | hardware attestation, TPM, measured boot |
| offline agent-token and SSH-key injection into a private raw root disk (token injection verified on a booted Debian 13 guest; SSH-key injection verified on a booted Debian 13 guest with `cloud_init.user: root`) | injection into a running VM's disk, qcow2, shared storage, or another user's `authorized_keys` |
| overlay disk exclusivity across VM records (create, attach, start) | protecting an overlay file from processes outside FluxVM |
| `apple.macos_vsock` for macOS guests: vsock device plus a LaunchDaemon agent (**experimental**: verified on one M4 with a macOS 27.0.1 guest for ping, exec and files; SSH remains the fallback) | exec policies in macOS guests (Landlock/seccomp are Linux-only) |
| extra disks (image, block device, NBD, blockd overlay (verified once on hardware); virtio, NVMe, USB), USB disk hot-attach and detach, private networks, console ports, bridged and vmnet networking, balloon, Rosetta, display, audio | (vmnet forwards may be UDP; NAT forwards are TCP only) |

The same table is encoded in `fluxvm_apple::CAPABILITIES`; unsupported requests are refused with a specific message before any
process starts.

## Hardware verification log

What has actually been run, and what has not. One Apple M4, macOS 27.2, 16 GiB, Debian 13 arm64 cloud image (kernel
6.12.111), VMs launched through `fluxctl serve`, 2026-10-11. Anything not listed here is still unverified on hardware.

| Item | Result |
| --- | --- |
| Offline token injection (`fluxvm-diskinject`) | **Verified.** The seed ISO carried no token, `/etc/fluxvm-guest-agent.token` was on the disk (0600, root), the guest booted in about 25 s with a clean `tune2fs` state and no ext4 errors across two boots, and with the agent installed it answered `ping`/`exec` over vsock with that token and refused a wrong one. The image had no SELinux; AppArmor was enabled and did not interfere. The SSH key stayed in cloud-init because `cloud_init.user` was not `root`, as designed. |
| `overlay` extra disk against VZ's NBD client | **Verified.** The runner logged `nbd-connected`; the guest saw a 256 MiB `virtio_blk` disk; a 32 MiB random write read back with the same sha256 (also after `drop_caches`); the disk's sha256 was identical across stop and start; the base file stayed byte-identical; `fluxvm-blockd branch` worked. |
| Frozen overlay as an attached disk | Found: served as writable, so guest writes got EIO. Fixed and **re-verified on hardware**: the guest sees `/sys/block/vdX/ro` = 1, mounts it read-only, reads the parent's data (hash matches), gets `Read-only file system` on writes and `EPERM` on raw writes, with no I/O error or journal abort; an overlay branched from it is writable and reads the parent's data. |
| blockd encryption and lazy HTTP base through VZ's NBD client | **Verified (once).** Three exports to one Debian 13 guest (release build, M4, macOS 27): plain, `encrypt` with a key file, and a lazy overlay on a Range-capable HTTP base with `sha256=`. `sha256sum` of each guest disk equalled the base's; a 900 KB `O_DIRECT`+`fsync` marker read back identical after `drop_caches` on all three; the encrypted overlay file contained none of the marker, the other two did; the base file was unchanged. The lazy export fetched 6 MiB before any guest read, the rest on demand, and a second full read made no HTTP requests. Not run: structured replies against VZ, rebase and resize under a running guest. |
| Fork of a running vz guest (`POST /v1/vms/{id}/fork`) | **Verified (once).** Debian 13 guest on `apple.usernet`, a perl loop sending `n=<counter> host mid boot t` over a `telemetry` vsock service every second. `count: 2` returned `201` in 2.2 s with two running children. Both children's first line was `n=7` at the same second, the parent's counter at the fork instant, and they then counted independently (parent n=28, children 27 and 26) while the parent never missed a beat (largest gap 2 s). Deleting both children left the MAC pool empty and the parent running. As expected, all three report the same hostname, machine-id and boot id: the guest had no agent, so no identity reset ran. A bare two-runner spike (restore of one saved state into two concurrent runners) took 1.1 s. Not run: fork with the guest agent (identity reset), fork of a paused VM, more than two children, children surviving a daemon restart. |
| `apple.usernet`: DHCP, ARP, ping gateway, DNS forwarding and `dns_deny`, ICMP deny, IPv6 SLAAC, netobs flows with `denied` verdicts | **Verified.** The lease was 10.0.2.15/24; denied DNS, ICMP and TCP-port flows appeared in `/v1/netobs/flows` with their reason. |
| `apple.usernet`: TCP, host-to-guest forwards | **Failed on first run, fixed, re-verified.** The gateway addressed TCP to `ff:ff:ff:ff:ff:ff` whenever the guest MAC was pinned (which the daemon always does), so Linux dropped every segment. After the fix: `curl` to HTTPS and HTTP sites returned 200, gateway frames are addressed to the guest MAC (zero broadcast-addressed TCP in a capture), a `127.0.0.1:18022 -> 22` forward gave the sshd banner and a full login, and IPv6 `curl -6` reached an IPv6-only host. |
| `apple.usernet`: TCP egress policy | **Verified.** `deny_cidrs` (1.1.1.1/32) refused immediately while 1.0.0.1 answered; `deny_ports` [25] refused to three public hosts while port 587 connected; `dns_deny` gave NXDOMAIN; every denial appeared as a `deny`/`dns_deny` event in `usernet.log` and as a `denied` flow (`drop_reason: policy`) in `/v1/netobs/flows`. |
| `apple.usernet`: TCP throughput | Download: 50 MB at about 12 MB/s with no retransmits, four parallel downloads about 12 MB/s total. **Upload was inefficient and is fixed:** 20 MB to a public server ran at 3 to 4 MB/s with about 2100 guest retransmitted segments (about 15% of the data) because the gateway advertised a fixed 256 KiB window while its queue to the host socket drained slower than the guest sent, so in-order segments were dropped unacknowledged and the rest of the burst was dropped as out of order. The window now follows the free room in that queue (zero below one MSS) and reopens when the host writer drains. After the fix, three 20 MB uploads ran at 8.4 to 10.0 MB/s with 0 retransmits, matching the host's own 7.9 to 10.0 MB/s to the same endpoint. IPv4 uploads and one flow at a time only; many concurrent flows were not measured. |
| `fluxvm-vz-switch` bridge into netobs (`apple.networks`, `--netobs-socket`) | **Verified (PASS).** Guest-to-guest flows between two VMs on a private network appeared in `/v1/netobs/flows`, and when the bridge socket was gone the switch kept forwarding and the daemon fell back to a switch without the flag. Two known defects are listed under "Limits" in [netobs.md](netobs.md): an undercount at multi-Gbit rates is not reported, and the server half of a flow can be oriented as outbound. The switch crate did not build with `netobs` by default, so a plain `cargo build` gave a switch that rejected the flag and the daemon always took the fallback; `netobs` is now a default feature. |
| Netobs accounting on a two-guest iperf3 (release daemon, `apple.networks`) | **Verified.** After a daemon restart the surviving switch exported nothing: `ensure_switch` returned early for a running switch, so the new daemon never bound the bridge socket (fixed). With it bound, one 15 s iperf3 stream (2.84 GB of payload) was recorded as 2 972 553 197 bytes in 1 963 784 packets, which is the payload plus 66 bytes of headers per segment, identical in both guests' views, oriented client 10.89.0.3 to server :5201. A debug-build daemon recorded only about 17% of a 3.8 Gbit/s stream and counted the rest as `queue_full` (7.08 GB sent, 4.5 M frames dropped, all counted). |
| Boot attestation end to end (`vz/attestation`, metadata vsock) | **Verified (PASS).** The guest read the signed manifest, the signature verified against the host public key, a tampered payload was rejected, and `vz-attestation.key` was mode 0600. Still a host claim, not hardware attestation. |
| Loopback TCP vsock (`vsock_tcp`) | **Verified (PASS).** The loopback listener reached the guest service; a non-loopback bind address was refused. |
| Runner accounting (`fluxvm_vm_runner_rss_bytes`, `fluxvm_vm_runner_cpu_seconds_total`) | **Failed on first run, fixed, re-verified.** The numbers were the runner process only: with 2 vCPUs pinned the runner showed 0.0% CPU and 73 MB RSS while `com.apple.Virtualization.VirtualMachine.xpc` (ppid 1, a separate pid) showed 200% and 710 MB. The XPC service is now attributed to its runner through the VM's open root disk and `efi.bin` and summed in; see "Host-side accounting". The responsible pid (`responsibility_get_pid_responsible_for_pid`) and the parent pid do not identify the runner, and `launchctl procinfo` needs root. |
| Display resize (`vz/display`) | **Partial.** The API call works and the connector's preferred mode in the guest changes; fbcon `virtual_size` and the active mode do not (a headless guest has no KMS client), so the result is not deterministic without one. The reply no longer claims the guest followed; see "Control preconditions, display resize". |
| Control preconditions (pause/resume/shutdown/stop, display, devices, snapshot) | **Verified (PASS) except** `pause` and `resume` on a stopped VM, which returned a raw `runner 'pause': connecting to .../<id>.ctl: No such file or directory (os error 2)`. Fixed: pause, resume, balloon, screenshot, input, USB and display now answer `409` `the VM is Stopped; start it first` before touching the runner (API tests; not re-run against hardware). |
| macOS guest vsock (`apple.macos_vsock`) | **Verified.** macOS 27.0.1 guest, device configured: a stock `perl` bound `AF_VSOCK` port 1024 and the host's `CONNECT 1024` answered `OK 1024` plus the guest's bytes (a port with no listener returned nothing). The LaunchDaemon agent installed with `install.sh` then answered from the host over the same proxy: `ping` -> `pong`, a wrong token refused, `exec` as root (`Darwin arm64`, `27.0.1`), `put-file`/`get-file` round trip, exec policy refused as Linux-only. Uninstall left nothing behind. **Not run:** PTY shell, `shutdown`, `reset-identity`, a guest reboot with the daemon, and `vz_guest` selecting the agent. |
| macOS guest agent: PTY shell, reset-identity, shutdown, reboot persistence, host selection | **Verified (once).** On the same macOS 27.0.1 guest: `open-shell` ran commands as root and honoured a resize frame (`stty size` 40 120); `reset-identity` changed `HostName`/`LocalHostName`/`ComputerName` (reverted afterwards); `shutdown` powered the guest off; after the power cycle the LaunchDaemon came up on its own and answered `ping`. A VM created from the template with `macos_vsock` and `agent.enabled` first fell back to SSH for 90 requests while the guest's daemon was not yet listening, then `exec` through the daemon's `/agent` route returned `root` with `fluxvm_vz_agent_calls_total` 0 to 1. Found and fixed: admission rejected `agent.enabled` for macOS guests, so the host could never select the agent. |
| Fork v2: strict identity, token rotation, parallel children, NAT to usernet | **Verified (once).** Debian 13 guest on `apple.usernet` with the new Linux agent (static aarch64 build, installed from a read-only virtiofs share). `count: 4` at default settings returned 201 in 3.6 s: parent held 392 ms, children restored 4 at a time (about 2.3 s each), identity reset 0.5 to 0.65 s each, all of `entropy, hostname, machine-id, ssh-host-keys, cloud-init, dhcp, network-state, agent-token` confirmed, tokens rotated. From inside the guests: 5 of 5 distinct hostnames, machine-ids, SSH host keys, `/dev/urandom` output and agent tokens, each guest's token equal to its record's, and each child refuses the parent's token (and vice versa). **A first run exposed a bug:** two children got the same machine-id and all three the same new host key, because every child resumed one CRNG state and crediting entropy does not reseed the kernel at once; adding `RNDRESEEDCRNG` fixed it. A guest without the agent: `unique` was refused (400, no child left behind), `best-effort` and `preserve` forked. `boot_id` stays the parent's. **NAT parent to usernet children (the gating spike): the child restores and runs but its NIC comes up disconnected (`Internal Network Error`), no address or route, and never recovers**, so a NAT guest cannot be forked. Not run: the SSH fallback against a reachable sshd, more than 4 children, daemon crash mid-fork. |
| Fork v2: scaling, hooks, readiness, crash recovery | **Verified (once).** Median of 3 per size, 768 MiB guests on `apple.usernet`, strict identity: N=1 1.6 s, N=2 1.8 s, N=4 2.7 s, N=8 4.7 s wall; the parent is held about 355 ms at every N; per-child restore 0.84 s (N=1) to 2.7 s (N=8, restores contend). `ready: exec` and `ready: {"tcp": 22}` passed (53 ms); `{"tcp": 9999}` with a 6 s budget failed the fork and left no child; hooks ran in every child (`touch`, `sh -c`) and a failing hook (`false`) rolled the batch back; typos, a zero port, a path with a space, `parallelism: 0` and `count: 33` were refused up front. **Crash test (`kill -9` of the daemon at 0.4 to 2.8 s into a 3-child fork, then restart): a first run found a leak**: records, snapshot dirs and intents were cleaned, but the children's runners kept running (3 orphaned guests per kill), because process lookup read `/proc`, which does not exist on macOS. After the fix every kill before completion ended with no children, no fork snapshot, no intent and the runner count back at 1; kills after completion left the 3 finished children. The adopt-whole path (all children at `identity-reset`, daemon killed before answering) is covered by unit tests only: the window is too short to hit by timing. One of the first forks after a daemon start failed once with a refused vsock proxy on a child (not reproduced in 8 further runs; the error now carries the runner's log tail). |
| Branches: create, start, survive source deletion and daemon restart, nest | **Verified (once).** Debian 13 guest on `apple.usernet` with the agent. `create` took 0.38 s (176 MB state) with the guest running. A marker written before `create` was in both started VMs and one written after was in neither. `start` of 2 VMs took 1.9 s (restore about 1.0 to 1.3 s each, identity reset 0.45 to 0.5 s via the agent), unique hostnames and machine-ids, label `fluxvm.dev/branch`. After deleting the source VM and restarting the daemon the branch was still listed and a further start took 1.4 s with the marker intact. A branch of a started VM recorded `parent_branch`; deleting the original branch left its children and the nested branch working. A duplicate name, an invalid name and an unknown field were refused (400/400/422), an unknown branch id on start 400. Not run: disk growth over time, many branches, a crash during `create`. |
| vmnet networks and the vmnet broker (`com.apple.vm.networking`) | **Diagnosed, not usable without Apple's profile.** A small program signed three ways: no entitlement, `vmnet_network_create` returns `VMNET_INVALID_ACCESS` (1002); with only `com.apple.security.virtualization` (the runner's) it **succeeds**; with `com.apple.vm.networking` the kernel kills it at launch (exit 137, AMFI: a restricted entitlement needs a provisioning profile). With the broker signed without the restricted entitlement it ran under launchd, but a guest on `apple.vmnet` (named through the broker, or unnamed in the runner itself, so sharing is not the problem) booted and then Virtualization reported `Internal Network Error` / `network-disconnected`: the interface cannot start without `com.apple.vm.networking`. A second named guest got `vmnetd XPC connection failed`. The runner now reports `vmNetworkingEntitlement` and admission refuses `apple.vmnet` and `apple.bridge_interface` up front, naming `apple.usernet` and `apple.networks`. Not fixable here: SIP and AMFI stay on, and the profile comes from Apple. |
| Atomic fork quota | **Verified.** Daemon with `[policy] max_memory_mib_host = 7000`, one 2 GiB usernet guest. A plan for 3 children was refused with the host-limit reason. Two simultaneous `POST /fork` with `count: 2` (each fits alone, both would need 10 240 MiB): one returned 201 with 2 children, the other 400 `host would exceed policy.max_memory_mib_host (7000): 8192 already used + 2048 requested`; the host ended with 3 VMs (6 144 MiB), the reservation was released (`quotas.json` showed no reservations), and a `quota.deny` event with `scope=fork` was recorded. |
| Live egress policy (`PUT /v1/vms/{id}/usernet/policy`) | **Verified.** A usernet guest created with `allow_cidrs: ["192.0.2.0/24"]` (a strict allow-list that excludes the internet) could not `curl https://example.com`; after `PUT {"allow_cidrs": []}` the same curl returned `HTTP/2 200` with no restart and the gateway log showed `reloaded the egress policy`. `PUT {"deny_ports": [443]}` blocked it again at once. Invalid CIDRs and an empty change were refused with 400 and nothing stored. After stop and start the stored policy (`deny_ports: [443]`) still applied. |
| Fork plan (`/v1/vms/{id}/fork/plan`) | **Verified, and one wrong first guess corrected.** A usernet guest planned for 3 children and then forked: the plan was `ok`; a NAT guest was refused with the real reason (`forkable`, "create the guest with apple.usernet"); `count: 99` was refused once. Memory: my first low bound used the 260 MiB a restored child holds right after the restore, but 12 s later the children held 700 to 910 MiB each (the parent held 735 to 794), so a 3-child fork added 2 733 MiB against a planned 768. The low bound now uses the parent's (or existing children's) resident size: planned 2 382 to 6 144 MiB, actual 2 733 MiB, inside the bound. Time: planned 3 085 ms from recorded forks, actual 3 142 ms. |
| Host history and fork timings in the event log | **Verified.** On an M4 daemon with 1 running guest and a 2-child fork: `GET /v1/host/history` returned 6 samples after about 30 s (`interval_seconds` 5, `capacity` 720) with the host pressure, available memory and per-VM CPU seconds/RSS; `GET /v1/events?event=vm.fork` returned `total_ms` 2477 and a `report` with each child's prepare (40 and 62 ms), restore (1677 and 1685 ms) and total (1758 and 1771 ms), matching the fork response. |
| Jumbo MTU on private networks (`apple.networks[].mtu`) | **Verified.** Two Debian guests on one network with `mtu: 9000`: the private card came up at MTU 9000 (the NAT card stayed 1500), an 8972-byte DF ping between them got 3/3 replies at 0.4 ms, and an 8973-byte DF ping was refused locally. `GET /v1/vznets` reported `mtu: 9000`. Before this change the switch dropped anything above 1600 bytes and the attachment stayed at 1500. |
| Jumbo MTU on `apple.usernet` and `apple.nat_mtu` | **Verified.** `usernet.mtu: 9000`: the guest card came up at MTU 9000 and an 8972-byte DF ping to the gateway got 3/3 replies. Control, the published v0.4.0 with the same request: the guest stayed at MTU 1500 and every jumbo ping failed. `usernet.mtu: 1400`: guest MTU 1400, a 1400-byte DF ping passed and 1401 was refused. `apple.nat_mtu: 1400` on the Mac's NAT: the guest card came up at 1400 through the seed's network-config and the internet (HTTP to example.com) worked. |
| `apple.usernet`: root login with an offline-injected SSH key, UDP forward, live netobs, pause precondition (`scripts/vz-usernet-live-test.sh`) | **Verified (2026-10-15, M4, macOS 27.2, Debian 13).** With `cloud_init.user: root` the daemon logged the key injected into the root disk and dropped the cloud-init copy, and root SSH worked through a `127.0.0.1:N -> 22` TCP forward. A guest `python3` UDP listener echoed a datagram sent to a UDP forward. The guest's own `curl` and DNS lookup showed up in `/v1/netobs/flows` and `/v1/netobs/dns` for that VM. A second `pause` on a paused VM returned `cannot pause: the VM is paused; pause needs a running VM`. |
| Named volumes on a VM sandbox (`scripts/vz-sandbox-volume-live-test.sh`) | **Verified (2026-10-15, same host).** A sandbox created with `volumes: [{name: data, guest_path: /mnt/data}]` showed `/mnt/data` as a mount point; a file written there was on the host under `volumes/_shared/data/`; it survived deleting the sandbox; a second sandbox with the same volume read it. `read_only` and several volumes were not run. |
| Full `vz` live suite from `main` on the Mac mini (`macos-live-test`, `vz-devices`, `vz-vsock-services`, `vz-usernet`, `vz-sandbox-volume`) | **All passed (2026-10-15).** 104 `ok` lines, no `FAIL`; no runner or temporary daemon left behind. |

Two things seen in the same runs that matter when you write guest-side tooling or debug a boot:

- The stock Debian 13 arm64 guest's `python3` dies with SIGILL on this M4, so guest-side helpers should use `socat`, `perl` or
  `curl` instead of Python.
- Guests with `apple.networks` plus cloud-init **took 2 minutes to finish booting** (seen as a stall: no console output, no
  address reported, SSH refused). Cause, confirmed on hardware: the seed's catch-all DHCP match (`name: e*`) also claimed the
  private card, which has no DHCP server, so `systemd-networkd-wait-online` waited for it until its 120 s timeout; the card's
  own `.network` file only arrived with `write_files` and was reloaded in `runcmd`, after `network-online.target`. Private
  cards now get a static MAC-matched entry in the seed's `network-config` (`CloudInitSpec.static_nics`), which sorts before
  the DHCP match: `wait-online` takes 1.4 s and the address is reported about 7 s after create (was ≥ 120 s).

## Honest limits

- This is a preview-quality backend. Linux ARM64 guests are what the live tests boot; macOS guests were booted by hand and cloned
  through `scripts/macos-guest-live-test.sh`.
- macOS guests are installed through the API or by hand, but finishing Setup Assistant still needs a person unless the host and guest
  are on macOS 27 (`provision_*`). The restore image is 26.6 GB and the installed disk about 24 GB, so plan for 55 GB or more free on
  the volume that holds them.
- Restoring a snapshot (warm starts, the warm sandbox pool, speculate) needs an unlocked login session; each path falls back to a cold boot
  where it can.
- Container warm-pool slots are hidden from `fluxctl ls` and `GET /v1/vms` unless you pass `--all` / `?all=true` or select
  `fluxvm.oci-warm`. A sandbox claimed from a warm slot is not hibernated until it restarts, because its root disk is a hot-attached
  USB disk.
- The Mac's DHCP server keeps one lease per MAC for a day, so a deleted VM's MAC is handed to the next new VM (and with it the old
  address), rather than filling the lease file.
- A sandbox that has a network card is on an unfiltered NAT; only offline and allow-listed sandboxes are isolated.
- Private networks (`apple.networks`) are IPv4 /24s with static addresses and an optional per-network MTU up to 9000: no DHCP, DNS or routing between networks. The switch runs
  in user space, so it is slower than the NAT card.
- Container sandboxes need boot artifacts built on Linux arm64 (`scripts/build-oci-boot.sh`, or the `oci-boot` CI workflow's
  artifacts) and take `linux/arm64` images (or `linux/amd64` under Rosetta).
- Several Linux-only crates still do not build on macOS; CI builds and tests the supported subset by package.
- Memory is not enforced by FluxVM here; the Mac's own memory pressure applies (runner RSS is reported, not capped). Plan for one or two small VMs on a 16 GB Mac.
