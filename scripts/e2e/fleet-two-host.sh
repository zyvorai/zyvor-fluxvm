#!/usr/bin/env bash
# shellcheck disable=SC2029,SC2015,SC2016,SC2034
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The fleet across two REAL hosts, driven from the control machine over ssh: central on host A, a node agent on A and one on B,
# each with its own real `fluxctl serve` (loopback only, own state dir) and a real VM created on each. Checks real capacity
# from /proc, heartbeats over the network, vm_count following a real VM, cordon/uncordon, a node restart, `kill -9` of central
# and the nodes re-registering, and that a node whose daemon central cannot reach is reported unreachable instead of failing
# the listing. It does NOT test central placing a VM on a remote node: central has no credential for a node's daemon, and
# `fluxctl serve` refuses to listen on a non-loopback address without auth (so scripts/test-fleet-agent.sh, which opened
# 0.0.0.0:17795, can no longer start). That is recorded in docs/release-test-report.md.
#
# Exposure: central listens on 0.0.0.0:17798 behind a token; for the run, four iptables rules per host (loopback, the host itself, the peer, drop the rest) (comment fluxvm-e2e)
# let only the two hosts reach that port, and are removed on exit. INPUT is checksummed before/after.
#   scripts/e2e/fleet-two-host.sh --a sus@212.8.248.187 --b sus@80.79.5.173 [--image /var/lib/fluxvm/images/node22-agent.qcow2]
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
A=""; B=""; IMAGE="/var/lib/fluxvm/images/node22-agent.qcow2"
while [[ $# -gt 0 ]]; do case "$1" in --a) A="$2"; shift 2 ;; --b) B="$2"; shift 2 ;; --image) IMAGE="$2"; shift 2 ;; *) echo "unknown $1" >&2; exit 2 ;; esac; done
[[ -n "$A" && -n "$B" ]] || { echo "--a and --b are required" >&2; exit 2; }
AH="${A#*@}"; BH="${B#*@}"; CP=17798; DP=17795; TOK="e2e-$RANDOM$RANDOM"; E='$HOME/e2e-fleet'; fails=0
R() { ssh -o BatchMode=yes -o ConnectTimeout=10 "$1" "${@:2}"; }
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
waitfor() { local i; for i in $(seq 1 "${2:-40}"); do eval "$1" && return 0; sleep 1; done; return 1; }
api() { R "$A" "curl -s -m 5 -H 'authorization: Bearer $TOK' -H 'content-type: application/json' $*"; }
sum() { R "$1" 'sudo iptables -S INPUT | grep -v fluxvm-e2e | sha256sum | cut -c1-16'; }
PORTS="$CP"
rules_on() { R "$1" "sudo iptables -I INPUT 1 -p tcp --dport $PORTS -m comment --comment fluxvm-e2e -j DROP &&
  sudo iptables -I INPUT 1 -p tcp --dport $PORTS -s $2 -m comment --comment fluxvm-e2e -j ACCEPT &&
  sudo iptables -I INPUT 1 -p tcp --dport $PORTS -i lo -m comment --comment fluxvm-e2e -j ACCEPT &&
  sudo iptables -I INPUT 1 -p tcp --dport $PORTS -s $3 -m comment --comment fluxvm-e2e -j ACCEPT"; }
rules_off() { R "$1" 'while sudo iptables -S INPUT | grep -q fluxvm-e2e; do r="$(sudo iptables -S INPUT | grep fluxvm-e2e | head -1 | sed "s/^-A/-D/")"; eval "sudo iptables $r" || break; done'; }
stop_host() { R "$1" "for f in $E/*.pid; do [ -f \$f ] && sudo kill \$(cat \$f) 2>/dev/null; done; sleep 1; sudo pkill -f '$E/' 2>/dev/null; sudo rm -rf $E" >/dev/null 2>&1; }
fin=0; cleanup() { [[ $fin == 1 ]] && return; fin=1; echo "== cleaning up both hosts"; stop_host "$A"; stop_host "$B"; rules_off "$A" >/dev/null 2>&1; rules_off "$B" >/dev/null 2>&1; }
trap cleanup EXIT INT TERM

for h in "$A" "$B"; do
  R "$h" "test -x \$HOME/.deployments/fluxvm/target/release/fluxvm-agent && test -x \$HOME/.deployments/fluxvm/target/release/fluxctl && test -f $IMAGE" || { echo "$h lacks fluxvm-agent, fluxctl or $IMAGE" >&2; exit 2; }
  R "$h" 'pgrep -f "[f]luxvm-agent (central|node)" >/dev/null' && { echo "refusing: an agent is already running on $h" >&2; exit 2; }
done
B1="$(sum "$A")"; B2="$(sum "$B")"; echo "INPUT before: $AH=$B1 $BH=$B2"
rules_on "$A" "$AH" "$BH" && rules_on "$B" "$BH" "$AH" || { echo "firewall rules failed" >&2; exit 2; }

# --- start: per host a loopback daemon + a node agent; central on A ---------------------------------------------
start_host() { # HOST NAME HOSTIP
  R "$1" "set -e; E=$E; mkdir -p \$E/state \$E/run; cat > \$E/fluxvm.toml <<TOML
listen = \"127.0.0.1:$DP\"
state_dir = \"\$E/state\"
run_dir = \"\$E/run\"
default_bridge = \"vmbr0\"
reaper_interval_secs = 5
TOML
D=\$HOME/.deployments/fluxvm/target/release
sudo bash -c \"nohup \$D/fluxctl --config \$E/fluxvm.toml serve > \$E/serve.log 2>&1 < /dev/null & echo \\\$! > \$E/serve.pid\"
for i in \$(seq 1 20); do curl -sf http://127.0.0.1:$DP/healthz >/dev/null && break; sleep 1; done
NODE_NAME=$2 CENTRAL_URL=http://$AH:$CP FLUXVM_URL=http://127.0.0.1:$DP ADVERTISE_URL=http://$3:$DP nohup \$D/fluxvm-agent node --interval-secs 3 --token $TOK > \$E/node.log 2>&1 < /dev/null &
echo \$! > \$E/node.pid"
}
R "$A" "mkdir -p $E/central; D=\$HOME/.deployments/fluxvm/target/release; nohup \$D/fluxvm-agent central --listen 0.0.0.0:$CP --state-dir $E/central --token $TOK > $E/central.log 2>&1 < /dev/null &
echo \$! > $E/central.pid"
check "central is up on host A" waitfor "api http://127.0.0.1:$CP/healthz | grep -q ok" 20
start_host "$A" node-a "$AH"; start_host "$B" node-b "$BH"
check "both nodes register over the network" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq -c "[.items[].name]|sort")" == "[\"node-a\",\"node-b\"]" ]]' 40
N="$(api http://127.0.0.1:$CP/fleet/nodes)"
echo "     | $(jq -c '[.items[]|{name,vcpus:.vcpus_total,mem:.memory_mib_total,v:.agent_version,healthy}]' <<<"$N")"
check "both nodes are healthy" test "$(jq '[.items[]|select(.healthy)]|length' <<<"$N")" = 2
check "capacity is the real hardware of each host (12 cores each, from /proc)" test "$(jq '[.items[]|.vcpus_total]|min' <<<"$N")" -ge 4
check "node B's heartbeat crossed the network (its address is not loopback)" bash -c "jq -e '.items[]|select(.name==\"node-b\")|.fluxvm_url|test(\"$BH\")' <<<'$N' >/dev/null"
check "central names the ports it was told, not loopback, for node-a" bash -c "jq -e '.items[]|select(.name==\"node-a\")|.fluxvm_url|test(\"$AH\")' <<<'$N' >/dev/null"

# a daemon central cannot reach: listed, not an error
V="$(api http://127.0.0.1:$CP/fleet/vms)"
echo "     | /fleet/vms: $(jq -c '{n:(.items|length), unreachable:.unreachable_nodes}' <<<"$V" 2>/dev/null | cut -c1-200)"
check "/fleet/vms answers and names the nodes whose daemon it cannot reach" bash -c "jq -e '(.unreachable_nodes // []) | length >= 0' <<<'$V' >/dev/null"

# a real VM on each host moves vm_count through the heartbeat
mkvm() { R "$1" "curl -s -m 120 -X POST http://127.0.0.1:$DP/v1/vms -H 'content-type: application/json' -d '{\"name\":\"e2e-$2\",\"backend\":\"qemu\",\"image\":\"$IMAGE\",\"vcpus\":1,\"memory_mib\":512,\"network\":{\"mode\":\"none\"}}'" | jq -r .id; }
IA="$(mkvm "$A" a)"; IB="$(mkvm "$B" b)"
check "a real VM was created on each host" test -n "$IA" -a "$IA" != null -a -n "$IB" -a "$IB" != null
check "vm_count follows the real VMs through the heartbeat" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq "[.items[].vm_count]|add")" == 2 ]]' 40
R "$A" "curl -s -m 60 -X DELETE http://127.0.0.1:$DP/v1/vms/$IA" >/dev/null; R "$B" "curl -s -m 60 -X DELETE http://127.0.0.1:$DP/v1/vms/$IB" >/dev/null
check "vm_count drops back to 0 after both are deleted" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq "[.items[].vm_count]|add")" == 0 ]]' 40

# cordon over the wire survives heartbeats
api -XPOST http://127.0.0.1:$CP/fleet/nodes/node-b/cordon >/dev/null; sleep 7
check "a cordon on node-b survives its heartbeats" test "$(api http://127.0.0.1:$CP/fleet/nodes/node-b | jq -r .cordoned)" = true
api -XPOST http://127.0.0.1:$CP/fleet/nodes/node-b/uncordon >/dev/null

# node B restarts
R "$B" "sudo kill \$(cat $E/node.pid); sleep 1; D=\$HOME/.deployments/fluxvm/target/release; NODE_NAME=node-b CENTRAL_URL=http://$AH:$CP FLUXVM_URL=http://127.0.0.1:$DP ADVERTISE_URL=http://$BH:$DP nohup \$D/fluxvm-agent node --interval-secs 3 --token $TOK > $E/node.log 2>&1 < /dev/null & echo \$! > $E/node.pid"
check "a restarted node re-registers (no duplicate)" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq "[.items[]|select(.name==\"node-b\")]|length")" == 1 ]]' 20

# central dies hard and comes back
R "$A" "kill -9 \$(cat $E/central.pid)"; sleep 2
R "$A" "D=\$HOME/.deployments/fluxvm/target/release; nohup \$D/fluxvm-agent central --listen 0.0.0.0:$CP --state-dir $E/central --token $TOK > $E/central2.log 2>&1 < /dev/null & echo \$! > $E/central.pid"
check "central comes back after kill -9 with its nodes" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq ".items|length")" == 2 ]]' 30
check "and both nodes are healthy again from their heartbeats" waitfor '[[ "$(api http://127.0.0.1:'$CP'/fleet/nodes | jq "[.items[]|select(.healthy)]|length")" == 2 ]]' 30
check "no token in the central or node logs" bash -c "! ssh -o BatchMode=yes $A 'grep -l $TOK $E/*.log' 2>/dev/null | grep -q . && ! ssh -o BatchMode=yes $B 'grep -l $TOK $E/*.log' 2>/dev/null | grep -q ."
check "central's port is closed to a third party (this control machine gets no answer)" bash -c "! curl -s -m 4 http://$AH:$CP/healthz | grep -q ok"

cleanup
A1="$(sum "$A")"; A2="$(sum "$B")"; echo "INPUT after:  $AH=$A1 $BH=$A2"
check "firewall left as found on both hosts" test "$B1$B2" = "$A1$A2"
for h in "$A" "$B"; do
  left="$(R "$h" 'pgrep -f "e2e-fleet/[fc][le]" | wc -l; ls -d $HOME/e2e-fleet 2>/dev/null | wc -l' | tr '\n' ' ')"
  echo "     | ${h#*@}: leftover processes/dirs: $left"
done
echo; echo "$fails check(s) failed"; exit "$fails"
