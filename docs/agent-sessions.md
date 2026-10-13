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

### Git

```bash
fluxctl agent git <session> status
fluxctl agent git <session> diff [--staged] [path]      # unified diff on stdout
fluxctl agent git <session> commit "message"            # stages everything unless --staged-only
fluxctl agent git <session> branch agent/try-1          # create and switch
fluxctl agent git <session> log -n 5
```

REST: `GET .../git/status|diff|log`, `POST .../git/commit|branch`; MCP: one tool, `session_git` with `op`
(`status|diff|log|commit|branch`; it needs `--allow-write` because it can commit). The repository is the session's
workspace path unless `repo` is given. Each call runs `git` in the guest through the ordinary exec path.

- **The guest needs git.** The stock `debian-13` has none: a call answers `409` and says so. A session with network
  access can install it (`sudo -n apt-get install -y git` ran live: git 2.47.3); an `offline` session cannot, so for
  offline work use an image that already has git.
- Commits use the repo's `user.name`/`user.email` when set, else `FluxVM Agent <agent@fluxvm.local>`, with signing off.
  The message goes to the ledger as `git.commit` with the commit id.
- Branch names and diff paths that could be read as options or climb out of the repository are refused (400).
  A diff is cut at 256 KiB (`truncated: true`).

Run live (M4, `vz`, NAT session with git installed through apt): status on a fresh repo, commit, "nothing to commit"
(409), edit then diff of one path, branch, second commit, log, "branch already exists" (409), a missing repo directory
(404), and on an offline session without git the 409 above. **Not built:** push/pull/fetch (a push is a network and
credential action that belongs behind an approval), merge and rebase, checkout of an existing branch, stash.

### Files

```bash
fluxctl agent put   <session> ./local.txt /workspace/dir/file.txt --mode 755   # parents are created
fluxctl agent files <session> /workspace                                        # list (or stat a file)
fluxctl agent cat   <session> /workspace/dir/file.txt
fluxctl agent pull  <session> /workspace/out.bin ./out.bin
fluxctl agent rm-file <session> /workspace/dir -r
```

| Route | |
|---|---|
| `GET /v1/agents/sessions/{id}/files?path=` | list a directory (`kind: dir`) or stat a file (`kind: file`): name, type, size, mtime, mode |
| `GET /v1/agents/sessions/{id}/files/content?path=` | `content_base64`, `mode` |
| `PUT /v1/agents/sessions/{id}/files/content` `{path, content_base64, mode?, parents?}` | write; parents are created by default |
| `DELETE /v1/agents/sessions/{id}/files?path=&recursive=` | remove; `/` and one-component paths are refused |

MCP: `session_files`, `session_file_read`, `session_file_write`. Paths must be absolute, without `..`, newline or NUL
(400 otherwise); another tenant's session is a 404; writes and removals go to the ledger as `file.write` and
`file.delete` (path and size, never content).

Run live (M4, `vz`, NAT session): a file with spaces in its name written into new nested directories, listed, cat'd and
removed; 1 MiB written and read back byte-identical (about 0.12 s each way); a missing path 404, `..` and `/etc` 400.
Writing under a directory that did not exist first created it with `sudo` and left it root-owned, which blocked later
writes; the daemon now hands the directories it creates to the guest user (found by this run, fixed, re-run).
**Limits:** a single write is capped by the daemon's request-body limit, about 1.5 MiB of file (a 4 MiB write was
reset by the connection, not answered with a clean 413); there is no chunked upload yet. Reads are whole files. List
entries come from GNU `find`, so a guest without it (busybox) is not supported.

### Background jobs

A command that outlives the request, with output you read incrementally:

```bash
J=$(fluxctl agent bg <session> -- 'npm run dev' | jq -r .job)
fluxctl agent logs <session> $J -f      # follows until exit, exits with the job's exit code
fluxctl agent kill <session> $J         # SIGTERM to its process group; exit code reads 143
```

`POST .../jobs` starts it (`setsid`, output to `/tmp/fluxvm-jobs/<job>.out`, exit code written last);
`GET .../jobs/{job}?offset=N` returns `state`, `exit_code`, `output`, `next_offset` and `more`; `DELETE .../jobs/{job}`
stops it. MCP: `session_job_start`, `session_job_read`, `session_job_kill`.

Run live (M4, `vz`, NAT session): a 4-second job followed to its exit code 7; a `sleep 300` job killed (143); a
200 000-byte output read back whole across several pages; an unknown job id returned 404. **This is polling over the
ordinary exec path, not a stream**: each read costs one exec (0.1 s on a NAT session, about 0.4 s offline), `-f` polls
twice a second, and there is no stdin or TTY. A job lives in the guest's `/tmp`, so it is gone after a disk-only
restore or a reboot, and output is not rotated.

### Preview URLs

A session can hand out an expiring URL that reaches **one TCP port** of its guest, for a dev server or a web app an
agent started:

```bash
fluxctl agent expose <session> 8000 --ttl 600      # -> {"url": "http://127.0.0.1:7788/preview/<64 hex>/", ...}
fluxctl agent previews <session>
fluxctl agent unexpose <session> <token>
```

| Route | |
|---|---|
| `POST /v1/agents/sessions/{id}/preview` `{port, ttl_seconds}` | mint (1-86400 s, default 3600); admin, tenant-scoped |
| `GET  /v1/agents/sessions/{id}/previews` | live ones |
| `DELETE /v1/agents/sessions/{id}/preview/{token}` | revoke |
| `ANY  /preview/{token}/{*path}` | the preview; **no bearer token**, the URL is the credential |

The token is 256 random bits stored under `<state_dir>/agent-previews/`. The guest sees the path with the
`/preview/{token}` prefix removed. A token stops working when it expires, is revoked, or its session is deleted, and
all three look the same (404). The token never enters the ledger. Responses carry `Referrer-Policy: no-referrer` and
`Cache-Control: no-store`.

What was run: on this M4 (`vz`, NAT session, `python3 -m http.server` in the guest) the URL returned the page and a
sub-path, a made-up token and a revoked token returned 404; router tests cover expiry, revocation, tenant scoping and
the ledger. **Not run:** Linux (the route reuses the netns proxy there), WebSocket, streaming or large bodies, and
HTTPS (the URL is as private as the daemon's own address: put it behind TLS yourself). An **offline** session has no
network address, so it has nothing to preview.

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
| Streaming PTY / stdin for a session | background **jobs** (above) cover long-running commands with polled output; there is still no stdin, no TTY and no push stream; the `console` route is VM-level and admin-only |
| Replay | the ledger is a log, not a recording |
| Preview URLs: WebSocket upgrades, large or streamed responses | the preview route is plain HTTP through the existing sandbox proxy; WebSocket and streaming bodies were not run through it (`/v1/sandboxes/{id}/ws/{port}/*` is separate and auth-gated) |
| Browser / computer-use profile | vz display and input exist; no CDP/Playwright profile |
| Secretless credentials | the credential broker (`docs/credential-broker.md`) is per sandbox; not bound to sessions |
