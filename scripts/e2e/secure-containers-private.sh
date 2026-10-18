#!/usr/bin/env bash
# shellcheck disable=SC2034
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The secure-container runtime end to end on a shared Linux/KVM host without touching what is there: a PRIVATE containerd (own
# socket, root and state under a temp dir, no CRI), an isolated `fluxctl serve` (own port and state dir), the shim and agent built
# from this tree, and scripts/e2e-secure-containers-ctr.sh run against them (`ctr run --runtime io.containerd.fluxvm.v2`, the
# gate is the guest's stdout marker). Everything it started is stopped by pid and removed at the end. Run as root:
#   sudo scripts/e2e/secure-containers-private.sh [--guest-image PATH] [--containerd PATH]
# containerd is looked up in PATH, then in k3s's bundled one (/var/lib/rancher/k3s/data/current/bin/containerd), which is only
# used as a binary - k3s itself is not touched. Needs: target/release/{containerd-shim-fluxvm-v2,fluxvm-container-agent}, fluxctl,
# a guest image with the guest agent (default /var/lib/fluxvm/images/secure-container.qcow2) and network access to pull busybox.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
GUEST="/var/lib/fluxvm/images/secure-container.qcow2"; CD="$(command -v containerd || true)"
while [[ $# -gt 0 ]]; do case "$1" in --guest-image) GUEST="$2"; shift 2 ;; --containerd) CD="$2"; shift 2 ;; *) echo "unknown $1" >&2; exit 2 ;; esac; done
[[ -n "$CD" ]] || CD="/var/lib/rancher/k3s/data/current/bin/containerd"
[[ "$(id -u)" == 0 ]] || { echo "run as root (sudo)" >&2; exit 2; }
[[ -e /dev/kvm ]] || { echo "needs /dev/kvm" >&2; exit 2; }
TD="${CARGO_TARGET_DIR:-target}/release"
SHIM="$PWD/$TD/containerd-shim-fluxvm-v2"; AGENT="$PWD/$TD/fluxvm-container-agent"; [[ "$TD" == /* ]] && { SHIM="$TD/containerd-shim-fluxvm-v2"; AGENT="$TD/fluxvm-container-agent"; }
FX="${FLUXVM_BIN:-$(command -v fluxctl)}"; CTR_BIN="$(command -v ctr || echo /usr/local/bin/ctr)"
for f in "$SHIM" "$AGENT" "$FX" "$CD" "$CTR_BIN" "$GUEST"; do [[ -e "$f" ]] || { echo "missing: $f" >&2; exit 2; }; done
export PATH="/usr/libexec:/usr/lib/qemu:$PATH"   # virtiofsd (container rootfs shares) lives there on Debian/Ubuntu
T="$(mktemp -d /tmp/fluxvm-sc.XXXXXX)"; PD=$((20000 + RANDOM % 20000)); PIDS=()
cleanup() {
  for id in $(curl -s "http://127.0.0.1:$PD/v1/vms" 2>/dev/null | jq -r '(.items // .)[]?|.id' 2>/dev/null); do curl -s -XDELETE "http://127.0.0.1:$PD/v1/vms/$id" >/dev/null 2>&1; done
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null; done; sleep 1
  for p in ${PIDS[@]+"${PIDS[@]}"}; do kill -9 "$p" 2>/dev/null; done
  pkill -f "$T/" 2>/dev/null; rm -rf "$T"
}
trap cleanup EXIT

mkdir -p "$T/bin" "$T/state" "$T/run" "$T/cd/root" "$T/cd/state"
ln -s "$SHIM" "$T/bin/containerd-shim-fluxvm-v2"
cat > "$T/fluxvm.toml" <<TOML
listen = "127.0.0.1:$PD"
state_dir = "$T/state"
run_dir = "$T/run"
default_bridge = "vmbr0"
reaper_interval_secs = 5
TOML
"$FX" --config "$T/fluxvm.toml" serve >"$T/serve.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:$PD/healthz" >/dev/null && break; sleep 1; done
curl -sf "http://127.0.0.1:$PD/healthz" >/dev/null || { echo "isolated daemon did not start"; tail -5 "$T/serve.log"; exit 1; }
echo "ok   isolated fluxvm daemon on :$PD"

cat > "$T/cd/config.toml" <<TOML
version = 2
root = "$T/cd/root"
state = "$T/cd/state"
[grpc]
  address = "$T/cd/containerd.sock"
[plugins."io.containerd.grpc.v1.cri"]
  disable = true
TOML
PATH="$T/bin:$PATH" FLUXVM_API_URL="http://127.0.0.1:$PD" FLUXVM_CONTAINER_GUEST_IMAGE="$GUEST" FLUXVM_CONTAINER_AGENT_BINARY="$AGENT" \
  FLUXVM_CONTAINERD_STATE_DIR="$T/cd/fluxvm" "$CD" --config "$T/cd/config.toml" >"$T/containerd.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 30); do [[ -S "$T/cd/containerd.sock" ]] && "$CTR_BIN" --address "$T/cd/containerd.sock" version >/dev/null 2>&1 && break; sleep 1; done
"$CTR_BIN" --address "$T/cd/containerd.sock" version >/dev/null 2>&1 || { echo "FAIL private containerd did not start"; tail -8 "$T/containerd.log"; exit 1; }
echo "ok   private containerd ($("$CD" --version | head -1 | cut -c1-60))"

# SC_SCRIPTS: space-separated e2e-secure-containers-*.sh names to run (default: the ctr smoke); each gets the same private stack.
fails=0
# These scripts talk to the host's Kubernetes cluster; a shared host's cluster is not ours, so none is reachable from here.
for sc in ${SC_SCRIPTS:-ctr}; do
  if grep -q kubectl "scripts/e2e-secure-containers-$sc.sh"; then echo "SKIP $sc: needs a Kubernetes cluster with the fluxvm RuntimeClass"; continue; fi
  PATH="$T/bin:$PATH" CTR="$CTR_BIN" CONTAINERD_ADDRESS="$T/cd/containerd.sock" FLUXVM_API_URL="http://127.0.0.1:$PD" \
    FLUXVM_CONTAINER_GUEST_IMAGE="$GUEST" FLUXVM_CONTAINER_AGENT_BINARY="$AGENT" FLUXVM_SECURE_CONTAINERS_E2E=1 KUBECONFIG=/nonexistent \
    timeout -k 10 "${SC_TIMEOUT:-600}" "scripts/e2e-secure-containers-$sc.sh" >"$T/$sc.log" 2>&1
  rc=$?
  if [[ $rc == 0 ]]; then echo "ok   $sc: $(grep -E 'PASS|passed|OK' "$T/$sc.log" | tail -1 | cut -c1-140)"
  else echo "FAIL $sc (rc $rc): $(tail -3 "$T/$sc.log" | tr '\n' ' ' | cut -c1-300)"; fails=$((fails + 1)); fi
done
echo; echo "$fails check(s) failed"; exit "$fails"
