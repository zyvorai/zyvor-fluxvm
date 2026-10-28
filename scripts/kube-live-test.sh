#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `fluxctl kube` (docs/kubernetes-on-mac.md): a k3s cluster in a VM, driven from the Mac.
#   scripts/kube-live-test.sh
# Needs Apple silicon, kubectl (`brew install kubectl`) and network for `kube fetch` and image pulls. For the `kube load`
# checks it also needs the Docker CLI pointed at a running engine (`eval "$(fluxctl docker env)"`); without one they are skipped.
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
# FLUXVM_LIVE_MEMORY_MIB sizes each of the two VMs (default 2560); smaller than the 4096 default so the test fits beside other VMs.
# FLUXVM_LIVE_CNI=cilium (this script's default) tests the opt-in Cilium cluster; flannel (now `kube up`'s own default) tests the --offline install, which cannot use Cilium.
CNI="${FLUXVM_LIVE_CNI:-cilium}"
OFFLINE="$T/offline"
ARGS=(--name "$NAME" --api-port "$API" --kubeconfig "$KUBECONFIG" --publish "$NODEPORT" --workers 1 --cni "$CNI" --memory-mib "${FLUXVM_LIVE_MEMORY_MIB:-2560}")
[[ "$CNI" == flannel ]] && ARGS+=(--offline "$OFFLINE")
cleanup() { "${K[@]}" down "${ARGS[@]}" --purge >/dev/null 2>&1 || true; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; exit 1; }

if [[ "$CNI" == flannel ]]; then
  FETCHED="$("${K[@]}" fetch "$OFFLINE")" || bad "fetch"
  [[ "$(grep -c '"verified": true' <<<"$FETCHED")" == 2 ]] || bad "fetch verification"; ok "fetch downloads k3s and checks the binary and images against the release's SHA-256 list"
fi
"${K[@]}" up "${ARGS[@]}" >/dev/null || bad "up"
NODES="$(kubectl get nodes --no-headers | grep -c ' Ready')" ; [[ "$NODES" == 2 ]] || bad "expected 2 Ready nodes, got $NODES"; ok "--workers 1 gives a server and an agent node, both Ready"
SERVER_IP="$("${K[@]}" status "${ARGS[@]}" | sed -n 's/.*"guest_ip": "\([0-9.]*\)".*/\1/p' | head -1)"
SSH=(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$USER@$SERVER_IP")
if [[ "$CNI" == flannel ]]; then
  "${SSH[@]}" 'sudo journalctl -u fluxvm-k3s-install --no-pager | grep -q "Skipping k3s download"' || bad "offline install log"; ok "--offline: the installer skipped the download"
else
  kubectl -n kube-system rollout status ds/cilium --timeout=240s >/dev/null || bad "cilium daemonset"; ok "Cilium is the pod network (its DaemonSet is rolled out on both nodes)"
  [[ "$(kubectl get ds -n kube-system -o name | grep -c flannel || true)" == 0 ]] || bad "flannel present"
fi
[[ "$(stat -f %Lp "$KUBECONFIG")" == 600 ]] || bad "kubeconfig mode"; ok "kubeconfig written with mode 600"
grep -q "server: https://127.0.0.1:$API" "$KUBECONFIG" || bad "kubeconfig server"; ok "kubeconfig points at the forwarded port"
[[ "$(kubectl config current-context)" == "$NAME" ]] || bad "context name"; ok "context is named after the VM"
kubectl wait --for=condition=Ready node --all --timeout=120s >/dev/null || bad "node ready"; ok "kubectl sees a Ready node"
[[ "$("${K[@]}" status "${ARGS[@]}" | grep -c '"api_answers": true')" == 1 ]] || bad "status"; ok "status reports the API"
[[ "$("${K[@]}" env "${ARGS[@]}")" == "export KUBECONFIG=$KUBECONFIG" ]] || bad "env"; ok "env prints the KUBECONFIG line"
[[ "$(kubectl get pods -n kube-system --no-headers | grep -c traefik || true)" == 0 ]] || bad "traefik should be off"; ok "Traefik is off by default"

WORKER="${NAME}-w1"
kubectl get node "$WORKER" >/dev/null || bad "worker node name"; ok "the worker node is named after its VM"
kubectl run web-on-server --image=nginx:1.29-alpine --labels=app=web --overrides='{"spec":{"nodeSelector":{"kubernetes.io/hostname":"'"$NAME"'"}}}' >/dev/null
kubectl expose pod web-on-server --name=cross --port=80 >/dev/null
kubectl wait --for=condition=Ready pod/web-on-server --timeout=180s >/dev/null || bad "pod on server"
kubectl run probe --rm -i --restart=Never --image=busybox:1.37 --overrides='{"spec":{"nodeSelector":{"kubernetes.io/hostname":"'"$WORKER"'"}}}' -- wget -qO- -T 10 http://cross >/dev/null 2>&1 || bad "cross-node traffic"; ok "a pod on the worker reaches a service on the server (pod network spans the VMs)"
kubectl create deployment web --image=nginx:1.29-alpine >/dev/null
kubectl rollout status deployment/web --timeout=180s >/dev/null || bad "rollout"; ok "a deployment rolls out (images pull inside the VM)"
kubectl create service nodeport web --tcp=80:80 --node-port="$NODEPORT" >/dev/null
for _ in $(seq 1 30); do [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$NODEPORT/")" == 200 ]] && break; sleep 1; done
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$NODEPORT/")" == 200 ]] || bad "nodeport"; ok "a NodePort service answers on the Mac (--publish)"
kubectl run dns --rm -i --restart=Never --image=busybox:1.37 -- nslookup web.default.svc.cluster.local >/dev/null 2>&1 || bad "cluster dns"; ok "cluster DNS resolves a service"

if docker info >/dev/null 2>&1; then
  printf 'FROM busybox:1.37\nCMD ["echo","loaded-from-docker"]\n' | docker build -q -t fluxvm-kube-loaded:1 - >/dev/null || bad "docker build"
  "${K[@]}" load "${ARGS[@]}" fluxvm-kube-loaded:1 >/dev/null || bad "kube load"
  for node in "$NAME" "$WORKER"; do
    kubectl delete pod "loaded-$node" --ignore-not-found >/dev/null
    kubectl run "loaded-$node" --image=fluxvm-kube-loaded:1 --image-pull-policy=Never --restart=Never --overrides='{"spec":{"nodeSelector":{"kubernetes.io/hostname":"'"$node"'"}}}' >/dev/null
    for _ in $(seq 1 60); do [[ "$(kubectl get pod "loaded-$node" -o jsonpath='{.status.phase}')" == Succeeded ]] && break; sleep 2; done
    [[ "$(kubectl logs "loaded-$node")" == loaded-from-docker ]] || bad "loaded image on $node"
  done
  ok "kube load copies an image built with Docker into every node (run with imagePullPolicy: Never on server and worker)"
  docker rmi fluxvm-kube-loaded:1 >/dev/null 2>&1 || true
else
  echo "skip kube load (no Docker engine reachable)"
fi

"${K[@]}" down "${ARGS[@]}" >/dev/null || bad "down"
! kubectl get nodes --request-timeout=3s >/dev/null 2>&1 || bad "API still answering after down"; ok "down stops the API"
"${K[@]}" up "${ARGS[@]}" >/dev/null || bad "up again"
kubectl wait --for=condition=Ready node --all --timeout=120s >/dev/null || bad "node ready after restart"
[[ "$(kubectl get deployment web -o jsonpath='{.metadata.name}')" == web ]] || bad "state kept"; ok "cluster state survives down and up"
echo "all kube checks passed"
