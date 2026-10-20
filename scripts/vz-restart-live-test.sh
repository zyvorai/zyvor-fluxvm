#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: restart a vz guest (`POST /v1/vms/{id}/restart`). The id stays, the runner process is replaced, the disk keeps what
# was written and memory does not; a paused guest restarts too, and so does a stopped one (a restart then just starts it).
#   scripts/vz-restart-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-restart.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7812}"
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
run_dir = "/tmp/fluxvm-run-restart"
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

field() { curl -fs "$B/$1" | jget "d.get(\"$2\")"; }
MARK="RESTART-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $MARK > /var/tmp/disk-marker; echo $MARK > /dev/shm/mem-marker; sync" >/dev/null
[[ "$(gexec "$P" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "disk and memory markers written" || bad "markers not written"
UP0="$(gexec "$P" 'cut -d. -f1 /proc/uptime')"; PID0="$(field "$P" pid)"

R="$(curl -sS -X POST "$B/$P/restart")"; echo "     $(echo "$R" | cut -c1-200)"
[[ "$(echo "$R" | jget 'd["id"]')" == "$P" && "$(echo "$R" | jget 'd["status"]')" == running ]] && ok "restart returns the same VM, running" || bad "restart: $R"
PID1="$(field "$P" pid)"
[[ -n "$PID1" && "$PID1" != None && "$PID1" != "$PID0" ]] && ok "the runner process was replaced (pid $PID0 -> $PID1)" || bad "pid $PID0 -> $PID1"
wait_agent "$P" && ok "the agent answers after the restart" || bad "agent never answered after restart"
UP1="$(gexec "$P" 'cut -d. -f1 /proc/uptime')"
[[ "$UP1" -lt 120 ]] && ok "the guest rebooted (uptime ${UP1}s)" || bad "uptime ${UP1}s (was ${UP0}s)"
[[ "$(gexec "$P" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "the disk marker survived" || bad "disk marker lost"
[[ "$(gexec "$P" 'cat /dev/shm/mem-marker 2>/dev/null || echo none')" == none ]] && ok "the memory marker is gone" || bad "memory marker survived a restart"

curl -fs -X POST "$B/$P/pause" >/dev/null && [[ "$(status "$P")" == paused ]] && ok "guest paused" || bad "pause"
R="$(curl -sS -X POST "$B/$P/restart")"
[[ "$(echo "$R" | jget 'd.get("status")')" == running ]] && wait_agent "$P" && ok "a paused guest restarts and answers" || bad "paused restart: $R"

curl -fs -X POST "$B/$P/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(status "$P")" == stopped ]] && break; sleep 2; done
[[ "$(status "$P")" == stopped ]] && ok "guest stopped" || bad "stop: $(status "$P")"
R="$(curl -sS -X POST "$B/$P/restart")"
[[ "$(echo "$R" | jget 'd.get("status")')" == running ]] && wait_agent "$P" && ok "a stopped guest restarts (just starts) and answers" || bad "stopped restart: $R"
[[ "$(gexec "$P" 'cat /var/tmp/disk-marker')" == "$MARK" ]] && ok "the disk marker survived three restarts" || bad "disk marker lost"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz restart: PASS"
