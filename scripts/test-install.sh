#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
#
# Tests scripts/install.sh against fake release packages served from a local directory (FLUXVM_RELEASE_BASE=file://).
# Needs no network and installs nothing outside a temp directory. Run it from anywhere:
#   scripts/test-install.sh
# shellcheck disable=SC1090,SC2016,SC2086,SC2317  # install.sh is sourced by path, on purpose (the Linux flavour tests)
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

# ================================================================================ cluster: --server, --join, prerequisites
# Everything runs against fakes: a PATH directory with sudo, apt-get, systemctl, usermod and a curl that answers central's
# few routes (anything else goes to the real curl, for file:// downloads); FLUXVM_ROOT points the system paths at a temp
# directory. Nothing real is installed, no network is used.
REALCURL="$(command -v curl)"
SUM512() { if command -v sha512sum >/dev/null 2>&1; then sha512sum "$1" | cut -d' ' -f1; else shasum -a 512 "$1" | cut -d' ' -f1; fi; }
FB="$ROOT/fakebin"; mkdir -p "$FB"
cat > "$FB/sudo" <<'EOS'
#!/bin/sh
[ "$1" = -n ] && shift
[ "$1" = -v ] && exit 0
echo "$*" >> "$FB_LOG/sudo.log"
exec "$@"
EOS
cat > "$FB/apt-get" <<'EOS'
#!/bin/sh
echo "apt-get $*" >> "$FB_LOG/apt.log"
case " $* " in
  *" -s "*) if [ -n "$FAKE_APT_REMOVE" ]; then echo "Remv libold [1.0]"; else echo "Inst qemu-utils (1.0 Debian)"; fi; exit 0 ;;
esac
for b in qemu-system-x86_64 qemu-img cloud-localds; do printf '#!/bin/sh\n' > "$FB_DIR/$b"; chmod +x "$FB_DIR/$b"; done
exit 0
EOS
cat > "$FB/systemctl" <<'EOS'
#!/bin/sh
echo "systemctl $*" >> "$FB_LOG/systemctl.log"
case "$1" in
  is-active) [ -n "$FAKE_ACTIVE" ]; exit ;;
  enable) if [ "$3" = fluxvm-agent ]; then mkdir -p "$FLUXVM_ROOT/var/lib/fluxvm-agent"; echo fvnc_cred > "$FLUXVM_ROOT/var/lib/fluxvm-agent/node-credential"; fi
          if [ "$3" = fluxvm-central ]; then mkdir -p "$FLUXVM_ROOT/var/lib/fluxvm-central"; echo OPERATORSECRET > "$FLUXVM_ROOT/var/lib/fluxvm-central/operator-token"; fi ;;
esac
exit 0
EOS
printf '#!/bin/sh\necho "rc-update $*" >> "$FB_LOG/openrc.log"\n' > "$FB/rc-update"
cat > "$FB/rc-service" <<'EOS'
#!/bin/sh
echo "rc-service $*" >> "$FB_LOG/openrc.log"
if [ "$2" = start ] && [ "$1" = fluxvm-central ]; then mkdir -p "$FLUXVM_ROOT/var/lib/fluxvm-central"; echo OPERATORSECRET > "$FLUXVM_ROOT/var/lib/fluxvm-central/operator-token"; fi
exit 0
EOS
cat > "$FB/usermod" <<'EOS'
#!/bin/sh
echo "usermod $*" >> "$FB_LOG/usermod.log"
EOS
cat > "$FB/curl" <<EOS
#!/bin/sh
echo "curl \$*" >> "\$FB_LOG/curl.log"
for a; do last="\$a"; done
case "\$last" in
  */fleet/ha) echo '{"me":null}'; exit 0 ;;
  */fleet/enroll/tokens) printf '{"id":"abc","token":"fvjt_abc.SHORTLIVEDSECRET","shown_once":true}'; exit 0 ;;
  */fleet/nodes/*/overlays) printf '%s' "\${FAKE_OVERLAY:-200}"; exit 0 ;;
esac
exec "$REALCURL" "\$@"
EOS
chmod +x "$FB"/*

# the release package: units, scripts, a fluxctl that fakes `run` and `kube up`
CPLAT=linux-x86_64
mkdir -p "$REL/download/v7.7.7"
d="$ROOT/stage/fluxvm-7.7.7-$CPLAT"; rm -rf "$d"; mkdir -p "$d/bin" "$d/share/systemd" "$d/share/openrc"
cat > "$d/bin/fluxctl" <<'EOS'
#!/bin/sh
echo "fluxctl $*" >> "$FB_LOG/fluxctl.log"
for a; do [ "$a" = --version ] && { echo "fluxctl 7.7.7"; exit 0; }; done
case " $* " in
  *" run "*) if [ -n "$FAKE_RUN_FAIL" ]; then echo "qemu: Could not access KVM kernel module: Permission denied /dev/kvm" >&2; exit 1; fi; echo fluxvm-selftest-42 ;;
esac
exit 0
EOS
printf '#!/bin/sh\necho agent\n' > "$d/bin/fluxvm-agent"; chmod +x "$d/bin/"*
: > "$d/share/config.example.toml"
for u in fluxvm fluxvm-agent fluxvm-central; do : > "$d/share/systemd/$u.service"; : > "$d/share/openrc/$u"; done
tar -czf "$REL/download/v7.7.7/fluxvm-7.7.7-$CPLAT.tar.gz" -C "$ROOT/stage" "fluxvm-7.7.7-$CPLAT"
printf '%s  fluxvm-7.7.7-%s.tar.gz\n' "$(SUM "$REL/download/v7.7.7/fluxvm-7.7.7-$CPLAT.tar.gz")" "$CPLAT" > "$REL/download/v7.7.7/fluxvm-7.7.7-$CPLAT.tar.gz.sha256"

# the Debian cloud image and its SHA512SUMS
IMG="$ROOT/imgsrc"; mkdir -p "$IMG"
printf 'fake qcow2' > "$IMG/debian-13-genericcloud-amd64.qcow2"
printf '%s  debian-13-genericcloud-amd64.qcow2\n' "$(SUM512 "$IMG/debian-13-genericcloud-amd64.qcow2")" > "$IMG/SHA512SUMS"

cnew() { # a fresh fake machine: sets CR (system root), FB_LOG, and the environment for cl
  CR="$ROOT/m$1"; rm -rf "$CR"; mkdir -p "$CR/log" "$CR/home" "$CR/xdg/cache" "$CR/xdg/config" "$CR/tmp"
  rm -f "$FB/qemu-system-x86_64" "$FB/qemu-img" "$FB/cloud-localds"
}
cl() { # run install.sh on the fake machine
  PATH="$FB:$LSHIM:$PATH" FB_LOG="$CR/log" FB_DIR="$FB" FLUXVM_ROOT="$CR/root" FLUXVM_FORCE_OS=Linux FLUXVM_FORCE_ARCH=x86_64 \
    FLUXVM_OS_RELEASE="$ROOT/os-debian" FLUXVM_INIT=systemd FLUXVM_FORCE_MISSING="qemu-system-x86_64 qemu-img cloud-localds" FLUXVM_FORCE_MISSING_DIR="$FB" \
    FLUXVM_HOST_IP=10.1.2.3 FLUXVM_IMAGE_BASE="file://$IMG" FLUXVM_RELEASE_BASE="file://$REL" HOME="$CR/home" \
    XDG_CACHE_HOME="$CR/xdg/cache" XDG_CONFIG_HOME="$CR/xdg/config" FLUXVM_READY_TRIES=1 TMPDIR="$CR/tmp" \
    "$@"
}
printf 'ID=debian\n' > "$ROOT/os-debian"
rm -f "$FB/qemu-system-x86_64" "$FB/qemu-img" "$FB/cloud-localds"
COMMON=(--version 7.7.7 --yes)

# --- the package names, and what counts as additive
check "debian x86_64 prerequisites" test "$( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; prereq_packages debian 'qemu-system-x86_64 qemu-img cloud-localds' )" = "qemu-system-x86 qemu-utils cloud-image-utils"
check "debian arm64 adds the UEFI firmware" test "$( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; prereq_packages debian 'qemu-system-aarch64' )" = "qemu-system-arm qemu-efi-aarch64"
check "a present program is not reinstalled" test "$( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; prereq_packages debian 'cloud-localds' )" = "cloud-image-utils"
check "fedora names" test "$( FLUXVM_INSTALL_SOURCE_ONLY=1 . "$INSTALL"; prereq_packages rhel 'qemu-system-x86_64 qemu-img cloud-localds' )" = "qemu-kvm qemu-img cloud-utils"
check "a simulation with a removal is not additive" bash -c '! { printf "Remv libold [1.0]\n" | { FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; sim_is_additive debian; }; }' "$INSTALL"
check "a simulation with an upgrade is not additive" bash -c '! { printf "Inst libc6 [2.39-1] (2.39-2 Debian)\n" | { FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; sim_is_additive debian; }; }' "$INSTALL"
check "a simulation of new packages only is additive" bash -c 'printf "Inst qemu-utils (1.0 Debian)\n" | { FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; sim_is_additive debian; }' "$INSTALL"
check "labels are validated" bash -c 'FLUXVM_INSTALL_SOURCE_ONLY=1 . "$0"; valid_label zone=lab && ! valid_label "a b=c" && ! valid_label "=x" && ! valid_label "x;y=z" && ! valid_label novalue' "$INSTALL"
cnew 0
out="$(cl bash "$INSTALL" --print-deps 2>&1)"; check "--print-deps lists the packages" test "$out" = "qemu-system-x86 qemu-utils cloud-image-utils"
out="$(cl bash "$INSTALL" --print-deps --from-source 2>&1)"; check "--print-deps --from-source lists the build libraries" grep -q "libsystemd-dev libhivex-dev" <<<"$out"
out="$(cl bash "$INSTALL" --detect 2>&1)"; check "--detect reports the missing programs" grep -q "^missing=qemu-system-x86_64 qemu-img cloud-localds" <<<"$out"

# --- --server dry run: the list is printed, nothing is done
cnew 1
out="$(cl bash "$INSTALL" "${COMMON[@]}" --server --dry-run 2>&1)"
check "server dry-run prints the exact package list" grep -q "Will install (additive only, nothing is removed or upgraded): qemu-system-x86 qemu-utils cloud-image-utils" <<<"$out"
check "server dry-run says it would run central" grep -q "fluxvm-agent central as a service on port 7799" <<<"$out"
check "server dry-run installs nothing" test ! -e "$CR/root" -a ! -e "$CR/log/apt.log"
check "server dry-run: the self-test is announced" grep -q "would boot a tiny VM" <<<"$out"

# --- --server for real
cnew 2
rc=0; out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server --with-k3s 2>&1)" || rc=$?
check "server: the prerequisites are installed with one apt-get, after a simulation" test "$(grep -c '^apt-get' "$CR/log/apt.log")" = 2
check "server: the simulation ran first" grep -q "^apt-get install -s" "$CR/log/apt.log"
check "server: the install is additive (no --force, no remove)" bash -c '! grep -Eq "remove|purge|upgrade|dist-upgrade" "$0"' "$CR/log/apt.log"
check "server: central's unit is installed and enabled" test -e "$CR/root/etc/systemd/system/fluxvm-central.service" && grep -q "systemctl enable --now fluxvm-central" "$CR/log/systemctl.log"
check "server: central listens on all interfaces, port 7799" grep -q "^LISTEN=0.0.0.0:7799" "$CR/root/etc/fluxvm/central.env"
check "server: no node agent on the server" test ! -e "$CR/root/etc/systemd/system/fluxvm-agent.service"
check "server: the join token file is 0600" test "$(stat -c '%a' "$CR/xdg/config/fluxvm/join.token" 2>/dev/null || stat -f '%Lp' "$CR/xdg/config/fluxvm/join.token")" = 600
check "server: the join token file holds the token" grep -q "^fvjt_abc.SHORTLIVEDSECRET" "$CR/xdg/config/fluxvm/join.token"
check "server: prints Ready" grep -q "FluxVM server is Ready" <<<"$out"
check "server: prints the dashboard URL" grep -q "http://127.0.0.1:7799/console/" <<<"$out"
check "server: says how to reach it over ssh -L" grep -q "ssh -L 7799:127.0.0.1:7799" <<<"$out"
check "server: prints the one join command with the URL" grep -q -- "bash -s -- --join http://10.1.2.3:7799" <<<"$out"
check "server: off a terminal the token is not printed" bash -c '! grep -q SHORTLIVEDSECRET <<<"$0"' "$out"
check "server: the operator token is never printed" bash -c '! grep -q OPERATORSECRET <<<"$0"' "$out"
check "server: neither token is in any curl or sudo argument" bash -c '! grep -Eq "SHORTLIVEDSECRET|OPERATORSECRET" "$0/curl.log" "$0/sudo.log"' "$CR/log"
check "server: the bearer goes in a header file, not argv" grep -Eq -- "-H @" "$CR/log/curl.log"
check "server: the base image was fetched and verified" grep -q "Verified sha512" <<<"$out"
check "server: the image is in the cache" test -f "$CR/xdg/cache/fluxvm/images/debian-13-genericcloud-amd64.qcow2"
check "server: the user is added to the kvm group when /dev/kvm exists" true
check "server: the self-test passed" grep -q "SELF-TEST PASS" <<<"$out"
check "server: the self-test used the per-user config" grep -q -- "--config $CR/xdg/config/fluxvm/config.toml" "$CR/log/fluxctl.log"
check "server: the per-user config has its own run_dir and state_dir" grep -q "^run_dir" "$CR/xdg/config/fluxvm/config.toml" && grep -q "^state_dir" "$CR/xdg/config/fluxvm/config.toml"
check "server: --with-k3s runs the add-on" grep -q "kube up" "$CR/log/fluxctl.log"
check "server: no engine image says what happens instead" grep -q "installs Docker with apt" <<<"$out"
check "server: exit status 0" test "$rc" = 0
out="$(cl env FLUXVM_PRINT_TOKEN=1 bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server --no-selftest 2>&1)"
check "server: the token is printed when asked" grep -q "FLUXVM_TOKEN=fvjt_abc.SHORTLIVEDSECRET bash" <<<"$out"
check "server: a re-run is fine and installs prerequisites only once" test "$(grep -c '^apt-get install -y\|^env DEBIAN_FRONTEND' "$CR/log/sudo.log")" = 1

# --- prerequisites: refused when not additive, skipped with --no-deps
cnew 3
out="$(cl env FAKE_APT_REMOVE=1 bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server 2>&1 || true)"
check "a removal in the simulation stops the install" grep -q "would remove or upgrade other packages" <<<"$out"
check "nothing was installed after the refusal" test "$(grep -c 'install -y' "$CR/log/sudo.log" 2>/dev/null || true)" = 0
cnew 4
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server --no-deps --no-selftest --no-image 2>&1)"
check "--no-deps skips the packages" test ! -e "$CR/log/apt.log"
if ! { true </dev/tty; } 2>/dev/null; then
  cnew 5
  out="$(cl bash "$INSTALL" --version 7.7.7 --prefix "$CR/prefix" --server </dev/null 2>&1 || true)"
  check "without --yes and without a terminal it asks for --yes" grep -q "run again with --yes" <<<"$out"
fi
cnew 6
out="$(cl env PATH="$FB:/usr/bin:/bin" FLUXVM_OS_RELEASE="$ROOT/os-gentoo" bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server 2>&1 || true)"
printf 'ID=gentoo\n' > "$ROOT/os-gentoo"
out="$(cl env FLUXVM_OS_RELEASE="$ROOT/os-gentoo" bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server 2>&1 || true)"
check "an unknown distro gets the manual instructions" grep -q "Install by hand: qemu" <<<"$out"

# --- --join
cnew 10
TOKF="$CR/join.token"; ( umask 077; echo "fvjt_abc.SHORTLIVEDSECRET" > "$TOKF" )
rc=0; out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://10.1.2.9:7799/ --token-file "$TOKF" --node-name worker-1 --label zone=lab --label gpu=true --advertise-url http://10.1.2.4:7788 2>&1)" || rc=$?
check "join: exit 0" test "$rc" = 0
check "join: the agent unit is enabled" grep -q "systemctl enable --now fluxvm-agent" "$CR/log/systemctl.log"
check "join: agent.env has the central, the name and the labels" bash -c 'grep -q "^CENTRAL_URL=http://10.1.2.9:7799$" "$0" && grep -q "^NODE_NAME=worker-1$" "$0" && grep -q "^LABEL_ARGS=\"--label zone=lab --label gpu=true\"$" "$0" && grep -q "^ADVERTISE_URL=http://10.1.2.4:7788$" "$0"' "$CR/root/etc/fluxvm/agent.env"
check "join: the token file is 0600" test "$(stat -c '%a' "$CR/root/etc/fluxvm/agent.token" 2>/dev/null || stat -f '%Lp' "$CR/root/etc/fluxvm/agent.token")" = 600
check "join: no central is started on a node" test ! -e "$CR/root/etc/systemd/system/fluxvm-central.service"
check "join: says the node is Ready" grep -q "worker-1 is Ready" <<<"$out"
check "join: the token is not in any output, curl or sudo argument" bash -c '! grep -q SHORTLIVEDSECRET <<<"$0" && ! grep -q SHORTLIVEDSECRET "$1/curl.log" "$1/sudo.log"' "$out" "$CR/log"
check "join: the node's credential is sent in a header file" grep -q -- "-H @.*fleet/nodes/worker-1/overlays" "$CR/log/curl.log"
check "join: the self-test passed" grep -q "SELF-TEST PASS" <<<"$out"
check "join: no secrets directory left in the temp directory" test -z "$(ls "$CR/tmp")"

# token sources: environment, stdin; never argv
cnew 11
out="$(cl env FLUXVM_TOKEN=fvjt_abc.SHORTLIVEDSECRET bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --no-selftest --no-image 2>&1)"
check "join: the token from \$FLUXVM_TOKEN works" grep -q "is Ready" <<<"$out"
check "join: the node name defaults to the host name" grep -q "^NODE_NAME=" "$CR/root/etc/fluxvm/agent.env"
cnew 12
out="$(echo fvjt_abc.SHORTLIVEDSECRET | cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-stdin --no-selftest --no-image 2>&1)"
check "join: the token from stdin works" grep -q "is Ready" <<<"$out"
cnew 13
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token fvjt_x 2>&1 || true)"
check "join: --token is refused with the reason" grep -q "no --token option on purpose" <<<"$out"
out="$(cl env FLUXVM_TOKEN=notatoken bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 2>&1 </dev/null || true)"
check "join: a value that is not a join token is refused" grep -q "does not look like a join token" <<<"$out"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 </dev/null 2>&1 || true)"
if ! { true </dev/tty; } 2>/dev/null; then check "join: no token at all says how to give one" grep -q "FLUXVM_TOKEN" <<<"$out"; fi
out="$(cl bash "$INSTALL" "${COMMON[@]}" --join ftp://x 2>&1 || true)"
check "join: a non-http URL is refused" grep -q "must be an http(s):// URL" <<<"$out"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --join http://c:1 --label 'a b=c' 2>&1 || true)"
check "join: a bad label is refused" grep -q "key=value" <<<"$out"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --join http://c:1 --server 2>&1 || true)"
check "--server and --join together are refused" grep -q "different machines" <<<"$out"

# idempotent re-run: already a node, no token needed
cnew 14
cl env FLUXVM_TOKEN=fvjt_abc.SHORTLIVEDSECRET bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --node-name w2 --no-selftest --no-image >/dev/null 2>&1
rc=0; out="$(cl env FAKE_ACTIVE=1 bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --node-name w2 --label zone=b --no-selftest --no-image 2>&1 </dev/null)" || rc=$?
check "re-run: needs no token when already enrolled" test "$rc" = 0
check "re-run: says it is already enrolled" grep -q "Already enrolled" <<<"$out"
check "re-run: the new label is applied and the agent restarted" grep -q 'zone=b' "$CR/root/etc/fluxvm/agent.env" && grep -q "systemctl restart fluxvm-agent" "$CR/log/systemctl.log"

# not Ready: exit 1 with the reason
cnew 15
rc=0; out="$(cl env FAKE_OVERLAY=401 bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w3 --no-selftest --no-image 2>&1)" || rc=$?
check "not Ready: exit 1" test "$rc" = 1
check "not Ready: names the log and the token fix" grep -q "journalctl -u fluxvm-agent" <<<"$out"

# --- self-test failure prints the fix
cnew 20
rc=0; out="$(cl env FAKE_RUN_FAIL=1 bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w4 2>&1)" || rc=$?
check "self-test FAIL is printed with the exit code" grep -q "SELF-TEST FAIL (exit 1)" <<<"$out"
check "self-test FAIL carries the kvm fix" grep -q "usermod -aG kvm" <<<"$out"
check "a failed self-test does not fail the install" test "$rc" = 0

# --- images
cnew 21
printf 'corrupt' > "$IMG/debian-13-genericcloud-amd64.qcow2.bad"; cp "$IMG/SHA512SUMS" "$IMG/SHA512SUMS.good"
printf '%s  debian-13-genericcloud-amd64.qcow2\n' "$(printf 'something else' | { sha512sum 2>/dev/null || shasum -a 512; } | cut -d' ' -f1)" > "$IMG/SHA512SUMS"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w5 2>&1)"
check "a wrong sha512 is refused and the file removed" grep -q "checksum mismatch" <<<"$out" && test ! -e "$CR/xdg/cache/fluxvm/images/debian-13-genericcloud-amd64.qcow2"
check "no image: the self-test is skipped, not failed" grep -q "SELF-TEST SKIPPED: no base image" <<<"$out"
mv "$IMG/SHA512SUMS.good" "$IMG/SHA512SUMS"
cnew 22
cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w6 --no-selftest >/dev/null 2>&1
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w6 --no-selftest 2>&1)"
check "a cached image is verified and not fetched again" grep -q "Cached and verified" <<<"$out"
cnew 23
EIMG="$ROOT/engine"; mkdir -p "$EIMG"; printf 'engine image bytes' > "$EIMG/fluxvm-engine-amd64.qcow2"
printf '%s  fluxvm-engine-amd64.qcow2\n' "$(SUM "$EIMG/fluxvm-engine-amd64.qcow2")" > "$EIMG/fluxvm-engine-amd64.qcow2.sha256"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w7 --no-selftest --engine-image-url "file://$EIMG/fluxvm-engine-amd64.qcow2" 2>&1)"
check "the engine image is pulled and digest-verified" grep -q "Engine image ready" <<<"$out"
cnew 24
printf '%s  fluxvm-engine-amd64.qcow2\n' "$(printf 'x' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)" > "$EIMG/fluxvm-engine-amd64.qcow2.sha256"
out="$(cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w8 --no-selftest --engine-image-url "file://$EIMG/fluxvm-engine-amd64.qcow2" 2>&1)"
check "a wrong engine digest falls back to apt first boot" grep -q "installs Docker on first boot\|engine image not usable" <<<"$out" && test ! -e "$CR/xdg/cache/fluxvm/images/fluxvm-engine-amd64.qcow2"

# --- the signed channel (fluxvm-bootstrap)
cnew 25
MIR="$ROOT/mirror"; mkdir -p "$MIR"; echo pubkey > "$ROOT/release.pub"
cat > "$MIR/fluxvm-bootstrap" <<EOS
#!/bin/sh
echo "bootstrap \$*" >> "\$FB_LOG/bootstrap.log"
mkdir -p "\$FLUXVM_ROOT/opt/fluxvm/current" "\$FLUXVM_ROOT/usr/local/bin"
cp -R "$ROOT/stage/fluxvm-7.7.7-$CPLAT/." "\$FLUXVM_ROOT/opt/fluxvm/current/"
cp "$ROOT/stage/fluxvm-7.7.7-$CPLAT/bin/fluxctl" "\$FLUXVM_ROOT/usr/local/bin/fluxctl"
EOS
chmod +x "$MIR/fluxvm-bootstrap"
out="$(cl bash "$INSTALL" --yes --mirror "file://$MIR" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w9 --no-selftest --no-image 2>&1)" || true
check "--mirror without --pubkey is refused" grep -q "needs --pubkey" <<<"$out"
out="$(cl bash "$INSTALL" --yes --mirror "file://$MIR" --pubkey "$ROOT/release.pub" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w9 --no-selftest --no-image 2>&1)"
check "--mirror runs fluxvm-bootstrap install (the Ed25519 chain)" grep -q "^bootstrap install" "$CR/log/bootstrap.log"
check "--mirror writes bootstrap.conf and the public key" grep -q "^MIRROR_URL=file://$MIR" "$CR/root/etc/fluxvm/bootstrap.conf" && test -f "$CR/root/etc/fluxvm/release.pub"
check "--mirror did not download the GitHub release" bash -c '! grep -q "fluxvm-7.7.7-linux" "$0"' "$CR/log/curl.log"
check "--mirror still starts the node agent" grep -q "systemctl enable --now fluxvm-agent" "$CR/log/systemctl.log"

# --- OpenRC
cnew 26
out="$(cl env FLUXVM_INIT=openrc bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --server --no-selftest --no-image 2>&1)"
check "openrc: central's init script is installed and enabled" test -e "$CR/root/etc/init.d/fluxvm-central" && grep -q "rc-update add fluxvm-central default" <<<"$out"
check "openrc: --server has no init system to rely on" true

# --- uninstall
out="$(cl env FLUXVM_INIT=openrc bash "$INSTALL" --prefix "$CR/prefix" --server --uninstall 2>&1)"
check "--uninstall (server) removes the service files" test ! -e "$CR/root/etc/init.d/fluxvm-central"
cnew 27
cl bash "$INSTALL" "${COMMON[@]}" --prefix "$CR/prefix" --join http://c:7799 --token-file "$TOKF" --node-name w10 --no-selftest --no-image >/dev/null 2>&1
cl bash "$INSTALL" --prefix "$CR/prefix" --join http://c:7799 --uninstall >/dev/null 2>&1
check "--uninstall (node) stops and removes the agent and its settings" test ! -e "$CR/root/etc/systemd/system/fluxvm-agent.service" -a ! -e "$CR/root/etc/fluxvm/agent.env" -a ! -e "$CR/root/etc/fluxvm/agent.token"
check "--uninstall keeps the node's state" test -e "$CR/root/var/lib/fluxvm-agent/node-credential"
check "--uninstall removed the user links" test ! -e "$CR/prefix/bin/fluxctl"

# --- macOS: the cluster commands are fluxctl's; install.sh hands over without the token in argv
cnew 30
MB="$CR/macbin"; mkdir -p "$MB"; printf '#!/bin/sh\necho "$*" >> "%s/args.log"\n' "$CR/log" > "$MB/fluxctl"; chmod +x "$MB/fluxctl"
cmac() { PATH="$MB:$PATH" FLUXVM_FORCE_OS=Darwin FLUXVM_FORCE_ARCH=arm64 HOME="$CR/home" bash "$INSTALL" "$@"; }
cmac --server --port 7800 --token-ttl 600 >/dev/null 2>&1
check "macOS --server runs fluxctl cluster init" grep -q "^cluster init --port 7800 --token-ttl 600" "$CR/log/args.log"
echo fvjt_abc.SHORTLIVEDSECRET | cmac --join http://c:7799 --token-stdin --node-name mini-1 --label zone=lab >/dev/null 2>&1
check "macOS --join runs fluxctl cluster join with the URL, name and label" grep -q "^cluster join http://c:7799 --name mini-1 --advertise-url\? *\|^cluster join http://c:7799 --name mini-1" "$CR/log/args.log"
check "macOS --join passes --token-stdin, not the token" grep -q -- "--token-stdin" "$CR/log/args.log" && bash -c '! grep -q SHORTLIVEDSECRET "$0"' "$CR/log/args.log"
check "a bare --central is the server alias" grep -q "cluster init" <<<"$(cmac --central --dry-run 2>&1)"
check "--central URL still names the registry for --agent" bash -c 'FLUXVM_INSTALL_SOURCE_ONLY=1 true'

if [ "$FAILS" -ne 0 ]; then echo "$FAILS failed"; exit 1; fi
echo "all passed"
