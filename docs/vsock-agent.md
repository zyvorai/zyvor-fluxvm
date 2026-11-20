# The guest agent on the vz backend

`fluxvm-guest-agent` (`crates/fluxvm-guest-agent`) is the in-guest agent the Linux backends already use for `exec`, file transfer, the
interactive shell and identity reset. It also runs in Linux guests on the `vz` backend, where it replaces SSH for sandbox commands and
files: no SSH handshake per command, and it works the same with or without a network card.

## How the daemon reaches it

```
daemon ── unix socket ──> fluxvm-vz-runner ── VZVirtioSocketDevice.connect(toPort: 17777) ──> fluxvm-guest-agent
         "CONNECT 17777\n"  <── "OK 17777\n"  then one JSON line each way
```

The runner already serves a Firecracker-style vsock proxy on `vm.vsock_socket` (it is how offline sandboxes reach the guest's sshd on
port 22). `fluxvm_vsock_client::call` dials it for `Vz` exactly as it does for Firecracker and Cloud Hypervisor, so the daemon has one
agent client for every backend. The daemon never links Virtualization.framework.

## Protocol

The shared crate `fluxvm-guest-protocol`: one JSON object per line, one request and one response per connection.

```json
{"token": "…", "op": "exec", "command": "uname -a", "timeout_seconds": 30}
{"result": "exec", "exit_code": 0, "stdout": "Linux …\n", "stderr": ""}

{"token": "…", "op": "put-file", "path": "/tmp/a", "content_base64": "aGk=", "mode": 420}
{"result": "file-written"}

{"token": "…", "op": "get-file", "path": "/tmp/a"}
{"result": "file-content", "content_base64": "aGk=", "mode": 420}

{"token": "…", "op": "ping"}
{"result": "pong"}
```

The operations (`op`, kebab-case) are `ping`, `exec`, `put-file`, `get-file`, `open-shell` (an interactive PTY; the connection then carries frames),
`shutdown`, `capabilities`, the supervised-job set `job-start`, `job-read`, `job-write`, `job-signal`, `job-info`, `job-list`, `job-reap`, and
`reset-identity` (see [agent-sessions.md](agent-sessions.md) for jobs). Results are `pong`, `exec`, `file-written`, `file-content`, `shell-opened`,
`shutting-down`, `capabilities`, `job-started`, `job-output`, `job`, `jobs`, `job-ack`, `identity-reset` and `error`. The default port is 17777.
Errors are `{"result": "error", "message": "…"}`. Files are capped at 64 MiB per transfer ([Request limits](#request-limits)).

### Request limits

The token is part of the request line, so the agent reads that line under limits before it knows who is calling (0.4.21; before
that the read had no size limit and no timeout, so any vsock peer could exhaust guest memory or hold a thread forever):

| Limit | Value | Constant | When it is hit |
|---|---|---|---|
| Pre-auth request line | 64 KiB | `AUTH_LINE_MAX_BYTES` | a line that ends inside it is parsed and authenticated as usual |
| Idle time per read, before the line is complete | 10 s | `READ_IDLE_TIMEOUT` | the connection is closed with no response |
| Total time to read the pre-auth line | 15 s | `AUTH_READ_DEADLINE` | the same: a peer that trickles bytes is dropped |
| Authenticated request line | 64 MiB of file as base64 plus 64 KiB of framing, about 85 MiB | `ENVELOPE_LINE_MAX_BYTES` | refused with `request is larger than the N-byte limit` |
| Time to read the rest of an authenticated line | 5 min | `BODY_READ_DEADLINE` | the connection is closed |
| File transfer (`put-file`, `get-file`) | 64 MiB | `MAX_FILE_TRANSFER_BYTES` (`fluxvm-guest-protocol`) | an error response |

- A line that reaches 64 KiB without a newline (a `put-file`) is read further only if it starts with `{"token":"…"` (whitespace allowed;
  the token must be the first member, which is how the host client writes it) and that token is valid. Otherwise the agent answers
  `unauthorized: missing or incorrect token` and closes the connection without reading more.
- A request that is complete but cannot be parsed gets a generic `unrecognised request` error before authentication: no echo of the request, no
  version, no list of operations.
- After the request line, the receive timeout is cleared, so an interactive shell's idle time is not limited.
- `get-file` reads regular files only (symlinks are followed to their target). A directory, FIFO, socket or device node such as `/dev/zero`
  is refused with `<path> is not a regular file`; before 0.4.21 a FIFO hung the agent and `/dev/zero` read until memory ran out. A file larger
  than 64 MiB is refused, and the read stops one byte past the limit, so a file that grows after the size check (or a `/proc` file that
  reports size 0) cannot exhaust memory either.
- `exec` timeouts (30 s when `timeout_seconds` is omitted), background job timeouts and procbox timeouts are clamped to ten years
  (`MAX_TIMEOUT_SECS`). A near-`u64::MAX` value used to overflow the deadline and panic the thread that enforces it, leaving a job `running` forever.

These limits protect the agent from an unauthenticated peer on the vsock; they do not replace the token (a guest agent with no token
provisioned refuses every request unless `FLUXVM_AGENT_ALLOW_INSECURE=1` is set in its environment). Covered by unit tests in
`crates/fluxvm-guest-agent/src/main.rs` (a line with no newline, a trickling peer, an oversized line with a wrong token, an authenticated
upload past the pre-auth cap, the envelope cap, `get-file` on a FIFO, a huge timeout); not measured on a real guest.

`exec` may carry a `process` object instead of relying on `command`: the agent then runs `argv` directly with `execve`, with no
`/bin/sh -c`, so it works in images without a shell. `env` adds `KEY=value` entries (or replaces the environment with
`clean_env`), `cwd` sets the directory, and `user` (`{"uid", "gid"}`) drops privileges. Older agents ignore the field, so only send it
to an agent known to support it. Over REST: `POST /v1/sandboxes/{id}/process {"process": {"argv": [...]}}`.

```json
{"token": "…", "op": "exec", "command": "", "process": {"argv": ["python3", "-c", "print(6*7)"], "cwd": "/tmp"}}
```

## Turning it on

A VM opts in with `"agent": {"enabled": true}` (port 17777 by default). The daemon generates a token for it. On Linux hosts the
token is written into the guest's disk before boot; guestkit cannot open a disk on macOS, so on `vz` the token travels in cloud-init
instead (`write_files`, `/etc/fluxvm-guest-agent.token`, mode 0600). The agent refuses every request without a matching token.

The guest image must contain the agent and its unit: `/usr/local/bin/fluxvm-guest-agent` (a static aarch64 build) and
[systemd/fluxvm-guest-agent.service](../systemd/fluxvm-guest-agent.service), ordered after `cloud-init.service` so the token is in
place when the agent starts. The [agent-micro](agent-micro.md) image is built that way. Stock cloud images (`debian-13`) do not have
it, so sandboxes on them keep using SSH.

`agent.enabled` is refused for macOS guests: the agent is a Linux binary.

In a [container sandbox](oci-sandboxes.md) there is no cloud-init and no systemd: `fluxvm-oci-init` (PID 1) starts the agent from
the initramfs with `--token-file` pointing at the token on the read-only `fluxvm-meta` share. With `FLUXVM_POWEROFF_VIA_INIT` set,
the agent's `shutdown` signals PID 1 (`SIGUSR2`) instead of running `shutdown`, which such images do not have.

## Sandboxes

See [vsock-proxy.md](vsock-proxy.md) for how sandbox `exec` and file operations pick the agent and fall back to SSH. Container
sandboxes use the agent only.
