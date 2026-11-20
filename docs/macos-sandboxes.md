# Agent sandboxes on a Mac

The sandbox API (`POST /v1/sandboxes`, `/process`, `/fs/read`, `/fs/write`, and the MCP tools `sandbox_create`, `sandbox_exec`,
`sandbox_read_file`, `sandbox_write_file`) works on the `vz` backend. An agent gets a disposable Linux VM, runs commands and moves files
in and out, and the VM disappears when its TTL runs out or it is deleted.

```bash
fluxctl serve &                                     # or: brew services start fluxvm
curl -s -X POST localhost:7788/v1/sandboxes -d '{"ttl_seconds": 600}' -H 'Content-Type: application/json'
# -> {"id": "…", "backend": "vz", "status": "running", …}   (returns once commands can run, about 8 s)
curl -s -X POST localhost:7788/v1/sandboxes/$ID/process -d '{"command": "uname -a; exit 3"}' -H 'Content-Type: application/json'
# -> {"result":"exec","exit_code":3,"stdout":"Linux … aarch64 …\n","stderr":""}
```

With no `template` and no `spec`, a Mac creates a 2-vCPU, 2 GiB Debian 13 VM (`image: "debian-13"`). Pass a `spec` (with
`"backend": "auto"`, which resolves to `vz` on a Mac) to choose the image and size. `fluxctl mcp serve --allow-write` offers the same
operations to an agent over MCP.

## Fast starts: the warm pool

A cold sandbox takes about 8 s. The daemon keeps `sandbox.warm_slots` (default 2 on a Mac, 0 elsewhere) warm VMs ready: stopped VMs
with a `warm` snapshot, labelled `fluxvm.pool=sandbox`. A default-shaped create (no `template`, no `spec`, no volumes, no resource
overrides, not tenant-scoped) restores one of them, which takes about 2 s, and that VM simply becomes the sandbox (it keeps its
`warm` snapshot; deleting the sandbox deletes it). The first create cold-boots and starts filling the pool in the background; every
create that claims a slot starts a refill. If no slot is free, or a restore fails (for example on a locked screen), the create
cold-boots as before. When the cause is the login session, the daemon log line says so, and `GET /v1/sandboxes/density` reports
`login_session` (`locked`, `no_session`, `unlocked`) and `restore_blocked` (the reason and the fix) while restores would fail, so
a pool that never hits can be told apart from one that is blocked by a locked screen.

Why a pool and not one snapshot restored many times: a saved VM state is tied to the MAC address and machine identifier it was saved
with (restoring with another MAC or identifier fails with "invalid argument"), and two VMs restored from one snapshot share a MAC and
so an address on the Mac's NAT; the Mac then reaches one or the other at random. Each slot therefore has its own MAC and snapshot, so
the number of sandboxes that can start fast at once is `warm_slots`. Slots are built one after another and the NAT frees a stopped
guest's address, so two slots can end up with the same one. Each slot records its address in the `fluxvm.slot-ip` label, and a slot
whose address a running VM already holds is skipped: that create cold-boots instead of sharing the address.

```toml
[sandbox]
warm_slots = 4   # 0 turns the pool off
```

Slots show up in `fluxctl list` as `sandbox-slot-…`. They cost disk (an APFS clone of the image plus a few hundred MB of saved memory
each), and no memory while stopped. Restoring needs an unlocked login session.

To run a container image (`alpine:3.22`, a distroless image) as the sandbox, one lightweight VM per container with no SSH and a
read-only root, pass `"oci": {"image": …}` or use `fluxctl sandbox run IMAGE -- CMD`; see [oci-sandboxes.md](oci-sandboxes.md).

For many sandboxes on one Mac (sizes such as `"profile": "tiny"`, admission on macOS memory pressure, idle pause and hibernate,
`POST /v1/sandboxes/warm` and `GET /v1/sandboxes/density`), see [agent-density.md](agent-density.md).

## How it differs from Linux

- **SSH unless the image has the agent.** Linux sandboxes talk to a guest agent over vsock. Stock cloud images (`debian-13`) have no
  agent, so the daemon uses SSH with its own key (`<state_dir>/sandbox_ed25519`, created on first use and authorised by cloud-init as
  user `sandbox`). A file is moved with `cat`/`chmod`, so the limits are 32 MiB per file and a normal Linux userland in the guest.
  Sandboxes on the `agent-micro` image use the agent over vsock and fall back to SSH; see [vsock-proxy.md](vsock-proxy.md).
- **Per-exec policy only with the agent.** `policy` (Landlock/seccomp confinement of one command) needs the agent and is refused on
  guests without it.
- **Network is not isolated by default.** A sandbox on `network.mode = "user"` has full outbound access through the Mac's NAT, and
  Virtualization.framework gives no way to filter it.
- **Offline sandboxes are.** `{"offline": true}` (MCP: `sandbox_create` with `offline`) attaches no network card at all, so there is
  nothing for the guest to route through; it has only `lo`. Commands and files still work: the daemon reaches the guest's sshd over
  **vsock** (stock Debian 13 images have `sshd-vsock.socket` on port 22) through the runner, using a built-in relay
  (`fluxctl vsock-proxy`, ssh's ProxyCommand). They cold-boot (about 6 s, no warm pool). For sandboxes that need *some* network, see below.
- **Host inference without a NIC.** `{"host_gpu": true}` (or `fluxctl agent run --host-gpu`) relays guest
  `http://127.0.0.1:8080/v1` over vsock to the host Metal/MLX (or llama.cpp) server and sets `OPENAI_BASE_URL`.
  See [host-inference.md](host-inference.md).
- **Allow-listed egress.** `{"allow_hosts": ["example.com", "*.pypi.org"]}` (MCP: `sandbox_create` with `allow_hosts`) implies `offline`:
  the guest has no card, and its only way out is an HTTP(S) proxy the runner serves over vsock (guest to host, port 3128). A small
  forwarder in the guest (`fluxvm-egress.service`, written by cloud-init) exposes it on `127.0.0.1:3128`, and `/etc/environment` plus an
  apt config point shells, curl and apt at it. The host decides: names are matched in the runner (exact, or `*.suffix`), only ports 80
  and 443, and a name that resolves to a loopback, private, link-local or CGNAT address is refused so an allowed name cannot be aimed at
  the Mac or the LAN. Refusals are `403` naming the host. A program that ignores the proxy settings simply has no route.
  Limits: HTTP and HTTPS only (no raw TCP, no UDP; DNS is done by the host), tools must honour `http_proxy`, the allow-list is fixed at
  creation, and these sandboxes cold-boot. HTTPS is tunnelled (CONNECT), not inspected.
- **The pool follows the base image.** Each warm slot is labelled with the build of `debian-13` it was made from
  (`fluxvm.image-id`). When a newer build is downloaded (the vendor's list is checked at most once a day), slots from the old one are
  never restored: the next sandbox cold-boots, the old slots are deleted, and the pool is rebuilt from the new image. `fluxctl run`
  warm templates are not covered yet; delete a `warm-*` VM to rebuild it.
- Sandboxes of any other shape (a `spec`, a `template`, volumes, a different size) always cold-boot.
- **Speculate and changesets work.** `POST /v1/sandboxes/{id}/speculate {"command", "paths": [...]}` snapshots the running sandbox, runs
  the command, records which files under `paths` were added, modified or deleted (with their contents), then puts the VM back by
  relaunching it from the snapshot. The result is a pending changeset; `approve` then `apply` writes it into the sandbox, refusing
  with a conflict if those files changed meanwhile. Measured: about 5 s for a small command. `paths` is required (the guest root is too
  large to scan). Limits: it needs an **unlocked login session** (it uses snapshot restore), the VM restarts its processes' memory
  state back to the snapshot (anything the command did in memory is discarded too), and only file changes are captured: network
  calls the command made really happened (the changeset's `side_effects` says whether the sandbox could reach out).
- **Volumes** (`volumes: [{name, guest_path, read_only?}]`) are named host directories passed as virtiofs shares and mounted through `/etc/fstab` by cloud-init, so the image needs cloud-init. A sandbox with volumes always cold-boots. Container sandboxes also take `volumes`; see [oci-sandboxes.md](oci-sandboxes.md). Not yet on `vz` for VM sandboxes: GPUs. Verified on an M4 with `scripts/vz-sandbox-volume-live-test.sh`: RW persistence across delete, two volumes at once, and a `read_only` volume the guest can read but not write.

## Volumes: files that outlive a sandbox

A sandbox is deleted when its TTL runs out, so anything it wrote is gone. A **named volume** is a directory on the Mac that a
sandbox mounts and that survives it:

```bash
curl -s -X POST localhost:7788/v1/sandboxes -H 'Content-Type: application/json' \
  -d '{"ttl_seconds": 600, "volumes": [{"name": "cache", "guest_path": "/mnt/cache"}]}'
```

- **Where it lives.** `<volumes_dir>/<tenant>/<name>` on the Mac (`[sandbox] volumes_dir`, default `<state_dir>/volumes`). A
  caller without a tenant uses `_shared`. The directory is created on first use and kept after the sandbox is deleted; the next
  sandbox that names it sees the same files.
- **How it reaches the guest.** As a virtiofs share, mounted at `guest_path` through `/etc/fstab` by cloud-init, so the image
  needs cloud-init (the default `debian-13` has it). A sandbox with volumes always cold-boots: it never takes a warm slot.
- **Rules.** Up to 4 volumes. `name` is 1 to 63 characters from `[a-z0-9._-]`. `guest_path` is absolute, at most 128
  characters from `[A-Za-z0-9/_.-]`, and starts under `/home`, `/mnt`, `/data`, `/srv`, `/opt`, `/workspace` or `/root`.
  `read_only: true` is enforced by the host.
- **One owner at a time.** A volume already attached to another VM, or to a container sandbox, is refused.
- **Not on `vz`:** GPUs. Container sandboxes take volumes too, with a looser `guest_path` rule ([oci-sandboxes.md](oci-sandboxes.md#volumes)).

## Verified

On an Apple M4 (macOS 27.2): create with no body, exec with exit code and output, file write and read with awkward paths and a mode,
a missing file reported cleanly, an offline sandbox with only `lo` (DNS and routes fail) still running commands and file transfers, the TTL removing the sandbox, no runner left behind, and the same through the MCP server over stdio.
With the pool: 2 warm slots built in about 15 s after the first create; two concurrent creates took 1.7 s and 3.6 s (7.6 s cold), got
different addresses and did not see each other's files; a create after the pool was consumed and refilled took 1.9 s.
`scripts/macos-live-test.sh` covers the REST path.
Volumes: `scripts/vz-sandbox-volume-live-test.sh` created a sandbox with a named volume, checked the guest saw the mount and that
its write landed in the host volume directory, deleted the sandbox, and read the file back from a second sandbox on the same
volume (2026-10-15, M4, macOS 27.2).

## Fork an offline sandbox: try four fixes in parallel

A sandbox with no network (`"offline": true`) can be forked into running copies that share its memory snapshot, which
is how an agent tries several approaches from one saved state and keeps the one that works. On an M4 mini it takes
about 4 s to fork into four, and the four tests of the demo ran in 0.3 s. The runnable walk-through, with its timings
and limits (a default NAT sandbox cannot be forked; the copies keep the parent's identity), is
[examples/agent-sandbox-demo](https://github.com/zyvorai/zyvor-fluxvm/tree/main/examples/agent-sandbox-demo).
