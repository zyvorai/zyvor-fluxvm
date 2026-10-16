#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Live test of apple.host_gpu: an offline vz sandbox reaches a host OpenAI-compatible
# server over vsock (docs/host-inference.md). Starts fluxctl infer serve --stand-in when
# nothing answers on MODEL_PORT (default 8080); otherwise uses the existing server.
#
#   scripts/vz-host-gpu-live-test.sh
# Needs Apple silicon, a daemon on FLUXVM_URL (default http://127.0.0.1:7788), curl, jq, python3.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }

URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
PORT="${MODEL_PORT:-8080}"
AUTH=()
[[ -n "${FLUXVM_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")
call() { curl -sf ${AUTH[@]+"${AUTH[@]}"} -H 'Content-Type: application/json' "$@"; }

ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; exit 1; }

curl -sf "$URL/healthz" >/dev/null || bad "daemon not up at $URL"

INFER=""
id=""
cleanup() {
  [[ -n "$id" ]] && call -X DELETE "$URL/v1/vms/$id" >/dev/null 2>&1 || true
  [[ -n "$INFER" ]] && kill "$INFER" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if ! curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1; then
  if ! command -v fluxctl >/dev/null || ! fluxctl infer serve --help >/dev/null 2>&1; then
    bad "nothing on :$PORT and fluxctl infer serve is unavailable; start a server or install 0.4.10+"
  fi
  fluxctl infer serve --stand-in --listen "127.0.0.1:$PORT" >/tmp/fluxvm-host-gpu-infer.log 2>&1 &
  INFER=$!
  for _ in $(seq 1 50); do
    curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null && break
    sleep 0.2
  done
fi
curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null || bad "no model server on 127.0.0.1:$PORT"
ok "host model on :$PORT"

if fluxctl infer status 2>/dev/null | grep -qiE 'ok|ready|stand-in|model'; then
  ok "fluxctl infer status"
fi

id="$(call -X POST "$URL/v1/sandboxes" -d "{\"name\":\"host-gpu-live-$$\",\"offline\":true,\"host_gpu\":true,\"ttl_seconds\":600}" | jq -r .id)"
[[ "$id" != null && -n "$id" ]] || bad "sandbox create with host_gpu"
ok "sandbox $id host_gpu"

guest='python3 - <<"PY"
import json, os, time, urllib.request
base = os.environ.get("OPENAI_BASE_URL", "http://127.0.0.1:8080/v1")
assert "127.0.0.1" in base, base
for _ in range(40):
    try:
        urllib.request.urlopen(base + "/models", timeout=2)
        break
    except Exception:
        time.sleep(0.5)
else:
    raise SystemExit("forwarder not ready: " + base)
models = json.load(urllib.request.urlopen(base + "/models"))
print("models", models["data"][0]["id"])
req = urllib.request.Request(
    base + "/chat/completions",
    data=json.dumps({"model": "x", "messages": [{"role": "user", "content": "ping"}]}).encode(),
    headers={"content-type": "application/json"},
)
print("chat", json.load(urllib.request.urlopen(req))["choices"][0]["message"]["content"])
try:
    urllib.request.urlopen("http://1.1.1.1", timeout=2)
    raise SystemExit("internet reachable")
except Exception as e:
    if "internet reachable" in str(e):
        raise
    print("offline ok")
PY'

out="$(call -X POST "$URL/v1/sandboxes/$id/process" -d "$(jq -n --arg c "$guest" '{command: $c}')")"
code="$(echo "$out" | jq -r '.exit_code // .ExitCode // empty')"
[[ "$code" == "0" ]] || bad "guest process exit=$code: $out"
stdout="$(echo "$out" | jq -r '.stdout // .output // empty')"
echo "$stdout" | grep -q 'offline ok' || bad "guest still has a route / bad output: $out"
ok "guest OPENAI_BASE_URL over vsock; no internet"
echo "PASS vz-host-gpu-live-test"
