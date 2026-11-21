# The Mac as a FluxVM host: `fluxctl mac host`

A Mac mini that runs FluxVM unattended has to stay awake, stay logged in, and stay unlocked: Virtualization.framework restores saved
VM state only in an unlocked login session ([deploy/launchd-notes.md](../deploy/launchd-notes.md)). `fluxctl mac host` reads and,
only when you ask, adjusts that. It never needs the daemon. Research, sources and a backlog: [apple-host-research.md](apple-host-research.md).

| Command | What it does | Changes the Mac? |
| --- | --- | --- |
| `mac host status [--json]` | power source, system/display/disk sleep, restart after power loss, Wake on LAN, Low Power Mode, what holds sleep off, keep-awake agent, console user and lock state, auto-login, FileVault, displays, free disk on the data volume, thermal state | no |
| `mac host headless-check [--json]` | the unlocked-session requirement: login window, locked screen, session owner, auto-login (and FileVault), idle sleep, display, free disk; each finding has a `fix:` line; exit 1 on a failure | no |
| `mac host keepawake [install\|uninstall\|status]` | `install` writes `~/Library/LaunchAgents/dev.zyvor.fluxvm.keepawake.plist` (`caffeinate -i -m -s`, `--display` adds `-d`) and loads it with `launchctl bootstrap gui/UID`; `--dry-run` prints it | `install`/`uninstall` only; no sudo |
| `mac host tune` | prints each recommended setting, the current value and the exact command | no |
| `mac host tune --apply --only NAME ...` | runs `sudo pmset -a NAME VALUE` for exactly the named settings that differ (`sleep`, `displaysleep`, `disksleep`, `autorestart`, `womp`, `lowpowermode`); there is no "all"; stops at the first failure and prints the values afterwards | yes, with your `sudo` password |
| `mac host rosetta` | whether Rosetta for Linux guests is available (the signed runner's answer plus the runtime file), and how a Linux guest uses it | no |

Things `tune` never changes, because they are security settings or need a password dialog: automatic login (not offered with FileVault
on), the screen lock, FileVault. `headless-check` tells you where to change them.

`-s` of `caffeinate` counts only on AC power, which a Mac mini always is. The keep-awake agent holds its assertions as long as it is
loaded; `uninstall` releases them. It is independent of the `dev.zyvor.fluxvm` daemon agent (`fluxctl service install`).

Rosetta: if the runner says installed but the runtime file `/Library/Apple/usr/libexec/oah/libRosettaRuntime` is missing, the command
warns instead of deciding; reports differ on macOS 27 ([apple-host-research.md](apple-host-research.md#11-rosetta-on-macos-27-conflicting-reports-and-a-local-observation)).
Install with `fluxvm-vz-runner install-rosetta`.

Status: parsers are tested on output captured from one Mac (`crates/fluxctl/src/mac_host_fixtures/`), the decisions and the apply and
keep-awake paths on a fake host and a temporary home. The read-only commands were run on that Mac. **Not run:** `keepawake install`
against launchd, `tune --apply`, a Mac with no display or at the login window, a laptop on battery, or a throttled Mac (that sample is
synthetic). On the capture Mac the two lock signals in `ioreg` disagreed; the check uses the session's own flag.
