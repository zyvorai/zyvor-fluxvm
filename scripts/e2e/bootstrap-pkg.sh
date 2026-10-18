#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# fluxvm-bootstrap in deb or rpm mode, end to end, in a real distro container: a signed mirror, a first install, an
# upgrade, a release that does not come up (automatic rollback), a manual rollback, pin, verify, and the status file.
#   scripts/e2e/bootstrap-pkg.sh deb [ubuntu:22.04]     scripts/e2e/bootstrap-pkg.sh rpm [fedora:43]
# Needs nfpm, docker (or podman as `docker`), openssl >= 3, python3, jq. Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
MODE="${1:?deb|rpm}"; IMAGE="${2:-$([[ $MODE == deb ]] && echo ubuntu:22.04 || echo fedora:43)}"
for t in nfpm docker openssl python3 jq; do command -v "$t" >/dev/null || { echo "needs $t" >&2; exit 2; }; done
T="$(mktemp -d "${HOME}/.cache/fluxvm-bspkg.XXXXXX" 2>/dev/null || mktemp -d)"; fails=0; CT=""; SRV=""
trap '[[ -n "$CT" ]] && docker rm -f "$CT" >/dev/null 2>&1; [[ -n "$SRV" ]] && kill "$SRV" 2>/dev/null; rm -rf "$T"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
case "$(uname -m)" in x86_64) U=x86_64; DA=amd64 ;; *) U=aarch64; DA=arm64 ;; esac

mkpkg() { # VERSION [broken]
  local v="$1" s="$T/stage/fluxvm-$1-linux-$U"
  rm -rf "$s"; mkdir -p "$s/bin" "$s/lib/bpf" "$s/share/systemd" "$s/share/completions" "$s/share/guest"
  printf '#!/bin/sh\necho "fluxctl %s"\n' "${2:+broken}${2:-$v}" > "$s/bin/fluxctl"; chmod 755 "$s/bin/fluxctl"
  ln -s fluxctl "$s/bin/fluxvm"; : > "$s/lib/bpf/x.o"; cp config.example.toml "$s/share/"
  cp packaging/systemd/fluxvm.service "$s/share/systemd/"; echo c > "$s/share/completions/fluxctl.bash"
  printf '#!/bin/sh\n' > "$s/share/guest/fluxvm-guest-agent"; chmod 755 "$s/share/guest/fluxvm-guest-agent"
  scripts/package-linux-pkgs.sh --stage "$s" --format "$MODE" --out "$T/art" >/dev/null
}
publish() { scripts/make-release-manifest.sh --version "$1" --artifacts "$T/art" --key "$T/k.pem" --out "$T/mirror" >/dev/null; }

openssl genpkey -algorithm ed25519 -out "$T/k.pem" 2>/dev/null; openssl pkey -in "$T/k.pem" -pubout -out "$T/k.pub"
mkdir -p "$T/mirror" "$T/art"
PORT=$((20000 + RANDOM % 20000))
( cd "$T/mirror" && exec python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1 ) & SRV=$!
mkpkg 0.0.1; mkpkg 0.0.2; mkpkg 0.0.3 broken
# only the real versions exist in the artifact dir when each release is published
mkdir -p "$T/all"; mv "$T/art"/* "$T/all/"
stage_art() { rm -rf "$T/art"; mkdir -p "$T/art"; cp "$T/all/"*"$1"* "$T/art/" 2>/dev/null; }
sleep 1

if [[ $MODE == deb ]]; then PREP='apt-get update -qq >/dev/null && apt-get install -y -qq curl openssl jq util-linux ca-certificates >/dev/null'; QV='dpkg-query -W -f="\${Version}" fluxvm'
else PREP='dnf install -y -q curl openssl jq util-linux >/dev/null'; QV='rpm -q --qf "%{VERSION}" fluxvm'; fi
CT="$(docker run -d --network host -v "$PWD/scripts/fluxvm-bootstrap.sh:/bs:ro" -v "$T/k.pub:/k.pub:ro" "$IMAGE" sleep infinity)"
docker exec "$CT" bash -c "$PREP" || { echo "container prep failed" >&2; exit 2; }
BS() { docker exec -e MIRROR_URL="http://127.0.0.1:$PORT" -e PUBKEY=/k.pub -e NODE_ID=e2e-node -e JITTER_MAX=0 -e HEALTH_WAIT=3 \
        -e SKIP_KVM_CHECK=1 -e BOOTSTRAP_CONF=/nonexistent -e INSTALL_MODE="$MODE" "$CT" bash /bs "$@"; }
inst() { docker exec "$CT" bash -c "$QV" 2>/dev/null; }
state() { BS status | jq -r .state; }

stage_art 0.0.1; publish 0.0.1
BS install >"$T/i1.log" 2>&1; rc=$?
[[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/i1.log"
check "first install of 0.0.1 from the signed mirror (rc=$rc)" test "$(inst)" = 0.0.1
check "fluxctl from the package runs" test "$(docker exec "$CT" fluxctl)" = "fluxctl 0.0.1"
check "status says updated" test "$(state)" = updated
BS install >"$T/i1b.log" 2>&1
check "a second run changes nothing" test "$(inst)" = 0.0.1 -a "$(grep -c 'installing' "$T/i1b.log")" = 0

stage_art 0.0.2; publish 0.0.2
BS install >"$T/i2.log" 2>&1
check "upgrade to 0.0.2" test "$(inst)" = 0.0.2
check "verify passes on the installed package" docker exec "$CT" bash -c "$([[ $MODE == deb ]] && echo 'dpkg -V fluxvm | grep -v " c " | grep . && exit 1 || exit 0' || echo 'rpm -V fluxvm --noconfig 2>&1 | grep -v "^$" && exit 1 || exit 0')"
check "bootstrap verify exits 0" BS verify

stage_art 0.0.3; publish 0.0.3
BS install >"$T/i3.log" 2>&1; rc=$?
check "a release that does not come up exits 3" test "$rc" = 3
check "...and the node is back on 0.0.2" test "$(inst)" = 0.0.2
check "...status says rolled-back" test "$(state)" = rolled-back
BS install >"$T/i4.log" 2>&1
check "the failed version is not retried" test "$(state)" = skipped -a "$(inst)" = 0.0.2

BS pin >/dev/null; check "pin holds the node" test "$(BS status | jq -r .state)" = pinned
BS unpin >/dev/null
check "the previous package was kept in the cache for rollback" docker exec "$CT" bash -c "ls /var/lib/fluxvm-bootstrap/cache | grep -q 0.0.1"
BS rollback >"$T/rb.log" 2>&1
check "manual rollback goes to 0.0.1" test "$(inst)" = 0.0.1
check "the cache keeps at most two packages" test "$(docker exec "$CT" bash -c 'ls /var/lib/fluxvm-bootstrap/cache | grep -c "\.\(deb\|rpm\)$"')" -le 2
check "...and no manifests of versions that are gone" test "$(docker exec "$CT" bash -c 'ls /var/lib/fluxvm-bootstrap/cache | grep -c "^manifest-.*json$"')" -le 2

echo; echo "$fails failed"; exit "$fails"
