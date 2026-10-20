#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: VM snapshots on a vz guest (`POST /v1/vms/{id}/snapshot`, `start-from-snapshot`, `GET .../snapshots`,
# `DELETE .../snapshots/{tag}`). A memory snapshot brings back disk and memory (the guest does not reboot); a disk-only snapshot
# (`memory: false`) brings back the disk with a cold boot; an unknown or deleted tag is refused (400) and a plain start recovers the VM.
#   scripts/vz-snapshot-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-snapshot.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7813}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -10 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-snapshot"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
status() { curl -fs "$B/$1" | jget 'd.get("status","")'; }
gexec() { # <vm> <command>
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 30}))' "$2")" | jget 'd.get("stdout","").strip()'
}
wait_agent() {
  for _ in $(seq 1 60); do [[ "$(gexec "$1" 'echo up' 2>/dev/null)" == up ]] && return 0; sleep 2; done
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' -d '{"command":"echo up","timeout_seconds":10}' | cut -c1-400 >&2; return 1
}

RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"parent","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "parent $P is up with the agent" || bad "agent never answered"

setmark() { gexec "$P" "echo $1 > /var/tmp/disk-mark; echo $1 > /dev/shm/mem-mark; sync" >/dev/null; }
marks() { gexec "$P" 'echo "$(cat /var/tmp/disk-mark 2>/dev/null || echo none) $(cat /dev/shm/mem-mark 2>/dev/null || echo none)"'; }
uptime_s() { gexec "$P" 'cut -d. -f1 /proc/uptime'; }
stop_vm() { curl -fs -X POST "$B/$P/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(status "$P")" == stopped ]] && return 0; sleep 2; done; return 1; }
tags() { curl -fs "$B/$P/snapshots" | jget 'sorted(s["tag"] if isinstance(s, dict) else s for s in d["items"])'; }
from_snap() { curl -s -o "$T/snap.out" -w '%{http_code}' -X POST "$B/$P/start-from-snapshot" -H 'content-type: application/json' -d "{\"tag\":\"$1\"}"; }

setmark v1; sleep 20; U0="$(uptime_s)"
[[ "$(marks)" == "v1 v1" ]] && ok "markers v1 written (guest uptime ${U0}s)" || bad "markers: $(marks)"
R="$(curl -sS -X POST "$B/$P/snapshot" -H 'content-type: application/json' -d '{"tag":"s1"}')"; echo "     $R"
[[ "$(echo "$R" | jget 'd["ok"] and d["memory"]')" == True ]] && ok "memory snapshot s1 taken" || bad "snapshot: $R"
[[ "$(status "$P")" == running ]] && ok "the VM keeps running after the snapshot" || bad "status after snapshot: $(status "$P")"
[[ "$(tags)" == '["s1"]' ]] && ok "snapshot s1 is listed" || bad "tags: $(tags)"

setmark v2; [[ "$(marks)" == "v2 v2" ]] && ok "markers changed to v2" || bad "markers: $(marks)"
stop_vm && ok "VM stopped" || bad "stop"
CODE="$(from_snap s1)"; echo "     start-from-snapshot: $CODE $(cut -c1-200 "$T/snap.out")"
[[ "$CODE" == 200 ]] && ok "start-from-snapshot s1 accepted" || bad "start-from-snapshot: $CODE $(cat "$T/snap.out")"
wait_agent "$P" && ok "the agent answers after the restore" || bad "agent never answered after restore"
[[ "$(marks)" == "v1 v1" ]] && ok "disk AND memory are back at v1" || bad "after restore marks are: $(marks)"
U1="$(uptime_s)"; [[ "$U1" -ge "$U0" ]] && ok "the guest did not reboot (uptime ${U1}s, was ${U0}s at the snapshot)" || bad "uptime ${U1}s < ${U0}s: the guest rebooted"

R="$(curl -sS -X POST "$B/$P/snapshot" -H 'content-type: application/json' -d '{"tag":"s2","memory":false}')"; echo "     $R"
[[ "$(echo "$R" | jget 'd["ok"] and d["memory"] == False')" == True ]] && ok "disk-only snapshot s2 taken" || bad "s2: $R"
[[ "$(tags)" == '["s1", "s2"]' ]] && ok "both snapshots are listed" || bad "tags: $(tags)"
setmark v3; stop_vm || bad "stop"
CODE="$(from_snap s2)"; [[ "$CODE" == 200 ]] && wait_agent "$P" && ok "start-from-snapshot s2 answers" || bad "s2 restore: $CODE $(cat "$T/snap.out")"
echo "     after the disk-only restore: $(marks), uptime $(uptime_s)s"
[[ "$(marks)" == "v1 none" ]] && ok "disk-only snapshot: disk is back at v1, memory is gone (cold boot)" || bad "disk-only marks: $(marks)"

stop_vm || bad "stop"
CODE="$(from_snap nope)"; echo "     unknown tag: $CODE $(cut -c1-200 "$T/snap.out")"
[[ "$CODE" == 400 ]] && ok "an unknown tag is refused (400)" || bad "unknown tag: $CODE"
echo "     status after the refused restore: $(status "$P")"
# A refused restore leaves the VM `failed`, not `stopped`; a plain start must recover it.
curl -fs -X POST "$B/$P/start" >/dev/null && wait_agent "$P" && ok "a plain start recovers the VM after a refused restore" || bad "start after refusal: $(status "$P")"
[[ "$(marks)" == "v1 none" ]] && ok "the disk is still at v1" || bad "marks after recovery: $(marks)"
stop_vm || bad "stop"
curl -fs -X DELETE "$B/$P/snapshots/s1" >/dev/null && [[ "$(tags)" == '["s2"]' ]] && ok "snapshot s1 deleted" || bad "delete s1: $(tags)"
CODE="$(from_snap s1)"; [[ "$CODE" == 400 ]] && ok "a deleted snapshot cannot be started from (400)" || bad "deleted tag: $CODE"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz snapshot: PASS"
