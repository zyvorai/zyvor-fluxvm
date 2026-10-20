#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: clone a vz guest (`POST /v1/vms/{id}/clone`). A running guest is refused (a clone needs a stopped VM for a consistent
# disk); a stopped guest clones, the clone cold-boots with the parent's disk contents but not its memory, and writes in the clone
# do not reach the parent.
#   scripts/vz-clone-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-clone.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7811}"
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
run_dir = "/tmp/fluxvm-run-clone"
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

MARK="CLONE-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $MARK > /var/tmp/disk-marker; echo $MARK > /dev/shm/mem-marker; sync" >/dev/null
[[ "$(gexec "$P" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "disk and memory markers written in the parent" || bad "markers not written"

CODE="$(curl -s -o "$T/run-clone.json" -w '%{http_code}' -X POST "$B/$P/clone" -H 'content-type: application/json' -d '{"name":"clone-running"}')"
[[ "$CODE" == 400 && "$(cat "$T/run-clone.json")" == *"stopped VM"* ]] && ok "cloning a running guest is refused (400)" || bad "running clone: $CODE $(cat "$T/run-clone.json")"

curl -fs -X POST "$B/$P/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(status "$P")" == stopped ]] && break; sleep 2; done
[[ "$(status "$P")" == stopped ]] && ok "parent stopped" || bad "stop: $(status "$P")"

CR="$(curl -sS -w '\n%{http_code}' -X POST "$B/$P/clone" -H 'content-type: application/json' -d '{"name":"clone-of-parent"}')"
CODE="${CR##*$'\n'}"; BODY="${CR%$'\n'*}"; echo "     $(echo "$BODY" | cut -c1-300)"
C="$(jget 'd["id"]' <<<"$BODY" 2>/dev/null || true)"
[[ "$CODE" == 201 && -n "$C" && "$C" != "$P" ]] && ok "clone created as $C (201)" || bad "clone: $CODE $BODY"
[[ "$(jget 'd["name"]' <<<"$BODY")" == clone-of-parent ]] && ok "clone has the requested name" || bad "clone name"

curl -fs -X POST "$B/$C/start" >/dev/null || bad "start clone"
wait_agent "$C" && ok "clone cold-boots and answers through the agent" || bad "clone agent never answered"
[[ "$(gexec "$C" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "clone has the parent's disk contents" || bad "clone lost the disk marker"
[[ "$(gexec "$C" 'cat /dev/shm/mem-marker 2>/dev/null || echo none')" == none ]] && ok "clone does not have the parent's memory (cold boot)" || bad "memory marker survived"

gexec "$C" 'echo only-clone > /var/tmp/clone-only; sync' >/dev/null
curl -fs -X POST "$B/$P/start" >/dev/null || bad "start parent"
wait_agent "$P" && ok "parent restarts" || bad "parent agent never answered"
[[ "$(gexec "$P" 'cat /var/tmp/clone-only 2>/dev/null || echo none')" == none ]] && ok "a write in the clone does not reach the parent" || bad "write leaked to the parent"
[[ "$(gexec "$P" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "parent still has its own disk marker" || bad "parent lost its marker"
echo "     machine-id parent: $(gexec "$P" 'cat /etc/machine-id')  clone: $(gexec "$C" 'cat /etc/machine-id')"
for V in "$C" "$P"; do curl -fs -X DELETE "$B/$V" >/dev/null || true; done
echo "vz clone: PASS"
