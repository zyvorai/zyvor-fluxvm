#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the guest log of a vz VM (`GET /v1/vms/{id}/logs?lines=N&follow=bool`). The log is the guest's raw serial console (hvc0):
# it holds the login prompt once the guest has booted, `lines` bounds what comes back, text written to the guest's console shows
# up in it, `follow` streams new console output from the tail, and an unknown VM is refused with 400.
#   scripts/vz-logs-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-logs.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7814}"
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
run_dir = "/tmp/fluxvm-run-logs"
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

logs() { curl -sS --max-time "${2:-10}" "$B/$P/logs?$1" || true; }
nlines() { awk 'END{print NR}'; }

for _ in $(seq 1 30); do logs "lines=100000" | grep -q "login:" && break; sleep 2; done
logs "lines=100000" | grep -q "Debian GNU/Linux 13" && ok "the log holds the guest's serial console (Debian banner)" || bad "no banner in the log: $(logs "lines=5" | cut -c1-200)"
logs "lines=100000" | grep -q "login:" && ok "the log holds the login prompt" || bad "no login prompt in the log"

[[ "$(logs "lines=1" | nlines)" -le 1 ]] && ok "lines=1 returns at most one line" || bad "lines=1 returned $(logs "lines=1" | nlines) lines"
[[ "$(logs "lines=3" | nlines)" -le 3 ]] && ok "lines=3 returns at most three lines" || bad "lines=3 returned $(logs "lines=3" | nlines) lines"
[[ "$(logs "lines=100000" | nlines)" -ge "$(logs "lines=3" | nlines)" ]] && ok "a larger lines returns at least as much" || bad "lines not monotonic"

M1="CONSOLE-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $M1 > /dev/console" >/dev/null; sleep 3
if logs "lines=100000" | grep -q "$M1"; then echo "     a write to /dev/console reaches the log"; else echo "     a write to /dev/console does NOT reach the log"; fi
M2="HVC0-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $M2 > /dev/hvc0" >/dev/null; sleep 3
logs "lines=100000" | grep -q "$M2" && ok "text written to the guest console (hvc0) appears in the log" || bad "console write not in the log"

M3="FOLLOW-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
( sleep 2; gexec "$P" "echo $M3 > /dev/hvc0" >/dev/null ) & WPID=$!
logs "follow=true&lines=1" 8 > "$T/follow.out"; wait "$WPID"
grep -q "$M3" "$T/follow.out" && ok "follow streams console output written after it started" || bad "follow did not carry $M3: $(cut -c1-200 "$T/follow.out")"
# lines=1 starts the stream at the last line, and the unterminated "login:" prompt counts as a line of its own, so the line before it
# is replayed too; what matters is that it is a tail, not the whole log.
! grep -q "Debian GNU/Linux 13" "$T/follow.out" && [[ "$(wc -c < "$T/follow.out" | tr -d ' ')" -lt 400 ]] \
  && ok "follow with lines=1 starts at the tail, not the whole log ($(wc -c < "$T/follow.out" | tr -d ' ') bytes)" || bad "follow replayed the log: $(cut -c1-200 "$T/follow.out")"

CODE="$(curl -s -o /dev/null -w '%{http_code}' "$B/00000000-0000-0000-0000-000000000000/logs")"
[[ "$CODE" == 400 ]] && ok "an unknown VM is refused (400, not 404)" || bad "unknown VM: $CODE"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz logs: PASS"
