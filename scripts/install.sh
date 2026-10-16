#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Install FluxVM from a GitHub release, in one command, with no sudo and no package manager:
#
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --version 0.4.8 --client claude-code
#
# What it does: finds the release (the latest published one unless --version), downloads the package for this
# machine and its .sha256, REFUSES TO CONTINUE if the checksum differs, unpacks it under PREFIX/share/fluxvm/<version>,
# and links the binaries into PREFIX/bin. Nothing outside PREFIX is written, except what you ask for (--service,
# --client). Read it before you pipe it: it is this file, served unchanged from the docs site.
#
#   --version X|latest   install this release (default: latest published)
#   --prefix DIR         install under DIR (default: ~/.local)
#   --client NAME        also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...) with write tools
#   --service            macOS: run `fluxctl service install` (a LaunchAgent). Linux: print the systemd steps
#   --no-deps            do not try to install the runtime library the package needs (macOS: hivex via Homebrew)
#   --brew               macOS only: use Homebrew (`brew install zyvorai/tap/fluxvm`) instead of downloading
#   --uninstall          remove what this script installed (VM images and state are left alone)
#   --dry-run            print what would run and stop
#   -h, --help
#
# Environment: FLUXVM_RELEASE_BASE overrides https://github.com/zyvorai/zyvor-fluxvm/releases (a mirror, or file://
# for tests). Packages exist for macOS on Apple silicon (arm64); Linux x86_64 and aarch64 where a release has them.
# Works with macOS's bash 3.2.
set -euo pipefail

BASE="${FLUXVM_RELEASE_BASE:-https://github.com/zyvorai/zyvor-fluxvm/releases}"
BASE="${BASE%/}"
PREFIX="${HOME:-/tmp}/.local"
VERSION="latest"
CLIENT=""
SERVICE=0
DEPS=1
BREW=0
UNINSTALL=0
DRY=0
FORMULA="zyvorai/tap/fluxvm"
URL="http://127.0.0.1:7788"

usage() {
  cat <<'EOUSAGE'
Install FluxVM from a GitHub release (no sudo, no package manager).
  --version X|latest   install this release (default: latest published)
  --prefix DIR         install under DIR (default: ~/.local)
  --client NAME        also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...)
  --service            macOS: install the LaunchAgent; Linux: print the systemd steps
  --no-deps            do not install the runtime library the package needs (macOS: hivex via Homebrew)
  --brew               macOS only: use Homebrew instead of downloading
  --uninstall          remove what this script installed (VM images are kept)
  --dry-run            print what would run and stop
  -h, --help
EOUSAGE
}

die() { printf '%s\n' "$1" >&2; exit "${2:-1}"; }
say() { printf '==> %s\n' "$*"; }
run() {
  if [ "$DRY" = 1 ]; then printf '    would run: %s\n' "$*"; return 0; fi
  printf '    $ %s\n' "$*"
  "$@"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; [ -n "$VERSION" ] || die "--version needs a value" 64; shift 2 ;;
    --prefix) PREFIX="${2:-}"; [ -n "$PREFIX" ] || die "--prefix needs a directory" 64; shift 2 ;;
    --client) CLIENT="${2:-}"; [ -n "$CLIENT" ] || die "--client needs a name" 64; shift 2 ;;
    --service) SERVICE=1; shift ;;
    --no-service) SERVICE=0; shift ;;
    --no-deps) DEPS=0; shift ;;
    --brew) BREW=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" 64 ;;
  esac
done
VERSION="${VERSION#v}"

OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS/$ARCH" in
  Darwin/arm64) PLATFORM="macos-arm64" ;;
  Linux/x86_64) PLATFORM="linux-x86_64" ;;
  Linux/aarch64|Linux/arm64) PLATFORM="linux-aarch64" ;;
  Darwin/*) die "FluxVM's Mac package is Apple silicon only (found $ARCH)." 2 ;;
  *) die "No FluxVM package for $OS/$ARCH. Build from source: cargo build --release -p fluxctl (see the README, Quickstart)." 2 ;;
esac
if [ "$OS" = "Darwin" ]; then
  MAJOR="$(sw_vers -productVersion | cut -d. -f1)"
  if [ "$MAJOR" -lt 14 ] 2>/dev/null; then
    die "macOS 14 (Sonoma) or newer is required (found $(sw_vers -productVersion))." 2
  fi
fi

SHARE="$PREFIX/share/fluxvm"
BIN="$PREFIX/bin"

# ---------------------------------------------------------------------------------------------- Homebrew method
if [ "$BREW" = 1 ]; then
  [ "$OS" = "Darwin" ] || die "--brew is for macOS." 2
  command -v brew >/dev/null 2>&1 || die "Homebrew is required for --brew: https://brew.sh" 2
  if [ "$UNINSTALL" = 1 ]; then
    say "Stopping the service and removing FluxVM (Homebrew)"
    run brew services stop "$FORMULA" || true
    run brew uninstall "$FORMULA"
    exit 0
  fi
  say "Installing FluxVM ($FORMULA)"
  run brew install "$FORMULA"
  if [ "$SERVICE" = 1 ]; then
    run brew services start "$FORMULA"
  fi
  if [ -n "$CLIENT" ]; then
    run fluxctl mcp install "$CLIENT" --allow-write
  fi
  say "Done. The Homebrew package is built from a tagged release; omit --brew to download a release directly."
  exit 0
fi

# ---------------------------------------------------------------------------------------------- uninstall
if [ "$UNINSTALL" = 1 ]; then
  say "Removing FluxVM from $PREFIX"
  if [ "$OS" = "Darwin" ] && [ -x "$BIN/fluxctl" ] && [ "$DRY" = 0 ]; then
    "$BIN/fluxctl" service uninstall >/dev/null 2>&1 || true
  fi
  if [ -d "$BIN" ]; then
    for l in "$BIN"/*; do
      [ -L "$l" ] || continue
      case "$(readlink "$l")" in
        *share/fluxvm/*) run rm -f "$l" ;;
      esac
    done
  fi
  if [ -d "$SHARE" ]; then run rm -rf "$SHARE"; fi
  say "Done. VM images and state stay where they are (macOS: ~/Library/Application Support/FluxVM)."
  exit 0
fi

# ---------------------------------------------------------------------------------------------- tools
command -v curl >/dev/null 2>&1 || die "curl is required." 2
command -v tar >/dev/null 2>&1 || die "tar is required." 2
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum or shasum is required to verify the download." 2
fi

# ---------------------------------------------------------------------------------------------- version
if [ "$VERSION" = "latest" ]; then
  case "$BASE" in
    file://*) die "cannot resolve 'latest' from a file:// base; pass --version." 64 ;;
  esac
  # GitHub redirects /releases/latest to /releases/tag/<tag>; a draft is never "latest".
  final="$(curl -fsSIL -o /dev/null -w '%{url_effective}' "$BASE/latest")" \
    || die "Could not reach $BASE/latest (is there a published release?)."
  VERSION="${final##*/}"
  VERSION="${VERSION#v}"
  if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
    die "Could not work out the latest version from $final."
  fi
fi
case "$VERSION" in
  *[!0-9A-Za-z._-]*|"") die "not a version: $VERSION" 64 ;;
esac

ASSET="fluxvm-$VERSION-$PLATFORM.tar.gz"
ASSET_URL="$BASE/download/v$VERSION/$ASSET"
say "FluxVM $VERSION for $PLATFORM"
say "from $ASSET_URL"

if [ "$DRY" = 1 ]; then
  printf '    would download: %s\n    would download: %s.sha256\n' "$ASSET_URL" "$ASSET_URL"
  printf '    would verify the checksum, unpack to %s/%s, link %s/current/bin/* into %s\n' "$SHARE" "$VERSION" "$SHARE" "$BIN"
  if [ "$SERVICE" = 1 ]; then printf '    would set up the service\n'; fi
  if [ -n "$CLIENT" ]; then printf '    would run: fluxctl mcp install %s --allow-write\n' "$CLIENT"; fi
  exit 0
fi

# ---------------------------------------------------------------------------------------------- download + verify
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-install.XXXXXX")"
STAGE=""
trap 'rm -rf "$TMP" ${STAGE:+"$STAGE"}' EXIT
# A progress bar only when someone is watching; piped into a log it is noise.
PROGRESS="-sS"
if [ -t 2 ]; then PROGRESS="--progress-bar"; fi
if ! curl -fL "$PROGRESS" -o "$TMP/$ASSET" "$ASSET_URL"; then
  die "Download failed: $ASSET_URL
This release may not have a package for $PLATFORM. Releases: $BASE" 1
fi
if ! curl -fsSL -o "$TMP/$ASSET.sha256" "$ASSET_URL.sha256"; then
  die "Could not download the checksum $ASSET_URL.sha256; not installing an unverified package." 1
fi
want="$(awk '{print $1; exit}' "$TMP/$ASSET.sha256")"
got="$(sha256_of "$TMP/$ASSET")"
case "$want" in
  *[!0-9a-f]*|"") die "The checksum file does not hold a SHA-256: $want" 1 ;;
esac
if [ "${#want}" != 64 ] || [ "$want" != "$got" ]; then
  die "Checksum mismatch for $ASSET:
  expected $want
  got      $got
Nothing was installed." 1
fi
say "Checksum verified ($got)"

# ---------------------------------------------------------------------------------------------- unpack + link
mkdir -p "$SHARE" "$BIN"
STAGE="$SHARE/.staging-$VERSION.$$"
rm -rf "$STAGE"; mkdir -p "$STAGE"
tar -xzf "$TMP/$ASSET" -C "$STAGE"
# The package is one directory (fluxvm-<ver>-<platform>/) holding bin/, share/, ...
INNER="$(find "$STAGE" -mindepth 1 -maxdepth 1 -type d | head -n1)"
if [ -z "$INNER" ] || [ ! -d "$INNER/bin" ]; then
  die "The package has no bin/ directory; not installing."
fi
rm -rf "${SHARE:?}/$VERSION"
mv "$INNER" "$SHARE/$VERSION"
ln -sfn "$VERSION" "$SHARE/current"

for f in "$SHARE/current/bin"/*; do
  name="$(basename "$f")"
  # Replace only our own links or nothing: never overwrite a real file of someone else's.
  if [ -e "$BIN/$name" ] && [ ! -L "$BIN/$name" ]; then
    printf '    skipped %s: %s/%s exists and is not a link\n' "$name" "$BIN" "$name" >&2
    continue
  fi
  ln -sfn "../share/fluxvm/current/bin/$name" "$BIN/$name"
done
say "Installed to $SHARE/$VERSION (linked from $BIN)"

# ---------------------------------------------------------------------------------------------- runtime dependencies
if [ "$OS" = "Darwin" ]; then
  # Downloads made with curl are not quarantined; clear the flag in case they were fetched another way.
  xattr -dr com.apple.quarantine "$SHARE/$VERSION" 2>/dev/null || true
  if [ "$DEPS" = 1 ] && command -v otool >/dev/null 2>&1; then
    if otool -L "$SHARE/current/bin/fluxctl" 2>/dev/null | grep -q hivex; then
      if [ ! -e /opt/homebrew/lib/libhivex.dylib ] && [ ! -e /usr/local/lib/libhivex.dylib ]; then
        if command -v brew >/dev/null 2>&1; then
          say "Installing the hivex library the package links (brew install hivex)"
          brew install hivex || printf 'warning: brew install hivex failed; fluxctl may not start until it is installed\n' >&2
        else
          printf 'warning: fluxctl needs libhivex. Install Homebrew (https://brew.sh) and run: brew install hivex\n' >&2
        fi
      fi
    fi
  fi
elif command -v ldd >/dev/null 2>&1; then
  missing="$(ldd "$SHARE/current/bin/fluxctl" 2>/dev/null | grep 'not found' || true)"
  if [ -n "$missing" ]; then
    printf 'warning: missing shared libraries:\n%s\n' "$missing" >&2
    printf 'Debian/Ubuntu: sudo apt-get install -y libhivex0 libsystemd0    Fedora/RHEL: sudo dnf install -y hivex systemd-libs\n' >&2
  fi
fi

# ---------------------------------------------------------------------------------------------- service, client
FLUXCTL="$BIN/fluxctl"
if [ "$SERVICE" = 1 ]; then
  if [ "$OS" = "Darwin" ]; then
    say "Installing the LaunchAgent (starts at login)"
    "$FLUXCTL" service install
    i=0
    until curl -fsS "$URL/healthz" >/dev/null 2>&1; do
      i=$((i + 1))
      if [ "$i" -gt 60 ]; then die "The daemon did not answer on $URL within 60 s. See: fluxctl service status" 1; fi
      sleep 1
    done
    say "Daemon is up: $(curl -fsS "$URL/healthz")"
  else
    cat <<EOSYSTEMD
==> Linux service (needs root, so it is not done for you):
    sudo install -m 0755 "$SHARE/current/bin/fluxctl" /usr/local/bin/fluxctl
    sudo install -m 0644 "$SHARE/current/share/config.example.toml" /etc/fluxvm.toml
    sudo install -m 0644 "$SHARE/current/share/systemd/fluxvm.service" /etc/systemd/system/fluxvm.service
    sudo systemctl daemon-reload && sudo systemctl enable --now fluxvm
EOSYSTEMD
  fi
fi

if [ -n "$CLIENT" ]; then
  say "Wiring $CLIENT to FluxVM over MCP (write tools enabled)"
  "$FLUXCTL" mcp install "$CLIENT" --allow-write
  "$FLUXCTL" mcp status || true
fi

INSTALLED="$("$FLUXCTL" --version 2>/dev/null || true)"
if [ -n "$INSTALLED" ]; then
  say "Installed: $INSTALLED"
else
  printf 'warning: fluxctl did not run; see the dependency note above\n' >&2
fi

case ":$PATH:" in
  *":$BIN:"*) ;;
  *)
    # shellcheck disable=SC2016  # $PATH is meant literally: it is the line the user will paste
    printf '\nAdd it to your PATH:\n  export PATH="%s:$PATH"      # put this line in ~/.zshrc or ~/.bashrc\n' "$BIN" ;;
esac

# Tab completion is one line per shell; offer the one for the shell this was run from.
case "${SHELL##*/}" in
  zsh)  COMPLETE='mkdir -p ~/.zfunc && fluxctl completions zsh > ~/.zfunc/_fluxctl   # and put fpath+=~/.zfunc before compinit in ~/.zshrc' ;;
  bash) COMPLETE='mkdir -p ~/.local/share/bash-completion/completions && fluxctl completions bash > ~/.local/share/bash-completion/completions/fluxctl' ;;
  fish) COMPLETE='mkdir -p ~/.config/fish/completions && fluxctl completions fish > ~/.config/fish/completions/fluxctl.fish' ;;
  *)    COMPLETE='fluxctl completions bash|zsh|fish   # prints the script for your shell' ;;
esac

cat <<EONEXT

Next:
  fluxctl doctor                                      # is this host ready? (every failure prints its fix)
  fluxctl run                                         # a throwaway VM and a shell
  fluxctl mcp install claude-code --allow-write       # let an agent drive it
  fluxctl agent run --policy developer-safe           # an agent session (releases that include it)
Tab completion:  $COMPLETE
Upgrade: run the same command again. Pin or roll back: --version X. Remove: --uninstall.
Docs: https://zyvorai.github.io/zyvor-fluxvm/docs/install
EONEXT
