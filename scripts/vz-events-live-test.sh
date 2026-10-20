#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the lifecycle event log of a vz guest (`GET /v1/events`, `GET /v1/events/stream`). A VM's create, pause, resume, restart
# (which is a stop, a start and then the restart event) and stop are recorded in order; the stream carries only events that happen
# after it connects; the vm, event-prefix, since and limit filters narrow the list.
#   scripts/vz-events-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-events.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7819}"
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
run_dir = "/tmp/fluxvm-run-events"
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
wait_agent "$P" && ok "guest $P is up" || bad "agent never answered"
E="http://127.0.0.1:$PORT/v1/events"
names() { jget '[e["event"] for e in d["items"]]'; }

[[ "$(curl -fs "$E?vm=$P" | names)" == '["vm.create"]' ]] && ok "a new VM has exactly one event: vm.create" || bad "after boot: $(curl -fs "$E?vm=$P" | names)"
[[ "$(curl -fs "$E?vm=$P" | jget 'd["items"][0]["fields"]["name"]')" == parent && "$(curl -fs "$E?vm=$P" | jget 'len(d["items"][0]["fields"]["image_sha256"])')" == 64 ]] \
  && ok "vm.create records the name and the image sha256" || bad "create fields: $(curl -fs "$E?vm=$P" | cut -c1-300)"

( curl -sN --max-time 60 "$E/stream?vm=$P" > "$T/stream.out" 2>/dev/null ) & SPID=$!
sleep 2
curl -fs -X POST "$B/$P/pause" >/dev/null; sleep 1
curl -fs -X POST "$B/$P/resume" >/dev/null; sleep 1
curl -fs -X POST "$B/$P/restart" >/dev/null; wait_agent "$P" || true
curl -fs -X POST "$B/$P/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(status "$P")" == stopped ]] && break; sleep 2; done
sleep 3

WANT='["vm.create", "vm.pause", "vm.resume", "vm.stop", "vm.start", "vm.restart", "vm.stop"]'
[[ "$(curl -fs "$E?vm=$P" | names)" == "$WANT" ]] && ok "the lifecycle is recorded in order (a restart is stop, start, then vm.restart)" || bad "events: $(curl -fs "$E?vm=$P" | names)"
[[ "$(curl -fs "$E?vm=$P" | jget 'd["items"] == sorted(d["items"], key=lambda e: e["ts"])')" == True ]] && ok "timestamps never go backwards" || bad "events out of order"
[[ "$(curl -fs "$E?vm=$P" | jget 'all(e["vm_id"] == "'"$P"'" for e in d["items"])')" == True ]] && ok "every event carries the VM id" || bad "vm_id missing"

kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true
GOT="$(grep -E '^event:' "$T/stream.out" | sed 's/event: //' | tr -d '\r' | tr '\n' ' ')"
[[ "$GOT" == "vm.pause vm.resume vm.stop vm.start vm.restart vm.stop " ]] && ok "the stream carried each later event, and not the earlier vm.create" || bad "stream: $GOT"
grep -q '^data: {' "$T/stream.out" && ok "stream events carry a JSON data line" || bad "no data lines in the stream"

[[ "$(curl -fs "$E?vm=$P&event=vm." | names)" == "$WANT" ]] && ok "event=vm. matches by prefix" || bad "prefix vm."
[[ "$(curl -fs "$E?vm=$P&event=vm.st" | names)" == '["vm.stop", "vm.start", "vm.stop"]' ]] && ok "event=vm.st narrows to stop and start" || bad "prefix vm.st: $(curl -fs "$E?vm=$P&event=vm.st" | names)"
[[ "$(curl -fs "$E?vm=$P&limit=2" | names)" == '["vm.restart", "vm.stop"]' ]] && ok "limit=2 keeps the two most recent events" || bad "limit: $(curl -fs "$E?vm=$P&limit=2" | names)"
[[ "$(curl -fs "$E?vm=00000000-0000-0000-0000-000000000000" | names)" == '[]' ]] && ok "another VM id has no events" || bad "other vm id"
[[ "$(curl -fs "$E?vm=$P&since=2999-01-01T00:00:00Z" | names)" == '[]' ]] && ok "since in the future returns nothing" || bad "since future"
[[ "$(curl -fs "$E?vm=$P&since=2000-01-01T00:00:00Z" | names)" == "$WANT" ]] && ok "since in the past returns everything" || bad "since past"

curl -fs -X DELETE "$B/$P" >/dev/null || true; sleep 2
echo "     events after the delete: $(curl -fs "$E?vm=$P" | names)"
echo "vz events: PASS"
