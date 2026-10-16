# FluxVM for AI agents

Give an agent a disposable, hardware-isolated machine it can break, with a handle to checkpoint it, roll it back,
see what it changed and read what it did. FluxVM runs the machine (Apple Virtualization.framework on a Mac; KVM,
Firecracker, Cloud Hypervisor and QEMU on Linux) and an **agent session** wraps it with a workspace, a network policy
and a ledger. On a Mac, `--host-gpu` / `runtime.host_gpu` exposes the host Metal/MLX OpenAI endpoint inside an offline
guest ([host-inference.md](../host-inference.md)).

```text
Agent (Claude Code, Codex, Gemini CLI, OpenCode, your own)
   │   MCP · REST · Go · Python · fluxctl
   ▼
Agent session ── workspace · policy · checkpoints · ledger
   │
   ▼
Sandbox ── snapshot · restore · fork (KVM) · speculate → changeset → approve/apply
   │
   ▼
vz · KVM · Firecracker · Cloud Hypervisor · QEMU
```

> **Honest status.** On an Apple M4 the session layer was run live for create, exec, memory checkpoint and restore, fork
> into diverging children, workspace diff and delete. It is **not** sub-second (create is seconds), and a session on the
> Mac's NAT **cannot fork** (offline or allow-listed sessions can). Since then the layer gained filesystem confinement
> (Landlock + seccomp, Linux guests), scoped session tokens, a guest job supervisor, per-action approvals, per-session HTTP
> method/path rules, budgets, resumable file transfer and fork-evaluate-promote. Those are **implemented and unit/router
> tested, not yet field-tested on a live guest**; the practical guide ends with the exact list.
> Start with [agent-sessions-guide.md](../agent-sessions-guide.md). The table at the end lists every claim and how it was
> checked. Compared with microsandbox: [vs-microsandbox.md](../vs-microsandbox.md).

## Install

macOS (Apple silicon, macOS 14 or newer) and Linux (x86_64):

```bash
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash                      # download, verify, install
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --service --client claude-code
fluxctl --version && curl -s http://127.0.0.1:7788/healthz       # {"ok":true} once the daemon runs
```

The installer needs no sudo or Homebrew, verifies the package's SHA-256, and is reversible (`--uninstall`); the options and
what it changes are in [install.md](../install.md). Homebrew (`brew install zyvorai/tap/fluxvm`) is the alternative on
a Mac.

**Which release has sessions.** The newest published release at the time of writing, 0.4.7, has sandboxes, snapshots,
speculative execution and MCP, but **not** the session API or `fluxctl agent`. Check with `fluxctl agent --help`
(`unrecognized subcommand` means an older release). Until the next release is published, build them from source:

```bash
git clone https://github.com/zyvorai/zyvor-fluxvm && cd zyvor-fluxvm
xcode-select --install && brew install hivex
cargo build -p fluxctl                                   # needs the sibling guestkit checkout, see README
FLUXVM_VZ_RUNNER=/opt/homebrew/opt/fluxvm/bin/fluxvm-vz-runner target/debug/fluxctl serve
```

Linux hosts get a package from the same installer once a release carries one (the release workflow builds
`linux-x86_64`); otherwise build from source (`cargo build --release -p fluxctl`).
Full macOS notes: [install-macos.md](../install-macos.md).

## 60 seconds

```bash
# a session: a sandbox, a network policy, a workspace directory
fluxctl agent run claude-code --policy developer-safe --ttl-seconds 3600     # prints the session, note its id

fluxctl agent exec <id> -- 'uname -a && mkdir -p /workspace/demo && echo hi > /workspace/demo/a.txt'
fluxctl agent checkpoint <id> --tag before-change
fluxctl agent exec <id> -- 'echo changed > /workspace/demo/a.txt'
fluxctl agent artifacts <id>                  # what changed in /workspace since the session started
fluxctl agent restore <id> before-change      # memory, disk: back to the checkpoint
fluxctl agent timeline <id>                   # what was done, in order
fluxctl agent rm <id>
```

Measured on an M4 mini (debug build, 5 runs): create 4.6-6.8 s, exec 0.1-0.4 s, checkpoint 0.7 s, restore 4 s, fork into 2
running children 2.5 s. Details and caveats: [benchmarks/agent-session.md](../benchmarks/agent-session.md).

## Wire it to your agent (MCP)

```bash
fluxctl mcp install claude-code --allow-write      # also: codex, gemini, cursor, vscode, ... see `fluxctl mcp --help`
fluxctl mcp status
```

The agent then has the `session_*` tools (create, exec, jobs, files, git, checkpoint, restore, fork, rollout, evaluate,
promote, grants, tokens, approvals, probe, events, artifacts, delete) and the review loop tools `sandbox_speculate`,
`sandbox_changesets`, `sandbox_changeset_decide`. Write tools exist only with `--allow-write`. For an agent you do not
fully trust, give the MCP server a scoped session token, not the admin one. The full table is in
[mcp.md](../mcp.md#agent-session-tools).

## From code

```python
from fluxvm import FluxVM

fx = FluxVM("http://127.0.0.1:7788")
with fx.create_session("codex", policy="developer-safe", ttl_seconds=900) as s:
    r = s.exec("python3 -c 'print(6 * 7)'")
    print(r.exit_code, r.stdout)
    s.checkpoint("clean")
    s.exec("rm -rf /workspace/*")
    s.restore("clean")
    print(s.artifacts()["changes"])
```

```go
c := fluxvm.NewClient("http://127.0.0.1:7788")
s, err := c.CreateSession(ctx, fluxvm.CreateSessionRequest{Agent: "codex", Policy: &fluxvm.SessionPolicyRequest{Preset: "offline"}})
defer s.Delete(ctx)
res, _ := s.Exec(ctx, "uname -a", 30)
```

## Try several fixes, keep the one that passes

Two ways, depending on the backend:

- **Any backend, one sandbox**: `sandbox_speculate` runs a command in an isolated copy and returns a **changeset**
  (added/modified/deleted files with contents). Review it, then `sandbox_changeset_decide` with `approve` and `apply`.
  Apply refuses with a conflict if the files changed since.
- **A forkable session** (offline or allow-listed on a Mac, or in-tree KVM on Linux): `fluxctl agent fork <id> --count 4`
  creates children that start from the parent's exact state and diverge; run a candidate in each, keep the winner,
  delete the rest. [examples/agent-sandbox-demo](https://github.com/zyvorai/zyvor-fluxvm/blob/main/examples/agent-sandbox-demo/README.md) does this at the sandbox
  level (4 copies in about 4 s on an M4 mini). A session on the Mac's NAT answers `409` with the reason and creates
  nothing. `fluxctl agent evaluate` forks one child per candidate, tests and ranks them and keeps the winner; `fluxctl
  agent promote <child> --dry-run` then shows a three-way merge of its workspace changes into the parent, which you apply
  with the `plan_id`.

## Policy

```bash
fluxctl agent run codex --policy offline                       # no network card at all
fluxctl agent run codex --policy developer-safe                # GitHub, crates.io, PyPI, npm, Go proxy
fluxctl agent run codex --allow api.example.com --allow '*.example.com'
```

Host allow-lists are enforced by the host (a proxy on ports 80/443; the guest has no other route). Beyond that:

- **Method and path rules** (`{host, methods, paths}` entries) are enforced per session by the host's egress proxy where
  the sandbox's traffic reaches it (QEMU/KVM with the proxy enabled); a session that cannot be filtered is refused at
  create unless you choose `best-effort`. Not on `vz`.
- **Filesystem rules** are enforced inside Linux guests with Landlock + seccomp (`--confine`, `--fs-read`, `--fs-write`).
- **Process rules** (`allow`, `deny`, `ask`) are a guardrail on the command string, **not** a security boundary; `ask`
  holds a command for an operator's approval.

How to choose and verify each: [agent-sessions-guide.md](../agent-sessions-guide.md#3-choosing-a-policy). Reference:
[agent-sessions.md](../agent-sessions.md#policy).

## What was checked, and how

| Claim | Status |
|---|---|
| create / exec / delete on `vz` | **Run** live (M4, macOS 27.2, stock `debian-13`) |
| memory checkpoint, restore reverts a file | **Run** live |
| disk-only restore | **Run**: it cold-boots, and exec returned 255 until the guest had finished booting; do not rely on it for quick rollback |
| workspace baseline and artifacts diff | **Run** live (an added file reported) |
| git clone into the workspace | **Run**: failed as designed on the stock image (no `git`, reported in `workspace.note`); success path not run |
| network allow-list | **Run** live through a session (`--allow example.com`): that host answered 200 over the proxy, `example.org` was refused, a direct connection to an IP had no route |
| tenant isolation (404 across tenants) | router tests, not a multi-tenant daemon |
| fork | **Run** live on `vz`: an offline session into 2 children, which inherited the parent's state and diverged; `409` for a NAT session. The in-tree KVM fork is **Not run** on hardware |
| MCP tools, Go and Python SDKs | mock-server tests; the CLI path is the one run live |
| sub-second start | **Not met** (see the benchmark); a goal |

## Where to go next

- [agent-sessions.md](../agent-sessions.md): the API, backends, policy compiler, roadmap.
- [macos-sandboxes.md](../macos-sandboxes.md), [agent-density.md](../agent-density.md): sandboxes on a Mac, warm pool, density.
- [credential-broker.md](../credential-broker.md), [http-acl.md](../http-acl.md), [procbox.md](../procbox.md): secrets, egress rules, rootless sandbox.
- [vs-microsandbox.md](../vs-microsandbox.md): how this compares, including where FluxVM is behind.
