#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of NBD structured replies between `fluxvm-blockd` and Virtualization.framework's NBD client: the guest
# reads and writes a plain overlay export and the server logs whether the client negotiated structured replies. The
# test passes either way as long as I/O is correct; the log line is the finding.
#   scripts/vz-blockd-structured-reply-live-test.sh [image]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust and python3. Defaults to `debian-13`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-blockd.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7806}"; NBDPORT="${FLUXVM_LIVE_NBD_PORT:-10829}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  [[ -n "${SPID:-}" ]] && kill "$SPID" 2>/dev/null || true
  pkill -f "fluxvm-blockd serve .*${T}" 2>/dev/null || true
  sleep 1; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -10 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -p fluxvm-blockd -j 4 2>&1 | tail -1
BLOCKD=./target/debug/fluxvm-blockd
[[ -x "$BLOCKD" ]] || bad "fluxvm-blockd missing"
IMG="${1:-debian-13}"
ssh-keygen -q -t ed25519 -N "" -f "$T/key" && PUB="$(cat "$T/key.pub")"


BASE_MIB=64
mkfile -n ${BASE_MIB}m "$T/base.raw"
dd if=/dev/urandom of="$T/base.raw" bs=1m count=1 conv=notrunc status=none
BASE_SHA="$(shasum -a 256 "$T/base.raw" | awk '{print $1}')"
OVL="$T/disk.ovl"
MARK_A="STRUCT-MARKER-$(openssl rand -hex 8)"

cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-blockd"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
serve() { # serve <export-spec>; blockd runs on the host, the guest attaches it as an nbd disk
  [[ -n "${SPID:-}" ]] && { kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; SPID=""; }
  $BLOCKD serve --export "$1" --listen 127.0.0.1:$NBDPORT > "$T/serve.log" 2>&1 & SPID=$!
  for _ in $(seq 1 20); do nc -z 127.0.0.1 $NBDPORT 2>/dev/null && return 0; sleep 0.5; done
  bad "blockd serve did not listen: $(cat "$T/serve.log")"
}
stop_serve() { kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; SPID=""; sleep 1; }
create_vm() {
  local req
  req="$(python3 -c '
import json,sys
print(json.dumps({
  "name": sys.argv[1], "backend": "vz", "image": sys.argv[2],
  "vcpus": 2, "memory_mib": 2048,
  "apple": {"extra_disks": [{"name": "enc", "kind": "nbd", "url": "nbd://127.0.0.1:%s/disk" % sys.argv[4]}]},
  "cloud_init": {"hostname": sys.argv[1], "user": "velora", "ssh_authorized_keys": [sys.argv[3]]},
}))
' "$1" "$IMG" "$PUB" "$NBDPORT")"
  curl -sS -X POST "$B" -H 'Content-Type: application/json' -d "$req"
}
field() { curl -fs "$V" | python3 -c "import sys,json;print(json.load(sys.stdin).get('$1') or '')"; }
boot() { # boot <name>: sets V, IP, sshc, DEV
  local resp; resp="$(create_vm "$1")"
  ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$resp" 2>/dev/null)" || bad "create $1: $resp"
  V="$B/$ID"; IP=""
  for _ in $(seq 1 60); do IP="$(field guest_ip)"; [[ -n "$IP" ]] && break; sleep 3; done
  [[ -n "$IP" ]] || bad "no guest_ip ($1)"
  sshc() { ssh -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 "velora@$IP" "$@"; }
  for _ in $(seq 1 30); do sshc true >/dev/null 2>&1 && break; sleep 3; done
  sshc true || bad "ssh ($1)"
  DEV="$(sshc "lsblk -bdnr -o NAME,SIZE" | awk -v s=$((BASE_MIB * 1024 * 1024)) '$2 == s { print $1; exit }')"
  [[ -n "$DEV" ]] || bad "no ${BASE_MIB} MiB disk in $1"
}
gread() { sshc "sudo dd if=/dev/$DEV bs=1 skip=$1 count=${#2} status=none"; }
gwrite() { sshc "printf %s '$2' | sudo dd of=/dev/$DEV bs=1 seek=$1 conv=notrunc,fsync status=none"; }
OFF_A=$((32*1024*1024)); OFF_B=$((40*1024*1024))
stop_vm() { curl -fs -X DELETE "$V" >/dev/null || true; sleep 2; }

# --- guest I/O through the export; the server log shows the negotiated reply mode ---
serve "disk=$T/base.raw,overlay=$OVL"
boot struct1
ok "guest sees the overlay export as /dev/$DEV"
gwrite $OFF_A "$MARK_A"
[[ "$(gread $OFF_A "$MARK_A")" == "$MARK_A" ]] && ok "marker written and read back" || bad "marker readback"
sshc "sudo dd if=/dev/$DEV of=/tmp/head.bin bs=1M count=1 status=none iflag=direct && sha256sum /tmp/head.bin" | awk '{print $1}' > "$T/head.sha"
[[ "$(cat "$T/head.sha")" == "$(dd if="$T/base.raw" bs=1m count=1 status=none | shasum -a 256 | awk '{print $1}')" ]] \
  && ok "direct 1 MiB read matches the base" || bad "base head mismatch"
LINE="$(grep 'client negotiated' "$T/serve.log" | sort -u | tr '\n' ' ')"
[[ -n "$LINE" ]] && ok "server log: $LINE" || bad "no negotiation line in serve.log: $(cat "$T/serve.log")"
stop_vm; stop_serve
echo "vz blockd structured reply: PASS"
