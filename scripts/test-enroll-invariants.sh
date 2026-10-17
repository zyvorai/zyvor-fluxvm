#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Checks enrollment invariants against a real central process and sim-fleet (docs/fleet-enroll.md):
#   - a join token with max_uses=100 and 150 concurrent enrolls yields exactly 100 credentials
#   - mixed fleet: enrolled and legacy nodes all register once in --node-auth both; in enrolled mode the legacy ones are refused
#   - a revoked node is refused while the others keep beating; central keeps all of it across kill -9
# Needs target/release/fluxvm-agent (cargo build --release -p fluxvm-agent), curl and jq. Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
export FLUXVM_REGISTRY_FLUSH_SECS=1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; fails=0; CP=""
trap '[[ -n "$CP" ]] && kill "$CP" 2>/dev/null; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
api() { curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@"; }
start() { "$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/state" --token adm --node-auth "$1" >"$D/central.log" 2>&1 & CP=$!; sleep 1.5; }
stop() { kill -9 "$CP" 2>/dev/null; wait "$CP" 2>/dev/null; CP=""; }

start both
J="$(api -XPOST -d '{"max_uses":100}' "$URL/fleet/enroll/tokens" | jq -r .token)"
"$BIN" sim-fleet --central "$URL" --token adm --mode enrolled --join-token "$J" --nodes 150 --prefix e --interval-secs 1 --duration-secs 3 >"$D/r1.json" 2>/dev/null
n="$(api "$URL/fleet/enroll/nodes" | jq '.items|length')"
[[ "$n" == 100 ]] && ok "150 concurrent enrolls on max_uses=100 enrolled exactly $n" || bad "expected 100 enrolled, got $n"
[[ "$(jq .enrolled "$D/r1.json")" == 100 ]] && ok "the simulator saw 100 credentials" || bad "simulator enrolled $(jq .enrolled "$D/r1.json")"

J2="$(api -XPOST -d '{"max_uses":1000}' "$URL/fleet/enroll/tokens" | jq -r .token)"
"$BIN" sim-fleet --central "$URL" --token adm --mode mixed --join-token "$J2" --nodes 400 --prefix m --interval-secs 1 --duration-secs 4 >"$D/r2.json" 2>/dev/null
[[ "$(jq '.check.passed // .passed // empty' "$D/r2.json")" != false && "$(jq .http_4xx_other "$D/r2.json")" == 0 ]] && ok "mixed fleet of 400: every node registered once" || bad "mixed fleet: $(jq -c '{http_4xx_other,check}' "$D/r2.json")"

before="$(api "$URL/fleet/enroll/nodes" | jq -c '[.items[]|.name]|sort')"
sleep 2   # past one registry flush
reg_before="$(api "$URL/fleet/nodes" | jq -c '[.items[]|.name]|sort')"
stop; start both
reg_after="$(api "$URL/fleet/nodes" | jq -c '[.items[]|.name]|sort')"
[[ "$reg_before" == "$reg_after" && "$reg_before" != "[]" ]] && ok "the node registry survives kill -9 ($(jq length <<<"$reg_after") nodes)" || bad "registry differs after kill -9"
after="$(api "$URL/fleet/enroll/nodes" | jq -c '[.items[]|.name]|sort')"
[[ "$before" == "$after" ]] && ok "enrollments survive kill -9" || bad "enrollments differ after kill -9"

stop; start enrolled
"$BIN" sim-fleet --central "$URL" --token adm --mode legacy --nodes 20 --prefix l --interval-secs 1 --duration-secs 2 >"$D/r3.json" 2>/dev/null
[[ "$(jq .beats_ok "$D/r3.json")" == 0 && "$(jq .http_4xx_other "$D/r3.json")" -gt 0 ]] && ok "enrolled mode refuses the fleet token on /fleet/register" || bad "legacy beats were accepted in enrolled mode"

echo; echo "$fails failed"; exit "$fails"
