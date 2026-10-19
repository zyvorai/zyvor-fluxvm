#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of named volumes on a `vz` VM sandbox: RW persistence across delete, two volumes at once, and a read_only
# volume that the guest can read but not write.
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
run() { curl -sS -X POST "$S/$1/process" -H 'Content-Type: application/json' -d "{\"command\": $(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$2")}"; }
del() { curl -fsS -X DELETE "http://127.0.0.1:$PORT/v1/vms/$1" >/dev/null || true; }

# --- single RW volume: write, survive delete, reattach ---
BODY='{"ttl_seconds":600,"backend":"vz","volumes":[{"name":"data","guest_path":"/mnt/data"}]}'
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODY")"
ID="$(jid <<<"$R" 2>/dev/null)" || bad "create: $R"
ok "sandbox $ID created with a volume"
OUT="$(run "$ID" 'mountpoint -q /mnt/data && echo mounted; echo from-guest > /mnt/data/hello.txt; sync')"
grep -q mounted <<<"$OUT" && ok "the volume is mounted in the guest" || bad "not mounted: $OUT"
F="$(find "$T/state/volumes" -name hello.txt 2>/dev/null | head -1)"
[[ -n "$F" && "$(cat "$F")" == from-guest ]] && ok "the guest's write is in the host volume directory ($F)" || bad "no file on the host"
del "$ID"
sleep 3
[[ "$(cat "$F")" == from-guest ]] && ok "the file survives deleting the sandbox" || bad "file lost"
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODY")"
ID2="$(jid <<<"$R" 2>/dev/null)" || bad "second create: $R"
OUT="$(run "$ID2" 'cat /mnt/data/hello.txt')"
grep -q from-guest <<<"$OUT" && ok "a new sandbox with the same volume reads the file" || bad "second sandbox read: $OUT"
del "$ID2"

# --- two volumes at once ---
# Release `data` first so a multi-volume create that also names it is not refused as "one owner".
BODY2='{"ttl_seconds":600,"backend":"vz","volumes":[{"name":"data","guest_path":"/mnt/data"},{"name":"cache","guest_path":"/mnt/cache"}]}'
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODY2")"
ID3="$(jid <<<"$R" 2>/dev/null)" || bad "two-volume create: $R"
ok "sandbox $ID3 created with two volumes"
OUT="$(run "$ID3" 'mountpoint -q /mnt/data && mountpoint -q /mnt/cache && echo both-mounted; echo a > /mnt/data/a.txt; echo b > /mnt/cache/b.txt; sync')"
grep -q both-mounted <<<"$OUT" && ok "both volumes are mounted" || bad "two-volume mount: $OUT"
FA="$(find "$T/state/volumes" -path '*/data/a.txt' 2>/dev/null | head -1)"
FB="$(find "$T/state/volumes" -path '*/cache/b.txt' 2>/dev/null | head -1)"
[[ -n "$FA" && "$(cat "$FA")" == a && -n "$FB" && "$(cat "$FB")" == b ]] \
  && ok "writes landed in both host volume directories" || bad "two-volume host files: data='$FA' cache='$FB'"
del "$ID3"

# --- read_only: seed on host, guest reads, write fails, host file unchanged ---
mkdir -p "$T/state/volumes/_shared/ro"
echo seed > "$T/state/volumes/_shared/ro/seed.txt"
BODYRO='{"ttl_seconds":600,"backend":"vz","volumes":[{"name":"ro","guest_path":"/mnt/ro","read_only":true}]}'
R="$(curl -sS -X POST "$S" -H 'Content-Type: application/json' -d "$BODYRO")"
ID4="$(jid <<<"$R" 2>/dev/null)" || bad "read_only create: $R"
ok "sandbox $ID4 created with a read_only volume"
OUT="$(run "$ID4" 'mountpoint -q /mnt/ro && echo mounted; cat /mnt/ro/seed.txt; if echo no > /mnt/ro/seed.txt 2>/tmp/ro-err; then echo wrote; else echo refused; fi')"
grep -q mounted <<<"$OUT" || bad "read_only not mounted: $OUT"
grep -q seed <<<"$OUT" && ok "guest reads the seeded file on a read_only volume" || bad "read_only read: $OUT"
grep -q refused <<<"$OUT" && ok "guest write on a read_only volume is refused" || bad "read_only write not refused: $OUT"
[[ "$(cat "$T/state/volumes/_shared/ro/seed.txt")" == seed ]] \
  && ok "host file unchanged after refused write" || bad "host seed.txt was modified"
del "$ID4"

echo "vz sandbox volume checks passed"
