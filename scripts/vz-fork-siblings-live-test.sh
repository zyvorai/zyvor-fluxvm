#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: fork a vz guest into two children (`POST /v1/vms/{id}/fork` with count 2). Each child must hold the parent's
# memory marker, get its own machine-id and hostname, and diverge independently: a file written in one child must not appear
# in its sibling or the parent.
#   scripts/vz-fork-siblings-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-forksiblings.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7808}"
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
run_dir = "/tmp/fluxvm-run-forksiblings"
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

MARK="SIBLING-FORK-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
gexec "$P" "echo $MARK > /dev/shm/marker" >/dev/null
[[ "$(gexec "$P" 'cat /dev/shm/marker')" == "$MARK" ]] && ok "memory marker written in the parent" || bad "marker not written"

curl -fs -X POST "$B/$P/pause" >/dev/null && [[ "$(status "$P")" == paused ]] && ok "parent paused" || bad "pause: $(status "$P")"
FORK="$(curl -sS -X POST "$B/$P/fork" -H 'content-type: application/json' -d '{"count":2}')"
read -r C1 C2 < <(python3 -c '
import sys,json
d=json.load(sys.stdin)
print(*[i["id"] for i in d.get("items",[])][:2])' <<<"$FORK")
[[ -n "${C1:-}" && -n "${C2:-}" && "$C1" != "$C2" ]] && ok "fork returned two children $C1 $C2" || bad "fork failed: $FORK"
curl -fs -X POST "$B/$P/resume" >/dev/null && [[ "$(status "$P")" == running ]] && ok "parent resumed" || bad "resume: $(status "$P")"

for V in "$P" "$C1" "$C2"; do wait_agent "$V" && ok "$V answers through the agent" || bad "$V agent never answered"; done
for V in "$C1" "$C2"; do
  [[ "$(gexec "$V" 'cat /dev/shm/marker')" == "$MARK" ]] && ok "child $V holds the parent's memory marker" || bad "child $V lost the marker"
done

ident() { gexec "$1" 'echo "$(cat /etc/machine-id) $(hostname)"'; }
IP="$(ident "$P")"; I1="$(ident "$C1")"; I2="$(ident "$C2")"
echo "     parent: $IP"; echo "     child1: $I1"; echo "     child2: $I2"
[[ -n "$I1" && -n "$I2" && "$I1" != "$IP" && "$I2" != "$IP" && "$I1" != "$I2" ]] && ok "each guest has its own machine-id and hostname" || bad "identities are not unique"
[[ "${I1%% *}" != "${I2%% *}" && "${I1%% *}" != "${IP%% *}" ]] && ok "machine-ids all differ" || bad "machine-id shared"

gexec "$C1" 'echo only-c1 > /tmp/diverge; echo c1 > /dev/shm/marker; sync' >/dev/null
[[ "$(gexec "$C1" 'cat /tmp/diverge')" == only-c1 ]] && ok "child 1 wrote its own file" || bad "child 1 write failed"
[[ "$(gexec "$C2" 'cat /tmp/diverge 2>/dev/null || echo none')" == none ]] && ok "the sibling does not see child 1's disk write" || bad "disk write leaked to the sibling"
[[ "$(gexec "$P" 'cat /tmp/diverge 2>/dev/null || echo none')" == none ]] && ok "the parent does not see child 1's disk write" || bad "disk write leaked to the parent"
[[ "$(gexec "$C2" 'cat /dev/shm/marker')" == "$MARK" && "$(gexec "$P" 'cat /dev/shm/marker')" == "$MARK" ]] \
  && ok "child 1's memory change stays in child 1" || bad "memory change leaked"
for V in "$C1" "$C2" "$P"; do curl -fs -X DELETE "$B/$V" >/dev/null || true; done
echo "vz fork into two independent children: PASS"
