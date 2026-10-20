#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the fork dry run (`POST /v1/vms/{id}/fork/plan`) on vz guests. A forkable guest gets an ok plan with a memory
# bound, bad counts, parallelism and names are refused with their own codes, a guest on the Mac's NAT is refused with `forkable`
# and the real fork agrees, and planning creates no VM.
#   scripts/vz-fork-plan-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-forkplan.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7810}"
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
run_dir = "/tmp/fluxvm-run-forkplan"
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

mkvm() { # <name> <extra json>
  curl -sS -X POST "$B" -H 'content-type: application/json' \
    -d "{\"name\":\"$1\",\"backend\":\"vz\",\"image\":\"agent-micro\",\"vcpus\":2,\"memory_mib\":1024,$2}"
}
plan() { curl -sS -X POST "$B/$1/fork/plan" -H 'content-type: application/json' -d "$2"; }
codes() { jget 'sorted(r["code"] for r in d["refusals"])'; }
nvms() { curl -fs "$B" | jget 'len(d["items"] if isinstance(d, dict) else d)'; }

RESP="$(mkvm okvm '"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
RESP="$(mkvm natvm '"network":{"mode":"user"}')"
N="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create nat: $RESP"
wait_agent "$P" && ok "forkable guest $P is up" || bad "agent never answered"
BEFORE="$(nvms)"

R="$(plan "$P" '{"count":2}')"; echo "     $(echo "$R" | cut -c1-420)"
[[ "$(echo "$R" | jget 'd["ok"] and d["refusals"] == [] and d["count"] == 2')" == True ]] && ok "a forkable guest gets an ok plan for 2 children" || bad "plan: $R"
[[ "$(echo "$R" | jget '0 < d["predicted"]["extra_rss_mib_low"] <= d["predicted"]["extra_rss_mib_high"]')" == True ]] \
  && ok "the memory prediction is a low/high bound" || bad "prediction: $R"

[[ "$(plan "$P" '{"count":33}' | codes)" == '["count"]' ]] && ok "count 33 is refused with code count" || bad "count 33: $(plan "$P" '{"count":33}')"
[[ "$(plan "$P" '{"count":0}' | codes)" == '["count"]' ]] && ok "count 0 is refused with code count" || bad "count 0"
[[ "$(plan "$P" '{"count":2,"parallelism":0}' | codes)" == '["parallelism"]' ]] && ok "parallelism 0 is refused with code parallelism" || bad "parallelism"
[[ "$(plan "$P" '{"count":2,"name_prefix":"bad name!"}' | codes)" == '["name"]' ]] && ok "an invalid name prefix is refused with code name" || bad "name: $(plan "$P" '{"count":2,"name_prefix":"bad name!"}')"

curl -fs -X POST "$B/$P/pause" >/dev/null && [[ "$(status "$P")" == paused ]] && ok "forkable guest paused" || bad "pause"
[[ "$(plan "$P" '{"count":1}' | jget 'd["ok"]')" == True ]] && ok "the plan is also ok for a paused guest" || bad "paused plan: $(plan "$P" '{"count":1}')"
curl -fs -X POST "$B/$P/resume" >/dev/null || true

[[ "$(status "$N")" == running ]] || sleep 5
RN="$(plan "$N" '{"count":1}')"; echo "     nat guest: $(echo "$RN" | cut -c1-420)"
[[ "$(echo "$RN" | jget 'd["ok"] == False and [r["code"] for r in d["refusals"]] == ["forkable"]')" == True ]] \
  && ok "a guest on the Mac's NAT is refused with code forkable" || bad "nat plan: $RN"
CODE="$(curl -s -o "$T/real.json" -w '%{http_code}' -X POST "$B/$N/fork" -H 'content-type: application/json' -d '{"count":1}')"
[[ "$CODE" == 400 ]] && ok "the real fork of that guest is refused too (400)" || bad "real fork returned $CODE: $(cat "$T/real.json")"

[[ "$(nvms)" == "$BEFORE" ]] && ok "planning created no VM ($BEFORE VMs before and after)" || bad "VM count changed: $BEFORE -> $(nvms)"
for V in "$N" "$P"; do curl -fs -X DELETE "$B/$V" >/dev/null || true; done
echo "vz fork plan: PASS"
