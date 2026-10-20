#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: fork a PAUSED vz guest (`POST /v1/vms/{id}/fork`). A memory marker is written in the running guest, the
# guest is paused, forked, and: the parent must still be paused afterwards, the child must run and hold the marker, and the
# parent must resume and still hold it.
#   scripts/vz-fork-paused-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-forkpaused.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7807}"
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
run_dir = "/tmp/fluxvm-run-forkpaused"
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

MARK="PAUSED-FORK-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $MARK > /dev/shm/marker" >/dev/null
[[ "$(gexec "$P" 'cat /dev/shm/marker')" == "$MARK" ]] && ok "memory marker written in the parent" || bad "marker not written"

curl -fs -X POST "$B/$P/pause" >/dev/null && [[ "$(status "$P")" == paused ]] && ok "parent paused" || bad "pause: $(status "$P")"

FORK="$(curl -sS -X POST "$B/$P/fork" -H 'content-type: application/json' -d '{"count":1}')"
echo "$FORK" | cut -c1-400
C="$(python3 -c '
import sys,json
d=json.load(sys.stdin)
def ids(x):
    if isinstance(x,dict):
        if "id" in x and x["id"] != sys.argv[1]: yield x["id"]
        for v in x.values(): yield from ids(v)
    elif isinstance(x,list):
        for v in x: yield from ids(v)
print(next(ids(d), ""))' "$P" <<<"$FORK")"
[[ -n "$C" ]] && ok "fork of the paused parent returned child $C" || bad "fork failed: $FORK"
[[ "$(status "$P")" == paused ]] && ok "parent is still paused after the fork" || bad "parent status after fork: $(status "$P")"
[[ "$(status "$C")" == running ]] && ok "child is running" || bad "child status: $(status "$C")"
wait_agent "$C" && ok "child answers through the agent" || bad "child agent never answered"
[[ "$(gexec "$C" 'cat /dev/shm/marker')" == "$MARK" ]] && ok "child holds the parent's memory marker" || bad "child lost the marker"

curl -fs -X POST "$B/$P/resume" >/dev/null && [[ "$(status "$P")" == running ]] && ok "parent resumed" || bad "resume: $(status "$P")"
wait_agent "$P" && [[ "$(gexec "$P" 'cat /dev/shm/marker')" == "$MARK" ]] && ok "parent still holds the marker after resume" || bad "parent lost the marker"
curl -fs -X DELETE "$B/$C" >/dev/null || true; curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz fork of a paused guest: PASS"
