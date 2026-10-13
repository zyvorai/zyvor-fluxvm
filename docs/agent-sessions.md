# Agent sessions

An **agent session** is one handle for what an AI agent needs from FluxVM: a sandbox, a workspace, a network
policy, checkpoints and a ledger of what was done. It is a thin layer over the existing sandbox, snapshot, fork,
change-set and event primitives. It adds no new isolation and no new hypervisor; everything an agent could already
do with `/v1/sandboxes` and `/v1/vms` it can now do through one id.

> Status: **MVP.** Run live on an Apple M4 (macOS 27.2, `vz`, stock `debian-13`): create, exec, memory checkpoint and
> restore, fork of an offline session into two diverging children, workspace baseline and artifacts, delete. Not run
> live: the in-tree KVM fork (Linux), a successful git clone (the stock image has no `git`), tenant tokens against a real
> multi-tenant daemon. Unit and router-level tests cover the rest. See [Measured](#measured) and [Roadmap](#roadmap).

## API

All routes need an `admin` token and are tenant-scoped (a tenant token only ever sees its own sessions; another
tenant's id answers `404`, not `403`).

| Route | What it does |
|---|---|
| `POST /v1/agents/sessions` | Create: sandbox + workspace + compiled policy. `201` with the session record. |
| `GET /v1/agents/sessions` | List the caller's sessions, oldest first. |
| `GET /v1/agents/sessions/{id}` | One session: backend, capabilities, policy, workspace, checkpoints, children. |
| `DELETE /v1/agents/sessions/{id}` | Delete the session and its sandbox. Idempotent (`204`). |
| `POST /v1/agents/sessions/{id}/exec` | `{command, timeout_seconds?}` through the sandbox's guest path. Recorded in the ledger. |
| `POST /v1/agents/sessions/{id}/checkpoint` | `{tag?, memory?}`; `memory:false` is disk-only. |
| `POST /v1/agents/sessions/{id}/restore` | `{tag}`; only tags this session took. |
| `POST /v1/agents/sessions/{id}/fork` | `{count, identity?}` (1-32) into child sessions that start from the current state. `identity` is `best-effort` (default), `unique` or `preserve`. **`409` with the reason when the session cannot fork; nothing is created.** |
| `GET /v1/agents/sessions/{id}/events` | `?since=` RFC 3339. `{ledger, vm_events}`. |
| `GET /v1/agents/sessions/{id}/artifacts` | Workspace changes since the baseline, checkpoints, children, counters. |

### Create

```json
{
  "agent": "claude-code",
  "workspace": {"git": "https://github.com/octocat/Hello-World", "path": "/workspace", "mode": "clone"},
  "runtime": {"image": "agent-micro", "cpus": 2, "memory_mb": 2048},
  "policy": {"preset": "developer-safe", "network": {"default": "deny", "allow": ["example.com"]}},
  "ttl_seconds": 3600
}
```

- `workspace.git` must be an `https://` URL. The workspace directory is always created (with `sudo` when the guest user
  is not root), then cloned into when `git` is given. A failed clone does not fail the session: the record says why in
  `workspace.note` (for example *git is not installed in the guest image*; the stock `debian-13` has none).
- A baseline of the workspace is recorded at the end of create, which is what `artifacts` diffs against.
- `runtime.image` follows the sandbox rules (`agent-micro` gives the vsock agent; another image cold-boots).

### Policy

A policy is compiled into fields the sandbox already enforces. The response reports exactly what was compiled.

| Input | Compiles to | Enforced by |
|---|---|---|
| `preset: "offline"` | `offline: true` (no network card) | vz: the VM has no NIC |
| `preset: "developer-safe"` | `allow_hosts`: `github.com`, `*.github.com`, `*.githubusercontent.com`, `crates.io`, `*.crates.io`, `static.crates.io`, `pypi.org`, `files.pythonhosted.org`, `registry.npmjs.org`, `proxy.golang.org`, `sum.golang.org` | vz: the host egress proxy (ports 80 and 443) |
| `preset: "open"` or no policy | nothing | n/a |
| `network.default: "deny"` + `allow: [host…]` | `allow_hosts` (merged, de-duplicated); `offline` when nothing is allowed | as above |
| `network.allow` entries with `methods` or `paths` | **refused (`400`)** | not supported yet; HTTP method/path rules exist for the QEMU/KVM egress proxy, not through this API |
| `filesystem`, `process` | stored, listed under `policy.unenforced` | **nothing enforces them yet** |

### Capabilities by backend

| Backend | snapshot / restore | fork | PTY |
|---|---|---|---|
| `vz` (macOS), offline or allow-listed session | yes (memory restore verified live; a disk-only restore cold-boots) | **yes** (run live: 2 children, 2.5 s) | no (VM-level `console` route is admin-only) |
| `vz`, session on the Mac's NAT (policy `open`) | yes | **no**: children would share one MAC and address | no |
| `flux-vm` (in-tree KVM, Linux) | yes | yes (not verified on hardware) | no |
| `qemu`, `firecracker` | yes | no | no |
| `cloud-hypervisor` | not confirmed | no | no |

`GET /v1/agents/sessions/{id}` reports `capabilities` for the session's backend so a client can decide without trying.

### Ledger

An append-only JSON-lines file per session records the API-level actions taken *through the session*: `session.create`,
`workspace.clone`, `exec` (command, exit code, duration), `checkpoint`, `restore`, `fork`, `session.delete`. It is not
a recording of the guest: commands an agent ran inside a shell it opened are one `exec` entry. Files live in
`<state_dir>/agent-sessions/<id>.json` and `<id>.ledger.jsonl`, so sessions survive a daemon restart; a sandbox that
expired by TTL leaves the session record behind until `DELETE`.

## Clients

```bash
fluxctl agent run claude-code --policy developer-safe --ttl-seconds 3600
fluxctl agent exec <id> -- 'cd /workspace && ls'          # one string = a shell line; several words = an argv
fluxctl agent checkpoint <id> --tag before-refactor
fluxctl agent restore <id> before-refactor
fluxctl agent timeline <id>
fluxctl agent artifacts <id>
fluxctl agent rm <id>
```

- **MCP** (`fluxctl mcp serve --allow-write`): `session_create`, `session_exec`, `session_checkpoint`, `session_restore`,
  `session_fork`, `session_events`, `session_artifacts`, `session_delete`, plus `sandbox_speculate`, `sandbox_changesets`
  and `sandbox_changeset_decide`.
- **Go**: `client.CreateSession(ctx, fluxvm.CreateSessionRequest{…})`, then `Exec`, `Checkpoint`, `Restore`, `Fork`,
  `Events`, `Artifacts`, `Delete` (`go/sessions.go`).
- **Python**: `FluxVM(url).create_session("codex", policy="developer-safe")`, a context manager that deletes on exit
  (`python/fluxvm/sessions.py`).

## Measured

`scripts/bench-agent-session.sh`, results in [benchmarks/agent-session.md](benchmarks/agent-session.md): on an M4 Mac
mini (debug build, 5 runs): on a NAT session create 6.8 s median (26.9 s on a cold first run), first exec 0.12 s, memory
checkpoint 0.7 s, restore 4.1 s; on an offline session create 4.6 s, exec 0.39 s, fork into 2 children 2.5 s. **These are
not sub-second.** Targets (sandbox claim < 300 ms, warm restore < 150 ms, fork child < 100 ms) are goals that need lazy
memory restore and are **not met** on `vz` today.

## Roadmap

Each item says why it is not in the MVP.

| Item | Why not yet |
|---|---|
| Promote a child's change set into its parent | children are independent sandboxes; copying the winner's files back needs a cross-sandbox transfer that is not built. `speculate` + `changesets/{cs}/apply` (MCP: `sandbox_speculate`, `sandbox_changeset_decide`) cover the single-sandbox case |
| Lazy memory restore (sub-second claim/fork) | a restore engine change, not an API change; not started |
| Workspace overlay mode, read-only mounts | `mode: "overlay"` is refused; clone is the only writable mode |
| Filesystem and process policy enforcement | procbox has Landlock/seccomp; wiring it into VM sessions is not done |
| HTTP method/path rules through the session policy | the egress proxy supports them on QEMU/KVM; not exposed here |
| Streaming PTY for a session | the `console` route is VM-level and admin-only; no sandbox-scoped PTY |
| Replay | the ledger is a log, not a recording |
| Preview URLs | `/v1/sandboxes/{id}/http/{port}/*` exists (auth-gated); no signed public URL |
| Browser / computer-use profile | vz display and input exist; no CDP/Playwright profile |
| Secretless credentials | the credential broker (`docs/credential-broker.md`) is per sandbox; not bound to sessions |
