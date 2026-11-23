#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

# Read-only check of the FluxVM + Kairon + Veyron stack on one Mac, layer by layer, each failure with its fix. Changes nothing.
#
#   scripts/stack-check.sh [--kubeconfig FILE]
#
# Layers: 1 the FluxVM daemon, 2 an unlocked login session, 3 a Kubernetes API, 4 the Kairon CRDs, 5 this Mac registered as a
# Node, 6 kairon-node's own health, 7 the Veyron server. A layer whose prerequisite failed is skipped, not failed.
# Environment: FLUXVM_URL (default http://127.0.0.1:7788), KAIRON_NODE_HEALTH (default http://127.0.0.1:32302),
# VEYRON_URL (default http://127.0.0.1:5151), KUBECONFIG, FLUXCTL (default fluxctl), STACK_CHECK_TIMEOUT (seconds, default 5).
# Exit status: 0 when nothing failed (skips are fine), 1 when any layer failed.
set -uo pipefail

FLUXVM_URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
KAIRON_NODE_HEALTH="${KAIRON_NODE_HEALTH:-http://127.0.0.1:32302}"
VEYRON_URL="${VEYRON_URL:-http://127.0.0.1:5151}"
FLUXCTL="${FLUXCTL:-fluxctl}"
TO="${STACK_CHECK_TIMEOUT:-5}"
while [ $# -gt 0 ]; do
    case "$1" in
        --kubeconfig) export KUBECONFIG="${2:?--kubeconfig needs a path}"; shift 2 ;;
        -h|--help) sed -n '5,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

FAILS=0
ok()   { printf '  ok    %-22s %s\n' "$1" "$2"; }
bad()  { printf '  FAIL  %-22s %s\n        fix: %s\n' "$1" "$2" "$3"; FAILS=$((FAILS + 1)); }
skip() { printf '  skip  %-22s %s\n' "$1" "$2"; }
get()  { curl -fsS -m "$TO" "$1" 2>/dev/null; }

echo "1. FluxVM daemon ($FLUXVM_URL)"
if get "$FLUXVM_URL/healthz" >/dev/null; then
    if get "$FLUXVM_URL/readyz" >/dev/null; then ok daemon "healthy and ready"; else bad daemon "answers but is not ready" "fluxctl doctor"; fi
else
    bad daemon "no answer" "fluxctl service install   (or: brew services start fluxvm); then fluxctl doctor"
fi

echo "2. Login session"
if command -v "$FLUXCTL" >/dev/null 2>&1; then
    DOC="$("$FLUXCTL" doctor --classic 2>&1 || true)"
    if grep -q "login session" <<<"$DOC" && grep -E "^warn +login session" <<<"$DOC" >/dev/null; then
        bad session "locked or nobody logged in: a sleeping VM cannot be restored" "unlock the Mac, or use a dedicated auto-login user with the screen lock off (deploy/launchd-notes.md)"
    elif grep -E "^ok +login session" <<<"$DOC" >/dev/null; then ok session "a user is logged in and the screen is unlocked"
    else skip session "fluxctl doctor did not report a login session"; fi
else
    skip session "$FLUXCTL not found"
fi

echo "3. Kubernetes API"
K8S=0
if ! command -v kubectl >/dev/null 2>&1; then
    skip kubernetes "kubectl not found"
elif NODES="$(kubectl get nodes --request-timeout="${TO}s" --no-headers 2>/dev/null)" && [ -n "$NODES" ]; then
    K8S=1; ok kubernetes "$(wc -l <<<"$NODES" | tr -d ' ') node(s) via ${KUBECONFIG:-the default kubeconfig}"
else
    bad kubernetes "the API does not answer (KUBECONFIG=${KUBECONFIG:-unset})" "fluxctl kube up --kubeconfig ~/.kube/veyron-mac; export KUBECONFIG=~/.kube/veyron-mac"
fi

echo "4. Kairon CRDs"
CRDS=0
if [ "$K8S" = 1 ]; then
    if [ -n "$(kubectl api-resources --api-group=kairon.zyvor.dev --no-headers 2>/dev/null)" ]; then CRDS=1; ok crds "API group kairon.zyvor.dev is served"
    else bad crds "no resources in API group kairon.zyvor.dev" "kubectl apply -f deploy/crd.yaml -f deploy/rbac.yaml -f deploy/macos/rbac.yaml   (from the Kairon repo)"; fi
else skip crds "needs a Kubernetes API"; fi

echo "5. This Mac as a Node"
if [ "$CRDS" = 1 ]; then
    MAC="$(kubectl get nodes -l kairon.zyvor.dev/backend.vz=true --no-headers 2>/dev/null)"
    if [ -z "$MAC" ]; then bad node "no Node carries kairon.zyvor.dev/backend.vz=true" "run kairon-node with scripts/macos-node.sh install (Kairon repo, docs/runbook-macos-node.md)"
    elif grep -q " Ready" <<<"$MAC"; then ok node "$(awk '{print $1}' <<<"$MAC" | tr '\n' ' ')Ready"
    else bad node "registered but not Ready: $(awk '{print $1, $2}' <<<"$MAC" | tr '\n' ';')" "scripts/macos-node.sh status; tail ~/Library/Logs/kairon-node.log"; fi
else skip node "needs the Kairon CRDs"; fi

echo "6. kairon-node ($KAIRON_NODE_HEALTH)"
if get "$KAIRON_NODE_HEALTH/healthz" >/dev/null; then
    if get "$KAIRON_NODE_HEALTH/readyz" >/dev/null; then ok kairon-node "healthy; FluxVM answers through it"
    else bad kairon-node "up, but not ready: it cannot reach the FluxVM daemon" "check FLUXVM_URL / FLUXVM_TOKEN in ~/.config/kairon/kairon-node.env, and layer 1"; fi
else
    skip kairon-node "no answer (not installed on this Mac, or on another health address)"
fi

echo "7. Veyron ($VEYRON_URL)"
if get "$VEYRON_URL/api/v1/health" >/dev/null; then ok veyron "the API answers; console: $VEYRON_URL/console"
else bad veyron "no answer" "scripts/macos-server.sh install   (Veyron repo, docs/macos-server.md); then scripts/macos-server.sh status"; fi

echo
if [ "$FAILS" -eq 0 ]; then echo "no layer failed"; else echo "$FAILS layer(s) failed"; exit 1; fi
