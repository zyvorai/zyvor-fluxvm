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

All routes need an `admin` token (or a [session token](#session-tokens) for the operations it was minted with) and are tenant-scoped (a tenant token only ever sees its own sessions; another
tenant's id answers `404`, not `403`).

| Route | What it does |
|---|---|
| `POST /v1/agents/sessions` | Create: sandbox + workspace + compiled policy. `201` with the session record. |
| `GET /v1/agents/sessions` | List the caller's sessions, oldest first. |
| `GET /v1/agents/sessions/{id}` | One session: backend, capabilities, policy, workspace, checkpoints, children. |
| `DELETE /v1/agents/sessions/{id}` | Delete the session and its sandbox. Idempotent (`204`). |
| `POST /v1/agents/sessions/{id}/exec` | `{command, timeout_seconds?}` through the sandbox's guest path. Recorded in the ledger. |
| `POST /v1/agents/sessions/{id}/tokens` | `{ops, ttl_seconds?, label?}`: mint a [session token](#session-tokens). `GET` lists live ones (no secrets); `DELETE .../tokens/{token}` revokes. Admin only. |
| `POST /v1/agents/sessions/{id}/promote` | `{dry_run?, plan_id?, skip_conflicts?, paths?}` on a fork child: apply its workspace changes to its parent. See [Promoting a fork](#promoting-a-fork). Admin only. |
| `POST /v1/agents/sessions/{id}/checkpoint` | `{tag?, memory?}`; `memory:false` is disk-only. |
| `POST /v1/agents/sessions/{id}/restore` | `{tag}`; only tags this session took. |
| `POST /v1/agents/sessions/{id}/fork` | `{count, identity?}` (1-32) into child sessions that start from the current state. `identity` is `best-effort` (default), `unique` or `preserve`. **`409` with the reason when the session cannot fork; nothing is created.** |
| `GET /v1/agents/sessions/{id}/events` | `?since=` RFC 3339. `{ledger, vm_events}`. |
| `GET /v1/agents/sessions/{id}/artifacts` | Workspace changes since the baseline, checkpoints, children, counters. |

`fluxctl agent rollout <session> --count N -- <cmd>` and MCP `session_rollout` are not a separate endpoint: they call
fork, then exec in every child in parallel, then delete the children (`--keep` keeps them), so they follow fork's rules.

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
  the sandbox; that is shown, not saved. There is no background sweep: reconciliation happens on access, so it also
  covers a daemon restart.

Not done: the gate and replay store are per daemon; reads (`files`, `git status`, `job read`) are not gated.

### Session tokens

An admin mints a bearer token that works on **one session** and only for the operations it names, so an agent can
run commands and read its workspace without VM-management access.

```json
POST /v1/agents/sessions/{id}/tokens
{"ops": ["exec", "files.read", "files.write", "git", "jobs"], "ttl_seconds": 3600, "label": "claude-code"}
```

The response carries `token` (`fxst_…`) **once**; the store keeps only its SHA-256. Grantable operations: `read`
(session record, events, trajectory, artifacts, preview list), `exec`, `jobs`, `files.read`, `files.write`, `git`,
`checkpoint`, `preview`. Not grantable, so admin-only: create, list, delete, restore, fork, terminals and `ssh`, and
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

### Policy

A policy is compiled into fields the sandbox already enforces. The response reports exactly what was compiled.

| Input | Compiles to | Enforced by |
|---|---|---|
| `preset: "offline"` | `offline: true` (no network card) | vz: the VM has no NIC |
| `preset: "developer-safe"` | `allow_hosts`: `github.com`, `*.github.com`, `*.githubusercontent.com`, `crates.io`, `*.crates.io`, `static.crates.io`, `pypi.org`, `files.pythonhosted.org`, `registry.npmjs.org`, `proxy.golang.org`, `sum.golang.org` | vz: the host egress proxy (ports 80 and 443) |
| `preset: "open"` or no policy | nothing | n/a |
| `network.default: "deny"` + `allow: [host…]` | `allow_hosts` (merged, de-duplicated); `offline` when nothing is allowed | as above |
| `network.allow` entries with `methods` or `paths` | **refused (`400`)** | not supported yet; HTTP method/path rules exist for the QEMU/KVM egress proxy, not through this API |
| `process: {allow: [glob…], deny: [glob…]}` | `policy.process` | the API, on `exec` and job start (`403`, ledgered as `exec.refused` / `job.refused`). A guardrail on the command string, **not a security boundary**: see below |
| `process.ask` | **refused (`400`)** | there is no per-command approval flow yet |
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
message; a raw REST client still sees the reset. There is no chunked upload yet. Reads are whole files. List
entries come from GNU `find`, so a guest without it (busybox) is not supported.

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
```

- **MCP** (`fluxctl mcp serve --allow-write`): `session_create`, `session_exec`, `session_checkpoint`, `session_restore`,
  `session_fork`, `session_promote`, `session_token`, `session_rollout`, `session_events`, `session_artifacts`,
  `session_delete`, plus `sandbox_speculate`, `sandbox_changesets`
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
| Promote a child's *non-file* state (memory, processes, volumes) | promotion copies workspace files only; a child's running processes and memory are not merged |
| Lazy memory restore (sub-second claim/fork) | a restore engine change, not an API change; not started |
| Workspace overlay mode, read-only mounts | `mode: "overlay"` is refused; clone is the only writable mode |
| Process policy as a security boundary | `process` rules are still string globs; filesystem confinement is wired, command confinement beyond it is not |
| HTTP method/path rules through the session policy | the egress proxy supports them on QEMU/KVM; not exposed here |
| Streaming exec output | `exec` is still buffered; jobs stream over SSE (long-polled from the guest supervisor, or polled every 400 ms on older agents) and the terminal (`shell`) streams |
| Replay | the ledger is a log, not a recording |
| Preview URLs: large or streamed responses | WebSocket upgrades are bridged (run with Chrome DevTools); large and streamed bodies were not run through the preview route |
| Browser / computer-use profile | a CDP recipe works on the stock image (see Browser above); no prebuilt browser image, no Playwright client run, no computer-use display profile |
| Secretless credentials | the credential broker (`docs/credential-broker.md`) is per sandbox; not bound to sessions |
