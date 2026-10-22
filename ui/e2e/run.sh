#!/usr/bin/env bash
# Smoke-test the built UI against its mock API: ./run.sh node|fleet [--widths 390,1280]
# Needs node, npm and Google Chrome (playwright-core drives the installed Chrome; no browser download).
set -euo pipefail
cd "$(dirname "$0")"
APP="${1:?usage: run.sh node|fleet}"; shift || true
CACHE="${FLUXVM_E2E_CACHE:-${TMPDIR:-/tmp}/fluxvm-ui-e2e}"
PW_VERSION="${PW_VERSION:-1.60.0}"
if [[ ! -d "$CACHE/node_modules/playwright-core" ]]; then
  mkdir -p "$CACHE"; (cd "$CACHE" && npm init -y >/dev/null 2>&1 && npm install "playwright-core@$PW_VERSION" >/dev/null 2>&1)
fi
PORT="${PORT:-$((20000 + RANDOM % 20000))}"
PORT="$PORT" node "mock-$APP.mjs" >/dev/null & MOCK=$!
trap 'kill $MOCK 2>/dev/null || true' EXIT
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/console/" >/dev/null && break; sleep 0.1; done
PW_CORE="$CACHE/node_modules/playwright-core" node smoke.cjs "$APP" "http://127.0.0.1:$PORT" "$@"
