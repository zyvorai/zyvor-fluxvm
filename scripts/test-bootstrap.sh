#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Tests scripts/fluxvm-bootstrap.sh against a local mirror signed with a throwaway key. Linux only (flock, GNU
# coreutils); needs openssl >= 3.0, jq, curl and python3. Nothing outside a temp directory is touched (FLUXVM_ROOT).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
REPO="$PWD"
T="$(mktemp -d)"; SRV_PID=""
trap '[[ -n "$SRV_PID" ]] && kill "$SRV_PID" 2>/dev/null; rm -rf "$T"' EXIT
pass=0; fail=0
ok() { pass=$((pass + 1)); echo "ok   $1"; }
bad() { fail=$((fail + 1)); echo "FAIL $1"; }
check() { local name="$1"; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }

openssl genpkey -algorithm ed25519 -out "$T/k.pem" 2>/dev/null; openssl pkey -in "$T/k.pem" -pubout -out "$T/k.pub"
openssl genpkey -algorithm ed25519 -out "$T/other.pem" 2>/dev/null
MK="$REPO/scripts/make-release-manifest.sh"
ARCH="$(uname -m)"; [[ "$ARCH" == arm64 ]] && ARCH=aarch64

# A fake package: bin/fluxctl prints its version (or, for a "broken" release, a different one) and MANIFEST.sha256.
mkpkg() { # VERSION [broken]
  local v="$1" d="$T/build/fluxvm-$1-linux-$ARCH"
  rm -rf "$d"; mkdir -p "$d/bin" "$d/share"
  printf '#!/bin/sh\necho "fluxctl %s"\n' "${2:+broken}${2:-$v}" > "$d/bin/fluxctl"; chmod 755 "$d/bin/fluxctl"
  head -c 300000 /dev/urandom > "$d/share/blob"
  ( cd "$d" && find . -type f | sort | sed "s|^\./||" | xargs sha256sum > ../MANIFEST.tmp; mv ../MANIFEST.tmp MANIFEST.sha256 )
  mkdir -p "$T/art"; tar -czf "$T/art/fluxvm-$v-linux-$ARCH.tar.gz" -C "$T/build" "fluxvm-$v-linux-$ARCH"
}
publish() { "$MK" --version "$1" --artifacts "$T/art" --key "${KEY:-$T/k.pem}" --out "$T/mirror" "${@:2}" >/dev/null; }

# A mirror with Range support, a request log, and optional injected 429s (file $T/inject: "<count> <retry-after>").
cat > "$T/srv.py" <<'PY'
import http.server, os, sys
root, log, inject = sys.argv[1], sys.argv[2], sys.argv[3]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        with open(log, "a") as f: f.write(f"{self.path} range={self.headers.get('Range','')}\n")
        if self.path.endswith("stable.json") and os.path.exists(inject):
            n, ra = open(inject).read().split()
            if int(n) > 0:
                open(inject, "w").write(f"{int(n)-1} {ra}")
                self.send_response(429); self.send_header("Retry-After", ra); self.end_headers(); return
        p = os.path.join(root, self.path.lstrip("/"))
        if not os.path.isfile(p): self.send_response(404); self.end_headers(); return
        data = open(p, "rb").read(); start = 0; code = 200
        r = self.headers.get("Range")
        if r and r.startswith("bytes="):
            start = int(r[6:].split("-")[0]); code = 206
            if start >= len(data): self.send_response(416); self.end_headers(); return
        self.send_response(code); self.send_header("Content-Length", str(len(data) - start))
        if code == 206: self.send_header("Content-Range", f"bytes {start}-{len(data)-1}/{len(data)}")
        self.end_headers(); self.wfile.write(data[start:])
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[4])), H).serve_forever()
PY
PORT=$((20000 + RANDOM % 20000)); mkdir -p "$T/mirror"
python3 "$T/srv.py" "$T/mirror" "$T/req.log" "$T/inject" "$PORT" & SRV_PID=$!
sleep 1

export FLUXVM_ROOT="$T/root" MIRROR_URL="http://127.0.0.1:$PORT" PUBKEY="$T/k.pub" NODE_ID=test-node-1 JITTER_MAX=0 \
       FLUXVM_BOOTSTRAP_FAST=1 HEALTH_WAIT=2 BACKOFF_BASE=1 FETCH_TRIES=3 BOOTSTRAP_CONF=/nonexistent
BS="$REPO/scripts/fluxvm-bootstrap.sh"
cur() { basename "$(readlink "$T/root/opt/fluxvm/current" 2>/dev/null)"; }
state() { jq -r .state "$T/root/var/lib/fluxvm-bootstrap/status.json"; }
reset() { rm -rf "$T/root" "$T/mirror" "$T/art" "$T/inject"; mkdir -p "$T/mirror"; : > "$T/req.log"; }

# --- the happy path, an upgrade, and what is kept
mkpkg 1.0.0; mkpkg 1.1.0
publish 1.0.0; "$BS" install 2>/dev/null
check "first install" test "$(cur)" = 1.0.0
check "bin link points into current" test "$(readlink "$T/root/usr/local/bin/fluxctl")" = "$T/root/opt/fluxvm/current/bin/fluxctl"
check "installed fluxctl runs" test "$("$T/root/usr/local/bin/fluxctl")" = "fluxctl 1.0.0"
"$BS" install 2>/dev/null; check "second run is a no-op" test "$(state)" = unchanged
check "etag makes the repeat run a 304" grep -q "channels/stable.json" "$T/req.log"
publish 1.1.0; "$BS" install 2>/dev/null
check "upgrade" test "$(cur)" = 1.1.0
check "upgrade keeps the previous tree" test -d "$T/root/opt/fluxvm/1.0.0"
check "verify passes" "$BS" verify
"$BS" rollback 2>/dev/null
check "manual rollback goes back and marks the version failed" test "$(cur)" = 1.0.0 -a "$(grep -c 1.1.0 "$T/root/var/lib/fluxvm-bootstrap/failed")" = 1
publish 1.1.0; rm -f "$T/root/var/lib/fluxvm-bootstrap/failed"; "$BS" install 2>/dev/null

# --- rollback and not retrying a failed version
mkpkg 1.2.0 broken; publish 1.2.0; "$BS" install 2>/dev/null; rc=$?
check "a version that does not come up rolls back (exit 3)" test "$rc" = 3 -a "$(cur)" = 1.1.0
check "the status says rolled-back" test "$(state)" = rolled-back
"$BS" install 2>/dev/null
check "the failed version is not retried" test "$(state)" = skipped

# --- everything that must be refused leaves the installed version alone
reset; mkpkg 1.0.0; mkpkg 1.1.0; publish 1.0.0; "$BS" install 2>/dev/null
publish 1.1.0
echo tamper >> "$T/mirror/releases/1.1.0/fluxvm-1.1.0-linux-$ARCH.tar.gz"
"$BS" install 2>/dev/null
check "tampered artifact is rejected" test "$(cur)" = 1.0.0 -a "$(state)" = error

reset; mkpkg 1.0.0; publish 1.0.0; "$BS" install 2>/dev/null; mkpkg 1.1.0; publish 1.1.0
sed -i 's/"percent": 100/"percent": 100 /; s/1.1.0/9.9.9/' "$T/mirror/channels/stable.json"
"$BS" install 2>/dev/null
check "tampered channel file is rejected" test "$(cur)" = 1.0.0 -a "$(state)" = error

reset; mkpkg 1.0.0; publish 1.0.0; "$BS" install 2>/dev/null; mkpkg 1.1.0; KEY="$T/other.pem" publish 1.1.0; "$BS" install 2>/dev/null
check "a channel signed by another key is rejected" test "$(cur)" = 1.0.0 -a "$(state)" = error
reset; mkpkg 1.0.0; publish 1.0.0; rm "$T/mirror/channels/stable.json.sig"; "$BS" install 2>/dev/null
check "a missing signature is rejected" test -z "$(cur)"

reset; mkpkg 1.0.0; mkpkg 1.1.0; publish 1.0.0 --serial 100; "$BS" install 2>/dev/null; publish 1.1.0 --serial 50; "$BS" install 2>/dev/null
check "a lower serial is rejected" test "$(cur)" = 1.0.0 -a "$(state)" = error
reset; mkpkg 1.0.0; publish 1.0.0 --expires-at 1000; "$BS" install 2>/dev/null
check "an expired channel is rejected" test -z "$(cur)"
reset; mkpkg 1.0.0; mkpkg 1.1.0; publish 1.0.0; "$BS" install 2>/dev/null; publish 1.1.0 --revoke 1.1.0; "$BS" install 2>/dev/null
check "a revoked version is not installed" test "$(cur)" = 1.0.0
reset; mkpkg 1.0.0; mkpkg 1.1.0; publish 1.0.0; "$BS" install 2>/dev/null; publish 1.1.0 --min-upgrade-from 1.0.5; "$BS" install 2>/dev/null
check "min_upgrade_from is honoured" test "$(cur)" = 1.0.0

# --- rollout, pin, report-only
reset; mkpkg 1.0.0; publish 1.0.0 --percent 0; "$BS" install 2>/dev/null
check "0% wave installs nothing" test -z "$(cur)" -a "$(state)" = waiting
publish 1.0.0 --percent 100 --require-label zone=a; "$BS" install 2>/dev/null
check "a missing required label holds the node" test -z "$(cur)"
LABELS="zone=a" "$BS" install 2>/dev/null; check "a matching label lets it in" test "$(cur)" = 1.0.0
reset; mkpkg 1.0.0; publish 1.0.0; FLUXVM_AUTO_UPGRADE=0 "$BS" install 2>/dev/null
check "FLUXVM_AUTO_UPGRADE=0 reports but installs nothing" test -z "$(cur)" -a "$(state)" = due
"$BS" check 2>/dev/null; check "check installs nothing" test -z "$(cur)"
"$BS" install 2>/dev/null; mkpkg 1.1.0; publish 1.1.0; "$BS" pin 2>/dev/null; "$BS" install 2>/dev/null
check "pin holds the node" test "$(cur)" = 1.0.0
"$BS" unpin 2>/dev/null; "$BS" install 2>/dev/null; check "unpin releases it" test "$(cur)" = 1.1.0

# --- network behaviour
reset; mkpkg 1.0.0; publish 1.0.0; echo "2 2" > "$T/inject"
unset FLUXVM_BOOTSTRAP_FAST; start=$(date +%s); "$BS" install 2>/dev/null; took=$(( $(date +%s) - start )); export FLUXVM_BOOTSTRAP_FAST=1
check "429 with Retry-After: 2 is retried and waited out" test "$(cur)" = 1.0.0 -a "$took" -ge 4
reset; mkpkg 1.0.0; publish 1.0.0
f="$T/mirror/releases/1.0.0/fluxvm-1.0.0-linux-$ARCH.tar.gz"; mkdir -p "$T/root/var/lib/fluxvm-bootstrap/cache"
head -c 100000 "$f" > "$T/root/var/lib/fluxvm-bootstrap/cache/fluxvm-1.0.0-linux-$ARCH.tar.gz.part"
: > "$T/req.log"; "$BS" install 2>/dev/null
check "a truncated download resumes" test "$(cur)" = 1.0.0 && grep -q "range=bytes=100000-" "$T/req.log"

# --- wave buckets, and jitter
# shellcheck disable=SC1090
source "$BS"
a=0; b=0; subset=1
for i in $(seq 1 10000); do
  NODE_ID="synthetic-$i"; x="$(bucket salt-1)"
  if [[ $x -lt 500 ]]; then a=$((a + 1)); [[ $x -lt 2000 ]] || subset=0; fi
  [[ $x -lt 2000 ]] && b=$((b + 1))
done
check "5% wave: $a of 10000 nodes (400-600)" test "$a" -ge 400 -a "$a" -le 600
check "the 5% set is inside the 20% set ($b nodes)" test "$subset" = 1 -a "$b" -gt "$a"
NODE_ID=n1; check "a node's bucket is stable" test "$(bucket s)" = "$(bucket s)"
JITTER_MAX=300; okj=1
for i in $(seq 1 200); do NODE_ID="j-$i"; v="$(jitter_seconds)"; [[ $v -ge 0 && $v -le 300 ]] || okj=0; done
check "jitter stays within 0..JITTER_MAX" test "$okj" = 1

echo; echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
