# Install FluxVM on a Mac with Homebrew

The quickest way to a working FluxVM on Apple silicon: one `brew install`, the daemon as a background service, and
an AI agent client wired to it over MCP. Everything on this page was run on an Apple M4 mini (macOS 27.2) against the
published `v0.4.4` package, from the tap, unless a line says otherwise. Install, service, upgrade (from 0.4.1) and uninstall
were run for real; a real model was run through the local-model relay on the agent-sandbox demo branch.

## Before you start

- Apple silicon, macOS 14 (Sonoma) or newer. Homebrew installs `hivex` for you (guest image inspection).
- A Mac that is logged in and unlocked when you restore snapshots or use warm slots; a headless Mac needs a dedicated
  auto-login user (see [Unattended hosts](agent-density.md#unattended-hosts)).
- Disk for VM images: the first Debian image is a few GB under `~/Library/Application Support/FluxVM`.

## 1. Install

```bash
brew install zyvorai/tap/fluxvm
fluxctl --version                  # fluxctl 0.4.4
brew test zyvorai/tap/fluxvm       # --version, the five binaries, and the runner's virtualization entitlement
```

The formula installs the five binaries into `bin/` and the example config into `share/fluxvm`:

| Binary | Role |
|---|---|
| `fluxctl` | The CLI, the daemon (`fluxctl serve`) and the MCP server (`fluxctl mcp serve`) |
| `fluxvm-vz-runner` | One per VM; carries the `com.apple.security.virtualization` entitlement |
| `fluxvm-vz-switch` | Private networks between guests |
| `fluxvm-usernet` | The userspace gateway behind `apple.usernet` (egress policy, forks) |
| `fluxvm-blockd` | The disk service (FluxDisk) |

**Not in the package:** the USB passthrough helper (the hosted build SDK has no `AccessoryAccess` framework) and the
vmnet and bridged-networking paths, which need a Developer ID signed build with Apple's provisioning profiles
([macos-signing.md](macos-signing.md)). The binaries are ad hoc signed, which is all the Virtualization.framework
features in [macos.md](macos.md) need.

## 2. Start the daemon

```bash
brew services start zyvorai/tap/fluxvm      # a LaunchAgent: starts at login, restarted if it exits
brew services info zyvorai/tap/fluxvm       # Running: true
curl -s http://127.0.0.1:7788/healthz       # {"ok":true}
tail -f /opt/homebrew/var/log/fluxvm.log
```

The API listens on `127.0.0.1:7788`. Use `brew services` for the background service, not `fluxctl service install`:
both register a LaunchAgent for the same daemon on port 7788, and Homebrew's is the one `brew upgrade` manages. Tested
both ways: `fluxctl service install` writes `~/Library/LaunchAgents/dev.zyvor.fluxvm.plist`, starts it and serves
`/healthz`; starting `brew services` while it runs makes the Homebrew one exit with `Address already in use`;
`fluxctl service uninstall` removes the plist and stops the daemon. Pick one: `fluxctl service install` now refuses
(before writing anything) when it runs from a Homebrew install, when a `brew services` plist exists, or when something
already listens on the configured address, and points you at `brew services start`. `--force` overrides it; a second
daemon needs its own `listen` and `state_dir` in the `--config` file.

> `/readyz` answers `503` on a Mac in 0.4.4: it still asked for the Linux eBPF dataplane. It is fixed in the release
> after 0.4.4. Use `/healthz` for liveness.

## 3. Run something

```bash
fluxctl run                          # a throwaway Debian 13 VM and a shell
fluxctl run -- uname -a              # one command, then the VM is gone
```

The first VM on a fresh state directory takes longer (about a minute here) while the image is prepared; later
sandboxes are ready in a few seconds, and in about a second when they claim a warm slot
([agent-density.md](agent-density.md)).

## 4. Connect an AI agent (MCP)

```bash
fluxctl mcp install claude-code --allow-write     # writes ~/.claude.json; --dry-run prints it first
fluxctl mcp status
```

Other clients: `cursor`, `claude-desktop`, `codex`, `vscode`, `windsurf`, `gemini`; the file each one uses is in
[mcp.md](mcp.md#installing-into-a-client). Restart the client afterwards.

Tested end to end from a fresh `HOME`: the entry is written, then `fluxctl mcp serve` answers the MCP handshake
(`serverInfo.name` `fluxvm`, 31 tools with `--allow-write`) and a real sandbox round trip works:

| Tool call | Result on the M4 |
|---|---|
| `sandbox_create` `{"offline": true}` | a running sandbox with only `lo`, 7.8 s on a fresh state directory |
| `sandbox_exec` `{"vm": id, "command": ...}` | `hello from fluxvm-vm`, `aarch64`, exit 0 |
| `vm_fork` `{"vm": id, "count": 2}` | two running copies |
| `vm_delete` | deleted |

Write tools (`--allow-write`) can create, run commands in and delete VMs: leave them off for a read-only agent. A runnable
agent loop with real timings is in [examples/agent-sandbox-demo](../examples/agent-sandbox-demo/README.md).

> **After `brew upgrade`:** an MCP entry (or `fluxctl service install` plist) written by 0.4.4 or earlier points at the
> versioned path (`/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl`). Homebrew removes the old version on upgrade, and the
> entry then points at nothing: tested, an entry written by 0.4.1 was dead right after upgrading to 0.4.4. Re-run
> `fluxctl mcp install`, or edit the entry to `/opt/homebrew/opt/fluxvm/bin/fluxctl`. Releases after 0.4.4 write the `opt`
> path themselves.

## 5. Update and remove

```bash
brew update && brew upgrade zyvorai/tap/fluxvm
brew services restart zyvorai/tap/fluxvm

brew services stop zyvorai/tap/fluxvm
brew uninstall zyvorai/tap/fluxvm
```

State (VMs, images, snapshots) stays in `~/Library/Application Support/FluxVM`, and the log in
`/opt/homebrew/var/log/fluxvm.log`; remove them yourself if you want them gone.

Both were run for real: 0.4.1 installed from the tap, then `brew upgrade` to 0.4.4 (Homebrew removed 0.4.1, the running
service kept serving until `brew services restart` started the new binary), and `brew uninstall` (formula, `opt` link
and binaries gone, no LaunchAgent or daemon left behind).

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `curl 127.0.0.1:7788/readyz` gives `503` | Expected on 0.4.4 (see above); `/healthz` is the check. |
| `the Apple runner fluxvm-vz-runner was not found (looked at: …/target/debug/…)` | A VM record from a development build points at a path that no longer exists. Delete the VM (`fluxctl delete <id>`) and create it again. |
| Snapshot restore or a warm slot fails | The login session is locked or absent. Unlock it, or set up the dedicated auto-login user. |
| `brew services` shows `Running: false` and the log says `Address already in use` | A `fluxctl service install` agent (from a version before the guard, or installed with `--force`) already holds port 7788: `fluxctl service uninstall`, then `brew services restart zyvorai/tap/fluxvm`. |
| `fluxctl service install` says FluxVM is managed by Homebrew, or that something already listens | The guard working: use `brew services start zyvorai/tap/fluxvm`, or stop the other daemon. `--force` installs anyway. |
| The MCP client lost the `fluxvm` tools after `brew upgrade` | The entry points at the removed versioned path: re-run `fluxctl mcp install <client>`. |
| The client does not list the `fluxvm` tools | Restart the client; `fluxctl mcp status` shows whether the entry is written. |
| A VM needs vmnet, bridged networking or USB | Not available in the ad hoc package; see [macos-signing.md](macos-signing.md). |
