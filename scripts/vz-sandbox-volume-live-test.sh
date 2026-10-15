#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of named volumes on a `vz` VM sandbox: the guest writes into a volume, the file lands in the host's volume
# directory, survives deleting the sandbox, and a second sandbox with the same volume reads it.
#   scripts/vz-sandbox-volume-live-test.sh
# Needs Apple silicon macOS 14+, the Xcode command line tools, Rust and python3. Uses the built-in `debian-13` image.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-sbvol.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7798}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-sbvol"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

S="http://127.0.0.1:$PORT/v1/sandboxes"
jid() { python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'; }
BODY='{"ttl_seconds":600,"backend":"vz","volumes":[{"name":"data","guest_path":"/mnt/data"}]}'
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODY")"
ID="$(jid <<<"$R" 2>/dev/null)" || bad "create: $R"
ok "sandbox $ID created with a volume"
run() { curl -sS -X POST "$S/$1/process" -H 'Content-Type: application/json' -d "{\"command\": $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$2")}"; }
OUT="$(run "$ID" 'mountpoint -q /mnt/data && echo mounted; echo from-guest > /mnt/data/hello.txt; sync')"
grep -q mounted <<<"$OUT" && ok "the volume is mounted in the guest" || bad "not mounted: $OUT"
F="$(find "$T/state/volumes" -name hello.txt 2>/dev/null | head -1)"
[[ -n "$F" && "$(cat "$F")" == from-guest ]] && ok "the guest's write is in the host volume directory ($F)" || bad "no file on the host"
curl -fsS -X DELETE "http://127.0.0.1:$PORT/v1/vms/$ID" >/dev/null || bad "delete failed"
sleep 3
[[ "$(cat "$F")" == from-guest ]] && ok "the file survives deleting the sandbox" || bad "file lost"
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODY")"
ID2="$(jid <<<"$R" 2>/dev/null)" || bad "second create: $R"
OUT="$(run "$ID2" 'cat /mnt/data/hello.txt')"
grep -q from-guest <<<"$OUT" && ok "a new sandbox with the same volume reads the file" || bad "second sandbox read: $OUT"
curl -fsS -X DELETE "http://127.0.0.1:$PORT/v1/vms/$ID2" >/dev/null || true
echo "vz sandbox volume checks passed"
