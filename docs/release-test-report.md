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

## Bugs the suites found (fixed with the suites)

- The `.rpm` owned its directories at mode 0775 when built under umask 0002, and could not be installed next to
  `systemd-udev` (`/usr/lib/modprobe.d` is 0755). The packager now fixes the umask and the modes, and `test-rpm.sh` checks them.
- `fluxvm-bootstrap` checked `/usr/local/bin/fluxctl` after a deb or rpm install (the packages use `/usr/bin`), so every
  package install was reported unhealthy.
- `fluxvm-bootstrap` counted "service not enabled" (a fresh node, a container) as a failed restart and rolled back.
- After an automatic rollback the "previous" pointer was left on the version it had just returned to, so a manual
  rollback could not reach the version before it.
- The cache kept the failed package and every manifest it had ever fetched.

## Not covered here

The container image (never built), k8s manifests rendered and applied, the `.deb` installed under real systemd with
the timer firing, `deploy-remote.sh` idempotency, real VMs (`/dev/kvm` is not open to the deploy user), EL9 (glibc 2.34
is below the package's floor), and a real fleet of 10,000 distinct clients.
