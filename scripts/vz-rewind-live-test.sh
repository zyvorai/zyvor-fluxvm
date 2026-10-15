#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of agent-session rewind on the vz backend: automatic points, undo, pin, bisect and branch (docs/agent-sessions.md).
#   scripts/vz-rewind-live-test.sh [image]        (default image: debian-13 from the catalog / image store)
# Needs Apple silicon macOS, the Xcode command line tools, Rust, python3 and curl. Takes a few minutes (cold boot).
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMG="${1:-debian-13}"
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-rewind-live.XXXXXX")"
PORT="${FLUXVM_LIVE_PORT:-7798}"
TOKEN="live-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
U="http://127.0.0.1:$PORT"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }
api() { curl -s -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
jget() {
  python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"
}

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-rewind-live"
[auth]
require = true
[[auth.tokens]]
name = "live"
token = "$TOKEN"
role = "admin"
EOT
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "$U/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "$U/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

SESSION="$(api -X POST "$U/v1/agents/sessions" -d "{\"agent\":\"live\",\"policy\":{\"preset\":\"offline\"},\"runtime\":{\"image\":\"$IMG\"}}")"
S="$(echo "$SESSION" | jget 'd.get("id","")')"
[[ -n "$S" ]] || bad "session create failed: $SESSION"
SU="$U/v1/agents/sessions/$S"
run() { # command -> stdout of the guest command
  api -X POST "$SU/exec" -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 60}))' "$1")" \
    | jget 'd.get("stdout","").strip()'
}
points() { api "$SU/rewind" | jget 'len(d["points"])'; }

# 1. Opt in, then a history whose third action breaks the machine.
[[ "$(api -X POST "$SU/rewind/config" -d '{"enabled":true,"max_points":10}' | jget 'd["enabled"]')" == True ]] \
  && ok "rewind enabled" || bad "could not enable rewind"
run 'echo v1 > /tmp/state' >/dev/null
run 'echo v2 > /tmp/state' >/dev/null
run 'touch /tmp/broken' >/dev/null
run 'echo v3 > /tmp/state' >/dev/null
[[ "$(points)" == 4 ]] && ok "a point before each of the 4 execs" || bad "points: $(api "$SU/rewind")"

# 2. Bisect names the command that broke the test, and leaves the machine as it was.
B="$(api -X POST "$SU/rewind/bisect" -d '{"test":"! test -e /tmp/broken"}')"
[[ "$(echo "$B" | jget '"touch /tmp/broken" in (d["culprit"]["command"] or "")')" == True ]] \
  && ok "bisect found the culprit: $(echo "$B" | jget 'd["culprit"]["command"]') ($(echo "$B" | jget 'd["ms"]') ms)" || bad "bisect: $B"
[[ "$(run 'cat /tmp/state')" == v3 ]] && ok "the machine is back at its head after bisect" || bad "bisect left the machine elsewhere"

# 3. Pin the point before the culprit, then undo one step: the last exec is reversed.
TAG="$(api "$SU/rewind" | jget '[p["tag"] for p in d["points"] if "touch /tmp/broken" in (p.get("command") or "")][0]')"
[[ "$(api -X POST "$SU/rewind/pin" -d "{\"tag\":\"$TAG\"}" -o /dev/null -w '%{http_code}')" == 200 ]] \
  && ok "point $TAG pinned" || bad "pin failed"
api -X POST "$SU/rewind/undo" -d '{"steps":1}' >/dev/null
[[ "$(run 'cat /tmp/state')" == v2 ]] && ok "undo reversed the last action (state v2)" || bad "undo: state is $(run 'cat /tmp/state')"

# 4. Undo to the pinned point: the breaking command is reversed too.
api -X POST "$SU/rewind/undo" -d "{\"to\":\"$TAG\"}" >/dev/null
[[ "$(run 'test -e /tmp/broken && echo yes || echo no')" == no ]] && ok "undo to a tag removed the breaking file" || bad "file still there"

# 5. Branch from the earliest point; the parent is untouched.
FIRST="$(api "$SU/rewind" | jget 'd["points"][0]["tag"]')"
BR="$(api -X POST "$SU/rewind/branch" -d "{\"tag\":\"$FIRST\"}")"
C="$(echo "$BR" | jget 'd["items"][0]["id"]' 2>/dev/null || true)"
[[ -n "$C" && "$C" != None ]] || bad "branch failed: $BR"
CS="$U/v1/agents/sessions/$C"
CO="$(api -X POST "$CS/exec" -d '{"command":"cat /tmp/state 2>/dev/null; test -e /tmp/broken && echo broken","timeout_seconds":60}' | jget 'd.get("stdout","").strip()')"
[[ "$CO" != *v2* && "$CO" != *v3* ]] && ok "the child starts from the old moment ($(echo $CO | tr '\n' ' '))" || bad "child state: $CO"
[[ "$(run 'cat /tmp/state')" == v2 ]] && ok "the parent is untouched by the branch" || bad "parent changed"
api -X DELETE "$CS" >/dev/null || true

api -X DELETE "$SU" >/dev/null || true
echo "rewind: PASS"
