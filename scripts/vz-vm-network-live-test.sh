#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the per-guest eBPF network routes (`GET /v1/vms/{id}/network/{stats,flows,drops,status}`, `POST .../network/edge`) and backups
# (`POST /v1/vms/{id}/backup`, `GET /v1/backups`, `DELETE /v1/backups/{name}`, `GET /v1/backups/{name}/root`) on a vz guest. The eBPF datapath is Linux-only,
# so the stats/flows maps are reported as not pinned (400) while drops and status answer, with `attached:false`; a backup needs QEMU's qcow2 tooling,
# so it is refused for a running guest and for a stopped one (no `qemu-img` here) and creates nothing; unknown backups are 404.
#   scripts/vz-vm-network-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-net.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7870}"
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
run_dir = "/tmp/fluxvm-run-net"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"; API="http://127.0.0.1:$PORT/v1"; NIL=00000000-0000-0000-0000-000000000000
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
V="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"net","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}' | jget 'd["id"]')" || bad "create"
for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "vz guest $V is running" || bad "guest not running"
call() { CODE="$(curl -sS -o "$T/r.out" -w '%{http_code}' -X "$1" "$API/$2" -H 'content-type: application/json' ${3:+-d "$3"})"; BODY="$(cat "$T/r.out")"; }
expect() { # <desc> <code> <substring>   (uses CODE/BODY from the last call)
  [[ "$CODE" == "$2" && "$BODY" == *"$3"* ]] && ok "$1 -> $2 \"$3\"" || bad "$1: $CODE $BODY"
}

call GET vms/$V/network/stats;  expect "network/stats"  400 "eBPF map is not pinned"
call GET vms/$V/network/flows;  expect "network/flows"  400 "eBPF map is not pinned"
call GET vms/$V/network/drops;  [[ "$CODE" == 200 && "$(echo "$BODY" | jget 'd["items"]')" == "[]" ]] && ok "network/drops -> 200 with no drops" || bad "drops: $CODE $BODY"
call GET vms/$V/network/status
[[ "$CODE" == 200 && "$(echo "$BODY" | jget '[d["attached"], d["interface"], d["policy_synced"], d["schema_compatible"]]')" == "[false, null, false, false]" ]] && ok "network/status -> 200: not attached, no interface, policy not synced" || bad "status: $CODE $BODY"
echo "     mode=$(echo "$BODY" | jget 'd["mode"]') required=$(echo "$BODY" | jget 'd["required"]') default_allow=$(echo "$BODY" | jget 'd["policy"]["default_allow"]')"
call POST vms/$V/network/edge '{}'; expect "network/edge with no body fields" 422 "namespace"
call GET vms/$NIL/network/stats; expect "network/stats on an unknown VM" 400 "VM not found"
call GET vms/$NIL/network/status; echo "     network/status on an unknown VM: $CODE $(echo "$BODY" | cut -c1-80)"

call GET backups; [[ "$CODE" == 200 && "$(echo "$BODY" | jget 'd["items"]')" == "[]" ]] && ok "backups: none yet" || bad "backups: $CODE $BODY"
call POST vms/$V/backup '{"name":"b1"}'; expect "backup of a running guest" 400 "needs a QEMU VM"
call DELETE backups/nope; expect "delete an unknown backup" 404 "not found"
call GET backups/nope/root; expect "download root of an unknown backup" 404 "not found"

curl -fs -X POST "$B/$V/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == stopped ]] && break; sleep 1; done
call POST vms/$V/backup '{"name":"b1"}'
if command -v qemu-img >/dev/null; then echo "     (qemu-img present: backup of the stopped guest -> $CODE $(echo "$BODY" | cut -c1-120))"; [[ "$CODE" != 5* ]] && ok "backup of a stopped guest is not a server error" || bad "backup 5xx"
else expect "backup of a stopped guest without qemu-img" 400 "qemu-img"; fi
call GET backups; [[ "$(echo "$BODY" | jget 'len(d["items"])')" -le 1 ]] && ok "a refused backup left no half-made entry (backups: $(echo "$BODY" | jget 'len(d["items"])'))" || bad "backups after: $BODY"
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == stopped ]] && ok "the guest is still just stopped" || bad "guest state"
echo "vz network/backup: PASS"
