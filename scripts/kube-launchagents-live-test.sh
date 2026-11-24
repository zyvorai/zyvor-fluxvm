#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
# Live check of the macOS LaunchAgent features on a real Apple Silicon Mac with a running k3s cluster that has a worker
# (`fluxctl kube up --name NAME --workers 1`). Nothing here is run in CI. Each step prints PASS/FAIL; exit 1 on any FAIL.
#
#   ./scripts/kube-launchagents-live-test.sh [--name dev] [--fluxctl path/to/fluxctl]
#
# Needs an unlocked login session. The `list`/`stop` output format is assumed (first column is the VM id): check the first run by eye. Changes: installs and removes the kube agent and the worker-1 agent for NAME, and
# stops worker 1 once to see it come back. It does not touch other clusters.
set -uo pipefail
NAME=dev
FLUXCTL=fluxctl
while [ $# -gt 0 ]; do
  case "$1" in
    --name) NAME="$2"; shift 2 ;;
    --fluxctl) FLUXCTL="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
fails=0
ok()   { echo "PASS  $1"; }
bad()  { echo "FAIL  $1"; fails=$((fails+1)); }
check() { local what="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi; }
jq_() { python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get(sys.argv[1]))' "$1"; }
wait_for() { local n=$1; shift; for _ in $(seq "$n"); do "$@" >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }

W="${NAME}-w1"
running() { "$FLUXCTL" list --all 2>/dev/null | grep -E "(^|[[:space:]])$W([[:space:]]|$)" | grep -qi running; }

check "cluster API answers"                    "$FLUXCTL" kube status --name "$NAME"
check "worker $W is running"                   running

echo "-- kube agent (Phase 1)"
check "kube agent install"                     "$FLUXCTL" kube agent install --name "$NAME"
[ "$("$FLUXCTL" kube agent status | jq_ loaded)" = "True" ] && ok "kube agent loaded" || bad "kube agent loaded"
plist="$HOME/Library/LaunchAgents/dev.zyvor.fluxvm.kube.plist"
grep -q "/opt/" "$plist" 2>/dev/null || grep -q "fluxctl" "$plist" && ok "plist names a stable fluxctl path (brew upgrade keeps opt/)" || bad "plist path"
check "doctor sees the agent loaded"           "$FLUXCTL" doctor
check "kube agent uninstall"                   "$FLUXCTL" kube agent uninstall

echo "-- helper agents (Phase 2)"
for h in vmnet usb; do
  s=$("$FLUXCTL" helper-agent "$h" status 2>&1) && echo "$s" | grep -q signature && ok "helper-agent $h status reports the signature" || bad "helper-agent $h status"
done

echo "-- per-node agent (Phase 3): stopping $W must bring it back, with no cluster supervisor"
check "node agent install"                     "$FLUXCTL" kube agent node install --name "$NAME" --worker 1
id=$("$FLUXCTL" list --all 2>/dev/null | awk -v w="$W" '$0 ~ w {print $1; exit}')
[ -n "$id" ] && "$FLUXCTL" stop "$id" >/dev/null 2>&1
if wait_for 30 running; then ok "$W restarted by its own agent"; else bad "$W did not restart within 60 s"; fi
echo "-- stands down beside the supervisor"
"$FLUXCTL" kube supervise --name "$NAME" >/dev/null 2>&1 & sup=$!
sleep 5
id=$("$FLUXCTL" list --all 2>/dev/null | awk -v w="$W" '$0 ~ w {print $1; exit}')
[ -n "$id" ] && "$FLUXCTL" stop "$id" >/dev/null 2>&1
sleep 20
grep -q "supervisor is running" "$HOME/Library/Logs/fluxvm-kube-$NAME-w1.log" && ok "node agent logged that it stood down" || bad "node agent did not log standing down"
kill "$sup" 2>/dev/null; wait "$sup" 2>/dev/null
check "node agent uninstall"                   "$FLUXCTL" kube agent node uninstall --name "$NAME" --worker 1
wait_for 30 running || "$FLUXCTL" kube up --name "$NAME" --workers 1 >/dev/null 2>&1

echo; [ "$fails" -eq 0 ] && echo "all checks passed" || { echo "$fails check(s) failed"; exit 1; }
