#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `apple.usernet` on a Linux guest: a root login with an injected SSH key (`cloud_init.user: root`), a TCP
# forward, a UDP forward, live guest traffic in `/v1/netobs/flows`, and the runner's plain refusal of a pause on a paused VM.
#   scripts/vz-usernet-live-test.sh [image]
# Needs Apple silicon macOS 14+, the Xcode command line tools, Rust, python3 and outbound access to example.com.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-usernet.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7796}"
TCPF="${FLUXVM_LIVE_TCP_FWD:-22731}"; UDPF="${FLUXVM_LIVE_UDP_FWD:-22732}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -5 "$T/daemon.log"; exit 1; }
skip() { echo "skip $*"; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
IMG="${1:-debian-13}"
ssh-keygen -q -t ed25519 -N "" -f "$T/key" && PUB="$(cat "$T/key.pub")"
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-usernet"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1"
REQ="{\"name\":\"unet\",\"backend\":\"vz\",\"image\":\"$IMG\",\"vcpus\":2,\"memory_mib\":2048,
  \"apple\":{\"usernet\":{\"port_forwards\":[
    {\"protocol\":\"tcp\",\"host_port\":$TCPF,\"guest_port\":22},
    {\"protocol\":\"udp\",\"host_port\":$UDPF,\"guest_port\":9999}]}},
  \"cloud_init\":{\"hostname\":\"unet\",\"user\":\"root\",\"ssh_authorized_keys\":[\"$PUB\"]}}"
RESP="$(curl -sS -X POST "$B/vms" -H 'Content-Type: application/json' -d "$REQ")"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
V="$B/vms/$ID"
field() { curl -fs "$V" | python3 -c "import sys,json;print(json.load(sys.stdin).get('$1') or '')"; }
[[ "$(field status)" == running ]] && ok "usernet VM is running" || bad "status $(field status)"
sshc() { ssh -i "$T/key" -p "$TCPF" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 root@127.0.0.1 "$@"; }
for _ in $(seq 1 60); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "no root SSH through the TCP forward 127.0.0.1:$TCPF"
ok "root login with the injected key through the usernet TCP forward"
[[ "$(sshc whoami)" == root ]] && ok "the session is root" || bad "whoami"
grep -q "SSH public key .*injected into" "$T/daemon.log" \
  && ok "the key was written into the root disk offline (daemon log)" \
  || bad "no offline key injection in the daemon log: $(grep -i -E 'inject' "$T/daemon.log" | head -3)"

# Live guest traffic reaches netobs.
sshc 'curl -sS -o /dev/null -m 20 http://example.com || true; getent hosts example.com >/dev/null || true'
sleep 2
FL="$(curl -fs "$B/netobs/flows?vm=$ID&closed=true&limit=100" || true)"
if grep -qi "example\|:80\|\"port\":80" <<<"$FL"; then ok "netobs: flows for the guest's own traffic"; else bad "netobs flows had no guest traffic: $(head -c 400 <<<"$FL")"; fi
DNS="$(curl -fs "$B/netobs/dns?vm=$ID&limit=50" || true)"
grep -qi example <<<"$DNS" && ok "netobs: DNS transaction for example.com" || bad "netobs dns: $(head -c 300 <<<"$DNS")"

# UDP forward: a listener in the guest echoes one datagram.
if sshc 'command -v python3 >/dev/null'; then
  sshc "nohup python3 -c 'import socket;s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind((\"0.0.0.0\",9999));d,a=s.recvfrom(64);s.sendto(b\"echo:\"+d,a)' >/dev/null 2>&1 &" || true
  sleep 1
  GOT="$(python3 - "$UDPF" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.settimeout(8)
s.sendto(b"ping-udp", ("127.0.0.1", int(sys.argv[1])))
try: print(s.recvfrom(64)[0].decode())
except Exception as e: print("ERR", e)
PY
)"
  [[ "$GOT" == echo:ping-udp ]] && ok "UDP forward: the guest answered through 127.0.0.1:$UDPF" || bad "UDP forward: $GOT"
else
  skip "UDP forward: no python3 in the guest image"
fi

# Control preconditions.
curl -fs -X POST "$V/pause" >/dev/null && [[ "$(field status)" == paused ]] && ok "pause" || bad "pause"
MSG="$(curl -sS -X POST "$V/pause" || true)"
if grep -qi "cannot pause\|not running\|paused" <<<"$MSG"; then ok "a second pause is refused: $(head -c 160 <<<"$MSG")"; else bad "second pause: $MSG"; fi
curl -fs -X POST "$V/resume" >/dev/null && [[ "$(field status)" == running ]] && ok "resume" || bad "resume"
curl -fs -X DELETE "$V" >/dev/null || true
echo "vz usernet checks passed"
