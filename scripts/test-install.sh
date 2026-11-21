#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
#
# Tests scripts/install.sh against fake release packages served from a local directory (FLUXVM_RELEASE_BASE=file://).
# Needs no network and installs nothing outside a temp directory. Run it from anywhere:
#   scripts/test-install.sh
# shellcheck disable=SC1090  # install.sh is sourced by path, on purpose (the Linux flavour tests)
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

# ================================================================================ Linux flavours
# The detection functions are sourced from install.sh (FLUXVM_INSTALL_SOURCE_ONLY) and run against fake os-release files,
# so these run on any host, macOS included. The service steps run with a fake `id` (never root: nothing is really installed)
# and, off Linux, a fake `uname`.
SHIM="$ROOT/shim"; mkdir -p "$SHIM"
printf '#!/bin/sh\nif [ "$1" = "-u" ]; then echo 1000; else exec /usr/bin/id "$@"; fi\n' > "$SHIM/id"
printf '#!/bin/sh\ncase "$1" in -s) echo Linux ;; -m) echo x86_64 ;; *) echo Linux ;; esac\n' > "$SHIM/uname"
chmod +x "$SHIM/id" "$SHIM/uname"
LSHIM="$SHIM"; if [ "$(uname -s)" = Linux ]; then rm -f "$SHIM/uname"; fi

osrel() { # name, then the lines of an os-release file
  local f="$ROOT/osrel.$1"; shift; printf '%s\n' "$@" > "$f"; printf '%s' "$f"
}
fam() { ( FLUXVM_INSTALL_SOURCE_ONLY=1 FLUXVM_OS_RELEASE="$1" . "$INSTALL"; linux_family ); }
check "debian is debian" test "$(fam "$(osrel d 'ID=debian')")" = debian
check "ubuntu is debian (ID_LIKE)" test "$(fam "$(osrel u 'ID=ubuntu' 'ID_LIKE=debian')")" = debian
check "a Debian derivative is debian (ID_LIKE)" test "$(fam "$(osrel m 'ID=zorin' 'ID_LIKE="ubuntu debian"')")" = debian
check "fedora is rhel" test "$(fam "$(osrel f 'ID=fedora')")" = rhel
check "rocky is rhel" test "$(fam "$(osrel r 'ID="rocky"' 'ID_LIKE="rhel centos fedora"')")" = rhel
check "arch is arch" test "$(fam "$(osrel a 'ID=arch')")" = arch
check "manjaro is arch (ID_LIKE)" test "$(fam "$(osrel mj 'ID=manjaro' 'ID_LIKE=arch')")" = arch
check "opensuse is suse" test "$(fam "$(osrel o 'ID="opensuse-leap"' 'ID_LIKE="suse opensuse"')")" = suse
check "sles is suse" test "$(fam "$(osrel s 'ID="sles"' 'ID_LIKE="suse"')")" = suse
check "alpine is alpine" test "$(fam "$(osrel al 'ID=alpine')")" = alpine
check "gentoo is unknown (no package names claimed)" test "$(fam "$(osrel g 'ID=gentoo')")" = unknown
check "a missing os-release is unknown" test "$(fam "$ROOT/none")" = unknown

depcmd() { ( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; linux_dep_command "$1" "$2" ); }
check "debian installs libhivex0 and libsystemd0" test "$(depcmd debian apt-get)" = "apt-get install -y libhivex0 libsystemd0"
check "rhel installs hivex and systemd-libs" test "$(depcmd rhel dnf)" = "dnf install -y hivex systemd-libs"
check "arch installs hivex and systemd-libs" test "$(depcmd arch pacman)" = "pacman -S --needed --noconfirm hivex systemd-libs"
check "suse installs libhivex0 and libsystemd0" test "$(depcmd suse zypper)" = "zypper --non-interactive install libhivex0 libsystemd0"
check "alpine has no dependency command (musl)" test -z "$(depcmd alpine apk)"

vlt() { ( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; version_lt "$1" "$2" ); }
check "glibc 2.34 is below the floor" vlt 2.34 2.35
check "glibc 2.9 is below 2.35 (numeric, not text)" vlt 2.9 2.35
check "glibc 2.35 meets the floor" bash -c '! ( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; version_lt 2.35 2.35 )' "$INSTALL"
check "glibc 2.39 meets the floor" bash -c '! ( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; version_lt 2.39 2.35 )' "$INSTALL"
check "init can be forced to openrc" test "$( export FLUXVM_INIT=openrc; FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; linux_init )" = openrc

# --detect prints what was found
det="$(PATH="$LSHIM:$PATH" FLUXVM_OS_RELEASE="$(osrel dd 'ID=fedora')" FLUXVM_INIT=systemd FLUXVM_GLIBC_VERSION=2.39 bash "$INSTALL" --detect 2>&1)"
check "--detect names the family" grep -q "^family=rhel" <<<"$det"
check "--detect names the init system" grep -q "^init=systemd" <<<"$det"
check "--detect names the glibc" grep -q "^glibc=2.39" <<<"$det"

# a Linux package with the service files (off Linux the shim makes install.sh believe it is on x86_64 Linux)
LPLAT=linux-x86_64
mkdir -p "$REL/download/v6.6.6"
d="$ROOT/stage/fluxvm-6.6.6-$LPLAT"; rm -rf "$d"; mkdir -p "$d/bin" "$d/share/systemd" "$d/share/openrc"
printf '#!/bin/sh\necho "fluxctl 6.6.6"\n' > "$d/bin/fluxctl"; printf '#!/bin/sh\necho agent\n' > "$d/bin/fluxvm-agent"; chmod +x "$d/bin/"*
: > "$d/share/config.example.toml"; : > "$d/share/systemd/fluxvm.service"; : > "$d/share/systemd/fluxvm-agent.service"
: > "$d/share/openrc/fluxvm"; : > "$d/share/openrc/fluxvm-agent"
tar -czf "$REL/download/v6.6.6/fluxvm-6.6.6-$LPLAT.tar.gz" -C "$ROOT/stage" "fluxvm-6.6.6-$LPLAT"
printf '%s  fluxvm-6.6.6-%s.tar.gz\n' "$(SUM "$REL/download/v6.6.6/fluxvm-6.6.6-$LPLAT.tar.gz")" "$LPLAT" > "$REL/download/v6.6.6/fluxvm-6.6.6-$LPLAT.tar.gz.sha256"
linst() { PATH="$LSHIM:$PATH" FLUXVM_RELEASE_BASE="file://$REL" HOME="$ROOT/home" "$@"; }
TOK="$ROOT/agent.token"; echo fvjt_secret > "$TOK"

out="$(linst env FLUXVM_INIT=systemd bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l1" --service 2>&1)"
check "systemd: the steps are printed with sudo, not run" grep -q "sudo systemctl enable --now fluxvm" <<<"$out"
check "systemd: the unit is installed" grep -q "sudo install -m 0644 .*share/systemd/fluxvm.service /etc/systemd/system/fluxvm.service" <<<"$out"
check "systemd: nothing was written to /etc" test ! -e /etc/systemd/system/fluxvm.service
check "systemd: no agent unless asked" bash -c '! grep -q fluxvm-agent <<<"$0"' "$out"
out="$(linst env FLUXVM_INIT=openrc bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l2" --service 2>&1)"
check "openrc: the init script is installed and enabled" grep -q "sudo rc-update add fluxvm default" <<<"$out"
check "openrc: the init script comes from share/openrc" grep -q "share/openrc/fluxvm /etc/init.d/fluxvm" <<<"$out"
out="$(linst env FLUXVM_INIT=none bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l3" --service 2>&1)"
check "no init system: it says how to run the daemon by hand" grep -q "fluxctl --config /etc/fluxvm.toml serve" <<<"$out"
out="$(linst env FLUXVM_INIT=systemd bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l4" --service --agent --central https://c.example:7799 --node-name node-1 --advertise-url http://10.0.0.5:7788 --token-file "$TOK" 2>&1)"
check "agent: the env file and token are installed 0644 / 0600" grep -q "install -m 0600 $TOK /etc/fluxvm/agent.token" <<<"$out"
check "agent: the unit is enabled" grep -q "sudo systemctl enable --now fluxvm-agent" <<<"$out"
check "agent: the token's value is never printed" bash -c '! grep -q fvjt_secret <<<"$0"' "$out"
out="$(linst env FLUXVM_INIT=openrc bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l5" --service --agent --central https://c.example:7799 --node-name node-1 --advertise-url http://10.0.0.5:7788 --token-file "$TOK" 2>&1)"
check "agent on openrc: init script enabled" grep -q "sudo rc-update add fluxvm-agent default" <<<"$out"

# --agent validation, before anything is downloaded
agent_err() { linst bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l6" "$@" 2>&1 || true; }
check "--agent without --service is refused" grep -q "needs --service" <<<"$(agent_err --agent)"
check "--agent without its options is refused" grep -q "needs --central" <<<"$(agent_err --service --agent)"
check "a loopback --advertise-url is refused" grep -q "not a loopback" <<<"$(agent_err --service --agent --central http://c:1 --node-name n --advertise-url http://127.0.0.1:7788 --token-file "$TOK")"
check "a bad --node-name is refused" grep -q "lowercase" <<<"$(agent_err --service --agent --central http://c:1 --node-name BAD --advertise-url http://10.0.0.5:7788 --token-file "$TOK")"
check "a missing --token-file is refused" grep -q "does not exist" <<<"$(agent_err --service --agent --central http://c:1 --node-name n --advertise-url http://10.0.0.5:7788 --token-file /nope)"
check "validation failed before anything was installed" test ! -e "$ROOT/l6"

# musl and old-glibc hosts get a clear message instead of a loader error
out="$(linst env FLUXVM_GLIBC_VERSION=2.34 bash "$INSTALL" --version 6.6.6 --prefix "$ROOT/l7" 2>&1)"
check "glibc below the floor warns" grep -q "glibc 2.34; the package needs 2.35" <<<"$out"

if [ "$FAILS" -ne 0 ]; then echo "$FAILS failed"; exit 1; fi
echo "all passed"
