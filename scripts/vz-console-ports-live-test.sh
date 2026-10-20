#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: named console ports and the read-only serial stream of a vz guest. `apple.console_ports` adds virtio console ports
# (`GET /v1/vms/{id}/ports/{name}`, a WebSocket of raw bytes both ways, no network or agent needed in the guest); `GET /v1/vms/{id}/serial`
# replays the tail of the console log and follows it, read only. The WebSocket client is a small stdlib Python one.
#   scripts/vz-console-ports-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-ports.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7816}"
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
run_dir = "/tmp/fluxvm-run-ports"
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

RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"parent","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{},"console_ports":["alpha","beta"]},"agent":{"enabled":true}}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "guest $P is up with console ports alpha and beta" || bad "agent never answered"
LS="$(gexec "$P" 'ls /dev/virtio-ports 2>&1 | tr "\n" " "')"; echo "     /dev/virtio-ports: $LS"
[[ "$LS" == *alpha* && "$LS" == *beta* ]] && ok "both ports appear in the guest under /dev/virtio-ports" || bad "ports in the guest: $LS"

WS="$T/ws.py"
cat > "$WS" <<'PY'
# usage: ws.py PORT VMID PATH   steps as JSON on stdin: send / expect / agent (run a guest command, its stdout joins the stream) / close
import base64, json, os, re, socket, struct, sys, time, urllib.request
port, vm, path = int(sys.argv[1]), sys.argv[2], sys.argv[3]
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
def agent(cmd):
    req = urllib.request.Request(f"http://127.0.0.1:{port}/v1/vms/{vm}/agent", method="POST",
        data=json.dumps({"command": cmd, "timeout_seconds": 30}).encode(), headers={"content-type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=40)).get("stdout", "")
for st in steps:
    if "send" in st:
        send(1 if st.get("kind") == "text" else 2, st["send"].encode())
    elif "agent" in st:
        r = agent(st["agent"])
        if st.get("keep"): seen += r
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
    elif "absent" in st:
        deadline = time.time() + st.get("timeout", 4)
        while True:
            f = frame(deadline)
            if f is None: break
            op, p = f
            if op in (1, 2): seen += p.decode(errors="replace")
        out["results"].append({"absent": st["absent"], "ok": st["absent"] not in seen, "tail": seen[-200:]})
    elif "close" in st:
        send(8, b"\x03\xe8")
print(json.dumps(out)); s.close()
PY
ws() { python3 -I "$WS" "$PORT" "$P" "$1"; }
okall() { jget 'all(r["ok"] for r in d["results"]) and d["status"].split()[1] == "101"'; }

R="$(ws /v1/vms/$P/ports/alpha <<<'[{"agent":"sleep 1; echo G2H-ONE > /dev/virtio-ports/alpha"},{"expect":"G2H-ONE","timeout":20}]')"
echo "     $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | okall)" == True ]] && ok "guest to host: a write to /dev/virtio-ports/alpha arrives on the websocket" || bad "guest to host: $R"

# The port drops host input while no guest process has it open, so the guest reader opens it first (one descriptor for both
# directions), announces READY, and only then does the host send.
R="$(ws /v1/vms/$P/ports/alpha <<<'[{"agent":"rm -f /tmp/got; nohup setsid sh -c \"exec 3<>/dev/virtio-ports/alpha; echo READY-H2G >&3; head -c 7 <&3 > /tmp/got\" >/dev/null 2>&1 </dev/null & sleep 1"},{"expect":"READY-H2G","timeout":20},{"send":"H2G-TWO"},{"agent":"sleep 2; cat /tmp/got; echo; ps | grep -c [h]ead","keep":1},{"expect":"H2G-TWO","timeout":10}]')"
echo "     $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | okall)" == True ]] && ok "host to guest: bytes sent on the websocket are read from /dev/virtio-ports/alpha" || bad "host to guest: $R"

R="$(ws /v1/vms/$P/ports/alpha <<<'[{"agent":"sleep 1; echo ONLY-BETA > /dev/virtio-ports/beta"},{"absent":"ONLY-BETA","timeout":5}]')"
[[ "$(echo "$R" | okall)" == True ]] && ok "the ports are separate: a write to beta does not reach alpha" || bad "isolation: $R"
R="$(ws /v1/vms/$P/ports/beta <<<'[{"agent":"sleep 1; echo ONLY-BETA2 > /dev/virtio-ports/beta"},{"expect":"ONLY-BETA2","timeout":20}]')"
[[ "$(echo "$R" | okall)" == True ]] && ok "and the second port carries its own traffic" || bad "beta: $R"

R="$(ws /v1/vms/$P/ports/nope <<<'[]')"; echo "     unknown port: $(echo "$R" | cut -c1-250)"
[[ "$(echo "$R" | jget '"101" not in d["status"] and d["status"].split()[1].startswith("4")')" == True ]] && ok "an unknown port name is refused with an HTTP error" || bad "unknown port: $R"

R="$(ws /v1/vms/$P/serial <<<'[{"expect":"login:","timeout":20},{"send":"typed-into-serial\n"},{"agent":"echo SERIAL-LIVE > /dev/hvc0"},{"expect":"SERIAL-LIVE","timeout":20}]')"
echo "     $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | okall)" == True ]] && ok "serial replays the console tail (login prompt) and follows new output" || bad "serial: $R"
R="$(ws /v1/vms/$P/serial <<<'[{"expect":"login:","timeout":20},{"send":"echo NOT-EXECUTED-SERIAL > /dev/hvc0\n"},{"absent":"NOT-EXECUTED-SERIAL","timeout":4}]')"
[[ "$(echo "$R" | okall)" == True ]] && ok "serial is read only: input sent on it never reaches the guest" || bad "serial read-only: $R"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz console ports and serial: PASS"
