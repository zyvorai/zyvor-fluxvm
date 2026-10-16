# Install FluxVM

One command, no sudo, no package manager needed:

```bash
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash
```

That downloads the release package for your machine from GitHub Releases, **verifies its SHA-256**, unpacks it under
`~/.local/share/fluxvm/<version>` and links the binaries into `~/.local/bin`. Run the same command again to upgrade.
The script is [`scripts/install.sh`](https://github.com/zyvorai/zyvor-fluxvm/blob/main/scripts/install.sh) served
unchanged: read it first with `curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | less` if you like.

## Contents

- [Requirements](#requirements)
- [The one-liner, and what it changes](#the-one-liner-and-what-it-changes)
- [Options](#options)
- [Examples](#examples)
- [Install without piping to a shell](#install-without-piping-to-a-shell)
- [Which version has what](#which-version-has-what)
- [Upgrade, pin, roll back, uninstall](#upgrade-pin-roll-back-uninstall)
- [Run the daemon](#run-the-daemon)
- [Connect an agent](#connect-an-agent)
- [Alternatives: Homebrew, source](#alternatives-homebrew-source)
- [Security notes](#security-notes)
- [Troubleshooting](#troubleshooting)

## Requirements

| Host | Package | Needs |
|---|---|---|
| macOS 14 (Sonoma) or newer, **Apple silicon** | `macos-arm64` | `curl`, `tar`; the `hivex` library (`brew install hivex`: the installer does it when Homebrew is present) |
| Linux **x86_64** | `linux-x86_64` | `curl`, `tar`; `libhivex0` and `libsystemd0` (Debian/Ubuntu) or `hivex` and `systemd-libs` (Fedora/RHEL); KVM (`/dev/kvm`) to run VMs. Built on Ubuntu 22.04, so glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+, Fedora/RHEL 9+) |
| Linux aarch64 | `linux-aarch64` | as above; cross-compiled on x86_64 by the release workflow and marked experimental, so a failed aarch64 build does not block the x86_64 package or the release |
| Intel Macs, Windows, other | none | build from source: `cargo build --release -p fluxctl` (see the [README](https://github.com/zyvorai/zyvor-fluxvm#readme)) |

The installer checks the platform first and exits with status 2 and the reason if there is no package for it. A release
that does not carry a package for your platform fails the download with the asset name and the releases URL.

## The one-liner, and what it changes

In order, and nothing else:

1. **Find the release.** `latest` is the newest *published* release (drafts are skipped), found by following
   `https://github.com/zyvorai/zyvor-fluxvm/releases/latest`. `--version` pins one.
2. **Download** `fluxvm-<version>-<platform>.tar.gz` and its `.sha256` to a temporary directory.
3. **Verify.** The tarball's SHA-256 must equal the `.sha256` file's. A mismatch, or a checksum file that is not a
   SHA-256, stops the install with nothing written. There is no flag to skip this.
4. **Unpack** to `PREFIX/share/fluxvm/<version>/` and repoint `PREFIX/share/fluxvm/current` at it. Older versions stay,
   which is what makes a rollback one command.
5. **Link** each `bin/*` into `PREFIX/bin`. It replaces only links it made earlier; a real file of yours with the same
   name is left alone and reported.
6. **Check dependencies** (macOS: install `hivex` with Homebrew if the package links it and it is missing, unless
   `--no-deps`; Linux: report missing shared libraries and the packages that provide them).
7. **Optionally** (only when you ask) install a service and wire an MCP client.

It writes nothing outside `PREFIX` (default `~/.local`) except what `--service` and `--client` do. It does **not** edit
your shell startup files: if `~/.local/bin` is not on your `PATH` it prints the line to add.

## Options

Pass them after `bash -s --`:

```bash
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --version 0.4.10 --client claude-code
```

| Option | Meaning |
|---|---|
| `--version X`, `--version latest` | Install release `X` (a leading `v` is accepted). Default: latest published. |
| `--prefix DIR` | Install under `DIR` instead of `~/.local` (it needs to be writable by you; for a system-wide prefix run the script as a user that may write there). |
| `--service` | macOS: run `fluxctl service install` (a LaunchAgent that starts at login) and wait for `/healthz`. Linux: print the systemd commands (root is needed, so they are not run for you). |
| `--client NAME` | After installing, run `fluxctl mcp install NAME --allow-write` (`claude-code`, `codex`, `gemini`, `cursor`, `vscode`, ...). |
| `--no-deps` | Do not try to install the runtime library. |
| `--brew` | macOS: use Homebrew (`brew install zyvorai/tap/fluxvm`) instead of downloading. |
| `--uninstall` | Remove what this script installed. Combine with `--prefix` and `--brew` as you installed. |
| `--dry-run` | Print the asset URL and what would run, then stop. Touches nothing, downloads nothing. |
| `-h`, `--help` | Usage. |

Environment: `FLUXVM_RELEASE_BASE` points the installer at a mirror of the releases layout
(`<base>/download/v<ver>/<asset>`), or at `file:///path` for an offline install.

Exit status: `0` success; `1` failed (download, checksum, service); `2` unsupported platform or missing tool;
`64` bad usage.

## Examples

```bash
# see what it would do
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --dry-run

# install, start the daemon at login, and give Claude Code the tools
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --service --client claude-code

# pin a version, into a project-local prefix
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --version 0.4.10 --prefix "$PWD/.fluxvm"
PATH="$PWD/.fluxvm/bin:$PATH" fluxctl --version

# offline, from a directory you populated with download/v0.4.10/<asset> and <asset>.sha256
FLUXVM_RELEASE_BASE=file:///srv/fluxvm-mirror bash install.sh --version 0.4.10
```

## Install without piping to a shell

Everything the script does, by hand (macOS shown; use `linux-x86_64` on Linux):

```bash
V=0.4.10
A=fluxvm-$V-macos-arm64.tar.gz
B=https://github.com/zyvorai/zyvor-fluxvm/releases/download/v$V
curl -fLO $B/$A && curl -fLO $B/$A.sha256
shasum -a 256 -c $A.sha256            # Linux: sha256sum -c $A.sha256     -> "$A: OK"
mkdir -p ~/.local/share/fluxvm && tar -xzf $A -C ~/.local/share/fluxvm
ln -sfn fluxvm-$V-macos-arm64 ~/.local/share/fluxvm/current
mkdir -p ~/.local/bin && for f in ~/.local/share/fluxvm/current/bin/*; do ln -sfn "$f" ~/.local/bin/; done
fluxctl --version
```

(The installer unpacks into `.../<version>` rather than the package's own directory name; the layout inside is the
same.) A tarball downloaded in a browser is quarantined by macOS: clear it with
`xattr -dr com.apple.quarantine ~/.local/share/fluxvm`. `curl` downloads are not quarantined.

## Which version has what

The one-liner installs the newest **published** release. As of this page that is **0.4.10**:

| Release | Has |
|---|---|
| `0.4.7` and earlier | sandboxes, snapshots, fork, speculative execution, MCP, the fleet. **No `fluxctl agent`.** |
| `0.4.8` and `0.4.9` | agent sessions: enforced filesystem policy, scoped tokens, supervised jobs, approvals, per-session HTTP rules, budgets, resumable transfer, fork/evaluate/promote. See the [session guide](agent-sessions-guide.md). |
| **`0.4.10`** (current) | everything above, plus `apple.host_gpu` / `fluxctl infer` for host Metal/MLX into offline VZ sandboxes ([host-inference.md](host-inference.md)), QMP sockets under a short `run_dir` (macOS `sockaddr_un` fix), and the Linux package built in `ubuntu:22.04` (glibc 2.35+). |

Check what you have:

```bash
fluxctl --version
fluxctl agent --help        # "unrecognized subcommand 'agent'" means a release without sessions
```

If you are on something older than 0.4.8 and need sessions immediately, build from source (below) or pin `--version 0.4.10`.

## Upgrade, pin, roll back, uninstall

```bash
# upgrade: run the installer again (idempotent; the old version is kept)
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash

# pin or roll back: the version you want is already on disk, so this is a relink
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --version 0.4.9

# list what is installed, and free space
ls ~/.local/share/fluxvm            # 0.4.9  0.4.10  current
rm -rf ~/.local/share/fluxvm/0.4.9  # only a version `current` does not point at

# uninstall
curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --uninstall
```

`--uninstall` removes the links it created and `~/.local/share/fluxvm`, and stops the LaunchAgent on macOS. **VM images and
state are never touched** (`~/Library/Application Support/FluxVM` on a Mac): delete them yourself to reclaim the space.

## Run the daemon

The binaries are a CLI, a daemon and an MCP server in one (`fluxctl`). For a quick try you need no daemon: `fluxctl run`
starts a throwaway VM. For the API and for agents you want it running:

| Where | How |
|---|---|
| macOS | `curl ... \| bash -s -- --service`, or `fluxctl service install` later. It is a LaunchAgent (starts at login) and needs a logged-in session for snapshots. `fluxctl service status`, `fluxctl service uninstall`. Check: `curl -s http://127.0.0.1:7788/healthz` |
| Linux | install the unit the package ships: `sudo install -m 0755 ~/.local/share/fluxvm/current/bin/fluxctl /usr/local/bin/`, `sudo install -m 0644 ~/.local/share/fluxvm/current/share/config.example.toml /etc/fluxvm.toml`, `sudo install -m 0644 ~/.local/share/fluxvm/current/share/systemd/fluxvm.service /etc/systemd/system/`, then `sudo systemctl daemon-reload && sudo systemctl enable --now fluxvm`. Host preparation (QEMU, Cloud Hypervisor, a bridge): `scripts/bootstrap-host.sh` |

If you installed with Homebrew, use `brew services` and not `fluxctl service install`: both register a LaunchAgent for the
same port ([install-macos.md](install-macos.md#2-start-the-daemon)).

## Connect an agent

```bash
fluxctl mcp install claude-code --allow-write       # or: codex, gemini, cursor, vscode; --dry-run prints the change
fluxctl mcp status
```

For an agent you do not fully trust, do not hand it the admin credential: mint a scoped token for one session and give
the agent that (see [session tokens](agent-sessions-guide.md#4-letting-an-agent-use-a-session-safely)).

## Alternatives: Homebrew, source

**Homebrew** (macOS, Apple silicon): the same package, managed by `brew`.

```bash
brew install zyvorai/tap/fluxvm
brew services start zyvorai/tap/fluxvm
```

The formula is updated by hand after a release, so it can trail the newest release by a little. Details:
[install-macos.md](install-macos.md).

**From source** (any Linux, or a Mac when you need unreleased work). You need a Rust toolchain and the sibling
`guestkit` checkout the build path-depends on:

```bash
git clone https://github.com/zyvorai/guestkit
git clone https://github.com/zyvorai/zyvor-fluxvm && cd zyvor-fluxvm
# macOS: xcode-select --install && brew install hivex
# Linux: sudo apt-get install -y libsystemd-dev libhivex-dev clang llvm libbpf-dev linux-libc-dev
cargo build --release -p fluxctl
target/release/fluxctl --version
```

On a Mac, point the source-built daemon at the signed runner from the package:
`FLUXVM_VZ_RUNNER=~/.local/share/fluxvm/current/bin/fluxvm-vz-runner target/release/fluxctl serve`.
[Getting started](getting-started.md) has the Linux host preparation.

## Security notes

- **Integrity, not provenance.** The checksum comes from the same release as the tarball, so it catches a corrupted or
  truncated download, a broken mirror, and a swapped tarball, but not a compromised release. For provenance on macOS see
  [macos-signing.md](macos-signing.md) (Developer ID signing and notarization are used when the signing secrets are
  configured; otherwise the package is ad hoc signed, which is all Virtualization.framework needs to run).
- **HTTPS only.** The default base is `https://github.com`; `curl -f` fails on any HTTP error rather than saving an error
  page. Nothing is fetched from anywhere else.
- **No privilege.** The script never calls `sudo`. The Linux service steps are printed for you to run.
- **No telemetry.** It makes the downloads above and, with `--service` or `--client`, talks to `127.0.0.1`.
- **Reversible.** `--uninstall` removes exactly what was created; `--dry-run` shows the plan first.

## Troubleshooting

| You see | Cause and fix |
|---|---|
| `No FluxVM package for Linux/riscv64` / `Apple silicon only (found x86_64)` | There is no package for this CPU. Build from source. |
| `Download failed ... may not have a package for linux-x86_64` | The release has no Linux asset (releases before the Linux workflow, or a draft). Pick a newer `--version`, or build from source. |
| `Could not reach .../releases/latest` | No network, a proxy, or no published release. Set `HTTPS_PROXY`, or pass `--version`. |
| `Checksum mismatch ... Nothing was installed.` | The download was altered or truncated. Run it again; if it repeats, do not use that release and report it. |
| `fluxctl: command not found` | `~/.local/bin` is not on `PATH`. Add `export PATH="$HOME/.local/bin:$PATH"` to your shell profile, or open a new shell. |
| `dyld: Library not loaded: .../libhivex.dylib` | `brew install hivex` (the installer does it unless `--no-deps`). |
| `error while loading shared libraries: libhivex.so.0` | `sudo apt-get install libhivex0` (Debian/Ubuntu) or `sudo dnf install hivex` (Fedora). |
| `version 'GLIBC_2.3x' not found` | The package needs glibc 2.35 or newer; the host is older. Upgrade the OS, or build from source. |
| `unrecognized subcommand 'agent'` | You installed a release older than the session work. Install a newer one, or build from source. |
| `skipped fluxctl: ... exists and is not a link` | A real file of yours is in the way. Move it, or use another `--prefix`. |
| The macOS daemon does not answer on `7788` | `fluxctl service status`; `tail ~/Library/Logs/fluxvm.log` (under Homebrew: `/opt/homebrew/var/log/fluxvm.log`); make sure only one of `brew services` and `fluxctl service install` is in use. |
| VMs will not start on a Mac | Virtualization needs a logged-in, unlocked session for some features; see [install-macos.md](install-macos.md#troubleshooting). |
