# Using FluxVM in DevOps

FluxVM is the sibling engine under [zyvorai/fabric](https://github.com/zyvorai/fabric). Platform pipelines should:

1. Bootstrap the host (`scripts/bootstrap-host.sh` — KVM, nbd, dirs).
2. Run `fluxctl serve`.
3. Gate on `GET /healthz` (alive) and `GET /readyz` (state dir + dataplane when required).
4. Let Fabric (or `fluxctl create --spec`) be the only writer.

## Probes

| Path | Auth | Ready meaning |
|------|------|----------------|
| `/healthz` | no | process up (`{"ok": true}`) |
| `/readyz` | no | `ok` + optional `kvm`, `state_dir`, `dataplane`; HTTP 503 when not ready |

Fabric `GET /readyz` nests this body under `fluxvm` and will stay 503 until this endpoint is 200.

See [contracts/fabric-fluxvm-readyz.json](contracts/fabric-fluxvm-readyz.json).

## CI

Portable gates (GitHub-hosted runners, no nested KVM):

| Workflow | When | What |
|---|---|---|
| [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) | every PR + `main` | workspace build/test, explicit virtio-vsock CSM units, MicroVM gates, image-customize matrix (runs on `ubuntu-24.04`: it needs `sudo modprobe nbd`, which the `zyvor-build` runners refuse), container-image build from a stub release tree (the Dockerfile copies a staged release tree and compiles nothing) |
| [`.github/workflows/cargo-update.yml`](../.github/workflows/cargo-update.yml) | Mondays 06:00 UTC + **workflow_dispatch** | `cargo update`, guestkit pin gate, `cargo check --locked`, `cargo audit`; opens a `Cargo.lock` PR. Add a `CARGO_UPDATE_TOKEN` secret so that PR gets CI (PRs from the default `GITHUB_TOKEN` do not trigger workflows) |
| [`.github/workflows/runner-workspace-cleanup.yml`](../.github/workflows/runner-workspace-cleanup.yml) | **workflow_dispatch** only | repair for a `zyvor-build` runner whose `_work/zyvor-fluxvm` holds root-owned files (checkout fails with `EACCES`): chowns the workspace back to the runner user; run it until the log shows the affected runner |
| [`.github/workflows/devops-gates.yml`](../.github/workflows/devops-gates.yml) | every PR + `main` | Fabric contract/examples, offline devops-gate, upgrade-snapshot dry-run |
| [`.github/workflows/network-fabric.yml`](../.github/workflows/network-fabric.yml) | path-filtered | eBPF build, fabric preflight/enable dry-run, workspace tests; opt-in TC/XDP smoke |
| [`.github/workflows/all-features.yml`](../.github/workflows/all-features.yml) | `push` to `main` + **workflow_dispatch** | umbrella of portable suites (vsock, fabric, devops, SC units + use-case matrix, Sentinel/intelligence static) |
| [`.github/workflows/secure-containers.yml`](../.github/workflows/secure-containers.yml) | path-filtered | SC crate fmt/unit/release build |
| [`.github/workflows/secure-containers-coverage.yml`](../.github/workflows/secure-containers-coverage.yml) | path-filtered | use-case matrix SoT lint + SC/NP/eBPF portable coverage |

### Runner prerequisites (`zyvor-build` self-hosted runners)

`ci.yml` `build-and-test` runs `scripts/ci-install-apt-deps.sh libsystemd-dev libhivex-dev clang llvm libbpf-dev linux-libc-dev`.
The script checks `dpkg-query` first and calls `apt-get` (through `sudo -n` unless root) only for packages that are missing.
The runners are not uniform: `zyvor-build-a`/`-b` allow `sudo apt-get`, `zyvor-build-c` to `-f` refuse it. So on c to f those
packages must be preinstalled (as root: `apt-get update && apt-get install -y <packages>`), or a sudoers rule added, for example
`/etc/sudoers.d/ghrunner-apt` with `<runner user> ALL=(root) NOPASSWD: /usr/bin/apt-get`. A runner that lacks them fails the job with
that remediation in the log instead of a bare sudo refusal. `scripts/test-ci-install-apt-deps.sh` (run by `devops-gates.yml`) tests the
script. Do not add a bare `sudo apt-get install` to a workflow that can land on those runners.

Privileged workflows (`flight-recorder.yml`, `memory-boot-profiler.yml`, `topology-intelligence.yml`) run their host tests under
`sudo`, which resets `PATH`, `CARGO_HOME` and `HOME` and would hide the runner user's rustup toolchain. They pass
`PATH`, `CARGO_HOME` and `RUSTUP_HOME` through: `sudo env "PATH=$PATH" "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}" ./scripts/test-....sh`.

Shell tests: do not write `echo "$out" | grep -q X` under `set -o pipefail`. `grep -q` exits at the first match and the writer then
dies with SIGPIPE ("echo: write error: Broken pipe"), a race that failed CI on loaded runners. Match against a here-string, as the
`has()` helper in `scripts/test-microvm.sh` does (`has() { grep -Eq -- "$1" <<<"$out"; }`), and use `$(...)` instead of
`cmd | grep -q` the way `ci-install-apt-deps.sh` does.

Live KVM / privileged BPF / multi-node stays on **self-hosted** runners and stays **off** until the matching repo variable is set (Settings → Variables).

### Repo variables (`vars.*`)

| Variable | Effect |
|---|---|
| `FLUXVM_EBPF_PRIVILEGED_CI=1` | Privileged BPF host steps on GitHub-hosted ubuntu (Network Fabric smoke, AF_XDP, XDP/TCP, VMM guard, topology, memprof, quiclb, scx, flight-recorder) |
| `FLUXVM_SECURE_CONTAINERS_LIVE_CI=1` | Live Secure Containers jobs on `fluxvm-lab` |
| `FLUXVM_REQUIRE_MULTI_NODE=1` | Multi-node NetworkPolicy live job |
| `FLUXVM_SECOND_CNI_GATE` / `FLUXVM_KATA_GATE` / `FLUXVM_REAL_FLEET_GATE` / `FLUXVM_ATTACHED_MIGRATION_GATE` | Set19 fail-hard GA command strings |
| `FLUXVM_LAB_KUBECONFIG` / `FLUXVM_RUNTIMECLASS` | Lab kubeconfig / RuntimeClass for live SC |
| `FLUXVM_SENTINEL_PRIVILEGED_CI=1` | Sentinel GA failure-injection on `fluxvm-sentinel-lab` |
| `FLUXVM_UPGRADE_DESTRUCTIVE_E2E=1` | Upgrade-manager bpffs round-trip on `fluxvm-ebpf-lab` |
| `FLUXVM_MIGRATION_PRIVILEGED_CI=1` (+ `FLUXVM_MIGRATION_TEST_PLAN`) | Migration orchestrator lab |
| `FLUXVM_FLEET_E2E=1` | Fleet rollout lab |
| `FLUXVM_FLEET_GUARD_HOST_TEST=1` (+ `FLUXVM_FLEET_GUARD_PLAN`) | Fleet guard host test |

### Self-hosted runner labels

| Label set | Used by |
|---|---|
| `fluxvm-lab` | Secure Containers live |
| `fluxvm-ebpf-lab` | Sentinel upgrade-manager destructive E2E |
| `fluxvm-sentinel-lab` | Sentinel GA certification privileged |
| `fluxvm-migration` | Migration orchestrator lab |
| `fluxvm-fleet` / `fluxvm-fleet-lab` | Fleet guard / fleet rollout |

Use-case matrix SoT: [secure-containers-use-case-matrix.md](secure-containers-use-case-matrix.md) (enforced by `scripts/check-use-case-matrix.sh`).

Lab smoke scripts (`scripts/test-boot-smoke.sh`, dataplane e2e) remain operator/self-hosted — not PR-gated on stock ubuntu.
## Lab verify

**Easiest:** ship the stack from sibling Fabric (or `./scripts/ship` here):

```bash
./scripts/ship sus@HOST
```

Post-deploy lab pack on a KVM host (pairs with Fabric HTTPS `:9095`):

```bash
sudo -E ./scripts/test-lab-verify.sh
# covers: devops units + live devops-gate + upgrade-snapshot +
#         four-tracks e2e + regression (readyz / KVM boot / sandbox / eBPF)
```

`scripts/devops-gate.sh` uses `curl -k` for Fabric self-signed TLS and auto-picks
`https://127.0.0.1:9095` when `FABRIC_URL` is unset.

## Production readiness

Read-only gate (Service Fabric schema/pins + Network Fabric health; optional VIP SLO):

```bash
FABRIC_URL=https://127.0.0.1:9095 FLUXVM_URL=http://127.0.0.1:7788 \
  ./scripts/test-production-readiness.sh

# with VIP latency / SLO
VIP=10.96.0.10 VIP_PORT=80 SERVICE=payments \
  ./scripts/test-production-readiness.sh
```

See [PRODUCTION.md](PRODUCTION.md) and [service-fabric-phase6.md](service-fabric-phase6.md).

## Kubernetes

DaemonSet in `deploy/k8s/` uses hostNetwork so Fabric on the same node can reach `127.0.0.1:7788`. GitOps wrapper: `deploy/k8s/gitops`.

## Upgrade with Fabric

Snapshot FluxVM **first**, then Fabric:

```bash
sudo ./scripts/upgrade-snapshot.sh snapshot --tag before-$VERSION
# install fluxvm
./scripts/upgrade-snapshot.sh verify
# on the Fabric repo:
#   sudo ./scripts/upgrade-rollback.sh snapshot --tag before-$VERSION
```

Examples: [examples/devops/README.md](https://github.com/zyvorai/zyvor-fluxvm/blob/main/examples/devops/README.md).
