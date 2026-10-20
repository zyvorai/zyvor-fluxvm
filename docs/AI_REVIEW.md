# AI code review (fluxvm)

_Generated 2026-10-20 by claude-opus-5-5 from a snapshot of tracked sources; verify before acting._

# FluxVM code review

## Summary

FluxVM is a Rust VM control plane: one REST API and CLI (`fluxctl serve`) over QEMU/KVM, Cloud Hypervisor, Firecracker, an in-tree KVM VMM (`crates/fluxvm-hypervisor`) and Apple Virtualization.framework (`vz`). Around that core it ships:

- an eBPF VM-edge dataplane (`bpf/`, `crates/fluxvm-network`);
- a containerd runtime-v2 shim ("Secure Containers");
- Kubernetes operators (`fluxvm-kube`, `fluxvm-microvm`);
- a Mac fleet, Python/Go/TS SDKs, a Terraform provider;
- many Python/shell "Sentinel" ops tools.

The repo is very large and documentation-heavy. Much of the newer work (Sets 5–19, FluxMesh) is self-described as not compiled or not run live.

## Bugs and correctness risks

**Static gates that cannot fail.** Under `set -e`, a command negated with `!` never triggers errexit. These checks print matches but always let the script continue:
- `scripts/test-migration-orchestrator-static.sh`: `! grep -R ... 'shell=True' "$ROOT/tools"`.
- `scripts/test-sentinel-release-admission-static.sh`: both `! find ... | grep -q .` lines (`__pycache__` / `.pyc` checks).
- By contrast, `scripts/test-sentinel-fleet-rollout-static.sh` does this correctly with `|| exit 1`. Use the same pattern.

**`kubectl apply -f microvm/` overwrites the real token.**
- `deploy/k8s/README.md` and `docs/tutorials/microvm/README.md` both tell users to apply the whole directory.
- That directory contains `deploy/k8s/microvm/secret.yaml` with `token: REPLACE_ME`, which silently replaces the secret created in the previous step.
- It also contains `servicemonitor.yaml`, which fails to apply without the Prometheus Operator CRDs.

**GitOps kustomization moves everything into the wrong namespace.** `deploy/k8s/gitops/kustomization.yaml` sets `namespace: fluxctl`, overriding `fluxvm-system`.
- Docs, logs commands and the `fluxvm-microvm-token` secret all assume `fluxvm-system`.
- `commonLabels` also rewrites DaemonSet/Deployment selectors. Selectors are immutable, so applying this over an existing install will fail. `labels:` with `includeSelectors: false` avoids that.

**Port reads are never returned to the guest (`crates/fluxvm-hypervisor/src/vcpu.rs`).**
- On `IoIn` and `MmioRead`, the loop fills a local `buf` and then drops it, so the guest never receives the value.
- MMIO errors are discarded (`let _ =`).
- `Unknown` exits loop forever while printing.
- If this loop is only the legacy/demo path, mark or remove it. Otherwise it is a correctness bug.

**Benchmark parsing failures become passing values (`scripts/sentinel-ga-benchmark-network.sh`).**
- `packet_loss_pct` defaults to `0.0` and throughput to `0` if parsing fails.
- A failed parse therefore reports "0% loss", which contradicts `benchmarks/README.md` ("missing metrics are not silently passed").
- The default server `127.0.0.1` measures loopback, not a VM path.

**Merging measurements loses provenance.** `scripts/sentinel-ga-merge-measurements.py` drops each input's `metadata` (host, kernel), keeping only file names.

**Installer depends on the working directory.** `scripts/install-secure-containers.sh` uses relative `target/release/...` paths without `cd` to the repo root. It also swallows env-example install failures with `2>/dev/null || true`.

**Microvm controller image is stale.** `deploy/k8s/microvm/controller.yaml` pins `ghcr.io/zyvorai/fluxvm:0.1.0`, while crates are at 0.4.8.

**Quickstart `cd` is wrong.** `README.md` runs `git clone .../zyvor-fluxvm.git && cd fluxvm`, but the clone directory is `zyvor-fluxvm`.

**Builds are not reproducible from this repo alone.**
- `crates/fluxvm-image/Cargo.toml` depends on `path = "../../../guestkit"`, outside the repo.
- Sources disagree on which repo that is: CI checks out `zyvorai/guestkit`, while `README.md` links `zyvor-guestkit`.

**Go and Actions versions may not resolve (uncertain).**
- `go 1.27` in `tools/fluxvm-policy-observer/go.mod` and `controllers/fluxvm-networkpolicy-controller/go.mod`.
- `actions/checkout@v7` and `setup-python@v7`/`setup-go@v7` across workflows, while `examples/devops/github-actions/fluxvm-gates.yml` uses `@v4`/`@v5`.
- Verify these exist. If not, `setup-go` with `go-version-file` and every pinned workflow will fail.

## Security concerns

**Unauthenticated admin on loopback.**
- Per `config.example.toml`, auth is off on loopback, and callers become `anonymous-admin`.
- `deploy/k8s/configmap.yaml` configures no `[auth]`.
- The node agent and NetworkPolicy controller rely on `hostNetwork` to reach `127.0.0.1:7788`. Any hostNetwork pod or local user on the node therefore gets full VM control.
- Ship k8s defaults with `auth.require = true`.

**mTLS identity headers may allow privilege escalation (uncertain).** `docs/auth-oidc-mtls.md` honors `X-Client-Cert-Role` and `X-Client-Cert-Tenant` whenever `tls.client_ca` is set. If FluxVM terminates mTLS itself, any client holding a valid cert could self-assert `admin` or another tenant. Verify that these headers are accepted only from a configured trusted proxy.

**AppArmor profile is broad (`deploy/apparmor/README.md`).**
- `owner /** rw`.
- `Ux` (unconfined) exec for `lsof`/`fuser`.
- `sys_module`, `net_raw`, and raw packet sockets.
- This is close to unconfined for a root daemon.

**Privileged DaemonSet.** It runs with `SYS_ADMIN` (per `deploy/k8s/README.md`). The same doc's prerequisites mention only `SYS_RESOURCE`, so the documented privilege set is inconsistent.

**Egress TLS interception.** It writes a CA key to `state_dir`. Any compromise of that host directory lets an attacker MITM all intercepted sandbox egress. Document key rotation and protection.

**Unauthenticated model relay.** The local-model relay in `examples/agent-sandbox-demo` has no auth (the docs say so).

**Lab infrastructure exposed in docs.** Public IPs of lab hosts appear in `docs/benchmarks/README.md` (`80.79.5.173`) and `docs/secure-containers-15min-lab.md` (`175.110.122.71`).

**`curl | bash` installer.** It is promoted in the READMEs. The SHA check verifies the package, not the script itself.

**Static test needs root on CI.** `sentinel-fleet-drift-slo.yml` runs a "static" test with `sudo -E`, and the script itself installs into `/usr/libexec`.

## Test and CI gaps

**Many "tests" are grep or string-presence checks, not behavior tests.** Examples:
- `scripts/test-hubble-ui.sh`, `scripts/test-four-tracks.py`, `scripts/test-project-production.py`.
- `scripts/test-fluxmesh*.sh`.
- `crates/fluxvm-apple/tests/vz27_*source_contract.rs`.

**A tautological test.** `scripts/test-production-dataplane.py::test_fqdn_wildcard_skipped` tests a list comprehension written inside the test, not product code.

**Handoff work merged without compiling.** `TEST_REPORT.md` and `README-FIRST.md` state that Set 5 was never compiled or run. Nothing visible in CI requires a KVM/containerd E2E before merge. The KVM, Secure Containers and fleet lab jobs are all opt-in through `vars.*` on self-hosted runners.

**SDKs never run against a live daemon by default.** Python, Go and TS SDKs and the MCP tools are tested only against mocks. Live tests exist but are not in CI.

**Dependabot covers only `cargo` and `github-actions`.** It misses the Go modules (`go/`, `controllers/`, `tools/`, `terraform-provider-fluxvm/`) and npm (`website/`, `typescript/`).

**`secure-containers-set18.yml` hygiene.**
- It has no `permissions:` block.
- It triggers on pushes to every branch.

**Uncertain whether CI builds without the sibling checkout.** `ci.yml` is not shown. `host-gpu.yml` checks out guestkit separately, but it is unclear whether `cargo build --workspace` runs on every PR.

## Documentation gaps

**Version drift.**

| Source | Version stated |
|---|---|
| `README.md` | current release 0.4.6 |
| `docs/agent-platform/README.md` | newest release 0.4.7 |
| Crates | 0.4.8 |
| `CONTRIBUTING.md` | changelog under "0.4.0 (unreleased)" |
| Client deck | v0.3.0 |
| `FEATURES.md` | "23 crates" (workspace lists 33) |

**Naming drift.**
- Repo: `zyvorai/fluxvm` vs `zyvorai/zyvor-fluxvm`.
- CLI: `fluxvm` vs `fluxctl` (e.g. `fluxvm cnp apply` vs `fluxctl cnp apply`).
- `FEATURES.md` says the CLI is `fluxvm`.

**SDK READMEs contradict themselves or the daemon.**
- `python/README.md` says the change-set routes are "not wrapped", while its own API table documents `baseline`/`changes`.
- All three SDK examples use `127.0.0.1:8080`; the daemon default is `7788`.

**Stale or duplicate root artifacts.** `PR_BODY.md` and `PR_BODY_SET5.md` are identical. `PR_BODY_SET3.md`, `README-FIRST.md` and `TEST_REPORT.md` refer to `patches/0005-...`. These belong in PRs or the changelog, not the repo root.

**Duplicate config keys.** `config.example.toml` lists `max_vcpus`/`max_memory_mib` twice in the commented `[policy]` block.

**No consolidated support matrix.** Status and verification claims are scattered across many READMEs. There is no single, dated matrix of what is GA, what is lab-only (e.g. `fluxvm_engine=kvm`, per `SECURITY.md`) and what is not yet run.

## Top 5 recommended actions (ordered)

1. **Fix the k8s deploy hazards.**
   - Move `secret.yaml` and `servicemonitor.yaml` out of `deploy/k8s/microvm/`, or stop documenting `kubectl apply -f microvm/`.
   - Fix `namespace: fluxctl` and `commonLabels` in `deploy/k8s/gitops/kustomization.yaml`.
   - Enable auth in the shipped ConfigMap.
2. **Make the "static gates" actually fail.** Replace bare `! cmd` with `if cmd; then exit 1; fi` (or `|| exit 1`), and stop benchmark scrip
