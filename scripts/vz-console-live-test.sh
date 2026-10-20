#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the interactive console of a vz guest (`GET /v1/vms/{id}/console`, a WebSocket carrying a PTY over the guest agent).
# A shell answers typed commands, a text frame resizes the PTY, a malformed control frame is dropped without ending the session,
# a second session works after the first closes, and a guest without the agent is refused with an HTTP error before any upgrade.
# The WebSocket client is a small stdlib Python one (no third-party packages).
#   scripts/vz-console-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-console.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7815}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -10 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-console"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
status() { curl -fs "$B/$1" | jget 'd.get("status","")'; }
gexec() { # <vm> <command>
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 30}))' "$2")" | jget 'd.get("stdout","").strip()'
}
wait_agent() {
  for _ in $(seq 1 60); do [[ "$(gexec "$1" 'echo up' 2>/dev/null)" == up ]] && return 0; sleep 2; done
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' -d '{"command":"echo up","timeout_seconds":10}' | cut -c1-400 >&2; return 1
}

RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"parent","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "parent $P is up with the agent" || bad "agent never answered"

WS="$T/ws-console.py"
cat > "$WS" <<'PY'
# usage: ws-console.py PORT PATH   (steps as JSON on stdin: [{"send":"..","kind":"bin|text"} | {"expect":"regex","timeout":N}])
import base64, json, os, re, socket, struct, sys, time
port, path = int(sys.argv[1]), sys.argv[2]
steps = json.load(sys.stdin)
s = socket.create_connection(("127.0.0.1", port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
buf = b""
while b"\r\n\r\n" not in buf:
    c = s.recv(4096)
    if not c: break
    buf += c
head, _, rest = buf.partition(b"\r\n\r\n")
status = head.split(b"\r\n")[0].decode()
out = {"status": status, "results": []}
if " 101 " not in status:
    out["body"] = rest.decode(errors="replace")[:300]
    print(json.dumps(out)); sys.exit(0)
rx = rest; seen = ""
def need(n, deadline):
    global rx
    while len(rx) < n:
        s.settimeout(max(0.1, deadline - time.time()))
        try: c = s.recv(65536)
        except socket.timeout: return False
        if not c: return False
        rx += c
    return True
def frame(deadline):
    global rx
    if not need(2, deadline): return None
    b0, b1 = rx[0], rx[1]; n = b1 & 0x7f; off = 2
    if n == 126:
        if not need(4, deadline): return None
        n = struct.unpack(">H", rx[2:4])[0]; off = 4
    elif n == 127:
        if not need(10, deadline): return None
        n = struct.unpack(">Q", rx[2:10])[0]; off = 10
    if not need(off + n, deadline): return None
    payload = rx[off:off + n]; rx = rx[off + n:]
    return b0 & 0x0f, payload
def send(op, data):
    mask = os.urandom(4); n = len(data)
    hdr = bytes([0x80 | op]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n))
    s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))
for st in steps:
    if "send" in st:
        send(1 if st.get("kind") == "text" else 2, st["send"].encode())
    elif "expect" in st:
        deadline = time.time() + st.get("timeout", 20); ok = False
        while True:
            m = re.search(st["expect"], seen)
            if m: ok = True; seen = seen[m.end():]; break
            f = frame(deadline)
            if f is None: break
            op, p = f
            if op in (1, 2): seen += p.decode(errors="replace")
            elif op == 9: send(10, p)
            elif op == 8: break
        out["results"].append({"expect": st["expect"], "ok": ok, "tail": seen[-200:] if not ok else ""})
    elif "close" in st:
        send(8, b"\x03\xe8")
print(json.dumps(out)); s.close()
PY
wsrun() { python3 -I "$WS" "$PORT" "/v1/vms/$1/console?cols=100&rows=30"; }
okall() { jget 'all(r["ok"] for r in d["results"]) and d["status"].split()[1] == "101"'; }

R="$(wsrun "$P" <<<'[{"expect":"[#$] ","timeout":30},{"send":"echo MARK$((6*7))\n"},{"expect":"MARK42","timeout":20}]')"
echo "     $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | okall)" == True ]] && ok "the console upgrades (101) and a typed command runs in the guest shell" || bad "console: $R"

R="$(wsrun "$P" <<<'[{"expect":"[#$] ","timeout":30},{"send":"{\"cols\":132,\"rows\":41}","kind":"text"},{"send":"sleep 1; stty size\n"},{"expect":"41 132","timeout":20}]')"
[[ "$(echo "$R" | okall)" == True ]] && ok "a text frame resizes the PTY (stty size reports 41 132)" || bad "resize: $R"

R="$(wsrun "$P" <<<'[{"expect":"[#$] ","timeout":30},{"send":"not json at all","kind":"text"},{"send":"echo STILL$((1+1))\n"},{"expect":"STILL2","timeout":20}]')"
[[ "$(echo "$R" | okall)" == True ]] && ok "a malformed control frame is dropped and the session carries on" || bad "malformed frame: $R"

R="$(wsrun "$P" <<<'[{"expect":"[#$] ","timeout":30},{"send":"export KEEP=first; echo SESS$((2+3))\n"},{"expect":"SESS5","timeout":20},{"close":1}]')"
R2="$(wsrun "$P" <<<'[{"expect":"[#$] ","timeout":30},{"send":"echo KEEP=${KEEP:-unset}\n"},{"expect":"KEEP=unset","timeout":20}]')"
[[ "$(echo "$R" | okall)" == True && "$(echo "$R2" | okall)" == True ]] && ok "a second session works after the first closes, with a fresh shell" || bad "second session: $R / $R2"

RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"noagent","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024}')"
N="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create noagent: $RESP"
R="$(wsrun "$N" <<<'[]')"; echo "     no agent: $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | jget '"101" not in d["status"] and d["status"].split()[1].startswith("4")')" == True ]] \
  && ok "a guest without the agent is refused with an HTTP error before any upgrade" || bad "no-agent console: $R"
for V in "$N" "$P"; do curl -fs -X DELETE "$B/$V" >/dev/null || true; done
echo "vz console: PASS"
