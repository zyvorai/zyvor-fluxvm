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

## Host safety guards

All of these read state without sudo and change no host setting.

- **macOS build recorded with saved state.** `snapshot save` writes the host build (`sw_vers -buildVersion`) to `host-build.txt` in the
  snapshot directory (fork and checkpoint copies carry it). A restore on a different build is not attempted: the guest boots cold from the
  snapshot's disk and the log says why, or, with `FLUXVM_RESTORE_ON_OS_UPDATE=refuse` on the daemon, the launch fails with
  "the saved VM state was written on macOS build X but this Mac now runs build Y". A snapshot with no record is restored as before.
  Templates are cold-booted disks and are not affected.
- **Locked-session guard.** Before restoring saved state the launch reads the console session. If it is locked or at the login window it fails
  with "restoring saved VM state needs an unlocked login session ... run `fluxctl mac host headless-check`". Cold boots are not blocked.
- **`mac-fleet doctor` / `status`.** The probe runs `fluxctl mac host headless-check --json` on each Mac over ssh. `doctor` prints one
  `headless` warning per failing check; `status` has a SESSION column (`unlocked`, `locked`, `login window`, or `-` for an older install).
- **Heartbeat facts and placement.** `fluxvm-agent node` adds an optional `health` object to its Apple capabilities (`session_unlocked`,
  `sleep_held`, `thermal_limit`, `free_disk_mib`, `os_build`), all optional and skipped when empty, so older agents and central interoperate.
  `apple_placement::score` rejects a Mac throttled below 80% or with under 5 GiB free, ranks a locked-session Mac last and a Mac whose sleep
  is not held slightly lower. Unknown never counts against a Mac.
- **Thermal admission.** Creating a `vz` VM on a Mac whose `pmset -g therm` shows `CPU_Speed_Limit` below 80 is refused, with an
  `admission.deny` audit event.
- **Keep-awake only while a VM runs (opt-in).** `FLUXVM_KEEPAWAKE_WHILE_RUNNING=1` on the daemon holds `caffeinate -i -m -s -w <pid>` while
  a `vz` VM runs and releases it after the last stops (checked on the reaper tick). Off by default.
- **Automatic macOS updates.** `status` shows `AutomaticallyInstallMacOSUpdates`; `headless-check` warns when it is on.

Not run on a Mac: none of this was exercised against a live daemon, a locked session, a throttled Mac or a macOS update; parsers and decisions
are unit-tested on fixtures and fakes.

