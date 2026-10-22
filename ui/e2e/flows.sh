#!/usr/bin/env bash
# Drive the fleet UI's forms (join token, lease, billing) against a real central and check the effect through its API.
#   ./flows.sh                                  start a central and one simulated Mac from BIN (default ../../target/debug/fluxvm-agent)
#   FLOWS_URL=http://127.0.0.1:7799 FLOWS_TOKEN=... ./flows.sh   use a central that is already running (it gets a tenant, a token and a lease)
# Needs node, npm, curl and Google Chrome (playwright-core drives the installed Chrome; no browser download).
set -euo pipefail
cd "$(dirname "$0")"
CACHE="${FLUXVM_E2E_CACHE:-${TMPDIR:-/tmp}/fluxvm-ui-e2e}"
PW_VERSION="${PW_VERSION:-1.60.0}"
if [[ ! -d "$CACHE/node_modules/playwright-core" ]]; then
  mkdir -p "$CACHE"; (cd "$CACHE" && npm init -y >/dev/null 2>&1 && npm install "playwright-core@$PW_VERSION" >/dev/null 2>&1)
fi
PIDS=(); T=""
cleanup() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done; [[ -n "$T" ]] && rm -rf "$T"; return 0; }
trap cleanup EXIT
if [[ -z "${FLOWS_URL:-}" ]]; then
  BIN="${BIN:-../../target/debug/fluxvm-agent}"
  [[ -x "$BIN" ]] || { echo "need $BIN (cargo build -p fluxvm-agent) or FLOWS_URL" >&2; exit 2; }
  T="$(mktemp -d)"; PORT="${PORT:-$((20000 + RANDOM % 20000))}"; export FLOWS_TOKEN="flows$RANDOM$RANDOM"
  "$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$T/state" --token "$FLOWS_TOKEN" >"$T/central.log" 2>&1 & PIDS+=($!)
  for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.2; done
  "$BIN" sim-node --name mac-e2e --central "http://127.0.0.1:$PORT" --token "$FLOWS_TOKEN" --interval-secs 2 >"$T/node.log" 2>&1 & PIDS+=($!)
  for _ in $(seq 50); do curl -sf -H "authorization: Bearer $FLOWS_TOKEN" "http://127.0.0.1:$PORT/fleet/nodes" | grep -q mac-e2e && break; sleep 0.2; done
  FLOWS_URL="http://127.0.0.1:$PORT"
fi
PW_CORE="$CACHE/node_modules/playwright-core" node flows.cjs "$FLOWS_URL"
