# Host hardening

Controls that protect the FluxVM host and its API even when a credential leaks
or a VMM has an unpatched bug. Each is opt-in.

## Step-up MFA (TOTP) for exec and writes

An `[[auth.tokens]]` entry can be paired with an `[[auth.mfa]]` rule (matched by
token `name`). Requests in scope must also carry a current TOTP code in
`X-Fluxvm-Totp`; a stolen bearer token alone is then not enough to run commands.

```toml
[[auth.tokens]]
token = "REPLACE_ME"
role  = "admin"
name  = "ci"

[[auth.mfa]]
token            = "ci"
totp_secret_file = "/etc/fluxvm/mfa/ci.totp"   # base32, chmod 600
require          = "exec"                      # or "write"
```

| `require` | Needs a code for |
|---|---|
| `exec` (default) | `POST /v1/vms/{id}/agent`, `.../qga/exec`, `.../exec`, agent-session `exec`, `pty-ticket`, and `GET .../pty` |
| `write` | everything in `exec`, plus any request that is not GET/HEAD |

Client: `fluxctl --server URL --server-token T --totp 123456 exec ID -- id`
(or `FLUXVM_TOTP`).

Behavior:
- RFC 6238, SHA-1, 6 digits, 30 s step, +/-1 step of clock skew.
- Each time-step is accepted once per token; a replayed code gets 401.
- Missing or wrong code: 401 with `WWW-Authenticate: TOTP realm="fluxvm"`.
- 5 wrong codes in 60 s lock that token out (429) until the window passes.
- An unreadable or malformed secret file fails closed.
- The audit log (`fluxvm_audit`) records `mfa=ok|missing|failed|locked|none`.
- Replay and lockout state is in memory: a daemon restart resets it.

Not covered:
- Local `fluxctl` (no `--server`) is not gated, because it already needs host
  access to the state directory.
- Only static tokens are matched. For OIDC and mTLS identities, enforce MFA at
  the IdP or proxy.

Break-glass: with host root, remove the `[[auth.mfa]]` rule (or fix the secret
file) and restart the daemon.

Enrollment: generate a secret with `head -c 20 /dev/urandom | base32`, save it
to the file above, and add it to an authenticator app as
`otpauth://totp/fluxvm:ci?secret=<base32>&issuer=fluxvm`.

## Positive exec allow-list (VMM Guard)

`fluxvm-guard` can restrict a guarded cgroup to a list of approved executables.
Anything else is denied (`enforce`) or logged (`audit`) as action 5,
`EXEC_UNLISTED`, with the same ring-buffer events as the other guard actions.

Matching is by kernel object identity (filesystem device + inode, scoped to the
VM key and policy generation), like the writable-file allow-list. Replacing a
binary with a different file at the same path is therefore not allowed.

Scope: like every guard policy, it applies to the cgroup registered for the
VM (`apply <uuid> <pid>`). `bpf_get_current_cgroup_id()` returns a task's own
cgroup, not its ancestors, so it does not cover the whole host. Register each
cgroup you want covered.

```bash
# 1. Inventory what the workload runs today (executables under dirs, resolved
#    symlinks, plus the interpreter of any #! script).
fluxvm-guard learn-exec --dir /usr/bin --dir /opt/vmm --pid $VMM_PID --out exec.toml
# 2. Review and trim exec.toml, then sign it.
fluxctl catalog keygen                                  # once
fluxvm-guard sign-exec-policy exec.toml ./signing.key   # writes exec.toml.sig
# 3. Apply in audit mode first; watch events; then switch to enforce.
export FLUXVM_GUARD_TRUSTED_KEYS=<base64 public key>
fluxvm-guard apply $VM_UUID $VMM_PID --mode audit --exec-policy exec.toml
fluxvm-guard events $VM_UUID 30
fluxvm-guard apply $VM_UUID $VMM_PID --mode enforce --exec-policy exec.toml
```

Rules:
- `--exec-policy` implies `--allow-exec`: the default deny-all exec flag
  would otherwise win over the allow-list.
- Verification fails closed: no trusted key, no `.sig` file, a tampered file,
  or a signature from another key is an error. `--unsigned` skips it (lab only).
- An empty path list, a relative path, or a non-executable file is rejected.
- Each script exec is a separate check on its interpreter, so interpreters
  must be listed. `learn-exec` adds `#!` interpreters, but a `#!/usr/bin/env x`
  script resolves to `env` only; list the real tool too.
- The `guard_exec` map exists only in a guard object built from this version.
  Loading an exec policy against an older loaded object fails with a clear
  error; existing policies without an exec list keep working.

Status: verified on a BPF-LSM kernel (Ubuntu 26.04, kernels 7.0.0-34 and 7.0.0-38) with
`scripts/test-vmm-guard-exec-patch-host.sh`: in audit mode an unlisted exec is
logged and allowed; in enforce mode it returns EPERM, listed binaries still run,
and the default deny-all exec still blocks everything.

## Signed virtual-patch rule packs (VMM Guard)

A rule pack is a time-limited mitigation for a known-vulnerable host component
(for example a QEMU helper or a library with a published CVE) that you cannot
patch immediately. It blocks specific files or devices until the real fix is
deployed. It is not an exploit-signature engine and does not replace patching.

The rules are exactly what the guard hooks can express:

| `action` | Effect (by kernel object identity) |
|---|---|
| `deny-exec` | executing this file is blocked |
| `deny-open` | opening this file is blocked, including loading it as a library |
| `deny-device` | opening this character/block device is blocked |

```toml
version     = 1
id          = "FLUX-2026-0001"
cve         = ["CVE-2026-1234"]
description = "Disable the vulnerable vdpa path until QEMU is upgraded"
valid_until = "2026-12-31T00:00:00Z"      # required, RFC 3339

[[rules]]
action = "deny-device"
path   = "/dev/vhost-vdpa"

[[rules]]
action = "deny-exec"
path   = "/usr/libexec/qemu-bridge-helper"
```

```bash
fluxvm-guard sign-rule-pack pack.toml ./signing.key            # writes pack.toml.sig
fluxvm-guard pack verify pack.toml --trusted-key "$PUBKEY"     # signature, schema, expiry
fluxvm-guard apply $VM_UUID $VMM_PID --mode audit \
    --rule-pack pack.toml --trusted-key "$PUBKEY"
fluxvm-guard status $VM_UUID    # lists packs; `expired_packs` names stale ones
```

Behavior:
- Verification fails closed, as for exec policies. Any bad signature, an
  expired pack, a duplicate id, a missing target path, or an unsupported
  schema aborts the whole apply; a partial set is never loaded.
- Deny rules always win over the exec and device allow-lists.
- Packs are loaded with the policy generation, so changing packs means
  re-running `apply` (the old generation is retired as before).
- `valid_until` is checked when you apply. A loaded pack keeps blocking after
  it expires; `status` reports it in `expired_packs` so you can re-apply
  without it. There is no timer that removes it from the kernel.
- File handles that were already open when a pack is applied are not closed.
- Version matching is up to the pack author: rules name concrete paths on the
  host, so list only paths that exist on the hosts the pack targets.
- Inspecting: `GET /v1/vms/{id}/guard` (read-only) returns the guard mode,
  exec allow-list, applied packs and `expired_packs`; `fluxctl --server URL
  guard status <vm>` calls it. Locally, `fluxctl guard ...` passes its
  arguments to `fluxvm-guard`. Applying packs stays a host-local, root
  operation (`fluxvm-guard apply --rule-pack`), because it writes BPF maps.

Status: verified on the same BPF-LSM kernels (7.0.0-34 and 7.0.0-38) by
`scripts/test-vmm-guard-exec-patch-host.sh`. With a pack applied in enforce
mode, a patched exec, a patched file open and a patched device open each return
EPERM and are counted; unpatched controls are unaffected; audit mode logs and
allows; a pack-less run blocks nothing extra.

## Running the host test

`scripts/test-vmm-guard-exec-patch-host.sh` needs root, `bpf` in
`/sys/kernel/security/lsm`, libbpf-dev, clang, bpftool, cgroup v2, `fluxctl`
and a built `fluxvm-guard`. BPF-LSM is usually off by default: add `bpf` to the
kernel command line, keeping the existing list, e.g.
`lsm=landlock,lockdown,yama,integrity,apparmor,bpf`, and reboot. Test inputs
use compiled binaries because distro binaries can share an inode (multicall
coreutils), and the guard matches by inode.

Device numbers: the guard compares kernel-internal `dev_t` values
(`(major << 20) | minor`). An earlier version used the compact userspace
encoding, so every device and file allow-list key (exec, writable-file and
device) silently failed to match; this is fixed and covered by the host test.

## Privileged CI

The `privileged` job in `.github/workflows/vmm-guard-qos.yml` runs the exec
allow-list and rule-pack test on a self-hosted Linux runner with BPF-LSM. It
does not run for pull requests, needs the `FLUXVM_EBPF_PRIVILEGED_CI=1`
repository variable, and waits for approval of the `privileged-ebpf`
environment. The runner user may `sudo` only
`scripts/ci-privileged-guard-test.sh` (installed as
`/usr/local/sbin/fluxvm-ci-guard-test`). That script copies the checkout to a
root-owned directory and runs the test from there. It still runs the code of the
commit under test as root, so keep the environment approval on.
