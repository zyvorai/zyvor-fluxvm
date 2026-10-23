#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: Studio's fleet VM tools against a real vz guest. A local `fluxctl serve` runs a guest with the vz runner, a
# `fluxvm-agent central` and a `fluxvm-agent node` put it behind central, and Chrome drives the VM's sheet (screen, snapshots,
# clone, command) through central. Every step is confirmed through the API, not from a toast.
#   scripts/vz-studio-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, node, curl, python3 and Google Chrome. Defaults to
# ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-studio.XXXXXX")"
NODE_PORT="${FLUXVM_LIVE_PORT:-7881}"; CENTRAL_PORT="${FLUXVM_LIVE_CENTRAL_PORT:-7882}"; TOKEN="live$RANDOM$RANDOM"
PIDS=()
cleanup() {
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T" "/tmp/fluxvm-run-studio"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; for f in daemon central agent; do [[ -f "$T/$f.log" ]] && { echo "-- $f.log"; tail -8 "$T/$f.log"; }; done; exit 1; }

cargo build -p fluxctl -p fluxvm-agent -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$NODE_PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-studio"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$NODE_PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$NODE_PORT/healthz" >/dev/null && ok "the node daemon is up" || bad "the node daemon did not start"

C="http://127.0.0.1:$CENTRAL_PORT"; AUTH="authorization: Bearer $TOKEN"
./target/debug/fluxvm-agent central --listen "127.0.0.1:$CENTRAL_PORT" --state-dir "$T/central" --token "$TOKEN" > "$T/central.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 40); do curl -fs "$C/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
./target/debug/fluxvm-agent node --name mac-live --central "$C" --fluxvm-url "http://127.0.0.1:$NODE_PORT" --token "$TOKEN" \
  --interval-secs 2 --state-dir "$T/agent" --no-overlay-sync > "$T/agent.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do curl -fs -H "$AUTH" "$C/fleet/nodes" | grep -q mac-live && break; sleep 1; done
curl -fs -H "$AUTH" "$C/fleet/nodes" | grep -q mac-live && ok "the node registered with central" || bad "the node did not register"

jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
V="$(curl -sS -X POST "http://127.0.0.1:$NODE_PORT/v1/vms" -H 'content-type: application/json' \
  -d '{"name":"studio-live","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}' | jget 'd["id"]')"
for _ in $(seq 1 30); do [[ "$(curl -fs "http://127.0.0.1:$NODE_PORT/v1/vms/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "http://127.0.0.1:$NODE_PORT/v1/vms/$V" | jget 'd["status"]')" == running ]] && ok "the vz guest $V is running" || bad "the guest is not running"
for _ in $(seq 1 20); do curl -fs -H "$AUTH" "$C/fleet/vms" | grep -q studio-live && break; sleep 1; done
curl -fs -H "$AUTH" "$C/fleet/vms" | grep -q studio-live && ok "central lists the guest" || bad "central does not list the guest"

CACHE="${FLUXVM_E2E_CACHE:-${TMPDIR:-/tmp}/fluxvm-ui-e2e}"; PW_VERSION="${PW_VERSION:-1.60.0}"
if [[ ! -d "$CACHE/node_modules/playwright-core" ]]; then
  mkdir -p "$CACHE"; (cd "$CACHE" && npm init -y >/dev/null 2>&1 && npm install "playwright-core@$PW_VERSION" >/dev/null 2>&1)
fi
PW_CORE="$CACHE/node_modules/playwright-core" STUDIO_TOKEN="$TOKEN" STUDIO_NODE="mac-live" STUDIO_VM="$V" STUDIO_NODE_URL="http://127.0.0.1:$NODE_PORT" \
  node ui/e2e/vzstudio.cjs "$C" || bad "the browser steps failed"
echo "vz studio: PASS"
