#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Install / upgrade / remove / purge of the .deb in a clean ubuntu:22.04 container (needs docker). Builds two fake
# versions from a stand-in tree, so it tests the packaging (maintainer scripts, conffile policy, file ownership),
# not the binaries. Run from anywhere; CI runs it in packaging-checks.yml.
set -euo pipefail
cd "$(dirname "$0")/.."
for t in nfpm docker; do command -v "$t" >/dev/null || { echo "test-deb: needs $t" >&2; exit 2; }; done

# Docker on macOS only shares the home directory, so work under it there.
BASE="${TEST_DEB_DIR:-$(mktemp -d "${HOME}/.cache/fluxvm-debtest.XXXXXX" 2>/dev/null || mktemp -d)}"
trap 'rm -rf "$BASE"' EXIT
arch="$(docker run --rm ubuntu:22.04 dpkg --print-architecture)"
case "$arch" in amd64) uarch=x86_64 ;; arm64) uarch=aarch64 ;; *) echo "test-deb: $arch" >&2; exit 2 ;; esac

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
  scripts/package-linux-pkgs.sh --stage "$s" --out "$BASE/out" >/dev/null
done

docker run --rm -v "$BASE/out:/d:ro" ubuntu:22.04 bash -euo pipefail -c '
fail() { echo "FAIL: $*" >&2; exit 1; }
apt-get update -qq >/dev/null
apt-get install -y -qq /d/fluxvm_0.0.1_*.deb >/dev/null
[ "$(fluxctl)" = "fluxctl 0.0.1" ] || fail "fluxctl not installed"
[ -L /usr/bin/fluxvm ] || fail "fluxvm link"
grep -q "^ExecStart=/usr/bin/fluxctl " /usr/lib/systemd/system/fluxvm.service || fail "unit ExecStart"
for f in /etc/fluxvm.toml /etc/fluxvm/upgrade.conf /etc/qemu/bridge.conf /usr/lib/modprobe.d/fluxvm.conf /usr/lib/modules-load.d/fluxvm.conf \
         /usr/lib/fluxvm/bpf/fluxvm_tc.bpf.o /usr/share/bash-completion/completions/fluxctl; do [ -e "$f" ] || fail "missing $f"; done
[ -d /var/lib/fluxvm ] || fail "state dir"

echo "# edited by the operator" >> /etc/fluxvm.toml
apt-get install -y -qq /d/fluxvm_0.0.2_*.deb >/dev/null
[ "$(fluxctl)" = "fluxctl 0.0.2" ] || fail "upgrade"
tail -n1 /etc/fluxvm.toml | grep -q "edited by the operator" || fail "upgrade overwrote /etc/fluxvm.toml"
dpkg -V fluxvm | grep -v " c " | grep . && fail "dpkg -V reports changes" || true

touch /var/lib/fluxvm/vm-state
apt-get remove -y -qq fluxvm >/dev/null
[ -e /usr/bin/fluxctl ] && fail "remove left the binary"
[ -e /etc/fluxvm.toml ] && [ -e /var/lib/fluxvm/vm-state ] || fail "remove deleted config or state"
apt-get purge -y -qq fluxvm >/dev/null
[ -e /var/lib/fluxvm/vm-state ] || fail "purge deleted the state directory"
[ -e /etc/fluxvm.toml ] || fail "purge deleted /etc/fluxvm.toml"
echo "test-deb: ok"
'
