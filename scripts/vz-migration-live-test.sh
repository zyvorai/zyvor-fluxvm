#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: live migration (`/v1/vms/{id}/migration/{start,status,cancel,tune,postcopy,finish}`, `POST /v1/migration/receivers`) is QEMU-only, so a
# vz guest must be refused cleanly on every route: 400 naming the backend, a malformed body told apart from an unsupported backend, an unknown or
# stopped VM refused for its own reason, no migration receiver process left behind, and the guest keeps running with its agent answering.
#   scripts/vz-migration-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-mig.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7860}"
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
run_dir = "/tmp/fluxvm-run-mig"
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
V="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"mig","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}' | jget 'd["id"]')" || bad "create"
for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "vz guest $V is running" || bad "guest not running"
gexec() { curl -sS -X POST "$B/$V/agent" -H 'content-type: application/json' -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 30}))' "$1")" | jget 'd.get("stdout","").strip()'; }
for _ in $(seq 1 60); do [[ "$(gexec 'echo up' 2>/dev/null)" == up ]] && break; sleep 2; done
gexec 'echo marker > /tmp/before-migration'

call() { # <method> <path under /v1> [body] -> CODE, BODY
  CODE="$(curl -sS -o "$T/r.out" -w '%{http_code}' -X "$1" "$API/$2" -H 'content-type: application/json' ${3:+-d "$3"})"; BODY="$(cat "$T/r.out")"
}
refused() { # <desc> <method> <path> <body> <substring>
  call "$2" "$3" "${4:-}"; [[ "$CODE" == 400 && "$BODY" == *"$5"* ]] && ok "$1 -> 400 \"$5\"" || bad "$1: $CODE $BODY"
}
refused "migration/start (tcp:)"   POST vms/$V/migration/start    '{"destination":"tcp:127.0.0.1:9999"}' "supports qemu only (backend=Vz)"
refused "migration/start (exec:)"  POST vms/$V/migration/start    '{"destination":"exec:cat"}'           "supports qemu only (backend=Vz)"
refused "migration/status"         GET  vms/$V/migration/status   ''                                      "supports qemu only (backend=Vz)"
refused "migration/cancel"         POST vms/$V/migration/cancel   ''                                      "supports qemu only (backend=Vz)"
refused "migration/tune"           POST vms/$V/migration/tune     '{"bandwidth_mbps":100}'                "supports qemu only (backend=Vz)"
refused "migration/postcopy"       POST vms/$V/migration/postcopy ''                                      "supports qemu only (backend=Vz)"
refused "migration/finish"         POST vms/$V/migration/finish   ''                                      "supports qemu only (backend=Vz)"

call POST vms/$V/migration/start '{}'; [[ "$CODE" == 422 && "$BODY" == *destination* ]] && ok "start without 'destination' -> 422 naming the field (checked before the backend)" || bad "no destination: $CODE $BODY"
call POST migration/receivers '{"vcpus":2,"memory_mib":1024,"disk":"/nonexistent/disk.raw"}'
[[ "$CODE" == 400 && "$BODY" == *"must already exist on shared storage"* ]] && ok "migration receiver with a missing disk -> 400 'must already exist on shared storage'" || bad "receiver: $CODE $BODY"
call POST migration/receivers '{}'; [[ "$CODE" == 400 ]] && ok "migration receiver with no sizes or disk -> 400 ($(echo "$BODY" | cut -c1-110))" || bad "receiver {}: $CODE $BODY"
! pgrep -f 'qemu-system.*incoming' >/dev/null && ok "no migration receiver (qemu -incoming) process was left behind" || bad "receiver process left"

for r in "POST migration/start" "GET migration/status"; do
  set -- $r; call "$1" "vms/$NIL/$2" '{"destination":"tcp:127.0.0.1:9999"}'
  [[ "$CODE" == 400 && "$BODY" == *"not found"* ]] || bad "$r on unknown VM: $CODE $BODY"
done
ok "migration start / status on an unknown VM -> 400 'not found'"

[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running && "$(gexec 'cat /tmp/before-migration')" == marker ]] && ok "through all of it the guest is still running and its agent answers (state intact)" || bad "guest disturbed"

curl -fs -X POST "$B/$V/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == stopped ]] && break; sleep 1; done
call POST vms/$V/migration/start '{"destination":"tcp:127.0.0.1:9999"}'
[[ "$CODE" == 400 && "$BODY" == *"requires a running VM (status=Stopped)"* ]] && ok "a stopped guest is refused for not running ('requires a running VM (status=Stopped)')" || bad "stopped: $CODE $BODY"
echo "vz migration: PASS"
