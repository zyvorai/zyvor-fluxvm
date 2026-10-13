# FluxVM and microsandbox

[microsandbox](https://github.com/superradcompany/microsandbox) is an Apache-2.0 microVM runtime from
superradcompany, built on libkrun, with a daemonless embeddable SDK. Both aim at running untrusted code and AI-agent
workloads in hardware-isolated VMs on a Mac or a Linux box. This page compares what each *documents*. It is not a
benchmark: nobody here ran microsandbox, and every FluxVM cell links to its doc or says what was run.

Legend for FluxVM: **Run** = exercised live on real hardware; **Tests** = unit or router tests only;
**Not run** = built but not verified on hardware; **No** = not available; **Behind** = microsandbox has it and
FluxVM does not.

Sources for microsandbox: its README and release notes (v0.7.x, October 2026; the README says the project is beta).

| Capability | microsandbox (as documented) | FluxVM | Status |
|---|---|---|---|
| Hardware isolation per sandbox | microVMs via libkrun | `vz` on Apple Silicon; KVM, Firecracker, Cloud Hypervisor, QEMU on Linux | **Run** on `vz` ([macos-sandboxes.md](macos-sandboxes.md)); Linux backends **Not run** here |
| Startup | "average guest boot under 100 ms" (M1, their measurement) | create 6.8 s median, first run 26.9 s, on an M4 debug build ([benchmarks/agent-session.md](benchmarks/agent-session.md)) | **Behind**; sub-second is a goal ([agent-sessions.md](agent-sessions.md)) |
| Embeddable, no daemon | yes: the SDK spawns the VM as a child process | no: a daemon (`fluxctl serve`) is required | **Behind** |
| Platforms | macOS (Apple Silicon), Linux (KVM), Windows (WHP) | macOS (Apple Silicon) and Linux hosts; no Windows host | **Behind** (Windows) |
| OCI images | yes, any registry | container sandboxes on `vz` ([oci-sandboxes.md](oci-sandboxes.md)) | **Tests** (live test not yet run on hardware, per that doc) |
| Fork a running sandbox | yes | `vz` Linux guests with no NIC of their own (offline or allow-listed) and the in-tree KVM engine ([fork-and-restore.md](fork-and-restore.md), [macos-sandboxes.md](macos-sandboxes.md)); a NAT sandbox cannot be forked; session fork returns `409` with the reason | **Run** on `vz` (2 children in 2.5 s, they diverged); KVM **Not run** |
| Snapshots and restore | full snapshots, `--cow-mem`, generations, branches, archive import | memory+disk and disk-only snapshots, restore; checkpoints per session | **Run** on `vz` (memory restore reverted a file change) |
| Speculative execution with review | not documented | `speculate` -> changeset -> approve/reject -> apply, conflict-checked ([speculative-execution.md](speculative-execution.md)) | **Tests**; not verified on hardware per that doc |
| Network allow-list | host allow-list in config; CONNECT proxy; 403 denial bodies; published ports | host allow-list via the egress proxy on `vz` ([macos-sandboxes.md](macos-sandboxes.md)); eBPF fabric with CIDR/port/DNS/rate rules on Linux ([network-fabric.md](network-fabric.md)); HTTP method/path ACL ([http-acl.md](http-acl.md)) | allow-list **Run** on `vz` (through a session: allowed host 200, other host refused, direct connection no route); eBPF fabric **Not run** on a real host in this repo |
| Secrets that never enter the VM | yes: host-scoped placeholders, TLS interception, header-scoped | credential broker: per-sandbox header grants injected by the egress proxy ([credential-broker.md](credential-broker.md)) | **Tests**; "not yet verified on hardware" per that doc |
| Rootless process sandbox | not documented | `fluxvm-procbox` Landlock + seccomp with a `learn` mode ([procbox.md](procbox.md)) | **Tests** (Linux) |
| Volumes | yes | named volumes on QEMU templates and container sandboxes; agent sessions on `vz` attach a named raw-disk volume (`--volume`, one VM at a time, no fork). Plain `vz` VM sandboxes do not take `volumes` ([macos-sandboxes.md](macos-sandboxes.md)) | partial |
| GPUs | not mentioned | VFIO GPU sandboxes on QEMU ([sandbox-gpus.md](sandbox-gpus.md)) | **Not run** on real GPUs |
| Config file for a sandbox | YAML with scripts | JSON create specs; session policy presets | **Behind** (no YAML/script format) |
| Install a sandbox as a command | `msb install` | no | **Behind** |
| SDK languages | TypeScript, Rust, Python, Go, Ruby | Go, Python and TypeScript SDKs for the sandbox API (session methods: Go and Python only), plus the REST API and MCP | **Behind** |
| MCP server | yes, plus Agent Skills | `fluxctl mcp serve` with read/write gating, `mcp install` for 7 clients, session and changeset tools ([mcp.md](mcp.md)) | **Tests**; client install steps in [install-macos.md](install-macos.md) |
| Agent session abstraction | not documented as one object | `/v1/agents/sessions`: workspace + policy + checkpoints + ledger ([agent-sessions.md](agent-sessions.md)) | **Run** on `vz` (MVP) |
| Detached exec jobs, metrics | yes (`msb metrics`) | `/metrics`, `netobs`, per-VM stats; no detached exec jobs | partial |
| Fleet / many hosts | not documented | central, tenants, leases, FluxMesh ([fleet-quickstart.md](fleet-quickstart.md)) | simulator **Run**; real multi-Mac fleet **Not run** |
| Guides | Docker-in-sandbox, Playwright, Browser Use, GitHub Actions runner | Agent demo with fork and parallel fixes ([examples/agent-sandbox-demo](https://github.com/zyvorai/zyvor-fluxvm/blob/main/examples/agent-sandbox-demo/README.md)); GitHub runner pool via Elvora | different set |
| Maturity | beta, "expect breaking changes" | Maturity tiers in the README; MVP for sessions | both early |

## Where FluxVM is behind, plainly

- **Startup time and embeddability.** microsandbox boots in under 100 ms by its own measurement and needs no daemon.
  FluxVM's `vz` sandbox takes seconds and needs `fluxctl serve`.
- **Developer ergonomics around the edges**: YAML sandbox files with scripts, `install` as a command, five SDK languages.
- **Windows hosts.**

## Where FluxVM is different

- **One control plane over several VMMs** (`vz`, KVM, Firecracker, Cloud Hypervisor, QEMU), with a fleet layer, tenants,
  quotas and a Mac cloud, rather than one embedded runtime.
- **Review before apply**: speculative runs end as a change set a person or agent approves.
- **Network policy depth on Linux**: eBPF dataplane, DNS/SNI/CIDR/port rules and flow observability.

Those are design differences, not a claim of being faster or safer. Where a column says **Not run**, the code exists
and the repo's own docs say it has not been verified on hardware.
