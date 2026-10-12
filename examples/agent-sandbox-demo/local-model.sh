#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# A sandbox with NO network card that still reaches a model server on the Mac: the guest's only way out is one vsock
# port relayed to 127.0.0.1:PORT on the host (`apple.vsock_services`, `tcp`). The script creates such a sandbox, starts
# a small relay inside it, asks the host server for /v1/models and a chat completion, and shows the guest still cannot
# reach the internet.
#
#   examples/agent-sandbox-demo/local-model.sh                  # a llama-server / mlx_lm.server already on 127.0.0.1:8080
#   MODEL_PORT=8081 examples/agent-sandbox-demo/local-model.sh
#   STAND_IN=1 examples/agent-sandbox-demo/local-model.sh       # no model needed: a tiny OpenAI-style stub on that port,
#                                                               # which proves the path, not inference
# Needs a daemon on Apple silicon with the default Linux image, curl, jq and python3. Everything it makes is deleted.
set -euo pipefail

URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
PORT="${MODEL_PORT:-8080}"
AUTH=()
[[ -n "${FLUXVM_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")
call() { curl -sf ${AUTH[@]+"${AUTH[@]}"} -H 'Content-Type: application/json' "$@"; }

STUB=""
id=""
cleanup() {
  [[ -n "$id" ]] && call -X DELETE "$URL/v1/vms/$id" >/dev/null 2>&1 || true
  [[ -n "$STUB" ]] && kill "$STUB" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

if [[ "${STAND_IN:-0}" == 1 ]]; then
  python3 - "$PORT" <<'PY' &
import http.server, json, sys
class H(http.server.BaseHTTPRequestHandler):
    def _j(self, o):
        b = json.dumps(o).encode()
        self.send_response(200); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self._j({"object": "list", "data": [{"id": "stand-in-model", "object": "model"}]})
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        last = body.get("messages", [{}])[-1].get("content", "")
        self._j({"object": "chat.completion", "model": "stand-in-model",
                 "choices": [{"index": 0, "message": {"role": "assistant", "content": "stand-in reply to: " + last}}]})
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
  STUB=$!
  sleep 1
fi
curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null \
  || { echo "nothing answers on 127.0.0.1:$PORT/v1/models: start llama-server or mlx_lm.server there, or STAND_IN=1" >&2; exit 1; }

echo "1. sandbox with no network card; vsock port 5100 relayed to 127.0.0.1:$PORT on this Mac"
id="$(call -X POST "$URL/v1/sandboxes" -d "{\"name\":\"local-model-$$\",\"offline\":true,\"ttl_seconds\":900,
  \"spec\":{\"name\":\"local-model-$$\",\"backend\":\"vz\",\"image\":\"debian-13\",\"vcpus\":2,\"memory_mib\":2048,
  \"apple\":{\"vsock_services\":[{\"port\":5100,\"tcp\":\"127.0.0.1:$PORT\"}]}}}" | jq -r .id)"

echo "2. inside it: a relay from 127.0.0.1:8080 to the vsock port, then the model API"
guest_cmd='ip -br a | tr -s " "; cat > relay.py <<PY
import socket, threading
def pump(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    finally:
        a.close(); b.close()
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 8080)); srv.listen(16)
while True:
    c, _ = srv.accept()
    v = socket.socket(socket.AF_VSOCK); v.connect((2, 5100))
    threading.Thread(target=pump, args=(c, v), daemon=True).start()
    threading.Thread(target=pump, args=(v, c), daemon=True).start()
PY
nohup python3 relay.py >relay.log 2>&1 &
sleep 1
python3 - <<PY
import json, urllib.request
print("models:", json.load(urllib.request.urlopen("http://127.0.0.1:8080/v1/models"))["data"][0]["id"])
req = urllib.request.Request("http://127.0.0.1:8080/v1/chat/completions", data=json.dumps({"model": "x",
  "messages": [{"role": "user", "content": "hello from a guest with no network card"}]}).encode(),
  headers={"content-type": "application/json"})
print("chat:", json.load(urllib.request.urlopen(req))["choices"][0]["message"]["content"])
try:
    urllib.request.urlopen("http://1.1.1.1", timeout=4)
    print("internet: REACHABLE")
except Exception as e:
    print("internet: unreachable (%s)" % type(e).__name__)
PY'
call -X POST "$URL/v1/sandboxes/$id/process" -d "$(jq -n --arg c "$guest_cmd" '{command: $c}')" \
  | jq -r '.stdout' | sed 's/^/   /'
echo "3. deleted on exit"
