#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2154,SC2034
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# High availability with real processes (Linux and macOS): a local redis-server and two `fluxvm-agent central` on it.
# One leader, the standby answers 503 naming it; the leader's state (nodes, tenants, keys) reaches the standby when the
# leader is `kill -9`ed; the old leader comes back as a standby (no split brain); a leader that is paused past its TTL
# (SIGSTOP) is replaced, and when it wakes it does not overwrite the new leader's state (fencing).
# Needs redis-server, curl, jq and target/release/fluxvm-agent (or BIN). Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
command -v redis-server >/dev/null || { echo "needs redis-server" >&2; exit 2; }
D="$(mktemp -d)"; B0=$((20000 + RANDOM % 20000)); RP=$B0; PA=$((B0 + 1)); PB=$((B0 + 2)); fails=0; PIDS=()
trap 'for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -CONT "$p" 2>/dev/null; kill "$p" 2>/dev/null; done; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
api() { local port="$1"; shift; curl -s -m 4 -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@" 2>/dev/null; }
code() { local port="$1"; shift; curl -s -m 4 -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@" 2>/dev/null; }
waitfor() { local i; for i in $(seq 1 "${2:-60}"); do eval "$1" && return 0; sleep 0.5; done; return 1; }
is_leader() { [[ "$(api "$1" "http://127.0.0.1:$1/fleet/ha" | jq -r .is_leader)" == true ]]; }
central() { # NAME PORT  -> sets PID_NAME
  mkdir -p "$D/st-$1"
  "$BIN" central --listen "127.0.0.1:$2" --state-dir "$D/st-$1" --token adm --ha-redis "redis://127.0.0.1:$RP" --ha-cluster e2e \
    --advertise-url "http://127.0.0.1:$2" --ha-ttl-secs 3 >>"$D/c-$1.log" 2>&1 &
  PIDS+=($!); eval "PID_$1=$!"
}
reg() { api "$1" -XPOST -d "{\"name\":\"$2\",\"fluxvm_url\":\"http://127.0.0.1:1\",\"vcpus_total\":4,\"memory_mib_total\":8192,\"vm_count\":0}" "http://127.0.0.1:$1/fleet/register"; }

redis-server --port "$RP" --bind 127.0.0.1 --save "" --appendonly no --dir "$D" >"$D/redis.log" 2>&1 & PIDS+=($!)
waitfor "curl -s -m1 telnet://127.0.0.1:$RP </dev/null >/dev/null 2>&1 || (exec 3<>/dev/tcp/127.0.0.1/$RP) 2>/dev/null" 20
central A "$PA"; central B "$PB"
check "both centrals answer /healthz" waitfor "curl -sf http://127.0.0.1:$PA/healthz >/dev/null && curl -sf http://127.0.0.1:$PB/healthz >/dev/null" 40
check "exactly one of them leads" waitfor '[[ "$(is_leader $PA; echo $?)$(is_leader $PB; echo $?)" =~ ^(01|10)$ ]]' 40
if is_leader "$PA"; then L=$PA; S=$PB; LN=A; SN=B; else L=$PB; S=$PA; LN=B; SN=A; fi
check "the standby names the leader in its /fleet/ha" test "$(api "$S" "http://127.0.0.1:$S/fleet/ha" | jq -r .leader)" = "http://127.0.0.1:$L"
check "the standby answers fleet requests with 503 and the leader's URL" bash -c "r=\$(curl -s -H 'authorization: Bearer adm' http://127.0.0.1:$S/fleet/nodes); [[ \$(jq -r .leader <<<\"\$r\") == http://127.0.0.1:$L ]]"
check "the standby still answers /healthz" test "$(code "$S" "http://127.0.0.1:$S/healthz")" = 200

# state written on the leader
reg "$L" n1 >/dev/null; reg "$L" n2 >/dev/null
check "the leader took two nodes" test "$(api "$L" "http://127.0.0.1:$L/fleet/nodes" | jq '.items|length')" = 2
api "$L" -XPOST -d '{"id":"acme"}' "http://127.0.0.1:$L/fleet/tenants" >/dev/null
TK="$(api "$L" -XPOST -d '{"name":"ci"}' "http://127.0.0.1:$L/fleet/tenants/acme/keys" | jq -r .token)"
check "a tenant and a key were created on the leader" test -n "$TK"
sleep 2   # the leader mirrors into redis on change; give it a beat

# kill -9 the leader
eval "kill -9 \$PID_$LN"; wait "$(eval echo \$PID_$LN)" 2>/dev/null
T0=$SECONDS
check "the standby takes over within the TTL plus a margin" waitfor "is_leader $S" 40
echo "     | takeover took $((SECONDS - T0))s (ttl 3s)"
check "the new leader has the nodes of the old one" test "$(api "$S" "http://127.0.0.1:$S/fleet/nodes" | jq -c '[.items[].name]|sort')" = '["n1","n2"]'
check "the new leader has the tenant" test "$(code "$S" "http://127.0.0.1:$S/fleet/tenants/acme")" = 200
check "the old tenant key works on the new leader" test "$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $TK" "http://127.0.0.1:$S/fleet/tenants/me")" = 200
reg "$S" n3 >/dev/null
check "the new leader accepts writes" test "$(api "$S" "http://127.0.0.1:$S/fleet/nodes" | jq '.items|length')" = 3

# the old leader returns: it must come back as a standby
central "$LN" "$L"
check "the restarted central comes up" waitfor "curl -sf http://127.0.0.1:$L/healthz >/dev/null" 40
sleep 3
check "it is a standby, not a second leader" test "$(is_leader "$L"; echo $?)" = 1
check "it points at the leader that took over" test "$(api "$L" "http://127.0.0.1:$L/fleet/ha" | jq -r .leader)" = "http://127.0.0.1:$S"
check "the node written during the failover is still there" test "$(api "$S" "http://127.0.0.1:$S/fleet/nodes" | jq '.items|length')" = 3

# fencing: freeze the leader past its TTL; the standby takes over; unfreeze; the old leader must not write
CUR=$S; OTH=$L; CN=$SN; ON=$LN
eval "kill -STOP \$PID_$CN"
check "a frozen leader is replaced after the TTL" waitfor "is_leader $OTH" 40
reg "$OTH" n4 >/dev/null
reg "$OTH" n5 >/dev/null
eval "kill -CONT \$PID_$CN"
sleep 4
check "the thawed old leader no longer leads" test "$(is_leader "$CUR"; echo $?)" = 1
check "its late writes did not erase the new leader's nodes" test "$(api "$OTH" "http://127.0.0.1:$OTH/fleet/nodes" | jq -c '[.items[].name]|sort')" = '["n1","n2","n3","n4","n5"]'
check "the thawed central answers fleet requests with 503" test "$(code "$CUR" "http://127.0.0.1:$CUR/fleet/nodes")" = 503
check "no credential in any central log" bash -c "! grep -q -F -e '$TK' $D/c-A.log $D/c-B.log"

echo; echo "$fails check(s) failed"; exit "$fails"
