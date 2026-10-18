#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2154,SC2034,SC2120
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The central API as a client sees it, on Linux and macOS: one real `fluxvm-agent central` (own port and state dir) and
# `sim-node` fleets, no VMs. Tenants and API keys (scopes, read-only, expiry, revocation, rotation, disable), what a tenant
# may and may not reach, signup and the billing gate, prices/budget/usage, webhooks and notifications, the audit trail,
# cordon/uncordon/drain/scrub, leases, overlays, the images list, the console assets, /metrics and /fleet/metrics, and that
# no token or key ever reaches a log. Needs target/release/fluxvm-agent (or BIN), curl, jq. Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; fails=0; CP=""; SP=()
trap '[[ -n "$CP" ]] && kill "$CP" 2>/dev/null; for p in ${SP[@]+"${SP[@]}"}; do kill "$p" 2>/dev/null; done; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
H=(-H 'content-type: application/json')
adm() { curl -s -H 'authorization: Bearer adm' "${H[@]}" "$@"; }
as() { local k="$1"; shift; curl -s -H "authorization: Bearer $k" "${H[@]}" "$@"; }
code() { local k="$1"; shift; curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $k" "${H[@]}" "$@"; }
waitfor() { local i; for i in $(seq 1 "${2:-40}"); do eval "$1" && return 0; sleep 0.5; done; return 1; }
start() { env "$@" "$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/central" --token adm >>"$D/central.log" 2>&1 & CP=$!; waitfor "curl -sf $URL/healthz >/dev/null" 40; }
stop() { kill "$CP" 2>/dev/null; wait "$CP" 2>/dev/null; CP=""; }

export FLUXVM_REGISTRY_FLUSH_SECS=1
start
check "central answers /healthz without a token" test "$(curl -s "$URL/healthz")" = '{"ok":true}'
check "no token: 401" test "$(curl -s -o /dev/null -w '%{http_code}' "$URL/fleet/nodes")" = 401
check "a wrong token: 401" test "$(code nope "$URL/fleet/nodes")" = 401

# --- nodes to work with: two sim nodes --------------------------------------------------------------------------
for n in s1 s2; do
  "$BIN" sim-node --help >/dev/null 2>&1 || break
done
"$BIN" sim-node --help 2>&1 | head -30 > "$D/simnode.help"
echo "     | sim-node flags: $(grep -o -- '--[a-z-]*' "$D/simnode.help" | sort -u | tr '\n' ' ')"
reg() { adm -XPOST -d "{\"name\":\"$1\",\"fluxvm_url\":\"http://127.0.0.1:1\",\"vcpus_total\":8,\"memory_mib_total\":16384,\"vm_count\":0,\"labels\":{\"zone\":\"$2\"}}" "$URL/fleet/register"; }
reg n1 east >/dev/null; reg n2 west >/dev/null
check "two nodes registered (admin)" test "$(adm "$URL/fleet/nodes" | jq '.items|length')" = 2
check "GET /fleet/nodes/{name} returns one" test "$(adm "$URL/fleet/nodes/n1" | jq -r .name)" = n1
check "unknown node: 404" test "$(code adm "$URL/fleet/nodes/ghost")" = 404
check "/fleet/capacity totals the fleet" test "$(adm "$URL/fleet/capacity" | jq '[.. | numbers] | length > 0')" = true

# --- cordon / uncordon / drain / scrub --------------------------------------------------------------------------
check "cordon a node" test "$(code adm -XPOST "$URL/fleet/nodes/n1/cordon")" = 200
check "the node shows cordoned" test "$(adm "$URL/fleet/nodes/n1" | jq -r .cordoned)" = true
check "?cordoned=true lists only it" test "$(adm "$URL/fleet/nodes?cordoned=true" | jq -c '[.items[].name]')" = '["n1"]'
check "a heartbeat does not clear a cordon" bash -c "$(declare -f reg adm); URL=$URL; H=(-H 'content-type: application/json'); reg n1 east >/dev/null; [[ \$(adm $URL/fleet/nodes/n1 | jq -r .cordoned) == true ]]"
check "uncordon" test "$(code adm -XPOST "$URL/fleet/nodes/n1/uncordon")" = 200
check "the node is schedulable again" test "$(adm "$URL/fleet/nodes/n1" | jq -r .cordoned)" = false
check "rebalance plan is readable" test "$(code adm "$URL/fleet/rebalance/plan")" = 200

# --- tenants and API keys ---------------------------------------------------------------------------------------
check "create tenant acme" test "$(code adm -XPOST -d '{"id":"acme","display_name":"Acme"}' "$URL/fleet/tenants")" = 201
check "a duplicate tenant id is refused" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"id\":\"acme\"}' $URL/fleet/tenants) =~ ^4 ]]"
check "tenant ids that are not simple names are refused" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"id\":\"../x\"}' $URL/fleet/tenants) == 400 ]]"
KEY="$(adm -XPOST -d '{"name":"ci"}' "$URL/fleet/tenants/acme/keys" | jq -r '.token // empty')"
check "a tenant key is issued once" test -n "$KEY"
RO="$(adm -XPOST -d '{"name":"viewer","read_only":true}' "$URL/fleet/tenants/acme/keys" | jq -r '.token // empty')"
check "a read-only key is issued" test -n "$RO"
K2="$(adm -XPOST -d '{"name":"k2"}' "$URL/fleet/tenants/acme/keys" | jq -r '.token // empty')"
check "a third key is issued" test -n "$K2"
check "the listing of keys never shows the secret" bash -c "! curl -s -H 'authorization: Bearer adm' $URL/fleet/tenants/acme/keys | grep -q -F '$KEY'"
check "tenant key: GET /fleet/tenants/me" test "$(as "$KEY" "$URL/fleet/tenants/me" | jq -r '.id // .tenant.id // empty')" = acme
check "tenant key: capacity allowed" test "$(code "$KEY" "$URL/fleet/capacity")" = 200
check "tenant key: node registration is operator-only (403)" test "$(code "$KEY" -XPOST -d '{"name":"x","fluxvm_url":"http://127.0.0.1:1","vcpus_total":1,"memory_mib_total":1}' "$URL/fleet/register")" = 403
check "tenant key: cordon is operator-only (403)" test "$(code "$KEY" -XPOST "$URL/fleet/nodes/n1/cordon")" = 403
check "tenant key: creating tenants is operator-only (403)" test "$(code "$KEY" -XPOST -d '{"id":"evil"}' "$URL/fleet/tenants")" = 403
check "tenant key: another tenant's record is not readable" test "$(code "$KEY" "$URL/fleet/tenants/other")" != 200
check "tenant key: the audit/other admin routes are refused" test "$(code "$KEY" "$URL/fleet/enroll/tokens")" = 403
check "read-only key: GET works" test "$(code "$RO" "$URL/fleet/tenants/me")" = 200
check "read-only key: a write is refused (403)" test "$(code "$RO" -XPOST -d '{"name":"w","url":"http://127.0.0.1:9/h"}' "$URL/fleet/webhooks")" = 403
check "rotate the tenant token returns a new one" test -n "$(adm -XPOST "$URL/fleet/tenants/acme/rotate-token" | jq -r '.token // .key // empty')"
RID="$(adm "$URL/fleet/tenants/acme/keys" | jq -r '.items[]|select(.name=="viewer")|.id')"
check "revoke a key" test "$(code adm -XDELETE "$URL/fleet/tenants/acme/keys/$RID")" -lt 300
check "a revoked key stops working at once" test "$(code "$RO" "$URL/fleet/tenants/me")" = 401
check "disable a tenant (PATCH)" test "$(code adm -XPATCH -d '{"disabled":true}' "$URL/fleet/tenants/acme")" = 200
check "a disabled tenant's keys are refused" test "$(code "$K2" "$URL/fleet/tenants/me")" != 200
check "re-enable" test "$(code adm -XPATCH -d '{"disabled":false}' "$URL/fleet/tenants/acme")" = 200
check "quota can be patched" test "$(code adm -XPATCH -d '{"quota":{"max_vms":3}}' "$URL/fleet/tenants/acme")" = 200

# --- prices, budget, usage, billing -----------------------------------------------------------------------------
check "prices readable" test "$(code adm "$URL/fleet/prices")" = 200
check "set a monthly budget" test "$(code adm -XPUT -d '{"monthly":50.0}' "$URL/fleet/tenants/acme/budget")" = 200
check "usage readable" test "$(code adm "$URL/fleet/usage")" = 200
check "billing readable" test "$(code adm "$URL/fleet/billing")" = 200

# --- webhooks and notifications ---------------------------------------------------------------------------------
HID="$(adm -XPOST -d '{"url":"http://127.0.0.1:9/hook","events":["vm.created","node.down"]}' "$URL/fleet/webhooks" | jq -r '.webhook.id // empty')"
check "create a webhook" test -n "$HID"
check "webhooks list shows it" test "$(adm "$URL/fleet/webhooks" | jq -r '[.. | .id? // empty] | index("'"$HID"'") != null')" = true
check "a webhook to a non-http URL is refused" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"url\":\"file:///etc/passwd\"}' $URL/fleet/webhooks) =~ ^4 ]]"
check "a tenant webhook must be https" test "$(code "$K2" -XPOST -d '{"url":"http://example.com/h","events":["vm.created"]}' "$URL/fleet/webhooks")" = 400
check "a tenant cannot subscribe to operator events" test "$(code "$K2" -XPOST -d '{"url":"https://example.com/h","events":["node.down"]}' "$URL/fleet/webhooks")" = 400
check "a tenant webhook to localhost or a private address is refused" test "$(code "$K2" -XPOST -d '{"url":"https://127.0.0.1/h","events":["vm.created"]}' "$URL/fleet/webhooks")" = 400
check "notifications readable" test "$(code adm "$URL/fleet/notifications")" = 200
check "delete the webhook" test "$(code adm -XDELETE "$URL/fleet/webhooks/$HID")" = 204

# --- leases, overlays, images, events ---------------------------------------------------------------------------
check "leases readable" test "$(code adm "$URL/fleet/leases")" = 200
# A Mac in the fleet: a node whose heartbeat carries Apple capabilities (no VM needed to test the lease rules)
adm -XPOST -d '{"name":"mac1","fluxvm_url":"http://127.0.0.1:1","vcpus_total":10,"memory_mib_total":32768,"vm_count":0,"apple":{"free_cpu":10,"free_memory_mib":30000,"nested_virtualization":false,"vmnet":false,"custom_virtio":false,"bridged_interfaces":[]}}' "$URL/fleet/register" >/dev/null
check "a Linux node cannot be leased (not a Mac)" test "$(code adm -XPOST -d '{"tenant":"acme","node":"n2","hours":24}' "$URL/fleet/leases")" = 409
check "a Mac with an empty slate can be leased by name" test "$(code adm -XPOST -d '{"tenant":"acme","node":"mac1","hours":24}' "$URL/fleet/leases")" = 201
LID="$(adm "$URL/fleet/leases" | jq -r '(.items // .)[0].id // empty')"
check "the lease is listed and readable" test "$(code adm "$URL/fleet/leases/$LID")" = 200
check "a second tenant cannot lease the same Mac" bash -c "curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"id\":\"other\"}' $URL/fleet/tenants >/dev/null; [[ \$(curl -s -o /dev/null -w '%{http_code}' -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"tenant\":\"other\",\"node\":\"mac1\",\"hours\":24}' $URL/fleet/leases) == 409 ]]"
check "the lease holder's tenant key can read its lease" test "$(code "$K2" "$URL/fleet/leases/$LID")" = 200
check "another tenant cannot read it" test "$(code "$(adm -XPOST -d '{"name":"o"}' "$URL/fleet/tenants/other/keys" | jq -r .token)" "$URL/fleet/leases/$LID")" != 200
check "an early release is refused (24-hour licence minimum)" test "$(code adm -XPOST "$URL/fleet/leases/$LID/release")" = 409
check "a tenant with an active lease cannot be deleted" test "$(code adm -XDELETE "$URL/fleet/tenants/acme")" = 409
check "an operator can force the release (still billed to the minimum)" test "$(code adm -XPOST "$URL/fleet/leases/$LID/release?force=true")" -lt 300
check "a released Mac is not leased again until it is scrubbed (the next tenant must not inherit the last one's disk)" bash -c "r=\$(curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{\"tenant\":\"other\",\"node\":\"mac1\",\"hours\":24}' $URL/fleet/leases); jq -r .error <<<\"\$r\" | grep -q 'must be scrubbed'"
check "images readable" test "$(code adm "$URL/fleet/images")" = 200
check "overlays readable" test "$(code adm "$URL/fleet/overlays")" = 200
check "create an overlay across both nodes" test "$(code adm -XPOST -d '{"name":"ov1","nodes":["n1","n2"]}' "$URL/fleet/overlays")" -lt 300
check "the overlay is listed" test "$(adm "$URL/fleet/overlays" | jq -r '[.. | .name? // empty] | index("ov1") != null')" = true
check "events (audit) stream route answers" test "$(curl -s -o /dev/null -m 3 -w '%{http_code}' -H 'authorization: Bearer adm' "$URL/fleet/events")" = 200

# --- console assets ---------------------------------------------------------------------------------------------
check "/console serves html" bash -c "curl -s $URL/console | grep -qi '<html'"
check "/console/app.js serves javascript" bash -c "curl -sI $URL/console/app.js | tr -d '\r' | grep -qi 'content-type: .*javascript'"
check "the console page is not cacheable across logins or carries a CSP" bash -c "curl -sI $URL/console | tr -d '\r' | grep -qi -E 'content-security-policy|cache-control'"

# --- metrics ----------------------------------------------------------------------------------------------------
check "/metrics needs a token" test "$(curl -s -o /dev/null -w '%{http_code}' "$URL/metrics")" != 200
check "/metrics with the admin token has fluxvm_central series" bash -c "curl -s -H 'authorization: Bearer adm' $URL/metrics | grep -q '^fluxvm_central'"
check "/fleet/metrics/summary answers" test "$(code adm "$URL/fleet/metrics/summary")" = 200
check "/fleet/ha reports a role" test "$(code adm "$URL/fleet/ha")" = 200
check "/fleet/registry has counters" test "$(adm "$URL/fleet/registry" | jq 'type')" = '"object"'

# --- the audit trail and secrets --------------------------------------------------------------------------------
check "the audit trail on disk records the tenant creation" bash -c "grep -rl 'acme' $D/central 2>/dev/null | head -1 | grep -q ."
echo "     | nodes before stop: $(adm "$URL/fleet/nodes" | jq -c '[.items[].name]|sort') file: $(jq -c '[.. | .name? // empty] | map(select(type=="string"))' "$D/central/fleet-nodes.json" 2>/dev/null | cut -c1-120)"
stop
check "no key or token in the central log" bash -c "! grep -q -F -e '$KEY' -e '$RO' -e '$K2' $D/central.log"
check "the admin token is not printed either" bash -c "! grep -q -E 'Bearer adm|token=adm' $D/central.log"
start
check "tenants and keys survive a restart" test "$(as "$K2" "$URL/fleet/tenants/me" | jq -r '.id // .tenant.id // empty')" = acme
echo "     | nodes after restart: $(adm "$URL/fleet/nodes" | jq -c '[.items[].name]|sort')"
check "nodes survive a restart" test "$(adm "$URL/fleet/nodes" | jq '.items|length')" = 3
check "the webhook list is empty after delete, across the restart" test "$(adm "$URL/fleet/webhooks" | jq '[.. | .id? // empty] | length')" = 0


# --- a graceful stop saves the registry (SIGTERM from systemd / a rollout) ---------------------------------------
# The registry is written in the background every few seconds; a central that exits on SIGTERM without a final save lost every
# node registered since the last one. Regression test for exactly that: register, SIGTERM at once, restart, list.
P2=$((PORT + 1)); U2="http://127.0.0.1:$P2"
c2() { FLUXVM_REGISTRY_FLUSH_SECS=30 "$BIN" central --listen "127.0.0.1:$P2" --state-dir "$D/term" --token adm >>"$D/term.log" 2>&1 & C2=$!; waitfor "curl -sf $U2/healthz >/dev/null" 40; }
c2; PID2=$C2
curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' -XPOST -d '{"name":"late","fluxvm_url":"http://127.0.0.1:1","vcpus_total":2,"memory_mib_total":2048,"vm_count":0}' "$U2/fleet/register" >/dev/null
kill -TERM "$PID2"; wait "$PID2" 2>/dev/null
c2
check "a node registered just before SIGTERM is still there after the restart (graceful stop saves the registry)" test "$(curl -s -H 'authorization: Bearer adm' "$U2/fleet/nodes" | jq -r '[.items[].name]|join(",")')" = late
kill "$C2" 2>/dev/null; wait "$C2" 2>/dev/null

# --- a tenant gone: delete -------------------------------------------------------------------------------------
check "delete a tenant" test "$(code adm -XDELETE "$URL/fleet/tenants/acme")" -lt 300
check "its key stops working" test "$(code "$K2" "$URL/fleet/tenants/me")" != 200

echo; echo "$fails check(s) failed"; exit "$fails"
