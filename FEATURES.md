# FluxVM Feature Reference

Exhaustive checklist. For the pitch, see [README.md](README.md); for who this is for, see [docs/POSITIONING.md](docs/POSITIONING.md); for the capability tour and metrics, see [docs/PRODUCT_OVERVIEW.md](docs/PRODUCT_OVERVIEW.md); for concrete scenarios, see [docs/use-cases.md](docs/use-cases.md).

## Project Statistics

- 23 crates (`ls crates/`)
- 4 VMM backends (QEMU/KVM, Cloud Hypervisor, Firecracker, in-tree FluxVM hypervisor)
- 4 storage backends (qcow2/raw default, LVM thin, NBD, Ceph RBD)
- 4 network modes (user-mode NAT, TAP+bridge, netns+DHCP, macvtap)
- 1 REST API (`fluxctl serve`), 1 CLI (`fluxvm`)
- Kubernetes: 2 CRD paths (`DisposableVm` via `fluxvm-kube`, MicroVM via `fluxvm-microvm`)

---

## VM Lifecycle

- Create, list, get, pause, resume, delete across all 4 backends
- Restart, rename, labels (`PATCH /v1/vms/{id}`), label selectors (`?label=env=prod,!tmp`) and bulk `start/stop/restart/delete -l`
- VM names or UUID prefixes accepted wherever fluxctl takes a VM id; `fluxctl wait --for running|stopped|agent`; `-o json|table|wide`; shell completions
- Clone a stopped VM with its data disks and labels (`fluxctl clone-vm`, `POST /v1/vms/{id}/clone`)
- QEMU data disks: qcow2 hot-add as `scsi-hd`, live `block_resize` of root or data disks, detach (`fluxctl disk`, `/v1/vms/{id}/disks`); a stopped `vz` VM's root disk can be grown offline
- QEMU serial console on a chardev socket teeing into `console.log`; websocket `GET /v1/vms/{id}/serial` + `fluxctl serial`
- Snapshots: save, list, delete, start-from; label-driven scheduled snapshots with retention (`fluxvm.io/snapshot-every`, `fluxvm.io/snapshot-keep`)
- VM lifecycle resilience: `run_strategy` (Always, RerunOnFailure, Manual, Halted) restarts a dead hypervisor process with exponential backoff and a crash-loop cap; QEMU `on_crash` (pvpanic and watchdog, restart/poweroff/pause/coredump-if-existing); cron `power_schedule` start/stop ([docs/run-strategy.md](docs/run-strategy.md))
- Backups to standalone qcow2 (root or `--all-disks`), live via one temporary internal snapshot
- VM templates (`/v1/vm-templates`, `fluxctl vm-template save|list|show|delete|create`)
- Instancetypes and preferences, KubeVirt-style (`/v1/instancetypes`, `/v1/preferences`, `fluxctl instancetype|preference ls|get|create|rm`): named, versioned size classes and device defaults, built-in `small`/`medium`/`large`/`memory-optimised`; `create --instancetype NAME[:VERSION] --preference NAME` (explicit spec wins over a preference, a clash with an instancetype is a 400, the resolved values are snapshotted with the VM); linted by `vm-spec check`. See [docs/instancetypes.md](docs/instancetypes.md)
- Backup chain verify and test-restore (`fluxctl backup-verify verify|test-restore`): re-read a CBT chain's manifests and layers (digests, sizes, parent links, completeness) without restoring, or restore to a scratch dir and `qemu-img check` it; scheduled verify jobs with PBS-style `ignore-verified`/`outdated-after`; `backup.verify`/`backup.test_restore` events and exit code 3 on a problem. See [docs/backup-verify.md](docs/backup-verify.md)
- Lifecycle events (`state_dir/events.jsonl`, `GET /v1/events`, SSE `/v1/events/stream`, `fluxctl events -f`) and quota usage (`GET /v1/quotas/me`)
- Remote fluxctl: `--server`/`FLUXVM_URL`, `--server-token`/`FLUXVM_TOKEN`, saved `fluxctl context`s; OpenAPI at `/v1/openapi.json`
- `"backend":"auto"` resolution (picks a backend based on spec/host capability)
- `ttl_seconds` — guaranteed cleanup of a VM even if the creating job crashes or disappears
- vsock guest agent: `exec`, PTY console, file transfer — no SSH or network path required (QEMU native AF_VSOCK; Cloud Hypervisor / Firecracker / in-tree FluxVM KVM via Firecracker-style UDS `CONNECT` proxy)
- qcow2 copy-on-write overlays for cheap disposable clones from a golden image
- Direct-kernel or firmware boot (Cloud Hypervisor, QEMU)
- UEFI Secure Boot (QEMU only) + emulated TPM 2.0 (QEMU and Cloud Hypervisor) — see [docs/secure-boot-tpm.md](docs/secure-boot-tpm.md)
- Phase 6 **security profiles** (`standard` / `measured` / `confidential-snp` / `confidential-tdx`): admission, software-test evidence, fleet capability heartbeats, and policy-gated secret release — see [docs/security-profiles.md](docs/security-profiles.md)

## Backends

- **QEMU/KVM** — broad guest/device compatibility, qcow2 CoW overlays, QMP socket, OVMF UEFI/Secure Boot + emulated vTPM
- **Cloud Hypervisor** — Rust VMM, direct-kernel or firmware boot, emulated vTPM (no Secure Boot — see [docs/secure-boot-tpm.md](docs/secure-boot-tpm.md))
- **Firecracker** — microVM backend, Linux kernel + raw root filesystem, includes the **Firecracker jailer** (chroot + uid/gid isolation, see [docs/operations.md](docs/operations.md#firecracker-jailer-chroot-uidgid-isolation-cgroups))
- **FluxVM hypervisor** (`backend: "flux-vm"`, binary `fluxvm-hypervisor`) — agent-sandbox track: memory snapshots, `/v1/sandboxes`, guest HTTP proxy + AutoResume, L7 egress, AutoPause, `/console`; in-tree KVM virtio-vsock host→guest CSM (guest-agent ping/exec); guest→host `uds_path_<port>` muxer still deferred
- **Apple Virtualization.framework** (`backend: "vz"`, macOS on Apple silicon) — Linux and macOS guests, snapshots, warm sandbox pool, offline and allow-listed sandboxes, direct kernel boot, **`apple.host_gpu`** host Metal/MLX OpenAI relay over vsock ([docs/host-inference.md](docs/host-inference.md)), and **container sandboxes**: an OCI image (`linux/arm64`) in its own lightweight VM with a read-only root and agent-only exec ([docs/macos.md](docs/macos.md), [docs/oci-sandboxes.md](docs/oci-sandboxes.md))
- **VM disks as OCI artifacts** — `fluxctl image push <file> <ref>` / `image pull-disk <ref> <out>` move a raw or qcow2 disk through any OCI registry (pure Rust, streamed, every digest verified) ([docs/oci-disk-images.md](docs/oci-disk-images.md))
- **OCI registry tools** (`fluxctl image inspect|tags|tag|rm|referrers|attach|index create`) — inspect manifests, list and retag, delete by digest, OCI 1.1 referrers with tag-schema fallback, attach SBOMs/attestations, build multi-arch indexes; client-side over the registry HTTP API ([docs/oci-registry-tools.md](docs/oci-registry-tools.md))
- **QEMU control sockets** — QMP under short `run_dir/qmp/<id>.sock` (macOS `sockaddr_un` / `SUN_LEN` safe); migration receivers under `run_dir/mr/<id>/` ([docs/operations.md](docs/operations.md#state-layout))
- **Agent sessions** — `fluxctl agent` / `/v1/agents/sessions`: policy, scoped tokens, jobs, approvals, budgets, fork/evaluate/promote, live event stream over SSE ([docs/agent-sessions-guide.md](docs/agent-sessions-guide.md))
- **Agent-sandbox tooling** — HTTP method/host/path egress ACL incl. opt-in HTTPS interception ([docs/http-acl.md](docs/http-acl.md)), guest file change-set API ([docs/sandbox-changes.md](docs/sandbox-changes.md)), stdlib-only Python and Go SDKs (`python/`, `go/`), and `fluxvm-procbox`, a rootless Landlock + seccomp process sandbox with profiles and `learn` for the lightweight tier ([docs/procbox.md](docs/procbox.md)), also selectable as a `/v1/sandboxes` kind with dry-run ([docs/procbox-backend.md](docs/procbox-backend.md))

## Networking

- QEMU user-mode NAT (SLIRP) — zero host config
- TAP + Linux bridge — VM on the host's L2
- Per-VM network namespace + dnsmasq DHCP (known `guest_ip`)
- Netns-sandbox `/28` IPAM pool utilization visibility (`GET /v1/network/ipam`,
  `fluxvm dataplane ipam-status`) — capacity/allocated/free/utilization and a
  `near_exhaustion` flag (≤5% free), ahead of a failed VM create discovering
  pool exhaustion the hard way
- macvtap — VM's own MAC directly on a parent link
- All 4 modes SSH-verified end to end in this project's own regression tests

### Network Fabric (GA, schema v4)

- TC/eBPF VM-edge dataplane, or Cilium-coexistence mode (never mutates Cilium's own maps)
- IPv4/IPv6 dual-stack L3+L4 policy
- Per-VM rate limits (Mbps/PPS)
- Security groups / CNP (Cloud Network Policy)
- Live policy reconfigure without VM restart
- REST observability: status, policy, stats, flows
- Native eBPF is the default dataplane (`mode = "ebpf"`, `required = true`); nftables only with explicit `mode = "legacy"` — see [docs/primary-ebpf.md](docs/primary-ebpf.md)
- Per-VM eBPF rule cap: 64 (kernel BPF verifier limit)
- Deny-by-default GA policy: `sudo ./scripts/enable-network-fabric-ga.sh --restart` (fail-closed GA) or `--lab` (soft default-allow); host readiness via `./scripts/network-fabric-preflight.sh` — see [docs/network-fabric.md](docs/network-fabric.md)
- **Kairon VM edge** (schema 12) — per-VM anti-spoof, learn-IP from ARP/ND, DNS and TLS SNI allow lists, egress token bucket and ingress `tbf`/police, attributed drops, live conntrack export/restore for migration, bounded tcpdump capture with pcap download; persisted across restarts. See [docs/vm-edge-contract.md](docs/vm-edge-contract.md)
- **`deny_udp`** on `VmNetworkPolicy` — Keep / strict cells: TC drops UDP+SCTP (DHCP still allowed); drop reason `udp-deny` (code 12). Used with gateway-only `allow_ports` for proxy-or-die. Keep product docs: Fabric `docs/keep/confine.md`.

### Service Fabric

- Node-local Maglev VIP load balancing
- Dual-stack NAT/DSR/SNAT
- Health-aware routing
- Incremental reconcile
- Per-service EDT (Earliest Departure Time pacing)
- Flow export

## AI agents

- **MCP server** — `fluxctl mcp serve` exposes VM, host, network and log tools over the Model Context Protocol (stdio) for Hermes Agent and other MCP clients; power and capture only with `--allow-write`. See [docs/mcp.md](docs/mcp.md)

## Images

- `build-image` — virt-builder-style pipeline, guestkit-based (never libguestfs)
- Per-distro package install (Debian/Ubuntu, RHEL-family, Arch, Windows via GuestKit agent)
- SHA-256 verification on every source image
- Ed25519-signed image catalog with REST CRUD (`allowed_image_dirs`, `trusted_signers`), also reachable
  entirely offline via `fluxvm catalog list|add|remove|rename|clone|export|lock|unlock|clean`
- Windows/Kryton golden-image customization with live QGA
- Guest operations over the QEMU guest agent: OS/host/clock/users/filesystems/vCPU info, SSH authorized-key add/remove/list, password set (stdin/file only), clock resync; `fluxctl vm guest …`, `/v1/vms/{id}/guest/…` (`docs/guest-ops.md`)

## Operations

- **cgroup v2 resource control** — CPU/memory/IO limits, freeze/thaw, PSI (pressure stall information)
- **Warm VM pools** — pre-started VMs for lower cold-start latency, resizable after creation
  (`POST /v1/pools/{name}/resize`, `fluxctl pool resize`) without deleting and recreating the pool.
  `GET /v1/pools`/`/v1/pools/{name}` and `fluxctl pool list`/`get` report computed `ready` (members
  paused and claimable right now) and `pending` (how many more are needed to reach target `size`)
  counts alongside a lifetime `claimed_total`, so "fully backfilled" vs "still catching up" vs
  "never actually claimed from" don't require inferring anything from `members.len()` vs `size`
  yourself
- **Firecracker jailer** — chroot + uid/gid drop per VM; optional `enforce` for production fail-closed ([docs/capability-figures.md](docs/capability-figures.md))
- **Firecracker virtio rate limiters** — `net_mbit_limit` / `net_pps_limit` / `blk_mbit_limit` / `blk_ops_limit` on create (plain Firecracker backend + FluxVm/`fluxvm-hypervisor` boot config)
- **Oversubscription policy** — `default_cpu_quota_percent` / `memory_max_equals_guest` ([docs/oversubscription.md](docs/oversubscription.md))
- **Firecracker static `cpu_template`** — create field; FluxVm when `fluxvm_engine=firecracker`
- **VM snapshots** — QEMU, Cloud Hypervisor, Firecracker, and FluxVm on `POST /v1/vms/{id}/snapshot`
- **Admission policy limits** — caps on what a request is allowed to provision
- **Bearer-token auth/RBAC** — on the REST API
- **State layout** — durable per-VM state under a documented path structure, see [docs/operations.md — State layout](docs/operations.md#state-layout)

## Storage

- **Default**: qcow2 (CoW overlays), raw
- **LVM thin** — thin-provisioned snapshots
- **NBD** — network block device-exported disks
- **Ceph RBD** — verified against a real Rook Ceph cluster

## Kubernetes & Fleet

- k3s add-ons (`fluxctl kube addons`): generated `config.yaml` (add-on toggles, Cilium, node labels/taints/reservations, Secrets encryption and rotation, CIS profile and `cis-check`, Traefik HelmChartConfig, ServiceLB pools), `registries.yaml` mirrors, embedded registry and airgap preload, kubeconfig merge/rename, host preflight, virtiofs PV and Rosetta recipes; unit-tested, not run on real hardware. See [docs/apple-k3s-addons.md](docs/apple-k3s-addons.md)

### `DisposableVm` CRD (`fluxvm-kube`)

- Node-local operator, one instance per node, reconciles only CRs whose `spec.node` matches
- Verified end to end against a real k3s cluster — 9/9 checks passing (CRD acceptance; `fluxctl serve` reachability; operator running; CR reconciles to a real running VM with live PID; out-of-band VM delete triggers self-healing replacement; CR delete blocks on finalizer until the real VM is actually gone with no leaked QEMU process — see [`scripts/test-kube-operator.sh`](scripts/test-kube-operator.sh))
- Declarative, not one-shot — a TTL-expired or externally-deleted VM gets replaced automatically on next reconcile, `Deployment`-style semantics
- `spec.networkMode`: `none` / `user` / `tap` / `macvtap`
- Placement: explicit `spec.node`, or `fluxvm-kube --enable-placement` to pin to the least-loaded capable node

### MicroVM (`fluxvm-microvm`)

- Scheduler-native alternative — kube-scheduler places a shadow Pod (capacity ticket only), VMM still runs on the host under `fluxctl serve`
- `MicroVMJob`, `MicroVMPool` CRs for job/pool patterns
- `MicroVMJob.spec.ttlSecondsAfterFinished` — Kubernetes-`Job`-style auto-cleanup: the Job controller deletes a finished `MicroVMJob` (and, via ownerReferences, its child `MicroVM`s) once it has sat in `Succeeded`/`Failed` for that long, instead of it (and every completed CI-style child VM) needing to be deleted by hand
- Not KubeVirt — no CDI or `virtctl`; QEMU has `fluxctl migrate` + migration receivers (not KubeVirt migration parity). See [docs/microvm.md](docs/microvm.md#vs-disposablevm-and-kubevirt) for the full comparison table against `DisposableVm` and KubeVirt

### Apple-native k3s (`fluxctl kube apple`)
- Persistent ARM64 Linux guest per node on Apple Virtualization: `create`, `join` (also to a cluster on another Mac with `--url --token-file`), `status`, `stop`/`start`, `upgrade`, `backup`/`restore`, `recover`, `delete` ([docs/apple-k3s.md](docs/apple-k3s.md))
- k3s binary verified by SHA-256 on the Mac and again in the guest; SSH host key pinned before first boot; join token travels on stdin only and is redacted from errors
- Journal written before each VM exists, ownership labels on every VM, `recover` adopts/relabels/reports orphans and never touches another owner's guests; failed joins and partial deletes clean up and stay retryable
- `upgrade` is server-first with a backup, skew checks, per-node Ready-at-version wait and rollback; `persist cycle` checks that data survives a stop/start
- Unit-tested with a fake host (ordering, cleanup, redaction); not yet run against real Apple hardware

### Kubernetes on Apple VZ (macOS LaunchAgents)
- `fluxctl kube agent install|uninstall|status` runs `kube supervise` (k3s in a Virtualization.framework VM) as a per-user LaunchAgent; `docker agent` does the same for the Docker engine VM
- Plist uses the Homebrew `opt/` path (survives `brew upgrade`), `ProcessType=Interactive`, and `--restart`/`--throttle-secs`; install is refused beside a supervisor that launchd does not own
- `kube supervise` holds a sleeping cluster asleep while the Mac is locked or logged out, then restores it once unlocked
- `fluxctl doctor` reports stale binary paths and unloaded agents across all `dev.zyvor.fluxvm*` LaunchAgents, including the vmnetd/usbd helpers
- VZ restores saved state only in an unlocked login session, so these are LaunchAgents, not LaunchDaemons ([deploy/launchd-notes.md](deploy/launchd-notes.md)); unit-tested, not yet verified on a live cluster

### macOS image store (`fluxctl mac`)
- `fluxctl mac images|pull|push|tag|rmi|import` keeps macOS guest templates in a local store (`~/.fluxvm/mac-images`, or `$FLUXVM_MAC_STORE`) and moves them to and from an OCI registry; names can share files, which are freed with the last name
- `mac run`, `ps`, `rm`, `exec` manage macOS VMs cloned from an image; `mac build` runs a Macfile (RUN over SSH, COPY over scp) in a throwaway instance and stores the result as a template, `--dry-run` prints the plan (SNAPSHOT and IPSW refused; the step logic is unit-tested with a fake host, live execution not verified); at most 2 macOS VMs, no fork, restore needs an unlocked session, and Apple's licence limits where images may be stored ([docs/macos-containers.md](docs/macos-containers.md)); unit-tested, not run against a real registry or VM
- `mac doctor` (preflight with exact fixes), `mac wait-ready NAME [--check CMD]`, `mac run --queue` (wait for a free macOS slot instead of a 409) and a versioned `--json` schema for `mac run|ps|inspect` (golden-tested); `scripts/test-mac-vz.sh` is a live acceptance script with a fake-`fluxctl` `--self-test`. Unit-tested only; the acceptance script has not been run on hardware ([docs/macos-containers.md](docs/macos-containers.md))

### Mac as a host (`fluxctl mac host`)
- `status` (power, sleep holders, session lock, auto-login, FileVault, displays, free disk, thermal; `--json`), `headless-check` (the unlocked-session requirement with a fix per finding; exit 1 on failure), `rosetta` (runner answer vs runtime file, guest steps)
- `keepawake install|uninstall|status` manages a `caffeinate` LaunchAgent, no sudo; `tune` prints recommended `pmset` settings and changes only what `--apply --only NAME` names, via `sudo pmset -a`; auto-login, screen lock and FileVault are never changed
- Parsers tested on captured command output, decisions and apply on a fake host; read-only commands run on one Mac, `keepawake install` and `tune --apply` not run ([docs/macos-host.md](docs/macos-host.md), research and backlog in [docs/apple-host-research.md](docs/apple-host-research.md))
### Mac integrations: MCP, events, metrics
- MCP tools (`fluxctl mcp serve`): `mac_images`, `mac_ps`, `mac_build_dry_run`, `mac_compose_ps`, `kube_apple_list|status|snapshot_ls|logs` (read-only, always offered) and `mac_run`, `mac_exec` (need `--allow-write`); no secrets in output
- `mac watch` / `mac events`: lifecycle events (`mac.instance.*`, `mac.slots.full|free`, `kube.apple.node.*`, `kube.apple.cluster.*`) into the daemon's event log, SSE stream and signed webhooks with `--record-to`; Kairon labels carried
- `mac metrics`: Prometheus text (instances by state, macOS slots n/2, template store bytes, `kube apple` node readiness, snapshot age), stdout or an atomic textfile ([docs/mac-integrations.md](docs/mac-integrations.md)); unit-tested with fakes, not run live

- Image trust and housekeeping: `mac keys|push --sign|sign|pull --verify|verify` (Ed25519 over the manifest digest, OCI referrer, local trust store; not cosign), a licence guard that refuses public registries unless overridden, a `mac build` step cache (`--no-cache`, `mac cache ls|prune`, size budget), chunk reuse on `mac pull` from other local tags (format unchanged), labels and `mac images --filter`, `mac search` over `/v2/_catalog`, `mac sbom` (bundle file list with sha256); unit-tested with fakes, not run against a real registry or macOS template ([docs/macos-containers.md](docs/macos-containers.md))

### Mac fleet over SSH (`fluxctl mac-fleet`)
- `up|add user@host...` and `apply inventory.yaml` install a pinned, checksum-verified FluxVM on each Mac, start the daemon and node agent LaunchAgents, enroll with a join token (via ssh stdin, never argv) and optionally prepare a k3s guest (`--k3s`); idempotent, parallel, per-Mac results with partial failure
- `status` (one table: version, daemon, VMs, macOS slots n/2, k3s, node), `exec HOST|--all -- cmd`, `upgrade` (refuses running VMs without `--force`), `remove --yes`, `doctor` (exact fix per finding), `--dry-run` everywhere
- System `ssh` with `BatchMode=yes` and pinned host keys (`--accept-new-hostkey` is explicit); remote commands are pure functions with golden tests and the transport is a trait with a fake ([docs/mac-fleet-quickstart.md](docs/mac-fleet-quickstart.md)); unit-tested, not run on real Macs

### Mac fleet scheduler and GitHub runners (`fluxctl mac-fleet run`, `fluxctl mac runner`)
- `mac-fleet run --template name:tag@sha256:HEX -- CMD` (and `mac run --fleet`) probes every Mac over ssh, scores them (tightest fit plus a bonus for already holding the exact template digest; labels, free disk, AC power, drain and Apple's 2 macOS guests per Mac are hard filters) and runs `fluxctl mac run --rm` on the winner; `--queue-timeout` waits for a free slot instead of failing; in-process 120 s leases stop two placements taking the same slot; "no Mac" always says why per Mac
- `mac-fleet drain|resume HOST` (state file `~/.config/fluxvm/mac-fleet-state.json`), `mac-fleet templates ls|distribute` (each Mac pulls, the pulled digest is compared with the pinned one, a mismatch is reported and that Mac is not used for that digest), `apply --prune` (removes FluxVM from Macs that left the inventory since the last `apply`; refuses a Mac with running VMs), `--step-timeout` on every fleet command and `--timeout`/`--probe-timeout`/`--pull-timeout` on the new ones
- `mac runner --repo OWNER/REPO|--org ORG --template REF [--fleet fleet.yaml]`: one ephemeral macOS VM per job with a just-in-time runner (token from a 0600 file or stdin, JIT config on ssh stdin only), deleted after the job, never more than 2 guests per Mac ([docs/mac-fleet-scheduler.md](docs/mac-fleet-scheduler.md), [docs/mac-runner.md](docs/mac-runner.md)); unit-tested with fakes, not run on real Macs or against GitHub

### `fluxvm-agent` (multi-host fleet, no Kubernetes)

- Central fleet registry + per-host heartbeat client
- Load-aware placement across hosts
- Per-node cordon/uncordon (`POST /fleet/nodes/{name}/cordon`/`/uncordon`) — take a node out of
  automatic placement for planned maintenance without deregistering it or touching any VM already
  running on it; an explicit `POST /fleet/vms {"node": "..."}` can still target a cordoned node
  directly, same precedent as a Kubernetes Pod's `spec.nodeName` bypassing the scheduler
- Per-node deregistration (`DELETE /fleet/nodes/{name}`) — permanently forget a decommissioned
  node instead of it sitting stale in the registry forever; refuses with `409` while the node's
  heartbeat is still fresh, so a live node's cordon state can't be silently wiped by its own next
  heartbeat re-registering it uncordoned
- Per-node VM listing (`GET /fleet/nodes/{name}/vms`) — "what's actually running on this node?",
  answered directly against that node (so it still works when other nodes in the fleet are
  unreachable) rather than filtered client-side out of the fleet-wide `GET /fleet/vms` aggregate
- Single-node lookup (`GET /fleet/nodes/{name}`) — one node's own `healthy`/`cordoned`/labels/
  free-capacity record, without fetching and filtering the whole `GET /fleet/nodes` list just to
  check one node's state; `404` for an unregistered name
- Fleet-wide `GET /fleet/vms` names any node it couldn't account for in an `"unreachable_nodes"`
  field (stale heartbeat, connection error, bad status, or unparseable body — each with its own
  reason) instead of silently dropping that node's VMs from the list
- Fleet-wide capacity aggregate (`GET /fleet/capacity`) — `nodes_total`/`nodes_healthy`/
  `nodes_cordoned`/`nodes_schedulable`, `vcpus_total`/`used`/`free`, `memory_mib_total`/`used`/`free`,
  computed entirely from each node's last heartbeat (no per-node proxy calls, so it stays cheap and
  never degrades when one node is briefly unreachable)
- Automatic (unaddressed) placement fails over to the next-best node when the first pick is
  unreachable (connection failure or an unparseable response) — an explicit rejection from a node
  that was actually reached is never retried elsewhere, and an explicit `"node"`/`--node` target
  stays a single, non-retried attempt either way
- Label/`nodeSelector`-aware placement — `fluxvm-agent node --label k=v` (repeatable) advertises a
  node's labels on every heartbeat, and an unaddressed create can require a subset of them via
  `"nodeSelector"` (exact-match key/value, same semantics as a Kubernetes Pod's `spec.nodeSelector`)
  without giving up failover the way hand-picking an exact `--node` would
- Fleet placement rules — `"antiAffinity"` (group, required/preferred) and `"topologySpread"` (node label, `maxSkew`) on `POST /fleet/vms`; opt-in fencing-safe automatic reschedule of `reschedulable` VMs after heartbeat loss (`FLUXVM_AUTO_RESCHEDULE_GRACE_SECS`, node `FLUXVM_NODE_SELF_FENCE_SECS`) — see [docs/fleet-affinity.md](docs/fleet-affinity.md)
- Outbound event webhooks (`[[event_sinks]]`): HMAC-SHA256 signed JSON POSTs of the event log, per-sink bounded queue, retries with backoff, dead-letter file, SSRF guard — see [docs/event-webhooks.md](docs/event-webhooks.md)
- `fluxctl fleet ...` CLI subcommand — full parity with every `/fleet/*` REST route (`nodes`, `node`,
  `cordon`, `uncordon`, `node-vms`, `vms`, `capacity`, `create` incl. `--node`/`--node-selector`,
  `delete`, `deregister`), previously reachable only via raw `curl`
- Verified across two real, physically separate hosts

## Secure Containers (GA)

- Containerd runtime-v2 shim `containerd-shim-fluxvm-v2`, RuntimeClass handler `fluxvm`
- Maps a Pod/task group onto one QEMU, Cloud Hypervisor, or Firecracker FluxVM (`FLUXVM_CONTAINER_BACKEND`; Firecracker uses ext4 block staging for Pod shares — no virtio-fs)
- CNI L2 — guest gets the real Pod IP when a CRI netns exists (Cilium / Calico / Flannel / generic)
- Multus `netN` hybrid: direct primary + bridge or per-secondary direct (`FLUXVM_CONTAINER_CNI_MULTUS_DATAPATH`)
- Allowlisted hostPath broker (create-time colon list + optional hotplug via `FLUXVM_HOSTPATH_BROKER`)
- Guest cgroup-v2 stats and resource updates
- Containerd task events, guest OCI process hardening
- Pod-UID write-through volumes, guest RO/masked paths/devices/sysctls/libseccomp
- VSOCK stdio streaming, real guest PTY / `ResizePty`
- Guest AppArmor/SELinux/seccomp enforcement
- **Known gaps vs full Kata**: live e2e matrix under load; seccomp remote policy RPC (out of scope); Firecracker has no live virtiofs write-through — see [docs/secure-containers.md](docs/secure-containers.md) and [docs/PRODUCTION.md](docs/PRODUCTION.md)

## Sentinel Observability

- eBPF-based host + guest runtime intelligence
- Per-VM syscall/page-fault telemetry
- Drop-reason tracking
- Flight recorder
- BPF-LSM VMM guard/QoS
- XDP shield
- Topology steering
- Implemented in the `fluxvm-intelligence` crate — see [docs/runtime-intelligence.md](docs/runtime-intelligence.md), [docs/flight-recorder.md](docs/flight-recorder.md)

## Security Posture (what's implemented vs. what's still open)

This mirrors the [maturity caveat](docs/proof-and-status.md#maturity-whats-real-today) on the proof-and-status page rather than contradicting it — read both together.

| Implemented today | Still open before untrusted multi-tenant use |
|---|---|
| Firecracker jailer (chroot + uid/gid isolation; `enforce` fail-closed) + virtio rate limiters | seccomp/AppArmor/SELinux policy beyond what Secure Containers guests get |
| cgroup v2 resource control, per-request `[policy]` caps, and per-tenant aggregate quotas (`[[policy.tenants]]`) | — |
| Per-VM network namespaces | — |
| Bearer-token auth/RBAC on the REST API | — |
| REST API request rate limiting, opt-in (`auth.rate_limit_rps`/`.rate_limit_burst`), keyed per authenticated caller | — |
| Audit logging (`tracing` target `fluxvm_audit`, every authenticated request) | — |
| SHA-256 verification + Ed25519-signed image catalog, now covering `distro`/`version`/`arch`/a tamper-evident `signed_at` timestamp/named signer identity (`signed_by`)/build-lineage recording (`build_pipeline`, `build_run_id`, `build_commit` -- which CI pipeline, run, and source commit produced this image, `fluxvm catalog sign --build-pipeline/--build-run-id/--build-commit`), not just `name`/`source`/`sha256`/`format` | Independent attestation of that claimed build lineage (e.g. Sigstore/in-toto) -- `build_pipeline`/`build_run_id`/`build_commit` are asserted by whoever ran `fluxvm catalog sign` and tamper-evident once signed (relabeling any of them post-signing invalidates the signature, same as every other covered field), but there is no cryptographic chain proving the named CI system actually produced these bytes |
| `network.mode: "none"` for zero network path out of a guest | — |

## Build and Development

- Cargo workspace, 23 crates, current stable Rust toolchain
- `scripts/bootstrap-host.sh` — one-time host prep (packages, Cloud Hypervisor, Firecracker, a bridge)
- `scripts/preflight.sh` — confirm every required tool is on `PATH`
- `scripts/test-networking.sh`, `scripts/test-lifecycle.sh` — end-to-end verification scripts
- `scripts/test-kube-operator.sh` — the 9-check k3s CRD/operator verification
- GitHub Actions CI + a separate DevOps-gates workflow
- Depends on sibling [`guestkit`](https://github.com/zyvorai/guestkit) checkout for `fluxvm-image` (offline image customization, no libguestfs)

## Deployment

- Single binary + config file (`config.example.toml` → `/etc/fluxvm.toml`)
- Remote host deploy — see [docs/operations.md — Deploy to a remote host](docs/operations.md#deploy-to-a-remote-host)
- Kubernetes manifests under [`deploy/k8s/`](deploy/k8s/) (Dockerfile + CRD/RBAC/DaemonSet)
- Containerd RuntimeClass deploy fragments under [`deploy/containerd/`](deploy/containerd/) (Secure Containers path)

## License

Zyvor Production License v1.0 (SPDX `LicenseRef-Zyvor-Production-1.0`): non-production use is free, production use requires a commercial license (sales@zyvor.dev). See [LICENSE](LICENSE) and [NOTICE](NOTICE).
