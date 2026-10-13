#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Install FluxVM on a Mac the way docs/install-macos.md describes, in one command:
#
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --client claude-code
#
# It only drives Homebrew (`brew install zyvorai/tap/fluxvm`, `brew services`) and, when asked, `fluxctl mcp install`.
# It needs no sudo, downloads nothing itself, and changes nothing outside Homebrew and your MCP client's config.
#
#   --client NAME   also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...; with --allow-write)
#   --no-service    install only; do not start the background daemon
#   --uninstall     stop the service and `brew uninstall` the formula (VM images are left in place)
#   --dry-run       print what would run and stop
#   -h, --help
#
# Linux is not packaged: the script says so and exits 2 (build from source, see README).
# Works with macOS's bash 3.2.
set -euo pipefail

CLIENT=""
SERVICE=1
UNINSTALL=0
DRY=0
FORMULA="zyvorai/tap/fluxvm"
URL="http://127.0.0.1:7788"

usage() {
  cat <<'EOUSAGE'
Install FluxVM on a Mac via Homebrew.
  --client NAME   also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...)
  --no-service    install only; do not start the background daemon
  --uninstall     stop the service and brew uninstall the formula (VM images are kept)
  --dry-run       print what would run and stop
  -h, --help
EOUSAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --client) CLIENT="${2:-}"; [ -n "$CLIENT" ] || { echo "--client needs a name" >&2; exit 64; }; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 64 ;;
  esac
done

say() { printf '==> %s\n' "$*"; }
run() {
  if [ "$DRY" = 1 ]; then printf '    would run: %s\n' "$*"; return 0; fi
  printf '    $ %s\n' "$*"
  "$@"
}

OS="$(uname -s)"; ARCH="$(uname -m)"
if [ "$OS" != "Darwin" ]; then
  echo "This installer is for macOS on Apple silicon. On Linux, build from source:" >&2
  echo "  cargo build --release -p fluxctl     # see the README, Quickstart (Linux/KVM)" >&2
  exit 2
fi
if [ "$ARCH" != "arm64" ]; then
  echo "FluxVM's Mac package is Apple silicon only (found $ARCH)." >&2
  exit 2
fi
MAJOR="$(sw_vers -productVersion | cut -d. -f1)"
if [ "$MAJOR" -lt 14 ] 2>/dev/null; then
  echo "macOS 14 (Sonoma) or newer is required (found $(sw_vers -productVersion))." >&2
  exit 2
fi
if ! command -v brew >/dev/null 2>&1; then
  echo "Homebrew is required: https://brew.sh (this script does not install it)." >&2
  exit 2
fi

if [ "$UNINSTALL" = 1 ]; then
  say "Stopping the service and removing FluxVM"
  run brew services stop "$FORMULA" || true
  run brew uninstall "$FORMULA"
  say "Done. VM images stay under ~/Library/Application Support/FluxVM (delete them yourself if you want the space)."
  exit 0
fi

say "Installing FluxVM ($FORMULA)"
run brew install "$FORMULA"

if [ "$SERVICE" = 1 ]; then
  say "Starting the daemon (a LaunchAgent: starts at login)"
  run brew services start "$FORMULA"
  if [ "$DRY" = 0 ]; then
    i=0
    until curl -fsS "$URL/healthz" >/dev/null 2>&1; do
      i=$((i + 1))
      if [ "$i" -gt 60 ]; then
        echo "The daemon did not answer on $URL within 60 s. See: tail /opt/homebrew/var/log/fluxvm.log" >&2
        exit 1
      fi
      sleep 1
    done
    say "Daemon is up: $(curl -fsS "$URL/healthz")"
  fi
fi

if [ -n "$CLIENT" ]; then
  say "Wiring $CLIENT to FluxVM over MCP (write tools enabled)"
  run fluxctl mcp install "$CLIENT" --allow-write
  run fluxctl mcp status
fi

if [ "$DRY" = 0 ]; then
  say "Installed: $(fluxctl --version)"
fi
cat <<'EONEXT'

Next:
  fluxctl run                                         # a throwaway Debian VM and a shell
  fluxctl sandbox run alpine:3.22 --rm -- echo hi     # a container in its own VM
  fluxctl mcp install claude-code --allow-write       # let an agent drive it
Agent sessions (`fluxctl agent ...`) are newer than the Homebrew package: if `fluxctl agent --help` says
"unrecognized subcommand", build from source (docs/agent-platform/README.md).
Docs: https://zyvorai.github.io/zyvor-fluxvm/docs/agent-platform/
EONEXT
