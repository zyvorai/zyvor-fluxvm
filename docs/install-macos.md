# Install FluxVM on a Mac with Homebrew

> **Prefer one command and no Homebrew formula?** `curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash`
> downloads and verifies the same release package into `~/.local` (no sudo), with `--service` and `--client` options.
> See [install.md](install.md). This page is the Homebrew route.

The quickest way to a working FluxVM on Apple silicon: one `brew install`, the daemon as a background service, and
an AI agent client wired to it over MCP. The current tap formula is **0.4.10** (`fluxctl agent`, `host_gpu` /
[host-inference.md](host-inference.md)). Install, service, upgrade and uninstall were originally verified on an
Apple M4 mini (macOS 27.2) against earlier 0.4.x packages from the tap; notes below that name `0.4.4` / `0.4.5` are
historical (for example `/readyz` and Homebrew `opt` paths). A local model path was verified with the
agent-sandbox demo (`examples/agent-sandbox-demo/local-model.sh`).

## Before you start

- Apple silicon, macOS 14 (Sonoma) or newer. Homebrew installs `hivex` for you (guest image inspection).
- A Mac that is logged in and unlocked when you restore snapshots or use warm slots; a headless Mac needs a dedicated
  auto-login user (see [Unattended hosts](agent-density.md#unattended-hosts)).
- Disk for VM images: the first Debian image is a few GB under `~/Library/Application Support/FluxVM`.

## 1. Install

```bash
brew install zyvorai/tap/fluxvm
fluxctl --version                  # fluxctl 0.4.10
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
already listens on the configured address, and points you at `brew services start`. `--force` overrides it.

A second daemon (a separate FluxVM for another purpose) needs its own address and its own state directory; both are
flags (or `FLUXVM_LISTEN` and `FLUXVM_STATE_DIR`) and `service install` writes them into the LaunchAgent:

```bash
fluxctl service install --listen 127.0.0.1:7789 --state-dir ~/fluxvm-second
fluxctl --server http://127.0.0.1:7789 list          # point the CLI (and MCP, via --server) at it
fluxctl serve --listen 127.0.0.1:7789 --state-dir /tmp/try   # or run one in the foreground
```

The guard checks the address you pass, so a second agent on a free port is accepted. Two daemons must never share a state
directory.

> `/readyz` answered `503` on a Mac in 0.4.4: it still asked for the Linux eBPF dataplane. It answers `200` from 0.4.5
> (checked on a Homebrew install of the 0.4.5 package). Use `/healthz` for liveness on 0.4.4.

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
(`serverInfo.name` `fluxvm`, 66 tools with `--allow-write` in 0.4.10) and a real sandbox round trip works:

| Tool call | Result on the M4 |
|---|---|
| `sandbox_create` `{"offline": true}` | a running sandbox with only `lo`, 7.8 s on a fresh state directory |
| `sandbox_exec` `{"vm": id, "command": ...}` | `hello from fluxvm-vm`, `aarch64`, exit 0 |
| `vm_fork` `{"vm": id, "count": 2}` | two running copies |
| `vm_delete` | deleted |

Write tools (`--allow-write`) can create, run commands in and delete VMs: leave them off for a read-only agent. A runnable
agent loop with real timings is in [examples/agent-sandbox-demo](https://github.com/zyvorai/zyvor-fluxvm/tree/main/examples/agent-sandbox-demo).

> **After `brew upgrade`:** an MCP entry (or `fluxctl service install` plist) written by 0.4.4 or earlier points at the
> versioned path (`/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl`). Homebrew removes the old version on upgrade, and the
> entry then points at nothing: tested, an entry written by 0.4.1 was dead right after upgrading to 0.4.4. Re-run
> `fluxctl mcp install`, or edit the entry to `/opt/homebrew/opt/fluxvm/bin/fluxctl`. 0.4.5 and later write the `opt`
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

## Optional: tab completion

`fluxctl completions bash|zsh|fish` prints the script for your shell. For zsh:

```sh
mkdir -p ~/.zfunc && fluxctl completions zsh > ~/.zfunc/_fluxctl
# in ~/.zshrc, before compinit:  fpath+=~/.zfunc
```

For bash write it to `~/.local/share/bash-completion/completions/fluxctl`, for fish to `~/.config/fish/completions/fluxctl.fish`.
Homebrew can do this for you if the formula calls `generate_completions_from_executable(bin/"fluxctl", "completions")`.
Run it again after an upgrade that adds commands.

## Optional: the fleet console

The web console (`/console`) is served by `fluxvm-agent central`, the multi-Mac control plane. The Homebrew package
installs `fluxd`, `fluxctl` and friends but **not** `fluxvm-agent`, so build it from a checkout:

```bash
git clone https://github.com/zyvorai/zyvor-fluxvm && cd zyvor-fluxvm
cargo build --release -p fluxvm-agent          # needs a Rust toolchain; binary: target/release/fluxvm-agent
```

Try it on this Mac with a simulated node (nothing boots, no VM is created):

```bash
target/release/fluxvm-agent central --listen 127.0.0.1:7799 --state-dir ./central-state &
# first start prints:  token: fvo_...   (also saved in ./central-state/operator-token, mode 0600)
TOKEN=$(cat ./central-state/operator-token)
target/release/fluxvm-agent sim-node --name studio-1 --central http://127.0.0.1:7799 --token "$TOKEN" &
open http://127.0.0.1:7799/console              # paste the token into the sign-in field
```

With no `--token`, central makes a random operator token on its first start and keeps it, so there is always one
default key. To use your own, pass `--token VALUE` (it always wins); to get a new generated one, stop central, delete
`central-state/operator-token` and start it again. `--no-auth` turns the check off entirely (lab only).

You land on **Overview** with one Mac. Use the top bar's **Cloud**, **Compute**, **Billing** and **Access** menus (or
⌘K) to move around. Stop it with `kill %1 %2`; the state stays in `./central-state`.

**Which key do I paste?** Central does not make one up for you:

| Who | Where the key comes from |
|---|---|
| Operator (whole fleet) | The `--token` (or `FLUXVM_AGENT_TOKEN`) you gave central, else the generated one printed on first start and kept in `<state-dir>/operator-token`. Lost it? `cat` that file, or restart with `--token NEW`. |
| Tenant | The operator runs `fluxctl fleet --token "$TOKEN" tenant create acme`; the token is printed **once**. Rotate it later from the console or `fluxctl`. |
| Sign-up | Shown once after the emailed confirmation link is used (needs `--signup-mail-webhook`, see [mac-cloud-tenancy.md](mac-cloud-tenancy.md)). |
| SSO | Nobody sees a token: people sign in through the company's identity provider. |

For real Macs, run `fluxvm-agent central` on one host (with `--tls-cert/--tls-key` or behind a TLS proxy, and
`--public-url`), then point each Mac's agent at it; see [fleet-quickstart.md](fleet-quickstart.md) and
[mac-cloud-tenancy.md](mac-cloud-tenancy.md#console) for tenants, sign-in and what each page does.

## Troubleshooting

Problems outside the Mac daemon (installer, Linux, `fluxctl` output) are collected in [troubleshooting.md](troubleshooting.md).

| Symptom | Cause and fix |
|---|---|
| `curl 127.0.0.1:7788/readyz` gives `503` | Expected on 0.4.4 only (see above); upgrade, or use `/healthz`. |
| `the Apple runner fluxvm-vz-runner was not found (looked at: …/target/debug/…)` | A VM record from a development build points at a path that no longer exists. Delete the VM (`fluxctl delete <id>`) and create it again. |
| Snapshot restore or a warm slot fails | The login session is locked or absent. Unlock it, or set up the dedicated auto-login user. |
| `brew services` shows `Running: false` and the log says `Address already in use` | A `fluxctl service install` agent (from a version before the guard, or installed with `--force`) already holds port 7788: `fluxctl service uninstall`, then `brew services restart zyvorai/tap/fluxvm`. |
| `fluxctl service install` says FluxVM is managed by Homebrew, or that something already listens | The guard working: use `brew services start zyvorai/tap/fluxvm`, or stop the other daemon. `--force` installs anyway. |
| The MCP client lost the `fluxvm` tools after `brew upgrade` | The entry points at the removed versioned path: re-run `fluxctl mcp install <client>`. |
| The client does not list the `fluxvm` tools | Restart the client; `fluxctl mcp status` shows whether the entry is written. |
| A VM needs vmnet, bridged networking or USB | Not available in the ad hoc package; see [macos-signing.md](macos-signing.md). |
