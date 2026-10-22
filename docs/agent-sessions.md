# Agent sessions

> **New here?** Start with the [practical guide](agent-sessions-guide.md): task-by-task walkthroughs with the commands to run,
> what you should see, and troubleshooting. This page is the reference.

An **agent session** is one handle for what an AI agent needs from FluxVM: a sandbox, a workspace, a network
policy, checkpoints and a ledger of what was done. It is a thin layer over the existing sandbox, snapshot, fork,
change-set and event primitives. It adds no new isolation and no new hypervisor; everything an agent could already
do with `/v1/sandboxes` and `/v1/vms` it can now do through one id.

> Status: **MVP.** Run live on an Apple M4 (macOS 27.2, `vz`, stock `debian-13`): create, exec, memory checkpoint and
> restore, fork of an offline session into two diverging children, workspace baseline and artifacts, delete. Not run
> live: the in-tree KVM fork (Linux), a successful git clone (the stock image has no `git`), tenant tokens against a real
> multi-tenant daemon. Unit and router-level tests cover the rest. See [Measured](#measured) and [Roadmap](#roadmap).

## API

All routes need an `admin` token (or a [session token](#session-tokens) for the operations it was minted with) and are tenant-scoped (a tenant token only ever sees its own sessions; another
tenant's id answers `404`, not `403`).

| Route | What it does |
|---|---|
| `POST /v1/agents/sessions` | Create: sandbox + workspace + compiled policy. `201` with the session record. |
| `GET /v1/agents/sessions` | List the caller's sessions, oldest first. |
| `GET /v1/agents/sessions/{id}` | One session: backend, capabilities, policy, workspace, checkpoints, children. |
| `DELETE /v1/agents/sessions/{id}` | Delete the session and its sandbox. Idempotent (`204`). |
| `POST /v1/agents/sessions/{id}/exec` | `{command, timeout_seconds?}` through the sandbox's guest path. Recorded in the ledger. |
| `POST /v1/agents/sessions/{id}/exec/stream` | `exec` with the output streamed as Server-Sent Events while the command runs: a `start` event (`{job}`), `output` and `stderr` events, then one `exit` event (`{exit_code, state}`). Same body as `exec`. It runs the command as a job, so the process policy, approvals, budget and confinement apply as for a job, and a dropped client can resume with `GET .../jobs/{job}/stream` and `Last-Event-ID`. A command held for approval answers `428` as JSON. `fluxctl agent exec --stream`. |
| `POST /v1/agents/sessions/{id}/tokens` | `{ops, ttl_seconds?, label?}`: mint a [session token](#session-tokens). `GET` lists live ones (no secrets); `DELETE .../tokens/{token}` revokes. Admin only. |
| `POST /v1/agents/sessions/{id}/uploads` | open a resumable [upload](#large-files-resumable-upload-and-ranged-download) of a large file; chunks go to `PUT .../uploads/{upload}?offset=N`, then `POST .../complete`. `GET .../files/range` reads a slice. |
| `GET /v1/agents/sessions/{id}/approvals` | the session's [approval](#approvals) requests. `POST .../approvals/{approval}/approve` and `.../deny` decide one (`{note?}`); admin only. |
| `POST /v1/agents/sessions/{id}/promote` | `{dry_run?, plan_id?, skip_conflicts?, paths?}` on a fork child: apply its workspace changes to its parent. See [Promoting a fork](#promoting-a-fork). Admin only. |
| `POST /v1/agents/sessions/{id}/checkpoint` | `{tag?, memory?}`; `memory:false` is disk-only. |
| `POST /v1/agents/sessions/{id}/restore` | `{tag}`; only tags this session took. |
| `GET /v1/agents/sessions/{id}/rewind` | The [rewind](#rewind-undo-the-last-action) config and its points (oldest first). `POST .../rewind/config` `{enabled?, max_points?, memory?, triggers?}`, `POST .../rewind/undo` `{steps?}` or `{to: tag}` and `POST .../rewind/pin` `{tag, pinned?}`, `POST .../rewind/branch` `{tag, count?, identity?}` and `POST .../rewind/bisect` `{test, good?, bad?, timeout_seconds?}` change and use it; admin only. |
| `POST /v1/agents/sessions/{id}/fork` | `{count, identity?}` (1-32) into child sessions that start from the current state. `identity` is `best-effort` (default), `unique` or `preserve`. **`409` with the reason when the session cannot fork; nothing is created.** |
| `GET /v1/agents/sessions/{id}/events` | `?since=` RFC 3339. `{ledger, vm_events}`. |
| `GET /v1/agents/sessions/{id}/events/stream` | **Server-Sent Events**, same data as `events` but live. `ledger` events carry a ledger entry, `vm` events a hypervisor event; history is replayed first (or only what is newer than `?since=`), then new entries follow about every 500 ms. The event id is a cursor, so `Last-Event-ID` resumes with no ledger entry repeated or missed (a VM event with the exact timestamp of the last one seen may repeat). An `error` event ends the stream once the session is deleted, expired or gone (its last entries, such as `session.delete`, come first) or after an hour. At most 64 streams per host. Needs `read`. |
| `GET /v1/agents/sessions/{id}/artifacts` | Workspace changes since the baseline, checkpoints, children, counters. |

`fluxctl agent rollout <session> --count N -- <cmd>` and MCP `session_rollout` are not a separate endpoint: they call
fork, then exec in every child in parallel, then delete the children (`--keep` keeps them), so they follow fork's rules.

### Create

```json
{
  "agent": "claude-code",
  "workspace": {"git": "https://github.com/octocat/Hello-World", "path": "/workspace", "mode": "clone"},
  "runtime": {"image": "agent-micro", "cpus": 2, "memory_mb": 2048, "host_gpu": false},
  "policy": {"preset": "developer-safe", "network": {"default": "deny", "allow": ["example.com"]}},
  "ttl_seconds": 3600
}
```

- `workspace.git` must be an `https://` URL. The workspace directory is always created (with `sudo` when the guest user
  is not root), then cloned into when `git` is given. A failed clone does not fail the session: the record says why in
  `workspace.note` (for example *git is not installed in the guest image*; the stock `debian-13` has none).
- A baseline of the workspace is recorded at the end of create, which is what `artifacts` diffs against.
- `workspace.mode: "readonly"` makes the workspace readable and not writable. It is enforced by the guest kernel
  (Landlock) for everything the agent runs (`exec`, jobs, git), so it turns on the default confinement even without a
  `filesystem` section, and create fails if the guest cannot enforce it (Linux guest with Landlock; set
  `filesystem.enforcement: "best-effort"` to create the session without it). The files routes (`files/content`
  PUT, `files` DELETE, `uploads`) refuse writes under the workspace with `403`. The clone at create time runs before the
  policy applies. `/tmp` and attached volumes stay writable. Unit-tested; not yet run on a real Linux guest.
- `runtime.image` follows the sandbox rules (`agent-micro` gives the vsock agent; another image cold-boots).
- `runtime.host_gpu` (`fluxctl agent run --host-gpu`): vz only; guest `OPENAI_BASE_URL` reaches the host Metal/MLX
  (or llama.cpp) server over vsock. See [host-inference.md](host-inference.md).
- `runtime.volumes: [{"name", "path"}]` (`fluxctl agent run --volume NAME:/mnt/data`) attaches a named host volume
  (`POST /v1/volumes`) as a virtio disk, formats it ext4 **only when `blkid` reports no file system**, mounts it at
  `path` (under `/mnt`, `/data`, `/srv`, `/opt`, `/workspace`, `/home` or `/root`), makes the mount point the guest
  user's and adds a `nofail` fstab line. vz only, at most 2. A volume is one disk: a second session naming an attached
  volume gets `409`, and **a session with a volume cannot fork**. Volumes are host-wide, so a tenant token is refused
  (`403`). Data survives deleting the session. Run live on vz: written in one session, read in the next; second attach
  `409`; fork `409`. A memory restore on a session with a volume brought the volume back to the checkpoint's content
  in that run, so treat a restore as rolling the volume back too. Not run: Linux (QEMU volumes are separate virtiofs
  directories), concurrent readers, volume resize while attached.

### Promoting a fork

Forks and `rollout` leave children independent. `promote` copies the winner's **workspace file changes** back onto the
parent, as a three-way merge.

1. **Fork** records the parent's workspace manifest as every child's baseline, so a child's `artifacts` (and a
   promotion) are exactly what it did since the fork. A child forked before this existed has no such baseline and
   cannot be promoted.
2. **Review**: `POST /v1/agents/sessions/{child}/promote {"dry_run": true}` returns `plan` and a `plan_id` digest:
   `apply` (added / modified / deleted, as the child changed them), `already_applied` (the parent already has the
   child's content), `conflicts` and the count of untouched files. A path is a conflict when the parent changed it too,
   so nothing the parent did is ever overwritten.
3. **Apply**: the same call without `dry_run`, optionally with `plan_id` (refused with `409` if the plan is different
   now) and `paths` (files or directories inside the workspace). Any conflict refuses the whole promotion with
   nothing changed, unless `skip_conflicts` applies just the clean paths.

Before the first write the parent gets a disk checkpoint, `pre-promote-<child>-<time>`, returned as `checkpoint`; if a
transfer fails midway the error says how many paths were applied and names that checkpoint, and restoring it undoes
them. Promotion refuses while commands run on either session, needs `sha256sum` in both guests (size and mtime cannot
compare two machines), and moves at most 2000 changes at once. Only regular files under the workspace are transferred;
file modes are kept, symlinks and ownership are not. Ledgered as `promote` on the parent and `promote.out` on the child.

Not run on a live guest: the plan is unit-tested and the route's refusals are router-tested, the transfer itself is not.

### Credential grants

The sandbox credential broker (`/v1/sandboxes/{id}/grants`) through the session, so an agent can call an authenticated
API without ever holding the secret: the egress proxy injects the `Authorization` header for the named hosts, for traffic
from this sandbox's address only.

```bash
fluxctl agent grant <session> --secret-ref gh-token --host api.github.com --ttl-seconds 1800
fluxctl agent grants <session>          # id, secret_ref, hosts, expiry, bound; never the value
fluxctl agent ungrant <session> [<grant id>]
```

- Admin only, and a session token cannot grant itself one. The secret is the request's own `value`, or the daemon's
  `FLUXVM_SECRET_<REF>`; the value is never ledgered or returned.
- **Bounded by the session**: a grant's expiry is capped at the session's, and deleting the session revokes them.
- **Not inherited**: a fork child starts with no grants. Grant each child you want to have one (a rollout of untrusted
  candidates therefore cannot use the parent's credentials by default).
- **Re-bound after restore**: a restored machine may come back on another guest address. `restore` re-binds the
  session's grants to the current one, so the old address stops matching at once; if the new address is already bound to
  a different sandbox's grants, this session's grants are **revoked** rather than left on a stale address
  (`grants_rebound` in the restore response, `grant.revoked` in the ledger). A grant with an unknown address matches
  nothing.
- **Method and path limits**: `--method GET` (repeatable) and `--path '/org/repo.git/*'` (repeatable, `*` matches any
  run) make the proxy inject the secret only for requests within them; a request outside still goes through, without
  the credential. A request path that is not canonical (`..`, `//`, `%`, backslash) never matches a limited grant, and
  an unknown method or path never does. Without limits a grant covers its hosts as before.
- The existing broker's limits (a TTL cap, a grant count per sandbox) apply. Not run live: the broker and the re-bind
  are unit-tested, not driven through the egress proxy from a guest.

### HTTP method and path rules

`network.allow` entries can limit what may be sent to a host:

```json
{"network": {"allow": [
  "pypi.org",
  {"host": "github.com", "methods": ["GET", "HEAD"], "paths": ["/org/*"]}
], "enforcement": "strict"}}
```

They compile to rules in the `docs/http-acl.md` grammar (`allow GET github.com/org/*`, and the same for subdomains), and
every other allowed host gets an `allow * host` rule, because the proxy denies whatever no allow rule matches. The
rules are installed for this sandbox in the host's egress proxy (`fluxvm_network::peer_policy`), keyed by its guest
address, and apply **in addition to** the host-wide `egress_http_rules`: a request must pass both, so a session can
only narrow what the host allows. `policy.http_rules` shows the compiled rules; `capabilities.http_policy` says
whether they are enforced.

- **Where it works**: backends whose traffic reaches the host's own egress proxy (`sandbox.egress_proxy_listen`
  set; QEMU/KVM and the in-tree engine) with a known guest address. **Not on `vz`** (its allow-list proxy is the
  runner's and matches host names only) and **not on procbox sandboxes**. Where it cannot be enforced, create returns
  `400` with the reason, unless `network.enforcement` is `best-effort` (the session is created with the host
  allow-list only and `capabilities.http_policy` is `false`, `policy.http_unenforced` in the ledger).
- HTTPS is judged per request only when the proxy intercepts TLS (`egress_tls_intercept`); without it a `CONNECT` is
  refused outright (`501`), so a limited host cannot be reached over TLS at all, which fails closed. A tunnel is refused
  up front when no rule could ever allow its host. Two `Host` headers are refused.
- **Restore and fork**: a restore re-binds the rules to the machine's current address; fork children get the rules
  installed for their own address. If an address is already bound to another sandbox's rules, the session is marked
  `http_unbound` and `exec`, jobs and terminals return `409` until it is recreated (`http_policy.unbound` in the
  ledger, `http_policy_bound: false` in the restore response), rather than running with a policy that is not applied.
  Deleting the session removes the rules.
- **Not verified live**: attribution relies on the proxy seeing the guest's own address, which holds when the
  sandbox's traffic is redirected to the proxy by the host network setup (transparent redirect or proxy settings). A
  guest with a direct route out bypasses the proxy, and the rules do not apply to it. The compile step, the rule store,
  and the proxy's per-request check are unit-tested; no guest traffic was driven through them.

### Budgets

`budget` at create limits what a session may consume; every field is optional.

```json
{"budget": {"max_execs": 200, "max_exec_seconds": 1800, "max_jobs": 10, "max_concurrent_jobs": 2, "max_children": 8}}
```

- `max_execs` commands through `exec`; `max_exec_seconds` the summed wall time of commands, and a command is also given
  at most the seconds left as its timeout; `max_jobs` started and `max_concurrent_jobs` running at once;
  `max_children` fork children (a rollout or `evaluate` spends it).
- Exceeding one is `403` with the limit and the amount used. Usage (`execs`, `exec_ms`, `jobs`, `children`) is returned
  as `usage` on `GET .../{id}`; it is kept in its own file, so counting never changes `revision`.
- A fork child starts with what is **left** of its parent's budget, so forking cannot multiply it. A fork that fails
  gives its children back.
- `max_exec_seconds` is checked when a command starts; commands running in parallel can overshoot it. Jobs are not
  counted against `max_exec_seconds`.
- `max_compute_seconds` is commands **plus** background jobs: finished jobs by their recorded start and end, running ones
  up to now. It is checked when a command or job starts, a command gets at most what is left, and a child starts with
  what is left. A running job is not stopped when the budget runs out (give the job its own `timeout_seconds`).
- `max_output_bytes` caps what one `exec` returns per stream (the rest is dropped on a character boundary, with
  `output_truncated` and `<stream>_dropped_bytes` in the answer and the ledger) and the bytes a job retains per stream.
  It never slows or kills the command.
- `max_workspace_bytes` is the workspace directory's size from `du -sk` in the guest, measured when a command or job
  starts (one extra command each time; the last value is `usage.workspace_bytes`). Growth during a run is not stopped;
  the *next* start is refused. It needs a workspace (`400` otherwise) and `du` in the guest.

Not done: token budgets (they need usage reports from the model client or gateway, which the daemon never sees) and a
process-count limit. `RLIMIT_NPROC` exists in the confinement policy, but the kernel does not apply it to root and
sessions run as root, so a `max_processes` knob would look enforced and not be; it waits for `run_as` support in
sessions.

### Evaluating candidates

`fluxctl agent evaluate <session> --candidate 'cmd A' --candidate 'cmd B' --test 'make test'` (MCP
`session_evaluate`) is "try several fixes in isolated machines, test each, review the winner, apply it":

1. forks one child per candidate (same forkability rules as `fork`, and `max_children` is spent);
2. in each child runs the candidate and then, if it exited 0, the test command, all children in parallel;
3. scores each: **100** passed (the test exited 0, or with no `--test` the candidate did), **10** ran cleanly but failed
   its test, **0** failed or never ran; ties go to the shortest, then the lowest index;
4. deletes the losers (unless `--keep-all`) and prints every result with output tails and the `winner`.

A winner exists only if something passed, so a failing result is never offered. Then
`fluxctl agent promote <winner> --dry-run` and apply, as above. It is client-side orchestration over fork, exec and
delete (like `rollout`); it needs a running session with a workspace baseline to be promotable.

### Lifecycle guarantees

- **Conflicting operations are refused, not interleaved.** Checkpoint, restore, fork and delete take the session's
  lifecycle slot; a second one, or any new `exec`, job start, file write/delete or git commit/branch while one runs,
  gets `409` naming what is in flight. Restore and fork also refuse while commands are running on the session (they
  would replace the machine under them); a checkpoint and a delete do not (delete is how a stuck session is ended).
  The gate is in the daemon's memory: it protects one daemon process, and a restart clears it.
- **Revisions.** The session record carries `revision`, bumped by every save. `If-Match: <revision>` (`*` always
  matches) on checkpoint, restore, fork and delete returns `412` when the session changed since you read it.
- **Idempotency.** `Idempotency-Key` is honoured on session create, `checkpoint`, `restore`, `fork`, `exec` and `jobs`
  (not on `tokens`, whose response holds a secret), with the semantics of the VM routes: a retry with the same key and
  body replays the first 2xx response, a different body is `422`, a key still running is `409`, records last 24 h.
- **Expiry.** A session past its `expires_at` is persisted as `expired` the next time anything reads it (every route
  refuses it with `409`; delete still works). The record and listing also report `gone` when the daemon no longer has
  the sandbox; that is shown, not saved.
- **Sweeper.** A background task (first pass 30 s after start, then every 60 s) marks expired sessions `expired` without
  anyone reading them and removes what an ended (`expired` or `deleted`) session left: tokens, grants, previews,
  approvals, job metadata, usage and HTTP rules. The record and ledger stay. `session.cleanup` is ledgered once per
  session; delete runs the same cleanup. Previews of a deleted session used to linger until their expiry; they no
  longer do.
- **Interrupted operations.** While a checkpoint, restore, fork or delete runs, a marker file exists
  (`<state_dir>/agent-session-ops/<session>.json`). If the daemon dies mid-way the marker is left, and the next read of
  the session shows `interrupted_op: {op, started_at}` and ledgers `lifecycle.interrupted` once, until another
  operation completes. It reports; it does not repair: whether a half-done restore or fork left the sandbox usable is
  for the operator to check.

Not done: the lifecycle gate is still in the daemon's memory (a restart clears it, and two daemons on one state
directory do not see each other); reads (`files`, `git status`, `job read`) are not gated; jobs running in a guest are
not re-adopted into a new daemon's view beyond the metadata it persisted. The sweeper and the marker are unit and
router tested, not run through a real daemon restart.

### Session tokens

An admin mints a bearer token that works on **one session** and only for the operations it names, so an agent can
run commands and read its workspace without VM-management access.

```json
POST /v1/agents/sessions/{id}/tokens
{"ops": ["exec", "files.read", "files.write", "git", "jobs"], "ttl_seconds": 3600, "label": "claude-code"}
```

The response carries `token` (`fxst_…`) **once**; the store keeps only its SHA-256. Grantable operations: `read`
(session record, events, trajectory, artifacts, preview list), `exec`, `jobs`, `files.read`, `files.write`, `git`,
`checkpoint`, `preview`, `screen` (the guest display: `GET .../screenshot[?max_width=N]` returns a PNG, `POST .../input`
takes one action or `{"actions": [...]}`, `POST .../screen/wait` blocks until the display changes (`timeout_seconds` ≤ 60); vz sessions only, ledgered as `screen.shot` / `screen.input`, typed text is not
copied into the ledger). Not grantable, so admin-only: create, list, delete, restore, fork, terminals and `ssh`, and
minting tokens (a session token cannot mint another).

- A token reaches only `/v1/agents/sessions/{its id}…`; any other path (another session, `/v1/vms`, the session list)
  is `403`. An operation it was not granted is `403`; an expired, revoked or unknown token is `401`.
- Lifetime is `ttl_seconds` (default 3600, at most 86400), capped at the session's own expiry. Deleting the session
  revokes every token. At most 32 live tokens per session. Tokens are ledgered as `token.mint` / `token.revoke`
  (never the secret) and calls are audited as `session-token:<id>`.
- A scoped caller sees the session as the session's tenant does. The process and filesystem policy applies to its
  `exec`, jobs, files and git calls exactly as to an admin's.

From the CLI: `fluxctl agent token <session> --ops exec,files.read,git --ttl-seconds 1800`, `fluxctl agent tokens
<session>`, `fluxctl agent untoken <session> <token id>`; MCP `session_token`. A session token is an ordinary bearer
token to the client, so hand it over as `FLUXVM_TOKEN` and `fluxctl agent exec|files|cat|put|git|bg|logs|checkpoint …`
work against that one session. Verbs the token does not grant, and `agent run`/`ls`/`fork`/`promote`, answer `403`.

Not done: tokens are not narrowed by path or by git repository.

### Approvals

### Enforcing the program list (`process.enforce`)

`process.allow` and `process.deny` match the command *string*, so `sh -c`, `$(...)` or a script the agent wrote walks around
them. `"process": {"allow": ["git *", "python3 *"], "enforce": "exec"}` additionally makes the programs the patterns name
(the first word of each `allow` and `ask` pattern) the only files the guest kernel lets start (Landlock execute right).
`fluxctl agent create --cmd-allow 'git *' --cmd-allow 'python3 *' --cmd-enforce`.

- A pattern's first word must be a literal program name or absolute path. A bare name is looked up in the usual bin
  directories; `g*t` or a relative path is refused at create. An `allow`/`ask` list is required.
- The shell, `true` and the dynamic loader are always allowed. A script the agent writes into `/tmp` or the workspace
  cannot be executed directly, and neither can a copied binary; it can still be run through an allowed interpreter
  (`python3 script.py`), which is why the interpreter you allow is the boundary.
- `exec`, background jobs and `rewind/bisect` tests run under the list. The server's own file helpers (listing, ranged reads,
  upload assembly, workspace size) keep the filesystem confinement but not the list. The `git` routes keep the list, so the
  session must allow `git` to use them.
- Jobs need the guest job supervisor (`jobs-v2`); without it the start is refused (`409`).
- Create fails when the create-time probe shows the guest did not enforce it (`capabilities.exec_allowlist` is true when it
  did). There is no best-effort mode for this key.
- `deny` and argument patterns such as `git push*` stay string rules: the kernel sees paths, not arguments.

`process.ask: ["deploy *", "git push*"]` makes a matching command wait for an operator. It applies to `exec` and job
start, with the same piece-by-piece matching as `allow`/`deny` (a piece that matches `ask` also satisfies a non-empty
`allow` list; `deny` still wins).

1. The agent's `exec` / job request is **not run**. The answer is `428` with `approval_id`, `summary`, `matched` and
   `expires_at` (10 minutes). Asking again for the same command returns the same request.
2. An operator decides: `POST .../approvals/{approval}/approve` (or `deny`, optional `{note}`), or
   `fluxctl agent approve|deny <session> <approval>`, MCP `session_approve`. **Admin credential only**: a session token
   carries a read-only role and is refused, so an agent cannot approve its own command.
3. The agent repeats the request with `approval_id` (CLI `--approval-id`), or sets `approval_wait_seconds` (at most 120;
   CLI `--approval-wait`) to hold the first request open for the decision. A held request releases the session's work
   slot, so checkpoint and restore are not blocked behind it.

The approval is bound to a SHA-256 digest of the session, the kind (`exec`/`job`), the exact command, its parameters
(timeout, stdin, output cap), the session's policy and the session **revision**, which restore, fork and checkpoint
change. A different command, a changed parameter or policy, or a restore in between makes it `403`. It is **single
use**: consuming it is a compare-and-set under one lock (two concurrent retries: one runs). Denied, expired and
already-used approvals are `403`. The ledger records `approval.requested`, `.approved`, `.denied`, `.consumed` and
`.refused` with the approver. Records live in `<state_dir>/agent-session-approvals/<session>.json`, and are removed
with the session.

Limits, said plainly: matching is on the command string, so an `ask` rule is as good as a string glob is (see the
process-policy caveat above) — a command written as `sh -c "$(...)"` matches the `sh` piece, not what it builds.
The approval covers the command and session revision, **not the workspace's file contents** (an edit by a file write
does not change the revision). File writes, git verbs and previews are not subject to `ask`. A `428` is not a 2xx, so
an `Idempotency-Key` on the request is not stored and can be reused after the decision. **Not run live:** the flow is
covered by unit tests of the decision logic (single use, mismatch, expiry, flood cap, concurrent consume) and a
router-level test (agent held, token cannot decide, other tenant 404, approve, one run, replay refused, denial
sticks); it was not run against a real guest.

### Policy

A policy is compiled into fields the sandbox already enforces. The response reports exactly what was compiled.

| Input | Compiles to | Enforced by |
|---|---|---|
| `preset: "offline"` | `offline: true` (no network card) | vz: the VM has no NIC |
| `preset: "developer-safe"` | `allow_hosts`: `github.com`, `*.github.com`, `*.githubusercontent.com`, `crates.io`, `*.crates.io`, `static.crates.io`, `pypi.org`, `files.pythonhosted.org`, `registry.npmjs.org`, `proxy.golang.org`, `sum.golang.org` | vz: the host egress proxy (ports 80 and 443) |
| `preset: "open"` or no policy | nothing | n/a |
| `network.default: "deny"` + `allow: [host…]` | `allow_hosts` (merged, de-duplicated); `offline` when nothing is allowed | as above |
| `network.allow` entries with `methods` or `paths` | **refused (`400`)** | not supported yet; HTTP method/path rules exist for the QEMU/KVM egress proxy, not through this API |
| `process: {allow: [glob…], deny: [glob…], ask: [glob…]}` | `policy.process` | the API, on `exec` and job start (`403`, ledgered as `exec.refused` / `job.refused`). `ask` holds a matching command for an operator's [approval](#approvals) (`428`). A guardrail on the command string, **not a security boundary**: see below |
| `filesystem: {read, write, enforcement?}` | `policy.exec`: a guest Landlock + seccomp policy (system dirs read-only, `/tmp` and the workspace/volumes writable, plus your `read`/`write` paths; a trailing `/**` means the directory) | the guest agent, on `exec`, jobs, file and git verbs. Linux guests only (including Linux on Apple VZ). **Create probes enforcement and returns `400` if the guest cannot give it** (macOS guest, procbox sandbox); `enforcement: "best-effort"` creates the session anyway and `capabilities.confinement` says `false`. Network reach is still the sandbox's (no NIC / egress proxy). Not run live yet |

`process` rules match the command string as submitted, with `*` as the only wildcard. A command is split on
`; && || | &` and newlines and every piece is checked: a piece matching `deny` refuses the whole command, and with a
non-empty `allow` every piece must match one of its patterns. A shell can build a command the string never shows
(`sh -c`, `$(...)`, variables, a script the agent wrote), so this stops accidents, not an adversary. For that, use the
network policy and `speculate`/changesets. The rules cover `exec` and jobs; the git, file and MCP verbs run fixed
commands and are not matched against them.

### Capabilities by backend

| Backend | snapshot / restore | fork | PTY |
|---|---|---|---|
| `vz` (macOS), offline or allow-listed session | yes (memory restore verified live; a disk-only restore cold-boots) | **yes** (run live: 2 children, 2.5 s) | **yes** (`shell`, below) |
| `vz`, session on the Mac's NAT (policy `open`) | yes | **no**: children would share one MAC and address | **yes** |
| `flux-vm` (in-tree KVM, Linux) | yes | yes (not verified on hardware) | no |
| `qemu`, `firecracker` | yes | no | no |
| `cloud-hypervisor` | not confirmed | no | no |

`GET /v1/agents/sessions/{id}` reports `capabilities` for the session's backend so a client can decide without trying.

On vz, `capabilities.memory_checkpoint` says whether the VM's device set can be saved with memory (probed at create
with the runner's `save-support`); when it is `false`, `capabilities.memory_checkpoint_reason` is the framework's reason
and a checkpoint without `{"memory": false}` is refused with `409` and that reason.

### Terminal (PTY)

`fluxctl agent shell <session> [-- command…]` opens an interactive shell on a real terminal; REST is a WebSocket,
`GET /v1/agents/sessions/{id}/pty?cols=&rows=&command=`, whose binary frames are raw terminal bytes both ways. The
daemon runs `ssh -tt` to the guest (the vsock proxy for an offline session), so the guest sees a tty: line editing,
job control and full-screen programs work. The size is set when it opens, and a resize is a text frame
`{"resize":{"cols":N,"rows":N}}` (`fluxctl` sends one on SIGWINCH): the daemon sets the guest tty's window size from a
second SSH exec, so the foreground program gets SIGWINCH after about a round trip. The close frame's reason is `exit=<code>` and the CLI exits with it. Opens and closes (duration, byte
counts, exit code, never the content) are ledgered as `pty.open`/`pty.close`. A session with `process` rules or a `filesystem` policy refuses the terminal and `ssh` (`403`): both are an SSH shell, which the guest confinement does not cover. At most 16 at once on a host, one hour
each. A session with `policy.process` rules is **refused (403)**: a shell would bypass them.

**Browser terminal.** `fluxctl agent shell <session> --web` (or `/console/terminal?session=<id>` on the daemon) opens a
terminal page built on xterm.js (MIT, vendored under `crates/fluxvm-api/src/terminal/vendor/`, checksums in
`SHA256SUMS`): WebGL rendering with a DOM fallback, 24-bit colour, Unicode 11, OSC 8 and plain-URL links, sixel and iTerm2
inline images, clipboard (OSC 52), 15,000 lines of scrollback. The WebSocket is opened with a **30-second single-use
ticket** (`POST .../pty-ticket`, then `GET /console/pty/{ticket}`), because a browser WebSocket cannot send
`Authorization`; the ticket lives in memory only, is never ledgered, and is refused for sessions with command rules.
The page's own CSP is `script-src 'self' 'wasm-unsafe-eval'` (the sixel decoder is WebAssembly; no string `eval`).
- **Tabs and panes** like WezTerm: a pill tab bar at the bottom with the tab index and the shell's own title, `+` for a
  new tab, split panes, the inactive pane dimmed. Keys: `⌘T` new tab, `⌘W` close pane/tab, `⌘1-9` tab, `⌘D` / `⌘⇧D`
  split, `⌃⇧⌥"` / `⌃⇧⌥%` split, `⌘⌃h/j/k/l` move focus, `⌘+` / `⌘-` / `⌘0` text size, `⌘K` clear. Off macOS the
  same keys are `Ctrl+Shift+…`. Every tab or pane is another shell in the same session.
- **Themes:** "Neon Storm" (default in a dark browser; taken from the author's `~/.wezterm.lua`: Tokyo Night Storm
  palette, `#82aaff` accent, `#16161e` chrome, a 135° `#16161e → #2a2b3b → #16161e` window gradient at 0.94 opacity,
  bar cursor 2 px wide, bell flash, a `HH:MM:SS` clock), plus macOS Pro, macOS Basic, Tokyo Night, Dracula and Solarized
  Dark. Font stack: Hack / MesloLGS / FiraCode / 0xProto Nerd Fonts if installed, then SF Mono, Menlo. Choice and text
  size are remembered per browser.
- **Search, copy mode, quick select, prompt marks, palette.** `⌘F` search bar over the scrollback (Enter / `⇧Enter`
  step, Esc closes); `⌃⇧X` copy mode (vi-like: `h j k l` `w b` `0 $` `g G`, `v` / `V` visual, `y` copy, `/` search,
  Esc); `⌃⇧Space` quick select (URLs, paths, UUIDs, IPv4, git-style hashes and 4+ digit numbers **on the visible
  screen** get letters; typing one copies it, with Shift a URL opens); `⌘↑` / `⌘↓` jump between prompts; `⌘⇧P` a
  command palette listing every action and theme. Prompt marks are OSC 133 sequences that the daemon's bash rc file
  emits **only when the client says `TERM_PROGRAM=FluxVM`** (this page), so other terminals see no change; a gutter bar
  marks each prompt and the status line shows the last non-zero exit. Limits: quick select reads screen rows, so a
  match wrapped across two rows is missed; marks need the rc-file bash shell (not `--plain`, not zsh); no kitty
  graphics, no ligatures; the cursor blink rate is the library's.
- **Not built yet:** window blur; "new session" from the picker. The Velora native window is a separate change.
- **Run** (Chrome headless on this M4, offline session): prompt, WebGL, typing, `TERM`/`COLORTERM`/`TERM_PROGRAM` in the
  guest, the guest tty size following the window (24x152 to 24x95), a 24-bit foreground kept as RGB, an OSC 8 link, a sixel
  adding an image canvas, `⌘T`, `⌘D`, `⌘⌃h`, `⌘1`, the bottom tab bar, the accent colour and gradient, and the exit code;
  then prompt marks arriving from the guest, `⌘↑`/`⌘↓` moving the viewport between them, "last exit 1" after `false`,
  `⌘F` finding text, quick select labelling a URL, a path and a hash and copying the URL, copy mode `v`+`w`+`y`
  selecting and copying "alpha b", and the palette switching the theme. **Not run:** Safari/WebKit (that is the Velora window's job), Firefox, a real WezTerm to compare against.

**Looks.** The terminal takes its look from yours: `fluxctl` sends its own `TERM` (the guest falls back to
`xterm-256color` when it lacks that terminfo) and, when your `COLORTERM` is `truecolor`/`24bit`, sets it in the guest so
24-bit-colour programs use it. The Debian prompt (green `user@host`, blue path), coloured `ls` and UTF-8 (`C.UTF-8`) are
the guest's own. For a bash login shell the daemon also starts it with a small rc file after the normal profile:
`grep`/`egrep`/`fgrep`/`diff`/`ip` colour, `ll` and `la`, `less -R`, coloured man pages and compiler messages.
`--plain` turns that off; a one-off command (`shell -- cmd`) never gets it. Colours are the guest's output rendered by
your terminal, so the result is whatever your Terminal profile makes of them: run in this session was a check that
the variables and aliases arrive, not a screenshot of Terminal.app.

**WezTerm, iTerm2 and other modern terminals.** The terminal is a transparent byte pipe, so whatever your emulator
renders (24-bit colour, OSC 8 hyperlinks, inline images by the iTerm2, sixel or kitty protocols, mouse reporting,
bracketed paste, ligatures, scrollback search) works with guest programs that emit it. `fluxctl` also forwards your
`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` (and sets `LC_TERMINAL=iTerm2` for iTerm.app) so tools that sniff the terminal
pick the right image protocol. Those sequences were sent from the guest and arrived byte for byte through the daemon,
the WebSocket and `fluxctl`; **no real WezTerm or iTerm2 was run here** to draw them, and the guest image has no image
or multiplexer tools (`chafa`, `imgcat`, `tmux`) unless you install them.
For WezTerm's own panes, tabs and domains, use plain ssh: `fluxctl agent ssh-config <session> --name demo >> ~/.ssh/config`
then `wezterm ssh demo` (or `ssh`, `scp`, `rsync`, port forwards). The block carries the shared sandbox key and, for an
offline guest, a `vsock-proxy` ProxyCommand through this binary, so it works only on the machine running the daemon
and is refused to tenant tokens (that key opens every sandbox on the host). Run: `ssh` and `scp` through the generated
config into an offline session.

Run on this M4 (`vz`, stock `debian-13`, offline session): `tty` printed `/dev/pts/0`, `isatty` was true for stdin and
stdout, `TERM` was `xterm-256color`, exit codes 0, 3, 5 and 7 came back, and a terminal opened at 120x33 reported
`stty size` `33 120` after typing into it through a local pseudo-terminal (raw mode on); resizing that terminal to
50x160 and then 20x70 showed `50 160` and `20 70` in the guest and a `WINCH` trap fired. **Not run:** `vim`/`top`,
Windows terminals, TLS (`wss`), `--server` over a network, a client other than `fluxctl`, Linux.

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
**Limits:** a single write is capped by the daemon's request-body limit, 1,500,000 bytes of file. The daemon drops the
connection on a larger body (a reset, not a clean 413), so `fluxctl agent put` and the MCP tool refuse first with a
message; a raw REST client still sees the reset. List entries come from GNU `find`, so a guest without it (busybox) is
not supported. Larger files use the resumable transfer below.

#### Large files: resumable upload and ranged download

```bash
fluxctl agent upload   <session> ./dataset.tar /workspace/dataset.tar --mode 644   # prints the upload id
fluxctl agent upload   <session> ./dataset.tar /workspace/dataset.tar --resume <upload id>   # after an interruption
fluxctl agent download <session> /workspace/out.tar ./out.tar [--resume]
```

| Route | |
|---|---|
| `POST .../uploads` `{path, size, sha256?, mode?, parents?}` | open an upload (size 1 byte to 2 GiB); at most 8 open per session |
| `PUT .../uploads/{upload}?offset=N` raw bytes | the next chunk (at most 4 MiB). `N` must be `received`; otherwise `409` says where to continue |
| `GET .../uploads/{upload}` / `GET .../uploads` | `received`, `size`, `parts`, state |
| `POST .../uploads/{upload}/complete` | assemble, verify, rename into place. Calling it again after success returns the same record |
| `DELETE .../uploads/{upload}` | drop the upload and its parts in the guest |
| `GET .../files/range?path=&offset=&length=&checksum=` | up to 4 MiB of a file with `size`, `eof` and (with `checksum=true`) the file's `sha256` |

- Chunks are written as part files under `/var/tmp/.fluxvm-uploads/<upload>/` in the guest. `complete` concatenates them
  into a temporary file next to the destination, checks the **size** and, when you gave one, the **SHA-256** (needs
  `sha256sum` or `shasum` in the guest), applies `mode`, then `mv`s it over the destination, so nothing half-written is
  ever at `path`. A mismatch is `409` and writes nothing.
- The record is on the host (`<state_dir>/agent-session-uploads/<session>.json`), so an upload survives a daemon restart;
  `received` is the byte to continue from. `fluxctl agent upload` hashes the file first and sends the checksum, retries a
  failed chunk three times, and re-asks the daemon where it is after an error, because the chunk may have landed.
- `files.write` is required to upload; `files.read` to read ranges and list uploads. Both are ledgered as `file.upload`,
  `upload.open` and `upload.abort` (path, size, whether a checksum was verified; never content). A session token's
  confinement applies to the assembly step. Uploads and parts are removed with the session by the cleanup pass.
- `download` writes `<local>.part`, asks for the guest's checksum on the first slice, verifies the finished file
  against it, and only then renames. Reads are slices, so a file that changes while it is being read is detected by
  size or checksum and reported, not silently mixed.

**Not run on a live guest.** The assembly and range scripts are unit-tested by running real `sh` (parts, rename, size
and checksum mismatches, a missing part, quoting) and the routes' input handling by router-level tests; chunk writes
use the existing file-write path, and none of it went through a daemon with a real sandbox. Part files take the
guest's disk (up to the file's size, twice during assembly).

### Background jobs

A command that outlives the request, with output you read incrementally:

```bash
J=$(fluxctl agent bg <session> --timeout 600 -- 'npm run dev' | jq -r .job)
fluxctl agent logs <session> $J -f      # follows until exit, exits with the job's exit code
fluxctl agent stdin <session> $J --close # EOF on stdin (job started with --stdin)
fluxctl agent kill <session> $J         # SIGTERM to the whole process tree; --signal KILL
```

`POST .../jobs {command, stdin?, max_output_bytes?, timeout_seconds?}` starts it. There are two engines; the response
and the job record say which one ran (`engine`):

- **`supervisor`** (guest agent with `jobs-v2`, checked with a `Capabilities` request at start): the agent runs the job in
  its own process group, with separate stdout and stderr spools under `/var/lib/fluxvm-jobs/<job>/`, an optional stdin
  pipe, a timeout and tree signals. The agent keeps draining its pipes after the retention cap, so the command is never
  given SIGPIPE and its **real exit status** is kept; `dropped_bytes` says how much was discarded. Confined sessions
  start the job through procbox (Landlock + seccomp) and return the guest's `enforcement` report.
- **shell-script** (older agents, or a guest the supervisor cannot reach): the earlier implementation. Output is merged
  stdout+stderr in `/tmp/fluxvm-jobs/<job>.out`, bounded with `head -c` (the job gets SIGPIPE at the cap), stdin has no
  end-of-file, and `timeout_seconds` is refused with `400`.

Reading:

- `GET .../jobs/{job}?offset=N` returns `state`, `exit_code`, `output`, `next_offset`, `more` and `truncated`. Supervised
  jobs also return `stderr`, `stderr_next_offset` (pass `stderr_offset`), `signal`, `dropped_bytes`, `eof` and
  `output_base64`; `wait_ms` (at most 25 000) holds the request until output arrives. `state` is `running`, `exited`,
  `killed` (stopped by a signal request), `signaled` or `timed_out`.
- `GET .../jobs/{job}/stream` is **Server-Sent Events**. `output` events carry `{offset, next_offset, output,
  output_base64}`, then one `exit` event, or an `error` event if the job or session went away. Shell-script jobs use
  `next_offset` as the event id; supervised jobs also send `stderr` events and use the id `<stdout>:<stderr>`, which
  `Last-Event-ID` (or `?offset=&stderr_offset=`) resumes with no gap or repeat. Supervised streams are paced by the
  guest's long poll, not a timer. At most 16 streams per host, one hour each; the session must stay `running`.
- `GET .../jobs` lists every job with `command`, `state`, `exit_code`, `signal`, `engine` and times. The metadata is
  kept on the host, so it survives a daemon restart; states are refreshed from the guest on each listing.

Writing and limits:

- `stdin: true` gives the job a stdin that `POST .../jobs/{job}/stdin {text | data_base64, close?}` writes to (up to
  48 KiB per write; `409` once the job has exited). `close: true` sends end-of-file (supervised jobs only; a shell-script
  job returns `400`). Only sizes are ledgered (`job.stdin`), never the data.
- `max_output_bytes` (default 16 MiB, at most 256 MiB) bounds retained output per stream.
- `DELETE .../jobs/{job}[?signal=TERM]` signals the whole process tree (supervised jobs; shell-script jobs get SIGTERM to
  the group and exit 143, state `killed`).
- The same guest agent also hardens synchronous `exec`: a timeout now kills the command's process group, output is
  capped at 16 MiB per stream (with a marker), and the call returns within about two seconds once the command has
  exited even if a background child keeps the pipes open.

CLI: `fluxctl agent bg [--stdin] [--max-output-bytes N] [--timeout S]`, `jobs`, `stdin <id> <job> [text] [--close]`,
`logs [-f]` (stderr goes to the terminal's stderr; supervised jobs long-poll), `kill [--signal S]`. MCP:
`session_job_start`, `session_job_read`, `session_job_stdin`, `session_jobs`, `session_job_kill`.

Run live earlier (M4, `vz`, NAT session, shell-script engine): a 4-second job followed to its exit code 7; a `sleep 300`
job killed (143); a 200 000-byte output read back whole across several pages; an unknown job id returned 404.
**Not run on a live guest:** the supervisor itself. Its behaviour (separate streams, binary output, offsets, long poll,
stdin EOF, drain past the cap with the real exit code, a descendant holding the pipes killed on timeout and cancel) is
covered by unit tests that run real processes on macOS, and the new synchronous-exec behaviour by two more. The
Linux-only confined spawn (`fluxvm-procbox::spawn`) was **not compiled or run here** (no Linux toolchain in the
development shell). The new API branches (`supervisor` engine in read/stream/stdin/kill/list) are compile-checked and their
helpers unit-tested, but were not run against a daemon. A job lives in the agent's memory: a reboot or agent restart
forgets the table (spools stay on disk); jobs are not re-adopted yet.

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
the ledger. A WebSocket upgrade on a preview URL is bridged with the same token check (run: a Chrome DevTools
round trip, see below; a bad token is 404). **Not run:** Linux (the route reuses the netns proxy there), streaming or
large bodies, and HTTPS (the URL is as private as the daemon's own address: put it behind TLS yourself). An **offline** session has no
network address, so it has nothing to preview.

### Browser (Chrome DevTools Protocol)

There is no browser image yet; this is a recipe on the stock `debian-13`, run on this M4 (`vz`, `policy open`, 2 GiB):

```bash
fluxctl agent run --policy open --memory-mb 2048                 # open: apt needs the network
fluxctl agent bg <s> -- sh -c 'sudo -n apt-get update -qq && sudo -n DEBIAN_FRONTEND=noninteractive \
  apt-get install -y -qq --no-install-recommends chromium socat'   # ~minutes; poll with `agent logs`
fluxctl agent bg <s> -- chromium --headless=new --no-sandbox --disable-gpu --remote-debugging-port=9222 about:blank
fluxctl agent bg <s> -- socat TCP-LISTEN:9223,fork,reuseaddr TCP:127.0.0.1:9222
fluxctl agent expose <s> 9223                                    # -> http://<daemon>/preview/<token>/
```

Why `socat`: Chrome 155 binds DevTools to `127.0.0.1` and ignored `--remote-debugging-address=0.0.0.0` in this run, so
the sandbox proxy (which reaches the guest's NIC address) could not connect without a forwarder.

A CDP client connects to `ws://<daemon>/preview/<token>` + the path of a target's `webSocketDebuggerUrl` from
`<preview>/json/list`. **That URL's host and prefix are wrong** in the JSON (Chrome reports the guest address and
omits `/preview/<token>`), so a client that follows `webSocketDebuggerUrl` itself (Playwright `connectOverCDP` by
HTTP endpoint) will not work through the preview: rewrite the URL as above. What was run: `/json/version` and
`/json/list` over HTTP, then over the WebSocket `Browser.getVersion`, `Page.navigate` to a `data:` URL,
`Runtime.evaluate document.title` (returned the page title) and `Page.captureScreenshot` (a 2.3 KB PNG). The same works
through the authenticated `/v1/sandboxes/{sandbox_id}/ws/{port}/{path}`. **Not run:** Playwright/Puppeteer clients,
headful or GPU, a window/computer-use display (vz display and input exist separately), a prebuilt browser image
(Chromium adds ~600 MB to the 2.8 GB stock root; the install above fits but leaves little free).

### Image readiness

`GET .../probe` (`fluxctl agent probe`, MCP `session_probe`) runs one short command in the guest, under the session's
confinement, and reports which tools exist (`git`, `sha256sum`, `python3`, `node`/`npm`, `cargo`, `go`, `gcc`/`make`,
`chromium`/`google-chrome`, `socat`, `playwright`, a Playwright browser cache) and what that makes possible:

| `ready_for` | needs |
|---|---|
| `git` | `git` (workspace clones, the git verbs) |
| `promote` | `sha256sum` (fork promotion compares content by sha256) |
| `build_node` / `build_python` / `build_rust` / `build_c` | `node`+`npm` / `python3` / `cargo` / `make` and `gcc` or `cc` |
| `browser_cdp` | a Chromium or Chrome |
| `browser_cdp_through_preview` | a browser **and** `socat` (Chrome binds DevTools to 127.0.0.1; see Browser) |
| `playwright` | a `playwright` CLI or a Playwright browser cache |

`workspace.require_ready` at create (`fluxctl agent run --require-ready`) turns a workspace that did not initialize
(directory not created, clone failed) from a session with a `workspace.note` into a `502` that removes the sandbox.

**DevTools URLs through a preview are rewritten.** A preview of Chrome's DevTools port answers `/json`, `/json/list`,
`/json/version` and `/json/new` with `webSocketDebuggerUrl` and `devtoolsFrontendUrl` pointing at the daemon and under
`/preview/<token>` (`wss` when the request came with `X-Forwarded-Proto: https`), so a client that follows them, such as
Playwright `connectOverCDP`, reaches the page. The rewrite is unit-tested on Chrome's document shape; **not run** against
a live Chrome through a preview, nor with a real Playwright client.

**No versioned coding or browser image is shipped by this change.** Building one needs an image pipeline and a registry,
not an API; what is added is the probe to verify any image against these needs, and the readiness gate. Until an image
exists, the Browser recipe above is how you get one.

### Ledger

An append-only JSON-lines file per session records the API-level actions taken *through the session*: `session.create`,
`workspace.clone`, `exec` (command, exit code, duration), `checkpoint`, `restore`, `fork`, `session.delete`. It is not
a recording of the guest: commands an agent ran inside a shell it opened are one `exec` entry. Files live in
`<state_dir>/agent-sessions/<id>.json` and `<id>.ledger.jsonl`, so sessions survive a daemon restart; a sandbox that
expired by TTL leaves the session record behind until `DELETE`.

### Trajectory and OpenTelemetry traces

`GET /v1/agents/sessions/{id}/trajectory` merges the ledger and the sandbox's VM events into one numbered,
time-ordered list. `fluxctl agent export <session> [--out file]` writes it as JSON Lines (a session header, then one
step per line). `--otlp http://collector:4318 [--otlp-header KEY=VALUE]` also sends it as an OTLP/HTTP JSON trace to
`<endpoint>/v1/traces`: one root span `agent.session`, one child span per step, trace id = the session's UUID, an
`exec` span covering its `duration_ms`, other steps as instants, and status error for a non-zero exit or a refused
command. It is built client-side from what the ledger records, after the fact: **no live streaming, no agent
reasoning or tool-call content** (FluxVM does not see it). Run: a session with a good exec, a failing exec and a
refused command, posted to a stub receiver that checked the path, content type and header and the 7 spans. **Not
run:** a real OpenTelemetry Collector or backend, OTLP/gRPC or protobuf.

## Rewind: undo the last action

Rewind takes a checkpoint automatically before every `exec`, keeps the last few in a ring, and puts the machine back to one of
them on request. It is **off by default**; turn it on per session:

```bash
curl -X POST $B/v1/agents/sessions/$ID/rewind/config -d '{"enabled": true, "max_points": 10}'
curl -X POST $B/v1/agents/sessions/$ID/exec -d '{"command": "rm -rf /srv/app"}'
curl -X POST $B/v1/agents/sessions/$ID/rewind/undo -d '{"steps": 1}'   # /srv/app is back, processes still running
curl $B/v1/agents/sessions/$ID/rewind                                   # the remaining points
```

- **A point** is a checkpoint with memory when the session's devices can be saved with memory (`capabilities.memory_checkpoint`)
  and `memory` is not `false`, otherwise disk-only. Each one records the command that was about to run.
- **`undo {steps}`** goes back that many actions (`1` reverses the last exec). It restores like `POST .../restore` does (same
  refusals while commands run, same re-binding of grants and HTTP rules), then **drops that point and every newer one**: they
  describe a timeline that no longer exists. There is no redo yet.
- **The ring** keeps `max_points` (default 10, at most 50); the oldest point and its snapshot are deleted when it is full, and
  lowering `max_points` trims it at once.
- **Best effort.** If a point cannot be taken (another checkpoint, restore or fork holds the session, or the snapshot fails), the
  command still runs and the ledger gets `rewind.skipped` or `rewind.capture_failed`. A point costs a snapshot per exec (seconds
  with memory, see Measured), so turn it on for sessions where undo matters.
- Points are kept outside the session record: taking one does not change `revision`. Ledger kinds: `rewind.config`,
  `rewind.point`, `rewind.undo`.
- **Pin** (`POST .../rewind/pin {tag}`, `{tag, pinned:false}` to release): a pinned point is skipped by eviction and kept by an
  undo that would otherwise drop it. At least one slot must stay free, so a pin that would fill the ring is `409`.
- **`undo {to: tag}`** goes back to a named point instead of counting steps.
- **Bisect** (`POST .../rewind/bisect {test, good?, bad?}`) answers "which command broke it?". The states are the points (the
  machine just before each exec) plus the machine as it is now. It restores a state, runs `test` in the guest (exit 0 = good),
  and binary-searches between a state that passes (`good`, default the oldest point) and one that fails (`bad`, default now).
  The answer is the command that ran between the last good and first bad state, plus every state it tested. It first checks
  that the ends disagree (`409` otherwise), takes a temporary snapshot of the machine as it is, and always restores it, so the
  session ends where it started; the ring is untouched. `test` is subject to the session's process policy and runs with its
  exec policy. A `n`-point ring needs about `log2(n)+2` restores.
- CLI: `fluxctl agent rewind <id> [on [--max-points N] [--disk-only] | off | undo [--steps N | --to TAG] | pin TAG [--unpin] | bisect TEST [--good TAG] [--bad TAG]]`;
  with no subcommand it lists the points.
- **Screenshots.** When the guest has a display, a point also keeps a 480 px thumbnail of it (`"screenshot": true` in the list),
  served by `GET .../rewind/{tag}/screenshot` (needs the `screen` scope). It is best effort with a 3 s limit: a guest with no
  display, a blank one or a slow one simply has none. It goes away with its point and with the session.
- **Triggers.** `config {triggers: [...]}` chooses what takes a point: `exec` (the default), `job` (a background job start),
  `file` (a file write, delete or a completed upload) and `git` (commit, branch). `fluxctl agent rewind <id> on --triggers
  exec,file,git`. Every trigger costs a snapshot, so turn on only what an undo needs to cover; the point's `command` reads
  `write /path`, `delete /path`, `upload /path`, `commit <message>` or `branch <name>` for the non-exec ones. Chunked upload
  parts and reads never take one. An `undo` reverses them like any other point: it puts the whole machine back.
- **Branch** (`POST .../rewind/branch {tag, count?, identity?}`): starts child sessions from a *past* point and leaves this
  session exactly as it is, so "what if I had not run that command?" runs in a child while the parent carries on. It needs a
  forkable session (same rules as `fork`, `409` with the reason otherwise), counts the children against the budget, and
  returns them like `fork` does (`parent` is this session, ledger kind `session.branch_child`). The point's snapshot is
  exported to a short-lived [branch](macos.md) that the children start from and is then removed; the point stays in the ring.
  A branch child has **no workspace baseline** (the parent's files now are not its starting point), so `artifacts` and
  `promote` on it do not apply. `fluxctl agent rewind <id> branch TAG [--count N]`.

## Clients

```bash
fluxctl agent run claude-code --policy developer-safe --ttl-seconds 3600
fluxctl agent exec <id> -- 'cd /workspace && ls'          # one string = a shell line; several words = an argv
fluxctl agent checkpoint <id> --tag before-refactor
fluxctl agent restore <id> before-refactor
fluxctl agent timeline <id>
fluxctl agent artifacts <id>
fluxctl agent rm <id>
fluxctl agent run claude-code --confine --fs-write /data/out        # guest confinement (Linux guests)
fluxctl agent token <id> --ops exec,files.read,git --ttl-seconds 1800
fluxctl agent promote <child> --dry-run                              # review, then --plan-id <id> to apply
fluxctl agent exec <id> --approval-wait 120 -- 'deploy prod'        # a process.ask command: waits for an operator
fluxctl agent approve <id> <approval>                               # operator, admin credential
fluxctl agent bg <id> --stdin --timeout 600 -- 'python3 -u repl.py' # supervised job
fluxctl agent upload <id> ./big.tar /workspace/big.tar              # resumable, verified
fluxctl agent grant <id> --secret-ref gh --host api.github.com --method GET --path '/repos/acme/*'
```

Walkthroughs for each of these, with expected output and a troubleshooting table, are in
[agent-sessions-guide.md](agent-sessions-guide.md).

- **MCP** (`fluxctl mcp serve --allow-write`): `session_create`, `session_exec`, `session_approvals`, `session_approve`,
  `session_job_start`/`_read`/`_stdin`/`_kill`, `session_jobs`, `session_files`, `session_file_read`, `session_file_write`,
  `session_git`, `session_checkpoint`, `session_restore`, `session_rewind_points`, `session_rewind`, `session_fork`, `session_rollout`, `session_evaluate`,
  `session_promote`, `session_grant`, `session_revoke_grants`, `session_token`, `session_preview`, `session_probe`,
  `session_events`, `session_artifacts`, `session_delete`, plus `sandbox_speculate`, `sandbox_changesets` and
  `sandbox_changeset_decide`. The full table is in [mcp.md](mcp.md#agent-session-tools).
- **Go**: `client.CreateSession(ctx, fluxvm.CreateSessionRequest{…})`, then `Exec`, `Checkpoint`, `Restore`, `Fork`,
  `Events`, `Artifacts`, `Delete` (`go/sessions.go`).
- **Python**: `FluxVM(url).create_session("codex", policy="developer-safe")`, a context manager that deletes on exit
  (`python/fluxvm/sessions.py`).

## Measured

`scripts/bench-agent-session.sh`, results in [benchmarks/agent-session.md](benchmarks/agent-session.md): on an M4 Mac
mini (debug build, 5 runs): on a NAT session create 6.8 s median (26.9 s on a cold first run), first exec 0.12 s, memory
checkpoint 0.7 s, restore 4.1 s; on an offline session create 4.6 s, exec 0.39 s, fork into 2 children 2.5 s. **These are
not sub-second.** Targets (sandbox claim < 300 ms, warm restore < 150 ms, fork child < 100 ms) are **not met** on `vz` today,
where restore is Apple's. On the KVM engine RAM is already restored lazily and the time goes to the identity reset and device
re-attach instead (see the roadmap).

## Roadmap

Each item says why it is not in the MVP.

| Item | Why not yet |
|---|---|
| Promote a child's *non-file* state (memory, processes, volumes) | promotion copies workspace files only; a child's running processes and memory are not merged |
| Lazy memory restore (sub-second claim/fork) | **Done on the in-tree KVM engine, not on `vz`.** KVM restores RAM with a `MAP_PRIVATE` mapping of `snap.mem` (pages fault in on first touch, children share unwritten pages; see [fork-and-restore.md](fork-and-restore.md#ram-restore-map_private-with-an-eager-fallback)), so it is not what limits a fork there: one measured child took 4.3–9.2 s, of which RAM restore was about 1.2 s and the guest identity reset about 3.5 s. Not done on `vz` (Apple's Virtualization framework restores the saved state itself, which is the 4.1 s restore above) |
| Workspace overlay mode | `mode: "overlay"` is refused; clone is the only writable mode. `mode: "readonly"` is enforced (see Create) |
| Process policy: argv rules as a security boundary | `process.enforce: "exec"` (`fluxctl agent create --cmd-allow ... --cmd-enforce`) makes the `allow`/`ask` programs a kernel-enforced executable allow-list (Landlock), which holds for `sh -c`, scripts and children. `deny` and argument patterns (`git push*`) are still string globs, because the kernel sees paths, not arguments. The guest's exec allow-list is verified in a real Linux guest (`scripts/e2e/guest-exec-policy.sh`); the session-create step that compiles `process.enforce` into it is unit-tested, not yet run against a booted session |
| HTTP method/path rules on `vz` | the runner's allow-list proxy matches host names only; per-session rules are enforced by the host egress proxy (see above) |
| Streaming exec output | **Done:** `POST .../exec/stream` (`fluxctl agent exec --stream`). Plain `exec` stays buffered; the stream is a job under the hood, long-polled from the guest supervisor or polled every 400 ms on older agents |
| Replay | the ledger is a log, not a recording |
| Preview URLs: large or streamed responses | WebSocket upgrades are bridged (run with Chrome DevTools); large and streamed bodies were not run through the preview route |
| Browser / computer-use profile | a CDP recipe works on the stock image (see Browser above); no prebuilt browser image, no Playwright client run, no computer-use display profile |
| Secretless credentials at create time | session grants exist (`POST .../grants`, see Credential grants: they end with the session, are capped at its expiry and follow a restore). Declaring them in the create body is not offered on purpose: a grant names a `FLUXVM_SECRET_*` variable, so a tenant able to create one could aim any daemon secret at a host it controls; granting stays an admin call |
