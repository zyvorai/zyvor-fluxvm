# Troubleshooting

One page for the problems people hit first, in the order they hit them. Every row below comes from a table or a
note that already lives on the page linked in its section; this page gathers them so you do not have to guess which
install page to open.

[Back to the docs index](index.md)

## Installing

| You see | Cause and fix |
|---|---|
| `No FluxVM package for Linux/riscv64` / `Apple silicon only (found x86_64)` | There is no package for this CPU. Build from source ([getting-started.md](getting-started.md)). |
| `Download failed ... may not have a package for linux-x86_64` | The release has no Linux asset (older release, or a draft). Pick a newer `--version`, or build from source. |
| `Could not reach .../releases/latest` | No network, a proxy, or no published release. Set `HTTPS_PROXY`, or pass `--version`. |
| `Checksum mismatch ... Nothing was installed.` | The download was altered or truncated. Run it again; if it repeats, do not use that release and report it. |
| `fluxctl: command not found` | `~/.local/bin` is not on `PATH`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell profile, or open a new shell. |
| `dyld: Library not loaded: .../libhivex.dylib` | `brew install hivex` (the installer does it unless `--no-deps`). |
| `error while loading shared libraries: libhivex.so.0` | `sudo apt-get install libhivex0` (Debian/Ubuntu) or `sudo dnf install hivex` (Fedora). |
| `unrecognized subcommand 'agent'` | You installed a release older than the session work. Install a newer one, or build from source. |
| `skipped fluxctl: ... exists and is not a link` | A real file of yours is in the way. Move it, or use another `--prefix`. |

More: [install.md](install.md#troubleshooting).

## The daemon on a Mac

| You see | Cause and fix |
|---|---|
| The daemon does not answer on `7788` | `fluxctl service status`; `tail ~/Library/Logs/fluxvm.log` (under Homebrew: `/opt/homebrew/var/log/fluxvm.log`). Make sure only one of `brew services` and `fluxctl service install` is in use. |
| `brew services` shows `Running: false` and the log says `Address already in use` | A `fluxctl service install` agent (from a version before the guard, or installed with `--force`) already holds port 7788: `fluxctl service uninstall`, then `brew services restart zyvorai/tap/fluxvm`. |
| `fluxctl service install` says FluxVM is managed by Homebrew, or that something already listens | The guard working: use `brew services start zyvorai/tap/fluxvm`, or stop the other daemon. `--force` installs anyway. |
| `curl 127.0.0.1:7788/readyz` gives `503` | Expected on 0.4.4 only; upgrade, or use `/healthz` for liveness. |
| The MCP client lost the `fluxvm` tools after `brew upgrade` | The entry points at the removed versioned path: re-run `fluxctl mcp install <client>`. |
| The client does not list the `fluxvm` tools | Restart the client; `fluxctl mcp status` shows whether the entry is written. |

More: [install-macos.md](install-macos.md#troubleshooting).

## Creating and running VMs on a Mac

| You see | Cause and fix |
|---|---|
| `the Apple runner fluxvm-vz-runner was not found (looked at: …/target/debug/…)` | A VM record from a development build points at a path that no longer exists. Delete the VM (`fluxctl delete <id>`) and create it again. |
| Snapshot restore or a warm slot fails | The login session is locked or absent. `fluxctl doctor` reports it (`login session`). Unlock it, or set up the dedicated auto-login user ([Unattended hosts](agent-density.md#unattended-hosts)). |
| A VM needs vmnet, bridged networking or USB | Not available in the ad hoc signed package; see [macos-signing.md](macos-signing.md). |

## Reading `fluxctl` output

| You see | Cause and fix |
|---|---|
| `-o table` has no effect on a command | `-o`/`--output-format` applies to the list-style commands (`list`, `list-images`, `snapshot-list`, `disk list`, `vm-template list`, `context list`, `events`, `sandbox list`) and to `doctor`; `fleet` prints tables on a terminal unless `-o` is given. Other commands print JSON. |
| Remote host, no results | `fluxctl --server http://host:7788 list`, or save it with `fluxctl context add` and `fluxctl context use`. Pass `--server-token` (or `FLUXVM_TOKEN`) if the daemon requires auth. |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | An error (the message is on stderr), or `fluxctl doctor` found a failing check |
| `2` | A usage error: an unknown flag or a missing argument |
| `N` | For `run`, `sandbox run`, `ssh`, `fleet exec` and `agent exec` (also when following an agent job or shell): the command's own exit status, so a script can branch on what ran inside the guest |

`fluxctl --help` prints the same table.

## Is this host ready?

`fluxctl doctor` checks the daemon, the binaries, free disk, `/dev/kvm` (Linux) or the runner's virtualization
entitlement, memory headroom and whether the login session is locked (macOS), and MCP or LaunchAgent entries that still point at a removed Homebrew
path. Each warning or failure prints the command that fixes it, and it exits `1` on a failure. It works with the
daemon down; add `-o json` for a machine-readable report.

## Still stuck

- Collect an incident bundle: [diagnostics-bundle.md](diagnostics-bundle.md).
- Check what the project says is and is not verified: [proof-and-status.md](proof-and-status.md).
- Fit questions: [faq.md](faq.md).
