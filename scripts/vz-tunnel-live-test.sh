#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the TCP tunnel to a vz guest (`GET /v1/vms/{id}/tunnel?port=N`, a WebSocket relaying raw bytes to a TCP port in the guest).
# A guest on the Mac's NAT is reached on its sshd (banner, then the key exchange the guest sends once the client has spoken), and
# the refusals are checked: port 0, a closed port, a stopped VM, an unknown VM and a usernet guest. The WebSocket client is a small stdlib Python one.
#   scripts/vz-tunnel-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-tunnel.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7818}"
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
run_dir = "/tmp/fluxvm-run-tunnel"
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

mkvm() { curl -sS -X POST "$B" -H 'content-type: application/json' -d "{\"name\":\"$1\",\"backend\":\"vz\",\"image\":\"agent-micro\",\"vcpus\":2,\"memory_mib\":1024,$2}"; }
field() { curl -fs "$B/$1" | jget "d.get(\"$2\")"; }
RESP="$(mkvm natvm '"network":{"mode":"user"},"agent":{"enabled":true}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "NAT guest $P is up" || bad "agent never answered"
# The runner learns a Linux guest's address from a `VELORA-IP <addr>` line the guest prints on its console (the stock image has a
# unit for that; agent-micro does not), so have the guest print the address it really has.
announce() { gexec "$1" 'IP=$(ip -4 -o addr show scope global | awk "{print \$4}" | cut -d/ -f1 | head -1); echo "VELORA-IP $IP" > /dev/hvc0; echo $IP'; }
echo "     guest address from inside: $(announce "$P")"
for _ in $(seq 1 30); do IP="$(field "$P" guest_ip)"; [[ -n "$IP" && "$IP" != None ]] && break; sleep 2; done
echo "     guest_ip: $IP"
[[ -n "$IP" && "$IP" != None ]] && ok "the guest reported an address ($IP)" || bad "no guest_ip"

WS="$T/ws.py"
cat > "$WS" <<'PY'
# usage: ws.py PORT PATH   steps as JSON on stdin: send / expect (regex over the bytes received) / close
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
        send(2, st["send"].encode())
    elif "expect" in st:
        deadline = time.time() + st.get("timeout", 20); ok = False; closed = False
        while True:
            m = re.search(st["expect"], seen)
            if m: ok = True; seen = seen[m.end():]; break
            f = frame(deadline)
            if f is None: break
            op, p = f
            if op in (1, 2): seen += p.decode(errors="replace")
            elif op == 9: send(10, p)
            elif op == 8: closed = True; break
        out["results"].append({"expect": st["expect"], "ok": ok, "closed": closed, "tail": seen[-200:] if not ok else ""})
    elif "close" in st:
        send(8, b"\x03\xe8")
print(json.dumps(out)); s.close()
PY
ws() { python3 -I "$WS" "$PORT" "$1"; }
okall() { jget 'all(r["ok"] for r in d["results"]) and d["status"].split()[1] == "101"'; }
code() { jget 'd["status"].split()[1]'; }

R="$(ws "/v1/vms/$P/tunnel?port=22" <<<'[{"expect":"SSH-2\\.0-OpenSSH","timeout":20},{"send":"SSH-2.0-fluxvm-live-test\r\n"},{"expect":"curve25519","timeout":20}]')"
echo "     $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | jget 'd["status"].split()[1]')" == 101 ]] && ok "the tunnel upgrades (101) to the guest's port 22" || bad "tunnel: $R"
[[ "$(echo "$R" | jget 'd["results"][0]["ok"]')" == True ]] && ok "the guest's SSH banner arrives through the tunnel" || bad "no banner: $R"
[[ "$(echo "$R" | jget 'd["results"][1]["ok"]')" == True ]] && ok "bytes flow both ways: after our banner the guest sends its key exchange" || bad "no key exchange: $R"

R="$(ws "/v1/vms/$P/tunnel?port=0" <<<'[]')"; [[ "$(echo "$R" | code)" == 400 ]] && ok "port 0 is refused (400)" || bad "port 0: $R"
R="$(ws "/v1/vms/00000000-0000-0000-0000-000000000000/tunnel?port=22" <<<'[]')"; [[ "$(echo "$R" | code)" == 404 ]] && ok "an unknown VM is a 404" || bad "unknown VM: $R"
R="$(ws "/v1/vms/$P/tunnel?port=9" <<<'[{"expect":"SSH","timeout":8}]')"; echo "     closed port 9: $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | jget 'd["status"].split()[1] != "101" or not d["results"][0]["ok"]')" == True ]] && ok "a closed guest port carries no data (refused or closed)" || bad "closed port: $R"

curl -fs -X POST "$B/$P/stop" >/dev/null; for _ in $(seq 1 30); do [[ "$(status "$P")" == stopped ]] && break; sleep 2; done
R="$(ws "/v1/vms/$P/tunnel?port=22" <<<'[]')"; echo "     stopped VM: $(echo "$R" | cut -c1-300)"
[[ "$(echo "$R" | code)" == 409 ]] && ok "a stopped VM is refused (409)" || bad "stopped VM: $R"
curl -fs -X DELETE "$B/$P" >/dev/null || true

RESP="$(mkvm usernetvm '"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}')"
U="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create usernet: $RESP"
wait_agent "$U" && ok "usernet guest $U is up" || bad "usernet agent never answered"
echo "     usernet guest address from inside: $(announce "$U")"; sleep 5
R="$(ws "/v1/vms/$U/tunnel?port=22" <<<'[{"expect":"SSH-2\\.0","timeout":8}]')"
echo "     usernet guest (guest_ip=$(field "$U" guest_ip)): $(echo "$R" | cut -c1-300)"
# usernet is a user-space NAT: the host cannot reach the guest's address, only a published forward, so the tunnel says so.
[[ "$(echo "$R" | code)" == 409 && "$R" == *"apple.usernet.port_forwards"* ]] \
  && ok "a usernet guest is refused (409) with a pointer to apple.usernet.port_forwards" || bad "usernet guest: $R"
curl -fs -X DELETE "$B/$U" >/dev/null || true
echo "vz tunnel: PASS"
