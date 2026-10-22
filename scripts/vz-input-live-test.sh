#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: keyboard and mouse input (`POST /v1/vms/{id}/input`), CD-ROM eject (`POST /v1/vms/{id}/cdroms/{name}/eject`), the display screenshot
# (`GET /v1/vms/{id}/screenshot`), the stored sign-in (`GET|DELETE /v1/vms/{id}/signin`) and the vmnet list (`GET /v1/vznets`) on a vz guest. A click and a
# batched key press are accepted, malformed or empty input is refused (422 / 400), an unknown VM is refused, a guest with no CD-ROM has nothing to eject.
#   scripts/vz-input-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-in.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7880}"
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
run_dir = "/tmp/fluxvm-run-in"
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
B="http://127.0.0.1:$PORT/v1/vms"; API="http://127.0.0.1:$PORT/v1"; NIL=00000000-0000-0000-0000-000000000000
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
V="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"in","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}' | jget 'd["id"]')" || bad "create"
for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "vz guest $V is running" || bad "guest not running"

call() { # <method> <path under /v1> [body]  -> sets CODE and BODY
  CODE="$(curl -sS -o "$T/r.out" -w '%{http_code}' -X "$1" "$API/$2" -H 'content-type: application/json' ${3:+-d "$3"})"; BODY="$(LC_ALL=C tr -d '\000' < "$T/r.out" | head -c 400)"
}
expect() { # <desc> <code> <substring> <method> <path> [body]
  call "$4" "$5" "${6:-}"; [[ "$CODE" == "$2" && "$BODY" == *"$3"* ]] && ok "$1 -> $2 ($3)" || bad "$1: $CODE $BODY"
}
expect "a click"                      200 '"actions":1'                    POST "vms/$V/input" '{"action":"click","x":10,"y":20}'
expect "a batch of one key press"     200 '"actions":1'                    POST "vms/$V/input" '{"actions":[{"action":"key","key":"a"}]}'
expect "an empty body"                422 'InputRequest'                   POST "vms/$V/input" '{}'
expect "an unknown action"            422 'InputRequest'                   POST "vms/$V/input" '{"action":"bogus"}'
expect "an empty batch"               400 'no input actions'               POST "vms/$V/input" '{"actions":[]}'
expect "input on an unknown VM"       400 'VM not found'                   POST "vms/$NIL/input" '{"action":"click","x":1,"y":1}'
expect "eject on a guest without it"  400 'disk operations are supported'  POST "vms/$V/cdroms/x/eject"
expect "eject on an unknown VM"       400 'VM not found'                   POST "vms/$NIL/cdroms/x/eject"
expect "the vmnet list"               200 '"items"'                        GET  vznets

curl -sS -D "$T/h.out" -o "$T/shot.png" "$API/vms/$V/screenshot"; tr -d '\r' < "$T/h.out" > "$T/h.txt"
[[ "$(head -1 "$T/h.txt")" == *200* ]] && grep -qi '^content-type: image/png' "$T/h.txt" && [[ "$(head -c 8 "$T/shot.png" | xxd -p)" == 89504e470d0a1a0a ]] \
  && ok "the screenshot is a PNG" || bad "screenshot: $(cat "$T/h.txt")"
[[ "$(grep -i '^x-fluxvm-screen-width' "$T/h.txt" | cut -d' ' -f2)x$(grep -i '^x-fluxvm-screen-height' "$T/h.txt" | cut -d' ' -f2)" == 1280x800 ]] && ok "the screenshot is 1280x800 and says so in its headers" || bad "screenshot size: $(cat "$T/h.txt")"
grep -qi '^cache-control: no-store' "$T/h.txt" && ok "the screenshot is not cacheable" || bad "screenshot cache-control"
expect "sign-in status with none stored"   200 '"configured":false'  GET    "vms/$V/signin"
expect "deleting a sign-in that is not there" 200 '"deleted":false'  DELETE "vms/$V/signin"
expect "a sign-in with an empty password"  400 'password is empty'   PUT    "vms/$V/signin" '{"password":""}'
expect "a sign-in with an unknown field"   422 'unknown field'       PUT    "vms/$V/signin" '{"password":"x","bogus":1}'
expect "sign-in status after the refusals" 200 '"configured":false'  GET    "vms/$V/signin"
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "and through all of it the guest is still running" || bad "guest stopped"
echo "vz input: PASS"
