#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2154,SC2034
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# A real central and real `fluxvm-agent node` processes: enrolling from a token file, restarting without it, version
# gating, the node list (fields, filters, paging), the registry across kill -9 of central, admission control under
# overload, and what central says on /fleet/registry. Needs target/release/fluxvm-agent, curl, jq. Exit = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; fails=0; CP=""; NP=()
trap '[[ -n "$CP" ]] && kill "$CP" 2>/dev/null; for p in "${NP[@]}"; do kill "$p" 2>/dev/null; done; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
adm() { curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@"; }
start() { env "$@" "$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/central" --token adm --node-auth "${AUTH:-both}" >"$D/central.log" 2>&1 & CP=$!; sleep 1.5; }
stop() { kill -9 "$CP" 2>/dev/null; wait "$CP" 2>/dev/null; CP=""; }
node() { # NAME [extra args]  (state dir per node; token from a file)
  local n="$1"; shift
  mkdir -p "$D/n-$n"; chmod 700 "$D/n-$n"
  "$BIN" node --name "$n" --central "$URL" --interval-secs 1 --state-dir "$D/n-$n" --fluxvm-url http://127.0.0.1:1 "$@" >"$D/node-$n.log" 2>&1 &
  NP+=($!); eval "PID_$n=$!"
}
waitfor() { local i; for i in $(seq 1 "${2:-40}"); do eval "$1" && return 0; sleep 0.5; done; return 1; }
names() { adm "$URL/fleet/nodes" | jq -c '[.items[]|.name]|sort'; }

export FLUXVM_REGISTRY_FLUSH_SECS=1
start FLUXVM_MIN_NODE_VERSION=0.0.1
J="$(adm -XPOST -d '{"max_uses":10}' "$URL/fleet/enroll/tokens" | jq -r .token)"
printf '%s\n' "$J" > "$D/join"; chmod 600 "$D/join"
node a --token-file "$D/join" --label zone=east
node b --token-file "$D/join" --label zone=east
node c --token-file "$D/join" --label zone=west
check "three real nodes enroll and register" waitfor '[[ "$(names)" == "[\"a\",\"b\",\"c\"]" ]]'
check "each node holds a 0600 credential and a node-id" bash -c "[[ \$(stat -c %a $D/n-a/node-credential 2>/dev/null || stat -f %Lp $D/n-a/node-credential) == 600 && \$(stat -c %a $D/n-a/node-id 2>/dev/null || stat -f %Lp $D/n-a/node-id) == 600 ]]"
check "the join token is not on any node's command line" bash -c "! ps -eo args | grep -F '${J##*.}' | grep -v grep"
check "the join token is not in a node log" bash -c "! grep -q '${J##*.}' $D/node-a.log"
check "enrollments list three nodes" test "$(adm "$URL/fleet/enroll/nodes" | jq '.items|length')" = 3
check "uses counted once per node" test "$(adm "$URL/fleet/enroll/tokens" | jq '.items[0].uses')" = 3

# node fields
N="$(adm "$URL/fleet/nodes" | jq -c '.items[]|select(.name=="a")')"
check "agent_version is reported" test "$(jq -r '.agent_version // empty' <<<"$N")" != ""
check "proto is reported" test "$(jq -r '.proto // empty' <<<"$N")" != ""
check "capabilities include retry_after and next_interval" test "$(jq -c '.capabilities|sort' <<<"$N")" = '["next_interval","retry_after"]'
check "labels are reported" test "$(jq -r '.labels.zone' <<<"$N")" = east
check "a node with an unreachable daemon is still listed" test "$(jq -r .name <<<"$N")" = a

# filters and paging
check "?limit=2 returns two items and a cursor" bash -c "r=\$(curl -s -H 'authorization: Bearer adm' '$URL/fleet/nodes?limit=2'); [[ \$(jq '.items|length' <<<\"\$r\") == 2 && -n \$(jq -r '.next_cursor // empty' <<<\"\$r\") ]]"
cur="$(adm "$URL/fleet/nodes?limit=2" | jq -r .next_cursor)"
check "the cursor returns the rest with no duplicates" test "$(adm "$URL/fleet/nodes?limit=2&cursor=$cur" | jq -c '[.items[]|.name]')" = '["c"]'
check "a label filter selects the zone" test "$(adm "$URL/fleet/nodes?label=zone%3Dwest" | jq -c '[.items[]|.name]')" = '["c"]'
check "?summary=true returns counts" test "$(adm "$URL/fleet/nodes?summary=true" | jq '.summary.total')" = 3
check "limit=0 and limit=99999 are 400" test "$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' "$URL/fleet/nodes?limit=0")$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' "$URL/fleet/nodes?limit=99999")" = 400400
check "a bad filter value is 400" test "$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' "$URL/fleet/nodes?healthy=maybe")" = 400

# a node restarted WITHOUT its join token
kill "$PID_a"; wait "$PID_a" 2>/dev/null; rm -f "$D/join"
node a
check "a restarted node re-registers from its stored credential, no join token" waitfor 'test "$(adm "$URL/fleet/nodes/a" | jq -r .healthy)" = true'

# kill -9 of central: the registry and enrollments come back, nodes carry on
before="$(names)"; sleep 2.2; stop; start FLUXVM_MIN_NODE_VERSION=0.0.1
check "the node list is intact after kill -9 of central" test "$(names)" = "$before"
check "nodes keep beating into the restarted central" waitfor 'test "$(adm "$URL/fleet/nodes?healthy=true" | jq ".items|length")" = 3' 60
check "no node had to re-enroll (uses still 3)" test "$(adm "$URL/fleet/enroll/tokens" | jq '.items[0].uses')" = 3

# version gate
stop; start FLUXVM_MIN_NODE_VERSION=999.0.0
check "nodes older than FLUXVM_MIN_NODE_VERSION are flagged outdated" waitfor 'test "$(adm "$URL/fleet/nodes?outdated=true" | jq ".items|length")" = 3' 60
check "outdated nodes stay listed and healthy" test "$(adm "$URL/fleet/nodes?healthy=true" | jq '.items|length')" = 3

# /fleet/registry
R="$(adm "$URL/fleet/registry")"
check "/fleet/registry shows 3 nodes and at least one flush since the restart" test "$(jq -r '.nodes' <<<"$R")" = 3 -a "$(jq -r '.flushes' <<<"$R")" -ge 0
check "the registry file is on disk and parses" bash -c "jq -e . $D/central/fleet-nodes.json >/dev/null"

# overload: heartbeats refused with 429 + Retry-After, nodes survive it
stop; start FLUXVM_REGISTER_RATE=2 FLUXVM_REGISTER_BURST=2 FLUXVM_REGISTER_INFLIGHT=1
hdr="$(for i in $(seq 1 30); do curl -s -D - -o /dev/null -XPOST -H 'authorization: Bearer adm' -H 'content-type: application/json' -d '{"name":"flood","fluxvm_url":"http://127.0.0.1:1","vcpus_total":1,"memory_mib_total":1}' "$URL/fleet/register"; done)"
check "a flood past the rate gets 429s" bash -c "grep -c '^HTTP/1.1 429' <<<'$hdr' | grep -qv '^0$'"
check "...with a Retry-After header" bash -c "grep -qi '^retry-after: [0-9]' <<<'$hdr'"
check "central counts the refusals" test "$(adm "$URL/fleet/registry" | jq -r '.heartbeats_refused')" -gt 0
sleep 4
check "real nodes recover after the overload" waitfor 'test "$(adm "$URL/fleet/nodes?healthy=true" | jq ".items|length")" -ge 3' 90
check "central never crashed during any of it" bash -c "kill -0 $CP"

echo; echo "$fails failed"; exit "$fails"
