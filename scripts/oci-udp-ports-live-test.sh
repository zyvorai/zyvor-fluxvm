#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of published UDP (and TCP) ports on a container sandbox (docs/oci-sandboxes.md, "Published ports"). Starts a
# daemon, runs an alpine container with `nc` echo servers on a UDP and a TCP port, then from the Mac sends a datagram and
# a TCP line to the published 127.0.0.1 ports and checks they come back. Also checks that a second sandbox cannot take a
# host port that is in use, that the same number can be held once per protocol, and that a TCP-only sandbox still works.
#   scripts/oci-udp-ports-live-test.sh              # uses ~/Library/Application Support/FluxVM/oci/boot
#   FLUXVM_OCI_BOOT_DIR=dist/oci-boot scripts/oci-udp-ports-live-test.sh
# Needs Apple silicon, Rust, network access to Docker Hub (first run), and the boot artifacts (`oci-kernel`, `oci-initrd`).
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
BOOT="$(cd "${FLUXVM_OCI_BOOT_DIR:-$HOME/Library/Application Support/FluxVM/oci/boot}" 2>/dev/null && pwd)" \
  || { echo "set FLUXVM_OCI_BOOT_DIR to the directory with oci-kernel and oci-initrd" >&2; exit 2; }
[[ -f "$BOOT/oci-kernel" && -f "$BOOT/oci-initrd" ]] || { echo "$BOOT has no oci-kernel / oci-initrd" >&2; exit 2; }
ALPINE="${FLUXVM_OCI_ALPINE:-alpine:3.22}"
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-oci-udp.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7797}"
UDP_HOST="${FLUXVM_LIVE_UDP_PORT:-15353}"; TCP_HOST="${FLUXVM_LIVE_TCP_PORT:-18080}"
cleanup() { [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -5 "$T/daemon.log"; exit 1; }
json() { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

cargo build -p fluxctl -p fluxvm-vz-switch -j 4 2>&1 | tail -1
FLUXCTL=./target/debug/fluxctl
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-oci-udp"

[apple]
oci_kernel = "$BOOT/oci-kernel"
oci_initrd = "$BOOT/oci-initrd"
EOT
$FLUXCTL --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"
SERVER="http://127.0.0.1:$PORT"; SB="$SERVER/v1/sandboxes"
create() { curl -sS -X POST "$SB" -H 'Content-Type: application/json' -d "$1" -w '\n%{http_code}'; }

START=$SECONDS
curl -fsS -X POST "$SERVER/v1/oci/images" -H 'Content-Type: application/json' -d "{\"image\":\"$ALPINE\"}" > "$T/pull.json" || bad "pull $ALPINE"
ok "pulled $ALPINE ($((SECONDS-START))s)"

# UDP and TCP echo servers inside the container (busybox nc, one connection each).
SERVE='while true; do nc -l -u -p 5353 -e /bin/cat; done & while true; do nc -l -p 8080 -e /bin/cat; done & wait'
BODY="$(python3 - "$ALPINE" "$UDP_HOST" "$TCP_HOST" "$SERVE" <<'PY'
import json, sys
image, udp, tcp, serve = sys.argv[1:5]
print(json.dumps({"name": "udp-echo", "ttl_seconds": 600, "oci": {
    "image": image, "ports": [f"{udp}:5353/udp", f"{tcp}:8080"], "command": ["sh", "-c", serve]}}))
PY
)"
RESP="$(create "$BODY")"; CODE="${RESP##*$'\n'}"; RESP="${RESP%$'\n'*}"
[[ "$CODE" == 201 || "$CODE" == 200 ]] || bad "create with a udp port: $CODE $RESP"
ID="$(json 'd["id"]' <<<"$RESP")"
ok "created a sandbox publishing ${UDP_HOST}/udp and ${TCP_HOST}/tcp ($ID)"

# The record says which network card it got: a UDP port means the userspace gateway.
REC="$(curl -fsS "$SERVER/v1/vms/$ID")"
[[ "$(json '"usernet" in (d["request"].get("apple") or {}) and d["request"]["apple"]["usernet"] is not None' <<<"$REC")" == True ]] \
  && ok "the sandbox uses apple.usernet (the plain NAT relays TCP only)" || bad "no apple.usernet on the record"

# Let the echo servers come up, then talk to them from the Mac.
sleep 3
RESULT="$(python3 - "$UDP_HOST" "$TCP_HOST" <<'PY'
import socket, sys, time
udp, tcp = int(sys.argv[1]), int(sys.argv[2])
out = []
# UDP: retry a few times, the guest's server may take a moment to bind.
got = None
for _ in range(10):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(2)
    s.sendto(b"ping-udp\n", ("127.0.0.1", udp))
    try:
        got, _ = s.recvfrom(1024); break
    except socket.timeout:
        time.sleep(1)
    finally:
        s.close()
out.append("udp=" + (got.decode().strip() if got else "NONE"))
got = None
for _ in range(10):
    try:
        c = socket.create_connection(("127.0.0.1", tcp), timeout=3)
        c.sendall(b"ping-tcp\n"); c.settimeout(3); got = c.recv(1024); c.close(); break
    except OSError:
        time.sleep(1)
out.append("tcp=" + (got.decode().strip() if got else "NONE"))
print(" ".join(out))
PY
)"
[[ "$RESULT" == *"udp=ping-udp"* ]] && ok "a UDP datagram to 127.0.0.1:${UDP_HOST} was echoed by the container" || bad "udp echo: $RESULT"
[[ "$RESULT" == *"tcp=ping-tcp"* ]] && ok "a TCP line to 127.0.0.1:${TCP_HOST} was echoed by the container" || bad "tcp echo: $RESULT"

# A second sandbox cannot take a host port that is already published, per protocol...
CLASH="$(create "{\"name\":\"clash\",\"oci\":{\"image\":\"$ALPINE\",\"command\":[\"sleep\",\"60\"],\"ports\":[\"${UDP_HOST}:5353/udp\"]}}")"
CCODE="${CLASH##*$'\n'}"
[[ "$CCODE" == 4* || "$CCODE" == 5* ]] && ok "the same UDP host port is refused while it is published ($CCODE)" || bad "clash was accepted: $CLASH"
[[ "$CLASH" == *"already published"* ]] && ok "the refusal says why" || bad "refusal text: $CLASH"

# ...but a TCP port with the UDP port's number is a different listener.
SAME="$(create "{\"name\":\"same-number\",\"ttl_seconds\":120,\"oci\":{\"image\":\"$ALPINE\",\"command\":[\"sleep\",\"60\"],\"ports\":[\"${UDP_HOST}:9000\"]}}")"
SCODE="${SAME##*$'\n'}"
[[ "$SCODE" == 201 || "$SCODE" == 200 ]] && ok "TCP ${UDP_HOST} is free while UDP ${UDP_HOST} is taken" || bad "same number, other protocol refused: $SAME"

# Deleting the sandbox releases the host ports.
curl -fsS -X DELETE "$SERVER/v1/vms/$ID" >/dev/null || bad "delete"
sleep 2
python3 - "$UDP_HOST" <<'PY' && ok "the UDP host port is released after delete" || bad "the UDP host port is still bound"
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
finally:
    s.close()
PY
echo "PASS"
