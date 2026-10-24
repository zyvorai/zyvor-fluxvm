#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `fluxctl docker` (docs/docker-engine.md): a real engine in a VM, driven from the Mac.
#   scripts/docker-engine-live-test.sh
# Needs Apple silicon, the Docker CLI (`brew install docker docker-compose`) and network for the first boot and image pulls.
# Uses its own VM name and socket, and deletes both at the end.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
command -v docker >/dev/null || { echo "install the Docker CLI: brew install docker docker-compose" >&2; exit 2; }
FLUXCTL="${FLUXCTL:-$(command -v fluxctl || echo ./target/debug/fluxctl)}"
[[ -x "$FLUXCTL" ]] || cargo build -p fluxctl 2>&1 | tail -1
T="$(mktemp -d "${HOME}/fluxvm-docker-engine-test.XXXXXX")"   # under $HOME so it can be shared at the same path
NAME="docker-engine-livetest-$$"
SOCK="/tmp/fluxvm-docker-engine-test-$$.sock"
cleanup() { "$FLUXCTL" docker down --name "$NAME" --socket "$SOCK" --purge >/dev/null 2>&1 || true; rm -rf "$T" "$SOCK"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; exit 1; }

PORT="${FLUXVM_LIVE_PORT:-18089}"
"$FLUXCTL" docker up --name "$NAME" --socket "$SOCK" --share "$T" --publish "$PORT" >/dev/null || bad "up"
export DOCKER_HOST="unix://$SOCK"
docker version --format '{{.Server.Version}}' >/dev/null || bad "docker version"; ok "the Docker CLI reaches the engine"
[[ "$("$FLUXCTL" docker status --name "$NAME" --socket "$SOCK" | grep -c '"socket_answers": true')" == 1 ]] || bad "status"; ok "status reports the socket"
[[ "$(docker run --rm alpine:3.22 echo hello)" == hello ]] || bad "run"; ok "docker run"

printf 'FROM alpine:3.22\nRUN echo built-in-the-vm > /marker\nCMD ["cat","/marker"]\n' > "$T/Dockerfile"
docker build -q -t fluxvm-engine-test "$T" >/dev/null || bad "build"
[[ "$(docker run --rm fluxvm-engine-test)" == built-in-the-vm ]] || bad "built image"; ok "docker build"

echo from-the-mac > "$T/note.txt"
[[ "$(docker run --rm -v "$T:/data:ro" alpine:3.22 cat /data/note.txt)" == from-the-mac ]] || bad "bind mount"; ok "bind mount of a shared Mac folder"

docker run -d --rm --name pub -p "127.0.0.1:$PORT:80" nginx:1.29-alpine >/dev/null
for _ in $(seq 1 20); do [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")" =~ ^[0-9]+$ && "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")" != 000 ]] && break; sleep 1; done
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/")" != 000 ]] || bad "published port"; docker rm -f pub >/dev/null; ok "a published container port is reachable from the Mac (--publish)"

docker network create fluxvm-engine-net >/dev/null
docker run -d --rm --name web --network fluxvm-engine-net alpine:3.22 sleep 60 >/dev/null
docker run --rm --network fluxvm-engine-net alpine:3.22 ping -c1 web >/dev/null || bad "network name resolution"
docker rm -f web >/dev/null; docker network rm fluxvm-engine-net >/dev/null; ok "user-defined network with name resolution"

cat > "$T/compose.yaml" <<'EOT'
services:
  api:
    build: .
    command: ["sh","-c","echo ok > /tmp/ready && sleep 120"]
    healthcheck: {test: ["CMD","test","-f","/tmp/ready"], interval: 2s, retries: 15}
  conf:
    image: alpine:3.22
    command: ["sleep","120"]
    volumes: ["./note.txt:/note.txt:ro"]
    tmpfs: ["/scratch"]
    read_only: true
EOT
(cd "$T" && docker compose -p fluxvm-engine-test up --build --wait -d) >/dev/null || bad "compose up"
[[ "$(cd "$T" && docker compose -p fluxvm-engine-test exec -T conf cat /note.txt)" == from-the-mac ]] || bad "compose bind mount"
(cd "$T" && docker compose -p fluxvm-engine-test down) >/dev/null; ok "compose up --build --wait, bind mount, tmpfs, read_only, health check"
echo "all checks passed"
