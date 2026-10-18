# Release and fleet test report

`scripts/test-release-e2e.sh --host <user@host>` runs every suite below on a Linux host (rsync, one release build of
`fluxvm-agent`, throwaway containers and state directories, nothing left behind) and prints one line per suite.

Run on Ubuntu (kernel 7.0, 12 cores, docker + podman), commit after #499, 2026-10-18:

| Suite | What it covers | Result |
|---|---|---|
| `static` | rustfmt, shellcheck (when installed), manifest/vendor-lock/glibc-floor/guestkit-pin tests, `verify-systemd-units` (29 units), `cargo test -p fluxvm-agent` (276) | ok |
| `packages` | `.deb` and `.rpm` built by nfpm: install, upgrade (edited config kept), remove, purge; rpm in Fedora 43 and Rocky 10 | ok |
| `bootstrap-tarball` | signed channel and manifest: tamper, wrong key, stale serial, expiry, revocation, `min_upgrade_from`, waves, rollback, resume, 429 | 35/35 |
| `bootstrap-deb` / `bootstrap-rpm` | the pull installer against a signed mirror in Ubuntu 22.04 / Fedora 43: install, upgrade, a release that does not come up (auto rollback), manual rollback, pin, verify, cache bounds | ok |
| `ha` | leader election, fencing and takeover against a real Redis | ok |
| `enroll-invariants` | 150 concurrent enrolls on `max_uses=100`, mixed fleet, `kill -9` | ok |
| `enroll-security` | 58 checks: token limits, expiry, revocation, name patterns, replay after rotate/revoke, what a credential may reach, `--node-auth` modes, no secrets in logs | ok |
| `central-node` | real nodes enrolling from a token file, restart without it, version gate, node list filters and paging, `kill -9` of central, 429 under overload | ok |
| `scale` | sim-fleet at 1,000 / 5,000 / 10,000 nodes: 0 failed beats, p99 <= 4 ms | ok |
| `systemd-deb` | the `.deb` under a real systemd (rootless podman, Ubuntu 22.04): `apt-get install` resolves dependencies, unit enabled but not started on a fresh install, every `ExecStart` target exists, `kill -9` restarts the daemon, the bootstrap timer is active and its oneshot runs, upgrade keeps edited config and does not restart the daemon, downgrade, remove, purge | 22/22 |
| `image-k8s` | the Dockerfile built from a staged tree and run (labels, binaries, bpf objects, tools, `bpftool`, empty entrypoint, no apt lists), `render-k8s.sh` output (5 images pinned by version and digest, bad arguments refused, no Secret, 19 well-formed documents) | ok |
| `agents` | `cargo test` for fluxvm-agent (276), fluxctl (228), fluxvm-guest-agent (34), guest/container protocols, container agent, vsock client: 0 failed | ok |
| `mutation` | six guards in the enrollment code removed one at a time (enrolled gate, revoked credential, revoked token, expired token, credential mac, rotation window): all six make a test fail (five the e2e suite, the rotation window the unit tests) | 6/6 killed |
| `deploy-remote` (`scripts/e2e/deploy-remote.sh`, from the control machine) | `--help`, `--dry-run`, `--preflight-only`, `--verify-only` change nothing; two real `--quick` deploys: same binary, service active, no new failed units | 18/18 |
| real VMs (existing scripts, run with `sudo` on the host with a guest-agent image built by `fluxctl build-image`) | `test-lifecycle.sh` 7/7 (exec over vsock, pause freezes CPU time, graceful stop, unique CIDs), `test-guest-agent-auth.sh` 5/5 (no token and wrong token refused on a raw vsock socket), `test-boot-smoke.sh` 7/7, `test-warm-pool.sh` 13/13 (beside the live daemon, on its own port) | ok |

`systemd-analyze security` rates `fluxvm.service` 9.0 UNSAFE: the daemon runs as root with broad capabilities by design, so this is
recorded, not gated.

## Bugs the suites found (fixed with the suites)

- The `.rpm` owned its directories at mode 0775 when built under umask 0002, and could not be installed next to
  `systemd-udev` (`/usr/lib/modprobe.d` is 0755). The packager now fixes the umask and the modes, and `test-rpm.sh` checks them.
- `fluxvm-bootstrap` checked `/usr/local/bin/fluxctl` after a deb or rpm install (the packages use `/usr/bin`), so every
  package install was reported unhealthy.
- `fluxvm-bootstrap` counted "service not enabled" (a fresh node, a container) as a failed restart and rolled back.
- After an automatic rollback the "previous" pointer was left on the version it had just returned to, so a manual
  rollback could not reach the version before it.
- The cache kept the failed package and every manifest it had ever fetched.
- The Dockerfile could not be built: it installs `bpftool`, and Ubuntu 24.04 has no such package (the binary ships in
  `linux-tools-<kernel>`). The image now installs `linux-tools-generic` and links its `bpftool` into `PATH`.
- `deploy-remote.sh` took an unknown option as a positional argument (the "password") and carried on with a full deploy,
  so a typo such as `--verfiy-only` deployed. It now refuses unknown options (`--` ends option parsing).
- `test-warm-pool.sh` could not run beside a live daemon (fixed port 7788); `BASE_URL` can now be overridden.

## Findings that are not bug fixes

- `deploy/k8s/gitops/kustomization.yaml` does not build (`kubectl kustomize` refuses its `../` resources), and with the
  load restrictor off it still carries the `__VERSION__` / `__DIGEST__` placeholders. `image-k8s` prints these as `KNOWN-GAP`.
  The supported path is `scripts/render-k8s.sh`.
- `purge` keeps `/etc/fluxvm.toml`, `/etc/fluxvm` and `/var/lib/fluxvm` on purpose (`postrm.sh`).
- The rotation window (one hour) cannot be waited out by the shell suite; only the unit test covers its end.
- The `.deb` was not installed on the test host itself: that host already runs a hand-deployed `fluxvm.service` from
  `/usr/local/bin`, which a package would overwrite. The package was installed under real systemd in a container instead.

## Agents, on Linux and on the Mac

Run 2026-10-18 on Ubuntu 26.04 (212.8.248.187; 80.79.5.173 for the two-host run) and on an Apple-silicon Mac mini (macOS 27.2,
16 GiB). Every run used its own ports and state directory; nothing of the hosts' own services was stopped or reconfigured.

| Suite | Linux | Mac |
|---|---|---|
| `cargo test`: fluxvm-agent (277), guest-agent (34), guest/container protocols (16, 12), container agent (34), shim (35), vsock/container clients, fluxctl (228) | 0 failed | fluxvm-agent, apple (131 + 21 contract tests), vz-switch (15), usernet (76), guest-agent (33), guest-protocol (16), vsock-client (2): 0 failed |
| `agent-central-api` (`scripts/e2e/agent-central-api.sh`): tenants and keys (scopes, read-only, revoke, rotate, disable, quota), what a tenant cannot reach, webhooks (https/private-address rules), budget/usage/billing, cordon, overlays, Mac leases (24 h minimum, forced release, scrub before reuse), console assets, `/metrics`, secrets in logs, restart durability, SIGTERM save | 82 checks, ok | 82 checks, ok |
| `agent-central-ha`: real Redis, two centrals, `kill -9` takeover, old leader returns as a standby, `SIGSTOP` fencing | 21 checks, ok | 21 checks, ok |
| `enroll-security` (58), `central-node`, `enroll-invariants`, `scale` | ok (see above) | ok; sim-fleet 1,000 nodes (32 MiB, p99 0 ms) and 5,000 nodes (89 MiB, p99 0 ms), 0 failed beats |
| `agent-central-vms`: a real QEMU VM placed through central, listing, exec over vsock, logs, stop/start, tenant isolation, delete | 25 ok, 3 known gaps | n/a |
| `guest-exec-policy`: exec policy inside a real guest (Landlock/seccomp): allowed reads and writes work, others are refused by the kernel, TCP deny, namespaces, memory limit, output cap, timeout, malformed and unenforceable policies fail closed | 19 checks, ok | n/a |
| `fleet-two-host` (`scripts/e2e/fleet-two-host.sh`): central on one real host, node agents on two, a real VM on each, capacity from /proc, heartbeats over the network, vm_count, cordon, node restart, `kill -9` of central | 17 checks, ok; temporary firewall rules removed, INPUT checksum unchanged | n/a |
| MCP end to end (`scripts/test-mcp-sdk.py`) and the Python (55), Go and TypeScript (26) SDK unit tests | ok | ok |
| SDKs against a real daemon (`scripts/e2e/sdk-live.sh`; Python on the host, Go through a tunnel) | ok | n/a (procbox is Linux-only) |
| procbox (114 tests) | as `sus`: 11 isolate tests skip (AppArmor blocks unprivileged user namespaces); as root: all run, 0 skipped | n/a |
| Guest agent over vsock: `test-lifecycle` 7/7, `test-guest-agent-auth` 5/5, `test-boot-smoke` 7/7, `test-warm-pool` 13/13 | ok | n/a |
| vz live tests, each on its own daemon and temp state: `macos-live-test` 54, `vz-devices` 26, `vz-usb-buses` 9, `vz-usernet` 11, `vz-vsock-services` 7, `vz-sandbox-volume` 6, `vz-rewind` 11, `vz-asif-overlay` 3, `macos-vz27-complete` (fluxvm-apple build, tests, clippy `-D warnings`, EFI Secure Boot on and off) | n/a | all ok |

### Bugs found and fixed

- **A graceful stop lost nodes.** `fluxvm-agent central` has no final save: on SIGTERM (systemd stop, a rollout) every node
  registered since the last background save was gone after the restart. It now saves the registry before exiting
  (`RegistryFlush`, with a unit test and a process-level check).
- `scripts/test-mcp-sdk.py` expected 12 fewer read-only MCP tools than `fluxctl mcp serve` exposes, so `mcp.yml` would fail on
  `main`; the read-only set is now exact and the write set is checked as a superset.
- `scripts/macos-live-test.sh` forced the generated `fluxvm_ed25519` key when any of `id_ed25519.pub`, `id_ecdsa.pub`,
  `id_rsa.pub` was missing (a `ls a b c` test under `pipefail`), so the stack check failed on a Mac that has just one key. The
  example stack also publishes port 5000, which macOS's AirPlay receiver owns; `FLUXVM_LIVE_DB_PORT` picks another.
- `scripts/test-fleet-agent.sh` can no longer start: `fluxctl serve` now refuses to listen on `0.0.0.0` without auth.
  `scripts/e2e/fleet-two-host.sh` replaces its working parts.
- `test-warm-pool.sh` and others hardcoded ports; `scripts/e2e/central-node.sh` and `enroll-security.sh` used GNU `stat -c`.

### Findings (not bugs in this change)

- **Central cannot control VMs on another host.** It calls a node's daemon with no credential, and the daemon refuses to listen
  on a non-loopback address without auth. Across hosts, registration, capacity, heartbeats and cordon work; create/exec/delete
  through central only work when the daemon is on the same host (loopback). Which credential central should present is still an
  open decision.
- Leases, volumes and warm pools are Mac features: on a Linux node a lease is refused ("not a Mac") and a pool backs off ("no host
  nobody leases"). QEMU snapshots and clones through central are refused ("disk-only snapshots are implemented for the vz backend").
- A guest on Landlock ABI 4 (Ubuntu 24.04) cannot enforce the default exec policy (`scope_ipc` needs ABI 6); the guest refuses to
  run the command rather than run it unconfined. `docs/guest-exec-policy.md` now says so and is marked verified.
- A released Mac must be scrubbed before it is leased again; an early lease release is refused until 24 hours, an operator can force it.
- The Mac's data volume was at 99-100% during this work. It briefly reached 100% (one VM image unpack failed with "No space
  left on device") and the Mac's SSH port-forward to the `aurora` VM was no longer running afterwards; the VM itself, the daemon
  on 7788 and the hand-installed binaries were unchanged.

### Not run

- Mac: `vz-agent-session` and `vz-https` (need `agent-micro-arm64.raw`, built on Linux), `vz-host-gpu` (uses the shared 7788
  daemon), the macOS-guest/IPSW tests (attended, 30-60 GB), `bench-sandbox-mac`, and `macos-vz27-live-test` (needs a hand-written
  VZ config and runner path).
- Linux: the containerd end-to-end secure-container gate (no containerd service; the host's containerd is k3s's), central's
  WebSocket tunnel, the fleet console's JavaScript, `gha-runners` against GitHub.
- No launchd plist exists for `fluxvm-agent central|node`; the guest LaunchDaemon is marked experimental.

## Not covered here

EL9 (glibc 2.34 is below the package's floor; needs a build on EL9), an `apply` of the manifests against a real
cluster, and a real fleet of 10,000 distinct clients with TLS.
