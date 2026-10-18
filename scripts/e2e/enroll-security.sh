#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Enrollment security checks against a real central (docs/fleet-enroll.md): every way a join token or a node
# credential must be refused, what a credential may reach, the three --node-auth modes, and that no secret reaches
# a log. Needs target/release/fluxvm-agent, curl, jq. Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
export FLUXVM_REGISTRY_FLUSH_SECS=1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; fails=0; CP=""
trap '[[ -n "$CP" ]] && kill "$CP" 2>/dev/null; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
adm() { curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@"; }
admc() { code -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@"; }
start() { "$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/state" --token adm --node-auth "$1" >"$D/central.log" 2>&1 & CP=$!; sleep 1.5; }
stop() { kill -9 "$CP" 2>/dev/null; wait "$CP" 2>/dev/null; CP=""; }
ID() { printf '%032x' "$1"; }
enroll() { curl -s -H "authorization: Bearer $1" -H 'content-type: application/json' -d "{\"name\":\"$2\",\"node_id\":\"$3\"}" "$URL/fleet/enroll"; }
enrollc() { code -H "authorization: Bearer $1" -H 'content-type: application/json' -d "{\"name\":\"$2\",\"node_id\":\"$3\"}" "$URL/fleet/enroll"; }
beat() { # CREDENTIAL NAME -> http code
  code -H "authorization: Bearer $1" -H 'content-type: application/json' \
    -d "{\"name\":\"$2\",\"fluxvm_url\":\"http://127.0.0.1:1\",\"vcpus_total\":4,\"memory_mib_total\":4096,\"vm_count\":0,\"labels\":{},\"daemon\":{\"reachable\":true}}" "$URL/fleet/register"; }
mk() { adm -XPOST -d "$1" "$URL/fleet/enroll/tokens"; }

start both
# --- join token rules
T="$(mk '{"max_uses":2}')"; TID="$(jq -r .id <<<"$T")"; TOK="$(jq -r .token <<<"$T")"
check "the token has the fvjt_ shape" test "${TOK#fvjt_}" != "$TOK"
check "the token list never shows a secret" bash -c "! curl -s -H 'authorization: Bearer adm' $URL/fleet/enroll/tokens | grep -q '${TOK##*.}'"
C1="$(enroll "$TOK" n1 "$(ID 1)" | jq -r .credential)"
check "enroll returns an fvnc_ credential" test "${C1#fvnc_}" != "$C1"
check "the same name and node_id again returns the same credential" test "$(enroll "$TOK" n1 "$(ID 1)" | jq -r .credential)" = "$C1"
check "...and cost no extra use" test "$(adm "$URL/fleet/enroll/tokens" | jq ".items[]|select(.id==\"$TID\").uses")" = 1
check "another node_id for an enrolled name is 409" test "$(enrollc "$TOK" n1 "$(ID 2)")" = 409
check "one node_id under a second name is 409" test "$(enrollc "$TOK" n9 "$(ID 1)")" = 409
check "the original credential still works after the 409s" test "$(beat "$C1" n1)" = 200
enroll "$TOK" n2 "$(ID 2)" >/dev/null
check "a third node is refused once max_uses=2 is spent" test "$(enrollc "$TOK" n3 "$(ID 3)")" -ge 400
check "a wrong secret is refused" test "$(enrollc "${TOK%.*}.deadbeef" nx "$(ID 5)")" = 401
check "garbage as a token is refused" test "$(enrollc "notatoken" nx "$(ID 5)")" -ge 400
check "no Authorization header is refused" test "$(code -H 'content-type: application/json' -d '{"name":"q","node_id":"x"}' "$URL/fleet/enroll")" -ge 400
check "the fleet-admin token is not a join token" test "$(enrollc adm nz "$(ID 6)")" -ge 400
check "a malformed node_id is refused" test "$(enrollc "$(mk '{}' | jq -r .token)" nm "../../etc")" -ge 400

PT="$(mk '{"name_pattern":"rack1-*"}' | jq -r .token)"
check "a name outside name_pattern is refused" test "$(enrollc "$PT" other "$(ID 7)")" -ge 400
check "a name inside name_pattern is accepted" test "$(enrollc "$PT" rack1-a "$(ID 8)")" = 200

RT="$(mk '{}')"; RID="$(jq -r .id <<<"$RT")"; RTOK="$(jq -r .token <<<"$RT")"
check "revoking a token: 200/204" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -XDELETE -H 'authorization: Bearer adm' $URL/fleet/enroll/tokens/$RID) =~ ^2 ]]"
check "a revoked token is refused" test "$(enrollc "$RTOK" r1 "$(ID 9)")" -ge 400
check "revoking a token leaves enrolled nodes working" test "$(beat "$C1" n1)" = 200

XT="$(mk '{"ttl_secs":1}' | jq -r .token)"; sleep 2.2
check "an expired token is refused" test "$(enrollc "$XT" e1 "$(ID 10)")" -ge 400
check "ttl_secs=0 and max_uses=0 are rejected" test "$(admc -XPOST -d '{"ttl_secs":0}' "$URL/fleet/enroll/tokens")$(admc -XPOST -d '{"max_uses":0}' "$URL/fleet/enroll/tokens")" = 400400

# --- what a credential may reach
check "a credential registers its own node" test "$(beat "$C1" n1)" = 200
check "...but not another name (403)" test "$(beat "$C1" n2)" = 403
for r in "GET /fleet/nodes" "GET /fleet/enroll/tokens" "GET /fleet/enroll/nodes" "POST /fleet/enroll/tokens" "DELETE /fleet/nodes/n1" "POST /fleet/nodes/n2/revoke" "GET /fleet/registry"; do
  m="${r%% *}"; p="${r#* }"
  check "a node credential cannot $m $p" test "$(code -X"$m" -H "authorization: Bearer $C1" -H 'content-type: application/json' -d '{}' "$URL$p")" = 403
done
check "a node credential may read its own overlays" test "$(code -H "authorization: Bearer $C1" "$URL/fleet/nodes/n1/overlays")" != 403
check "...but not another node's" test "$(code -H "authorization: Bearer $C1" "$URL/fleet/nodes/n2/overlays")" = 403
check "a join token cannot register" test "$(beat "$TOK" n1)" -ge 400
check "a forged mac is refused" test "$(beat "${C1%.*}.0000000000000000000000000000000000000000000000000000000000000000" n1)" -ge 400
check "a tampered generation is refused" test "$(beat "$(sed -E 's/\.[0-9]+\./.99./' <<<"$C1")" n1)" -ge 400

# --- rotate, renew, revoke, rebind
C2="$(adm -XPOST "$URL/fleet/nodes/n1/rotate" | jq -r .credential)"
check "rotate issues a different credential" test -n "$C2" -a "$C2" != "$C1"
check "the new credential works" test "$(beat "$C2" n1)" = 200
check "the previous generation still works inside the window" test "$(beat "$C1" n1)" = 200
C3="$(curl -s -XPOST -H "authorization: Bearer $C2" "$URL/fleet/enroll/renew" | jq -r .credential)"
check "renew with the current credential gives the next generation" test "${C3#fvnc_}" != "$C3" -a "$C3" != "$C2"
check "a join token cannot renew" test "$(code -XPOST -H "authorization: Bearer $TOK" "$URL/fleet/enroll/renew")" -ge 400
adm -XPOST "$URL/fleet/nodes/n1/revoke" >/dev/null
check "revoke kills every generation at once" test "$(beat "$C1" n1)$(beat "$C2" n1)$(beat "$C3" n1)" = 401401401 -o "$(beat "$C3" n1)" = 403 -o "$(beat "$C3" n1)" = 401
check "a revoked node cannot rotate" test "$(admc -XPOST "$URL/fleet/nodes/n1/rotate")" = 404
adm -XPOST "$URL/fleet/nodes/n2/rebind" >/dev/null
check "rebind frees the name for new hardware" test "$(enrollc "$(mk '{}' | jq -r .token)" n2 "$(ID 22)")" = 200
check "unknown node action is 404" test "$(admc -XPOST "$URL/fleet/nodes/n2/explode")" = 404

# --- persistence of the secret: credentials survive a restart of central
C4="$(enroll "$(mk '{}' | jq -r .token)" p1 "$(ID 31)" | jq -r .credential)"
sleep 2; stop; start both
check "a credential still works after kill -9 and restart" test "$(beat "$C4" p1)" = 200
check "a revoked node stays revoked after restart" test "$(beat "$C3" n1)" -ge 400
check "the enroll secret file is mode 0600" test "$(stat -c %a "$D/state/fleet-enroll.json" 2>/dev/null || stat -f %Lp "$D/state/fleet-enroll.json")" = 600

# --- the three modes
stop; start legacy
check "legacy: /fleet/enroll is 404" test "$(enrollc "$TOK" l1 "$(ID 41)")" = 404
check "legacy: a node credential is refused" test "$(beat "$C4" p1)" -ge 400
check "legacy: the fleet token registers" test "$(beat adm lg)" = 200
stop; start enrolled
check "enrolled: the fleet token cannot register" test "$(beat adm lg2)" = 403
check "enrolled: a credential registers" test "$(beat "$C4" p1)" = 200
check "enrolled: the operator can still read the node list" test "$(admc "$URL/fleet/nodes")" = 200
stop; start both
check "both: the fleet token registers" test "$(beat adm lg3)" = 200
check "both: a credential registers" test "$(beat "$C4" p1)" = 200

# --- nothing secret in logs
check "no join-token secret in the central log" bash -c "! grep -q '${TOK##*.}' $D/central.log"
check "no credential mac in the central log" bash -c "! grep -q '${C4##*.}' $D/central.log"
check "the central log does not contain the admin token as a secret value" bash -c "! grep -qE 'Bearer adm|authorization' $D/central.log"

echo; echo "$fails failed"; exit "$fails"
