#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `fluxctl kube` (docs/kubernetes-on-mac.md): a k3s cluster in a VM, driven from the Mac.
#   scripts/kube-live-test.sh
# Needs Apple silicon, kubectl (`brew install kubectl`) and network for the first boot and image pulls.
# Uses its own VM name, ports and kubeconfig, and deletes all of them at the end.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
command -v kubectl >/dev/null || { echo "install kubectl: brew install kubectl" >&2; exit 2; }
FLUXCTL="${FLUXCTL:-$(command -v fluxctl || echo ./target/debug/fluxctl)}"
[[ -x "$FLUXCTL" ]] || cargo build -p fluxctl 2>&1 | tail -1
T="$(mktemp -d "${HOME}/fluxvm-kube-test.XXXXXX")"
NAME="kube-livetest-$$"
API="${FLUXVM_LIVE_API_PORT:-16443}"
NODEPORT="${FLUXVM_LIVE_NODEPORT:-30080}"
export KUBECONFIG="$T/kubeconfig.yaml"
K=("$FLUXCTL" kube)
ARGS=(--name "$NAME" --api-port "$API" --kubeconfig "$KUBECONFIG" --publish "$NODEPORT")
cleanup() { "${K[@]}" down "${ARGS[@]}" --purge >/dev/null 2>&1 || true; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; exit 1; }

"${K[@]}" up "${ARGS[@]}" >/dev/null || bad "up"
[[ "$(stat -f %Lp "$KUBECONFIG")" == 600 ]] || bad "kubeconfig mode"; ok "kubeconfig written with mode 600"
grep -q "server: https://127.0.0.1:$API" "$KUBECONFIG" || bad "kubeconfig server"; ok "kubeconfig points at the forwarded port"
[[ "$(kubectl config current-context)" == "$NAME" ]] || bad "context name"; ok "context is named after the VM"
kubectl wait --for=condition=Ready node --all --timeout=120s >/dev/null || bad "node ready"; ok "kubectl sees a Ready node"
[[ "$("${K[@]}" status "${ARGS[@]}" | grep -c '"api_answers": true')" == 1 ]] || bad "status"; ok "status reports the API"
[[ "$("${K[@]}" env "${ARGS[@]}")" == "export KUBECONFIG=$KUBECONFIG" ]] || bad "env"; ok "env prints the KUBECONFIG line"
[[ "$(kubectl get pods -n kube-system --no-headers | grep -c traefik || true)" == 0 ]] || bad "traefik should be off"; ok "Traefik is off by default"

kubectl create deployment web --image=nginx:1.29-alpine >/dev/null
kubectl rollout status deployment/web --timeout=180s >/dev/null || bad "rollout"; ok "a deployment rolls out (images pull inside the VM)"
kubectl create service nodeport web --tcp=80:80 --node-port="$NODEPORT" >/dev/null
for _ in $(seq 1 30); do [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$NODEPORT/")" == 200 ]] && break; sleep 1; done
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$NODEPORT/")" == 200 ]] || bad "nodeport"; ok "a NodePort service answers on the Mac (--publish)"
kubectl run dns --rm -i --restart=Never --image=busybox:1.37 -- nslookup web.default.svc.cluster.local >/dev/null 2>&1 || bad "cluster dns"; ok "cluster DNS resolves a service"

"${K[@]}" down "${ARGS[@]}" >/dev/null || bad "down"
! kubectl get nodes --request-timeout=3s >/dev/null 2>&1 || bad "API still answering after down"; ok "down stops the API"
"${K[@]}" up "${ARGS[@]}" >/dev/null || bad "up again"
kubectl wait --for=condition=Ready node --all --timeout=120s >/dev/null || bad "node ready after restart"
[[ "$(kubectl get deployment web -o jsonpath='{.metadata.name}')" == web ]] || bad "state kept"; ok "cluster state survives down and up"
echo "all kube checks passed"
