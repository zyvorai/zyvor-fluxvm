#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Tests the .rpm built by scripts/package-linux-pkgs.sh --format rpm from a stand-in tree (it tests the packaging:
# metadata, dependencies, file list, scriptlets and, with docker, install / upgrade / remove in clean containers; not the
# binaries).
#   1. static, needs `rpm` and `nfpm`: name, release, requires, weak requires, file ownership, scriptlet $1 handling
#   2. containers, needs docker: install, upgrade with an edited config, erase, in fedora:43 and Rocky 10
# Without rpm the static part is skipped, without docker the container part is skipped; each says so.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v nfpm >/dev/null || { echo "test-rpm: needs nfpm" >&2; exit 2; }

BASE="${TEST_RPM_DIR:-$(mktemp -d "${HOME}/.cache/fluxvm-rpmtest.XXXXXX" 2>/dev/null || mktemp -d)}"
trap 'rm -rf "$BASE"' EXIT
case "$(uname -m)" in x86_64) uarch=x86_64 ;; aarch64|arm64) uarch=aarch64 ;; *) echo "test-rpm: $(uname -m)" >&2; exit 2 ;; esac
fails=0
bad() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

for v in 0.0.1 0.0.2; do
  s="$BASE/fluxvm-$v-linux-$uarch"
  mkdir -p "$s/bin" "$s/lib/bpf" "$s/share/systemd" "$s/share/completions" "$s/share/guest"
  printf '#!/bin/sh\necho fluxctl %s\n' "$v" > "$s/bin/fluxctl"; chmod 755 "$s/bin/fluxctl"
  ln -s fluxctl "$s/bin/fluxvm"
  : > "$s/lib/bpf/fluxvm_tc.bpf.o"
  cp config.example.toml "$s/share/"
  cp packaging/systemd/fluxvm.service "$s/share/systemd/"
  echo "# bash completion" > "$s/share/completions/fluxctl.bash"
  printf '#!/bin/sh\n' > "$s/share/guest/fluxvm-guest-agent"; chmod 755 "$s/share/guest/fluxvm-guest-agent"
  scripts/package-linux-pkgs.sh --stage "$s" --format rpm --out "$BASE/out" >/dev/null
done
R1="$BASE/out/fluxvm-0.0.1-1.$uarch.rpm"
[[ -f "$R1" && -f "$BASE/out/fluxvm-0.0.2-1.$uarch.rpm" && -f "$R1.sha256" ]] || { echo "FAIL: rpm or .sha256 not named fluxvm-<ver>-1.$uarch.rpm" >&2; exit 1; }

if command -v rpm >/dev/null; then
  q() { rpm -qp "$@" "$R1"; }
  [[ "$(q --queryformat '%{NAME} %{VERSION} %{RELEASE} %{ARCH}')" == "fluxvm 0.0.1 1 $uarch" ]] || bad "name/version/release/arch"
  req="$(q --requires)"
  for d in hivex-libs systemd-libs iproute qemu-img qemu-kvm 'glibc >= 2.35'; do grep -qxF "$d" <<<"$req" || bad "Requires lacks '$d'"; done
  grep -q 'libhivex0\|libc6\|qemu-utils' <<<"$req" && bad "deb dependency names leaked into the rpm"
  rec="$(q --recommends)"
  for d in nftables bpftool dnsmasq swtpm; do grep -qxF "$d" <<<"$rec" || bad "Recommends lacks '$d'"; done
  files="$(q -l)"
  for f in /usr/bin/fluxctl /usr/bin/fluxvm /usr/lib/systemd/system/fluxvm.service /usr/lib/fluxvm/bpf/fluxvm_tc.bpf.o \
           /usr/lib/modprobe.d/fluxvm.conf /usr/lib/modules-load.d/fluxvm.conf /usr/share/fluxvm/config.example.toml \
           /usr/share/fluxvm/upgrade.conf.example /usr/share/bash-completion/completions/fluxctl; do
    grep -qxF "$f" <<<"$files" || bad "file list lacks $f"
  done
  # The node's real config and the state directory are created by the scriptlet, never owned by the package.
  grep -qE '^/etc/|^/var/' <<<"$files" && bad "package owns something under /etc or /var"
  # Directories the package owns must be 0755: Fedora's systemd-udev owns /usr/lib/modprobe.d with 0755 and rpm refuses a clash.
  rpm -qplv "$R1" | awk '$1 ~ /^d/ && $1 != "drwxr-xr-x" { print; bad_d=1 } END { exit bad_d }' || bad "a directory in the rpm is not 0755"
  q --dump | awk '{ if ($6 != "root" || $7 != "root") exit 1 }' || bad "a file is not owned by root"
  sc="$(q --scripts)"
  grep -q 'remove|0)' <<<"$sc" || bad "preuninstall does not handle rpm's 0"
  grep -q '2) upgrading=1' <<<"$sc" || bad "postinstall does not handle rpm's 2"
  rpm -K "$R1" | grep -q 'digests OK' || bad "rpm -K digests"
else
  echo "test-rpm: rpm not installed, skipping the static checks"
fi

if docker info >/dev/null 2>&1; then
  for image in fedora:43 quay.io/rockylinux/rockylinux:10; do
    if ! out="$(docker run --rm -v "$BASE/out:/d:ro" "$image" bash -euo pipefail -c '
    fail() { echo "FAIL: $*" >&2; exit 1; }
    dnf install -y -q /d/fluxvm-0.0.1-1.*.rpm >/dev/null
    [ "$(fluxctl)" = "fluxctl 0.0.1" ] || fail "fluxctl not installed"
    for f in /etc/fluxvm.toml /etc/fluxvm/upgrade.conf /etc/qemu/bridge.conf /usr/lib/fluxvm/bpf/fluxvm_tc.bpf.o; do [ -e "$f" ] || fail "missing $f"; done
    [ -d /var/lib/fluxvm ] || fail "state dir"
    echo "# edited by the operator" >> /etc/fluxvm.toml
    dnf install -y -q /d/fluxvm-0.0.2-1.*.rpm >/dev/null
    [ "$(fluxctl)" = "fluxctl 0.0.2" ] || fail "upgrade"
    rpm -q fluxvm | grep -q "0.0.2" || fail "old version still installed after upgrade"
    tail -n1 /etc/fluxvm.toml | grep -q "edited by the operator" || fail "upgrade overwrote /etc/fluxvm.toml"
    touch /var/lib/fluxvm/vm-state
    dnf remove -y -q fluxvm >/dev/null
    [ -e /usr/bin/fluxctl ] && fail "erase left the binary"
    [ -e /etc/fluxvm.toml ] && [ -e /var/lib/fluxvm/vm-state ] || fail "erase deleted config or state"
    echo "ok"
    ' 2>&1)" || [[ "${out##*$'\n'}" != ok ]]; then bad "container test in $image: $out"; fi
  done
else
  echo "test-rpm: docker not available, skipping the install tests"
fi

if (( fails )); then echo "test-rpm: $fails failure(s)" >&2; exit 1; fi
echo "test-rpm: ok"
