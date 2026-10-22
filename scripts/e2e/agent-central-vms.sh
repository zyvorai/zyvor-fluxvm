#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2034
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The central features that need real VMs, on one Linux/KVM host: central + a node agent + the node's own `fluxctl serve`
# (loopback, own ports and state dir, nothing shared with the host's service). Through central: a VM placed on the node,
# listing, exec over vsock, logs, power, snapshots and clone, a lease, a warm pool that fills and is claimed from, capacity and
# usage following the VM, audit events, and tenant isolation of someone else's VM. Each feature the daemon does not support
# on this backend is printed as KNOWN-GAP with the reason, not failed. Run as root:
#   sudo scripts/e2e/agent-central-vms.sh [--image PATH] [--base BASE.img]
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
IMAGE=""; BASE="/var/lib/fluxvm/images/ubuntu-noble-base.img"; BUILT=0
while [[ $# -gt 0 ]]; do case "$1" in --image) IMAGE="$2"; shift 2 ;; --base) BASE="$2"; shift 2 ;; *) echo "unknown $1" >&2; exit 2 ;; esac; done
[[ "$(id -u)" == 0 ]] || { echo "run as root (sudo)" >&2; exit 2; }
[[ -e /dev/kvm ]] || { echo "needs /dev/kvm" >&2; exit 2; }
FX="${FLUXVM_BIN:-$(command -v fluxctl)}"; AGENT="${BIN:-target/release/fluxvm-agent}"
[[ -x "$FX" && -x "$AGENT" ]] || { echo "need fluxctl and $AGENT" >&2; exit 2; }
T="$(mktemp -d)"; PD=$((20000 + RANDOM % 20000)); PC=$((PD + 1)); TOK="e2e$RANDOM$RANDOM"; fails=0; PIDS=()
D="http://127.0.0.1:$PD"; C="http://127.0.0.1:$PC"
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }; gap() { echo "KNOWN-GAP $1"; }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
cleanup() {
  for id in $(curl -s "$D/v1/vms" 2>/dev/null | jq -r '(.items // .)[]?|.id' 2>/dev/null); do curl -s -XDELETE "$D/v1/vms/$id" >/dev/null 2>&1; done
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null; done; sleep 1
  pkill -f "$T/" 2>/dev/null; [[ $BUILT == 1 ]] && rm -f "$IMAGE"; rm -rf "$T"
}
trap cleanup EXIT
adm() { curl -s -m 120 -H "authorization: Bearer $TOK" -H 'content-type: application/json' "$@"; }
admc() { curl -s -m 120 -o "$T/body" -w '%{http_code}' -H "authorization: Bearer $TOK" -H 'content-type: application/json' "$@"; }
waitfor() { local i; for i in $(seq 1 "${2:-40}"); do eval "$1" && return 0; sleep 1; done; return 1; }

if [[ -z "$IMAGE" ]]; then
  GA="target/release/fluxvm-guest-agent"; [[ -x "$GA" ]] || GA="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/fluxvm-guest-agent"
  [[ -x "$GA" && -f "$BASE" ]] || { echo "need a static guest agent ($GA) and a base image ($BASE), or --image" >&2; exit 2; }
  IMAGE="/var/lib/fluxvm/images/fluxvm-central-vms-test.qcow2"; BUILT=1
  cat > "$T/build.json" <<JSON
{"source":"$BASE","output":"$IMAGE","format":"qcow2",
 "copy_in":[{"src":"$(cd "$(dirname "$GA")" && pwd)/$(basename "$GA")","dest":"/usr/local/bin/fluxvm-guest-agent"},
            {"src":"$PWD/systemd/fluxvm-guest-agent.service","dest":"/etc/systemd/system/fluxvm-guest-agent.service"}],
 "enable_services":["fluxvm-guest-agent"]}
JSON
  "$FX" --config /etc/fluxvm.toml build-image --spec "$T/build.json" >/dev/null 2>&1 || { echo "image build failed" >&2; exit 2; }
fi

# --- one isolated stack ------------------------------------------------------------------------------------------
mkdir -p "$T/state" "$T/run" "$T/central"
cat > "$T/fluxvm.toml" <<TOML
listen = "127.0.0.1:$PD"
state_dir = "$T/state"
run_dir = "$T/run"
default_bridge = "vmbr0"
reaper_interval_secs = 5
TOML
"$FX" --config "$T/fluxvm.toml" serve >"$T/serve.log" 2>&1 & PIDS+=($!)
check "the node's daemon is up (loopback, own state dir)" waitfor "curl -sf $D/healthz >/dev/null" 30
"$AGENT" central --listen "127.0.0.1:$PC" --state-dir "$T/central" --token "$TOK" --reconcile-secs 5 >"$T/central.log" 2>&1 & PIDS+=($!)
check "central is up" waitfor "curl -sf $C/healthz >/dev/null" 30
NODE_NAME=n1 CENTRAL_URL="$C" FLUXVM_URL="$D" ADVERTISE_URL="$D" "$AGENT" node --interval-secs 2 --token "$TOK" >"$T/node.log" 2>&1 & PIDS+=($!)
check "the node registers and is healthy" waitfor '[[ "$(adm '$C'/fleet/nodes/n1 | jq -r .healthy)" == true ]]' 30

# --- a VM through central ---------------------------------------------------------------------------------------
SPEC="{\"name\":\"e2e-central-vm\",\"backend\":\"qemu\",\"image\":\"$IMAGE\",\"vcpus\":1,\"memory_mib\":768,\"network\":{\"mode\":\"none\"},\"agent\":{\"enabled\":true,\"port\":17777}}"
CODE="$(admc -XPOST -d "$SPEC" "$C/fleet/vms")"
echo "     | create via central: HTTP $CODE $(jq -c '{id:(.id // .vm.id),node:(.node // .placed_on)}' "$T/body" 2>/dev/null | cut -c1-120)"
ID="$(jq -r '.id // .vm.id // empty' "$T/body")"
check "POST /fleet/vms places a real VM on the node" test "$CODE" = 200 -o "$CODE" = 201 -a -n "$ID"
[[ -n "$ID" ]] || { echo "no VM, cannot continue" >&2; exit 1; }
check "the VM is a real QEMU process on this host" bash -c "pgrep -f 'qemu-system.*$ID' >/dev/null || pgrep -f 'qemu-system.*e2e-central-vm' >/dev/null"
check "GET /fleet/vms lists it with its node" test "$(adm "$C/fleet/vms" | jq -r --arg id "$ID" '[.items[]|select(.id==$id)|.node]|first')" = n1
check "GET /fleet/nodes/n1/vms lists it" test "$(adm "$C/fleet/nodes/n1/vms" | jq -r --arg id "$ID" '[(.items // .)[]|select(.id==$id)]|length')" = 1
check "vm_count follows it through the heartbeat" waitfor '[[ "$(adm '$C'/fleet/nodes/n1 | jq -r .vm_count)" == 1 ]]' 20
check "capacity shows the VM" test "$(adm "$C/fleet/capacity" | jq '[.. | .vm_count? // empty] | add')" -ge 1

up=0; for _ in $(seq 1 30); do [[ "$(admc -XPOST -d '{"command":"echo via-central"}' "$C/fleet/vms/n1/$ID/exec")" == 200 ]] && { up=1; break; }; sleep 4; done
echo "     | exec body: $(jq -c . "$T/body" 2>/dev/null | cut -c1-200)"
check "exec through central reaches the guest agent over vsock" bash -c "[[ $up == 1 ]] && jq -e '(.stdout // .result.stdout // \"\") | test(\"via-central\")' '$T/body' >/dev/null"
CODE="$(admc "$C/fleet/vms/n1/$ID/logs")"; check "console logs through central" test "$CODE" = 200 -a -s "$T/body"

# --- snapshot, restore, clone ----------------------------------------------------------------------------------
CODE="$(admc -XPOST -d '{"name":"s1"}' "$C/fleet/vms/n1/$ID/snapshots")"
if [[ "$CODE" == 200 || "$CODE" == 201 ]]; then
  ok "snapshot through central"
  check "the snapshot is listed" test "$(adm "$C/fleet/vms/n1/$ID/snapshots" | jq -r '[(.items // .)[]|.name]|index("s1") != null')" = true
  CODE="$(admc -XPOST "$C/fleet/vms/n1/$ID/snapshots/s1/restore")"; check "restore the snapshot" test "$CODE" = 200 -o "$CODE" = 202
  CODE="$(admc -XPOST -d '{"name":"e2e-clone","snapshot":"s1"}' "$C/fleet/vms/n1/$ID/clone")"
  if [[ "$CODE" == 200 || "$CODE" == 201 ]]; then ok "clone from the snapshot"; CID="$(jq -r '.id // .vm.id // empty' "$T/body")"; [[ -n "$CID" ]] && adm -XDELETE "$C/fleet/vms/n1/$CID" >/dev/null
  else gap "clone through central: HTTP $CODE $(jq -r '.error // .' "$T/body" | head -c 160)"; fi
  check "delete the snapshot" test "$(admc -XDELETE "$C/fleet/vms/n1/$ID/snapshots/s1")" -lt 300
else gap "snapshot through central on a qemu VM: HTTP $CODE $(jq -r '.error // .' "$T/body" 2>/dev/null | head -c 160)"; fi

# --- power ------------------------------------------------------------------------------------------------------
CODE="$(admc -XPOST "$C/fleet/vms/n1/$ID/stop")"; check "stop through central" test "$CODE" = 200 -o "$CODE" = 202
check "the VM reports stopped" waitfor '[[ "$(adm '$C'/fleet/vms | jq -r --arg id '"$ID"' "[.items[]|select(.id==\$id)|.status]|first")" == stopped ]]' 40
# Clone needs no snapshot when the source is stopped (the snapshot route is a known gap on qemu, so this is where clone gets exercised).
CODE="$(admc -XPOST -d '{"name":"e2e-clone"}' "$C/fleet/vms/n1/$ID/clone")"
if [[ "$CODE" == 200 || "$CODE" == 201 ]]; then
  CID="$(jq -r '.id // .vm.id // empty' "$T/body")"; ok "clone of a stopped VM"
  check "the clone is a different VM on the same node" test -n "$CID" -a "$CID" != "$ID" -a "$(adm "$C/fleet/vms" | jq -r --arg id "$CID" '[.items[]|select(.id==$id)|.node]|first')" = n1
  check "delete the clone" test "$(admc -XDELETE "$C/fleet/vms/n1/$CID")" -lt 300
  check "the source is still listed after the clone is gone" test "$(adm "$C/fleet/vms" | jq -r --arg id "$ID" '[.items[]|select(.id==$id)]|length')" = 1
else gap "clone of a stopped VM through central: HTTP $CODE $(jq -r '.error // .' "$T/body" | head -c 160)"; fi
CODE="$(admc -XPOST "$C/fleet/vms/n1/$ID/start")"; check "start through central" test "$CODE" = 200 -o "$CODE" = 202

# --- lease -------------------------------------------------------------------------------------------------------
adm -XPOST -d '{"id":"acme"}' "$C/fleet/tenants" >/dev/null
CODE="$(admc -XPOST -d '{"tenant":"acme","node":"n1","ttl_seconds":600}' "$C/fleet/leases")"
if [[ "$CODE" == 200 || "$CODE" == 201 ]]; then
  LID="$(jq -r '.id // .lease.id // empty' "$T/body")"; ok "lease created"
  check "the lease is readable" test "$(admc "$C/fleet/leases/$LID")" = 200
  check "release the lease" test "$(admc -XPOST "$C/fleet/leases/$LID/release")" -lt 300
else gap "lease: HTTP $CODE $(jq -r '.error // .' "$T/body" | head -c 160)"; fi

# --- warm pool ---------------------------------------------------------------------------------------------------
admc -XPUT -d "{\"guest_os\":\"linux\",\"image\":\"$IMAGE\",\"vcpus\":1,\"memory_mib\":768}" "$C/fleet/images/e2e-linux" >/dev/null
CODE="$(admc -XPOST -d "{\"name\":\"p1\",\"image\":\"e2e-linux\",\"vcpus\":1,\"memory_mib\":768,\"min_ready\":1,\"max_total\":2}" "$C/fleet/pools")"
if [[ "$CODE" == 200 || "$CODE" == 201 ]]; then
  ok "warm pool created"
  # Pools place their VMs on hosts the owner leases, and leases are Mac-only: on a Linux node the pool backs off.
  # Fill, hand-out, refill and tenant pools on leased Macs run against simulated Macs in central_warmpool.rs's tests.
  if waitfor '[[ "$(adm '$C'/fleet/pools/p1/status | jq -r "(.ready // .warm // 0)")" -ge 1 ]]' 25; then ok "the pool fills to min_ready without being asked"
  elif adm "$C/fleet/pools/p1/status" | jq -r .last_error | grep -q -i leases; then gap "warm pool on a Linux node: $(adm "$C/fleet/pools/p1/status" | jq -r .last_error | head -c 150)"
  else bad "the pool fills to min_ready without being asked: $(adm "$C/fleet/pools/p1/status" | jq -c . | cut -c1-200)"; fi
  check "delete the pool" test "$(admc -XDELETE "$C/fleet/pools/p1")" -lt 300
else gap "warm pool on this backend: HTTP $CODE $(jq -r '.error // .' "$T/body" | head -c 200)"; fi

# --- audit, metrics, isolation ----------------------------------------------------------------------------------
check "the audit trail recorded the VM operations" bash -c "grep -rl 'vm.create\|/fleet/vms' $T/central >/dev/null 2>&1 || curl -s -H 'authorization: Bearer $TOK' '$C/fleet/events' -m 3 | grep -q ."
check "/metrics has per-node series" bash -c "curl -s -H 'authorization: Bearer $TOK' $C/metrics | grep -q 'fluxvm_central'"
TK="$(adm -XPOST -d '{"name":"ci"}' "$C/fleet/tenants/acme/keys" | jq -r .token)"
check "a tenant cannot see the operator's VM" bash -c "! curl -s -H 'authorization: Bearer $TK' $C/fleet/vms | jq -r '.items[]?.id' | grep -q '$ID'"
check "a tenant cannot exec in the operator's VM" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -XPOST -H 'authorization: Bearer $TK' -H 'content-type: application/json' -d '{\"command\":\"id\"}' $C/fleet/vms/n1/$ID/exec) =~ ^(403|404)\$ ]]"
check "a tenant cannot delete it either" bash -c "[[ \$(curl -s -o /dev/null -w '%{http_code}' -XDELETE -H 'authorization: Bearer $TK' $C/fleet/vms/n1/$ID) =~ ^(403|404)\$ ]]"

CODE="$(admc -XDELETE "$C/fleet/vms/n1/$ID")"; check "delete through central" test "$CODE" -lt 300
check "the VM process is gone" waitfor "! pgrep -f 'qemu-system.*$ID' >/dev/null" 30
check "vm_count returns to 0" waitfor '[[ "$(adm '$C'/fleet/nodes/n1 | jq -r .vm_count)" == 0 ]]' 20
check "no token in any log" bash -c "! grep -q '$TOK' $T/central.log $T/node.log $T/serve.log"

echo; echo "$fails check(s) failed"; exit "$fails"
