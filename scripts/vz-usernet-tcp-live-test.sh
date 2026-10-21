#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `apple.usernet` TCP on a Linux guest: host-to-guest port forwards (SSH and an HTTP server), TCP egress
# through the userspace gateway, and `deny_ports` refusing a port while another still connects.
#   scripts/vz-usernet-tcp-live-test.sh [image]
# Needs Apple silicon macOS, the Xcode command line tools, Rust, python3 and internet access (HTTPS to example.com and
# a TCP connect to 1.1.1.1). Defaults to the built-in `debian-13`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-unettcp.XXXXXX")"
PORT="${FLUXVM_LIVE_PORT:-7799}"; SSH_FWD="${FLUXVM_LIVE_SSH_FWD:-18022}"; HTTP_FWD="${FLUXVM_LIVE_HTTP_FWD:-18080}"
cleanup() {
  # FLUXVM_LIVE_KEEP=1 leaves the daemon, guest and state dir up for debugging; stop them yourself afterwards.
  [[ -n "${FLUXVM_LIVE_KEEP:-}" ]] && { echo "kept running: $T (daemon pid ${DPID:-?})"; return; }
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  pkill -f "fluxvm-usernet .*$T" 2>/dev/null || true
  sleep 1; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -5 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -p fluxvm-usernet -j 4 2>&1 | tail -1
IMG="${1:-debian-13}"
ssh-keygen -q -t ed25519 -N "" -f "$T/key" && PUB="$(cat "$T/key.pub")"
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-unettcp"
EOT
FLUXVM_USERNET="$PWD/target/debug/fluxvm-usernet" \
  ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"
B="http://127.0.0.1:$PORT/v1/vms"

REQ="{\"name\":\"unettcp\",\"backend\":\"vz\",\"image\":\"$IMG\",\"vcpus\":2,\"memory_mib\":2048,
  \"apple\":{\"usernet\":{\"deny_ports\":[25],
    \"port_forwards\":[{\"protocol\":\"tcp\",\"host_addr\":\"127.0.0.1\",\"host_port\":$SSH_FWD,\"guest_port\":22},
                      {\"protocol\":\"tcp\",\"host_addr\":\"127.0.0.1\",\"host_port\":$HTTP_FWD,\"guest_port\":8080}]}},
  \"cloud_init\":{\"hostname\":\"unettcp\",\"user\":\"velora\",\"ssh_authorized_keys\":[\"$PUB\"]}}"
RESP="$(curl -sS -X POST "$B" -H 'Content-Type: application/json' -d "$REQ")"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"

# The legacy guest_ip discovery does not work on usernet, so the SSH forward is also the readiness probe.
# The forward accepts TCP before the guest's sshd is up, and ConnectTimeout does not cover that stall, so every attempt
# gets a hard alarm (perl is on every macOS; `timeout` is not).
sshc() { perl -e 'alarm shift; exec @ARGV' "${SSH_TIMEOUT:-30}" ssh -n -p "$SSH_FWD" -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
  -o BatchMode=yes -o ConnectTimeout=8 velora@127.0.0.1 "$@"; }
for _ in $(seq 1 60); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "no SSH through the 127.0.0.1:$SSH_FWD forward"
ok "host-to-guest forward: SSH login through 127.0.0.1:$SSH_FWD"

CODE="$(sshc 'curl -sS -m 20 -o /dev/null -w %{http_code} https://example.com')" || CODE="curl-failed"
[[ "$CODE" == 200 ]] && ok "TCP egress: HTTPS to example.com returned 200" || bad "HTTPS egress gave '$CODE'"

# deny_ports [25] must refuse fast, while another port on the same host connects.
tcp() { sshc "timeout 8 bash -c 'exec 3<>/dev/tcp/$1/$2' 2>/dev/null && echo open || echo closed"; }
[[ "$(tcp 1.1.1.1 443)" == open ]] && ok "deny_ports: 1.1.1.1:443 connects" || bad "1.1.1.1:443 did not connect"
START=$SECONDS
[[ "$(tcp 1.1.1.1 25)" == closed ]] && ok "deny_ports: 1.1.1.1:25 refused in $((SECONDS - START))s" || bad "port 25 was not refused"

# A second forward carrying real payload: an HTTP server in the guest, fetched from the host. `&` backgrounds a whole `a && b && c` list, and
# that wrapper shell would keep the ssh session's pipes, so ssh never returns: separate the server with `;` and redirect it.
sshc 'mkdir -p /tmp/www && echo fluxvm-forward-ok > /tmp/www/index.html; nohup python3 -m http.server 8080 --directory /tmp/www >/dev/null 2>&1 </dev/null &' \
  || { sshc 'command -v python3; python3 --version' 2>&1 | sed 's/^/     guest: /'; bad "starting the guest HTTP server"; }
BODY=""; for _ in $(seq 1 10); do BODY="$(curl -fs -m 5 "http://127.0.0.1:$HTTP_FWD/" 2>/dev/null || true)"; [[ -n "$BODY" ]] && break; sleep 1; done
[[ "$BODY" == fluxvm-forward-ok* ]] && ok "host-to-guest forward: HTTP body through 127.0.0.1:$HTTP_FWD" || bad "forward returned '$BODY'"

# The denial must show in the gateway log and the flow table.
LOG="$(find "$T/state" -name usernet.log -print -quit)"
[[ -n "$LOG" ]] && grep -qi "deny" "$LOG" && ok "denial recorded in usernet.log" || bad "no deny entry in usernet.log (${LOG:-none})"
curl -fs -X DELETE "$B/$ID" >/dev/null || true
echo "usernet TCP and forwards: PASS"
