# Changelog

## Unreleased

- **`fluxctl kube supervise` publishes NodePort and LoadBalancer services to 127.0.0.1 by itself.** Create a service, `curl 127.0.0.1:<nodePort>`
  works; delete it and the forward goes. Loopback only; ports below 1024, the API port and ports already in use on the Mac are skipped.
  `fluxctl kube services` lists them (JSON). `--no-auto-publish` turns it off. Proved on a real k3s: nginx NodePort answered 200 from the Mac
  with no flags and refused after the service was deleted.
- **`fluxctl kube checkpoint`, `restore`, `checkpoints`, `checkpoint-rm`: save a running cluster and put it back.** A checkpoint is a snapshot of
  the server VM with its memory, so `restore` brings back every object and running pod exactly as they were, then waits for the node and
  re-opens the API forward. On a real one-node cluster saving took 1.0 s and restoring 12.8 s. Single-node clusters only: a cluster with
  workers is refused with the reason. The daemon's snapshot calls are reused; `VmApi` gains `list_snapshots` and `delete_snapshot`.
- **Studio: start, stop and restart a Docker container.** The Docker page now has a button per container for what you do most, with a spinner
  while the engine works and a toast when it answers ("web restarted", or "web was already stopped" when there was nothing to do).
  `POST /v1/docker/containers/{id}/{start|stop|restart}` (admin) is the route; any other action or a malformed id is refused before it
  reaches the engine. Create, remove and pull stay with the docker CLI.
- **Studio: scale, restart, cordon and delete from the Kubernetes page.** Deployments and StatefulSets get **+** and **−** (0 to 100
  replicas), every workload gets **Restart** (a rollout restart), pods get **Delete** (with a confirmation) and nodes get **Cordon** or
  **Uncordon**, each with a spinner and a toast. Four new admin-only routes under `/v1/kube/{cluster}/`; the page's old "read-only" wording is gone.
- **`fluxctl docker` and `fluxctl kube`: a stalled first boot says why.** A VM that never reports an address now says its first boot may be
  stuck on cloud-init installing packages (no internet, or a slow link), points at `fluxctl serial NAME`, and says the command can be run again to
  pick the VM up. When the guest answers ssh but Docker or Kubernetes is not ready in time, the error now carries what cloud-init says
  in the guest (`cloud-init status` and the end of its output log, read with `sudo`), so the cause is in the message instead of in a console.
- **Fleet: a VM's screen in the browser, through central.** The Screen tab of a VM that is not a macOS guest now opens its graphical
  console (noVNC, QEMU VMs) from the fleet console, so a VM's screen is reachable the way its SSH port already is, with no node or VNC
  port exposed. `POST /fleet/vms/{node}/{id}/ws-ticket` (the usual authentication; owned VMs, keys that may make changes, members
  and up) returns a 30-second single-use ticket and `GET /fleet/ws/{ticket}` is the WebSocket it opens; central asks the node for a ticket of
  its own with the fleet token and relays the node's VNC socket. It is a tunnel for every limit that matters: `tunnels.enabled`,
  `max_per_tenant`, idle timeout, maximum lifetime, revoke checks, the **Tunnels** page and the activity log. The ticket, not a cookie
  or a token in a URL, is the credential for the socket. `VncScreen` (and `wsUrl`) moved into `@fluxvm/ui-core` so Studio and the fleet
  console share one component; the node's Display tab behaves as before.
- **Studio: a Docker page.** The engine `fluxctl docker up` runs, in Studio: its version, the containers (state, Docker's own status text,
  published ports, age; filter and a running or not-running switch), the images (tags, size, containers using them) and a container's log in
  the same colourful window as a pod's (filter, tail length, follow, wrap, copy, download). Read-only and admin only. With no engine VM, a
  stopped one or a socket that does not answer, the page says what to run. Daemon: `GET /v1/docker` and
  `GET /v1/docker/containers/{id}/logs?tail=N`, talking to `/tmp/fluxvm-run-<uid>/docker-engine.sock` (checked against a real Docker 26.1 engine:
  list, images and framed logs). The log window is the pod's, generalised (`LogsPane`).
- **Studio: CPU and memory, Workloads and Events for Kubernetes clusters.** The Kubernetes page shows each node's CPU and memory use
  as a bar and each pod's CPU and memory (from the cluster's metrics-server; clusters without one just have no numbers), a *Workloads* tab
  (Deployments, StatefulSets, DaemonSets with ready of wanted and images; Services with type, cluster IP, ports and NodePorts) and an
  *Events* tab (newest first, warnings-only switch, namespace and text filters). Daemon: `GET /v1/kube/{name}` gains `metrics` and
  per-row usage, and `GET /v1/kube/{name}/workloads` and `GET /v1/kube/{name}/events` are new, read-only like the rest.
- **Studio: a VNC console for QEMU machines.** The Display tab of a running QEMU machine opens its graphical console in the browser
  ([noVNC](https://github.com/novnc/noVNC) 1.7.0, MPL-2.0, attributed in `NOTICE`, loaded on first use): keys and clicks, Ctrl+Alt+Del,
  full screen, view-only, fit to window and text sent to the machine's clipboard. The daemon side is `POST /v1/vms/{id}/ws-ticket` with
  `{"kind":"vnc"}` (admin, single use, refused with the reason when the machine is not a running QEMU machine with a VNC socket) and the
  existing `GET /console/ws/{ticket}`, which bridges the WebSocket to the VM's `vnc.sock` (raw RFB in binary frames). No port is opened.
- **Studio: Connect and Display tabs on a machine.** *Connect* lists the ways in as commands to copy: `fluxctl ssh ID` (with `-l` for the
  cloud-init user), plain `ssh user@address`, `ssh -p PORT` for each published port 22, and `fluxctl fleet ssh NAME` for access through
  your fleet's central, with what each needs and the machine's published ports (web ports get an Open link); it opens nothing, and
  quotes every value it puts in a command. *Display* (for `vz` guests) shows the guest's screen and, when you take control, sends
  clicks, scrolling and keys, through the node's existing `screenshot` and `input` routes; it is the display component the fleet console
  already had, now shared (`VmDisplay` in `@fluxvm/ui-core`).

## 0.4.15 (2026-10-24)

**Docker and Kubernetes on a Mac, and a Kubernetes page in Studio.** `fluxctl docker` and `fluxctl kube` each run in managed Debian VMs
on Apple silicon, and Studio can show, read and open what runs in the cluster. Existing VM, sandbox and Linux-backend behaviour is
unchanged; the daemon gains new API routes and three direct TLS dependencies (`rustls`, `tokio-rustls`, `rustls-pemfile`, already in the
lock through `reqwest`).

- **`fluxctl docker`: a real Docker engine in a FluxVM VM.** The built-in Docker socket maps containers to microVMs and cannot
  `build`, run Compose, bind-mount Mac paths or create networks. `fluxctl docker up [--share HOST[:GUEST][:ro]]` creates (or
  starts) a Debian VM running `dockerd` and publishes its socket at `{run_dir}/docker-engine.sock`; `env`, `status`, `down [--purge]`
  and `supervise` complete it, and `docker agent install` runs the supervisor as a LaunchAgent. Shared folders appear in the guest at
  the same path, so `docker build`, `docker compose up --build --wait`, bind mounts, networks and health checks work unchanged.
  `--publish HOST[:GUEST]` forwards chosen Mac loopback ports to the guest, so a published container port is reachable from the Mac.
  See [docs/docker-engine.md](docs/docker-engine.md) and `scripts/docker-engine-live-test.sh`.
- **`fluxctl kube`: Kubernetes (k3s) in FluxVM VMs, on a Mac.** Like `colima start --kubernetes`, built the way `fluxctl docker` is.
  `fluxctl kube up` creates (or starts) a Debian VM running k3s, forwards the API server to `127.0.0.1:6443` over a supervised SSH
  connection, and writes a kubeconfig (cluster, user and context named after the VM, mode 600) to `~/.kube/fluxvm-NAME.yaml`,
  leaving `~/.kube/config` alone. `env`, `status`, `down [--purge]` and `supervise` complete it, and `kube agent install` runs the
  supervisor as a LaunchAgent. Also: `--share` and `--publish` as for `docker`; `--workers N` adds agent VMs (`NAME-w1`…) on a
  private network shared with the server, so nodes and pods reach each other (VMs on the default NAT network cannot); `kube load
  IMAGE…` copies images built with `fluxctl docker` into every node; `kube fetch DIR` plus `up --offline DIR` install from a
  SHA-256-verified download with no internet on first boot. **Cilium is the default pod network** (`--cni cilium|flannel`, chart
  pinned, `--cilium-version`), installed through k3s's helm controller; `--offline` defaults to flannel, which is pinned to the
  private card when there are workers. The LaunchAgent, SSH-forward and pid-file code is shared with `fluxctl docker`. See
  [docs/kubernetes-on-mac.md](docs/kubernetes-on-mac.md) and `scripts/kube-live-test.sh`.
- **Studio: Kubernetes page, pod logs and shell.** *Platform → Kubernetes* lists each `fluxctl kube` cluster (the VMs labelled
  `fluxvm.kube`) with its nodes and pods, read-only, using the client certificate in `~/.kube/fluxvm-NAME.yaml` of the daemon's user
  and reaching the API through the loopback forward that file names, then the server VM's own address. `GET /v1/kube` and
  `GET /v1/kube/{name}` serve it. Each pod has **Logs** and **Shell**: `GET /v1/kube/{name}/pods/{ns}/{pod}/log` returns the tail as
  text, and `POST …/exec-ticket` (admin) plus `GET /console/kube/ws/{ticket}` bridge the cluster's `exec` over mutual TLS to the
  browser terminal (single-use 30 s ticket, names validated). Logs open in a Terminal.app-style window coloured by level, key, number,
  address, URL and HTTP verb and status, with a `-word` filter, tail size, previous instance, follow, wrap, copy and download. The
  Machines page groups a cluster's VMs as `kube NAME`.
- **Studio: a traffic graph for Kubernetes clusters.** `GET /v1/kube/{name}/flows` runs `hubble observe` in each Cilium agent over the
  cluster's `exec` API and folds the flows into workload-to-workload edges (protocol, port, flow and new-connection counts, drops).
  The Kubernetes page gets a **Traffic** tab: a left-to-right service map with line weight by flow count, red for drops, a details
  panel per workload, and filters for namespace, name or port and for system pods. Needs Cilium (the `fluxctl kube` default);
  flannel clusters are told so.

## 0.4.14 (2026-10-23)

- **Linux release build.** The Linux package job now builds the eBPF objects with clang 18 (the Ubuntu 22.04 container's clang 14 could not
  build `fluxvm_qemu_egress` or the service XDP objects), trusts the workspace for git, and runs on runners with Docker. The verifier
  complexity budget for `fluxvm_tc` is 650,000 instructions (65% of the limit; the VM-edge program uses 548,638).
- **Removed the previous consoles.** `/console/legacy` and `/console/app.js` are gone from the node daemon and from fleet central (and the
  embedded `console.html`/`fleet_console.html` and scripts). Studio, at `/console/`, has everything they had.
- **Studio: stacks and ports on the node Machines page.** A strip per `fluxvm.stack` with Start all / Stop all, and a Ports column
  (`8080→80`). These were the two things the legacy node console had that Studio lacked.
- **Studio: team members.** A tenant's Members page (add by work email, change a role, remove or leave) and a Members button per tenant for the
  operator. This was the last page of the previous fleet console without a Studio equivalent.

## 0.4.13 (2026-10-23)

- **FluxVM Studio: a web UI for the node daemon and for fleet central.** `fluxctl serve` serves a React app at
  `/console/` and `fluxvm-agent central` one at its own `/console/`, both embedded in the binary (no Node at runtime) and
  working behind any reverse-proxy prefix. Node: overview with live events, machines (create wizard; console, guest,
  network and egress policy, storage, protect, migration), sandboxes, agent sessions, networks (virtual networks, groups,
  policies, services, flows, inspect), images, volumes, branches, backups, host (with the Mac pool on a Mac) and GPU passthrough, access and
  an API explorer. Central: hosts (cordon, drain, scrub, lease), VMs, networks, volumes, warm pools, tunnels, enrollment,
  operations, the image catalog, tenants and keys, billing with invoices, activity and webhooks. A central VM's sheet has its live
  screen with Control (clicks, scrolling and keys, macOS guests), snapshots and clones, and a command runner. Dark by default with a light theme, ⌘K palette, reduced-motion support. New:
  `POST /v1/vms/{id}/ws-ticket` and `GET /console/ws/{ticket}` (single-use browser WebSocket tickets, since browsers cannot
  send `Authorization` on one). The old consoles stay at `/console/legacy` for one release. `/console` now redirects to
  `/console/`. Tests: `ui/e2e/run.sh` (page smoke test on a mock API) and `ui/e2e/flows.sh` (the fleet forms against a real
  central and a simulated Mac) and `ui/e2e/run.sh vmtools` (a VM's screen, snapshots, commands and the catalog); `scripts/e2e/agent-central-api.sh` now checks the Studio routes, and
  `scripts/e2e/agent-central-vms.sh` clones a stopped VM through central. See docs/ui.md.

- **Egress policy for user-mode (slirp) guests.** `fluxvm_qemu_egress` allowed only loopback, so a guest on QEMU
  user-mode networking could reach nothing (no guest agent download, no package mirror, no control plane) and no policy
  could change that: `network/policy` needs a host-visible interface. The program now reads a per-VM policy from pinned
  maps: `loopback` (the unchanged default), `allowlist` (`cidr` + optional `tcp|udp/port` rules), `audit` (allow
  everything, record what an allow-list would have dropped) and `open`. Refused or would-be-refused destinations are
  counted per `(address, port, protocol)`, so `POST /v1/vms/{id}/network/egress/suggest` can propose the rules that
  fix a denial. New: `GET/PUT/DELETE /v1/vms/{id}/network/egress`, `…/egress/observed`, `…/egress/suggest[?apply=true]`,
  `GET/PUT /v1/network/egress/defaults[?apply_running=true]`, `CreateVmRequest.egress`, `[sandbox.dataplane]
  qemu_egress_mode` / `qemu_egress_rules`, and `fluxctl network egress*`. A VM started before this build keeps the old
  program (no maps) and must be restarted to take a policy. IPv4 only; Cilium rewrites service addresses to the backend
  before the hook runs, so rules must name the backend. See docs/qemu-egress.md.


- **`exec`, `shell`, `sandbox run` and `fleet exec` keep argument boundaries.** Several words after `--` were joined with
  spaces, so `fluxctl exec ID -- sh -c 'a b'` ran `sh -c a b` and `{{.Names}}`-style arguments were lost. They now use
  the same rule as `agent exec`: one argument is a shell line as typed, several are quoted word by word.

- **macOS: the vz runner no longer gets silently terminated.** The windowless runner is now exempt from AppKit
  automatic/sudden termination and holds an activity assertion while the guest runs, and a SIGTERM/SIGINT it receives
  is logged (`{"event":"signal"}` in `vz-runner.log`) instead of looking like a clean exit. Previously a guest could
  vanish with no `vm.stop` event and nothing in the log.

## 0.4.12 (2026-10-17)

- **Docker Engine API shim.** On macOS, `fluxctl serve` listens on `{run_dir}/docker.sock` by default. Point Docker CLI
  at it with `DOCKER_HOST=unix://…/docker.sock` (`brew install docker`). Containers map to OCI sandboxes; images to the
  FluxVM OCI cache. Supported: `pull`, `images`, `run`, `ps`, `rm`, `logs`, `exec`, `stop`. Not supported: `build`,
  Compose daemon, bind mounts, Docker networks. See
  [docs/oci-sandboxes.md](docs/oci-sandboxes.md#docker-cli-docker_host) and
  [docs/vs-desktop-hypervisors.md](docs/vs-desktop-hypervisors.md). Live: `scripts/docker-compat-live-test.sh`.

- **Step-up TOTP for exec and writes.** `[[auth.mfa]]` pairs a static API token with a TOTP secret file; guest exec,
  shells and (optionally) every write then need a current code in `X-Fluxvm-Totp` (`fluxctl --totp`). Codes are
  single-use, repeated failures lock the token out, and the audit log records `mfa=`. See docs/host-hardening.md.
- **VMM Guard exec allow-list and signed rule packs.** `fluxvm-guard apply --exec-policy` restricts a guarded
  cgroup to approved executables; `--rule-pack` loads signed, time-limited deny rules (exec, open, device) as a
  virtual patch. `learn-exec`, `sign-exec-policy`, `sign-rule-pack` and `pack verify` are new. Signing is shared with
  the image catalog (`fluxvm_core::signing`). Verified on a BPF-LSM kernel with
  `scripts/test-vmm-guard-exec-patch-host.sh`.
- **Guard status over REST and `fluxctl guard`.** `GET /v1/vms/{id}/guard` (read-only) reports mode, exec
  allow-list, applied rule packs and `expired_packs`; `fluxctl guard ...` wraps `fluxvm-guard` locally.
- **Fix: guard device/file allow-lists never matched.** Device numbers were encoded in the userspace layout, but the
  kernel compares `(major << 20) | minor`, so the device, writable-file and new exec allow-list keys all missed.

## 0.4.11 (2026-10-16)

- **macOS package is Developer ID signed and notarized.** The published Homebrew / GitHub tarball is notarized and
  stapled. `fluxctl` is signed with `com.apple.security.cs.disable-library-validation` so Hardened Runtime can load
  Homebrew's `libhivex` (different Team ID). First clean cut after the 0.4.10 asset churn.

- **Speculate reports egress from the VM's network policy.** Sandbox speculate results include whether the sandbox
  could reach the network, derived from the applied policy (not a guess).

## 0.4.10 (2026-10-16)

- **QMP sockets under the short `run_dir` (macOS `sockaddr_un` overflow).** Putting `qmp.sock` under
  `~/Library/Application Support/FluxVM/instances/<uuid>/` (or `migration-receivers/…`) overflows the ~104-byte Unix
  socket path limit on macOS; launches and CI then failed with `path must be shorter than SUN_LEN`. QEMU now binds
  `run_dir/qmp/<id>.sock` (default `/tmp/fluxvm-run-<uid>/…`), records it as `control_socket`, and falls back to the
  legacy workspace path for older VMs. Migration receiver workspaces move to `run_dir/mr/<id>/`. Recovery tests use a
  short `/tmp` tempdir. Unit-tested: machine-type reconcile + expired receiver TTL (`fluxvm-scheduler` 300/300).

- **Linux release package is built in an `ubuntu:22.04` container.** The self-hosted runners differ in OS release, and a
  package linked on a newer glibc (2.38/2.39) failed the workflow's own 22.04 smoke test. The glibc floor is now 2.35
  whatever runner builds it, and the runner host needs no sudo. Smoke test and release attach run as a separate job.
- **`apple.host_gpu`: host Metal/MLX inference for offline VZ sandboxes.** Linux guests cannot use GPU passthrough; with
  `host_gpu: true` (sandbox create, session `runtime.host_gpu`, or `fluxctl agent run --host-gpu`) the daemon relays guest
  `http://127.0.0.1:8080/v1` over vsock to `[apple].host_infer_port` (default 8080), installs the cloud-init forwarder, and
  injects `OPENAI_BASE_URL` on exec. `GET /v1/host/infer` / `fluxctl infer status` probe the host server; `fluxctl infer serve
  --stand-in` is a stub for path tests. Docs: [host-inference.md](docs/host-inference.md). Unit-tested (wire + cloud-init);
  live path: `examples/agent-sandbox-demo/local-model.sh`.

## 0.4.9 (2026-10-16)

- **`fluxctl vm rename|clone|fork`: the VM verbs under one noun.** `rename-vm`, `clone-vm` and `fork-vm` read differently from
  `fleet`, `macos-pool` and `vm-template`. `vm <verb>` is the same command (it is rewritten to the existing handler, so there is
  one implementation and one set of flags). Nothing is removed or changed: the hyphenated names keep working, and on a terminal
  they print a one-line "now `fluxctl vm clone`" note on stderr (nothing on a pipe, so scripts and captured output are
  untouched). The grouped `--help` and the docs use the new spelling. Not done, deliberately: removing the old names, which
  would be the breaking change. Unit-tested (all three verbs, the flags of `fork`, the hint, an unknown verb).

- **`[sandbox.dataplane] require_edge = true` refuses a VM that cannot be enforced.** A VM with no host-visible interface
  (user NAT, `network.mode = none`) has no edge to attach the dataplane to, so it has always run with no network policy,
  rate limit or flow records, even with `required = true` (the default), which only warns since the last release. With
  `require_edge = true` such a create is refused before launch with a message naming the fix. It is a separate key, off by
  default, because `required` is on by default and turning it into a refusal would reject every `mode = none` VM on upgrade;
  the nftables mode and `vz` VMs are not affected. Unit-tested (the two switches, and `apply_sandbox_policy` refusing and not
  refusing). Run live on a Linux host (`required = true`, `network.mode = none` qemu VM): `require_edge = true` gave HTTP 400
  with the message, `false` gave 201 and a running VM. Not run: a real tap/bridge VM with it on, macOS.


- **A locked Mac now says why warm-pool restores fail.** On a locked screen (or with nobody logged in) every snapshot
  restore fails; sandboxes then silently cold-booted and `macos-pool checkout` returned a bare restore error. The session is
  now read in one shared place (`fluxvm-core::login_session`, also used by `doctor`): `GET /v1/sandboxes/density` gains
  `login_session` and `restore_blocked`; new `GET /v1/macos-pool/status` / `fluxctl macos-pool status` report whether a
  checkout can restore and the slot counts; a checkout that fails because of the session returns 503 with the fix and leaves
  the slot idle; the warm-restore and hibernate-wake warnings carry the reason. Additive only (older readers ignore the new
  fields). Run on this Mac while its screen was locked: `status` and the density report both showed `locked` with the hint.
  Unit-tested: the parser, the 503 mapping, an older report without the fields. Not run: a checkout of a real enrolled slot on
  a locked Mac (the 503 path), or an unlocked-to-locked transition.


- **The installer ends with `fluxctl doctor` and a one-line tab-completion command for your shell.** `fluxctl completions`
  already printed bash, zsh and fish scripts, but nothing told anyone. `scripts/install.sh` (and its copy served from the
  site) now lists `fluxctl doctor` first under "Next" and prints the exact completion line for `$SHELL` (creating the
  directory it writes into); `docs/install-macos.md` has the same in a short section. The Homebrew formula lives in the tap
  and is not changed here (a `generate_completions_from_executable` line would install them). Run: the message block for
  zsh, bash, fish and an unknown shell, `bash -n`; not run: a full install.

- **`scripts/preflight.sh` now runs `fluxctl doctor`, so the two cannot drift apart.** New `fluxctl doctor --host-only`
  checks the machine's prerequisites and skips the daemon check, for a host FluxVM is not installed on yet (an unopenable
  `/dev/kvm` is then a warning, since a packaged daemon runs as root). The script uses it when a `fluxctl` that has the flag
  is on PATH or built in `./target`, and otherwise runs its old checks, so it still works on a fresh machine. `--native-kvm`
  keeps its extra binaries. Run on macOS (wrapper path, fallback path, bad argument); the Linux run is listed in the PR.

- **`fluxctl doctor --fix` repairs MCP entries that `brew upgrade` left pointing at a removed path.** It rewrites only the
  entry's `command` to the current `fluxctl`, so `--allow-write`, `--context`, env and tokens survive (a plain `mcp install`
  rewrites the whole entry and drops them); an entry that does not name a `fluxctl` is left alone, a second run changes
  nothing, and the report is taken again afterwards. Without the flag `doctor` still never writes. The LaunchAgent is not
  touched (it starts a service): its fix command is still printed. Unit-tested (JSON and Codex TOML, comments kept) and run
  on macOS against a stale entry under a scratch `$HOME`. Not run: against a real upgraded Homebrew install.

- **`fluxctl doctor` no longer fails a healthy Linux host because of who runs it.** Run on a Linux host for the first time,
  as a user outside the `kvm` group, it exited 1 with `kvm FAIL` while the root daemon was healthy and its VMs were running.
  An unopenable `/dev/kvm` is now a warning while the daemon answers (it runs as another user), and stays a failure when
  nothing answers or `/dev/kvm` is missing. Also silenced two Linux-only dead-code warnings. Run on the Linux host as an
  unprivileged user (warn, exit 0), as root (ok, exit 0) and with no daemon (FAIL, exit 1).

- **macOS guests get their agent token from the host; nothing is provisioned by hand in a clone.** The guest agent
  needed a token typed into every macOS guest (no cloud-init), or it refused everything. The daemon now writes the VM's
  token to `agent-token` (0600) in the read-only `firstboot` share and attaches that share whenever a macOS guest has
  `apple.macos_vsock` and `agent.enabled`; `fluxvm-guest-agent --token-seed <file>` (set in the shipped LaunchDaemon plist)
  adopts it only while the agent has no token of its own, and a token already there always wins. Install the agent once,
  without a token, in the template. Unit-tested, and run live on an M4 with `scripts/vz-macos-agent-seed-live-test.sh`:
  macOS 26 installed from the cached IPSW, the agent installed with no token (it refused), a clone's guest token equalled the
  VM's, the agent log named the share, and `uname -sm` ran through the API; a clone with the agent off got no share. The
  share is readable by any process in the guest that can read it (like the other firstboot files). **Not run:** a macOS 27
  guest, FileVault, a token rotation followed by a reboot.

- **UDP published ports for container sandboxes.** `ports` accepts `HOST:CONTAINER/udp` next to TCP. The Mac's NAT
  relay is TCP-only, so a sandbox that publishes UDP gets the userspace gateway (`apple.usernet`) as its network card,
  which already forwards both; a UDP port is refused together with `expose`, `gateway_hosts` or private `networks`
  (not run with the gateway). Host-port conflicts are per protocol, and a sandbox's ports on the gateway now count when
  another sandbox asks for the same host port. Compose `ports: ["53:53/udp"]` imports as a UDP port instead of being
  refused. Unit-tested, and run live on an M4 with `scripts/oci-udp-ports-live-test.sh`: a UDP datagram and a TCP line
  to the published `127.0.0.1` ports are echoed by `nc` in an alpine container, a clash is refused, and delete frees
  the port. This is also the first live run of published TCP ports for container sandboxes. Not run: a UDP port next
  to volumes, restarts or health checks, and UDP through `--fleet` placement.


- **`fluxctl doctor` reports a locked or absent login session on a Mac.** Snapshot restore and warm-pool slots need a
  logged-in, unlocked session; on a locked or headless Mac they failed with no hint why. `doctor` now reads the
  console session from `ioreg` (`CGSSessionScreenIsLocked` inside `IOConsoleUsers`; `IOConsoleLocked` is not used,
  it said `No` while the session was locked) and warns with the unattended-hosts fix. Unit-tested on captured
  `ioreg` text, and run on an M4 whose screen was locked at the time (warned). **Not run:** a Mac with nobody
  logged in at the console, and the pool status endpoint does not report it yet.

- **A VM the dataplane cannot attach to is no longer skipped silently under `required = true`.** A VM on user NAT
  or `network.mode = none` has no host-visible interface, so the eBPF/Cilium dataplane has nothing to attach to and
  skips it (failing closed would be wrong: there is no edge). That skip was logged at `debug`, so an operator who set
  `[sandbox.dataplane] required = true` saw nothing while the VM ran without policy, rate limits or flow records. It
  is now a `warn` that says so; without `required` it stays at `debug`. Unit-tested (the decision is a pure function),
  and observed in a live daemon log on a Linux host: with `required = true` a `network.mode = none` VM logged the warning,
  and with `required = false` it logged nothing.



- **CLI rough edges.** `fluxctl --help` now lists `doctor` and prints the exit-code contract: `0` success, `1` an error
  or a failing `doctor` check, `2` a usage error, and for `run`, `sandbox run`, `ssh`, `fleet exec` and
  `agent exec` the command's own status (these pass the guest's code through, so they are deliberately not one
  enum). The `-o` help text and docs now name every command that honours it (`disk list`, `vm-template list` and
  `context list` were missing). `fluxctl mcp status` marks an entry `"stale": true` with a `fix` when its binary
  no longer exists, which is what `brew upgrade` leaves behind. `fluxctl run` and the SSH wait print a "still
  waiting" line every 15 s instead of staying silent for up to three minutes. Unit-tested; the progress lines
  were not run against a slow first boot.

- **`fluxctl doctor`: is this host ready?** A read-only report that works with the daemon down. It checks the daemon
  (`/healthz`, `/readyz`), the binaries on `PATH`, free disk under the state dir and, per platform, `/dev/kvm`
  (Linux) or the runner's `com.apple.security.virtualization` entitlement and memory headroom (macOS), plus MCP
  client and LaunchAgent entries that still point at a removed Homebrew `Cellar` path. Each warning or failure
  prints the command that fixes it; exit code 1 on a failure. Text by default, JSON with `-o json`. Unit-tested
  (the checks take injected observations) and run on an M4 with the daemon down (exit 1, fix printed) and up
  (exit 0). Run on a Linux host (80.79.5.173) as an unprivileged user, as root and with no daemon: see the next entry for what it
  found. **Not run:** the stale-path warning against a real upgraded Homebrew install (unit-tested only).

- **Fleet-level macOS pool checkout.** The per-Mac macOS pool (`/v1/macos-pool`, two slot-bound guests) could only be
  used by calling one Mac. Each Mac's heartbeat now carries its idle slot count (`apple.macos_pool_idle`, absent when
  the daemon would not say), and `fluxvm-agent central` has `POST /fleet/macos-pool/checkout` (operator): it checks a
  slot out on the healthy, uncordoned Mac with the most idle ones, tries the next when a report was stale (409), and
  names the node in the reply; `POST /fleet/macos-pool/{node}/{id}/return` gives it back. Operator-only for now (no
  tenant leasing). Unit-tested with stub nodes only; not run across real Macs.

- **`vz` capability table: what the macOS 27 SDK cannot do.** Added a row for virtio-input devices, a GDB debug stub,
  a guest battery and clipboard sharing for macOS guests: the SDK headers have no public API for them, so FluxVM cannot
  offer them. The one unwired public class is the custom-Virtio shared-memory region, which needs a guest-side driver
  to be useful and is left out for now.

- **Cross-Mac private networks: a real-cipher loopback test.** `fluxvm-vz-switch` had overlay tests only with a
  stand-in cipher. A new integration test runs three switches with the real AES-256-GCM overlay on one machine
  (flood across Macs, unicast after learning, a 1400-byte frame, and a wrongly keyed switch that neither receives nor
  injects). Still not run on two real Macs.

- **Rebalance plan for `vz` Macs (dry run).** `GET /fleet/rebalance/plan[?high_water=0.85&max_moves=10]` (operator)
  lists which VMs could leave a Mac whose memory use is above the high-water mark (of what VMs may use, after macOS
  headroom) and which Mac each would fit on without going over the mark or out of CPU. Only VMs created `reschedulable`
  or ephemeral are candidates and macOS guests never are, the same rules as a reschedule drain; there is no live
  migration, so applying a move is a recreate. Changes nothing. Unit-tested only; not run on a real fleet.

- **`vz` placement prefers a Mac that already has the image.** Each Mac's heartbeat now lists its cached built-in images
  (new `GET /v1/host/images`), and `fluxvm-agent central` adds a bonus for a host that holds the requested image name,
  so a new `debian-13` VM lands where the ~400 MB download is already done instead of fetching it again. A host that
  cannot fit the VM is never chosen for it. A path in `image` carries no bias. Unit-tested only; not run across two Macs.

- **`apple.usernet` live checks, and the rows that said "unverified" now say what ran.** New
  `scripts/vz-usernet-live-test.sh` boots a Debian 13 guest on an M4 and checks: root login with an SSH key written
  into the root disk offline (cloud-init copy dropped), a TCP forward, a UDP forward answered by a guest listener,
  guest traffic and a DNS lookup in `/v1/netobs/flows` and `/v1/netobs/dns`, and the runner's plain refusal of a pause
  on a paused VM. All passed. Not covered: the vsock-to-TCP relay and the `fluxvm-blockd` NBD client.

- **Volumes on `vz` VM sandboxes.** `volumes` were refused unless the template was QEMU-backed, although `vz` already
  mounts `shared_folders` as virtiofs. A Mac sandbox now takes named volumes (persistent host directories, cloud-init
  fstab mounts, cold boot). Verified on an M4 by `scripts/vz-sandbox-volume-live-test.sh` (write from the guest,
  survives delete, re-read by a second sandbox); `read_only` and several volumes are unit-tested only.

- **`vz` placement keeps macOS headroom.** A Mac's heartbeat advertised all of its unified memory minus allocated VMs,
  so `fluxvm-agent central` could fill a 16 GB Mac to the last MiB and leave macOS to swap. It now advertises memory
  after the larger of 3 GiB and a quarter, the rule Kairon's Mac node already uses (`apple_placement::allocatable_mib`).
  Unit-tested only; not run on a second Mac.

- **Docs and comments no longer call hardware-verified Apple features unverified.** `apple.usernet` (DHCP, DNS, TCP
  egress, TCP forwards, policy) and macOS-guest vsock were verified on an M4 (hardware log in `docs/macos.md`), but
  the capability table, model docs, runner header and one doc paragraph still said "unverified" or "never booted".
  They now say what ran; UDP forwards on usernet remain unit-tested only. No behaviour change.

- **Migration retries no longer inherit a cancelled attempt's limits.** `migration/start` stated capabilities but not
  the tunables, and QEMU keeps them: after a cancelled 1 Mbps attempt, a retry with default settings stayed `active`
  and never finished. Every start now sets `max-bandwidth`, `downtime-limit`, multifd channels and the auto-converge
  throttle, using QEMU's own default when the request leaves one out. Found by the two-host script; the retry now
  completes in about 4 s.

- **Linux aarch64 release package is cross-compiled.** The release workflow built it on a hosted arm64 runner this
  organization does not have, so the job queued forever. It now cross-compiles on x86_64 (marked experimental: a
  failure does not block the x86_64 package or the release), and `scripts/package-linux.sh` packages a cross-built
  binary. Workflow and script only; no aarch64 release has been cut with it yet.

- **User-manual PDFs removed.** `docs/user/pdf/` (four PDFs, their HTML sources and an index) and the README section
  listing them are gone; the markdown manual is the source. The PDF builder (`build-user-pdfs.mjs`), the
  `.docs-tools` markdown dependency and the PDF copying in `sync-to-website.mjs` are removed too.

- **AppArmor: Kairon image cache and shared-disk locks.** The `fluxctl`/`fluxvm` profiles allow read+lock on
  `/var/lib/kairon-node/image-cache/**` (files are `kairon`-owned 0600, so `owner` never matched; every image-booted
  Machine failed `opening base image ... Permission denied`) and `k` on `owner /**` for QEMU's image lock on a
  `storage=shared` disk. `scripts/test-apparmor-profile-rules.sh` checks both, in CI. Found on a two-node Kairon
  cluster: the veyron daily-ops E2E went from 18 to 37 passed. Reload with `apparmor_parser -r`.

- **QEMU live migration: machine type, relay, retries.** A receiver boots the source's concrete machine type
  (label `fluxvm.dev/machine-type`, refused when the host's QEMU lacks it). The TCP relay closes both sides when
  either ends and now fronts every QEMU source (the egress hook drops non-loopback dials). Capabilities/params are
  restated on each attempt, a leftover TLS creds object is replaced, cancelling a completed non-post-copy migration
  resumes the guest, and a receiver still receiving keeps its TTL. Unit-tested, and run live between two hosts (shared NFS disk, TCP relay) with
  `scripts/test-migration-two-host.sh`: pre-copy, multifd, post-copy, x509 TLS (no `tls_hostname` given), a cancel
  mid-transfer followed by a retry, an unsupported machine type refused, a source on QEMU 8.2.2 into 10.2.1, and a source on QEMU 7.2.22 (Debian 12, machine `pc-q35-7.2`) into 10.2.1.
  In each the guest kept its uptime across the move. Also run: zstd multifd compression, and auto-converge with a
  guest dirtying memory (QEMU throttled the guest CPU up to 99% and the migration converged). Not run: a source
  older than QEMU 7.2. The script's `rapi` no longer aborts when the daemon restarts mid-poll, and a source guest that
  never answers now prints the VM record and console tail.

- **Browser terminal: search, copy mode, quick select, prompt marks, command palette.** `⌘F` search, `⌃⇧X` vi-like copy
  mode, `⌃⇧Space` quick select (URLs, paths, UUIDs, IPv4, hashes, numbers on screen), `⌘↑`/`⌘↓` between prompts,
  `⌘⇧P` palette. Prompt marks are OSC 133 from the bash rc file, emitted only for `TERM_PROGRAM=FluxVM`. Node tests
  cover the matcher, hint labels, motions, yank, prompt jumps and palette filter; each was also driven in headless
  Chrome on a vz session. Quick select misses matches wrapped across rows; not run in Safari or Firefox.

- **Browser terminal.** `/console/terminal?session=<id>` and `fluxctl agent shell --web`: xterm.js (vendored, MIT) with
  WebGL, tabs and split panes, themes (default "Neon Storm" after the author's WezTerm config), inline images
  (sixel, iTerm2), links, a clock, and WezTerm-style keys. Auth for the WebSocket is a 30 s single-use in-memory ticket
  (`POST /v1/agents/sessions/{id}/pty-ticket`). Run in headless Chrome on vz.

- **Plain ssh into a session and terminal-program forwarding.** `fluxctl agent ssh-config <session> [--name N]`
  (`GET .../ssh`) prints an OpenSSH `Host` block (address, or a `vsock-proxy` ProxyCommand for an offline guest) so
  `ssh`, `scp`, `rsync` and WezTerm's `wezterm ssh` work; local machine only, refused to tenant tokens.
  `fluxctl agent shell` forwards `TERM_PROGRAM` and its version. Hyperlink, inline-image (iTerm2, sixel, kitty), mouse
  and 24-bit colour sequences were checked to arrive intact; no real WezTerm was available to draw them.

- **Terminal look.** `fluxctl agent shell` forwards the local `TERM` and truecolor `COLORTERM` to the guest and, for a
  bash login shell, adds colour for `grep`/`diff`/`ip`, `ll`/`la`, coloured man pages and compiler messages (`--plain`
  turns it off). Checked by variables and aliases in the guest, not by a Terminal.app screenshot.

## 0.4.8 (2026-10-13)

This release adds **AgentSession**, a single object an agent drives instead of assembling VM, sandbox, snapshot and
fork itself, with background jobs, a file API, git verbs, an interactive terminal, preview URLs (including
WebSockets), command rules, volumes, rollouts and trajectory/OTLP export, plus per-disk I/O throttling on QEMU.
Session behaviour below was run on an Apple M4 (`vz`, stock `debian-13`); the in-tree KVM fork, tenant tokens against
a real daemon, Linux, a real OpenTelemetry collector and a successful `git clone` were not run.

### Added

- **Terminal resize.** `fluxctl agent shell` forwards a local window resize (SIGWINCH) as a `{"resize":{...}}` text
  frame; the daemon sets the guest tty's size, so editors and `top` reflow. Run live: 30x100 to 50x160 to 20x70
  reported correctly in the guest and a `WINCH` trap fired.
- **Interactive terminal for agent sessions.** `fluxctl agent shell <session> [-- cmd]` (WebSocket
  `GET /v1/agents/sessions/{id}/pty`) gives a real pty over `ssh -tt`, on offline sessions too (vsock proxy). Exit
  codes propagate; open/close are ledgered (counts, not content). Size follows the local window (resize is forwarded); refused for
  sessions with command rules; `capabilities.pty` is now true on vz. Run live on vz with a local pseudo-terminal.
- **OTLP trace export.** `fluxctl agent export <session> --otlp http://collector:4318 [--otlp-header K=V]` sends a
  session's trajectory as one OTLP/HTTP JSON trace (root span plus a span per step). After-the-fact, built from the
  ledger: no live stream, no reasoning content. Checked against a stub receiver, not a real collector.
- **Preview URLs carry WebSockets.** A WebSocket upgrade on `/preview/{token}/...` is now bridged to the guest with
  the same token check (it used to answer `101` and then drop). Run live on vz with Chrome DevTools: navigate,
  evaluate, screenshot. Docs gain a Chromium/CDP recipe for the stock image (needs `socat` because Chrome binds
  DevTools to localhost). No browser image, no Playwright client run.
- **Volumes for vz agent sessions.** `runtime.volumes` / `fluxctl agent run --volume NAME:/path` attaches a named
  host volume as a virtio disk, formats it ext4 only when `blkid` says it is empty, mounts it and persists an fstab
  line. One VM at a time (`409` otherwise), no fork for such a session, tenant tokens refused. New sandbox create
  field `extra_disks` (vz). Run live on vz: data written in one session was read in the next.
- **Rollouts.** `fluxctl agent rollout <session> --count N -- <cmd>` (MCP `session_rollout`) forks N children, runs
  the command in each in parallel, returns every exit code and output, then deletes the children (`--keep` to keep
  them). It is composed client-side from fork, exec and delete, so it follows fork's rules (in-tree KVM, or a vz
  session with no NIC) and each child is a full sandbox. Run live on vz: 3 children, fork 2.5 s, parallel exec 129 ms
  wall; each child saw the parent's file. Not a reward/RL harness: no scoring, no reset-to-checkpoint verb.
- **Process rules enforced.** A session's `policy.process: {allow, deny}` (glob patterns over the command string) is
  now checked on `exec` and job start: a refused command is `403` and ledgered as `exec.refused`/`job.refused`.
  Compound commands are split and each piece checked. This is a guardrail against accidents, not a sandbox
  boundary. `process.ask` is refused at create (no approval flow yet); `filesystem` rules are still unenforced.
- **Trajectory export.** `GET /v1/agents/sessions/{id}/trajectory` merges the session ledger and the sandbox's VM
  events into one time-ordered, numbered list; `fluxctl agent export <session> [--out file]` writes it as JSON Lines
  (a header line, then one step per line). It records what the API and hypervisor layer did, not the agent's
  reasoning, which FluxVM never sees.  `--otlp` sends the same data as OpenTelemetry traces.
- **File API for agent sessions.** `GET .../files?path=` (list or stat), `GET|PUT .../files/content`, `DELETE .../files?path=&recursive=`; `fluxctl agent put|files|cat|pull|rm-file`; MCP `session_files`, `session_file_read`, `session_file_write`. About 1.5 MiB per write; deleting `/` or a one-component path is refused. Run on a `vz` NAT session on an M4.
- **Git verbs for agent sessions.** `GET .../git/status|diff|log`, `POST .../git/commit|branch`; `fluxctl agent git <session> status|diff|log|commit|branch`; MCP `session_git` (writes need `--allow-write`). Runs `git` in the guest, so the image needs it (stock `debian-13` has none: a session with network access can `apt-get install git`; an offline one cannot, and gets `409` with that message). A diff is capped at 256 KiB. No push, pull, fetch, merge, rebase or checkout of an existing branch.

- **Background jobs for agent sessions.** `fluxctl agent bg|logs [-f]|kill`, MCP `session_job_start|read|kill`, and
  `POST /v1/agents/sessions/{id}/jobs`, `GET|DELETE .../jobs/{job}`: a detached command whose output is read from a byte
  offset. Polling over exec, not a stream; no stdin or TTY. Run on a `vz` NAT session on an M4.

- **AgentSession API and CLI.** `POST/GET /v1/agents/sessions`, `GET|DELETE .../{id}`, `.../exec`, `.../checkpoint`,
  `.../restore`, `.../fork`, `GET .../events`, `GET .../artifacts`. `fluxctl agent run|ls|get|exec|checkpoint|restore|
  fork|timeline|artifacts|rm`. Sessions are tenant-scoped, survive a daemon restart, and keep an action ledger. Policy
  presets (`offline`, `developer-safe`, `open`) and host allow-lists are enforced; filesystem and process rules are
  recorded as `unenforced`. Fork works on `vz` for offline or allow-listed Linux guests and is refused with a reason
  (409) on NAT sessions. Timings and targets: `docs/benchmarks/agent-session.md`.
- **MCP tools** `session_*`, `sandbox_speculate`, `sandbox_changesets`, `sandbox_changeset_decide` (writes need
  `--allow-write`); **Go and Python SDK** session clients; `skills/fluxvm/SKILL.md`.
- **`scripts/install.sh`** (Homebrew-only; `--client`, `--no-service`, `--uninstall`, `--dry-run`); `/agents` page with
  install tabs and the microsandbox comparison (`docs/vs-microsandbox.md`).

- **Preview URLs for agent sessions.** `fluxctl agent expose|previews|unexpose`, MCP `session_preview`, and
  `POST/GET/DELETE /v1/agents/sessions/{id}/preview[s]` plus a token-gated `/preview/{token}/...` route: an expiring,
  revocable URL to one guest TCP port. Run on a `vz` NAT session on an M4; WebSocket, streaming and Linux not run.
- **Per-disk I/O throttling (QEMU).** `io.throttle` on data disks and `root_disk_io` sets IOPS and bandwidth limits
  (total or read/write, with optional bursts), applied by `block_set_io_throttle` when QEMU starts, on a migration
  target and on hot-add. `PUT /v1/vms/{id}/disks/{name}/throttle` and `fluxctl disk throttle` change them live and
  keep them for later starts; `GET /v1/vms/{id}/disks` reports them. vhost-user-blk disks are refused.
- **Host PSI.** `GET /v1/host/pressure` returns whole-host CPU, memory and IO pressure from `/proc/pressure`, in the
  same shape as `GET /v1/vms/{id}/pressure`. Kairon's DRS uses it to move Machines off hosts that are stalling.

### Fixed
- **The sandbox HTTP proxy now works on macOS.** It connected through the guest's Linux network namespace and failed
  on `vz` with "network namespaces are only available on Linux"; on non-Linux hosts it now connects to the guest's NAT
  address directly (`/v1/sandboxes/{id}/http/{port}/*`).

### Fixed
- **Cloud Hypervisor virtiofs shares.** The `--fs` argument (boot and hot-add) no longer passes `dax=off`. Cloud
  Hypervisor dropped DAX and current releases (v53 here) refuse the option, so a VM with a share exited at start while
  FluxVM reported it running.

## 0.4.7 (2026-10-13)

This release is about FluxMesh. It adds a new crate, `fluxvm-mesh`, and `fluxctl cloud`; **nothing in the macOS package
changes** except the version number, because the FluxMesh binaries are not packaged yet. Build them from source with
`cargo build -p fluxvm-mesh -p fluxctl`. Everything below is unit-tested only; none of it has run on real hosts.

### Added
- **FluxMesh core** (`fluxvm-mesh`, phases 1-3): a personal-cloud overlay control plane that sits above Network
  Fabric and Service Fabric without replacing either. Deterministic overlay IPAM, node/peer identity, NAT
  classification from multi-observer mappings, direct-before-relay path ranking, a cross-node service catalog,
  authoritative FluxDNS (`*.flux`, A and SRV), fail-closed ACLs compiled to Network Fabric CNP, exit-node and
  subnet-router policy, an authenticated relay (plain and native TLS on :443), a STUN client, and a Linux
  WireGuard reconciler (`fluxmesh-agent`). Binaries: `fluxmesh`, `fluxmesh-relay`, `fluxmesh-agent`. Docs in
  `docs/fluxmesh*.md`. State lives in `/var/lib/fluxvm/mesh` on Linux and
  `~/Library/Application Support/FluxVM/mesh` on macOS (`FLUXMESH_DIR` to move it). Not packaged in the macOS
  release yet, and the WireGuard/DNS/relay paths have only unit-test coverage; no multi-host run has been done.
- **FluxMesh enrollment and operator UX** (phase 4): zero-touch enrollment over verified TLS with short-lived,
  replay-protected HMAC invitations (the joiner generates its WireGuard key locally and sends only the public key),
  coordinated distributed key rotation with ack-before-activate and rollback, a dual-stack ULA overlay, rendezvous-hash
  FluxDNS HA, relay HA with multi-home failover and hysteresis, the `fluxctl cloud` flow (`up`, `status`, `invite`,
  `accept`, `serve-enrollment`), and the macOS NetworkExtension contract in `native/fluxmesh-macos` (not built or
  signed here). `fluxctl cloud` uses its own `--state` paths and does not use `--server`/`--server-token`.
- **FluxMesh public edge** (phases 5-7): an optional HTTPS ingress (rustls, HTTP/2, direct HTTP/3 on QUIC) with
  route-bound short-lived share links, native ACME v2 (HTTP-01 and DNS-01 including wildcards, with optional
  DNSSEC-AD verification), dynamic DNS (Cloudflare and a webhook), PCP/NAT-PMP/UPnP router mapping, overlay mDNS
  reflection, and a blind public relay for CGNAT sites (QUIC first, TLS/TCP fallback, active-standby or
  active-active) that never sees application plaintext or a site's certificate key. Adds Prometheus-style edge
  metrics, SLO diagnostics and OTLP span export. Binaries: `fluxmesh-{ingress,edge,mdns,pd,public-relay,metrics}`.
  Pulls `h3`/`h3-quinn` into the workspace. Not in the macOS package, and none of it has run against a real
  browser, CA, resolver or router yet.

### Fixed
- `fluxctl cloud serve-enrollment` declared `--tls-key` twice, so clap's debug assertion failed and the CLI test
  `every_subcommand_has_consistent_arguments` was red on `main`. It now uses the shared `--tls-key` option
  (`FLUXMESH_TLS_KEY`, default `<mesh dir>/tls/key.pem`).

### Docs
- `docs/fluxmesh.md` is now the overview for the whole FluxMesh stack (status, programs, files, ports, quick start);
  phase docs use `<mesh dir>` instead of hard-coded Linux paths. Network Fabric, cross-Mac networks, proof and
  status, the docs index and the README point at FluxMesh with its real status (unit-tested, not run on real hosts).
- Fleet operations and quickstart docs describe the generated operator token and `--no-auth`; the README's
  "What's new" and status table reflect 0.4.6 and the redesigned console.

### Repository
- `packaging/homebrew/fluxvm.rb` follows the published 0.4.6 package, and `examples/fleet-console-video/` holds the
  generator for the console demo video.

## 0.4.6 (2026-10-13)

The security-review round for the fleet platform: console tunnels end with their person and have a maximum lifetime;
viewers no longer get live screens or console logs (and the log view is drawn as a macOS Terminal window); the payment
gate covers start, restart, resume and snapshot restore; `/auth/login` is rate limited; shared warm pools are off for
tenants unless the operator allows them; and sign-up confirms the email address by a mailed link (a mail webhook is
needed before sign-up can be opened); the console has a new look (a top bar with grouped menus and the Zyvor Z);
and `fluxvm-agent central` makes and keeps an operator token when none is given. Three defaults changed: see "Behaviour
change" below.

Tested before release: `fluxvm-agent` (236) and `fluxctl` (187), the only crates that changed since 0.4.5; a package
built locally and installed through Homebrew from a throwaway tap (`brew test`, the `service install` guard,
`/healthz` and `/readyz`, the MCP handshake with a sandbox create, exec and fork, both demo scripts); the packaged
`fluxctl` against a source-built central for the sign-up confirmation flow; and the docs site build.

### Changed: `central` makes an operator token when none is given
- Starting `fluxvm-agent central` without `--token` used to leave `/fleet/*` open to anyone. It now generates a random
  operator token (`fvo_…`) on first start, stores it in `<state-dir>/operator-token` (mode 0600), prints it once, and
  reuses it on later starts. `--token` always wins; deleting the file gets a new one. **Behaviour change:** scripts that
  started central with no token must now send the printed one, or pass `--no-auth` (or `FLUXVM_AGENT_NO_AUTH=1`) to
  keep the open lab behaviour. `--ha-redis` still needs an explicit `--token` shared by every central, and
  `--oidc-issuer` refuses `--no-auth`.

### Changed: the fleet console has a new look
- **Netra-style shell.** A 48 px frosted top bar replaces the sidebar: the orange Zyvor Z with "FluxVM", grouped menus
  (**Overview**, **Cloud**: Macs, Networks, Images; **Compute**: Virtual machines, Volumes, Warm pools, Tunnels;
  **Billing**: Billing, Usage; **Access**: Members, API keys, Activity, Notifications, and for operators Tenants and
  Policy), search and ⌘K, alerts, refresh, a theme icon and an account menu. Menus open on hover or click, close with
  Esc or an outside click, and move with the arrow keys. Below 860 px the groups sit behind a menu button.
- Netra's colours, hairline cards, pill buttons and focus ring in light and dark; icons are inline SVG (no icon font);
  the favicon is the Z. Routes (`#/vms/<mac>/<id>`), roles, SSO and the content security policy are unchanged.

### Added: sign-up confirms the email address; shared warm pools are opt-in
- **Sign-up email confirmation (policy `signup_require_email_verification`, on by default).** `POST /signup` records a
  pending sign-up (24 hours) and mails a link through the operator's **mail webhook** (`--signup-mail-webhook URL|log`,
  `--signup-mail-secret-file`, needs `--public-url`; central does not speak SMTP). The answer is `202`, the same whether
  or not the address has an account. The link opens a page whose button (`POST /signup/verify`, also JSON, also
  `fluxctl fleet signup-verify`) creates the tenant with `owner_email_verified` and shows its token once, so a mail
  scanner cannot use a link up. The Stripe customer is made then. A confirmed owner's first SSO sign-in no longer
  rotates the token. Sign-up cannot be opened while confirmation is required and no mail webhook is set.
  `GET /signup/config` reports `email_confirmation`; the console's sign-up sheet says "check your email".
  **Behaviour change:** a central that had `signup_enabled` and no mail webhook now keeps sign-up closed until one is set
  (or confirmation is turned off, for labs).
- **Shared warm pools are off for tenants (policy `allow_shared_pool_claims`, default `false`).** Their VMs share a
  network and the image's credentials between tenants, and a tenant could plant something in a warm VM that another
  claims next; neither network isolation nor per-claim secrets fixes that. Tenants use pools of their own; the operator
  can allow shared claims for tenants that trust each other. **Behaviour change** for operators using shared pools.

### Changed: viewers no longer get live screens or console logs; console log viewer redrawn
- `GET /fleet/vms/{node}/{id}/screen` and `/logs` now answer `403` for the `viewer` role (both can hold secrets);
  members, admins, owners, API keys and the operator are unchanged. The console says why instead of showing a broken
  picture.
- The console's log view is drawn like a macOS Terminal window: traffic lights, a title, line numbers, a blinking
  cursor, times compacted to `HH:MM:SS.mmm` (full value on hover), sources and keywords in colour, and ANSI colour
  codes honoured. All log text goes through text nodes.

### Fixed: payment gate and sign-in flooding
- Start, restart, resume and snapshot restore are held for a tenant that still owes a payment method, like create and
  clone already were.
- `GET /auth/login` is limited to 20 sign-in starts per client address per minute (behind a proxy on the same machine,
  the forwarded address), so one caller can no longer evict everyone's pending sign-ins.

### Fixed: console tunnels outlived their person and had no end
- A tunnel opened from a console session is now ended (within 30 seconds) when that person is removed from the tenant or
  demoted to a viewer; before, only the tenant being disabled, the API key or the VM ending closed it.
- New policy field `tunnels.max_lifetime_minutes` (default 480, 1 to 10080): a tunnel closes that long after it opened,
  however busy; before, steady traffic kept one open for ever. Policies saved earlier load with the default. Signing out
  of the console and rotating the primary token still do not end open tunnels (documented).

## 0.4.5 (2026-10-12)

The first release after 0.4.4 that fixes what installing 0.4.4 from the Homebrew tap on a clean Mac turned up, and adds
the guard and flags for running a second daemon. Tested before release: the whole workspace per crate (2151 tests, none
failing; the two Linux-only crates do not build on macOS), a package built locally and installed through Homebrew from a
throwaway tap (`brew test`, the service, `/readyz`, the MCP handshake and a sandbox round trip, a second daemon beside
Homebrew's, both demo scripts), and the docs site build.

### Docs and site: agent sandboxes and the Mac cloud platform, with their evidence labels
- New `/agents` page on the docs site: the fork-into-four demo with its real output, the offline and local-model story,
  the measured memory table and the MCP setup, with the caveats (only offline sandboxes fork; copies keep the parent's
  identity; first create on a fresh daemon took 61 s).
- `/mac-cloud` now shows what a team gets (leases and tenants, snapshots, volumes, tunnels, warm pools, metrics, SSO,
  billing, SDKs), each card tagged with what it was run against; its steps use `fluxctl fleet`. `/mac` fixes the stale
  Homebrew tab (`brew install zyvorai/tap/fluxvm`) and drops the removed capacity estimates for the measured figures.
  The home page replaces its illustrative terminal and numbers with measured ones.
- README, POSITIONING and PRODUCT_OVERVIEW gain the agent-sandbox and platform rows; a PR-time `Docs site check`
  workflow builds the site before merge.

### Fixed: `fluxctl service install` no longer starts a second daemon beside Homebrew's
- It refuses, before writing the plist, when it runs from a Homebrew install, when a `brew services` plist for `fluxvm`
  exists, or when something already accepts connections on the configured `listen` address, and says what to do
  (`brew services start zyvorai/tap/fluxvm`, or a separate `listen` and `state_dir`). Both agents defaulted to
  `127.0.0.1:7788` and the same state directory, so the second one exited with `Address already in use` after it had
  opened the state directory. `--force` overrides; a reinstall over its own loaded agent is not treated as a clash.
- New global `--listen HOST:PORT` and `--state-dir DIR` flags (and `FLUXVM_LISTEN`, `FLUXVM_STATE_DIR`) override `listen`
  and `state_dir` from the config, before or after the subcommand; `service install` writes them into the LaunchAgent, so
  a second daemon is `fluxctl service install --listen 127.0.0.1:7789 --state-dir ~/fluxvm-second`. The guard checks the
  overridden address.

### Fixed: three rough edges found installing 0.4.4 from the Homebrew tap on a clean Mac
- `fluxctl mcp install` (and `fluxctl service install`, and the ssh ProxyCommand of `fluxctl fleet ssh`) wrote the versioned
  Cellar path (`/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl`), which `brew upgrade` plus `brew cleanup` removes. They now
  write the version-free `opt` path when there is one.
- `GET /readyz` answered `503` on every Mac because it required the Linux eBPF dataplane. The dataplane is only required on
  Linux now.
- Stopping a VM on a Mac no longer logs `nftables table delete ... running nft (install nftables)`.

### Docs: install on a Mac with Homebrew
- New `docs/install-macos.md`: install, the service, a first VM, and wiring an MCP client, each tested from the published
  tap on an M4 from a fresh `HOME`. `fluxctl service install`, a real `brew upgrade` (0.4.1 to 0.4.4) and `brew
  uninstall` were run too; the page records the clash with `brew services` and that an MCP entry written by 0.4.4 or
  earlier is dead after an upgrade.

### Added: an agent-sandbox demo with real timings
- `examples/agent-sandbox-demo/local-model.sh`: a sandbox with no network card reaching a model server on the Mac's
  loopback through one vsock port. The relay was "unverified on hardware"; it is now verified on an M4 with a stand-in
  server and with a real `llama-server` model (SmolLM2-135M, a deliberately tiny model); not yet with MLX or a larger
  model.
- `examples/agent-sandbox-demo/run.sh`: an offline sandbox, a failing test, a fork into four running copies, four fixes
  tried in parallel, the passing one kept. About 4 s to fork and 0.3 s to test on an M4 mini; its README lists what it
  does not do (a NAT sandbox cannot fork; copies keep the parent's identity; the first create on a fresh daemon took 61 s).

## 0.4.4 (2026-10-12)

This is the first release since 0.4.1 with a package: the 0.4.2 and 0.4.3 tags were never published, because their macOS
package builds failed on the hosted runner (the USB helper could not import `AccessoryAccess`). Everything in 0.4.2 and
0.4.3 below ships here, together with the fleet platform and the fix.

### Fixed: the macOS package builds on hosted runners
- `scripts/package-macos.sh` leaves out `FluxVMUSBAccess.app` and its launch agent when the SDK has no `AccessoryAccess`
  framework (or with `FLUXVM_SKIP_USBD=1`) and says so; the release workflow summary notes it. USB passthrough then
  needs a source build (`docs/macos-signing.md`). Packages built where the framework exists are unchanged.


### Docs: Mac cloud operations guide, `fluxctl fleet` reference, node API and OpenAPI coverage
- New `docs/mac-cloud-operations.md`: every `fluxvm-agent central` flag, what central keeps on disk, which paths are
  public and why, a pre-launch checklist, and upgrade notes. New `docs/fluxctl-fleet.md`: all 65 `fluxctl fleet` commands,
  generated from the binary's `--help`.
- `docs/api.md`: node stats (`GET /v1/stats`), volumes (`/v1/volumes`) and tunnels (`GET /v1/vms/{id}/tunnel`), including
  which addresses a vz tunnel may reach.
- `schemas/fleet-openapi.yaml`: tunnels, metrics (`/fleet/metrics/*`, `/metrics`), pool status, members, sign-up,
  Stripe webhook, invoices and payment method, and the console sign-in routes (75 paths, 98 operations).
- Tenancy, warm-pool and billing docs now say what is not covered: viewers see screens and logs, tunnels outlive a
  sign-out, shared warm pools share credentials and a network, the sign-up email is not verified.
- `docs/fleet-quickstart.md`: the tenant commands that were run against the simulator for snapshots, volumes, power,
  logs, pools and tunnels.

### Added: vGPU (mediated devices) and GPU health
- `GET /v1/host/mdev` lists mdev parents, profiles (free instances, framebuffer size, MIG slices from MIG-backed
  profile names) and instances, with whether a VM lists or holds each. `POST /v1/host/mdev` creates an instance,
  idempotent for the same UUID, parent and type; `DELETE /v1/host/mdev/{uuid}` removes one a VM does not use. Both
  admin.
- `vfio_devices` takes mdev UUIDs next to PCI addresses; QEMU gets `vfio-pci,sysfsdev=/sys/bus/mdev/devices/<uuid>`.
  Entries that are neither, or listed twice, are refused at create.
- `GET /v1/host/gpus/health` judges each GPU from AER counters, PCIe link width and `nvidia-smi` (ECC, retired pages,
  temperature, MIG mode) when installed. `GET /v1/capabilities` reports `host.mdev` and `host.gpu_health`.
- Fixed: a VM with `vfio_devices` failed to start on the eBPF dataplane with `Could not open '/dev/vfio/vfio':
  Operation not permitted`; the VFIO container is now on its device-cgroup allowlist.
- Lab-tested from Kairon (`scripts/lab-vgpu.sh`, kernel 7.0, QEMU 10.2) with the kernel's `mbochs` sample driver:
  a VM booted with an instance by UUID, the guest saw it, and the instance was removed after stop. NVIDIA vGPU and
  MIG are unit-tested only. See `docs/vgpu.md`.



### Added: node API for volumes, tunnels, usage stats and restore (used by the fleet platform)
- `fluxvm-api`: persistent volumes (`/v1/volumes`), a TCP tunnel to a VM's port (`GET /v1/vms/{id}/tunnel?port=N`,
  WebSocket, admin only), and the vz usage and storage reports central graphs.
- Tunnels to a vz guest connect only to an address on this Mac's vmnet `bridge*` interfaces (the address is what the
  guest printed on its own serial console), never to the Mac's LAN or to an address another VM on the node reports.
  Namespace guests may use `169.254.x.y` (never the metadata address), which tunnels refused before.
- `fluxvm-scheduler`/`fluxvm-apple`: node stats, restore-from-snapshot and the runner/usage plumbing for the above.

### Added: fleet SDKs, an OpenAPI description and a Terraform provider
- `schemas/fleet-openapi.yaml`: OpenAPI 3.1 for central's fleet API (tenants, keys, leases, VMs, networks, images,
  webhooks, prices, billing, budgets, usage, events, policy), plus the volumes and pools shapes.
- Go: `github.com/zyvorai/fluxvm/go/fleet`, a typed client with context support, bearer auth, an `APIError` with
  status and central's message (`errors.Is` with `ErrNotFound`, `ErrConflict`, `ErrQuota`, ...) and retries of
  idempotent calls on 429/5xx (POSTs are never retried); `go/fleet/fleettest` is a stateful fake central for tests.
- Python `fluxvm.FleetClient` and TypeScript `FleetClient` with the same calls and retry rules (`Conflict` and
  `ServiceUnavailable` errors added).
- `terraform-provider-fluxvm`: resources `fluxvm_tenant`, `fluxvm_api_key`, `fluxvm_lease`, `fluxvm_vm`,
  `fluxvm_network`, `fluxvm_webhook`, `fluxvm_volume`, `fluxvm_pool`; data sources `fluxvm_images`, `fluxvm_nodes`,
  `fluxvm_tenant`; import by id or name. Tested over the plugin protocol against the fake central. `fluxvm_volume`
  and `fluxvm_pool` need a central with volumes and pools. See `docs/fleet-sdk.md`.


### Security: review fixes in the cloud platform round
- A sign-up owner's first SSO sign-in now rotates the tenant's primary token and clears its named keys and webhooks.
  Sign-up does not verify the email, so whoever held the sign-up token may not be that person.
- Stripe invoices are created with `auto_advance=false` and set to true when finalized, so a crash between the draft
  and its items cannot leave Stripe to finalize and charge a half-filled invoice.
- The default sign-up quota caps volumes at 100 GiB.
- Tunnels to a vz guest connect only to an address on this Mac's vmnet bridges (the address comes from the guest's own
  serial console), and never to an address another VM on the node reports. Namespace guests may use `169.254.x.y`
  (not the metadata address), which tunnels refused before.

### Added: usage graphs per VM and per Mac, `fluxctl fleet top`, and a Prometheus endpoint (fleet)
- Nodes serve `GET /v1/stats`: host CPU ticks, memory, free disk and VM counts, and per VM CPU seconds, memory and
  disk bytes (plus network bytes where the host can see them). `vz` guests are measured with `proc_pid_rusage` on the
  runner and the Virtualization XPC service that runs the guest; Linux backends from their cgroup and TAP device.
  `vz` guests have no network numbers (Virtualization.framework exposes none); every sample says where its numbers
  come from.
- Central samples every healthy Mac every 15 s (`--metrics-interval-secs`; 16 at a time, 5 s timeout, leader only)
  and keeps one-minute points for 24 hours in memory, bounded to 1000 VM and 500 Mac series; deleted VMs lose their
  series at once.
- `GET /fleet/metrics/vms/{node}/{id}`, `/fleet/metrics/nodes/{name}` (`?range=1h|6h|24h`), `/fleet/metrics/vms`,
  `/fleet/metrics/summary` and `POST /fleet/metrics/collect`. Tenants see only their own VMs and the Macs they lease.
- `GET /metrics` in the Prometheus text format: fleet, node, VM and tenant gauges and counters, leases, the HA leader,
  webhook deliveries and requests by route and status. It takes the fleet token or a scrape-only `--metrics-token`.
- Console: CPU, memory, disk and network charts with a 1h/6h/24h toggle and hover/keyboard tooltips in the VM details;
  CPU, memory and VM count in the Mac details; fleet CPU and memory tiles on the overview. Plain SVG, no library.
- `fluxctl fleet top [--sort cpu|mem]` and `fluxctl fleet metrics <vm> [--range]`. The simulated Mac reports varying
  usage. See `docs/fleet-metrics.md`.

### Added: VM snapshots and clones through the fleet
- `GET/POST /fleet/vms/{node}/{id}/snapshots`, `DELETE …/snapshots/{name}`, `POST …/snapshots/{name}/restore` and
  `POST /fleet/vms/{node}/{id}/clone`. A snapshot is an APFS clone of the disk and firmware files, plus with
  `include_memory` the guest's saved state (Virtualization.framework `saveMachineStateTo`); restore stops the VM, puts
  the snapshot back and starts it; a clone is a new VM on the same Mac from a snapshot or a stopped VM's disk, owned by
  the source's tenant and counted against its quota. Owners only (404 otherwise); read-only keys only list.
- Per-VM snapshot limit: `max_snapshots_per_vm` in the policy (default 20, 0 turns snapshots off) and per tenant in its
  quota. Optional `snapshot_gib_per_hour` price adds a "Snapshot storage" line to statements.
- Activity log targets name the snapshot; new notifications `vm.snapshot`, `vm.restored`, `vm.cloned`.
- Console: a Snapshots section in the VM details (take, list with time and size, restore and delete with confirmation,
  new VM from a snapshot) and a Clone sheet; snapshot fields in the price list, tenant quota and policy.
- `fluxctl fleet snapshot|snapshots|restore|snapshot-rm|clone`.
- Node (`fluxctl serve`): disk-only `vz` snapshots (`POST /v1/vms/{id}/snapshot {"memory": false}`, any VM state),
  which restore by booting cold; restoring a running `vz` VM stops and restarts it instead of refusing; clones from a
  snapshot (`POST /v1/vms/{id}/clone {"snapshot": …}`), and clones of macOS `vz` guests (hardware model and auxiliary
  storage copied, new machine identifier); snapshot lists say whether each has memory (`has_memory`). The simulated
  Mac (`sim-node`) supports all of it.

### Added: persistent volumes (fleet)
- Disks that keep their data when the VM using them is deleted. `POST /fleet/volumes` (`name`, `size_gib` 1-2048,
  optional `node`) makes a sparse raw file on one Mac through its `fluxctl serve` (new `/v1/volumes` routes); a launch
  with `"volumes": ["name"]` lands on that Mac and gets each volume as a virtio disk the guest finds at
  `/dev/disk/by-id/virtio-<volume id>`. Deleting the VM detaches it. Linux `vz` guests only.
- A volume in use cannot be deleted, resized or attached twice (`409`); resize only grows, while detached. Tenants see
  only their own volumes; `max_volume_gib` caps a tenant's total; the price list's `volume_gib_per_hour` bills them;
  `volume.created` and `volume.deleted` notifications.
- `fluxctl fleet volumes|volume-create|volume-rm|volume-resize` and `fleet launch --volume NAME`; a Volumes page in the
  console (create, resize, type-the-name delete) and a volume picker in Launch VM. The simulated Mac serves volumes
  too. See `docs/mac-cloud-tenancy.md#volumes`.
- Security: a tenant's launch may no longer name `apple.extra_disks` (any host file, device or NBD server on the Mac);
  tenants attach storage as volumes. On the node, `policy.allowed_image_dirs` now applies to extra disks at create too
  (the host's volumes directory is allowed), a volume file already used by another VM is refused, and a clone does not
  take its source's volumes.

### Added: SSH and port tunnels to VMs through central
- `fluxctl fleet ssh [user@]<vm> [-- ssh args]` runs the system `ssh` with a `tunnel-stdio` ProxyCommand;
  `fluxctl fleet tunnel-stdio <vm> <port>` relays stdin/stdout; `fluxctl fleet port-forward <vm> <local>:<remote>`
  opens a tunnel per local connection; `fluxctl fleet tunnels [close <id>]` lists and closes them.
- Central: `GET /fleet/vms/{node}/{id}/tunnel?port=N` (WebSocket; owner only, not read-only keys), relayed to the
  node's new `GET /v1/vms/{id}/tunnel` with the fleet token; `GET /fleet/tunnels` and `DELETE /fleet/tunnels/{id}`.
  The node connects only to the guest (usernet forward, netns, or the guest's NAT/DHCP address), never to loopback.
- Policy `tunnels`: allowed ports (default 22 and 1024-65535), idle timeout (60 minutes), open tunnels per tenant
  (20). Opening and closing, with bytes each way, are in the activity log. Catalog images take an `ssh_user`.
- Console: a Connect section in the VM details (copyable commands and an SSH config block), a Tunnels page (per tenant
  for the operator, with Close), and the tunnel rules on the Policy page. The simulated Mac echoes tunnel bytes.

### Added: warm pools, so launches (macOS CI jobs above all) start in seconds
- A pool keeps `min_ready` VMs of a catalog image booted and idle (`/fleet/pools`). A `POST /fleet/vms` that names a
  pool (`"pool"`), or whose image and size match one the caller may use, takes a ready VM instead of booting one: central
  renames it, applies the launch's TTL and answers with `"warm": true`; the pool boots a replacement. The pick is atomic
  under the tenancy lock, so concurrent launches never share a VM. Launches that need cloud-init, networks or disks boot
  as before.
- Tenant pools run on the tenant's leased Macs and count against its quota; shared (operator) pools run on Macs nobody
  leases, are in no quota, and are billed to whoever takes a VM from the moment it is taken (`billed_from` in the
  ledger, used by metering and statements). Tenants cannot take macOS VMs from shared pools while
  `require_lease_for_macos` is on, or any while `isolate_tenant_networks` is on.
- A reconciler on the HA leader (every 5 s, `FLUXVM_WARM_POOL_INTERVAL_SECS`) replaces warm VMs that fail, stall or sit
  on a Mac the pool may no longer use, trims extras, respects leases, drains, scrubs and Apple's two macOS guests per
  Mac, backs off after failures, deletes taken VMs whose TTL ran out, and logs replacements and failures as `warm-pool`.
- `GET /fleet/pools/{name}[/status]`: ready, booting, claims in the last 24 hours and average claim time. `GET
  /fleet/vms` marks warm (`warm_pool`, `warm_state`) and taken (`claimed_from`) VMs.
- `fluxctl fleet pools|pool-create|pool-set|pool-rm`; the console's Warm pools page (cards with ready/booting dots, a
  min-ready stepper, claim stats, create and launch sheets, delete with confirmation), "Warm, ready" tags in the VM
  list and "Started instantly from warm pool …" on launch. `gha-runners` pools take VMs from a warm pool with
  `"vm": {"pool": "…"}` and `launch: exec`. The simulated Mac supports `PATCH /v1/vms/{id}`. See `docs/warm-pools.md`.

### Added: console sign-in with SSO, and team members with roles (fleet)
- `fluxvm-agent central --oidc-issuer … --oidc-client-id … --public-url …` turns on OpenID Connect sign-in for the
  console: authorization code flow with PKCE, `state` bound to the browser, ID token verified against the provider's
  JWKS (RS256/384/512, PS256/384/512, ES256/384) with `iss`, `aud`/`azp`, `exp`, `nonce` and `email_verified` checks.
  Routes `GET /auth/config|login|callback|session`, `POST /auth/logout`.
- Sessions: random token stored hashed in the tenancy state (survives restarts and HA failover), sent as an `HttpOnly`,
  `SameSite=Lax` (and `Secure` over https) cookie, 12 hours by default. Changes made with the cookie need the console's
  `X-FluxVM-Console` header and a same-origin `Origin`.
- Members per tenant with roles `viewer`, `member`, `admin`, `owner`, enforced by central on every request and applied
  at once (`GET/POST /fleet/tenants/{id|me}/members`, `PATCH/DELETE …/members/{email}`). Operators sign in with the
  emails given by `--sso-operator` or the policy's `operator_emails`. An email nobody added is refused with a page.
- The activity log records the person's email (`user`) for changes made in a session, and each sign-in.
- Console: **Sign in with …**, the signed-in person and **Switch team** in the side footer, a **Members** page (add with
  a role, change a role, remove), members in a tenant's details for the operator, and actions a role cannot take hidden.
- `fluxctl fleet members | member-add <email> --role | member-role <email> <role> | member-rm <email>`.
  See `docs/mac-cloud-tenancy.md`.

### Added: self-service sign-up and Stripe invoices (fleet)
- Sign-up: with `signup_enabled` in the policy, anyone can create a tenant from the console's sign-in page or
  `fluxctl fleet signup` (`POST /signup`, no token). It gets `signup_default_quota`, its email is kept as
  `owner_email`, and the token is shown once. Checks the ID, email shape and optional allowed domains; one account per
  email; 5 sign-ups an hour per client address and 100 in all. `GET /signup/config` says whether it is open.
- Stripe (`--stripe-secret-key-file` or `STRIPE_SECRET_KEY`, `--stripe-webhook-secret-file`, `--public-url`): a
  customer per tenant, created at sign-up or when the operator turns invoicing on (`PUT /fleet/tenants/{id}/billing`).
  `POST /fleet/billing/invoices?month=YYYY-MM` (operator, `fluxctl fleet invoice-run`) turns each statement into an
  invoice, amounts in the currency's smallest unit (zero-decimal currencies such as JPY handled), with an idempotency
  key per tenant, month and step and a stored record, so a second run bills nobody twice. Optional run on the 1st
  (`billing_auto_invoice`, off by default).
- Tenants list invoices with a Pay link (`GET /fleet/billing/invoices`, `fluxctl fleet invoices`) and save a card with
  Stripe Checkout in setup mode (`POST /fleet/billing/payment-method`). `signup_require_payment_method` holds new
  tenants' leases and VM launches (`402`) until they do.
- `POST /stripe/webhook` checks `Stripe-Signature` (HMAC-SHA256, 5 minutes tolerance) and follows invoice status;
  a failed payment notifies the tenant (`billing.payment_failed`). Optional suspension of tenants with invoices unpaid
  for N days (`billing_suspend_unpaid`, off by default), lifted when paid.
- Console: "Create an account" on the sign-in page, the sign-up sheet and a token screen that cannot be dismissed
  before the token is stored; Invoices and Payment method on Billing; "Create invoices for <month>" with per-tenant
  status for the operator; owner email, Stripe customer link and invoicing switch in the tenant details; Sign-up and
  Invoices settings in Policy. See `docs/mac-cloud-billing.md`.
- Central now serves with the client address available (`ConnectInfo`), for the sign-up rate limit.

### Changed: the cloud platform features work together (fleet)
- Central's request checks run in one order: public routes (`/auth/*`, `/signup*`, `/stripe/webhook`; `GET /metrics`
  checks its own token), then the token, API key or SSO session, operator-only routes, read-only keys, the member's
  role, and the sign-up payment-method requirement, which now also covers clones, volumes and warm pools.
- SSO roles know the new routes: members take snapshots, clone, open tunnels and manage volumes; warm pools, closing
  tunnels and payment methods need an admin; viewers see usage graphs and invoices but open no tunnel. The console
  hides those actions by role too.
- A sign-up's email becomes the tenant's owner on its first SSO sign-in, while the tenant has no members yet.
- A launch with volumes never takes a warm VM; a tenant with a warm pool cannot be deleted until the pool is, and its
  console sessions end with it.
- SDK and Terraform: `fleet.Pool` reads central's `size` and `status` (`VCPUs`, `MemoryMiB`, `Ready`, `Total` are filled
  from them); `fluxvm_pool` replaces the pool when `image`, `vcpus` or `memory_mib` change and checks names of 1-40;
  `fluxvm_volume` checks 1-2048 GiB and central's name rule. Snapshots and clones are in `schemas/fleet-openapi.yaml`.

### Added: per-disk QEMU I/O, virtio-blk and vhost-user-blk data disks
- `io` on data disks (`POST /v1/vms/{id}/disks`, `data_disks`) and `root_disk_io` on create: `bus` (`scsi`,
  `virtio-blk`, `vhost-user-blk`), `aio` (`io_uring`, `native`, `threads`), `cache`, `iothread` and `queues`.
  `fluxctl disk attach --bus --aio --cache --iothread --queues`. Stored per disk in `disks/<name>.io.json`.
- Live virtio disks hot-add onto root ports reserved at create (`disk_hotplug_ports`, default 4, at most 16). VMs
  created earlier keep their device layout.
- A `vhost-user-blk` disk is served by its own `qemu-storage-daemon`, ended with the VM. Live resize and
  hibernation refuse these disks; live and CBT backups leave them out.
- `GET /v1/capabilities` reports `host.io_uring`, `host.vhost_user_blk` and `host.nvme_multipath`. The VMM seccomp
  filter allows io_uring.
- Disks on host block devices (an NVMe-oF namespace, an RBD map) are put on the VM's device-cgroup allowlist at
  boot and before a live attach. Before, QEMU got `Operation not permitted` opening them on the eBPF dataplane. An
  attach that would overflow the 8-entry allowlist is refused.
- Lab-tested from Kairon (`scripts/lab-nvmeof.sh`, kernel 7.0, QEMU 10.2): booted a VM from an nvme-tcp namespace
  in place, hot-added a virtio-blk disk (io_uring, cache none, iothread, 2 queues) and a vhost-user-blk disk (2
  queues), and kept it running through a path loss.

### Added: control VMs from the fleet console and `fluxctl fleet`
- Power: Start, Stop, Restart, Pause and Resume on a VM through central (`POST /fleet/vms/{node}/{id}/{start|stop|
  restart|pause|resume}`), in the VM details and as `fluxctl fleet start|stop|restart|pause|resume <vm>`. Stop and
  Restart ask for confirmation in the console.
- Interactive screen: "Control" in the Screen panel sends clicks, scrolling and typing to the guest
  (`POST /fleet/vms/{node}/{id}/input`, 1-200 actions per call). Keystrokes are not written to the activity log.
- Console log: `GET /fleet/vms/{node}/{id}/logs` (capped at 4 MiB), in the VM details and `fluxctl fleet logs <vm>`.
- Same rules as the rest of `/fleet`: a tenant reaches only its own VMs (404 otherwise) and read-only API keys can read
  logs but not control anything. The simulated Mac (`sim-node`) powers VMs, draws typed text and has a log.

### Added: prices, statements, budgets, notifications and signed webhooks (fleet)
- The operator sets a price list (Mac classes, VM time on shared hosts); central turns metered use into monthly
  statements per tenant (`GET /fleet/billing`, CSV with spreadsheet-formula guard, forecast for the current month) and
  tenants set a monthly budget that warns at 80% and when over. A lease keeps the price it started with.
- Central records notifications (`vm.*`, `lease.*`, `budget.*`, operator-only `node.*`/`drain.done`) and sends them to
  webhooks signed with HMAC-SHA256 (`X-FluxVM-Signature`), retried three times. Tenant webhooks must be `https` and
  reach a public address, pinned at connect time; operator webhooks may go anywhere.
- `fluxctl fleet prices|bill|budget|notifications|webhooks`, and console pages for each. See `docs/mac-cloud-tenancy.md`.

### Fixed: an operator's forced lease release did not notify the tenant
- `lease.released` took its tenant from the caller, so a release by the operator had none and reached neither the
  tenant's bell nor its webhooks. It now takes the lease's tenant.


### Fixed: incident bundle directories were world-readable
- `diagnostics_dir` and its per-VM and per-bundle directories are now created, and tightened if they exist, with mode
  0700. They inherited the umask (0755), so on a multi-user host other local users could read bundles and guest memory
  dumps. Incident bundles are now also in the README feature table, `docs/index.md` and `docs/operations.md`.

## 0.4.3 (2026-10-12)


### Added: incident bundles
- `POST /v1/vms/{id}/diagnostics` starts a job that collects the VM record, cgroup and PSI files, the console log,
  QMP state (status, CPUs, memory, block, migration, PCI, chardevs and more), dataplane status, flows, drops, tc and
  BPF state of the VM's interface, host memory and info, and optionally a packet capture into
  `<diagnostics_dir>/<id>/<token>/bundle.tar.gz`. `GET .../diagnostics/{token}` reports the job with per-section
  results; `.../bundle` downloads it. `fluxctl incident`.
- `memory_dump: true` writes a QEMU `dump-guest-memory` ELF next to the bundle (guest paused while it is written),
  reports its size, sha256 and pause time, and serves it at `.../memory-dump`. Limited by
  `diagnostics_max_memory_dump_mib` (default 16384) and free space.
- Secrets are fingerprinted by default (`redact: false` keeps them). The last five bundles per VM are kept. See
  `docs/diagnostics-bundle.md`.

### Added: named API keys per tenant
- A tenant can hold up to 50 named keys besides its primary token (`GET/POST /fleet/tenants/me/keys`,
  `DELETE .../keys/{id|name}`; `fluxctl fleet keys`; an API keys page in the console). Each has a name, an optional
  expiry and a read-only flag, is shown once and stored only as a hash, and is revoked on its own. The activity log
  names the key behind each action. Only the primary token (or the operator) creates and revokes keys, so a leaked CI
  key cannot mint more; a read-only key is refused every change with 403.
  Tried against the three-Mac simulation (read-only, expiry-free and revoked keys, key-minting-key and
  `/fleet/register` attempts all refused as intended); not on real Macs.


### Added: live VM screen and an activity log, in the console and `fluxctl fleet`
- `GET /fleet/vms/{node}/{id}/screen` returns a picture of a VM's display through central (the owner or the operator
  only, image types only, never cached), shown live in the console's VM panel and saved by `fluxctl fleet screen`.
- `GET /fleet/events` is an activity log of every change made through central: who, what was created or deleted, and
  the result, including refused attempts at operator routes. A tenant sees only its own; the operator can filter by
  `tenant`. Request bodies are not stored (only the name of what was created), command text is never recorded, and
  page views are not logged. The log is a file in central's state directory (`fleet-audit.jsonl`).
  Tried against the three-Mac simulation, with two tenants for the isolation checks; not on real Macs.

### Changed: console pickers, networks and usage
- Image and network choices are larger cards with a selected mark, checkboxes are drawn properly (they rendered as a
  dark block), Macs on a network are chips, and Usage shows where VM hours went (macOS or Linux), two decimals under an
  hour so a short run is not `0.0`, and a note that a lease in progress adds its 24-hour minimum when released.


### Added: `fluxctl fleet` for tenants and operators
- `fluxctl fleet status` (a tenant's quota, Macs and next step, or the operator's fleet health), `launch --image` from the
  catalog, `run <vm> -- cmd`, `rm`, `images` and `networks` (list, show, create, delete, rotate, add-mac, remove-mac),
  with human output on a terminal and JSON when piped. VMs are found by name or ID prefix. Point it at central with
  `--central`/`CENTRAL_URL` and `--token`/`FLUXVM_AGENT_TOKEN` ([docs/mac-cloud-tenancy.md](docs/mac-cloud-tenancy.md)).
  Tried against the three-Mac simulation (`scripts/fleet-sim.sh`) as tenant and operator, not against real Macs.

### Fixed: a Mac's heartbeat reported zero macOS guests
- `fluxvm-agent node` counted macOS guests from a `state` field that `GET /v1/vms` does not return (it returns `status`),
  and looked for a `starting` value that does not exist (`creating` does), so a real Mac always reported 0 and central's
  two-guest placement limit never applied to real nodes. The simulator emitted both keys, which hid it. The count now reads
  `status` (`creating`, `running`, `paused`).


### Changed: console visual polish
- Status shows as a pill, the lease ring shows the hours left in the 24-hour minimum (it read as an empty disc at the
  start), each Mac card carries vCPU and memory use bars, tiles lift on hover (not with Reduce Motion), empty states are
  calmer, VMs without a catalog image show their disk name, and the theme button says what it is ("Theme: Auto").
- Fixed: "Healthy Macs 3 of 3" drew its full bar red, as if over quota.

### Added: `scripts/bench-macos-pool.sh`
- Times macOS warm-pool checkout and return over N rounds (min/median/max) and samples runner RSS, against a local
  `fluxctl serve` on a Mac. Checked against a stub server only; not yet run against real guests.



### Added: fleet console as a cloud UX, image catalog, tenant view of leased Macs
- The `/console` web console is rebuilt as a cloud UX: overview with quota bars and your Macs, My Macs, virtual machines,
  networks, images to launch from, usage, search and a command bar ([docs/mac-cloud-tenancy.md](docs/mac-cloud-tenancy.md#console)).
- `GET/PUT/DELETE /fleet/images[/{name}]`: an operator-managed image catalog (tenants read, the operator writes).
  `GET /fleet/nodes/{name}` for a tenant returns only a Mac it leases, and only size, free capacity and Apple
  capabilities. The operator tenant list carries each tenant's current use. A tenant's unreachable-node list is limited
  to its own Macs and carries no error text.
- Known limit: the catalog returns each image's disk path on the Macs to tenants, because the console puts it in the
  create request; resolving the name on the server would remove that.

### Added: grow a stopped vz VM's root disk
- `PATCH /v1/vms/{id}/disks/root` (`fluxctl disk resize <id> root --size-gib N`) now works on the `vz` backend for macOS
  and Linux guests: the VM must be stopped, the size is an absolute grow-only target, and `apple.asif_overlay` VMs are
  refused. It extends the sparse file only; the guest still has to grow its own partition or APFS container
  (`diskutil apfs resizeContainer`), which is not automated and not verified on hardware ([docs/macos.md](docs/macos.md)).
- A third macOS guest is refused at create and at start with 409 (Apple allows two per Mac); this used to fail late
  with a raw VZ error.

### Added: simulated Macs and a one-command fleet demo
- `fluxvm-agent sim-node` registers a simulated Mac with central (capacity, heartbeats, simulated macOS guests, overlay
  config), and `scripts/fleet-sim.sh` runs a central plus three of them: a tenant leases a Mac, starts two macOS VMs
  (a third is refused at Apple's two-guest limit), runs a command and creates a private network

pple's two-guest limit), runs a command and creates a private network

  ([docs/fleet-quickstart.md](docs/fleet-quickstart.md)). No Macs or real guests are involved, so it exercises central,
  tenancy, scheduling, overlay allocation and the console, not virtualization.

### Added: fleet web console, overlay key rotation, HA lock release
- `GET /console` on central is a web console for the fleet: a tenant token shows your leases, VMs, private networks and
  usage (lease or release a Mac, delete VMs, create networks); the fleet token adds nodes (cordon, drain, scrub),
  tenants and every tenant's usage ([docs/mac-cloud-tenancy.md](docs/mac-cloud-tenancy.md#console)).
- Cross-Mac overlay keys have epochs. `POST /fleet/overlays/{name}/rotate` issues a new key and the reconciler rotates
  keys older than `overlay_key_rotation_days` (default 30, `0` disables); the previous key stays valid until the next
  rotation, and the switch reloads keys without restarting. The epoch is authenticated, and replays are checked at send
  time ([docs/cross-mac-networks.md](docs/cross-mac-networks.md#key-rotation)).
- A leader central releases the HA lock on Ctrl-C or SIGTERM, so a standby takes over at once instead of after the TTL.
  Not verified across two Macs.


### Added: `POST /v1/branches/prune` (`fluxctl branch prune`)
- Remove branches by policy: keep the newest N, older than a cutoff, scoped to a VM or a name prefix. A dry run unless
  `dry_run: false` (`--yes`); an empty policy is refused; a branch labelled `fluxvm.dev/keep: "true"` is never removed.
  Verified on an M4 (dry run, keep label, real prune, CLI).

### Added: `branch diff`, `branch export`, `branch import`
- `GET /v1/branches/{a}/diff/{b}` compares two branches (sizes, lineage, labels). `POST /v1/branches/{id}/export` writes
  a branch to an owner-only bundle directory and `POST /v1/branches/import` makes a new branch from one (both admin).
  The bundle carries the agent token like the disk; a saved state restores only on a Mac of the same model and a
  compatible macOS. Round trip covered by unit tests; not tried across two Macs.

### Fixed
- Raw base images are cloned with the macOS-safe copy on every backend (`cp --reflink` is GNU-only).



### Added: virtual networks (eBPF routing, Geneve overlay, SNAT, floating IPs)
- A `tap` NIC with `vnet` (`{vni, ip, prefix_len, gateway}`) joins a VNI-isolated IPv4 network routed entirely in eBPF:
  no bridge, no per-network device, no iptables. Same-host guests are routed tap to tap; other hosts are reached through
  one collect-metadata Geneve device (`fvgnv0`, UDP 6081). The gateway exists on every host and answers ARP and ping.
  Guests can only send from their own address. The guest gets its address, gateway and MTU 1450 through cloud-init.
- `PUT /v1/network/vnets/{vni}` applies a network (CIDR, gateway, endpoints on every host, uplink, SNAT address,
  floating IPs); `GET` lists and `DELETE` removes it (refused while a VM here is attached). Capability `host.vnet`.
- Egress SNAT to the uplink address (ports 61000-65534, LRU flow table, no TCP state machine) and 1:1 floating IPs. The
  uplink program runs first in the TCX chain (`fluxvm-tcx attach-first`, ahead of Cilium's `cil_from_netdev`), passes
  everything else untouched, and is detached with the last network that uses it.
- IPv4 only, no TTL decrement, no load balancing inside a network, no VM-edge policy on vnet VMs, NIC hotplug refused.
  The root-gated `vnet_netns` test covers two netns-simulated hosts over Geneve; same-host routing, isolation and SNAT
  also ran with real QEMU guests through Kairon on one node. Cross-host on real hardware has not run. See
  `docs/virtual-network.md`.


## 0.4.2 (2026-10-12)

### Added: cross-Mac private networks (encrypted UDP overlay)
- One `apple.networks` private network can span several Macs: the `fluxvm-vz-switch` of the same network on each Mac
  forms a full mesh and carries Ethernet frames over UDP sealed with AES-256-GCM under the network's key (replay window,
  authenticated peers only, MAC learning with split horizon, no MAC spoofing). Central allocates the subnet, key and peers
  per tenant and node agents sync them into a per-Mac `overlay.json`. Use an `mtu` of at most 1440 on a 1500-byte
  underlay (60 bytes of overhead). See `docs/cross-mac-networks.md`. Unit-tested; not verified between two real Macs.

### Added: highly available central (`fluxvm-agent central --ha-redis`)
- Several centrals can share one Redis (`--ha-redis`, `--advertise-url`, `--ha-ttl-secs`, `--ha-cluster`). A Lua-scripted
  leader lock with a TTL is renewed every third of it; a standby takes over once a dead leader's lock lapses. Leadership is
  a lease that ends before the lock does, so a hung Redis or a starved task cannot leave an ex-leader serving. Tenancy and
  the node registry are mirrored, every write is fenced by the lock in the same Lua step, and a new leader loads the
  mirrored state before it serves. Standbys answer 503 naming the leader; `GET /fleet/ha` and `/fleet/ha/leader` need no
  token. `fluxvm-agent node --central a,b` fails over between centrals. See `docs/fleet-ha.md`.
- Tested with an in-memory store and, with `FLUXVM_TEST_REDIS` set, against real Redis (lock, fencing, hand-over). Redis
  Cluster and a multi-central failover under load are not verified.

### Added: Developer ID signing, notarization and Tart image import
- `scripts/package-macos.sh` signs with a Developer ID identity and notarizes when the signing secrets are present, and
  ships the runner as `FluxVMRunner.app` when a provisioning profile for `com.apple.vm.networking` is supplied
  (`docs/macos-signing.md`). Without the secrets the package is ad hoc signed as before. Not yet exercised with real
  certificates.
- `fluxctl vm-image pull` accepts Tart images and converts them into FluxVM bundles.

### Added: Mac cloud (tenants, 24 h host leases, OCI VM images, GitHub Actions runners, drain, metering)
- `fluxvm-agent central` can run as a shared Mac cloud: tenants with hashed tokens, quotas and route allow-lists; whole-Mac
  leases of at least 24 h (the macOS licence's rule); a VM ledger; drain (`cordon`, `evict-ephemeral`, `reschedule`);
  scrub before reuse; `GET /fleet/usage` metering (JSON or CSV). `fluxctl fleet tenant|lease|drain|usage|...`.
  See `docs/mac-cloud-tenancy.md`.
- VM images as OCI artifacts (`fluxctl vm-image push|pull`): zstd chunks, zero chunks skipped, sparse atomic pulls with
  every digest checked, per-VM identity never pushed (`docs/vm-images.md`).
- `fluxvm-agent gha-runners`: one fresh VM per queued job through JIT runner configs, deleted after the job; GitHub lists
  are paginated and an optional `workflow_job` webhook starts and retires runners at once
  (`docs/github-actions-runners.md`).
- A create's quota is reserved from the check until the node answers, so parallel creates by one tenant cannot overshoot;
  with tenants present an unknown token is refused even without `--token`. `policy.isolate_tenant_networks` (off by
  default) puts each tenant's `vz` VMs on their own vmnet network.
- Two Macs, GHCR pulls, per-tenant vmnet and webhook mode were not verified on hardware.

### Added: changed-block-tracking backups to S3 (`POST /v1/vms/{id}/cbt-backup`, `cbt-restore`, `/v1/cbt/synthesize`)
- Incremental QEMU backups with persistent dirty-bitmap checkpoints into zstd qcow2 layers uploaded to an S3 object store
  (optionally AES-256-GCM encrypted, Object Lock honoured). The parent checkpoint is dropped only after the upload, so a
  failed run loses no writes. Restore stacks the chain; synthesize consolidates a chain into a synthetic full so retention
  can delete old layers. Jobs persist and report failed after a restart. Verified against QEMU 10.2.

### Added: balloon, host memory pressure, NUMA and PCI topology (QEMU)
- `CreateVmRequest.balloon` adds a virtio-balloon with free-page reporting; `POST /v1/vms/{id}/balloon` and
  `GET /v1/vms/{id}/memory` cover QEMU VMs; `GET /v1/host/memory` reports MemAvailable and memory PSI.
- `GET /v1/host/topology`: NUMA nodes (CPUs, memory, hugepages, distances) and placeable PCI functions with IOMMU groups.
  `CreateVmRequest.numa` binds guest nodes to host nodes and pins vCPU threads; `POST /v1/host/pci/bind|release`.

### Added: `GET /v1/host/storage`
- Where disk space goes: the state volume's free space and, per VM, per branch and for the image cache, allocated and
  logical bytes (sparse files count what they occupy). Allocated bytes are upper bounds, since copy-on-write clones share
  blocks; free space is what is really left. Used by the Velora Control Centre's Storage pane.

### Fixed: fork and branch-start quotas are checked as one atomic batch
- A fork of N children was priced as N separate checks of one VM against an unchanged ledger (the API loop) for **API
  token quotas**, and host and tenant checks read the ledger without holding anything, so two forks racing each other (or
  a fork and a create) could both be admitted into room that only one fits. The batch is now admitted and reserved in one
  step under the store lock (`QuotaLedger::reserve_batch`): host, tenant and token limits are priced for all N children,
  creates and other forks see the reservation, and it is released when the children are recorded or the fork failed. A
  reservation left by a crash is cleared when the daemon starts. `quota.deny` events for a refused fork carry
  `scope=fork`, the machine and the count. The fork and branch-start plans use the same check without reserving.
  Verified on an M4: with `max_memory_mib_host = 7000` and a 2 GiB parent, two simultaneous `count: 2` forks admitted
  exactly one (6 144 MiB) and refused the other whole.

### Added: live egress policy for `apple.usernet` guests (`PUT /v1/vms/{id}/usernet/policy`)
- Change a running usernet guest's allow/deny CIDRs, ports and DNS names without restarting it: the daemon validates the
  change like create, stores it in the VM record, rewrites the gateway's config and signals it (SIGHUP); new connections and
  DNS queries follow the new policy, open ones are not cut. `GET` returns the stored policy. A stale pid file never
  causes a signal to an unrelated process (the pid is checked to be `fluxvm-usernet`).

### Added: jumbo MTU on private networks (`apple.networks[].mtu`)
- `mtu` (1280 to 9000, default 1500) is a property of the network: the first member defines it, later members inherit it or
  must agree. The runner raises the card's `maximumTransmissionUnit`, the guest gets the same MTU through its networkd unit,
  fallback script and seed network-config, and `fluxvm-vz-switch` accepts frames up to 9216 bytes (it dropped anything above
  1600). `GET /v1/vznets` reports `mtu`. Verified on an M4: two guests at MTU 9000, 8972-byte DF ping 3/3. Container sandboxes
  (`oci.networks`) stay at 1500.

### Added: Control Centre endpoints
- `GET /v1/host/overview` and `GET /v1/host/apple/capabilities` (see docs/api.md), consumed by Velora's Control Centre.
- `GET /v1/host/history`: an in-memory hour of host memory and per-VM CPU/RSS at 5 s spacing, so a dashboard that connects
  late has its sparklines at once. Empty after a daemon restart.
- `POST /v1/vms/{id}/fork/plan` and `POST /v1/branches/{id}/start/plan`: refusals (with the real reason) and a memory/time
  bound for a fork or branch start, without doing it.
- The `vm.fork` and `vm.branch.start` events now carry `total_ms` and a per-child `report` (prepare, restore, identity,
  hooks, ready), so a fork can be replayed later from the event log.

## 0.4.1 (2026-10-11)

### Added: Flux Branch (`fluxctl branch`, `/v1/vms/{id}/branches`, `/v1/branches`)
- Named, durable checkpoints of a running vz Linux guest (state and disk as APFS clones, stored outside the VM's workspace),
  startable any number of times with fork's identity reset, token rotation, parallel restore, hooks and readiness. They
  survive the source VM's deletion and a daemon restart, and a VM started from a branch can be branched (`parent_branch`).
  Verified on an M4: create 0.38 s, start of 2 VMs 1.9 s, checkpoint-instant file semantics. Not yet: commit, rebase,
  flatten, diff, MCP tools.

### Changed: fork v2 (`POST /v1/vms/{id}/fork`): strict identity, rotated agent token, parallel children
- **`identity` defaults to `unique`**: the fork fails and every child is deleted unless the guest confirms hostname, machine-id,
  entropy, SSH host keys, cloud-init, DHCP and network state, and the child's agent token is rotated. Before this a failed reset
  was only logged. `best-effort` keeps the old behaviour, `preserve` resets nothing. A guest with no (or an old) agent is reset
  over SSH when reachable, else refused with the missing capability named. **This is a behaviour change for callers whose guests
  lack the agent**: pass `identity: "best-effort"`.
- New request fields `identity`, `network` (`auto`, `isolated`, `none`, `preserve`) and `parallelism`; the response gains `fork`
  with per-child `prepare_ms`, `restore_ms`, identity result and `parent_snapshot_ms`. `fluxctl fork-vm --identity --network
  --parallel` and the MCP `vm_fork` tool carry them.
- vz children are restored up to `parallelism` at a time, admission is checked once for the whole batch before the parent is
  paused, and journal intents recorded from concurrent children no longer race.
- Guest agent: `ResetIdentity` gains SSH host keys, cloud-init, DHCP and network-state resets and `new_agent_token`.
- **Fixed: forked children drew identical random numbers.** All children of one fork resumed the same kernel CRNG state and the
  reset credited entropy without reseeding, so two children got the same machine-id and all of them the same new SSH host key.
  The agent now issues `RNDRESEEDCRNG`. Found by checking uniqueness from inside the guests on an M4.
- **Post-fork `hooks` and `ready`** (`running`, `agent`, `exec`, `tcp`, `http`) with `ready_timeout_seconds`: hooks run as argv
  through the agent after the identity reset, readiness is probed inside each child, and a failure deletes the whole batch.
  `fluxctl fork-vm --hook --ready --ready-timeout`.
- **Fixed: crash recovery on macOS left VMs running.** Rolling back an interrupted operation found VMM processes through
  `/proc`, which macOS does not have, so a `kill -9` of the daemon mid-fork (or mid-create) removed the record and workspace
  and left the runner and guest alive. Recovery now finds Apple runners with `ps` and always stops a fork child's runner
  before deleting its record. Found by killing the daemon on an M4.
- **Crash recovery adopts a fully finished fork** (every child at `identity-reset` and running) instead of deleting running
  work; any other interrupted fork is still rolled back whole. Fork metrics (`fluxvm_fork_*`) are exported.
- Verified on an M4: 4 strict children in 3.6 s; 5 of 5 distinct machine-ids, host keys and tokens. A NAT guest still cannot be
  forked: its state restored onto a usernet NIC leaves the NIC disconnected.

### Fixed: `apple.usernet.mtu` above 1500 had no effect
- The gateway advertised the MTU over DHCP but the file-handle attachment stayed at Virtualization's default 1500, so the guest
  kept 1500 and any jumbo frame was dropped. The runner now sets `maximumTransmissionUnit` to match. Verified on an M4: MTU 9000 in
  the guest and 8972-byte DF pings to the gateway succeed; the published 0.4.0 stays at 1500 and fails them.

### Added: `apple.nat_mtu`
- Lowers the MTU a Linux guest sets on its primary card on the Mac's NAT (576 to 1500) through the seed's network-config.
  Virtualization has no MTU property for NAT attachments, so this cannot raise it. Verified: guest MTU 1400, NAT still works.

### Changed: vmnet and bridged networks are refused up front without the networking entitlement
- A runner without `com.apple.vm.networking` boots a vmnet guest and then reports `Internal Network Error`. The runner's
  `host-capabilities` now includes `vmNetworkingEntitlement` and admission refuses `apple.vmnet` and `apple.bridge_interface`
  with the reason and the alternatives (`apple.usernet`, `apple.networks`). The vmnet broker's kill at launch is explained
  (restricted entitlement, no provisioning profile); it cannot be fixed by signing here.

### Fixed: the host could never choose the macOS guest agent
- Admission refused `agent.enabled` for every macOS guest ("fluxvm-guest-agent is a Linux binary"), and the sandbox path sets it for
  `apple.macos_vsock` guests, so a macOS agent that answered over vsock was never used. It is now accepted with `macos_vsock`
  (still refused without it). Verified on an M4: `exec` through the daemon reached the LaunchDaemon agent as root. Also verified
  on the guest: the PTY shell (with resize), `reset-identity` (hostname), `shutdown`, and the daemon starting by itself after a
  power cycle.

### Added: Homebrew install
- `brew install zyvorai/tap/fluxvm` installs the 0.4.0 Apple-silicon package (`fluxctl`, the signed runner, `fluxvm-vz-switch`,
  `fluxvm-blockd`, `fluxvm-usernet`). The first v0.4.0 build lacked `fluxvm-usernet`, which `apple.usernet` needs next to
  `fluxctl`; the tag was re-cut with it before anything was published.

### Added: fork a running `vz` Linux guest (`POST /v1/vms/{id}/fork`)
- One runner save plus APFS clones, then each child is restored from the same state with the parent's machine identifier and
  MAC. Needs `network.mode = none` or `apple.usernet` without port forwards; macOS guests, extra disks, vmnet/bridged/private
  networks are refused. A child's shared MAC is never recycled. Verified once on an M4: two children in 2.2 s, both resumed at the
  parent's counter and ran independently, the parent unaffected. Identity reset needs the guest agent (not run).

### Verified: FluxDisk encryption and the lazy HTTP base through VZ's NBD client
- On an Apple M4 with a Debian 13 guest: an encrypted overlay and a lazy overlay on an HTTP base read back the base byte-for-byte,
  took a 900 KB `O_DIRECT` write that read back identical after `drop_caches`, and the encrypted overlay file held no plaintext.
  The lazy export fetched 6 MiB before any guest read and nothing on a second read. Structured replies against VZ and
  rebase/resize under a running guest were not run.

### Verified: the macOS guest agent over vsock (`apple.macos_vsock`)
- On an Apple M4 with a macOS 27.0.1 guest: the guest binds `AF_VSOCK`, the host reaches it through the runner's `CONNECT`
  proxy, and the LaunchDaemon agent installed by `crates/fluxvm-guest-agent/macos/install.sh` answers `ping`, enforces its
  token, runs `exec` as root and round-trips files; exec policies are refused as Linux-only. Not run: the PTY shell,
  `shutdown`, `reset-identity`, a reboot with the daemon installed, and `vz_guest` choosing the agent. Still experimental.
- Packaging: `fluxvm-blockd` is now in the macOS package and the Homebrew formula (the daemon looks for it next to `fluxctl`).

## 0.4.0 (2026-10-11)

### Added: native macOS-first VZ base (FluxDisk, usernet, netobs, diskinject, macOS guest agent)
Run on an Apple M4 (macOS 27.x) with a Debian 13 guest; the results, and what was not run, are in the "Hardware verification
log" of `docs/macos.md`. Verified: offline token injection, FluxDisk `overlay` against VZ's NBD client (persistence, byte-identical
base, branch, read-only frozen overlay), usernet (DHCP, ARP, DNS and deny, ICMP, IPv6 SLAAC, TCP, forwards, egress policy,
throughput), netobs through usernet and through the vz-switch bridge (byte-exact on a two-guest iperf3), attestation, loopback
`tcp` vsock services, and the vsock transport to a macOS 27 guest. Not run on hardware: the macOS LaunchDaemon agent, blockd
encryption / lazy HTTP base / structured replies on VZ, many-flow usernet, fast fork on vz, and guests with an overlay on macOS.
- `fluxvm-blockd` (FluxDisk): pure-Rust NBD server for VZ's NBD attachment. Copy-on-write overlays with instant `branch`,
  `rebase`, grow-only `resize`, AES-256-XTS at rest, lazy HTTP range base with a persisted block cache, structured replies and
  parallel requests, IOPS/bandwidth limiter. New `apple.extra_disks` kind `overlay`, served by a supervised blockd per VM.
- `fluxvm-usernet`: userspace NAT gateway over `VZFileHandleNetworkDeviceAttachment` (`apple.usernet`): ARP, DHCPv4, DNS
  (UDP and TCP) with allow/deny, TCP (window scaling, SACK, NewReno) and UDP NAT, outbound ICMP echo, fragmentation, opt-in
  IPv6, egress CIDR/port/domain policy, host-to-guest TCP/UDP forwards, flow-event stream.
- `fluxvm-netobs` and `/v1/netobs/{summary,flows,flows/stream,dns,history,capture,capture.pcap}`: bounded flow table, TCP
  health, DNS/TLS (SNI, JA3/JA4, ServerHello)/QUIC Initial/HTTP-2/h2c metadata with redaction, RED rollups, capped pcap ring
  (off by default, admin-only). Fed by the usernet frame tap and by `fluxvm-vz-switch` through a bridge socket.
  `FLUXVM_NETOBS=off` disables it.
- `fluxvm-diskinject`: offline ext4 writer for raw Linux guest disks on macOS; the vz agent token is injected before launch
  with a cloud-init fallback.
- `apple.macos_vsock` (experimental): vsock device for macOS guests plus a macOS-buildable `fluxvm-guest-agent` and LaunchDaemon;
  SSH remains the fallback. The vsock transport is verified on a macOS 27.0.1 guest; the LaunchDaemon agent has not been run.
- `apple.root_caching` / `apple.root_sync`, runner control preconditions, display resize, per-feature minimum macOS in refusals,
  loopback-only `tcp` vsock services (local model server), `fluxvm_vm_runner_*` metrics, a signed boot manifest in the
  `metadata` vsock service (a host claim, not a hardware root of trust).
- Not done: NAT MTU (not in the SDK for NAT attachments), fast fork on vz (needs hardware).

### Fixed during hardware verification
- A stopped VM's pause, resume and balloon now answer 409 "start it first" instead of a runner error.
- VM accounting attributes the `com.apple.Virtualization.VirtualMachine.xpc` service process to its runner.
- usernet: TCP was sent to the broadcast MAC when the guest's MAC was pinned; upload is paced to the host queue (3-4 MB/s with
  2100 retransmits became about 9 MB/s with none).
- netobs: every frame lost between a switch and the observer is counted (`bridge_dropped{reason}`, `switch_forward_dropped`),
  flows are oriented by the handshake, and a restarted daemon binds its bridge again for a switch that outlived it.
- Guests with `apple.networks` plus cloud-init took 2 minutes to boot: private cards are now static entries in the seed's
  `network-config`, so `systemd-networkd-wait-online` no longer waits on them.

### Verified: `apple.asif_overlay` on a Linux guest
- `scripts/vz-asif-overlay-live-test.sh`: on macOS 27.2 a 64 MiB guest write grows `disk-overlay.asif` and leaves the base `root.raw`
  byte-identical. The docs no longer list the overlay as unverified (macOS guests with an overlay were not run).

### Added: `apple.usb_controllers` and `usb_bus` (several USB buses on a `vz` guest)
- `"apple": {"usb_controllers": 2}` (or `fluxctl create --usb-controllers 2`; 1-4, implies `usb_controller`) gives the guest that
  many XHCI controllers. A USB disk hot-attached with `"usb_bus": N` (`fluxctl disk attach --usb-bus N`) joins bus N (0-based);
  detach now finds the device on whichever controller holds it (it only looked at the first before). Disks present at boot are
  placed by Virtualization.framework. Verified on Debian 13 with `scripts/vz-usb-buses-live-test.sh`: two disks hot-attached on
  different buses are both visible and sit on different USB buses; bus 2 of 2 and 9 controllers are refused. The guest lists one
  more XHCI controller than configured (3 for 2). Physical USB passthrough still uses bus 0.

### Added: `apple.vsock_services` (guest→host vsock beyond the egress proxy and self-control)
- `"apple": {"vsock_services": [{"port": 5001, "builtin": "metadata"}, {"port": 5002, "builtin": "telemetry"},
  {"port": 5003, "socket": "echo"}]}` (or `fluxctl create --vsock-service 5001=metadata --vsock-service 5003=socket:echo`).
  `metadata` returns one JSON line (id, name, vcpus, memory_mib) and closes; `telemetry` appends the guest's bytes to
  `<workspace>/telemetry.log` (8 MiB cap, then dropped); `socket: NAME` relays to `/tmp/fluxvm-<uid>/<vm id>.svc-NAME`, a socket
  the host operator runs, so a spec can never name an arbitrary host path. Only listed ports answer; 22, 3128, 7790 and 7791 and
  ports below 1024 are refused, at most 16 entries. Verified on a Debian 13 guest with `scripts/vz-vsock-services-live-test.sh`
  (all three kinds, an unlisted port stays closed).

### Fixed: `POST /v1/vms/{id}/clone` on the `vz` backend
- A `vz` clone is now an APFS copy (`cp -c`) of the stopped VM's raw disk instead of a `qemu-img convert` to qcow2 (which the
  clone's create then converted back to raw). It is instant and no longer needs `qemu-img` on the Mac, where it is usually not
  installed. A cloud-init hostname equal to the source's name becomes the clone's name. macOS guests are refused with a pointer
  to template clones. Verified on an Apple M4 (macOS 27.2) through Velora's `--selftest fluxvm`: the clone boots on `vz`, keeps
  the source's files and answers to its own hostname.

### Fixed: `GET /v1/vms/{id}/logs?follow=true` stalls in URLSession clients
- The stream now sends `X-Content-Type-Options: nosniff` and `Cache-Control: no-cache`. Without them URLSession held back the first
  512 bytes of the `text/plain` body for content sniffing, so a quiet console showed nothing in Swift clients.

### Added: `image: "macos"` installs from Apple's newest restore image
- With `apple.install`, the image name `macos` (in `image` or `apple.media`) is resolved through the runner's new `latest-ipsw`
  command (`VZMacOSRestoreImage.fetchLatestSupported`), downloaded once over HTTPS and cached as `images/macos-<build>.ipsw`.
  `GET|POST /v1/host/apple/ipsw` and `fluxctl vz ipsw [--download]` show or pre-fetch it. Only the metadata lookup was run on
  hardware (macOS 27.0.1, build 26A434); a download attempt was stopped at 21.9 of 26.6 GB because the 23 GiB USB drive it was writing to could not hold the rest.
  The IPSW is 26.6 GB (not ~15 GB); the download now checks free space first and resumes a partial file.

### Added: `fluxctl create` flags for `apple.*`
- `fluxctl create --name N --image I` builds a `vz` request without a spec file, with `--guest`, `--install`, `--display
  WxH[@PPI]`, `--displays`, `--window`, `--rosetta`, `--clipboard`, `--microphone`, `--mute`, `--nested-virtualization`,
  `--usb-controller`, `--asif-overlay`, `--recovery`, `--bridge` and `--provision-*`; flags also override a `--spec` file.
  MCP `vm_create` now lists the `apple.*` fields.

### Added: runner-only `vz` device state reachable over REST, CLI and MCP
- `GET /v1/host/apple` and `GET /v1/vms/{id}/vz/{secure-boot,custom-virtio,usb,usb/physical}`, plus
  `POST .../vz/custom-virtio/reset` and `POST .../vz/usb/physical {"registry_id"}` (admin), with `fluxctl vz
  host|secure-boot|custom-virtio|usb` and read-only MCP `host_apple_capabilities` and `vm_vz_status`. These runner commands
  existed but had no route.
- The capability matrix no longer says data disks are "not implemented" (use `apple.extra_disks`) or that vmnet is limited to
  one network per VM; `disk attach` logs when a running `vz` VM will only see the disk at its next boot.

### Added: agents drive a `vz` VM's screen (PR #214)
- `GET /v1/vms/{id}/screenshot` (PNG, scaled with `?max_width`) and `POST /v1/vms/{id}/input` (type, key with modifiers, move,
  click, double/right/middle click, drag, scroll; batches of up to 100 validated before anything is sent), with `fluxctl
  screenshot`/`input` and MCP `vm_screenshot` (image content, read-only) and `vm_input` (write). Works for Linux and macOS
  guests without a console window or host Screen Recording/Accessibility permission. Verified on an Apple M4 (macOS 27.2) with a
  Debian 13 guest: console login by typing, Ctrl+C, and clicks, drag and scroll checked with evdev in the guest, before and after
  a reboot and a stop/start.

### Added: stored guest sign-in in the Keychain (PR #215)
- `/v1/vms/{id}/signin` (`PUT`/`GET`/`DELETE`/`POST`), `fluxctl signin set|status|clear|type` and MCP `vm_sign_in` keep a
  guest username and password in the host's login Keychain and type them into the VM's login screen (`password`, `username`
  or `username_tab` mode), so an agent never sees the password. Deleting the VM removes the item. Verified with a Debian 13
  tty1 login.

### Added: more named images, and qcow2 without qemu-img (PR #218)
- `ubuntu-26.04`, `fedora-44`, `centos-stream-10`, `almalinux-10`, `rocky-10` and `kali` (rolling), each booted and reached
  over ssh on `vz`. Builtin file names may use a `*` for versioned releases; BSD-style checksum lists are read; downloads send
  a User-Agent (cloud.centos.org refused requests without one).
- qcow2 v2/v3 is converted to raw natively (deflate or zstd clusters, zero clusters left as holes); qemu-img is only needed for
  backing files or encryption. Tarball images are extracted sparse (Kali's 25 GiB disk: 12 GB to 5.6 GB on disk).

### Added: `fluxctl mcp install` (PR #219)
- `fluxctl mcp install|uninstall|status` registers FluxVM's MCP server with Claude Code, Cursor, Claude Desktop, Codex, VS Code,
  Windsurf or Gemini CLI by editing that client's config (user-wide or `--project`), keeping other entries and Codex's TOML
  comments. Checked with `claude mcp list` (connected), `codex mcp list` and Cursor's agent.

### Added: guest self-control (PR #220)
- With `apple.self_control` (Linux guests), software inside the VM gets an MCP server at `http://127.0.0.1:7790/mcp` to
  snapshot, restore and restart that VM only (`self_info`, `self_snapshot_list`, `self_snapshot`, `self_snapshot_restore`,
  `self_snapshot_delete`, `self_restart`). The guest reaches it over vsock and the host names the VM, so no network, token or
  host port is involved. Verified on an M4 with a Debian 13 guest: a restore brought back a file's old content with the same boot
  id, and a restart gave a new one.

### Changed: website
- A Mac cloud page and illustration, a refreshed `/mac` page, an Open Graph image, and an apple.com-style home page (PRs #216
  and #217).

### Fixed: `secure_boot` on `vz`
- `secure_boot: true` was rejected for the `vz` backend by the scheduler's backend check ("secure_boot requires backend qemu"), so the
  Apple EFI Secure Boot support could not be used through the API. `vz` is now allowed (a TPM is still refused). Checked on an
  Apple M4: a Debian 13 cloud image boots with Secure Boot on (`SecureBoot=1`, `SetupMode=0` in the guest) and the node heartbeat
  reports `secure_boot` and `rosetta` host capabilities. `secure-boot-status` cannot read the store while the VM runs.

### Fixed: macOS 27 follow-ups after hardware testing (PR #200)
- Custom Virtio device id is now `0x3F` (PCI `1af4:107f`). As merged (`0xFF00`) the PCI device id fell outside the range Linux
  binds (`0x1040..0x107f`), so a guest saw the device but never attached a driver.
- `guest/virtio-flux/virtio_flux.c` builds on Linux 6.12 again (missing `virtio_config.h` include, `no_llseek`,
  `virtio_find_vqs` taking `struct virtqueue_info`, `virtio_set_drvdata`).
- `crates/fluxvm-apple/tests/vz27_source_contract.rs` was unformatted and failed `scripts/ci-fmt-check.sh`.

### Added: complete macOS 27 host path (PRs #198 and #199)
- **Custom Virtio host provider and guest bus**: `apple.custom_virtio: true` adds a vendor Virtio device. Queue 0 is a bounded
  versioned JSON control channel (`ping`, `echo`, `capabilities`, `stats`, `map-probe`); queue 1 is a bulk queue
  (`bulk_zero`/`fill`/`copy`/`crc32`). The guest driver is in `guest/virtio-flux` (`/dev/fluxvm`, `/dev/fluxvm-bulk`,
  `fluxvm_virtioctl`). Verified on an Apple M4 with a Debian 13 guest (after #200): `ping`, `echo`, `stats` and `capabilities`.
  **Bulk queue:** `fluxvm_virtioctl bulk-test` hung because the driver gave the device a kernel-stack request buffer; it now uses a heap buffer and all four bulk operations (fill, zero, copy, crc32; 1 B to 1 MiB) pass on a Debian 13 guest, with the guest checking the result and the CRC32 matching an independent computation. `bulk-test`, `bulk-zero-test`, `bulk-copy-test` and `bulk-crc-test` in `fluxvm_virtioctl` drive them (the ioctl gained an `op` field).
- **Shared vmnet broker** (`macos/vmnetd`, `fluxvm-vmnetd`): VMs that name the same network with `apple.vmnet.name` share it
  through a broker over XPC ([docs/VMNET_BROKER.md](docs/VMNET_BROKER.md)). Not verified: on the test host the ad-hoc signed
  broker (`com.apple.vm.networking`) was killed at launch (SIP on), so the two-runner test did not run. Without a broker a
  named shared vmnet VM fails with "vmnetd XPC connection failed".
- **Physical USB passthrough** through Accessory Access: runner control commands `usb-physical-list` and `usb-physical-attach`,
  with a `FluxVMUSBAccess` app that needs the `com.apple.developer.accessory-access.usb` entitlement. There is no REST route or
  `fluxctl` command. Not verified: the ad-hoc signed app was killed at launch and no USB device was attached to the test Mac.
- **`fluxvm-vz-runner host-capabilities`** and `fluxvm_apple::host_capabilities`: reports OS version, CPU and memory,
  nested virtualization, vmnet, custom Virtio, guest memory mapping, the USB API and bridgeable interfaces. Verified on the M4.
- **Apple placement in the fleet**: `fluxvm-agent node` heartbeats carry the host's `apple` capabilities, and `fluxvm-agent
  central` uses `apple_placement` for requests with backend `vz` (nodes without Apple capabilities are excluded for `vz`; a plain
  `vz` request prefers the tightest fit). Verified on loopback with one real M4 node plus two fake nodes registered by hand; a
  request with a named shared vmnet was placed on the only vmnet-capable node. Not verified: more than one real Mac. Central's `vz` filtering now has four unit tests (request parsing, nodes without Apple capabilities, feature requirements, the two-macOS-guest limit).
- **Capability decode fix**: the Rust decode of `maximumVmCPUs` and `usbPassthroughAPI` uses explicit serde renames; checked
  against the real M4 output.
- Unit tests on the M4: `fluxvm-apple` 49 (plus 1 `backend` integration test and 6 source-contract tests, which only grep Swift
  and C source), `fluxvm-agent` 55, `fluxvm-core` 79, `fluxvm-scheduler` 204; clippy on `fluxvm-apple` and `fluxvm-agent` is clean.
- Still not verified: the two-runner broker test, physical USB, more than one Mac, the custom Virtio zero/copy/crc32 operations.

### Added: a Mac section on the website, and an architecture guide
- A `/mac` page on the docs site: measured numbers (one Apple M4, macOS 27.2), what runs on a Mac, how it works, which Mac to
  choose (Apple's published specs, with FluxVM capacity guidance marked as estimates), what macOS 27 adds, and what is and is not
  verified.
- [docs/macos-architecture.md](docs/macos-architecture.md): the process model, control socket, vsock, guests, networking, storage,
  warm pool, sandboxes and fleet in detail, with a verified / not-verified table.

### Fixed: container sandboxes on `vz`
- `GET /v1/sandboxes/{id}/logs` returns the console tail one line per line (it was joined with ` | `), so
  `fluxctl sandbox run` and `sandbox logs` print the container's output as written. `sandbox run` sends init and agent
  lines to stderr.
- Container sandboxes and `apple.oci_warm_sizes` require at least 512 MiB, the `vz` minimum, and say so up front.
- The `oci-boot` workflow builds the boot artifacts again: USB_PCI for the warm pool's XHCI, `gen_init_cpio` compiled
  directly, and `musl-gcc` for zstd-sys.
- Faster container sandbox creation with a cached image: registry tokens, manifests fetched by digest and HTTP connections
  are reused across pulls (one registry round trip instead of four), and warm slots boot with
  `usb_storage.delay_use=0` so a claim no longer waits a second for the USB rootfs. Measured on Apple silicon: warm claim
  3.6 s to 1.3 s, cold start 2.6 s to 1.75 s.
- `scripts/vz-devices-live-test.sh` (with `scripts/nbd-test-server.py`) checks extra disks of every kind and controller,
  USB hot-plug, console ports and the Linux display size on hardware; it passes on a Debian 13 guest.
- A deleted `vz` VM's MAC address goes to the next new VM, so it gets the old DHCP lease back. The Mac keeps a lease
  per MAC for a day, and a fresh MAC per VM filled `/var/db/dhcpd_leases` (253 entries after a day of live tests).
- `scripts/vz-devices-live-test.sh` also checks `fluxctl port-connect` both ways over the daemon's websocket.
  `scripts/macos-live-test.sh` prints the daemon log, the guest console or the HTTP response when a check fails.

- A warm slot is never restored while a running VM holds the address it was built with. Slots are built one after another and the
  NAT frees a stopped guest's address, so two slots could end up with the same one; a restored guest keeps it, which gave two
  concurrent sandboxes one address. Each slot records its address in the `fluxvm.slot-ip` label, and a slot whose address is in use
  is skipped, so that create cold-boots.

### Changed: `vz` follow-ups
- `fluxctl disk detach` unplugs a hot-attached USB disk from the running guest at once.
- Warm-claimed container sandboxes are not hibernated until they restart (their root is on USB until then).
- `fluxctl ls` and `GET /v1/vms` leave out container warm-pool slots unless given `--all` / `?all=true` or a `fluxvm.oci-warm`
  selector.
- `apple.serial_log_max_mib` (default 16) sets the console log rotation size; `sandbox logs` reads the rotated file too.

### Added: `vz` developer conveniences
- **Named console ports**: `apple.console_ports` ([docs/macos.md](docs/macos.md#named-console-ports)) adds
  `VZVirtioConsolePortConfiguration` ports. A Linux guest sees `/dev/virtio-ports/<name>`, and the runner bridges each port to a
  unix socket. Connect with `fluxctl port-connect <vm> <name>` or the websocket `GET /v1/vms/{id}/ports/{name}`.
- **Linux display size**: Linux guests now use `display_width` and `display_height` for their scanout instead of a fixed
  1280x800. The defaults are the same as for macOS guests (2560x1600).
- **Serial log rotation**: the runner moves `console.log` to `console.log.1` past 16 MiB, keeping one old file.

### Added: `vz` storage options
- **`apple.extra_disks`** ([docs/macos.md](docs/macos.md#extra-disks)) takes `kind` (`image`, `block` for a host device through
  `VZDiskBlockDeviceStorageDeviceAttachment`, `nbd` for an NBD export through `VZNetworkBlockDeviceStorageDeviceAttachment`),
  `caching` (`automatic`, `cached`, `uncached`), `sync` (`full`, `fsync`, `none`), `controller` (`virtio`, `nvme`, `usb`) and a
  `name`. Combinations a kind cannot use are refused at create time.
- **`fluxctl disk` / `/v1/vms/{id}/disks` work on `vz` VMs**: `ls`, `attach` (`--size-gib` makes a sparse image; `--path`,
  `--url`, `--kind`, `--read-only`, `--caching`, `--sync`, `--controller`) and `detach`. Changes apply at the next start, and a USB
  image disk is also hot-attached to a running guest.

### Added: warm pool for container sandboxes
- **`apple.oci_warm_slots`** and **`apple.oci_warm_sizes`** ([docs/oci-sandboxes.md](docs/oci-sandboxes.md#warm-pool)) keep
  pre-booted container VMs per size. Their init waits in the new `wait` mode with its DHCP lease. A matching sandbox gets its
  rootfs hot-attached over USB (macOS 15) and its volumes swapped in with the new runner `share-set` command. The claim goes over
  vsock, and init continues as on a cold boot. Any failure cold-boots instead.
- `GET /v1/sandboxes/density` adds `oci_warm_slots_configured`, `oci_warm_slots_ready`, `oci_warm_slots_booting`,
  `oci_warm_resident_mib`, `oci_warm_hits`, `oci_warm_misses` and `oci_warm_last_claim_ms`.
- The OCI kernel fragment enables XHCI and USB mass storage. `scripts/oci-live-test.sh` checks a warm claim on macOS 15.

### Added: x86-64 container images under Rosetta
- **`oci.platform: "linux/amd64"`** ([docs/oci-sandboxes.md](docs/oci-sandboxes.md#x86-64-images-rosetta)) pulls the amd64
  variant and runs it under Rosetta for Linux. The VM gets the Rosetta share, and init registers it with `binfmt_misc` (flags `F`
  and `C`) before the container starts; the kernel now has `CONFIG_BINFMT_MISC`. Without Rosetta, create fails with the
  `softwareupdate --install-rosetta` hint. Cached rootfs entries record their `architecture`.
- Available as `fluxctl sandbox run --platform`, `fluxctl oci pull --platform`, `POST /v1/oci/images` `platform`, a stack
  service's `platform` (also imported from compose), and MCP `oci_platform`.

### Added: private networks between `vz` guests
- **`apple.networks`** ([docs/macos.md](docs/macos.md#private-networks-between-guests)) adds a network card per private network
  (`VZFileHandleNetworkDeviceAttachment`), backed by a new `fluxvm-vz-switch` process per network. It forwards by registered MAC,
  drops spoofed source MACs and unknown unicast, and exits when idle. Each network gets a `10.89.N.0/24`; addresses and MACs are
  assigned at create time under one lock and kept in the VM record. A network belongs to one tenant.
- **Container sandboxes**: `oci.networks` and `oci.hosts`. Init configures the card by MAC without DHCP, and works with `offline`.
  **Full VMs** get a systemd-networkd unit and a `fluxvm-vznet.service` fallback through cloud-init.
- **Stacks**: `network = "private"` puts every service on `stack-<name>` with fixed addresses and direct `/etc/hosts` names, with no
  gateway relay and no unique-port rule.
- `fluxctl vznet ls` and `GET /v1/vznets`. The macOS package and Homebrew formula install `fluxvm-vz-switch`, and
  `scripts/oci-live-test.sh` checks two sandboxes on one network.

### Added: container sandboxes on a Mac, one VM per container
- **OCI images as sandboxes on `vz`** ([docs/oci-sandboxes.md](docs/oci-sandboxes.md)): `POST /v1/sandboxes` with
  `"oci": {"image", "command"?, "entrypoint"?, "env"?, "workdir"?, "user"?, "read_only_root"?, "exit_policy"?}` boots the image
  in its own lightweight VM. Virtualization.framework direct kernel boot (`VZLinuxBootLoader`) starts a small kernel and initramfs,
  and the new `fluxvm-oci-init` (PID 1) mounts the image read-only, runs DHCP, starts the guest agent and runs the entrypoint as
  65534 with `no_new_privs`. There is no SSH: exec and files go through the agent only. `offline`, `allow_hosts`, `profile` and
  `ttl_seconds` apply.
- **Pull and rootfs cache**: a pure-Rust registry client (`linux/arm64`, digest-verified blobs, anonymous tokens or
  `[[apple.oci_registry_credentials]]`). A network-less builder VM unpacks the layers (whiteouts, owners, xattrs, diff_id checks)
  into `state_dir/oci/rootfs/<digest>.ext4` once per digest. Each sandbox gets an APFS clone of it.
  `fluxctl oci pull | ls | rm | prune` and `/v1/oci/images`, `/v1/oci/prune` manage the cache.
- **`fluxctl sandbox run IMAGE [--rm] -- CMD`** prints the container's console and exits with its exit code.
  `fluxctl sandbox logs ID` and `GET /v1/sandboxes/{id}/logs` return the console tail with `exit_code` and `init_error`.
- **MCP**: `sandbox_create` takes `oci_image`, `oci_command` and `oci_env`; the new read-only `sandbox_logs` tool returns the
  exit code.
- **Exec with argv**: `POST /v1/sandboxes/{id}/process` takes `{"process": {"argv", "env"?, "cwd"?}}`, which the agent runs
  without a shell (distroless images). `AgentRequest::Exec.process` is optional, so older agents and clients are unaffected.
- **Runner**: `kernel`, `initrd` and `kernel_args` boot Linux guests directly on `vz`. New options are a read-only root disk
  (`apple.root_read_only`) and virtiofs shares with fixed tags (`apple.tagged_shares`).
- **Boot artifacts**: `scripts/build-oci-boot.sh` (Linux arm64) and the `oci-boot` CI workflow build `oci-kernel` and
  `oci-initrd` from `guest/oci-vz.config.fragment`, using checksummed kernel and e2fsprogs sources. New config keys:
  `apple.oci_kernel`, `oci_initrd`, `oci_cmdline` and `oci_builder_memory_mib`.
- `scripts/oci-live-test.sh` is the hardware test; it passes on an Apple M4 (exit codes, exec, offline and allow-listed network, a private
  network between sandboxes, `linux/amd64` under Rosetta, a warm-pool claim).

### Added: running services in container sandboxes
- **Published ports**: `oci.ports` (`HOST:CONTAINER`; `fluxctl sandbox run -p`, MCP `oci_ports`) become runner forwards on the
  Mac's `127.0.0.1`. Init prints `VELORA-IP <address>` after its DHCP lease so the runner can reach the container.
  `oci.expose` relays ports to other guests at the NAT gateway, and `oci.gateway_hosts` names the gateway in the container's
  `/etc/hosts`.
- **Named volumes** for container sandboxes (`volumes`, `fluxctl sandbox run -v NAME:/path[:ro]`): virtiofs shares tagged
  `fluxvm-vol<N>` that init mounts before switching into the image's root. Containers may mount them anywhere except the paths
  init manages.
- **Restart policy and health checks**: `oci.restart` (`no` | `on-failure` | `always`), `oci.max_restarts` and `oci.healthcheck`.
  Init restarts with 1-60 s backoff and stops an unhealthy process when a policy is set. `/logs` reports `restarts` and `health`
  (`FLUXVM-RESTART`, `FLUXVM-HEALTH` markers). `fluxctl sandbox run --restart --max-restarts --health-cmd --health-interval`.
- **Registry passwords from the macOS Keychain**: `keychain_service` in `[[apple.oci_registry_credentials]]` reads the password
  with `security find-generic-password` at pull time instead of storing it in the config.
- **Container services in stacks** ([docs/macos-stacks.md](docs/macos-stacks.md#container-services)): `container = "IMAGE"` with
  `command`, `entrypoint`, `env`, `restart`, named `volumes`, and `ready` as the health check that `up` waits for. They can be
  mixed with VM services. **`fluxctl import-compose`** converts a `docker-compose.yml` and lists what it could not carry over.
- Fixed: OCI rootfs build locks are keyed by rootfs path, so `prune` in one state dir no longer skips blob cleanup while another
  state dir is building.

### Added: secrets, fleet placement for stacks, a dashboard, and a LaunchAgent
- **Write-only secrets** ([docs/oci-sandboxes.md](docs/oci-sandboxes.md#security-defaults)): `oci.secret_env` (`{"NAME": "value"}`)
  reaches the container's process environment through a 0600 `secrets.json` on the read-only meta share. It is never stored in the
  VM record or returned by the API (`env` and the agent token still are). `fluxctl sandbox run --secret-env NAME` reads the value
  from its own environment.
- **`secret_env` in stacks** ([docs/macos-stacks.md](docs/macos-stacks.md#secrets)): names whose values `up` takes from its
  environment. Container services get them as above; VM services get `~/.config/fluxvm/secrets.env` (0600) over SSH, which
  `after_up` sources. Only the names are part of the spec hash.
- **Stacks on a fleet** ([docs/macos-stacks.md](docs/macos-stacks.md#running-a-stack-on-a-fleet)): `fluxctl up | down | ps --fleet URL`
  puts the whole stack on one `fluxvm-agent central` node: where it already runs (also a drained node), else `--node` /
  `placement.node`, else the healthy, undrained node with the `placement.labels` / `--node-selector` labels and the most free
  capacity for the stack's total size.
- **Dashboard** ([docs/operations.md](docs/operations.md)): `GET /console` now lists VMs and container sandboxes grouped by stack,
  with start/stop/restart/delete, console and container logs, health and memory pressure. The static page is served without auth
  under a strict CSP and asks for the API token, which it keeps in session storage. `fluxctl dashboard` opens it.
- **`fluxctl service install | uninstall | status`** ([deploy/launchd-notes.md](deploy/launchd-notes.md)): writes and loads the
  `dev.zyvor.fluxvm` LaunchAgent that runs `fluxctl serve` in the user's login session.
- **API token from the macOS Keychain**: `fluxctl context add NAME --server URL --token-keychain SERVICE` reads the token with
  `security find-generic-password` (account = context name) instead of storing it in `contexts.json`.

### Added: Mac Studio options, macOS guests through the API, and many small agent VMs per Mac
- **Mac Studio options on `vz`** ([docs/macos.md](docs/macos.md)): up to eight displays, Linux clipboard (SPICE), bridged networking
  (`apple.bridge_interface`, needs the networking entitlement), vmnet networks on macOS 26+ (`apple.vmnet`: shared or host-only,
  subnet, a DHCP reservation, port forwards), custom virtio devices on macOS 27, an ASIF disk overlay that snapshots with the VM,
  macOS guest provisioning, balloon control and USB mass-storage attach. `apple_placement::pick` scores hosts (macOS guests: two per Mac); the fleet registry now calls it for `vz` requests (see above).
- **macOS guests through the API**: `apple.install` installs from an IPSW into a new disk, then boots; `apple.firstboot` puts SSH keys
  in place and turns on Remote Login on first boot. Examples in `examples/macos-install.json` and `macos-clone.json`.
- **Agent density** ([docs/agent-density.md](docs/agent-density.md)): sandbox `profile` (`tiny` 1/512, `small` 1/1024, `standard`);
  pressure admission on macOS (`vm_stat` and `kern.memorystatus_vm_pressure_level`, new `policy.deny_host_pressure_level`);
  AutoPause and idle reclaim cover `vz` sandboxes (labelled `fluxvm.sandbox`); `sandbox.hibernate_idle_secs` saves an idle `vz`
  sandbox's memory and stops it, and the next request restores it; `POST /v1/sandboxes/warm`, `GET /v1/sandboxes/density`,
  `fluxctl sandbox warm | density`.
- **Guest agent on `vz`** ([docs/vsock-agent.md](docs/vsock-agent.md), [docs/vsock-proxy.md](docs/vsock-proxy.md)): sandboxes with the
  agent (the `agent-micro` image, or `agent.enabled`) run commands and move files over vsock through the runner, falling back to SSH;
  the token travels by cloud-init. The runner's relays now close and release their connections when done (they used to stay open
  for the VM's life).
- **`agent-micro` image** ([docs/agent-micro.md](docs/agent-micro.md)): `scripts/build-agent-micro.sh` and
  `build-and-publish-agent-micro.sh`; registered as a catalog entry; sandbox create takes `image`.
- **Ballooning under pressure** ([docs/ballooning.md](docs/ballooning.md)): idle reclaim starts after 60 s while the host is at warn
  or critical.
- **Fleet** ([docs/kairon-mac-scheduling.md](docs/kairon-mac-scheduling.md), [docs/topology.md](docs/topology.md)): `fluxvm-agent node`
  reports the density report; central placement skips nodes at critical pressure and prefers calm ones.
- **Metrics**: `fluxvm_host_memory_pressure_level`, `fluxvm_host_mem_available_mib`, `fluxvm_sandbox_warm_hits_total` /
  `_misses_total`, `fluxvm_sandbox_balloons_inflated`, `fluxvm_vz_agent_calls_total`, `fluxvm_vz_agent_ssh_fallbacks_total`.
- Scripts: `scripts/density-smoke.sh`, `scripts/e2e-smoke.sh`; guides: [docs/integration.md](docs/integration.md),
  [deploy/launchd-notes.md](deploy/launchd-notes.md), [deploy/kairon-node-mac.md](deploy/kairon-node-mac.md).
- Verified on an Apple M4 since: building and booting `agent-micro` (`scripts/e2e-smoke.sh` with the guest agent answering over vsock),
  USB disk hot-attach (`scripts/vz-devices-live-test.sh`), and cloning a prepared macOS guest. Not verified: bridged networking, vmnet,
  macOS guest install and provisioning through the API, physical USB passthrough, a multi-Mac fleet.

### Added: macOS guest clones, VMPal-parity options, and MCP VM lifecycle tools
- **Clone prepared macOS guests** ([docs/macos.md](docs/macos.md#macos-guests)): `POST /v1/vms` with `backend: "vz"`,
  `apple: {guest_os: "macos"}` and `image` = a template's `disk.raw` clones the disk (APFS `cp -c`) and takes `hardware.bin` and
  `auxiliary.bin` from beside it; every clone gets its own machine identifier. The runner reads the guest's address from the Mac's
  DHCP lease file (`/var/db/dhcpd_leases`), since macOS guests have no systemd. Verified by hand on an Apple M4 (macOS 27.2 host,
  macOS 27.0.1 guest). `scripts/macos-guest-live-test.sh` clones a template through the API and checks the address and SSH login.
  If key login is refused on a fresh clone, FileVault is on in the template; turn it off there first (see the macOS guests section).
- **Display, audio, sharing and USB options** on `apple`: `display_width`, `display_height`, `display_ppi`, `audio_output` (on by
  default), `microphone` (off), macOS-guest shared folders under `/Volumes/My Shared Files`, and for Linux guests `rosetta` and
  `nested_virtualization` (macOS 15, M3 or later); `usb_controller` adds an XHCI controller (macOS 15+).
- **MCP VM lifecycle tools** ([docs/mcp.md](docs/mcp.md)): with `--allow-write`, `vm_create` (optional `ready_exec`), `vm_clone`,
  `vm_exec`, `vm_snapshot`, `vm_snapshot_restore`, `vm_snapshot_delete` and `vm_delete`; `vm_snapshot_list` is always offered.
  `vm_exec` reaches `vz` guests over the Apple SSH transport.

### Added: native macOS support (Apple silicon)
- **`vz` backend** (`crates/fluxvm-apple`): runs ARM64 Linux guests on Apple's Virtualization.framework through a signed
  Swift helper (`fluxvm-vz-runner`) supervised over a unix control socket. REST lifecycle (create, start, stop, pause,
  resume, delete) works through the existing API; `backend: "auto"` resolves to `vz` on macOS. See [docs/macos.md](docs/macos.md).
- The daemon, REST API, scheduler and `fluxctl` now **build and test on macOS**: the few Linux-only syscalls (`setns`, the `bpf`
  syscall, `CLOCK_BOOTTIME`) are cfg-gated with `Unsupported` stubs; no behaviour change on Linux.
- macOS defaults for the state and run directories, a `cp -c` raw-disk clone, and a `hdiutil` NoCloud seed builder.
- `scripts/macos-live-test.sh`: boots a real VM through the API and exercises the lifecycle (**PASS** on an Apple M4).
- Verified on macOS: core 70, scheduler 171, api 56, network, storage 17, guest-protocol 14 unit tests when the backend landed;
  `fluxvm-apple` had 16 (it has 49 unit tests, 1 integration test and 6 source-contract tests by 0.4.0). Not verified at the time: macOS guests (cloned and reached over SSH later, see below), Linux-only crates.
- **Developer features on the `vz` backend** (each verified live on an Apple M4, see `scripts/macos-live-test.sh`): TCP port forwards
  (including `guests: true` on the NAT gateway), virtiofs shared folders, named images (`debian-13`, `debian-12`, `ubuntu-24.04`,
  downloaded, checksum-verified and cached offline-tolerantly), snapshots with restore (memory and disk, about 0.7 s to save and 1.4 s to
  SSH after restore), and `network.mode = "none"`, which now really attaches no network card.
- **`fluxctl run`** (a throwaway VM and a shell, warm-started in 2 to 3 s), **`fluxctl ssh`**, and **`fluxctl up | down | ps`** for
  multi-VM stacks described in a `fluxvm.toml` ([docs/macos-stacks.md](docs/macos-stacks.md)).
- **Agent sandboxes on a Mac** ([docs/macos-sandboxes.md](docs/macos-sandboxes.md)): the sandbox API and MCP tools over SSH; a warm pool
  (`sandbox.warm_slots`) that follows the base image; `offline` sandboxes reached over vsock; `allow_hosts` for allow-listed HTTP(S)
  egress through a host proxy; speculate and changesets.
- **Packaging:** `scripts/package-macos.sh`, a release workflow and a Homebrew formula template (no tap is published).
- Docs: `docs/macos-cluster.md` (FluxVM on a Mac mini, Mac Studio, MacBook Pro and a Mac cluster for private inference),
  README cards and a Mac section on the website home page.

### Changed
- **Native eBPF is the primary dataplane.** Omitted dataplane configuration now
  selects `mode = "ebpf"`, `required = true`. A failed attachment on a host-visible
  VM interface fails creation/start instead of falling back to nftables. Existing
  explicit `legacy`, `cilium`, or lab `required = false` selections remain honored.
  This changes upgrades with omitted mode: install BPF objects and run host
  preflight first, or set `mode = "legacy"` explicitly. Default policy remains
  allow-all; use the production profile for deny-by-default. See
  [primary-ebpf.md](docs/primary-ebpf.md).

### Added: crash safety, fork, density, vhost-net (2026-10-08)

Everything in this block is implemented and type-checked and passes unit tests.
**None of it has been run end to end on real VMs yet.** No performance figure is
claimed.

Crash safety ([docs/crash-recovery.md](docs/crash-recovery.md))
- **Durable writes.** `fluxvm_storage::write_durable` (write temp, fsync, rename,
  fsync the directory) now backs `vms.json`, the quota ledger, journal intents and
  idempotency records.
- **Operation journal.** `<state_dir>/journal/` records create, snapshot, fork and
  restore intents (and the existing delete intent) before any resource is
  allocated. `OpGuard` marks an intent abandoned if the operation is dropped or
  fails; owner liveness uses pid plus `/proc` start time.
- **Recovery on start.** `reconcile()` rolls back intents of dead owners
  (`replay_pending_ops`, at most 5 attempts) and sweeps orphaned workspaces (1 h
  grace) and, only with `FLUXVM_ORPHAN_NET_SWEEP=1`, network namespaces and taps
  (120 s grace; off by default because the names are host-wide), validating every
  name against FluxVM's own scheme and the VM id first.
- **`Idempotency-Key`** on `POST /v1/vms`, `DELETE /v1/vms/{id}`, `POST
  /v1/vms/{id}/snapshot` and `/fork`: durable 24 h replay with `Idempotent-Replayed:
  true`; 422 for a changed body, 409 while the first request runs, 400 for a bad key.
- **`scripts/test-crash-recovery.sh`**: SIGKILLs the daemon mid create, delete,
  snapshot and fork and checks convergence. Run it only on hosts you control; an
  earlier version deleted every `eph-<8hex>` namespace on the host.
- Known gaps: IPAM lease not released by the orphan sweep, QEMU internal savevm not
  rolled back, extra-NIC and user-named taps and jailer trees not journaled, delete
  intent omits nbd and Ceph.

Fork and restore ([docs/fork-and-restore.md](docs/fork-and-restore.md))
- **`MAP_PRIVATE` snapshot restore** on the in-tree KVM engine, with an eager-copy
  fallback (`FLUXVM_KVM_EAGER_RESTORE=1`, `FLUXVM_KVM_LOCK_MEM=1`,
  `FLUXVM_HUGEPAGES=1`, size mismatch, mmap error).
- **Child identity reset.** Fork children get their own hostname, machine-id and
  entropy through the new `AgentRequest::ResetIdentity`. Best effort; open connections
  are not reset.
- **`?ready=exec`** (or `?wait_first_command=true`) on create, fork and pool claim
  returns `first_command_ms` and `phases`.
- Fork is journaled and all-or-nothing.
- Scripts: `bench-fork.sh` (ready time, PSS, disk per child), `bench-first-command.sh`.

Networking ([docs/native-io-performance.md](docs/native-io-performance.md))
- **vhost-net bring-up** in the kernel's order: owner, features, mem table, rings,
  IRQ relay, backend. Previously the backend was bound before the rings were
  programmed and no interrupt reached the guest.
- `net_datapath` in the hypervisor Metrics, `FLUXVM_VHOST_NET=0` toggle, opt-in
  **multiqueue** (`--net-queue-pairs`, `FLUXVM_NET_QUEUE_PAIRS`, up to 4), re-bind
  after snapshot restore.
- `scripts/bench-kvm-vhost.sh`. Result tables are empty until measured.

Memory density ([docs/ROADMAP-DENSITY.md](docs/ROADMAP-DENSITY.md))
- Balloon control `GET|POST /v1/vms/{id}/balloon`, per-VM PSS report `GET
  /v1/vms/{id}/memory`, idle reclaim (`[sandbox] idle_balloon_secs`,
  `idle_balloon_percent`), pressure-aware admission (`[policy]
  min_host_mem_available_mib`, `max_host_mem_psi_some_avg10`,
  `max_host_mem_psi_full_avg10`, `pressure_defer_secs`; all off by default).
- `scripts/bench-density-count.sh` (consumes host memory; needs `FLUXVM_BENCH_CONFIRM=1`).

Sandboxes
- **Speculative execution**: `POST /v1/sandboxes/{id}/speculate` and changesets
  (list, get, approve, reject, apply) with `fluxctl sandbox` commands. Apply only
  writes files, only after approval, and only if the real files still match.
- **Guest exec policy** (Landlock + seccomp) with enforcement reported back.
- **Per-sandbox credential grants** at `/v1/sandboxes/{id}/grants`: in memory, not
  persisted, injected by the egress proxy.

### Changed (2026-10-08)
- **The guest agent fails closed** when it has no token file and refuses every
  request. Set `FLUXVM_AGENT_ALLOW_INSECURE=1` in the guest to run unauthenticated.

### Added
- **Eject install media.** `POST /v1/vms/{id}/cdroms/{name}/eject` (admin) removes a CD-ROM's medium,
  live over QMP when the QEMU VM is running, and records the drive as empty (`path: ""`). Later starts and
  migration receivers create a bare `ide-cd` device, so the guest's device layout stays the same. Ejecting is
  idempotent, and live migration now rejects only CD-ROMs that still hold media, so an installed Windows VM
  can migrate after ejecting its ISO. See [api.md](docs/api.md).
- **Backups on every engine.** `POST /v1/vms/{id}/backup` and `restore-backup` work for Cloud Hypervisor,
  Firecracker and flux-vm VMs that are stopped, and for `storage: shared` disk files. A running VM still needs
  QEMU on default storage (internal snapshot); others get "stop the VM first". A restore converts back to the
  disk's own format (raw for everything but QEMU on default storage) instead of always writing qcow2.
- **Live size after hotplug.** A CPU or memory hot-add records `fluxvm.dev/live-vcpus` /
  `fluxvm.dev/live-memory-mib` and `fluxvm.dev/hotplugged` on the VM (cleared on the next start). Hot-added
  VMs are refused as migration sources until restarted.
- **Adoptable migration receivers.** `POST /v1/migration/receivers` takes the source VM's `record` and
  launches the receiver from that record's own device model (network, seed, vsock, firmware), with its own
  workspace, tap/netns and CID, so it works host-to-host or on one node. `POST
  /v1/migration/receivers/{id}/adopt` turns it into a running VM once the incoming side finishes, and
  `POST /v1/vms/{id}/migration/finish` removes the paused source. See `docs/runtime-boundary.md`.
- **`storage: shared`.** A raw file or block device on shared storage used in place, never deleted,
  guarded by `<disk>.fluxvm-lock` so two VMs cannot run on it at once. `POST /v1/vms?shared_takeover=true`
  breaks the lock after fencing the previous node (HA re-create).
- **Read-only serial for every backend.** `GET /v1/vms/{id}/serial` on Cloud Hypervisor, Firecracker and
  flux-vm streams `console.log` (last 64 KiB, then follow); QEMU keeps the interactive socket.
- **MCP server for AI agents.** `fluxctl mcp serve` speaks the Model Context Protocol over stdio, so
  Hermes Agent and other MCP clients can list and inspect VMs, host readiness, VM-edge network data
  and console logs through the REST API. `--allow-write` adds `vm_power` and `vm_capture`; without it
  they are neither listed nor callable. No new dependencies. See `docs/mcp.md`.
- **GPUs for sandboxes.** `gpus: N` on `POST /v1/sandboxes` passes N free VFIO-bound GPUs through to a
  QEMU-backed sandbox. FluxVM picks them under a lock (NUMA-aware, deterministic), so two callers are never given the
  same GPU; a shortage is a 503. Refused with `confidential`, `procbox`, or a non-QEMU template. The Python, Go and
  TypeScript SDKs take `gpus`. See `docs/sandbox-gpus.md`.
- **Kairon VM edge, enforced (dataplane schema 12).** The edge spec Kairon posts to
  `POST /v1/vms/{id}/network/edge` is now loaded into the VM's TC program instead of only being stored:
  anti-spoof (`spoof_mac` / `spoof_ip`), learn-IP from guest ARP and IPv6 neighbor advertisements,
  DNS query-name and TLS SNI allow lists (`dns_deny` / `sni_deny`, exact or `*.suffix`), and an egress
  token bucket. Ingress bandwidth and packet rate use a `tbf` qdisc and a `matchall` police on the
  host interface. Netns VMs get a routed mode that skips MAC and ARP checks behind the namespace
  router. `drops` reads the datapath's per-reason counters, `learned-ip` reads the learned address
  (falling back to the DHCP lease), and conntrack export dumps the live table and returns an empty
  snapshot instead of 400 when there is nothing to export. The spec and a pending conntrack restore
  are persisted under `<state_dir>/network-edge/` and reapplied on every attach, so they survive
  FluxVM and VM restarts. New drop-reason codes 13-16. `scripts/test-vm-edge-verdict.py` checks the
  verdicts against the real object in CI. See `docs/vm-edge-contract.md`.
- **VM-edge packet capture.** `POST /v1/vms/{id}/network/capture` now runs a bounded `tcpdump`
  (1-30 s, 20,000 packets, 1,600-byte snaplen, optional filter) on the VM's dataplane interface inside
  its network namespace. `GET .../network/capture` lists sessions with `state`
  (`running`/`done`/`failed`/`interrupted`) and packet counts; `GET .../network/capture/{token}`
  downloads the pcap (409 while running). One capture per VM at a time; the newest 16 are kept.
  Needs `tcpdump` on the host; the AppArmor profiles now allow it, `nsenter` and `network packet raw`.

### Fixed
- **A NIC hot-added to a netns VM broke its next start.** The NIC was recorded in `network.extra`, which a
  `netns: true` VM couldn't relaunch with, so the next start or snapshot restore failed. Extra NICs on a netns
  QEMU VM are now host-bridge taps handed to QEMU as inherited file descriptors (QMP `getfd` at hot-add,
  `-netdev tap,fd=` on every launch), so they also work at create. Other engines refuse netns + extra NICs at
  create.
- **Migrating after unplugging the last extra NIC failed** (receiver exited, "Broken pipe"): a VM launched
  with extra NICs has its primary NIC on `hotplug-pcie-0`, which a receiver built from the NIC-less record
  doesn't. Removing the last extra NIC now sets `fluxvm.dev/hotplugged`, so migration is refused until a restart.
- **Descriptor hot-add right after create failed** with "connecting to QMP socket: No such file or directory":
  the fd-passing QMP session now waits for QEMU's socket like every other QMP call.
- **Relaunch after unplugging a middle NIC failed** with "extra NIC N is missing a bridge": the free slot an
  unplug leaves (to keep later NICs on their PCIe ports) is now carried through instead of rejected.
- **AppArmor profile blocked the eBPF dataplane.** The `fluxvm` and `fluxctl` profiles did not allow
  `bpftool` (or the `uname`/`basename` its Ubuntu wrapper runs), `/sys/fs/bpf` or the `bpf` /
  `perfmon` capabilities, so on an AppArmor host no VM could attach to the eBPF dataplane. Reinstall
  and reload `deploy/apparmor/fluxvm`.
- **`--locked` builds failed** because `Cargo.lock` was stale against the guestkit dependency.
- **Multi-vCPU KVM snapshot-restore clock desync.** `apply_vcpu` was missing
  the `KVM_KVMCLOCK_CTRL` ioctl on restore (Firecracker's own restore order
  does call it), so a 2+ vCPU FLUXKVM1 restore could resume with sibling
  vCPUs' kvmclock bookkeeping disagreeing. Fixed and verified 30/30 clean
  restores at 2 vCPUs (was 24/25 unfixed) — closed the `native-kvm` hosted-CI
  flake this exposed.
- **Guest virtio-net had no RX path.** The device could send but not deliver
  inbound frames to the guest; added the bounds-checked descriptor-chain
  walk and used-ring publish so DHCP/ping/TCP now work end to end
  (`scripts/test-kvm-net.sh`). Also fixed a byte-swapped tap address
  (`Tap::open` used `from_be_bytes` instead of `from_ne_bytes`).
- **Two sources of `native-kvm` CI log-capture corruption.** A leftover
  per-exit diagnostic `eprintln!` in `vm.rs` and a routine `[net]`/`[blk]`
  queue-notify diagnostic could interleave mid-line with guest serial output
  under hosted-runner load, dropping vCPUs from the topology smoke test's
  parsed output. Both removed/quieted.
- **`fluxvm-procbox`'s library `Policy::default()` was insecure** —
  `tcp_connect`/`tcp_bind` defaulted to `Any`, silently skipping all socket
  filtering for anyone constructing a bare `Policy` directly (the CLI and
  scheduler integration already defaulted to `Deny` and were unaffected).
  Now defaults to `Deny`/`Deny`.
- **Firecracker snapshot restore used a stale API field.** `/snapshot/load`
  had moved `uds_path` under `vsock_override: {uds_path}`; restoring a
  Firecracker sandbox with the agent enabled silently broke. Also cleans up
  a stale vsock socket file before rebinding.
- **Dependabot alerts.** `jsonwebtoken` 9→10.4.0 (medium, type confusion in
  algorithm handling — no call-site changes needed beyond enabling the
  `rust_crypto` backend feature); website's Docusaurus toolchain pulled
  vulnerable transitive `image-size`/`qs`/`serialize-javascript`/`uuid` —
  pinned via `npm` `overrides` to patched versions, `npm audit` now clean.
  The low-severity `lru` alert remains open: it's pinned by `ratatui 0.28.1`
  inside the separate `guestkit` repo (a path dependency), not fixable from
  this repo until `guestkit` bumps `ratatui` to a line that drops `lru`.

### Added
- **Full guest CPUID topology, AMD included.** Leaves 1/4/0xB/0x1F plus AMD
  `0x8000_0008`/`0x8000_001E`; a real bug was found and fixed where AMD's
  `TopologyExtensions` CPUID feature bit was never set, so an AMD guest
  kernel would never even read the extended topology leaf. Validated with
  spoofed `AuthenticAMD` vendor strings booting real Linux guests on the
  (Intel) lab host at 1/2/4 vCPUs, since no AMD hardware is available.
- **FLUXKVM1 v5 full-fidelity snapshots**, superseding the v4 line below:
  XSAVE, XCRS, MSRs, LAPIC state, vCPU events, and debugregs per vCPU, plus
  clock, IRQCHIP, and PIT state per VM. Backward-compatible with older
  snapshot versions on load.
- **Real, guest-visible CPU hotplug.** `HotplugCpu` previously only
  validated a request and returned a fake success message. Every vCPU up to
  `max_cpus` is now pre-created parked at boot (correct final-topology
  CPUID, present in the MP table); a `maxcpus=<cpus>` kernel argument caps
  auto-onlining, and the guest brings the rest online itself via the
  standard `echo 1 > /sys/devices/system/cpu/cpuN/online` path. Verified
  live, 2→4 vCPUs, 5/5 clean runs (`scripts/test-kvm-hotplug.sh`).
- **VM sandbox dry-run beyond flux-vm.** `POST /v1/sandboxes/{id}/dry-run`
  and `POST /v1/vms/{id}/restore` now also work on QEMU and Firecracker
  sandboxes (stop + relaunch from the snapshot, since their snapshots are
  self-contained), not just the in-tree flux-vm engine. Cloud Hypervisor
  sandboxes don't exist as a create path, so there was nothing to extend
  there.
- **Egress proxy: HTTP/2 interception**, a response body cap, and a
  transparent (non-explicit-proxy) redirect mode.
- **`fluxvm-procbox` isolation and observability.** Namespace isolation that
  no longer requires a user namespace in root mode, per-sandbox uid drop,
  seccomp socket-arg denial, and a `learn` observation-sidecar mode that can
  merge multiple runs.
- **CI:** dedicated workflows exercising native KVM, agent-sandbox,
  `fluxvm-procbox`, and the Python/Go SDKs end to end.

### Changed
- **README and social preview images redesigned** in an Apple.com-style
  blue/white/orange-accent system (light + dark hero, OG cards).

### Added
- **Native KVM hardening.** Preflight of a KVM snapshot before stopping the
  running guest, no silent cold-boot fallback when a native memory restore
  fails, staged atomic snapshot writes (a tag must be deleted before reuse),
  and VHD-footer images rejected as raw sectors.
- **Multi-vCPU in-tree KVM.** 2/4/8-vCPU guests boot (guest CPUID topology,
  `KVM_SET_IDENTITY_MAP_ADDR`), virtio queues are serviced by an
  ioeventfd-fed worker, APs join the pause barrier, and snapshots carry every
  vCPU's registers and MP state (FLUXKVM1 v4).
- **Golden native-agent template.** Static musl guest agent
  (`scripts/build-guest-agent-static.sh`), a Cloud-init image builder with a
  baked systemd-networkd config, and `scripts/setup-native-agent-template.sh`
  to register the `native-agent` vm-template.
- **Agent-sandbox tooling.** HTTP method/host/path egress ACL
  (`egress_http_rules`) with opt-in HTTPS interception
  (`egress_tls_intercept`); sandbox file change-set
  (`POST /v1/sandboxes/{id}/baseline`, `/changes`); rootless `fluxvm-procbox`
  (Landlock + seccomp, TOML profiles, ptrace `learn`) also selectable as a
  sandbox kind with a real-discard `dry-run`; stdlib-only Python and Go SDKs
  (`python/`, `go/`); confidential auto/required detection
  (`GET /v1/host/confidential`).

### Changed
- **Docs: README is now a landing page.** The detailed sections moved verbatim to `docs/`
  (`getting-started`, `architecture`, `proof-and-status`, `vs-libvirt`, `faq`, `kubernetes-operator`,
  `ecosystem`, `index`); every old README anchor still resolves, and `scripts/check-doc-links.py` checks
  relative links and anchors across the docs.
- **QEMU serial is a UNIX socket.** `-serial stdio` became
  `-chardev socket,...,logfile=console.log,logappend=on`, so `/logs` output is
  unchanged; VMs started before upgrading need a restart for `fluxctl serial`.
- **Docs: `NEXT-FEATURES` ranked set marked complete.** Intro, post-GA closes
  (#104 NBD, Keep concurrency, #106 remote seccomp, #108 AppArmor build-image,
  #111 H4), cleared live-evidence archive vs still-open honesty (H2 virtio-win,
  optional `mode=remote` live smoke, CH-preferred virtio-fs depth). No Set 20
  placeholders.
- **Secure Containers live lab gates.** Provision helper
  (`scripts/provision-secure-containers-lab.sh`), evidence runner
  (`scripts/evidence-sc-live-matrix.sh`), and e2e legs for userns-load /
  seccomp-NOTIFY / SELinux-mountLabel. Lab evidence on 175.110.122.71:
  ctr + TTY + namespaces + userns-load + CNI churn PASS; seccomp NOTIFY
  deny PASS (`docs/benchmarks/evidence/sc-live-remaining-*.txt`); SELinux
  mountLabel fail-closed PASS (guest `is_selinux_enabled` false at check);
  CLONE_NEWUSER live PASS via ctr (cross-VM inode distinctness not required).
  Non-goals (remote policy RPC, FC live virtiofs, Kata-equivalence) recorded
  in evidence.
- **Secure Containers userns + mountLabel live fixes.** Guest agent:
  NEWNS before NEWUSER, skip `setgroups` after userns maps, chroot (not
  pivot_root) under userns on virtiofs. Shim: promote
  `io.zyvor.mountLabel` annotation to OCI `linux.mountLabel`. E2e scripts
  use `ctr --seccomp-profile` / annotations (containerd 2.x rejects
  `ctr run --config` with an image ref).
- **Docs: Secure Containers P0 status sync.** Set 4/5 handoff “remaining”
  lists, `deploy/containerd/README` hostPath note, `PRODUCT_OVERVIEW` direct
  datapath row, and `NEXT-FEATURES` intro now match GA multi-VMM / broker /
  CNI code closed in #87 (live e2e under load still open).
- **Secure Containers P0 code:** Cloud Hypervisor virtiofs shared-rootfs
  (`FLUXVM_CONTAINER_BACKEND=cloud-hypervisor`), Multus hybrid/direct secondaries
  (`FLUXVM_CONTAINER_CNI_MULTUS_DATAPATH`), Calico/Flannel CNI providers,
  allowlisted hostPath broker hotplug (`FLUXVM_HOSTPATH_BROKER`, colon-separated
  `FLUXVM_HOSTPATH_ALLOW`), and Firecracker block-staging shares (ext4 pack at
  launch — no virtio-fs upstream). Shim passes `FLUXVM_CONTAINER_KERNEL` on
  Firecracker creates and skips `shared_memory` for that backend.
  Operator env template: `deploy/containerd/env.example`.
  Phase orchestrator: `scripts/complete-secure-containers-phases.sh`.
- **Secure Containers promoted to GA.** RuntimeClass `fluxvm` /
  `containerd-shim-fluxvm-v2` is no longer labeled developer preview. Scope
  boundaries (not Kata-equivalent; Firecracker is block-staging not live
  virtiofs) remain in [docs/secure-containers.md](docs/secure-containers.md).

### Added
- **VM platform (Tier 2).** Label selectors (`GET /v1/vms?label=`,
  `fluxctl list -l`) and bulk `start/stop/restart/delete -l` (delete needs
  `--yes`). VM clone (`POST /v1/vms/{id}/clone`, `fluxctl clone-vm`) of a
  stopped VM with its data disks and labels. QEMU disks
  (`/v1/vms/{id}/disks`, `fluxctl disk list|attach|resize|detach`): qcow2
  data disks hot-added as `scsi-hd`, live `block_resize`. Serial console on a
  QEMU chardev socket teeing into `console.log`, over websocket
  `GET /v1/vms/{id}/serial` and `fluxctl serial`. Root or all-disk backup to
  standalone qcow2 (`POST /v1/vms/{id}/backup`, `fluxctl backup
  [--all-disks]`), live via one temporary snapshot. Label-driven scheduled
  snapshots with retention (`fluxvm.io/snapshot-every`,
  `fluxvm.io/snapshot-keep`). VM templates (`/v1/vm-templates`,
  `fluxctl vm-template`). `GET /v1/openapi.json` (public).
- **fluxctl remote mode.** `--server` / `FLUXVM_URL` (+ `--server-token` /
  `FLUXVM_TOKEN`) and saved `fluxctl context`s drive a remote daemon for the
  core VM verbs, including `serial` (websocket) and `events -f` (SSE); VM
  names resolve against the server.
- **Day-2 VM operations (Tier 1).** `restart`, `PATCH /v1/vms/{id}` (rename,
  labels), snapshot list/delete, lifecycle events (`state_dir/events.jsonl`,
  `GET /v1/events`, SSE `/v1/events/stream`, `fluxctl events -f`),
  `GET /v1/quotas/me`, VM name / UUID-prefix resolution in every fluxctl VM
  argument, `-o json|table|wide`, `fluxctl completions`, `fluxctl wait`, and
  console resize on SIGWINCH.
- **H4 in-tree hypervisor:** virtio-fs attach (virtiofsd + MMIO tag),
  FLUXKVM1 migrate export/import bundles, CPU hotplug (`max_cpus` /
  `KVM_CREATE_VCPU`) and disk hotplug validation. Control API actions
  `MigrateExport`/`MigrateImport`/`HotplugCpu`/`HotplugDisk`.

- **AppArmor profile covers `fluxctl build-image`.** Enforce-mode
  `deploy/apparmor/fluxvm` allows guestkit helpers (kmod/sync/mount/chroot),
  `/run/guestkit*/**`, `/tmp` staging, `owner /**` for specs outside
  `/var/lib/fluxvm`, `ipc_lock`, and NUMA sysfs. Smoke:
  `scripts/test-apparmor-build-image.sh` (CI on ubuntu image-customize).
  Fixes #107.
- **Remote seccomp NOTIFY policy RPC.** Opt-in
  `io.zyvor.seccomp.notify.mode=remote`: the guest broker asks the host over
  AF_VSOCK (`DEFAULT_SECCOMP_POLICY_PORT` / `io.zyvor.seccomp.notify.rpc-port`)
  before answering each notification. Shim listens on the host;
  `FLUXVM_SECCOMP_POLICY_RPC=continue|deny` (default deny) or
  `FLUXVM_SECCOMP_POLICY_RPC_URL` (HTTP POST). Timeouts/errors fail closed;
  syscall args stay off the wire. `addfd` remains local-only.
- **`deny_udp` Keep kill switch.** `VmNetworkPolicy.deny_udp` sets iface
  `reserved0` bit0; TC drops UDP/SCTP after DHCP (reason `udp-deny` / 12).
  Host-side only — no PacketWolf. Pairs with gateway-only broker/proxy ports.
- **Host admission, confinement, and QEMU migration receivers.** O(1) host quota ledger
  (including untracked migration receivers), `policy.require_catalog_names`, symlink-aware
  `allowed_image_dirs` / hostPath broker, optional `FLUXVM_VMM_SECCOMP` for QEMU/Cloud Hypervisor,
  AppArmor/SELinux packaging under `deploy/`, and `POST /v1/migration/receivers` for QEMU
  `-incoming defer`. `kubectl-fluxvm` covers console/exec/pause/resume/delete with SSH node
  hostname validation. Evidence: `scripts/record-baseline.sh`, `scripts/test-migration-receiver.sh`.
- **Phase 6 security profiles.** `CreateVmRequest.security_profile` (`standard` /
  `measured` / `confidential-snp` / `confidential-tdx`) with distinct requested vs
  achieved evidence, host capability discovery, fleet admission that still gates
  an explicit `node`, QEMU SNP/TDX providers, and measured software-test secret
  release. Hardware-free CI: `./scripts/test-security-profiles.sh` /
  [`.github/workflows/security-profiles.yml`](.github/workflows/security-profiles.yml).
  Docs: [docs/security-profiles.md](docs/security-profiles.md),
  [docs/guides/security-profiles-howto.md](docs/guides/security-profiles-howto.md).
  Measured never claims host-memory protection; `*_launch_verified` stays false
  until a real hardware run.
- **Firecracker sandbox templates.** Operator-authored templates with
  `"backend": "firecracker"` are no longer coerced to the in-tree FluxVm backend
  (volumes remain QEMU-only; Firecracker cells cannot use virtiofs).
- **Persistent sandbox volumes.** `POST /v1/sandboxes` accepts `volumes: [{name, guest_path, read_only?}]`.
  Each volume is a host directory under `sandbox.volumes_dir` (default `<state_dir>/volumes`), scoped per
  tenant, shared into the guest over virtiofs and mounted through an fstab line, so it survives sandbox
  replacement. A volume can be attached to one VM at a time. Volumes need a QEMU-backed sandbox: an
  operator-authored template whose `spec.json` says `"backend": "qemu"` is now honoured instead of being
  coerced to the in-tree backend (a client-supplied `spec` is still always the in-tree backend, so a client
  cannot pick QEMU or its `extra_args`). Guest paths are limited to `/home`, `/mnt`, `/data`, `/srv`, `/opt`,
  `/workspace`, `/root` and to `[A-Za-z0-9/_.-]` because they reach generated cloud-init commands.
  `GET /v1/sandboxes` now also lists QEMU-backed sandboxes. There is no size quota: the directory is only as
  bounded as its host filesystem. Snapshot and AutoPause remain in-tree-backend features.

### Fixed
- **Concurrent guest-agent token inject (NBD race).** Concurrent VM creates
  could fail with `/dev/nbd0p1 already mounted` or inject the wrong token
  because guestkit handed two mounts the same `/dev/nbdN` (#104). Fixed in
  sibling [guestkit#35](https://github.com/zyvorai/guestkit/pull/35) (v1.2.5):
  flock around allocate+connect. FluxVM CI/lab pick this up via the sibling
  guestkit checkout — rebuild/redeploy against guestkit ≥1.2.5; Keep can raise
  `ZYVOR_AGENT_SANDBOX_CREATE_CONCURRENCY` once that version is on the host.
- **Dataplane resume after failed/cancelled migration.** Quiesce from `migrate start` is cleared when
  start returns an error, when QEMU reports `Failed`/`Cancelled`, on later terminal status polls, and on
  `migrate cancel` — so abandoned migrations no longer leave the VM edge stuck rejecting new flows.
- **`kubectl-fluxvm` SSH option injection.** `DisposableVm.spec.node` / MicroVM runtime node must be a
  plain hostname and is passed as `ssh -- <node> -- …`.
- **Pod identities were never released.** `pod_identity::forget` had no caller, so every Secure Containers
  Pod left its `pod_uid` -> `pod_id` entry in `pod-ids.json` for the node's lifetime (110 entries had built up
  on the lab node). VM delete now releases it, unless another VM still carries the same Pod UID (a failed
  sandbox create is retried on a new VM that may already be running). Verified live, warm-pool and cold: the
  entry appears while the Pod runs and is gone after it is deleted.
- **A bridge-hotplugged NIC leaked its tap.** `hotplug_nic` (the warm-pool bridge-chain path) never set the
  VM's `tap_name`, and stop/delete clean the network only for a record that names its primary tap, so the
  `hn0…` tap outlived every claimed VM (found live: one stray tap after a warm-pool bridge Pod). It is now
  recorded with the first hotplugged NIC; verified live (no links left after delete).
- **Secure Containers: a Pod could not be deleted.** On a live k3s + Cilium node `kubectl delete pod`
  left Pods `Terminating` indefinitely (in `direct` and `bridge` mode alike). Three causes, all found on a
  clean, freshly restarted containerd:
  1. `fluxvm-container-agent` moved only the outer (reaper) process into the container cgroup, after it had
     already forked the workload, so the workload stayed in the agent's own cgroup. `Kill(all)` by cgroup missed
     it, the wrapper died on SIGTERM and orphaned it, and it kept the stdio pipes open, so containerd's
     `task.Delete` waited for EOF forever. The outer process now joins the cgroup before forking, and forwards
     SIGTERM/SIGINT/SIGHUP/SIGQUIT/SIGUSR1/SIGUSR2 to the workload (as runc's init does). This also means cgroup
     limits, the device policy and the in-guest network policy now actually apply to the workload.
  2. That in-guest policy (`bpf/fluxvm_guest_cgroup.bpf.c`) keyed inbound packets by
     `bpf_get_current_cgroup_id()`, the interrupted task's cgroup, so once workloads really were in their cgroup all
     inbound traffic was dropped (fail-closed). It now uses `bpf_skb_cgroup_id()`.
  3. containerd runs the shim's `delete` subcommand whenever one container's connection closes; it destroyed the
     whole Pod VM, powering the guest off under the still-running sandbox (StopPodSandbox then failed forever
     against a vanished VM). It now only destroys the VM when no other task remains.
  4. The shim process itself stayed behind after every Pod (one idle process per Pod). containerd closes the
     connection ~3 s after `Shutdown`, and ttrpc then drops the in-flight handler; the handler awaited the
     daemon's VM delete (a graceful guest power-off takes far longer), so it was cancelled before it signalled the
     shim to exit, and the VM record, CNI netns alias and shim were all left. The teardown now runs on a detached
     task, so it completes and the shim exits.
  5. Found while exercising the direct datapath live: two Pods created at the same instant raced on one fixed
     `pod-ids.json.tmp` (bare ENOENT, so the VM create was rejected and `auto` silently fell back to the bridge
     chain); the pod-id and ipcache stores now use a lock and unique temp files.
  6. `VmStore::update` was an upsert, so a late writer could resurrect a VM that `delete` had just removed; it now
     only replaces an existing record.
  7. A rejected direct create left a `failed` VM record behind on every `auto` fallback; the shim now removes it.
  8. With workloads truly inside their cgroup the OCI device policy is enforced, and containerd's default
     "deny all" would leave containers unable to open the standard devices; the agent now adds runc's default
     allowed-device list (unit-tested).
  8b. `/dev` inside a Secure Container was the image's empty directory, so `/dev/null`, `/dev/zero` and
     `/dev/urandom` did not exist (`iperf3`: "failed to open /dev/urandom"; `dd if=/dev/zero` failed). This
     predates the cgroup change (an A/B against the pre-fix agent behaves the same). The agent bind-mounted the
     container rootfs onto itself *non-recursively* before `pivot_root`, which dropped the `/dev` tmpfs the OCI
     mounts had just placed under it; the self-bind is now recursive. The nodes are also `chmod`ed after `mknod`,
     since `mknod` applies the umask. Verified live: `/dev/null` (writable by a non-root user), `/dev/zero` and
     `/dev/urandom` work, and `/dev`, `/dev/shm`, `/proc`, `/sys`, ConfigMap volumes and `resolv.conf` all survive
     the pivot.
  9. A NIC hotplugged into a warm-pool VM is enumerated by the guest a moment after `device_add` returns; the shim's
     guest network script now waits for it instead of failing with an empty "exit=1".
  Live result: a Pod is fully deleted in ~13 s with nothing left behind (no tap/veth links, netns aliases,
  QEMU, VM records, shim processes).

### Added
- **Warm-pool Pods can run containers.** A pool member keeps the virtiofs shares its template was created with,
  so a Pod's container rootfs (staging directory, kubelet volumes) could not be delivered and a warm-pool Pod
  never became Ready. New:
  `CreateVmRequest.shared_memory` (boot with shareable memory so shares can be hot-added; set it on pool
  templates), and `POST /v1/vms/{id}/hotplug/share` (admin, QEMU): starts a `virtiofsd` for a host directory and
  hot-plugs a `vhost-user-fs-pci` device onto one of the reserved top PCIe root ports (`fs{N}`, N = share
  index; the `virtiofsd` is respawned per port attempt because it serves one connection). With
  `FLUXVM_CONTAINER_WARM_POOL` the shim hot-adds the Pod's shares right after the NIC. Live: a Pod claimed from a
  warm pool was Ready in **7 s** (a cold Pod takes ~140 s), with the container started, `logs`, `exec`, a
  ConfigMap volume and `/dev` working; two shares hot-added, mounted, read and written from the guest.
  Pool templates must have no `shared_folders` of their own: the shim expects the Pod's shares to be `fs0…`.
- **Warm-pool Pods get their eBPF Pod identity.** A pool member is booted before any Pod exists, so a
  claimed VM never carried `pod_uid` and Pod-scoped network policy (`/v1/vms/{id}/network/pod-policy`,
  "VM has no associated Pod identity") could not apply to it. `POST /v1/pools/{name}/claim` now takes a
  `pod_uid` (validated: 1–128 of `[A-Za-z0-9._-]`, checked before a member is taken), the shim sends the Pod's
  UID with `FLUXVM_CONTAINER_WARM_POOL`, and the identity is minted when the NIC is hot-added. A bridge-chain
  hotplug (`hotplug_nic`) now also attaches the dataplane when the VM has a Pod identity (the direct hotplug
  already did). Live, direct and bridge: the record carries the Pod's UID, the eBPF meta `pod_id` equals the
  `pod-ids.json` entry, and setting a Pod-scoped policy on the claimed VM returns 200. Enforcement against
  peer traffic was not exercised.
- **Multus secondaries exercised live.** With Multus in the CNI chain (a reversible rename; Cilium's
  `cni-exclusive` had to be relaxed while testing and was restored), a Secure Container Pod annotated with a
  NetworkAttachmentDefinition got `net1`, with `auto` using the bridge chain and reporting why, `direct` refusing
  as documented, and host ⇄ guest `net1` reachable (3/3 both ways). Ordinary Pods keep working through
  Multus → Cilium.
- **Cilium CNI (Secure Containers)** — shim `FLUXVM_CONTAINER_CNI_PROVIDER`
  (`auto`/`cilium`/`generic`) with Multus-safe `netN` filtering, eth0-preferring
  L2 handoff, `configs/cilium-cni.toml`, `docs/cilium-cni.md`,
  `deploy/k8s/cilium/`, and `scripts/evidence-cilium-cni.sh`. Does not write
  Cilium private maps.
- **Direct (bridge-less) datapath** — an eBPF redirect between the outer device and the
  VM's tap in place of a bridge, with Cilium's `lxc*` hooks untouched
  (`docs/direct-datapath.md`). Secure Containers: shim
  `FLUXVM_CONTAINER_CNI_DATAPATH=bridge|direct|auto` (default `auto`; live
  Cilium + KVM evidence passed, `docs/benchmarks/evidence/direct-datapath-live-20260919T202359Z.txt`), tap created inside the Pod netns and handed to
  QEMU as a descriptor, warm-pool NICs via QMP `getfd`. Standalone: `network.direct`
  with `l2-uplink` (shared per-uplink steering maps, ARP steering by `guest_ips`,
  several VMs on one unbridged NIC), `MicroVM.spec.networkMode: direct`,
  `POST /v1/vms/{id}/hotplug/nic` `direct` body. Also: `redirect_peer` capability
  probe, packet-flow hop view for bridge-less VMs, `scripts/bench-direct-datapath.sh`
  (bridge vs direct forwarding cost), `scripts/evidence-direct-datapath.sh`.
- **S1 depth** — `scripts/e2e-networkpolicy-s1-depth.sh` proves mid-flow TCP
  kill after CT-clear revoke and SYN anti-replay under reconnect churn.
- **S2** — `scripts/evidence-networkpolicy-second-cni.sh` (second kubeconfig or
  nftables policy-engine stand-in) alongside `REQUIRE_MULTI_NODE=1` Set 17.
- **S9** — Kata P0/P1 matrix (`scripts/evidence-kata-p0p1-matrix.sh`,
  `tests/oci-fixtures/`), hostPath allowlist broker
  (`fluxvm.io/hostpath-allow` / `FLUXVM_HOSTPATH_ALLOW`), and
  `SECCOMP_IOCTL_NOTIF_ADDFD` notify mode (`io.zyvor.seccomp.notify.mode=addfd`).
- **S10 / S11** — `scripts/evidence-fleet-multihost.sh` and
  `scripts/evidence-migration-attached-vm.sh` wired into the Set 19 GA gate.
- **H2** — virtio-pci ECAM device at `00:01.0` with BAR0 sizing + ACPI MCFG.
- **H3** — vhost-net mem-table + VRING NUM/ADDR/BASE/KICK/CALL; kernel datapath
  kicks when rings are programmed (userspace pump remains fallback).

### Changed
- **CLI rename: `fluxvm-cli` → `fluxctl`** — crate path `crates/fluxctl`,
  package `fluxctl`, binary `/usr/local/bin/fluxctl` (clap name `fluxctl`).
  `fluxctl serve` is the control-plane daemon. Deploy/Makefile also install a
  `fluxvm` → `fluxctl` symlink for one release of command compatibility;
  prefer `fluxctl` in new scripts and docs.
- **README and social preview rewritten** — benefit-led hero, a "Proof & status" section
  tying each claim to its evidence, and a refreshed 1280×640 social image
  (`docs/assets/social-preview.svg` → `.png`). Every existing README anchor is preserved.

### Fixed
- **VM-edge eBPF program rejected on Linux 7.x** — `fluxvm_tc.bpf.o` exceeded the
  verifier's 1,000,000-instruction limit on 7.0 (the pod-policy rule scan inlined
  the whole rule match on each of 64 iterations, in both address-family paths). The
  per-rule match is now a global BPF function verified once (needs Linux >= 5.5);
  the object uses ~17% of the limit. `scripts/test-verifier-budget.sh` guards it,
  `scripts/test-pod-policy-verdict.py` proves matching is unchanged, and
  `scripts/test-ebpf-smoke.sh` (stale since Set 19: no `fluxvm_pridx` writes or
  binding) passes again.
- **Attempted the Set 9S guest `aya` ELF-parse fix** —
  `docs/secure-containers-set9s.md` hypothesized the guest LSM MAC test's
  intermittent "error parsing ELF data" failure as a version/feature-
  unification mismatch between `aya` 0.13.1 and the workspace's pinned
  `object 0.36.7`, naming a newer `aya`/`aya-obj`/`object` pin as the
  likely fix. Bumped `fluxvm-container-agent`'s `aya` dependency to
  `"0.14"` (pulling `aya-obj` to 0.3.0 and `object` to 0.39.1). `cargo
  build`/`cargo test -p fluxvm-container-agent` (30 tests) pass clean, no
  API breakage from the two non-patch bumps, and Set 8S's own
  `aya`-loaded `cgroup_skb` test was confirmed to genuinely execute under
  real root and pass. **Honest limit**: Set 9S's own test — the one that
  actually exercises the failing code path — could not be run at all on
  the available build host, even as root, because that host's kernel has
  no `bpf` in its active `lsm=` boot parameters. This means the bump has
  *not* been validated against the real failure mode; there is no more or
  less confidence it's fixed than before, only that it doesn't regress
  anything else. See `docs/secure-containers-set9s.md` for the full
  account — not yet root-caused to a confirmed fix.
- Fixed a stale Secure Containers CI gate: `secure-containers-coverage.yml`'s
  `go-controller`/`go-observer` jobs were still pinned to Go 1.23 via
  `actions/setup-go`, even though both
  `controllers/fluxvm-networkpolicy-controller` and
  `tools/fluxvm-policy-observer` require `go 1.27` in their own `go.mod` —
  every run failed outright at the toolchain-version check, before a
  single test ran. Switched both jobs to `go-version-file` pointing at
  each module's own `go.mod`, so this can't silently drift again.
- **`enforce_token_quotas` counted every VM on the node, not just the calling
  token's own, against `max_vms_per_token`/`max_memory_mib_per_token`** — the
  function's own prior comment called this "conservative," but with two
  tokens both configured with a quota of 10, the first token to reach 10 VMs
  blocked every *other* token from creating any more: a quota meant to
  contain one noisy tenant instead let that tenant deny service to every
  other tenant, the opposite of what a per-token quota is for. Fixed by
  stamping a new `CreateVmRequest.created_by_token` server-side in
  `create_vm`/`create_sandbox` from the authenticated caller (never trusted
  from the request body) and filtering by it. Deliberately *not*
  `skip_deserializing`: `fluxvm-storage::Store` round-trips every `VmRecord`
  through the same serde codec on every `list()`/`get()`, so a
  `skip_deserializing` field silently comes back `None` after the first
  storage round trip — this broke quota enforcement outright during
  development (caught by a failing regression test) before being fixed by
  relying on the handler's unconditional overwrite instead, proven by a
  dedicated spoofing test. `enforce_token_quotas` also moved from
  `fluxvm-api` into `VmManager` (`fluxvm-scheduler`) so `create_vm` and
  `create_sandbox` share one implementation — `create_sandbox` previously
  skipped quota enforcement entirely, a second real gap this closes. 3 new
  tests: cross-token isolation, request-body spoofing resistance, and a
  serde round-trip proving the persistence path the `skip_deserializing`
  attempt broke.
- **The sandbox HTTP proxy had no cap on the guest's response body** —
  `sandbox_proxy_inner` capped the *request* body at 16 MiB but the
  *response* read from the guest (`up.into_body().collect()`) had only a
  30s timeout, no size limit at all: a malicious or compromised guest could
  stream an effectively unbounded response for up to 30s per request, a
  straightforward host-memory-exhaustion primitive from inside any tenant's
  own sandbox. Fixed with one named constant,
  `SANDBOX_PROXY_MAX_BODY_BYTES` (16 MiB), now shared by both the existing
  request cap and a new response cap via
  `Limited::new(up.into_body(), SANDBOX_PROXY_MAX_BODY_BYTES).collect()`.
  Factored the handshake/send/receive half of `sandbox_proxy_inner` into a
  new `proxy_over_tcp` helper, testable without `CAP_SYS_ADMIN`/
  `connect_in_netns`: `oversized_guest_response_body_is_rejected_not_fully_buffered`
  streams a genuine over-cap response from a real loopback `TcpListener`
  standing in as the guest and asserts the proxy doesn't return it fully
  buffered.
- **The sandbox HTTP proxy routes had no role check at all** —
  `sandbox_http_proxy`/`sandbox_http_proxy_default_port` took no
  `Extension<Role>` parameter, unlike every other guest-reaching route
  (`sandbox_fs_read`, `sandbox_process`, `agent_exec`, `qga_exec`, ...),
  which all gate on `require_admin(role)?`. Since the route is registered
  with `any(...)`, a `read-only` token could issue arbitrary HTTP methods
  into a tenant's guest through it — the same shape as the already-fixed
  `POST /v1/egress/check` gap above, missed by that earlier audit because
  these two handlers return a bare `Response`, not `ApiResult<T>`, so the
  `require_admin` error needs `.into_response()` instead of `?`. Fixed by
  adding the same gate both handlers' siblings already have. 2 new tests
  (`readonly_token_cannot_call_sandbox_proxy`/
  `admin_token_can_reach_sandbox_proxy_role_gate`).
- **A real, currently-exploitable cross-tenant bypass on the short-form
  sandbox proxy route** — `extract_vm_uuid` only recognized `/v1/vms/` and
  `/v1/sandboxes/` path prefixes; `tenant_guard_middleware` silently no-ops
  when it returns `None`. The router registers a third, separately-named
  proxy route matching neither prefix, `/sandbox/{id}/{*path}`
  (`sandbox_http_proxy_default_port`) — the actual AI-agent-facing entry
  point per `docs/agent-sandbox-gaps.md`'s "Guest HTTP reverse proxy"
  description, distinct from the already-correctly-scoped long-form
  `/v1/sandboxes/{id}/http/{port}/{*path}` route. Any authenticated caller
  who knew or guessed another tenant's sandbox UUID could proxy HTTP
  traffic straight into that tenant's guest through this specific route.
  Fixed by teaching `extract_vm_uuid` the third prefix — no handler changes
  needed, `tenant_guard_middleware` already wraps the whole router as an
  outer layer. New test,
  `short_form_sandbox_proxy_route_is_tenant_scoped`, mirroring the existing
  `sandbox_id_routes_are_tenant_scoped_like_vm_routes` test exactly.
- **A wedged `fluxvm-hypervisor` child could hang `pause`/`resume`/`shutdown`/
  snapshot control requests forever** — `control::request` had no timeout on
  its Unix-socket round trip, so a stuck vCPU thread still holding `VmState`'s
  lock, or a hung KVM ioctl inside `dispatch()`, left the caller's
  `next_line().await` blocked with no way to notice or recover — the same
  class of "no timeout on I/O to something outside this process's control"
  bug already fixed for the sandbox HTTP proxy, below, just one layer down:
  the peer here is the hypervisor subprocess itself, not a guest. Every
  request kind now gets a default 15s bound, with snapshot save/restore
  (which chain pause + snapshot I/O + resume inside one call) given 45s of
  headroom. 4 new tests, including a regression test that reproduces the
  exact wedge (server accepts and reads the request, then never answers).
- **A `tap`+`netns=true` sandbox's HTTP proxy hung indefinitely instead of
  reaching the guest** — `sandbox_proxy_inner` connected via a plain
  `reqwest::Client`, which always dials from the calling thread's ambient
  network namespace; a netns-sandboxed guest's `guest_ip` is only routable
  from inside that VM's own namespace, so every request over this path
  silently hung until timeout (vsock-based guest exec/file access was
  unaffected, since vsock doesn't route through the guest's netns at all —
  a fully healthy guest looked completely unreachable over HTTP alone). New
  `connect_in_netns()` `setns()`s a throwaway `std::thread` (never tokio's
  blocking pool, which reuses threads and would leak the namespace change
  into unrelated work) before calling `connect()`, then hands the connected
  socket back to the async runtime; since `reqwest` has no way to accept a
  pre-connected socket, the proxy now speaks HTTP/1.1 directly over that
  stream via `hyper`. Two related framing/hang bugs surfaced once this path
  was actually exercised end-to-end and fixed alongside it: forwarding the
  inbound `Content-Length`/`Transfer-Encoding` headers onto hyper's own
  `Full` body (which computes its own) duplicated them, leaving the guest's
  HTTP server waiting on body bytes that were never coming on any non-empty
  request; and the guest-response body read had no timeout of its own,
  so a guest that accepted a request but never finished its response body
  hung the whole proxy call, while forwarding the guest response's
  `Connection`/`keep-alive` headers onto the caller's own connection made
  the caller's pool try to reuse a connection axum had already closed,
  surfacing as an unexplained "connection closed before message completed".
- **A Firecracker guest kernel could block forever on `getrandom()` before
  its CRNG was seeded** — with no fast entropy source (no RDRAND passthrough,
  no virtio-rng), a guest boots with `crng_init=0`, and anything that calls
  `getrandom()` before enough environmental noise accumulates — in practice,
  the guest's own init/runtime startup — blocks indefinitely, indistinguishable
  from a hung process from the outside: alive, no output, nothing listening.
  `firecracker_config()` now unconditionally attaches Firecracker's built-in
  entropy device, and `FluxVmBackend`'s default kernel command line adds
  `random.trust_cpu=on rdrand=force` so the guest trusts RDRAND directly and
  seeds fully at boot even without a virtio-rng driver built into its kernel.
- **`start_from_snapshot` silently ignored a `loadvm_tag` on `Firecracker`/`FluxVm`
  VMs instead of rejecting it** — `create_vm_snapshot` (the save side) already
  correctly bails with "snapshot not supported for backend ..." for every
  backend but `Qemu`/`CloudHypervisor`, but `start_from_snapshot` (the
  restore side) had no matching check of its own: it set `loadvm_tag` on the
  launch request and called `start_impl` unconditionally. Neither
  `FirecrackerBackend`'s nor `FluxVmBackend`'s `launch` ever reads
  `req.loadvm_tag` at all, so a `start_from_snapshot` call against either
  backend silently produced an ordinary cold boot instead of a restore —
  the caller believed they got a VM resumed from saved state when they
  silently didn't, with no error naming the mismatch. New
  `snapshot_backend_error` is now shared by both the save and restore
  paths so they can never drift apart again. 2 new tests.
- **`fluxvm-cloud-hypervisor`'s `snapshot_save` had the exact same
  resume-failure-discard bug as `fluxvm-qemu`'s, below** — found while
  auditing every other `let _ = ...`-discarded `Result` across the crates
  that fix didn't already cover. `snapshot_save` pauses (`ch-remote pause`),
  takes the snapshot (`ch-remote snapshot`), then resumes
  (`ch-remote resume`) — but the resume call's result was also a bare
  `let _ = ...`, so a resume failure *after a successful snapshot* still
  returned `Ok(())`, leaving the VM silently paused with no error surfaced.
  Fixed the same way: both failure combinations are now reported
  explicitly. 4 new tests against a scripted fake `ch-remote` shell script
  (this backend shells out to a real binary rather than a QMP socket),
  covering the success path, resume-fails-after-save-succeeds, both-fail,
  and snapshot-fails-but-resume-succeeds.
- **`fluxvm-qemu`'s internal-snapshot resume failure was silently discarded** —
  `snapshot_save` pauses a running VM (QMP `stop`), takes the internal
  `savevm` snapshot, then resumes it (QMP `cont`) — but the `cont` call's
  result was a bare `let _ = ...`, so a resume failure *after a successful
  snapshot* still returned `Ok(())`. A caller acting on that `Ok` believed
  the VM was still running when it had actually been left paused, with no
  error surfaced anywhere to explain why. Fixed: both failure combinations
  (snapshot failed and/or resume failed) are now reported explicitly,
  naming which one happened; a snapshot that saved fine but failed to
  resume now returns an error saying exactly that, rather than a clean
  `Ok(())`. 4 new tests against a scripted fake QMP socket, covering the
  success path, resume-fails-after-save-succeeds, both-fail, and the
  already-paused VM (never told to `stop`/`cont` at all) case.
- **Warm pools (`/v1/pools`) had no tenant scoping at all** — the same gap
  as sandboxes above, in the same audit pass: `list_pools` had no tenant
  filter, `get_pool`/`delete_pool`/`claim_pool` (name-keyed, so
  `tenant_guard_middleware`'s UUID-based extraction never covered them
  either way) had no tenant check, and `create_pool` never forced the
  caller's own token `tenant` onto `spec.template.tenant`. The last one
  is the most consequential: every member ever backfilled from an
  untenanted pool inherits `template.tenant` unchanged, so a tenant-scoped
  admin token creating a pool without this fix would have produced members
  no tenant-scoped token — including its own creator, after claiming one —
  could ever reach via `/v1/vms/{id}/...`. Fixed: `create_pool`
  inherits/rejects the token tenant on `spec.template` exactly like
  `create_vm`; `list_pools` filters by tenant like `list_vms`;
  `get_pool`/`delete_pool`/`claim_pool` 404 for a mismatched tenant before
  reaching the underlying `VmManager` call; `claim_from_pool` additionally
  rejects (and cleans up, matching its own existing resume-failure
  cleanup) a claim whose resumed member's tenant doesn't match the
  claiming token's. 6 new tests.
- **Sandboxes (`/v1/sandboxes`) had no tenant scoping at all** —
  `tenant_guard_middleware` only ever recognized `/v1/vms/{id}...` paths, so
  every `/v1/sandboxes/{id}/...` route (snapshot, `fs/read`, `fs/write`,
  `process`) was reachable by any tenant-scoped token for any tenant's
  sandbox by UUID, not just its own; `list_sandboxes` had no tenant filter
  at all, unlike `list_vms`; and `create_sandbox` never forced the caller's
  own token `tenant` onto the resolved spec the way `create_vm` already
  does, so a tenant-scoped admin token could create a sandbox with no
  tenant at all, or an explicit `spec.tenant` naming a *different* tenant
  outright — either would have made the per-record tenant check
  meaningless for that sandbox from the moment it was created. Found by
  auditing tenant-scoping coverage across every `VmRecord`-backed id space,
  not just `/v1/vms`, following the same "audit every X for a Y" method
  that already found the `egress/check` gap below. Fixed: `extract_vm_uuid`
  now also recognizes `/v1/sandboxes/{id}...` (a sandbox is a `VmRecord`
  like any other); `list_sandboxes` filters by the caller's token tenant
  exactly like `list_vms`; a new `enforce_sandbox_tenant`
  (`fluxvm-scheduler::sandbox`) inherits/rejects the token tenant on the
  resolved spec, covering both the `template` and explicit-`spec` paths
  uniformly. 6 new tests (4 pure unit tests on `enforce_sandbox_tenant`, 2
  router-level regression tests proving cross-tenant list/id-route access
  is denied).
- **`POST /v1/egress/check` had no role check at all** — every other mutating
  route calls `require_admin`, but this one never did, so a `read-only`
  bearer token could call it directly and get back a real secret: when the
  checked host matches a configured `[sandbox] credential_vault` entry, the
  response includes that entry's plaintext `inject_authorization` value,
  which only an `admin` caller should ever see. Found by auditing every
  mutating route in `fluxvm-api::router` for a `require_admin` call — this
  was the only one missing it. Now returns 403 for `read-only` tokens,
  matching `docs/api.md`'s own documented RBAC contract ("any mutating
  route ... returns 403"). 2 new tests
  (`readonly_token_cannot_call_egress_check`/
  `admin_token_can_call_egress_check`).

### Added
- **F1 Hubble SID attribution** — `packetflow::from_flow_record_attributed`
  overlays CEP / Cilium-agent SecurityIdentity onto hubble observe flows
  (source + peer-VM destination); Fabric CT id remains the fallback. Never
  writes Cilium private maps.
- **F2 Prometheus scrape for `:9108`** — microvm node-agent exposes
  `MICROVM_METRICS_ADDR=0.0.0.0:9108` with scrape annotations;
  `deploy/k8s/microvm/servicemonitor.yaml` + `deploy/prometheus/fluxvm-scrape.yaml`.
- **S8 Observer evidence path** — `scripts/evidence-policy-observer-scrape.sh`
  archives `/metrics` + RSS into `docs/benchmarks/evidence/`.
- **H1 FLUXKVM1 v3 virtio live-state** — snapshot packs/restores virtio queue
  rings + status/features; disk/TAP paths still from boot config.
- **H3 vhost-net partial bind** — `VHOST_SET_OWNER` + `VHOST_NET_SET_BACKEND`
  for queues 0/1; VRING GPA / mem-table programming remains follow-up.
- **Firecracker capability-figure adoption** — [docs/capability-figures.md](docs/capability-figures.md)
  maps FC design/SPEC targets (≤5 MiB overhead, ≤125 ms boot, 5 microVMs/core/sec)
  onto FluxVM benches and threat-containment layers. `[jailer] enforce` (or
  `auth.require` + non-loopback listen) fail-closes serve/launch unless jailer
  is enabled; jailed boots default to `8250.nr_uarts=0`. Create fields
  `net_mbit_limit` / `net_pps_limit` / `blk_mbit_limit` / `blk_ops_limit` wire
  Firecracker virtio `rate_limiter` JSON. Benches report
  `mutation_per_core_sec`, `boot_to_ready_ms`, optional VMM RSS.
- **Positioning** — FluxVM is no longer framed as a “Disposable Compute Engine”;
  it is a general Rust-native VM control plane. Disposable / short-lived
  workloads remain a supported use pattern (optional TTL, CoW), not the product
  identity. `DisposableVm` CRD name unchanged.
- **Firecracker adoption reframed for general CP** — [docs/capability-figures.md](docs/capability-figures.md)
  splits Track A (production isolation for all guests) vs Track B (optional
  microVM density). Virtio rate limits now propagate through `fluxvm-hypervisor`
  BootConfig as well as the plain Firecracker backend; added
  `configs/production-hardening.toml` and NEXT-FEATURES FC1–FC3.
- **FC1–FC3 complete (multi-backend)** — oversub policy defaults +
  [docs/oversubscription.md](docs/oversubscription.md); static `cpu_template`
  for Firecracker/FluxVm-firecracker; `/v1/vms/{id}/snapshot` for Firecracker
  and FluxVm (CH/QEMU already supported).
- **`GET /fleet/nodes/{name}`** — exactly one node's own fleet record
  (`healthy`/`cordoned`/`labels`/free-capacity), without a caller having to
  fetch and filter the whole `GET /fleet/nodes` list just to check one
  node's state in isolation. `404` for a name that was never registered,
  matching the status `cordon`/`uncordon`/`deregister` already use for the
  identical case on this same path segment (unlike
  `GET /fleet/nodes/{name}/vms`'s `400`, since that route's unknown-name
  case is a bad *proxy target*, not a missing *resource*). `fluxctl fleet
  node NAME` is the matching CLI form.
- **Label/`nodeSelector`-aware placement for the fleet registry** —
  automatic `POST /fleet/vms` placement was purely capacity-based
  (`pick_best_capacity` scored nodes only on residual vCPU/memory), with no
  way to express "only nodes in zone X" or "only nodes with a GPU" short of
  pinning an exact `--node`, which throws away all failover. `NodeInfo`/
  `RegisterRequest` gain a `labels: HashMap<String, String>` (serde-default,
  backward compatible with an older node agent that predates labels);
  `fluxvm-agent node` gains a repeatable `--label key=value` flag, carried
  on every heartbeat. `POST /fleet/vms` accepts an optional `"nodeSelector"`
  object; placement now filters candidates to nodes whose labels are a
  superset of it (Kubernetes `nodeSelector` semantics) before scoring
  capacity, threaded through every retry of the failover loop below, not
  just the first pick. An unmatched selector fails with a clear 503 naming
  it. An explicit `"node"` target still bypasses the selector entirely,
  same as it bypasses cordoning. `GET /fleet/nodes` now reports each node's
  labels; `fluxctl fleet create --node-selector key=value` is the matching
  CLI form. 10 new tests.
- **Automatic fleet-wide VM placement now fails over to the next-best node
  when the first pick is unreachable**, instead of hard-failing the create
  outright. `pick_best_capacity` chooses the highest-residual-capacity node
  using only heartbeat data; a node only gets excluded once its heartbeat
  has gone stale past `HEALTHY_WINDOW_SECS` (30s), so a node that crashed,
  hung, or dropped off the network moments after its last heartbeat still
  looked "healthy" and got picked, hard-failing the create even while other
  schedulable nodes sat idle. New `pick_best_capacity_excluding` (exclusion-
  set variant; `pick_best_capacity` is now a thin wrapper, so existing call
  sites/tests are unaffected) and a `DispatchError::{Unreachable,Rejected}`
  split: an unreachable node (connection failure, or a response that isn't
  valid JSON) is excluded and the next-best remaining candidate tried,
  looping until one accepts the create or every schedulable node has been
  tried; a node that reached its own `fluxctl serve` and explicitly rejected
  the request (bad fields) is never retried elsewhere, since every other
  node would reject the identical body identically. An explicit `"node"`
  target is unaffected — still a single, non-retried attempt, so a caller
  who pinned a node gets an honest failure instead of a surprise landing
  somewhere else. 4 new tests.
- **`fluxctl fleet` — CLI parity for the fleet registry's REST API**
  (`nodes`, `cordon`, `uncordon`, `deregister`, `capacity`, `create`,
  `vms`, `node-vms`, `delete`, `node`). Every fleet operation used to mean
  a raw `curl` call; this gives `fluxvm-agent central`'s multi-host fleet
  registry the same CLI-parity treatment `migrate`/`ping`/`copy-to` already
  gave the per-node REST API, just against the central registry instead of
  a node's own `fluxctl serve`. New `crates/fluxctl/src/fleet_client.rs`,
  a thin `reqwest`-based client speaking the same opaque-JSON boundary
  `fluxvm-agent::central` itself uses (it treats `CreateVmRequest`/
  `VmRecord` bodies as opaque JSON rather than depending on `fluxvm-core`,
  and this client sits on the other side of exactly that boundary). Real
  integration tests spin up the actual `fluxvm-agent::central` router, not
  mocks.
- **`GET /fleet/capacity`** — a fleet-wide aggregate distinguishing total,
  free, and schedulable capacity (a cordoned or unhealthy node still counts
  toward total/free but not schedulable), closing the gap between
  `GET /fleet/nodes` listing per-node numbers and there being any single
  answer to "how much room is left in the fleet right now."
- **`fluxctl resources` — CLI parity for the cgroup resource patch.**
  `POST /v1/vms/{id}/resources` has existed since cgroup v2 resource control
  landed, but was explicitly left REST-only when `freeze`/`thaw`/`frozen`
  closed the rest of this section's CLI gap (see that entry above): it takes
  a multi-field `ResourcePatch` that deserved real flag design instead of a
  JSON blob shoved onto the command line. `fluxctl resources <id>
  [--cpu-quota-percent N] [--memory-max-bytes N] [--io-weight N]
  [--pids-max N] [--cpuset-cpus SPEC]` maps one flag per `ResourcePatch`
  field and preserves its "only touch what's set" contract exactly — a
  field omitted from the command line is left alone, not reset — with the
  CLI itself refusing a bare `fluxctl resources <id>` with no flags at all
  (a plain `bail!` in the match arm; clap's own `Option`-only shape has no
  way to express "at least one of these" at the parser level) rather than
  silently issuing a no-op `POST`. `--cpuset-cpus` accepts the exact set
  syntax `cpuset.cpus`/`cpuset.cpus.effective` themselves read back in
  (`"0-3"`, `"0,2,4"`, `"0-1,4-5"`, per `fluxvm_cgroup::cpuset`'s own
  `parse_set`/`format_set`), via a new, independent `parse_cpuset_spec` that
  additionally rejects an empty string (ambiguous here, since "leave
  cpuset pinning untouched" is already expressed by omitting the flag) and
  a reversed range like `"5-2"` — which the existing cgroup-side parser
  would silently expand to an empty range under plain `start..=end` and
  apply as "pin this VM to no CPUs at all," a typo failing open into a
  much worse outcome than a parse error. 17 new tests: CLI-argument-parsing
  coverage per flag and for all five together, the "parses fine at the
  clap layer but the match arm still refuses zero flags" split, a
  distinctness check against `freeze`/`pause`, and dedicated
  `parse_cpuset_spec` coverage of every accepted and rejected shape.
  Verified building, `cargo test -p fluxctl` (50/50), `cargo clippy -p
  fluxctl --no-deps` (clean against this change), and `cargo fmt -p
  fluxctl -- --check` on the Linux remote (this crate doesn't build on
  macOS, same as always). `set_resources` itself is unchanged and was
  already verified against real `memory.max`/`cgroup.procs` files by
  `scripts/test-cgroup-resources.sh` when the REST route first landed —
  this only adds a CLI front end for it.
- **`fluxctl freeze`/`thaw`/`frozen` — CLI parity for the cgroup v2 freezer.**
  `POST /v1/vms/{id}/freeze`, `POST .../thaw`, and `GET .../frozen` have
  existed since cgroup v2 resource control landed (see "Resource control
  (cgroup v2)" further down this changelog), but — like `resources`,
  `stats`, and `pressure` alongside them — there was no CLI form at all;
  triggering one meant a raw `curl` call. This one mattered more than the
  other three cgroup routes because it's easy to reach for by mistake:
  `freeze`/`thaw` act on the cgroup freezer directly (`cgroup.freeze`),
  stopping every process in the VM's `fluxvm.slice/{id}.scope` at the
  kernel level, which is a different mechanism from `pause`/`resume`
  (already CLI-native) — those go through the VMM's own control socket
  (QMP `stop`/`cont` for QEMU, `ch-remote pause`/`resume` for Cloud
  Hypervisor) and do nothing to help when that socket itself is wedged,
  exactly the case `freeze` covers. `fluxctl freeze <id>` and `fluxctl thaw
  <id>` call `VmManager::freeze`/`::thaw` directly; `fluxctl frozen <id>`
  reports the freezer's current state as `{"frozen": true|false}` without
  changing anything, matching `GET .../frozen` exactly. All three are
  bodyless GETs/POSTs against an existing `VmManager` method, so no new
  wire types were needed; `resources`/`stats`/`pressure` stay REST-only —
  `resources` takes a multi-field patch that deserves real flag design of
  its own, and the other two are read-only introspection with no CLI
  urgency yet. 7 new CLI-argument-parsing tests, including one proving the
  three new commands aren't accidentally aliased to each other or to
  `pause`/`resume`.
- **`fluxctl migrate start`/`status`/`cancel` — CLI parity for live migration.**
  `POST /v1/vms/{id}/migration/start`, `GET .../migration/status`, and
  `POST .../migration/cancel` have existed since the QEMU (and, more
  recently, Cloud Hypervisor) source-side live-migration transport landed
  (see docs/runtime-boundary.md's runtime contract v1), but triggering one
  meant a raw HTTP call with a hand-built JSON body — there was no CLI
  equivalent at all, unlike every other VM lifecycle operation. This closes
  that gap for the standalone mode the runtime-boundary doc already calls
  out: a deployment with no Fabric orchestrator driving these routes over
  HTTP still needs a way to move a VM off a node by hand.
  `fluxctl migrate start <id> --destination <tcp:host:port|unix:/path>
  [--mode pre-copy|post-copy] [--bandwidth-mbps N] [--max-downtime-ms N]
  [--multifd-channels N]` requires the VM to already be `Running` and
  refuses anything but a `tcp:`/`unix:` destination, same shared allowlist
  the REST route already validated against (`exec:` included, so this can't
  become an arbitrary shell invocation on the source host); `--mode` takes
  exactly `MigrationMode`'s own kebab-case wire spelling rather than a
  second CLI vocabulary for the same two values. `migrate status`/`migrate
  cancel <id>` stay QEMU-only — Cloud Hypervisor's `send-migration` is
  fire-and-forget with no status-polling or cancellation primitive of its
  own — and now surface that as a clear CLI error instead of only being
  discoverable by reading the REST handler's source. 5 new
  CLI-argument-parsing tests plus 2 for the `--mode` parser; verified on the
  Linux remote alongside the rest of this crate's suite.
- **`fluxctl ping`/`copy-to`/`copy-from` — CLI parity for the vsock guest
  agent's health check and file transfer.** The REST API has exposed
  `POST /v1/vms/{id}/agent/put-file` and `.../get-file` since the guest
  agent itself gained `PutFile`/`GetFile` (see the "Add vsock guest agent
  file transfer" entry further down this changelog), but the CLI only ever
  grew an `exec` command alongside them — copying a file into or out of a
  VM meant hand-rolling the HTTP call yourself, base64 and all, with no
  local equivalent of the `scp`-style ergonomics `exec` already gets.
  `fluxctl copy-to <id> <local> <remote> [--mode <bits>]` reads a local file,
  rejects anything already over the guest agent's own
  `MAX_FILE_TRANSFER_BYTES` cap before spending a base64 encode and a vsock
  round trip on content that would just be rejected guest-side anyway, and
  writes it in; `fluxctl copy-from <id> <remote> <local>` reads it back out
  and restores the guest-reported Unix permission bits on the local copy,
  not just its bytes — a copied-out private key or script keeps behaving
  the way its mode implies instead of landing at this process's umask
  default. `fluxctl ping <id>` adds the missing health check for this same
  channel: `qga ping` has covered the separate QEMU guest-agent socket for a
  while, but there was no way to ask "is the plain vsock agent even up"
  without spending a real `exec` round trip to find out — a new
  `VmManager::agent_ping` (and `POST /v1/vms/{id}/agent/ping`, gated the
  same as every other agent route) answers that with one `AgentRequest::Ping`
  instead. New CLI-argument-parsing tests for all three commands, plus unit
  tests for the size-cap check and the base64-decode-and-restore-mode path
  `copy-to`/`copy-from` are built on.
- **All-features GitHub CI** — `.github/workflows/all-features.yml` runs portable
  suites on `push` to `main` and `workflow_dispatch` (vsock CSM, Network Fabric
  preflight/enable dry-run, devops gates, Secure Containers units + use-case
  matrix, Sentinel/intelligence static). `ci.yml` adds an explicit virtio-vsock
  CSM filter; `network-fabric.yml` path-filters and runs preflight + enable
  dry-runs after installing BPF objects. `docs/DEVOPS.md` documents
  `vars.FLUXVM_*` and self-hosted lab labels. Privileged/live lanes stay opt-in.
- **In-tree FluxVM KVM virtio-vsock host→guest CSM** — `fluxvm-hypervisor`
  previously bound the Firecracker-style unix socket and advertised the MMIO
  device so the guest driver could bind, but drained TX without parsing
  packets and never accepted `CONNECT`, so guest-agent ping/exec could not
  complete on `fluxvm_engine=kvm`. The device now implements the Firecracker
  unix proxy handshake (`CONNECT <port>\n` → virtio REQUEST → guest RESPONSE
  → `OK <host_port>\n`) plus RW/credit/shutdown bridging for host-initiated
  streams. Guest→host `uds_path_<port>` muxer remains deferred. Unit tests
  cover header codec, CONNECT parsing, and CONNECT→REQUEST→OK→RW.
- **Network Fabric eBPF enablement UX** — new
  `scripts/network-fabric-preflight.sh` (bpffs, bpftool/tc, BPF object,
  systemd MEMLOCK/paths); `enable-network-fabric-ga.sh` merges from
  `configs/network-fabric-{ga,lab}.toml` (SoT), supports `--lab` soft
  profile, schema-v4 managed markers, and post-restart health hints. README /
  `config.example.toml` / production-dataplane runbook surface the one-liner.
  Global default remains `mode=legacy`.
- **`GuestImage.spec.kernel` now actually reaches the VM it boots** — the
  field has existed on `GuestImageSpec` since `GuestImage` was introduced
  (`docs/microvm.md` already documented it as an "optional ... `kernel`"),
  but nothing downstream ever read it: `images::guest_image_host_path` only
  ever surfaced the disk image, `fluxvm_client::create_body` never emitted a
  `"kernel"` key in the `POST /v1/vms` body, and `MicroVMSpec` itself has no
  `kernel` field to carry one — so a GuestImage catalogued for Firecracker's
  direct-kernel boot silently had no way to actually get its kernel to the
  VM it named. `CreateVmRequest.kernel` (`fluxvm-core`) and the scheduler's
  own Firecracker-eligibility check (`req.kernel.is_some()`) were already
  real and load-bearing on the `fluxctl serve` side — the microvm CRD layer
  was the missing link. Wired now: `guest_images::reconcile` verifies
  `spec.kernel` is present on the node (same fail-closed contract as
  `spec.sha256` above — a GuestImage naming a kernel that isn't staged is
  never marked Ready, never launches a guest with no kernel or a stale one
  reused from a same-named entry) and records the confirmed path in a new
  `status.kernelPath`. `node_agent::resolve_image` now resolves both the
  disk path and `status.kernelPath` from the same GuestImage lookup, and
  `FluxVMClient::create_vm`/`create_body` take an added `kernel: Option<&str>`
  forwarded straight onto the create request. Warm-pool templates
  (`pools::reconcile` → `ensure_pool`) are unaffected — they don't resolve
  the GuestImage catalog for their `image` field either, so pool templates
  must still name a direct disk image, matching existing behavior. 9 new
  unit tests (`guest_images::resolve_kernel`, `images::guest_image_kernel_path`,
  `fluxvm_client::resolved_kernel_is_forwarded`). Docs:
  [docs/microvm.md](docs/microvm.md#guestimage),
  [docs/tutorials/microvm/05-guestimage.md](docs/tutorials/microvm/05-guestimage.md)
  (new "catalog a direct-kernel-boot image" section). Verified with `cargo
  build`/`cargo test`/`cargo clippy --no-deps`/`cargo fmt --check` on macOS
  and the real Linux build host — no live Firecracker guest was booted
  end-to-end to confirm the forwarded `kernel` path actually produces a
  working direct-kernel boot; that's a hardware/root capability this
  environment doesn't have, so it's verified by code inspection of the
  already-existing `fluxvm-core`/scheduler kernel handling plus these unit
  tests, not a live run.
- **`GuestImage.spec.sha256` is now actually verified** — the field has
  existed on `GuestImageSpec` since `GuestImage` was introduced (`docs/microvm.md`
  already documented it as an "optional `sha256`"), but nothing in
  `guest_images::reconcile` ever read it: any file staged at `spec.source`
  flipped `status.ready=true` regardless of its contents, so a corrupted
  transfer or an operator staging the wrong file under the right path was
  silently handed to every `MicroVM` naming that catalog entry — no error,
  no signal, just a guest that boots off the wrong disk. Fail-closed now:
  when `spec.sha256` is set, the reconciler hashes the staged file and
  compares before `status.ready` flips true; a mismatch leaves
  `status.ready=false`, clears `status.path`, and reports
  `status.message: "sha256 mismatch: expected <spec>, got <actual>"` so the
  cause is visible on `kubectl get gimg -o yaml` rather than a `MicroVM`
  stuck unexplainably Pending against a Ready-but-wrong image. A GuestImage
  with `spec.sha256` unset keeps today's behavior exactly (Ready as soon as
  the file exists) — this is opt-in, not a change to any existing manifest's
  observed lifecycle. Re-hashing a multi-GB disk image on every 30s
  reconcile forever would be its own quiet cost, so the digest is only
  recomputed when the staged file's size/mtime or the requested `spec.sha256`
  itself changes since the last successful check — a new internal
  `status.verifiedSignature` field (`"{len}:{mtime}:{sha256}"`) is the cache
  key, so an unchanged file costs one `stat()` per reconcile instead of a
  full re-read. New `sha2` dependency in `fluxvm-microvm` — already vetted
  and present in the workspace via `fluxvm-image`'s own catalog signing,
  which uses the identical `expected .../got ...` mismatch message
  convention (`fluxvm-image::verify_sha256`) this mirrors for consistency.
  8 new unit tests in `fluxvm-microvm::guest_images` (`verify_staged_file`
  and its `signature`/`hash_file` helpers are plain functions against real
  temp files via `tempfile`, matching this crate's existing
  `jobs::ttl_expired`/`policy` convention of keeping reconcile decisions
  testable without a cluster): no-`sha256` short-circuit, matching digest,
  case-insensitive comparison, mismatch clears `path`, a missing file fails
  closed even without a `sha256` requirement, a fresh cache hit is trusted
  without re-hashing, and changing the requested digest invalidates a stale
  cache entry rather than trusting it. Docs:
  [docs/microvm.md](docs/microvm.md#guestimage),
  [docs/tutorials/microvm/05-guestimage.md](docs/tutorials/microvm/05-guestimage.md)
  (new "verify the staged file's digest" section). Verified with `cargo
  build`/`cargo test`/`cargo clippy --no-deps`/`cargo fmt --check` on macOS
  (`fluxvm-microvm` builds there) and on the real Linux build host — no live
  k3s cluster exercised this end to end, so the `MicroVM` reading a stale
  `status.path` from a since-invalidated GuestImage is verified by code
  inspection of `images::guest_image_host_path` (unchanged; it already
  requires `status.ready`) plus these unit tests, not a live cluster run.
- **`MicroVMJob.spec.ttlSecondsAfterFinished` now actually does something** —
  the field has existed on the CRD since `MicroVMJob` was introduced (it's
  right there in `MicroVMJobSpec`, and `examples/microvm/microvmjob.yaml`
  never set it), but nothing in `jobs::reconcile` ever read it: a finished
  Job — and every child `MicroVM` it created along the way — sat around
  forever until an operator ran `kubectl delete mvmj` by hand. CI-style
  fan-out workloads (`MicroVMJob`'s own documented use case in
  `docs/tutorials/microvm/02-job.md`) are exactly the kind that create a lot
  of these and finish quickly, so this was a slow, silent resource leak
  every run added to. Fixed the same way Kubernetes' own `Job` TTL
  controller works: the reconciler now stamps a new `status.finishedAt`
  (RFC3339) the first reconcile that observes a terminal phase
  (`Succeeded`/`Failed`) — set once and carried forward unchanged on every
  later reconcile, so restarting the controller or re-reconciling an
  already-finished Job can never push its deletion out — and once
  `now >= finishedAt + ttlSecondsAfterFinished`, deletes the `MicroVMJob`
  object outright. Deleting it is enough: its child `MicroVM`s already carry
  a `controller_owner_ref` back to the Job (set when `jobs::reconcile`
  creates them), so Kubernetes' own garbage collector cascades the delete to
  them, and each child's own finalizer-driven cleanup (shadow Pod,
  EndpointSlice, node-agent-side VMM teardown — unchanged, in
  `controller.rs`) runs exactly as it would for a `kubectl delete mvm` today.
  A Job with `ttlSecondsAfterFinished` unset keeps today's behavior exactly
  — this is opt-in per Job, not a change to any existing manifest's
  observed lifecycle. The reconcile's own requeue interval is now
  TTL-aware too (`next_requeue`): it sleeps until the TTL deadline instead
  of the previous fixed 10s poll, clamped to a 1s floor and the existing 10s
  ceiling, so a short TTL is honored promptly rather than waiting up to one
  more full poll interval after it expires. 7 new unit tests in
  `fluxvm-microvm::jobs` (`ttl_expired`/`next_requeue` are pure functions,
  matching this crate's existing `policy`/`capacity` convention, so no
  cluster is needed to exercise the TTL math: not-yet-expired, expired-at-
  and-past-the-deadline, a zero-TTL expiring immediately, the no-TTL steady
  cadence, mid-countdown sleep length, capping at the steady ceiling for a
  far-off TTL, and flooring at 1s when a reconcile lands essentially exactly
  on the deadline). Docs:
  [docs/microvm.md](docs/microvm.md#microvmjob),
  [docs/tutorials/microvm/02-job.md](docs/tutorials/microvm/02-job.md),
  `FEATURES.md`'s "MicroVM" bullet; `examples/microvm/microvmjob.yaml` now
  sets `ttlSecondsAfterFinished: 300` to show the shape. Verified with
  `cargo build`/`cargo test`/`cargo clippy --no-deps`/`cargo fmt --check`
  both on macOS and on the real Linux build host (`fluxvm-microvm` builds
  on both) — no live k3s cluster exercised this end to end, so the
  ownerReference-cascade-to-a-real-finalizer-cleanup path is verified by
  code inspection of the existing, unchanged `controller.rs` cleanup path
  plus these unit tests, not a live cluster run.
- **Image catalog signing: build-lineage/CI-provenance recording** — closes
  the second, deeper half of the gap FEATURES.md's Security Posture table
  named explicitly as still open ("Real build-lineage/CI-provenance
  recording (which pipeline/run produced this image)") after the first half
  (`signed_at`/`signed_by`/covering `distro`/`version`/`arch`) closed
  earlier. `CatalogEntry` gains three new optional fields —
  `build_pipeline`, `build_run_id`, `build_commit` — settable via three new
  `fluxvm catalog sign` flags (`--build-pipeline`/`--build-run-id`/
  `--build-commit`) and now covered by `canonical_payload`, the same
  tamper-evidence treatment `distro`/`version`/`arch` got: relabeling any of
  them in `catalog.json` after signing invalidates the signature (new test:
  `verify_rejects_a_relabeled_build_provenance_field`). Read the honesty
  caveat plainly, though — same posture as `signed_by` — this *records* a
  claim asserted by whoever ran `catalog sign`, not an independently
  verified attestation; there is still no cryptographic chain proving the
  named CI system actually produced these bytes (that would need something
  like Sigstore/in-toto, which this project's signing scheme deliberately
  avoids, same reasoning as avoiding a `cosign`/Fulcio/Rekor dependency —
  see this file's own module doc comment). Live-verified against the real
  `fluxvm` binary on the test host: signed an entry with
  `--build-pipeline github-actions/build-images.yml --build-run-id
  987654321 --build-commit <sha>`, confirmed `GET`/`catalog list` reports
  `signature_valid: true`/`signed_by: "release-ci"`, then hand-edited
  `build_run_id` in `catalog.json` and confirmed the same entry flips to
  `signature_valid: false`/`signed_by: null` — the tamper-evidence claim is
  real, not just unit-tested. 3 new catalog tests (round-trip through
  `sign_entry`, the relabel-invalidates-signature test above, and the
  existing sign/verify suite extended to cover the new fields) plus 2 new
  `fluxvm catalog sign` CLI-parsing tests.
- **Cloud Hypervisor backend: real live migration** — `POST
  /v1/vms/{uuid}/migration/start` previously bailed "live migration
  contract v1 supports qemu only" for every non-QEMU backend; Cloud
  Hypervisor now actually migrates. `migration_start` validates the
  destination through the same shared `validate_migration_transport`
  `fluxvm-qemu` already used (moved into `fluxvm-core::backend` so the two
  backends' `tcp:`/`unix:`-only contract can't drift apart), maps
  `MigrationStartRequest` onto `ch-remote send-migration`'s own
  `destination_url=...,downtime_ms=...,memory_mode=precopy|postcopy,
  connections=...` config string, and pre-checks three real Cloud
  Hypervisor constraints up front rather than relaying its raw validation
  error for them (all three verified live): it has no bandwidth-throttle
  knob at all, so `bandwidth_mbps` is rejected outright rather than
  silently ignored; `connections` (its multifd analogue) and a `unix:`
  destination are mutually exclusive; and post-copy mode requires exactly
  one connection. `RuntimeCapabilities`'s migration entry gained a new
  `status_pollable` field (`true` for QEMU, `false` for Cloud Hypervisor)
  and a Cloud Hypervisor entry, so Fabric can tell up front which contract
  it's getting. 9 new tests (5 in `fluxvm-cloud-hypervisor` against a
  scripted fake `ch-remote` logging its full argv, 2 shared-validator
  tests moved with the function, plus the pre-existing QEMU validator
  tests now exercising the shared implementation).

  The single biggest way this differs from QEMU's contract, discovered
  only by testing against real binaries rather than assumed from Cloud
  Hypervisor's own docs: `send-migration` returns as soon as the request
  is *accepted*, not once the transfer finishes. Verified live against a
  real `cloud-hypervisor`/`ch-remote` v53.0 pair by pointing `send-migration`
  at a `unix:` destination nothing was listening on — the call still
  returned success immediately, with the real "Migration failed: ... No
  such file or directory" only ever surfacing seconds later in the VMM's
  own log file. Cloud Hypervisor's API has no status-query or cancellation
  primitive at all for what happens next, so `migration_status`/
  `cancel_migration` deliberately stay qemu-only (with a Cloud
  Hypervisor-specific error explaining why, not the generic "supports qemu
  only" every other unsupported backend gets) rather than inventing a
  status this backend cannot actually report. Also verified live, over
  both `unix:` and `tcp:` destinations on that same real pair: a running
  VM was fully handed off between two separate `cloud-hypervisor`
  processes — the destination's `ch-remote info` came back with the
  migrated config and `"state":"Running"`, and the source process exited
  on its own right after, exactly as Cloud Hypervisor's docs describe. That
  source exit needed no new handling here: this project's existing
  reconcile loop already notices the pid is gone and marks the VM
  `Stopped` (releasing its tap/netns/sandbox policy on this node) the same
  as it would for any other process that exited on its own — which is
  exactly the right outcome for a VM that just left this node for another
  one.
- **Cloud Hypervisor backend: real CPU/memory hotplug** — `POST
  /v1/vms/{uuid}/hotplug/cpu`/`hotplug/memory` previously worked on QEMU
  only ("Cloud Hypervisor/Firecracker backends have no hotplug support in
  this codebase," per this file's own QEMU-hotplug entry below); Firecracker
  still doesn't, but Cloud Hypervisor now does. `build_args` reserves the
  same headroom QEMU already reserves by default (mirrored via two new
  shared `max_vcpus`/`max_memory_mib` helpers so the two backends' default
  formulas can never drift apart) — `--cpus boot=N,max=M` and `--memory
  size=NM,hotplug_size=HM` (Cloud Hypervisor's own headroom parameter is
  *additive*, unlike QEMU's absolute `-m maxmem=`, so `hotplug_size` is
  computed by subtracting `size` back out of the same absolute ceiling QEMU
  would use for an identical request). `hotplug_cpu`/`hotplug_memory` query
  `ch-remote info` for the VM's current live vCPU count/memory size, compute
  the new absolute total, and call `ch-remote resize --cpus`/`--memory` to
  it — `resize` itself takes an absolute target, not a delta, the opposite
  shape from QEMU's own `device_add`-based fill-the-next-free-slot
  mechanism. Grow-only by this crate's own choice (`resize` itself can
  shrink too, auto-offlining vCPUs in the guest) to match the QEMU backend's
  existing no-unplug contract. A memory add must be a whole multiple of
  128MiB, Cloud Hypervisor's own ACPI-hotplug requirement — checked up
  front with a clear message instead of relaying its raw
  "not a valid size" error. 8 new tests against a scripted fake `ch-remote`
  (extending the existing `snapshot_save` fixture pattern with a fake
  `info` JSON response and resize-argv logging) plus 2 build-args tests
  proving the new `max=`/`hotplug_size=` headroom math. Verified live
  end-to-end against a real `cloud-hypervisor v53.0`/`ch-remote v53.0` pair
  on a KVM-capable Linux host — not just unit tests against a fake binary:
  booted with `--cpus boot=1,max=4`/`--memory size=512M,hotplug_size=2048M`,
  resized vCPUs 1→2 and memory 512M→1G→2G→2.5G (`ch-remote info` confirming
  the new live totals each time), confirmed the documented 128MiB-alignment
  and max-headroom rejections fail with Cloud Hypervisor's own real error
  strings, and confirmed `resize --cpus`/`--memory` reject a target beyond
  `max_vcpus`/`size + hotplug_size` exactly as this crate's own pre-checks
  now anticipate before ever shelling out.
- **`fluxvm-agent central`'s fleet-wide `GET /fleet/vms` now names any node
  it couldn't account for, instead of silently omitting it** — the same
  surface-don't-hide fix the single-node `GET /fleet/nodes/{name}/vms`
  route got below, extended to the aggregate it was originally written to
  work around. Previously a node excluded up front for a stale heartbeat,
  or one whose own `GET /v1/vms` call failed partway through (connection
  error, non-2xx response, unparseable body), just disappeared from
  `"items"` with nothing but a server-side `tracing::warn!` the caller
  never saw — a fleet member being down looked identical to it simply
  having no VMs, with no way to tell the two apart from the response
  alone. The response now carries a second field, `"unreachable_nodes"`:
  an array of `{"node": "...", "reason": "..."}` naming every node that
  didn't make it into `"items"` and why (`"unhealthy (stale heartbeat)"`
  for one excluded before it was even queried; a specific connection/
  status/parse error for one that was queried and failed). It's `[]` on
  the ordinary all-healthy path, so an existing caller that only reads
  `"items"` sees no behavior change; one that wants to know whether the
  list it just got is the *whole* fleet's now has a direct answer instead
  of having to cross-reference `GET /fleet/nodes` or grep server logs. 4
  new tests in `fluxvm-agent::central`: all-healthy-and-reachable reports
  an empty `unreachable_nodes`; an unreachable node is named while the
  other node's real VMs still come through; a stale-heartbeat node is
  named without ever being contacted; a node that rejects the call with a
  non-2xx is named with its own error message folded into the reason.
  Docs:
  [docs/operations.md — Distributed node-agent](docs/operations.md#distributed-node-agent),
  `FEATURES.md`'s "`fluxvm-agent`" bullet. Same honest limit as the two
  fleet-registry features below: not re-verified against the real
  two-physically-separate-host rig `scripts/test-fleet-agent.sh`
  exercises — that script is unchanged and wasn't re-run against live
  hardware; verification here is the 4 new unit-level tests plus a real
  (if local) build/test/clippy/fmt pass, not a live-fleet run.
- **`fluxvm-agent central` can now list exactly one node's VMs** —
  `GET /fleet/nodes/{name}/vms`. The only existing way to see what's
  running on a specific node was `GET /fleet/vms`, which aggregates every
  *healthy* node's VMs into one list tagged by node — useful for a
  fleet-wide view, but the wrong tool for the actual question an operator
  has right before (or right after) cordoning a node: "what, specifically,
  is on this one node?" That question needed either grep'ing the
  fleet-wide aggregate client-side (which silently drops a node's VMs
  entirely, with only a server-side `tracing::warn!` the caller never
  sees, if that one node happens to be unreachable — exactly the node an
  operator is most likely asking about mid-maintenance) or bypassing
  `fluxvm-agent` altogether and querying that host's local `fluxctl serve`
  directly (which means already knowing, and having direct network access
  to, that host — the whole point of the fleet registry is not needing
  that). The new route proxies straight to the named node's own
  `GET /v1/vms`, tags each VM with `"node"` the same way the fleet-wide
  list does, and — unlike the fleet-wide list — turns that one node being
  unreachable or erroring into a real `502` naming the node, instead of
  quietly omitting it. Works for any registered node regardless of
  `cordoned`/`healthy` state (an operator checking on a node they just
  cordoned, or one that just went stale, still gets a real answer or a
  real error, never silence). 5 new tests in `fluxvm-agent::central`
  against a real (not mocked) minimal axum server bound to an OS-assigned
  loopback port standing in for the target node's `fluxctl serve` — the
  same shape `fluxvm-container-agent`'s own tests already use for this —
  covering the happy path (VMs returned and correctly tagged with the
  node name), an unknown node name (400, not a panic or an empty list),
  a node that's unreachable (bound-then-dropped listener; 502, not a
  silently empty result), a node whose own `GET /v1/vms` itself errors
  (its status/body is propagated, not swallowed), and a node with zero
  VMs (empty list, not an error). Docs:
  [docs/operations.md — Distributed node-agent](docs/operations.md#distributed-node-agent),
  `FEATURES.md`'s "`fluxvm-agent`" bullet. Same honest limit as the
  cordon feature below: not re-verified against the real
  two-physically-separate-host rig `scripts/test-fleet-agent.sh`
  exercises — that script is unchanged and wasn't re-run against live
  hardware; verification here is the 5 new unit-level tests plus a real
  (if local) Linux build/test/clippy/fmt pass, not a live-fleet run.
- **`fluxvm-agent central` fleet nodes can now be cordoned for planned
  maintenance** — `POST /fleet/nodes/{name}/cordon` and `.../uncordon`.
  Previously the only way to stop new VMs landing on a specific node ahead
  of a reboot/upgrade/decommission was to either stop its `fluxvm-agent
  node` heartbeat client outright and wait out `HEALTHY_WINDOW_SECS` (30s)
  for the central registry to mark it unhealthy — at which point it also
  silently drops out of the fleet-wide `GET /fleet/vms` aggregation and
  `DELETE /fleet/vms/{node}/{id}` still works but the operator has lost
  the node's own reported capacity/VM-count in the meantime — or take the
  whole node offline and lose visibility into it entirely. Neither gives
  a clean "stop scheduling new work here, but keep everything already
  running and keep reporting on it" state, which is the actual maintenance
  workflow a fleet operator needs. New `NodeInfo.cordoned: bool`
  (`#[serde(default)]`, so an existing `fleet-nodes.json` still loads as
  "never cordoned") excludes a node from `pick_best_capacity`'s
  placement candidates regardless of how much free capacity it reports,
  while `GET /fleet/nodes` keeps reporting it (now with a `"cordoned"`
  field) and the fleet-wide `GET /fleet/vms` keeps aggregating its VMs
  exactly as before — cordoning never touches health, capacity reporting,
  or any VM already there. An explicit `POST /fleet/vms {"node": "..."}`
  naming a cordoned node by name still resolves and proxies through
  unchanged — deliberately, matching the same precedent a Kubernetes Pod
  with `spec.nodeName` set already has (it bypasses the scheduler and can
  still land on a cordoned node): cordoning was only ever meant to narrow
  automatic placement's candidate set, never to become a second admission
  check bolted onto every route. A node's own heartbeat (`POST
  /fleet/register`, sent by `fluxvm-agent node` every `--interval-secs`)
  carries no notion of cordoning at all — the node agent has no idea the
  feature exists — so `register`'s handling had to change too: it now
  looks up whether the node already has a `cordoned` flag set before
  overwriting the rest of its `NodeInfo`, instead of blindly re-inserting
  a fresh record (which would have silently un-cordoned every node the
  very next heartbeat, seconds after an operator cordoned it). 9 new unit
  tests in `fluxvm-agent::central`, all against the same pure,
  HTTP-free functions the existing placement tests already use
  (`pick_best_capacity`, plus new `set_cordoned`/`apply_register`/
  `resolve_target` extracted the same way to stay testable without
  standing up a mock node backend): cordoned-node placement exclusion
  even against a node with far more free capacity, the only-registered-
  node-cordoned case, uncordon restoring eligibility, cordoning an
  unregistered name reporting not-found instead of silently no-op'ing,
  a heartbeat preserving an existing cordon while still updating every
  other field, a brand-new node's first-ever heartbeat registering
  uncordoned, and both directions of the explicit-`"node"`-bypasses-
  cordon behavior (a cordoned node targeted by name still resolves; an
  unregistered name still errors). Docs:
  [docs/operations.md — Distributed node-agent](docs/operations.md#distributed-node-agent),
  `FEATURES.md`'s "`fluxvm-agent`" bullet. Not re-verified against the
  real two-physically-separate-host rig `scripts/test-fleet-agent.sh`
  already exercises for the rest of this subsystem — that script is
  unchanged by this commit and wasn't re-run against live hardware; the 9
  new tests are unit-level only, against the same in-process registry
  logic the existing placement tests already cover the same way.
- **`fluxvm catalog` gained CLI parity with the image catalog's REST CRUD** —
  `list`/`add`/`remove`/`rename`/`clone`/`export`/`lock`/`unlock`/`clean`,
  alongside the existing `keygen`/`sign`. The underlying
  `fluxvm_image::catalog` functions (`add_entry`, `remove_entry`,
  `rename_entry`, `clone_entry`, `export_entry`, `set_read_only`,
  `clean_downloads`, `list_with_verification`) already backed all of this
  over REST (`POST/GET/DELETE /v1/images/catalog...`) — the CLI itself had
  never grown past the two offline-signing verbs, so managing a catalog
  without a running `fluxctl serve` (seeding a fresh host before the daemon
  is up, or a script that would rather shell out than depend on an HTTP
  endpoint) had no path at all. Each new subcommand is a thin wrapper
  reading `catalog.path` straight off `--config`/`FLUXVM_CONFIG`, the same
  one-shot-process model `fluxctl pool claim` already uses — works whether
  or not `fluxctl serve` happens to be running against the same
  `state_dir`. `lock`/`unlock` replace the REST route's boolean
  `{"read_only": bool}` body with two verbs, clearer on a command line.
  9 new tests (clap parsing for every new subcommand, including that
  `catalog add` rejects a missing `--source`). Docs:
  [docs/operations.md — Image catalog & signing](docs/operations.md#image-catalog--signing).
- **Warm VM pools now report computed occupancy, not just raw membership** —
  `GET /v1/pools`, `GET /v1/pools/{name}`, `fluxctl pool list`, and
  `fluxctl pool get` previously returned the bare `PoolRecord`: `size` (the
  *target* member count) and `members` (the ids of members currently ready),
  with nothing naming which was which. Telling "fully backfilled" apart
  from "still catching up after a resize or a burst of claims" meant a
  caller had to already know, unprompted, to compare `members.len()`
  against `size` itself — and there was no visibility at all into how
  heavily a pool had actually been used over its lifetime, which is the
  other half of knowing whether it's sized right. New `PoolView`
  (`fluxvm-core::model`), a `#[serde(flatten)]` wrapper around `PoolRecord`
  adding two computed fields — `ready` (`members.len()`) and `pending`
  (`size` minus `ready`, saturating so a pool briefly over target
  mid-shrink reports 0 rather than an underflowed `usize`) — plus a new
  persisted `PoolRecord.claimed_total: u64`, a lifetime counter of
  successful `claim_from_pool` calls, bumped via a new
  `PoolStore::increment_claimed` right after a claim has already fully
  succeeded (member resumed, and tenant-checked when the caller's token
  carries one) — best-effort and fail-open the same way `delete_pool`'s own
  member cleanup is, so a stats-increment failure can never turn an
  already-completed claim into an error for the caller who just received
  their VM. `#[serde(default)]` on the new field so a `pools.json` written
  before this existed still loads cleanly (as 0, the honest "unknown
  history" value). All four pool-returning routes (`create`, `list`, `get`,
  `resize`) and both CLI print sites (`pool list`/`get`, which read
  straight off the same local `VmManager` the server uses, not through
  HTTP) now go through `PoolView` — purely additive, no existing field
  renamed or removed. 9 new tests (4 `fluxvm-core` unit tests on `PoolView`
  covering the ready/pending arithmetic, the saturating-at-target-exceeded
  case, and that the flatten doesn't shadow a persisted field; 2
  `fluxvm-storage` unit tests on `increment_claimed`, including the
  pool-deleted-concurrently no-op case; 1 `fluxvm-api` router test
  asserting `ready`/`pending`/`claimed_total` on both the list and
  by-name routes). Docs: `FEATURES.md`'s "Warm VM pools" bullet.
- **Warm VM pools can now be resized after creation** —
  `POST /v1/pools/{name}/resize` (admin-only, body `{"size": N}`) and
  `fluxctl pool resize <name> --size N`. Previously `PoolSpec::size` was
  fixed for the life of a pool: an operator whose real load outgrew (or
  shrank below) a pool's original size had no way to change it short of
  `DELETE`-ing the pool outright and `POST`-ing a new one from the same
  spec, discarding every still-ready warm member in the process just to
  change one number. New `VmManager::resize_pool` and
  `PoolStore::set_size` (`fluxvm-scheduler`/`fluxvm-storage`). Growing
  only updates the stored target and fires the same background
  `spawn_backfill` `create_pool`/`claim_from_pool` already use — the
  reaper's per-tick top-up is the same backstop for a resize as it is for
  those two, so nothing new was needed there. Shrinking is handled
  synchronously instead: excess ready members are popped and deleted
  immediately, fail-open per member (a single stuck delete is logged and
  skipped, matching `delete_pool`'s own best-effort cleanup) rather than
  left for the next reaper tick — asking for a smaller pool is asking to
  give resources back, and the reaper itself only ever grows a pool
  toward its target, never shrinks it, so an under-trimmed shrink simply
  stays put rather than drifting back up. Both directions serialize
  against the same per-pool `backfill_locks` mutex `backfill_pool` itself
  already uses, so a resize can't race a concurrent reaper-triggered (or
  claim-triggered) backfill into an inconsistent membership count. Tenant
  scoping matches every other name-keyed pool route (`get`/`delete`/
  `claim`): a mismatched tenant's token gets the same 404
  `pool_visible_to` already returns for those. 10 new tests (2
  `fluxvm-storage` unit tests on `set_size`, 4 `fluxvm-scheduler` unit
  tests on `resize_pool` covering zero-size rejection, an unknown pool,
  growing, and shrinking down to and past actual membership, 4
  `fluxvm-api` router tests covering the REST route's tenant scoping,
  admin-only enforcement, and validation). Docs: `docs/api.md`'s pools
  section, `docs/operations.md`'s "Warm VM pools" section (with an
  explicit "Real limits today" noting this has not yet been exercised
  against real hardware the way the rest of that section has been —
  `scripts/test-warm-pool.sh` doesn't cover it), `FEATURES.md`.
- **REST API rate limiting** — `auth.rate_limit_rps`/`auth.rate_limit_burst`
  (opt-in, both must be set together). Closes a real gap: `fluxvm-api`
  had no request-volume limiting at all, in either the network-dataplane
  sense (`[[policy.tenants]]`/per-VM Mbps/PPS limits already exist there)
  or the control-plane sense — a single caller holding one valid token
  could issue unbounded requests with nothing to push back. New
  `fluxvm-api::rate_limit::Limiter`, a small hand-rolled per-key token
  bucket (no new dependency), keyed by the same actor identity the audit
  log already attributes a request to (token name, OIDC subject, mTLS
  cert CN, or `"anonymous-admin"` on an unauthenticated loopback
  deployment) rather than raw connection volume — two callers sharing
  one token share one bucket by design. Runs after auth (so every
  request it sees already carries that identity) and before the
  per-tenant scope guard, with its own explicit bypass for `/healthz`/
  `/readyz` (auth only skips resolving an identity for those two, it
  doesn't stop them reaching the layers below it) so a probe can never
  be starved by a caller's own throttling. A throttled request gets `429` with
  `Retry-After` set. Absent by default: no rate limiting at all,
  byte-for-byte the behavior before this existed. Docs:
  `docs/operations.md`'s "REST API rate limiting" section.
- **AppArmor local-include for `swtpm` on Debian/Ubuntu** —
  `packaging/apparmor/usr.bin.swtpm.fluxvm`. Found by actually driving a
  real `fluxctl create` call with `tpm: true` through the compiled binary
  on a real Ubuntu host (not just the argument-syntax smoke test the
  Secure Boot/vTPM change shipped with): the distro's own `swtpm` package
  ships an AppArmor profile confining it to libvirt's conventional paths,
  with nothing for FluxVM's own `<state_dir>/instances/<id>/` workspace,
  so `spawn_swtpm` fails closed with a permission error on any host
  where that profile enforces (the common case). The snippet extends the
  packaged profile via its own already-present `#include
  <local/usr.bin.swtpm>` hook — Debian/Ubuntu's supported mechanism for
  this, never edits the package-owned file itself. Not live-verified
  end-to-end in this session (installing it edits a host's live security
  policy, deliberately not done here without separate authorization) —
  see `docs/secure-boot-tpm.md`'s "Real limits" for the honest framing
  and the diagnosed-but-unverified distinction.
- **Stronger image catalog signatures** — the Ed25519 signed payload now
  covers `distro`/`version`/`arch` and a new tamper-evident `signed_at`
  timestamp, not just `name`/`source`/`sha256`/`format`. Previously
  `distro`/`version`/`arch` were present on a `CatalogEntry` but excluded
  from what was actually signed, so they could be edited in `catalog.json`
  post-signing (e.g. relabeling `arch` to mislead a platform-matching
  consumer) without invalidating the signature — a real gap, closed here.
  `read_only` stays deliberately unsigned (a mutable operational flag
  toggled via its own REST route, not provenance data). `[[catalog.trusted_signers]]`
  is now a named list (`name`/`public_key`, the same `[[auth.tokens]]`-shaped
  convention this project already uses for labeled credential lists)
  instead of a bare list of base64 keys, so `GET /v1/images/catalog`'s
  new `signed_by` field can report real signer identity — the *name* of
  whichever configured key actually verified the signature, derived
  fresh on every call from which key matched, never trusted from
  anything the entry itself claims about its own signer. **Breaking
  change**: an entry signed before this existed will fail verification
  against the wider payload; re-run `fluxvm catalog sign` for every
  entry after upgrading if `trusted_signers` is configured. Does not
  close the deeper "no real build-lineage/CI-provenance recording" gap —
  see FEATURES.md's Security Posture table for what's still genuinely
  open there. Docs: `docs/operations.md`'s "Image catalog & signing"
  section.
- **Per-tenant aggregate admission quotas** — `[[policy.tenants]]`
  (`tenant`, `max_vcpus_total`, `max_memory_mib_total`, `max_vms_total`),
  summed across every existing VM a tenant already owns plus the incoming
  request, matched against `CreateVmRequest.tenant` (already authoritative
  by the time `fluxvm-scheduler` sees it — resolved by `fluxvm-api` from
  `[[auth.tokens]]`'s own `tenant` field or an OIDC claim). Closes a real
  gap: `[policy]`'s existing fields (`max_vcpus`, `max_memory_mib`, etc.)
  only ever validate one incoming request in isolation, with no way to cap
  how much a tenant accumulates across many VMs over time. New
  `fluxvm-scheduler::validate_tenant_policy`, called from `VmManager::create`
  right after the existing per-request `validate_policy`, only when the
  request has a `tenant` set and at least one `[[policy.tenants]]` entry
  exists — a full `Store::list()` scan is skipped entirely otherwise, so
  hosts that don't use this pay nothing extra per create. The aggregate,
  fleet-wide counterpart to Kairon's own `MachineQuota` CRD (a sibling
  project). 6 new unit tests. Docs: `docs/operations.md`'s "Policy" section.
- **UEFI Secure Boot (QEMU) + emulated vTPM 2.0 (QEMU and Cloud
  Hypervisor)** — `CreateVmRequest.secure_boot`/`.tpm`, with deliberately
  different scope per field: `tpm` works on both backends, `secure_boot`
  is QEMU-only, permanently — not "not implemented yet" on Cloud
  Hypervisor. Cloud Hypervisor's own `--firmware` is a single opaque file
  with no documented separate variable store to enroll Secure Boot keys
  into and no documented enforcement mechanism (confirmed against Cloud
  Hypervisor's own `docs/uefi.md`/Windows-guest docs) — claiming support
  there would be dishonest, not just unimplemented, so it's rejected
  outright. Also fixes a real, independent pre-existing gap:
  `CreateVmRequest.firmware` already existed and Cloud Hypervisor already
  wired it correctly, but the QEMU backend never read it at all despite
  `docs/tiny-windows.md`/three `examples/*.json` setting it on QEMU-backend
  requests — those silently fell back to QEMU's default BIOS. Now wired as
  split code/vars pflash (`-drive if=pflash,...unit=0,readonly=on` +
  `...unit=1` for a per-VM writable `<workspace>/ovmf_vars.fd` copy of a
  new `Config::qemu_ovmf_vars_template`, plus `smm=on`/
  `driver=cfi.pflash01,property=secure,value=on`) — the prerequisite
  Secure Boot itself needs. `tpm: true` spawns a shared `swtpm` sidecar
  (`fluxvm_core::process::spawn_swtpm`, reused by both backends; mirrors
  the existing `virtiofsd` spawn/wait/kill-on-failure pattern, whose
  readiness-poll loop was extracted into `wait_for_socket_ready` for
  reuse) — QEMU wires `-tpmdev emulator` + `-device tpm-crb`; Cloud
  Hypervisor wires its own, simpler `--tpm socket=<path>` (confirmed
  against a real `cloud-hypervisor --help`, v53.0). TPM state persists
  under `<workspace>/tpm/` for the VM's lifetime, independent of the
  sidecar process's own per-launch lifecycle. Both fields are rejected
  outright at `create()` time on a backend that doesn't support them
  (unlike `vfio_devices`/`numa_node`, which those backends silently
  ignore) — a caller believing they got Secure Boot/measured boot when
  they silently didn't is a real, security-relevant footgun. Real unit
  coverage for both backends' `build_args` argument construction; the
  `swtpm`/OVMF-vars-copy process-spawning glue has no dedicated unit test
  of its own (the same pre-existing gap `spawn_virtiofsd_instances` itself
  already had). Docs: [`docs/secure-boot-tpm.md`](docs/secure-boot-tpm.md).
- **QGA: `guest-network-get-interfaces`** — `GET /v1/vms/{uuid}/qga/network-interfaces`
  (read-only, no `admin` role required unlike the other `qga/*` routes)
  returns the guest's own reported interfaces/IPs via the real
  `qemu-guest-agent` protocol. Unlike `guest_ip`, which only ever comes from
  parsing a dnsmasq DHCP lease file (`NetworkSpec::Tap { netns: true }`
  only — `None` for every other network mode, `User`/SLIRP included), this
  works for any network mode the guest agent can reach. Verified against a
  real running VM under `network.mode = "user"` (SLIRP), which has no lease
  file at all: the guest's own real SLIRP-assigned address (`10.0.2.15`)
  came back correctly.
- **QEMU backend: real CPU/memory hotplug** — `POST /v1/vms/{uuid}/hotplug/cpu`
  (`add_vcpus`) and `POST /v1/vms/{uuid}/hotplug/memory` (`add_memory_mib`),
  QEMU-only. CPU hotplug fills unrealized `query-hotpluggable-cpus` slots via
  `device_add` in deterministic `(socket-id, core-id, thread-id)` order;
  memory hotplug attaches a `pc-dimm` backed by a fresh `memory-backend-ram`
  object, rolling the backend object back if the `device_add` fails. Both use
  the hotplug headroom (`-smp maxcpus=`, `-m slots=/maxmem=`) every QEMU VM
  has reserved since `max_vcpus`/`max_memory_mib` were added — this wires up
  the actual `device_add`/`object-add` calls that headroom existed for but
  nothing in this repo previously issued. Verified against a real running VM
  (`query-cpus-fast`/`query-memory-devices` independently confirm the new
  vCPU thread and DIMM). Docs: [`docs/api.md`](docs/api.md).
- **Docs: next-features backlog** — ranked Sentinel / hypervisor / Fabric
  follow-ups after Set 15 + FC/CH parity ([`docs/NEXT-FEATURES.md`](docs/NEXT-FEATURES.md)).
- **Secure Containers Set 15** — directional attachment health for
  `fluxvm_pod_ingress` (`pod_ingress_required` / `pod_ingress_attached` on
  `NativeAttachmentStatus`; aggregate `attached` requires both hooks when
  ingress is pinned); read-only Sentinel Policy Observer
  (`tools/fluxvm-policy-observer`) exporting Pod-policy counters and
  hook/rule pressure on `:9091`. Docs:
  [`docs/secure-containers-set15.md`](docs/secure-containers-set15.md).
- **In-tree KVM FC/CH parity (P0–P2)** — virtio-mmio vsock/balloon/rng with
  `KVM_IRQFD`; auto `virtio_mmio.device=` cmdline; token-bucket net/blk
  rate limits; optional vhost-net open; ACPI RSDP/XSDT/FADT/MADT + PVH
  `rsdp_paddr`; jailer (`--jailer` / `FLUXVM_JAILER`); PCI ECAM (`--pci`);
  `FLUXKVM1` snapshot v2 (all vCPUs). P3 stubs for virtio-fs / migration /
  hotplug. Boot hang past `init_zbud` resolved; guests mount `vda` and reach
  `/sbin/init`. Docs: [`crates/fluxvm-hypervisor/README.md`](crates/fluxvm-hypervisor/README.md),
  [`docs/agent-sandbox-gaps.md`](docs/agent-sandbox-gaps.md),
  [`docs/kvm-density.md`](docs/kvm-density.md).
- **Secure Containers Set 5** — authenticated VSOCK stdio streaming on port
  17779 (lifecycle stays on 17778); guest pipes for non-TTY I/O; real guest
  PTY for `terminal=true` init/exec with `ResizePty`/`CloseIO`; output drain
  before TaskExit/Wait/Delete; legacy virtiofs stdio via
  `FLUXVM_CONTAINER_STREAMING_STDIO=0`; `scripts/e2e-secure-containers-tty.sh`.
  Docs: [docs/secure-containers.md](docs/secure-containers.md),
  [docs/secure-containers-set5.md](docs/secure-containers-set5.md).
- **Secure Containers Set 4** — Pod-UID-scoped write-through kubelet `volumes/` /
  `volume-subpaths/` virtiofs exports with bind-source rewrite; guest OCI
  security (read-only rootfs, masked/RO paths, device nodes, sysctls,
  libseccomp syscall-name rules fail-closed); mount rollback on create failure;
  `scripts/e2e-secure-containers-volume.sh`. Docs:
  [docs/secure-containers.md](docs/secure-containers.md),
  [docs/secure-containers-set4.md](docs/secure-containers-set4.md).
- **Secure Containers Set 3** — containerd task lifecycle events (create/start/
  exec/pause/resume/exit/delete) with async Wait watchers and exit
  de-duplication; definitive delete metadata; init/exec process separation;
  retry-safe staging cleanup; guest OCI process hardening (supplementary GIDs,
  umask, rlimits, `noNewPrivileges`, capability sets). Docs:
  [docs/secure-containers.md](docs/secure-containers.md),
  [docs/secure-containers-set3.md](docs/secure-containers-set3.md).
- **Secure Containers Set 2** — CNI L2 Pod-IP path (QEMU TAP on the prepared
  host bridge when a CRI netns is present; `FLUXVM_CONTAINER_CNI=0` disables),
  guest cgroup-v2 resource limits/stats/pids/update + sandbox resource config,
  shim `Stats`/`Update`/`Pids` wiring, VM shape from resource hints. Docs:
  [docs/secure-containers.md](docs/secure-containers.md).
- **Secure Containers (containerd runtime-v2)** — developer-preview foundation:
  `fluxvm-container-protocol` / `fluxvm-container-agent` / `fluxvm-container-client` /
  `fluxvm-containerd-shim` (`containerd-shim-fluxvm-v2`, runtime
  `io.containerd.fluxvm.v2`); virtiofs Pod share + VSOCK lifecycle on :17778;
  `deploy/containerd/`, install/test/e2e scripts, CI workflow
  `.github/workflows/secure-containers.yml`. Docs:
  [docs/secure-containers.md](docs/secure-containers.md). QEMU-only; hostPath
  hotplug and broader CNI/OCI namespace parity remain follow-up gates
  (Sets 4–5 cover Pod-UID volumes and VSOCK stdio/TTY).
- **Service Fabric full mesh datapath (Fabric-side)** — remote backends merge into
  existing Maglev `/v1/network/services` upserts (lifecycle v2: weighted drain +
  optional VIP match; no new FluxVM tunnel APIs; Geneve/VXLAN still N/A). See
  Fabric `service-lb::remote_backend`.
- **Service Fabric gen8 connect** — cgroup/connect6 + Maglev affinity parity with TC
  (`fluxvm_fct4`/`fct6`); `cgroup_connect` attaches both v4 and v6; program generation **8**
  (schema 4 unchanged).
- **Service Fabric map-tier ELFs** — `-DFLUXVM_MAP_TIER=S|M|L` objects
  (`fluxvm_service*_tier_{S,M,L}.bpf.o`); `map_tier` selects loader path + catalog limits.
- **Service Fabric SLO harness** — `scripts/test-service-fabric-slo.sh` + optional
  `SLO_VIP_P99_MS` / `SLO_PRESSURE_IDLE` / `SLO_REQUIRE_CHANNELS` / `SLO_CI_SHAPE` gates.
- **Service Fabric universal CI SLO defaults** — `SLO_CI=1` applies VIP p99 /
  pressure / EDT fairness / HA failover RTT / Mpps floors; RSS under-load PPS via
  `scripts/test-service-fabric-rss.sh` (`SLO_RSS_PPS_MIN`, soft-skip without VIP).
- **Service Fabric lab Mpps / CPU ceilings** — `SLO_LAB=1` /
  `scripts/test-service-fabric-lab.sh` raises connect Mpps floor to `0.05`, adds
  packet Mpps (`SLO_PKT_MPPS_MIN=0.10`) + fluxvm CPU ceiling (`SLO_CPU_MAX_PERCENT=85`)
  during storm; CI floor unchanged at `0.01`.
- **Remote ipcache ingest** — `POST/DELETE /v1/network/ipcache/remote` for Fabric
  ClusterMesh-like identity fan-out (local VM rows preserved).
- **Service Fabric gen7 ops tranche** — opt-in cgroup/connect4 (`fluxvm_service_connect.bpf.o`),
  map pressure controller (`POST /v1/network/services/pressure/reconcile`), XDP
  native→generic attach + ethtool offload/channels in `services/status`, perf lab
  `scripts/test-service-fabric-perf.sh`; program generation **7** (schema 4 unchanged).
- **Service Fabric v6** — identity/L7 service policy maps (`fluxvm_spol`/`sid4`/`sid6`),
  Envoy transparent-proxy redirect contract, HA mutation queue drain (`fluxvm_haq`);
  ABI stays schema **4**, program generation **6**. See `docs/service-fabric-v6-phase6.md`.
- **Windows Kryton golden path** — sibling [Kryton](https://github.com/zyvorai/kryton)
  builds sysprepped qcow2; `scripts/prepare-windows-golden.sh` installs under
  `/var/lib/fluxvm/images/`; docs [`windows-golden.md`](docs/windows-golden.md) /
  [`tiny-windows.md`](docs/tiny-windows.md); examples `build-image-kryton-golden.json`,
  Tiny11 QGA/TAP; gated smokes accept `KRYTON_WINDOWS_IMAGE`.
- **In-tree KVM memory snapshots (Phase 3c)** — paused `FLUXKVM1` mmap dump +
  GPRs/sregs; restore via control API. Lab-only (not Firecracker-compatible).
  Smoke: `scripts/test-kvm-snapshot-smoke.sh`.
- **Cilium agent CEP identity (Phase 2b)** — when `mode=cilium`, enrich
  CiliumEndpoint views from agent HTTP (`identity_source=cilium-agent`); still
  never writes Cilium private maps.
- **Concurrent density bench** — `scripts/bench-density.sh` (parallel keep-alive
  creates + p50/p95).
- **MicroVM Prometheus histograms** — create→Scheduled / schedule→Running /
  create→Running on `MICROVM_METRICS_ADDR` (default `127.0.0.1:9108`).

### Changed
- **MicroVM dual-run hardening** — shadow Pods request `10m`/`32Mi` (not guest
  vCPU/RAM); converted MicroVMs annotate
  `microvm.fluxvm.zyvor.io/driven-by=fluxvm-kube` so the node agent skips
  `POST /v1/vms`; deploy controller leaves `--convert` off (opt-in). Policy
  gates: `scripts/test-microvm-policy.py`.
- **GuestImage Ready** — node reconciler marks Ready when `spec.source` is a
  host file; MicroVM `spec.image` can resolve a same-namespace GuestImage name
  (still no CDI pull). Docs/tutorials: [docs/tutorials/microvm/05-guestimage.md](docs/tutorials/microvm/05-guestimage.md).
- **Lab performance publish** — `scripts/bench-microvm.sh` + refreshed
  [docs/benchmarks/README.md](docs/benchmarks/README.md) (2026-09-08 sandbox
  avg_create **5444** ms; MicroVM p50_running **2736** ms).
- **MicroVM production deploy path** — `deploy-remote` installs
  `fluxvm-microvm`/`fluxvm-kube`; DaemonSet `FLUXVM_TOKEN` from Secret;
  `publish-image.yml` for GHCR; CI MicroVM gates; dataplane e2e uses bearer
  auth; Job/Pool in k8s smoke; GuestImage/Pool printer columns.
- **rustls CryptoProvider** — `fluxvm-microvm` / `fluxvm-kube` install `ring`
  before kube TLS clients (rustls 0.23).

### Added
- **MicroVM** (`fluxvm-microvm`) — Kubernetes-native disposable compute without
  KubeVirt: `MicroVM` / `MicroVMJob` / `MicroVMPool` / `GuestImage` on
  `microvm.fluxvm.zyvor.io`, shadow-Pod capacity tickets, cluster controller +
  node agent against local `fluxctl serve`, optional `DisposableVm` → `MicroVM`
  conversion. Docs: [docs/microvm.md](docs/microvm.md). Manifests:
  `deploy/k8s/microvm/`. Tests: `cargo test -p fluxvm-microvm` /
  `scripts/test-microvm.sh`. (Formerly internal aether design.)
- Hubble-style **packet flow** renderer: hop path (guest → tap → tc/eBPF →
  uplink → peer), `--output color|plain|json`, `fluxctl hubble flow` detailed
  view, `/v1/network/hubble/flows/text`, and a Colorful/Normal Hubble-lite UI
  ([docs/packet-flow.md](docs/packet-flow.md)).
- DevOps pack paired with Fabric: probe contract
  (`docs/contracts/fabric-fluxvm-readyz.json`), `scripts/devops-gate.sh`,
  `scripts/upgrade-snapshot.sh` for N→N+1 state_dir snapshots, kustomize
  wrapper `deploy/k8s/gitops`, CI workflow `devops-gates.yml`, and
  [docs/DEVOPS.md](docs/DEVOPS.md).
- **Four-track production** — OIDC already shipped; mTLS header identity
  (`X-Client-Cert-*` when `[tls].client_ca` set); CiliumEndpoint views +
  Hubble-lite UI/flows; Cloud Hypervisor `qga.enabled` serial socket;
  in-tree KVM `FLUXVM_KVM_LOCK_MEM=1` (MAP_POPULATE+mlock).
- **OIDC JWT validation** — `auth.oidc_issuer` + `auth.oidc_audience` enable
  discovery/JWKS bearer validation alongside `[[auth.tokens]]` (role/tenant claims).
- **API TLS / mTLS** — optional `[tls]` cert/key; `tls.client_ca` requires client certs.
### Added
- **In-tree KVM virtio-blk (Phase-2 start)** — attach `cfg.disk` / rootfs as
  virtio-mmio block at `0xFEB00200` with sector R/W; cmdline advertises both
  MMIO slots.
- **In-tree KVM linux-loader** — bzImage/ELF load via `linux-loader`, zero-page
  `boot_params` (cmdline/initrd/e820), RSI set for 64-bit boot protocol;
  raw-dump fallback if parse fails.
- **In-tree KVM CPUID + boot smoke** — `KVM_SET_CPUID2` so Linux can run;
  `--kernel`/`--disk` use `from_boot_config`;
  `scripts/test-kvm-linux-boot-smoke.sh` checks for `Linux version` on serial.
- **In-tree KVM root mount** — Firecracker-style e820, MP table, CMOS RTC,
  PIT2, virtio IRQ pulse; lab reaches `EXT4-fs (vda)` / `VFS: Mounted root`.
- **In-tree KVM userspace** — do not stop the VMM at root mount; smoke uses
  a ttyS0 probe init and passes on `FLUXVM_USERSPACE_OK` /
  `FLUXVM_PROMPT_READY`.
- **In-tree KVM serial console** — 16550 RX queue, IIR/LSR, COM1 IRQ 4 pulse
  for interactive UART; smoke injects `/userspace-probe.sh` onto a temp rootfs.
- **In-tree KVM serial stdin** — host stdin → UART RX; `FLUXVM_SERIAL_INJECT`
  one-shot after userspace; smoke checks `FLUXVM_STDIN_OK:ping`.
- **In-tree KVM pause/resume** — park vCPU (no `KVM_RUN` while paused);
  `scripts/test-kvm-pause-smoke.sh`. Memory snapshots remain Firecracker-only.
- **Lab verify scripts** — `scripts/test-lab-four-tracks-e2e.sh`,
  `scripts/test-lab-regression.sh`, `scripts/test-lab-verify.sh` for post-deploy
  four-track + regression gates on a KVM host (also runs devops units, live
  `devops-gate`, and `upgrade-snapshot`; stdin closed for SSH-safe serial smokes).
- **DevOps gate TLS** — `scripts/devops-gate.sh` uses `curl -k` and auto-picks
  Fabric HTTPS then HTTP when `FABRIC_URL` is unset.
- **Sandbox bench image paths** — `scripts/bench-sandbox.sh` honors `IMAGE` /
  `FLUXVM_BENCH_IMAGE` with lab fallbacks (`bionic-fabric-rootfs.ext4`, etc.).
- **CH Windows boot (Phase-1)** — `hyperv: true` → `kvm_hyperv=on`;
  `examples/windows-ch.json`; CH QGA host path via serial socket
  ([ch-windows-qga.md](docs/ch-windows-qga.md)); named virtio-serial remains QEMU-only.
- **Density roadmap** — [ROADMAP-DENSITY.md](docs/ROADMAP-DENSITY.md): KVM boot +
  pause + lock-mem Done; CEP-*shaped* Hubble-lite Done; real Cilium-agent CEP
  and kvm memory snapshots stay Not started / FC-only.
- **Project production baseline** — `SECURITY.md`, `CONTRIBUTING.md`,
  `Makefile`, whole-stack [docs/PRODUCTION.md](docs/PRODUCTION.md),
  `/readyz` (HTTP 503 when not ready), VM `tenant` + `GET /v1/vms?tenant=`,
  token `tenant` (authoritative create + scoped list/get/mutate),
  `scripts/release-checklist.sh`.
- **Production dataplane** — FQDN→IPv4/IPv6 resolve at apply, FluxVM ipcache,
  `GET /v1/network/health|/ipcache`, `POST /v1/network/refresh-dns`,
  `fluxvm dataplane health|ipcache|refresh-dns`, prod TOML + runbook
  ([docs/production-dataplane.md](docs/production-dataplane.md)).
  Tests: `scripts/test-production-dataplane.py`,
  `scripts/test-production-dataplane-e2e.sh`.
- **Network policy (Fabric v4)** — CNP compiler (`toCIDR`,
  `toCIDRSet`, `toEntities`, `toFQDNs`, `toPorts` ranges/named ports, deny,
  `enableDefaultDeny`, `auditMode`), reserved identities, `fluxvm_gid`
  updates, conntrack learn/hit, `fluxvm cnp` / `fluxctl identity` /
  `fluxctl observe`, REST `/v1/network/cnp`, `/v1/network/identities`,
  `/v1/network/observe`.
  Docs: [docs/network-policy.md](docs/network-policy.md).
  Tutorials: [docs/tutorials/network-policy/](docs/tutorials/network-policy/README.md).
  Tests: `scripts/test-network-policy.py`;
  e2e via `scripts/test-security-groups-e2e.sh`,
  `scripts/test-production-dataplane-e2e.sh`.
- **Security groups** — label identities for the VM-edge
  dataplane. Named groups with `key=value` labels allocate a stable
  identity in the `0x10000+` range; VM policy `groups` / `labels` select
  membership; allow + deny CIDRs merge into the TC maps. REST
  `/v1/network/groups` and `/v1/vms/{id}/network/effective`, CLI
  `fluxvm group`. BPF `fluxvm_gid`, `fluxvm_ct`, `fluxvm_deny4/6`,
  ICMP passthrough, `icmp/0` L4 rules.
  Docs: [docs/network-groups.md](docs/network-groups.md).
  Tests: `scripts/test-security-groups.py`,
  `scripts/test-security-groups-e2e.sh`.

- **Fleet agent hardening** — bearer auth (`--token`), optional TLS (`--tls-cert`/`--tls-key`), persisted `fleet-nodes.json`, residual CPU/memory placement.
- **Kubernetes CRD** — `tap`/`macvtap` fields (`bridge`, `parent`, `netns`, …); optional `spec.node` + `fluxvm-kube --enable-placement`; DaemonSet packaging documented; CI container image build.
- **Security boundary** — fail-closed auth off-loopback, JSON audit logs (`fluxvm_audit`), `allowed_network_modes` / `allow_extra_args` policy, per-token VM/memory quotas, seccomp-bpf (Linux), AppArmor profile, UDS `0o600`, optional cosign catalog verify.
- **Network** — netns NAT via nftables, real IPAM (`ipam.json`), egress allowlist wired to dataplane, auto L7 redirect when proxy listen is set.
- **Snapshots** — QEMU QMP `savevm` + Cloud Hypervisor `ch-remote snapshot`; `POST /v1/vms/{id}/snapshot`.
- **Sandbox** — `fluxvm_engine = "kvm"` (no Firecracker child), multi-port proxy defaults, native TC/eBPF dataplane (`legacy`/`ebpf`/`cilium`, default nftables), bench script + docs.
- **eBPF / Cilium / Network Fabric v1** — TC L3+L4 allowlists, per-VM policy API (`/v1/vms/{id}/network/{policy,stats,flows}`), optional XDP node guard, flows/stats maps; modes `legacy`/`ebpf`/`cilium` (default nftables); docs: [docs/network-fabric.md](docs/network-fabric.md), [docs/ebpf-cilium.md](docs/ebpf-cilium.md).
- **Network Fabric v2** — Mbps/PPS egress limits, fail-closed live policy reconfigure, `GET /v1/vms/{id}/network/status`, native attach without known guest IP, `scripts/validate-network-fabric.sh`.
- **Network Fabric v3 GA** — IPv4/IPv6 TC+XDP policy, schema versioning + policy fingerprints, TC/XDP program-ID ownership, pre-attach map config, orphan pin reconcile, NDJSON flow exporter (`scripts/export_network_flows.py`).
- **Windows** — `unattend_path` / `sysprep` on `build-image` `windows{}`.
- **QEMU placement** — optional `numa_node`, `cpuset`, `hugepages`, `vfio_devices` on create.
- Richer Prometheus metrics (auth/egress denies, create/start latency).

### Fixed
- **TLS / mTLS serve** — install rustls `ring` CryptoProvider before binding so
  `[tls]` / `client_ca` no longer panic on rustls 0.23 feature detection.
- **CH QGA gate** — allow `qga.enabled` on `cloud-hypervisor` (serial socket),
  not only QEMU virtio-serial.
- **eBPF smoke** — `scripts/test-ebpf-smoke.sh` uses dual netns and one persistent
  `ip netns exec` session so policy/XDP checks work on hosts where same-netns
  `ping -I` fails and `/sys` remounts drop bpffs pins across separate execs.

### Changed
- Docs/README refreshed for Network Fabric **schema v4** (groups, CNP, observe,
  health/ipcache/FQDN refresh, production runbook, tutorials) while keeping
  historical v1–v3 notes; architecture, L4/IPv6/rate limits, REST status/schema,
  XDP, `LimitMEMLOCK`, dual-netns smoke/e2e, and vs-traditional comparison remain.
- Docs/tutorials for project production: `/readyz`, token/VM `tenant`,
  [docs/tutorials/production/](docs/tutorials/production/README.md),
  auth exceptions, and links from [PRODUCTION.md](docs/PRODUCTION.md).
- **Network Fabric v3 GA ship** — `configs/network-fabric-ga.toml` +
  `scripts/enable-network-fabric-ga.sh`; `required=true` fail-closes only when a
  host-visible VM edge exists (user NAT / `mode=none` soft-skip).

## 0.3.0

### Added
- **Windows offline customize** — a `windows{}` block on `build-image` (RDP/WinRM/firewall/scripts + Zyvor GuestKit agent inject), via GuestKit registry plans and `inject_windows_agent` (needs host `libhivex`/`hivex-devel` and guestkit's `registry-write` + `agent` features). Linux-only fields (`packages`, `commands`, `enable_services`, `ssh_key`, top-level `hostname`) can't be combined with it.
- **Live QGA control** — QEMU virtio-serial guest-agent CLI/REST for PowerShell and firewall rules after boot: `fluxctl qga ping|powershell|exec|firewall-open|firewall-close`, mirrored at `POST /v1/vms/{id}/qga/ping|exec|firewall/open|firewall/close`.
- Gated offline smoke test: `scripts/test-windows-customize.sh`.
- Client presentation decks (`docs/client-presentations/`).

## 0.2.0

### Added
- **VNC** for every QEMU-backed VM via a unix socket — no port allocation.
- **Interactive console/shell** — `GET /v1/vms/{id}/console` (WebSocket) and the guest agent's `OpenShell` vsock op for a real PTY.
- **File transfer** — `PutFile`/`GetFile` guest-agent vsock ops (`POST /v1/vms/{id}/agent/{put,get}-file`).
- **virtiofs shared folders** (QEMU backend).
- **True suspend-to-disk resume** (`-loadvm`) and a virtio-scsi controller.
- **Static-IP netns mode** — the guest gets a real DHCP-leased IP, with deterministic address reservation.
- **`GET /v1/vms/{id}/cpuset`**.
- **Kubernetes DaemonSet packaging** (`fluxvm-kube`).
- Catalog: read-only flag and orphaned-download cleanup.
- Cross-distro CI job, build-image tutorials, and a real-hardware boot smoke test.
- Multiple hypervisor backends: `fluxvm-hypervisor`, `fluxvm-cloud-hypervisor`, `fluxvm-firecracker` (alongside the existing QEMU backend).

### Changed
- **Image customization now goes through GuestKit** instead of virt-customize/libguestfs.
- Renamed Ephemera to FluxVM; added an agent-sandbox track.
- `KillMode=process` on the systemd unit, so a daemon restart/upgrade no longer SIGTERMs every running VM's QEMU process.

### Fixed
- CPU/memory hotplug (no reserved PCIe root port slots) and NIC hotplug (missing `bridge.conf`) — hotplug now actually attaches.
- Usermode-network hostfwd bound to `127.0.0.1` instead of `0.0.0.0`.
- Guest agent `OpenShell`: isolated into a double-forked process, never reaped its own shell child, fixed `EPERM` on `TIOCSCTTY`, added `PrivateTmp` for disk provisioning.
- Dockerfile build failure (stale Rust pin, missing `--locked`).
- `-nodefaults` was dropping the implicit VGA card — added explicit `-vga std`.
- `ephemera-image` (now the image crate) mounts guest fstab entries shallowest-first.
- systemd: grant `CAP_SYS_CHROOT` for GuestKit's image-customization chroot, `CAP_SYS_ADMIN` and a writable `/var/lock` for disk provisioning.
- systemd: `/sys/fs/cgroup/fluxvm.slice` creation failed with `Permission denied` despite running as root — the unit's capability set didn't include `CAP_DAC_OVERRIDE`, so root's usual DAC bypass against the cgroupfs's `dr-xr-xr-x` mode didn't apply. Found live: every VM start/restart since a host reboot left `VmRecord.cgroup_path` null, breaking memory limit/usage and any other cgroup-based resource control.
- systemd: `RuntimeDirectory=fluxvm`/`StateDirectory=fluxvm` replace a plain `ExecStartPre mkdir` for `/run/fluxvm` — the old approach raced ProtectSystem's namespace setup and failed with `226/NAMESPACE` on every fresh boot (`/run` is a tmpfs, wiped every reboot).
- cloud-init: `write_files` support in `CloudInitSpec`.
- Test fixtures: added `shared_folders`/`virtiofsd_pids` to the storage test fixture.
- Fixed the pacman branch of the GuestKit package install and hardened its regression test.

## 0.1.0

- Initial release.
