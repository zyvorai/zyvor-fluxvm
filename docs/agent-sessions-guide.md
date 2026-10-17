# Agent sessions: a practical guide

[agent-sessions.md](agent-sessions.md) is the reference: every route, field and limit. This guide is the other half: how
to **use** sessions, by task, with the commands to run and what you should see. Read it top to bottom once, then use the
table of contents as a cookbook.

**Install first:** `curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash` ([install.md](install.md))
(0.4.8+ includes sessions; current is 0.4.11). Check `fluxctl agent --help`: `unrecognized subcommand` means an older
package — upgrade, or build from source (the install page says how). On a Mac, `--host-gpu` reaches the host Metal/MLX
server from an offline guest ([host-inference.md](host-inference.md)).

Every example uses `fluxctl agent ...` (REST: `/v1/agents/sessions/...`). Replace `$S` with a session id.

## Contents

1. [The model in five minutes](#1-the-model-in-five-minutes)
2. [First session](#2-first-session)
3. [Choosing a policy](#3-choosing-a-policy)
4. [Letting an agent use a session safely (tokens, MCP)](#4-letting-an-agent-use-a-session-safely)
5. [Running work: exec, jobs, files, large files](#5-running-work)
6. [Approvals: a human in the loop](#6-approvals-a-human-in-the-loop)
7. [Network rules and credentials](#7-network-rules-and-credentials)
8. [Budgets](#8-budgets)
9. [Checkpoint, fork, evaluate, promote](#9-checkpoint-fork-evaluate-promote)
10. [Previews and browsers](#10-previews-and-browsers)
11. [Operating it: ledger, cleanup, restarts](#11-operating-it)
12. [Fleet node agent](#12-fleet-node-agent)
13. [Troubleshooting](#13-troubleshooting)
14. [What has and has not been verified](#14-what-has-and-has-not-been-verified)

---

## 1. The model in five minutes

A **session** is one sandbox (a VM) plus a workspace directory, a compiled **policy**, a **ledger** of what happened,
and optional **checkpoints** and **fork children**. You create it, hand it to an agent, and inspect or roll back what the
agent did.

There are two kinds of caller, and the difference matters:

| Caller | Credential | Can do |
|---|---|---|
| **Operator** (you, CI, the orchestrator) | the daemon's admin token (or none in lab mode) | everything: create, restore, fork, delete, mint tokens, approve commands, grant credentials |
| **Agent** | a **session token** (`fxst_...`) minted for one session with a list of operations | only those operations on only that session |

The agent must never hold an admin credential. Approvals (section 6) and credential grants (section 7) are built on that
split: the agent can *ask*, only the operator can *decide*.

What is enforced, and where:

| Rule | Enforced by | Strength |
|---|---|---|
| `network` allow-list, `offline` | the sandbox (no network card, or the host proxy) | a real boundary |
| `network.allow` methods and paths | the host's egress proxy, per sandbox | a real boundary **where the guest's traffic goes through that proxy**; see 7 |
| `filesystem` | Landlock + seccomp inside the guest | a real boundary on Linux guests |
| `process` allow / deny / ask | string matching in the daemon | a **guardrail**, not a boundary |
| session tokens | the daemon | a real boundary between sessions |

If you remember one thing: **`process` rules stop accidents, not adversaries.** For a boundary use `network` and
`filesystem`.

## 2. First session

```bash
# 1. a session with a repository, restricted to GitHub, expiring in an hour
fluxctl agent run claude-code \
  --repo https://github.com/acme/app.git \
  --policy developer-safe --ttl-seconds 3600

# offline + host Metal/MLX (or llama.cpp / stand-in) inside the guest:
# fluxctl infer serve --stand-in   # or mlx_lm.server --port 8080
# fluxctl agent run --policy offline --host-gpu
# see docs/host-inference.md
```

The command prints the session record. Note `id`, `capabilities` (what this backend can do: `snapshot`, `fork`,
`pty`, `confinement`, `http_policy`) and `workspace.note` (why a clone failed, if it did; pass `--require-ready` to
fail the create instead).

```bash
S=<id from the record>

# 2. run something
fluxctl agent exec $S -- 'cd /workspace && ls -la'      # one string = a shell line; several words = an argv

# 3. start something long, read it, stop it
J=$(fluxctl agent bg $S --timeout 600 -- 'npm run dev' | jq -r .job)
fluxctl agent logs $S $J -f                              # follows until it exits
fluxctl agent kill $S $J

# 4. see what happened
fluxctl agent timeline $S                                # the ledger plus VM events
fluxctl agent artifacts $S                               # workspace changes since the baseline

# 5. finish
fluxctl agent rm $S
```

`fluxctl agent probe $S` tells you what the guest image can do (is `git` installed? `sha256sum`? a browser?), which
decides whether `git`, `promote` and checksummed uploads will work. Run it once on a new image.

### 2.1 Which engine your session got (Mac / vz)

On a Mac the session's commands reach the guest one of two ways, and the session says which in
`capabilities.engine` (also `capabilities.supervisor`, and a `session.engine` ledger entry):

| `engine` | What it is | You get |
|---|---|---|
| `agent` | the guest agent over vsock (image `agent-micro`, or a spec with `agent.enabled`) | hardened exec (a background child cannot hold a command open past its timeout), supervised jobs, `filesystem` confinement |
| `ssh` | the daemon's SSH key into a stock image such as `debian-13` | exec, files, PTY, checkpoints; **no** supervisor and **no** confinement, jobs run as shell scripts |

If `fluxctl catalog` has an `agent-micro` entry, sessions without `runtime.image` use it; otherwise they get the stock
image and `engine: ssh`. Build and register `agent-micro` as described in [agent-micro.md](agent-micro.md), or pass
`runtime.image: agent-micro` explicitly. To refuse the weaker engine instead of getting it, set `runtime.engine: "agent"`:
the create returns 409 and removes the sandbox when the guest would run on SSH. Per-session `http_rules` on vz need the setup in [7.1](#71-limit-a-host-to-methods-and-paths).

### 2.2 A macOS guest as a session

`runtime.template: "<name>"` starts a session on a clone of a macOS template (`<templates_dir>/<name>/spec.json`, built by
[`scripts/macos-session-template.sh`](../scripts/macos-session-template.sh) from an IPSW). It is the `ssh` engine with these
differences, all reported in `capabilities` (`guest_os: "macos"`):

| | macOS guest | Linux guest |
|---|---|---|
| exec, files, jobs (shell scripts), PTY, screenshot/input | yes | yes |
| memory checkpoint / restore | yes, needs the host's login session **unlocked** | yes |
| `fork` | **no** (409): the saved state is bound to one machine identity | yes on offline vz |
| `network.allow` / `offline`, `http_rules`, `filesystem` confinement, `runtime.volumes` | **refused** (400): no in-guest forwarder, no agent, and no NIC means no SSH | yes |
| default workspace | `/Users/<provision_username>/workspace` (the system volume is read-only) | `/workspace` |
| concurrent guests | 2 per host (Apple's limit); a third create is 409 | host memory |

A macOS session is **unrestricted on the network**: use it for work you would trust on your own Mac's NAT, not for untrusted code.

## 3. Choosing a policy

Pick the strictest policy that still lets the task finish. Policies are fixed at create; to change one, create a new
session (a fork child inherits its parent's).

### 3.1 Network

| You want | Flags | Result |
|---|---|---|
| No network at all | `--policy offline` | no NIC; also what makes a `vz` session forkable |
| Package registries and GitHub only | `--policy developer-safe` | a fixed allow-list (see the reference) |
| Exactly these hosts | `--allow api.example.com --allow '*.pypi.org'` | everything else refused |
| Unrestricted | `--policy open` | do this only for throwaway work |

REST equivalent:

```json
{"policy": {"network": {"default": "deny", "allow": ["github.com", "api.github.com"]}}}
```

### 3.2 Commands (a guardrail)

```bash
fluxctl agent run codex --policy offline \
  --cmd-allow 'git *' --cmd-allow 'cargo *' --cmd-allow 'ls*' \
  --cmd-deny 'git push*'
```

A command is split on `; && || | &` and newlines, and every piece is checked. A matching deny refuses the whole
command; with any allow rule, every piece must match one. Refusals are `403`, ledgered as `exec.refused`. A shell can
still build a command the string never shows (`sh -c "$(...)"`, a script the agent wrote), which is why this is not a
boundary. Use it to keep an agent on rails, and add section 6's approvals for the few commands that matter.

### 3.3 Filesystem confinement (Linux guests)

```bash
fluxctl agent run claude-code --policy developer-safe \
  --confine --fs-write /data/out --fs-read /srv/datasets
```

Every command, job, file operation and git call then runs under Landlock + seccomp: system directories read-only, `/tmp`
and the workspace writable, plus the paths you add. **Create probes this**; if the guest cannot enforce it (a macOS
guest, a procbox sandbox) the create fails with `400` and the reason. Add `--confine-best-effort` to create the session
anyway and read `capabilities.confinement` (`false` means it is **not** confined).

Check it worked:

```bash
fluxctl agent get $S | jq .capabilities.confinement                # true
fluxctl agent exec $S -- 'touch /etc/x'                            # fails: not writable
fluxctl agent exec $S -- 'touch /workspace/x && echo ok'           # ok
```

### 3.4 Everything at once, as REST

```bash
curl -sX POST $FLUXVM_URL/v1/agents/sessions -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{
  "agent": "claude-code",
  "workspace": {"git": "https://github.com/acme/app.git", "require_ready": true},
  "runtime": {"cpus": 4, "memory_mb": 8192},
  "policy": {
    "network": {"default": "deny", "allow": [
      "pypi.org",
      {"host": "github.com", "methods": ["GET", "HEAD"], "paths": ["/acme/*"]}
    ]},
    "filesystem": {"write": ["/data/out"]},
    "process": {"deny": ["rm -rf /*"], "ask": ["deploy *", "git push*"]}
  },
  "budget": {"max_execs": 500, "max_compute_seconds": 3600, "max_children": 8},
  "ttl_seconds": 7200
}'
```

## 4. Letting an agent use a session safely

### 4.1 Mint a session token

```bash
fluxctl agent token $S --ops exec,jobs,files.read,files.write,git --ttl-seconds 1800 --label claude-code
# {"token": "fxst_...", "id": "...", "expires_at": "..."}   <- shown once
```

- Operations: `read`, `exec`, `jobs`, `files.read`, `files.write`, `git`, `checkpoint`, `preview`. Restore, fork, delete,
  token and grant management, approvals and terminals are **never** grantable.
- The token works on this session only (any other id is `404`), expires at most 24 h out and never after the session,
  and is revoked when the session is deleted.
- `fluxctl agent tokens $S` lists live tokens (never the secrets); `fluxctl agent untoken $S <token id>` revokes one.
- **Tokens only limit anything when the daemon has `[auth]` configured.** With no auth (lab mode) every bearer,
  session tokens included, is treated as an anonymous admin, so a token does not narrow what the agent can do.
  Configure `[auth]` with at least one admin token before handing an agent a session token.

Give the agent the token, not your admin credential:

```bash
FLUXVM_URL=http://daemon:7788 FLUXVM_TOKEN=fxst_... fluxctl agent exec $S -- 'cargo test'
```

### 4.2 MCP

`fluxctl mcp serve --allow-write` exposes sessions to MCP clients (Claude Code, Cursor, Codex...). `fluxctl mcp install
claude-code --allow-write` registers it. The server calls the daemon with whatever credential it was started with, so
for an **agent-facing** server start it with a session token (`FLUXVM_TOKEN=fxst_...`), not the admin token. That is
what makes `session_approve` refuse an agent's own approval.

| Task | Tools |
|---|---|
| lifecycle | `session_create`, `session_checkpoint`, `session_restore`, `session_fork`, `session_rollout`, `session_evaluate`, `session_promote`, `session_delete` |
| run | `session_exec`, `session_job_start`, `session_job_read`, `session_job_stdin`, `session_job_kill`, `session_jobs` |
| files, git | `session_files`, `session_file_read`, `session_file_write`, `session_git` |
| policy | `session_approvals`, `session_approve`, `session_grant`, `session_revoke_grants`, `session_token`, `session_probe`, `session_preview` |
| observe | `session_events`, `session_artifacts` (a trajectory export is the CLI's `agent export`) |

Details and a client setup walkthrough are in [mcp.md](mcp.md).

## 5. Running work

### 5.1 `exec`: short, synchronous

```bash
fluxctl agent exec $S --timeout-seconds 120 -- 'cargo build --release'
```

The CLI relays stdout and stderr and exits with the command's exit code. A command that outlives its timeout is killed
**with its whole process group**. Output is capped at 16 MiB per stream (a marker says so) and the call returns promptly
even if a background child kept the pipes open.

### 5.2 Jobs: long-running, streaming

```bash
J=$(fluxctl agent bg $S --stdin --timeout 1800 --max-output-bytes 67108864 -- 'python3 -u repl.py' | jq -r .job)
fluxctl agent stdin $S $J 'print(1+1)
'                                       # a newline is not added: include it
fluxctl agent stdin $S $J --close       # EOF
fluxctl agent logs  $S $J -f            # stdout; stderr goes to your stderr; exits with the job's code
fluxctl agent jobs  $S                  # state: running | exited | killed | signaled | timed_out
fluxctl agent kill  $S $J --signal KILL # TERM by default; the whole process tree gets it
```

How it behaves (the `engine` field on the job says which implementation ran it):

| | `supervisor` (guest agent with `jobs-v2`) | `shell` (older agents) |
|---|---|---|
| stdout and stderr | separate, each resumable | merged |
| exit status | the command's own, even when output exceeds the cap | SIGPIPE at the cap |
| stdin EOF | `agent stdin --close` | not possible |
| timeout | `--timeout` kills the tree | `400`; wrap the command in `timeout` |
| kill | any signal, to the tree | SIGTERM, exit 143 |

For your own client, the same data over REST:

```bash
# poll with a long wait (supervised jobs), resuming from offsets
curl -s "$URL/v1/agents/sessions/$S/jobs/$J?offset=0&stderr_offset=0&wait_ms=20000" -H "authorization: Bearer $TOKEN"

# or Server-Sent Events: output / stderr / exit events; the id is "<stdout offset>:<stderr offset>"
curl -sN "$URL/v1/agents/sessions/$S/jobs/$J/stream" -H "authorization: Bearer $TOKEN"
# reconnect with:  -H 'Last-Event-ID: 1024:0'
```

### Following a session live

`GET /v1/agents/sessions/$S/events` returns the ledger and VM events so far. To watch them as they happen, use the
Server-Sent Events twin, which replays the history and then follows (about every 500 ms):

```bash
curl -sN "$URL/v1/agents/sessions/$S/events/stream" -H "authorization: Bearer $TOKEN"
# only what is newer than a time:  ...events/stream?since=2026-10-17T09:00:00Z
# resume after a drop:             -H 'Last-Event-ID: 12|2026-10-17T09:00:03.5Z'
```

`ledger` events are what the API did, `vm` events what the hypervisor saw, and an `error` event ends the stream (the
session is gone, or the hour limit is up: reconnect with the last id). The id is a cursor: no ledger entry is repeated
or missed, but a VM event with the same timestamp as the last one seen may repeat. At most 64 streams per host. In
TypeScript, `fx.watchSessionEvents(id)` does the reconnecting for you ([client](../typescript/README.md)).

A job lives in the guest agent's memory with its output spooled on the guest's disk. A reboot or agent restart forgets
the table (spools stay); a disk-only restore rolls the spool back.

### 5.3 Files

```bash
fluxctl agent put   $S ./config.toml /workspace/config.toml --mode 644    # up to ~1.5 MB
fluxctl agent cat   $S /workspace/config.toml
fluxctl agent files $S /workspace
fluxctl agent rm-file $S /workspace/tmp -r
```

### 5.4 Large files: resumable and verified

For anything bigger than `put`'s limit (up to 2 GiB):

```bash
fluxctl agent upload $S ./model.bin /workspace/model.bin --mode 644
# upload 7c1f... opened; resume with --resume 7c1f...
#   8388608/734003200 bytes (1%) ...
```

If it is interrupted (laptop closed, network blip), continue where it stopped:

```bash
fluxctl agent upload $S ./model.bin /workspace/model.bin --resume 7c1f...
```

What you get: the file is hashed locally first, sent in 4 MiB chunks, assembled in the guest, **size- and
SHA-256-verified, and only then renamed into place**, so `/workspace/model.bin` is never half written. A wrong size or
checksum writes nothing and says why.

Downloading:

```bash
fluxctl agent download $S /workspace/results.tar ./results.tar          # writes results.tar.part, verifies, renames
fluxctl agent download $S /workspace/results.tar ./results.tar --resume # continue a .part
```

REST, if you are writing your own client:

```bash
U=$(curl -s -X POST $URL/v1/agents/sessions/$S/uploads -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
      -d '{"path":"/workspace/model.bin","size":734003200,"sha256":"<hex>"}' | jq -r .id)
curl -s -X PUT "$URL/v1/agents/sessions/$S/uploads/$U?offset=0" --data-binary @chunk0 -H "authorization: Bearer $TOKEN"
#   -> {"received": 4194304, "remaining": ...}; a wrong offset is 409 with the right one
curl -s -X POST $URL/v1/agents/sessions/$S/uploads/$U/complete -H "authorization: Bearer $TOKEN"
curl -s "$URL/v1/agents/sessions/$S/files/range?path=/workspace/model.bin&offset=0&length=4194304&checksum=true" ...
```

Guest disk use: parts are as large as the file, plus a temporary copy during assembly.

## 6. Approvals: a human in the loop

Use this for the commands you do not want to forbid but do not want to happen unattended: a deploy, a push, a
migration.

### 6.1 Set the rule

```bash
curl -sX POST $URL/v1/agents/sessions -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{
  "agent": "claude-code",
  "policy": {"preset": "developer-safe", "process": {"ask": ["deploy *", "git push*"]}}
}'
```

(There is no CLI flag for `ask` yet; use REST or the MCP `policy` object.)

### 6.2 What the agent sees

```bash
fluxctl agent exec $S -- 'deploy prod'
# POST .../exec: 428 Precondition Required:
#   this command needs an operator's approval (approval_id 0b6e...): approve it with POST .../approvals/0b6e.../approve,
#   then repeat the request with approval_id
```

Nothing ran. The agent has three options: stop and tell the user, **wait** for a decision in the same call

```bash
fluxctl agent exec $S --approval-wait 120 -- 'deploy prod'
```

(the request is held up to 120 s and runs as soon as it is approved; it does not block checkpoints meanwhile), or
retry later with the id once approved:

```bash
fluxctl agent exec $S --approval-id 0b6e... -- 'deploy prod'
```

### 6.3 What the operator does

```bash
fluxctl agent approvals $S                 # who asked for what, state, expiry
fluxctl agent approve   $S 0b6e... --note "ticket OPS-123"
fluxctl agent deny      $S 0b6e... --note "not on a Friday"
```

You need an **admin** credential. A session token gets `403`, which is the point: an agent cannot approve itself.

### 6.4 What an approval means

- It is for **exactly** that command and its parameters (timeout, stdin, caps), under that session policy and that
  session **revision** (a restore, fork or checkpoint changes the revision). Change any of them and it is `403`.
- It is **single use**. Two concurrent retries: one runs, the other is refused.
- It expires after 10 minutes (pending or approved but unused).
- It does **not** pin the workspace's file contents, and it only covers `exec` and job start, not file writes or git
  verbs.
- Everything is ledgered: `approval.requested`, `.approved`, `.denied`, `.consumed`, `.refused`, with who decided.

REST: `GET .../approvals`, `POST .../approvals/{id}/approve|deny {"note": "..."}`.

## 7. Network rules and credentials

### 7.1 Limit a host to methods and paths

```bash
curl -sX POST $URL/v1/agents/sessions -H "authorization: Bearer $ADMIN" -H 'content-type: application/json' -d '{
  "policy": {"network": {"allow": [
    "pypi.org",
    {"host": "github.com", "methods": ["GET", "HEAD"], "paths": ["/acme/*"]}
  ]}}
}'
```

The agent can read `github.com/acme/*` and `pip install`, and cannot `POST` (no pushes, no API writes) or reach other
orgs. `policy.http_rules` in the session record shows the compiled rules (grammar: [http-acl.md](http-acl.md)).

**Read this before relying on it:**

- It is enforced by the host's egress proxy, keyed by the guest's address, **on top of** the host-wide
  `egress_http_rules`. A request must pass both: a session can only narrow.
- It works where the sandbox's traffic reaches that proxy with the sandbox's own address (QEMU/KVM with
  `sandbox.egress_proxy_listen` set, and a network setup that forces or redirects traffic there). It does **not** apply
  on procbox sandboxes, and a guest with a direct route out bypasses it.
- **On a Mac (`vz`)** it works for a session with a host allow-list (`policy.network.allow`, which gives the guest no
  network card) when the daemon runs the proxy on loopback (`[sandbox] egress_proxy_listen = "127.0.0.1:18080"`). The
  runner relays each allowed request to that proxy with a secret the guest never sees, so the session's rules apply to
  plain `http://` requests. The guest reaches the relay at `127.0.0.1:3128` (`curl -x http://127.0.0.1:3128 ...`).
  Without `egress_tls_intercept` an HTTPS `CONNECT` is refused, so a ruled host cannot be reached over TLS. A guest
  on the Mac's NAT is refused with `400` (it has its own route out).
- HTTPS is judged per request only when the proxy intercepts TLS (`egress_tls_intercept`); otherwise a `CONNECT` is
  refused (`501`), so a limited host cannot be reached over TLS at all (fail closed).
- Create **fails with `400` and the reason** when the sandbox cannot be filtered. Set `"network": {"enforcement":
  "best-effort"}` to create it with the host allow-list only, then check `capabilities.http_policy`.

Verify, from inside the session:

```bash
fluxctl agent exec $S -- 'curl -s -o /dev/null -w "%{http_code}\n" https://github.com/acme/app'       # 200
fluxctl agent exec $S -- 'curl -s -o /dev/null -w "%{http_code}\n" -X POST https://github.com/acme/app' # 403
fluxctl agent timeline $S | jq '.[] | select(.kind|startswith("policy"))'
```

### 7.2 Give the session a credential without giving the agent the secret

```bash
# the daemon holds FLUXVM_SECRET_GH_TOKEN="Bearer ghp_..." in its environment
fluxctl agent grant $S --secret-ref gh-token --host api.github.com \
  --method GET --path '/repos/acme/*' --ttl-seconds 1800
fluxctl agent grants $S                 # id, hosts, methods, paths, expiry; never the value
fluxctl agent ungrant $S [<grant id>]   # one, or all
```

The agent's `curl https://api.github.com/repos/acme/app` arrives with an `Authorization` header the proxy added; the
agent never sees the token and cannot exfiltrate it. A request outside the grant's method or path (or with a
non-canonical path: `..`, `//`, `%`) goes through **without** the credential.

Rules: operator only; expiry is capped at the session's; **not inherited** by fork children (grant each child
explicitly); re-bound on restore, and revoked if the restored address belongs to another sandbox
(`grants_rebound: false`, `grant.revoked` in the ledger).

### 7.3 After a restore or a fork

HTTP rules follow the sandbox: restore re-binds them to its current address, and each fork child gets its own. If an
address is already bound to another sandbox's rules, the session becomes `http_unbound` and refuses `exec`, jobs and
terminals with `409` until you recreate it. That is deliberate: the alternative is running with a policy that is not
applied.

## 8. Budgets

```bash
fluxctl agent run codex --policy developer-safe \
  --max-execs 300 --max-compute-seconds 3600 --max-jobs 10 --max-concurrent-jobs 2 \
  --max-children 8 --max-output-bytes 1048576 --max-workspace-bytes 5368709120
```

| Limit | Counts | Refusal |
|---|---|---|
| `max_execs` | commands | `403`, with used and limit |
| `max_exec_seconds` | summed command wall time (a command gets at most what is left as its timeout) | `403` |
| `max_compute_seconds` | commands **plus** background jobs | `403` at the next start |
| `max_jobs`, `max_concurrent_jobs` | jobs started, running at once | `403` |
| `max_children` | fork children (rollouts and `evaluate` spend it) | `403` |
| `max_output_bytes` | per stream: what `exec` returns and a job retains | output is **cut**, `output_truncated: true`; never refused |
| `max_workspace_bytes` | `du` of the workspace at command/job start | `403` when over; growth during a run is not stopped |

`fluxctl agent get $S | jq .usage` shows what was used. A fork child starts with what is left. Budgets are checked when
work *starts*: a running job is not killed when the compute budget runs out, so give jobs their own `--timeout`.
There is no token budget (the daemon does not see model usage) and no process-count limit.

## 9. Checkpoint, fork, evaluate, promote

The point of sessions: try things, keep what works, roll back what does not.

> **Fork speed on a Mac.** A vz fork clones the disk and saved state with `clonefile(2)` (APFS copy-on-write), so a
> fork of two children took about 2 s and used almost no disk (measured: 20 files cloned, 0 copied). On a volume that
> cannot clone (not APFS, or the state directory on another volume) the files are fully copied: slower and real disk.
> The fork response says which: `files_cloned` and `files_copied` (also in the `fork` ledger entry), and the daemon
> logs a warning per copied file.
>
> **Warm starts without a pool.** Measured on an M-series Mac with `agent-micro`, offline policy: a cold session create
> took about 84 s, a memory checkpoint 0.5 s, a restore 3.8 s, and a fork 1.4 s for one child and 2.8 s for four. To
> skip the cold boot, create one prepared session (tools installed, repo cloned), checkpoint it, and fork it per task:
> each child is a running guest in about 1.5 s. A separate pre-restored pool would add idle RAM for little gain over
> that, so none is built. Fork needs a session with no NIC of its own (`offline` or a host allow-list).

### 9.1 Checkpoint and restore

```bash
fluxctl agent checkpoint $S --tag before-refactor            # memory + disk
fluxctl agent checkpoint $S --tag disk-only --disk-only      # much smaller; restore cold-boots
fluxctl agent restore $S before-refactor
```

A restore rolls back **everything**: processes, files, volumes. Jobs started after the checkpoint are gone. Add
`If-Match: <revision>` on the REST calls if two actors might race.

### 9.2 Try N approaches in parallel (`rollout`)

```bash
fluxctl agent rollout $S --count 4 -- 'cd /workspace && ./try.sh $RANDOM && cargo test'
```

Forks four children, runs the command in each, prints every result, deletes the children unless `--keep`.

### 9.3 Let the daemon pick a winner (`evaluate`)

```bash
fluxctl agent evaluate $S \
  --candidate 'cd /workspace && patch -p1 < /tmp/fix-a.diff' \
  --candidate 'cd /workspace && patch -p1 < /tmp/fix-b.diff' \
  --candidate 'cd /workspace && sed -i s/old/new/ src/lib.rs' \
  --test 'cd /workspace && cargo test' --keep-all
```

Each candidate runs in its own child, then `--test` runs there; results are scored (passed the test 100, ran but failed
the test 10, errored 0) and ranked. The winner is kept and the losers deleted (`--keep-all` keeps them). The output
names the winner session.

### 9.4 Take the winner's changes back (`promote`)

```bash
fluxctl agent promote <winner> --dry-run                     # a plan: files to add/modify/delete, conflicts, plan_id
fluxctl agent promote <winner> --plan-id <plan_id>           # apply exactly that plan
fluxctl agent promote <winner> --plan-id <id> --skip-conflicts --path /workspace/src
```

Promotion is a three-way merge of the child's workspace changes into the parent using the fork-time baseline, so it
refuses conflicting files unless you say `--skip-conflicts`. It takes a parent checkpoint (`pre-promote-*`) first: if
the result is wrong, `restore` it. Needs `sha256sum` in the guest (`agent probe`). Workspace files only: a child's
processes and memory are not merged.

## 10. Previews and browsers

```bash
fluxctl agent bg $S -- 'cd /workspace && npm run dev -- --host 0.0.0.0 --port 3000'
fluxctl agent expose $S 3000 --ttl 1800        # prints an expiring URL that reaches port 3000 and nothing else
fluxctl agent previews $S
fluxctl agent unexpose $S <token>
```

For a browser (CDP) the preview route bridges WebSocket upgrades and rewrites the DevTools discovery URLs (`/json`,
`/json/list`, `/json/version`) to point back at the preview, so Playwright or Puppeteer can `connectOverCDP` to the
preview URL. There is no prebuilt browser image: install Chromium in your image or session, and run
`agent probe $S` to see whether a browser and Playwright are present. See [Browser](agent-sessions.md#browser-chrome-devtools-protocol)
in the reference for the recipe.

## 11. Operating it

### 11.1 Where things are

| What | Where (under the daemon's `state_dir`) |
|---|---|
| session record | `agent-sessions/<session>.json` |
| ledger (audit; kept after delete) | `agent-sessions/<session>.ledger.jsonl` |
| tokens (hashed) | `agent-session-tokens/<session>.json` |
| approvals | `agent-session-approvals/<session>.json` |
| job metadata, usage, uploads | `agent-session-jobs/`, `agent-session-usage/`, `agent-session-uploads/` |
| in-flight lifecycle marker | `agent-session-ops/<session>.json` (exists only while an operation runs) |
| previews | `agent-previews/<token>.json` |

### 11.2 Read what happened

```bash
fluxctl agent timeline $S                 # ledger + VM events
fluxctl agent export   $S > run.jsonl     # trajectory: a header line, then one line per step
```

Ledger kinds worth alerting on: `exec.refused`, `job.refused`, `approval.requested`, `approval.refused`,
`policy.http_unenforced`, `http_policy.unbound`, `grant.revoked`, `lifecycle.interrupted`, `session.expire`,
`session.cleanup`. Secrets and file contents are never written to it.

### 11.3 Cleanup and restarts

- A background **sweeper** (first pass 30 s after start, then every 60 s) marks expired sessions `expired` and removes
  what ended sessions left: tokens, grants, previews, approvals, job metadata, usage, uploads and HTTP rules. The record
  and ledger stay. `fluxctl agent rm` runs the same cleanup immediately.
- If the daemon dies **during** a checkpoint, restore, fork or delete, the next `agent get` shows
  `interrupted_op: {op, started_at}` and the ledger gets `lifecycle.interrupted`. It is a report, not a repair: inspect
  the sandbox, and `restore` a known checkpoint if in doubt.
- The lifecycle gate (which refuses a second restore while one runs) is in daemon memory: one daemon per state
  directory.
- Credential grants are in memory only: a daemon restart drops them; re-grant.

## 12. Fleet node agent

If you run several hosts behind `fluxvm-agent central`, each host's `fluxvm-agent node` heartbeats to it.

```bash
fluxvm-agent node --name mac-01 --central http://central-a:7799,http://central-b:7799 \
  --fluxvm-url http://127.0.0.1:7788 --advertise-url http://mac-01:7788 --token $FLEET_TOKEN --interval-secs 10
```

What to expect:

- Requests time out (3 s connect, 10 s total); after failures the next beat is delayed with jitter, doubling per failure
  up to 8x the interval.
- If the node **cannot reach its own daemon** it still heartbeats and says so: `GET /fleet/nodes` shows
  `daemon_reachable: false` with the error, and placement skips it. It is not "stale" and not "zero VMs". That tells you
  to look at `fluxctl serve` on that host, not at the network.
- Free CPU and memory come from the **sum of the active VMs' requested sizes**, which central uses in place of its
  per-VM estimate. Stopped VMs hold no capacity. A VM record without a size counts as 2 vCPU / 2 GiB.

```bash
fluxctl fleet nodes | jq '.items[] | {name, healthy, daemon_reachable, free_vcpus, free_memory_mib, allocated}'
```

## 13. Troubleshooting

| You see | Meaning | Do |
|---|---|---|
| `400 the guest cannot enforce the filesystem policy ...` | macOS guest, procbox sandbox, or no agent | use a Linux guest, or `--confine-best-effort` and check `capabilities.confinement` |
| `400 per-sandbox HTTP rules cannot be enforced: ...` | vz/procbox, proxy not enabled, or guest address unknown | enable `sandbox.egress_proxy_listen` on a KVM host, or `network.enforcement: best-effort` |
| `403 refused by the session's process policy` | a `deny` or missing `allow` match | adjust the rule, or ask the operator |
| `428 this command needs an operator's approval` | a `process.ask` match | section 6 |
| `403 this approval was granted for a different command, parameters, policy or session state` | command, timeout or revision changed since approval | request a new approval |
| `403 ... budget is used up (n of m)` | a budget limit | create a new session or raise the budget |
| `409 a restore is already in progress` / `... command(s) are still running` | the lifecycle gate | wait, or end the commands |
| `409 ... HTTP policy is not bound ...` | restore/fork landed on an address owned by another sandbox's rules | delete and recreate the session |
| `412` on a lifecycle call | `If-Match` revision is stale | `agent get`, then retry with the new revision |
| `409 upload ... send the next chunk with offset=N` | a chunk was repeated or skipped | continue at `N` (the CLI does) |
| `409 upload ... checksum does not match` | corrupted in transit or wrong `sha256` | restart the upload |
| `404` on a session you just created | another tenant's token, or a session token for a different session | use the right credential |
| job `engine: shell` and `timeout_seconds` is `400` | older guest agent | upgrade the agent, or wrap in `timeout` |
| `interrupted_op` on `agent get` | the daemon died mid-operation | inspect, restore a checkpoint if needed |

## 14. What has and has not been verified

Be honest with yourself before putting this in front of untrusted agents.

**Run on real hardware earlier** (Apple M4, `vz`, stock `debian-13`): create/exec/checkpoint/restore, fork of an offline
session, files, the shell-script job engine, previews, the PTY terminal, trajectory export.

**Driven through a real guest on a Mac** (Apple M4, `vz`, the `agent-micro` image; `scripts/vz-agent-session-live-test.sh`
runs these 12 checks): the engine the session gets, hardened `exec` (a background child does not hold the command; a
timeout leaves no process), the job supervisor with its exit code, fork cloning, Landlock confinement refusing a write
outside the policy, and per-session HTTP method/path rules through the egress proxy (GET allowed, POST and unlisted hosts
refused). Earlier, on the stock `debian-13` image (SSH engine): approvals end to end, token scopes, upload/download with
checksum and resume, output caps, the exec budget, evaluate and promote, and the sweeper.

**Unit and router tested only, not driven through a real guest or daemon:**

- the Linux-only code paths on a KVM host (Landlock/seccomp through the procbox `spawn` path was first compiled by CI);
- method/path-limited credential grants, and HTTPS rules (these need `egress_tls_intercept` and the CA in the guest);
- budgets beyond the first set and the fleet node agent changes.

**Not built:** a packaged browser image, token budgets, a process-count limit, executable/argv-level command
confinement, and a daemon-restart-proof lifecycle gate.

Treat anything in the second list as "implemented and reviewed, not field-tested", and run its checks from sections 3.3
and 7.1 on your own hosts before relying on it.
