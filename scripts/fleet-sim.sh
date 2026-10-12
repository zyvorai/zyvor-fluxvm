#!/usr/bin/env bash
# Run a whole Mac cloud on one machine, with no Macs: a central registry and three simulated Macs, then a tenant
# that leases a Mac, starts two macOS VMs, runs a command in one, and creates a private network.
#
#   scripts/fleet-sim.sh              # build, start, run the demo, keep running until Ctrl-C
#   FLUXVM_SIM_PORT=7800 scripts/fleet-sim.sh
#
# Needs cargo, curl and python3. See docs/fleet-quickstart.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT="${FLUXVM_SIM_PORT:-7799}"
CENTRAL="http://127.0.0.1:${PORT}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-sim.XXXXXX")"
ADMIN_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  echo "Stopped. Logs and state were in $WORK"
}
trap cleanup EXIT INT TERM

echo "==> Building fluxvm-agent"
(cd "$ROOT" && cargo build -q -p fluxvm-agent)
AGENT="${CARGO_TARGET_DIR:-$ROOT/target}/debug/fluxvm-agent"

echo "==> Starting central on $CENTRAL (state in $WORK)"
RUST_LOG="${RUST_LOG:-info}" "$AGENT" central --listen "127.0.0.1:${PORT}" --state-dir "$WORK/central" \
  --token "$ADMIN_TOKEN" --reconcile-secs 15 >"$WORK/central.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
  curl -fsS "$CENTRAL/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done
curl -fsS "$CENTRAL/healthz" >/dev/null || { echo "central did not start; see $WORK/central.log" >&2; exit 1; }

# name vcpus memory-GiB extra-flags
start_mac() {
  local name="$1" vcpus="$2" mem="$3"
  shift 3
  "$AGENT" sim-node --name "$name" --central "$CENTRAL" --token "$ADMIN_TOKEN" \
    --vcpus "$vcpus" --memory-gib "$mem" --interval-secs 2 \
    --overlay-config "$WORK/$name/overlay.json" "$@" >"$WORK/$name.log" 2>&1 &
  PIDS+=($!)
}
echo "==> Starting three simulated Macs"
start_mac studio-1 12 32 --label zone=lab
start_mac studio-2 24 64 --nested --label zone=lab
start_mac mini-1 8 16 --label zone=desk

api() { # method path token [json]
  local method="$1" path="$2" token="$3" body="${4:-}"
  if [ -n "$body" ]; then
    curl -sS -X "$method" "$CENTRAL$path" -H "Authorization: Bearer $token" \
      -H 'content-type: application/json' -d "$body"
  else
    curl -sS -X "$method" "$CENTRAL$path" -H "Authorization: Bearer $token"
  fi
}
field() { # json-path on stdin, e.g. field lease.node
  python3 -c 'import json,sys
v=json.load(sys.stdin)
for k in sys.argv[1].split("."):
    v=v[int(k)] if isinstance(v,list) else v.get(k) if isinstance(v,dict) else None
print("" if v is None else v)' "$1"
}

for _ in $(seq 1 30); do
  n="$(api GET /fleet/nodes "$ADMIN_TOKEN" | python3 -c 'import json,sys
print(len(json.load(sys.stdin).get("items", [])))')"
  [ "$n" -ge 3 ] && break
  sleep 0.5
done
echo "    $n Macs registered"

echo "==> Operator: create tenant 'acme' (quota: 2 leased Macs, 4 VMs)"
TENANT_TOKEN="$(api POST /fleet/tenants "$ADMIN_TOKEN" \
  '{"id":"acme","quota":{"max_leased_hosts":2,"max_vms":4}}' | field token)"
[ -n "$TENANT_TOKEN" ] || { echo "tenant creation failed" >&2; exit 1; }

echo "==> Tenant: lease a Mac (24 h minimum, Apple's licence)"
LEASED="$(api POST /fleet/leases "$TENANT_TOKEN" '{}' | field lease.node)"
echo "    leased $LEASED"

echo "==> Tenant: start two macOS VMs (Apple allows two per Mac)"
for name in build-1 build-2; do
  out="$(api POST /fleet/vms "$TENANT_TOKEN" \
    "{\"backend\":\"vz\",\"name\":\"$name\",\"vcpus\":4,\"memory_mib\":8192,\"apple\":{\"guest_os\":\"macos\"}}")"
  echo "    $name: $(echo "$out" | field vm.id) on $(echo "$out" | field node)"
  [ "$name" = build-1 ] && VM1="$(echo "$out" | field vm.id)"
done

echo "==> Tenant: a third macOS VM is refused"
api POST /fleet/vms "$TENANT_TOKEN" '{"backend":"vz","name":"build-3","apple":{"guest_os":"macos"}}' | head -c 200
echo

echo "==> Tenant: run a command in build-1 through central"
api POST "/fleet/vms/$LEASED/$VM1/exec" "$TENANT_TOKEN" '{"command":"sw_vers"}' | field stdout

echo "==> Tenant: create a private network across its Macs"
api POST /fleet/overlays "$TENANT_TOKEN" '{"name":"acme-net"}' | field subnet
sleep 3
echo "    $LEASED's overlay.json: $(cat "$WORK/$LEASED/overlay.json" 2>/dev/null | head -c 160)..."

cat <<EOF

The simulated cloud is running.
  Console:        $CENTRAL/console
  Operator token: $ADMIN_TOKEN
  Tenant token:   $TENANT_TOKEN   (tenant 'acme')
  Logs:           $WORK/*.log

Try:  curl -s -H "Authorization: Bearer $TENANT_TOKEN" $CENTRAL/fleet/tenants/me
      curl -s -H "Authorization: Bearer $ADMIN_TOKEN" "$CENTRAL/fleet/usage?format=csv"

Ctrl-C stops everything.
EOF
wait
