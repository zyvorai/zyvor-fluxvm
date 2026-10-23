#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of the Docker Engine API shim (docs/oci-sandboxes.md#docker-cli-docker_host).
#   FLUXVM_OCI_BOOT_DIR=dist/oci-boot scripts/docker-compat-live-test.sh
# Needs Apple silicon, Docker CLI (`brew install docker`), network for alpine pull, and OCI boot artifacts.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
command -v docker >/dev/null || { echo "install Docker CLI: brew install docker" >&2; exit 2; }
BOOT="$(cd "${FLUXVM_OCI_BOOT_DIR:-dist/oci-boot}" 2>/dev/null && pwd)" || { echo "set FLUXVM_OCI_BOOT_DIR to the directory with oci-kernel and oci-initrd" >&2; exit 2; }
[[ -f "$BOOT/oci-kernel" && -f "$BOOT/oci-initrd" ]] || { echo "$BOOT has no oci-kernel / oci-initrd" >&2; exit 2; }
ALPINE="${FLUXVM_OCI_ALPINE:-alpine:3.22}"
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-docker-compat.XXXXXX")"
PORT="${FLUXVM_LIVE_PORT:-7799}"
RUN_DIR="$T/run"
SOCK="$RUN_DIR/docker.sock"
cleanup() { [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -20 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -p fluxvm-vz-switch -j 4 2>&1 | tail -1
FLUXCTL=./target/debug/fluxctl
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "$RUN_DIR"
docker_sock = "$SOCK"

[apple]
oci_kernel = "$BOOT/oci-kernel"
oci_initrd = "$BOOT/oci-initrd"
EOT
$FLUXCTL --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do
  [[ -S "$SOCK" ]] && curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break
  sleep 1
done
[[ -S "$SOCK" ]] || bad "docker.sock not created"
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null || bad "daemon did not start"
ok "daemon + docker.sock"

export DOCKER_HOST="unix://$SOCK"
docker version >/dev/null || bad "docker version"
ok "docker version"
docker info 2>/dev/null | grep -q fluxvm || bad "docker info missing fluxvm"
ok "docker info"

docker pull "$ALPINE" >/dev/null || bad "docker pull"
ok "docker pull $ALPINE"
docker images --format '{{.Repository}}:{{.Tag}}' | grep -q "${ALPINE%%:*}" || bad "docker images"
ok "docker images"

OUT="$(docker run --rm "$ALPINE" echo hello-from-fluxvm)" || bad "docker run"
[[ "$OUT" == *hello-from-fluxvm* ]] || bad "docker run output: $OUT"
ok "docker run --rm"

CID="$(docker run -d "$ALPINE" sleep 120)" || bad "docker run -d"
ok "docker run -d ($CID)"
docker ps --format '{{.ID}}' | grep -q "${CID:0:12}" || bad "docker ps"
ok "docker ps"
EXECOUT="$(docker exec "$CID" uname -s)" || bad "docker exec"
[[ "$EXECOUT" == Linux ]] || bad "docker exec output: $EXECOUT"
ok "docker exec"
docker stop "$CID" >/dev/null || bad "docker stop"
ok "docker stop"
docker rm -f "$CID" >/dev/null || bad "docker rm"
ok "docker rm"

echo "PASS docker-compat-live-test"
