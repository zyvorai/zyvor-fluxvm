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

## Not covered here

EL9 (glibc 2.34 is below the package's floor; needs a build on EL9), an `apply` of the manifests against a real
cluster, `test-fleet-agent.sh` (needs two hosts), and a real fleet of 10,000 distinct clients with TLS.
