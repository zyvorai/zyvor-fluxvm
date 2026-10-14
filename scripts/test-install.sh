#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Tests scripts/install.sh against fake release packages served from a local directory (FLUXVM_RELEASE_BASE=file://).
# Needs no network and installs nothing outside a temp directory. Run it from anywhere:
#   scripts/test-install.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
INSTALL="$HERE/install.sh"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-install-test.XXXXXX")"
trap 'rm -rf "$ROOT"' EXIT

case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) PLAT=macos-arm64 ;;
  Linux/x86_64) PLAT=linux-x86_64 ;;
  Linux/aarch64|Linux/arm64) PLAT=linux-aarch64 ;;
  *) echo "skip: no package platform for $(uname -s)/$(uname -m)"; exit 0 ;;
esac

if command -v sha256sum >/dev/null 2>&1; then SUM() { sha256sum "$1" | cut -d' ' -f1; }; else SUM() { shasum -a 256 "$1" | cut -d' ' -f1; }; fi

FAILS=0
ok()   { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { # name, command...
  local name="$1"; shift
  if "$@"; then ok "$name"; else fail "$name"; fi
}

REL="$ROOT/releases"
# make_release VERSION: a package whose fluxctl prints its version, plus a correct .sha256
make_release() {
  local v="$1"
  local d="$ROOT/stage/fluxvm-$v-$PLAT"
  rm -rf "$d"; mkdir -p "$d/bin" "$d/share/systemd" "$REL/download/v$v"
  printf '#!/bin/sh\necho "fluxctl %s"\n' "$v" > "$d/bin/fluxctl"
  printf '#!/bin/sh\necho runner\n' > "$d/bin/fluxvm-vz-runner"
  chmod +x "$d/bin/"*
  echo "listen = \"127.0.0.1:7788\"" > "$d/share/config.example.toml"
  tar -czf "$REL/download/v$v/fluxvm-$v-$PLAT.tar.gz" -C "$ROOT/stage" "fluxvm-$v-$PLAT"
  printf '%s  fluxvm-%s-%s.tar.gz\n' "$(SUM "$REL/download/v$v/fluxvm-$v-$PLAT.tar.gz")" "$v" "$PLAT" \
    > "$REL/download/v$v/fluxvm-$v-$PLAT.tar.gz.sha256"
}
inst() { FLUXVM_RELEASE_BASE="file://$REL" HOME="$ROOT/home" bash "$INSTALL" "$@"; }

mkdir -p "$ROOT/home"
make_release 9.9.9
make_release 9.9.10
P="$ROOT/prefix"

# --- dry run changes nothing
out="$(inst --version 9.9.9 --prefix "$P" --dry-run 2>&1)"
check "dry-run names the asset" grep -q "fluxvm-9.9.9-$PLAT.tar.gz" <<<"$out"
check "dry-run installs nothing" test ! -e "$P"

# --- install
inst --version 9.9.9 --prefix "$P" >/dev/null 2>&1
check "bin/fluxctl is linked" test -L "$P/bin/fluxctl"
check "every bin/* is linked" test -L "$P/bin/fluxvm-vz-runner"
check "fluxctl runs through the link" test "$("$P/bin/fluxctl" --version)" = "fluxctl 9.9.9"
check "current points at the version" test "$(readlink "$P/share/fluxvm/current")" = "9.9.9"
check "share/ files are unpacked" test -f "$P/share/fluxvm/9.9.9/share/config.example.toml"
check "no staging directory left" test -z "$(find "$P/share/fluxvm" -maxdepth 1 -name '.staging-*')"

# --- upgrade keeps the old version and swaps current
inst --version v9.9.10 --prefix "$P" >/dev/null 2>&1
check "upgrade runs the new version" test "$("$P/bin/fluxctl" --version)" = "fluxctl 9.9.10"
check "the old version is kept" test -d "$P/share/fluxvm/9.9.9"

# --- rollback
inst --version 9.9.9 --prefix "$P" >/dev/null 2>&1
check "--version rolls back" test "$("$P/bin/fluxctl" --version)" = "fluxctl 9.9.9"

# --- someone else's file is not overwritten
P2="$ROOT/prefix2"; mkdir -p "$P2/bin"; echo mine > "$P2/bin/fluxctl"
inst --version 9.9.9 --prefix "$P2" >/dev/null 2>&1
check "a foreign file is left alone" test "$(cat "$P2/bin/fluxctl")" = "mine"
check "the rest is still linked" test -L "$P2/bin/fluxvm-vz-runner"

# --- tampered package: refused, nothing installed
make_release 8.8.8
echo tamper >> "$REL/download/v8.8.8/fluxvm-8.8.8-$PLAT.tar.gz"
P3="$ROOT/prefix3"
if err="$(inst --version 8.8.8 --prefix "$P3" 2>&1)"; then fail "a tampered package must fail"; else ok "a tampered package fails"; fi
check "the error says checksum mismatch" grep -q "Checksum mismatch" <<<"$err"
check "nothing was installed" test ! -e "$P3/bin/fluxctl"

# --- a checksum file that is not a checksum
make_release 7.7.7
echo "not-a-hash  x" > "$REL/download/v7.7.7/fluxvm-7.7.7-$PLAT.tar.gz.sha256"
if inst --version 7.7.7 --prefix "$ROOT/prefix4" >/dev/null 2>&1; then fail "a bad .sha256 must fail"; else ok "a bad .sha256 fails"; fi

# --- missing release
if err="$(inst --version 1.2.3 --prefix "$ROOT/prefix5" 2>&1)"; then fail "a missing release must fail"; else ok "a missing release fails"; fi
check "the error names the asset URL" grep -q "fluxvm-1.2.3-$PLAT.tar.gz" <<<"$err"

# --- 'latest' cannot be resolved from a file:// base
if err="$(inst --prefix "$ROOT/prefix6" 2>&1)"; then fail "latest from file:// must fail"; else ok "latest from file:// fails"; fi
check "the error says to pass --version" grep -q -- "--version" <<<"$err"

# --- bad version strings
if inst --version '1.0;rm -rf' --prefix "$ROOT/prefix7" >/dev/null 2>&1; then fail "a shell-ish version must fail"; else ok "a shell-ish version fails"; fi

# --- unknown flag
if inst --frobnicate >/dev/null 2>&1; then fail "an unknown flag must fail"; else ok "an unknown flag fails"; fi

# --- uninstall removes ours and only ours
echo keep > "$P/bin/other-tool"
inst --prefix "$P" --uninstall >/dev/null 2>&1
check "uninstall removes the links" test ! -e "$P/bin/fluxctl"
check "uninstall removes share/fluxvm" test ! -e "$P/share/fluxvm"
check "uninstall leaves foreign files" test "$(cat "$P/bin/other-tool")" = "keep"

if [ "$FAILS" -ne 0 ]; then echo "$FAILS failed"; exit 1; fi
echo "all passed"
