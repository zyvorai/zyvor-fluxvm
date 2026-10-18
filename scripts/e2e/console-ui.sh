#!/usr/bin/env bash
# shellcheck disable=SC2034,SC2154
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The fleet console in a headless Chrome (Linux and macOS): central + two simulated Macs + a tenant with a lease and a VM, then
# scripts/e2e/console-ui.mjs signs in as operator and tenant and opens every page. Needs node, puppeteer (npm i -g puppeteer, or
# puppeteer-core with CHROME_PATH), curl, jq and target/release/fluxvm-agent (or BIN). Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BIN="${BIN:-target/release/fluxvm-agent}"
[[ -x "$BIN" ]] || { echo "build $BIN first" >&2; exit 2; }
command -v node >/dev/null || { echo "needs node" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://localhost:$PORT"; PIDS=()
trap 'for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null; done; rm -rf "$D"' EXIT
H=(-H 'authorization: Bearer adm' -H 'content-type: application/json')
"$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/c" --token adm >"$D/central.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 40); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done
for n in sim-mac sim-mac2; do "$BIN" sim-node --name "$n" --central "http://127.0.0.1:$PORT" --token adm --interval-secs 2 >/dev/null 2>&1 & PIDS+=($!); done
for _ in $(seq 1 40); do [[ "$(curl -s "${H[@]}" "http://127.0.0.1:$PORT/fleet/nodes" | jq '.items|length')" == 2 ]] && break; sleep 0.5; done
TK="$(curl -s "${H[@]}" -XPOST -d '{"id":"acme","display_name":"Acme"}' "http://127.0.0.1:$PORT/fleet/tenants" | jq -r .token)"
curl -s -o /dev/null -H "authorization: Bearer $TK" -H 'content-type: application/json' -XPOST -d '{}' "http://127.0.0.1:$PORT/fleet/leases"
curl -s -o /dev/null -H "authorization: Bearer $TK" -H 'content-type: application/json' -XPOST -d '{"backend":"vz","name":"web1","apple":{"guest_os":"macos"}}' "http://127.0.0.1:$PORT/fleet/vms"
export NODE_PATH="${NODE_PATH:-$(npm root -g 2>/dev/null)}" UI_PROFILE="$D/chrome"
node scripts/e2e/console-ui.mjs "$URL" adm "$TK"
