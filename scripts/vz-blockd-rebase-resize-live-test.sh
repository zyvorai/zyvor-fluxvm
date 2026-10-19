#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `fluxvm-blockd` rebase and resize against a `vz` guest's overlay extra disk:
# offline grow + rebase keep guest data; live rebase/resize while blockd is serving must be refused
# (shared flock held by the server) without disturbing the guest's data.
#   scripts/vz-blockd-rebase-resize-live-test.sh [image]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust and python3. Defaults to `debian-13`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-blockd.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7804}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
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

# 64 MiB base with a recognizable first block; overlay starts empty on top.
BASE_MIB=64
mkfile -n ${BASE_MIB}m "$T/base.raw"
dd if=/dev/urandom of="$T/base.raw" bs=1m count=1 conv=notrunc status=none
BASE_HEAD="$(dd if="$T/base.raw" bs=1m count=1 status=none | shasum -a 256 | awk '{print $1}')"
OVL="$T/disk.ovl"
NEW_MIB=96

cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-blockd"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
disk_json() {
  printf '{"name":"ovl","kind":"overlay","path":"%s","overlay":"%s"}' "$T/base.raw" "$OVL"
}
create_vm() {
  local name="$1"
  local req
  req="$(python3 -c '
import json,sys
print(json.dumps({
  "name": sys.argv[1], "backend": "vz", "image": sys.argv[2],
  "vcpus": 2, "memory_mib": 2048,
  "apple": {"extra_disks": [json.loads(sys.argv[3])]},
  "cloud_init": {"hostname": sys.argv[1], "user": "velora",
                 "ssh_authorized_keys": [sys.argv[4]]},
}))
' "$name" "$IMG" "$(disk_json)" "$PUB")"
  curl -sS -X POST "$B" -H 'Content-Type: application/json' -d "$req"
}

RESP="$(create_vm blockd1)"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
V="$B/$ID"
ok "VM $ID with overlay disk"
field() { curl -fs "$V" | python3 -c "import sys,json;print(json.load(sys.stdin).get('$1') or '')"; }
IP=""; for _ in $(seq 1 60); do IP="$(field guest_ip)"; [[ -n "$IP" ]] && break; sleep 3; done
[[ -n "$IP" ]] || bad "no guest_ip"
sshc() { ssh -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 "velora@$IP" "$@"; }
for _ in $(seq 1 30); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "ssh to $IP"
ok "SSH to the guest at $IP"
[[ -f "$OVL" ]] && ok "overlay file created at $OVL" || bad "no overlay file"

# Find the overlay disk by size (64 MiB).
DEV="$(sshc "lsblk -bdnr -o NAME,SIZE" | awk -v s=$((BASE_MIB * 1024 * 1024)) '$2 == s { print $1; exit }')"
[[ -n "$DEV" ]] && ok "guest sees ${BASE_MIB} MiB overlay as /dev/$DEV" || bad "no ${BASE_MIB} MiB disk (lsblk: $(sshc 'lsblk -bdn -o NAME,SIZE' | tr '\n' ';'))"

# Marker at a high offset so it lands in the overlay, not only the base head.
sshc "sudo dd if=/dev/zero of=/dev/$DEV bs=1M seek=32 count=1 conv=fsync status=none && echo MARKER-OK | sudo dd of=/dev/$DEV bs=1 seek=$((32*1024*1024)) conv=notrunc,fsync status=none"
GOT="$(sshc "sudo dd if=/dev/$DEV bs=1 skip=$((32*1024*1024)) count=9 status=none")"
[[ "$GOT" == MARKER-OK ]] && ok "guest wrote overlay marker" || bad "marker read '$GOT'"
AFTER_HEAD="$(dd if="$T/base.raw" bs=1m count=1 status=none | shasum -a 256 | awk '{print $1}')"
[[ "$AFTER_HEAD" == "$BASE_HEAD" ]] && ok "base.raw head unchanged after guest write" || bad "base head changed"

# --- live probes while blockd is serving: must refuse (shared flock) ---
set +e
LIVE_REBASE_OUT="$($BLOCKD rebase --overlay "$OVL" --parent "$T/base.raw" 2>&1)"
LIVE_REBASE_RC=$?
LIVE_RESIZE_OUT="$($BLOCKD resize --overlay "$OVL" --size ${NEW_MIB}M 2>&1)"
LIVE_RESIZE_RC=$?
set -e
[[ $LIVE_REBASE_RC -ne 0 ]] && echo "$LIVE_REBASE_OUT" | grep -qi 'in use' \
  && ok "live rebase refused while served: $(echo "$LIVE_REBASE_OUT" | tr '\n' ' ' | head -c 160)" \
  || bad "live rebase should refuse (rc=$LIVE_REBASE_RC out='$LIVE_REBASE_OUT')"
[[ $LIVE_RESIZE_RC -ne 0 ]] && echo "$LIVE_RESIZE_OUT" | grep -qi 'in use' \
  && ok "live resize refused while served: $(echo "$LIVE_RESIZE_OUT" | tr '\n' ' ' | head -c 160)" \
  || bad "live resize should refuse (rc=$LIVE_RESIZE_RC out='$LIVE_RESIZE_OUT')"
GOT="$(sshc "sudo dd if=/dev/$DEV bs=1 skip=$((32*1024*1024)) count=9 status=none")"
[[ "$GOT" == MARKER-OK ]] && ok "marker intact after refused live probes" || bad "marker lost after live probes: '$GOT'"

# --- offline resize ---
curl -fs -X DELETE "$V" >/dev/null || bad "delete failed"
# Wait for blockd to release the overlay.
for _ in $(seq 1 30); do ! pgrep -f "fluxvm-blockd serve .*${T}" >/dev/null 2>&1 && break; sleep 0.5; done
sleep 1
[[ -f "$OVL" ]] || bad "overlay gone after delete"
$BLOCKD resize --overlay "$OVL" --size ${NEW_MIB}M >"$T/resize.out" 2>&1 \
  && ok "offline resize to ${NEW_MIB} MiB: $(tr '\n' ' ' <"$T/resize.out")" \
  || bad "offline resize failed: $(cat "$T/resize.out")"

RESP="$(create_vm blockd2)"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "recreate after resize: $RESP"
V="$B/$ID"
IP=""; for _ in $(seq 1 60); do IP="$(field guest_ip)"; [[ -n "$IP" ]] && break; sleep 3; done
[[ -n "$IP" ]] || bad "no guest_ip after resize"
sshc() { ssh -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 "velora@$IP" "$@"; }
for _ in $(seq 1 30); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "ssh after resize"
DEV="$(sshc "lsblk -bdnr -o NAME,SIZE" | awk -v s=$((NEW_MIB * 1024 * 1024)) '$2 == s { print $1; exit }')"
[[ -n "$DEV" ]] && ok "guest sees resized ${NEW_MIB} MiB disk /dev/$DEV" || bad "no ${NEW_MIB} MiB disk after resize"
GOT="$(sshc "sudo dd if=/dev/$DEV bs=1 skip=$((32*1024*1024)) count=9 status=none")"
[[ "$GOT" == MARKER-OK ]] && ok "marker intact after offline resize" || bad "marker after resize: '$GOT'"

# --- offline rebase onto a content-identical copy of the base ---
curl -fs -X DELETE "$V" >/dev/null || true
for _ in $(seq 1 30); do ! pgrep -f "fluxvm-blockd serve .*${T}" >/dev/null 2>&1 && break; sleep 0.5; done
sleep 1
cp "$T/base.raw" "$T/base-v2.raw"
$BLOCKD rebase --overlay "$OVL" --parent "$T/base-v2.raw" >"$T/rebase.out" 2>&1 \
  && ok "offline rebase onto base-v2.raw: $(tr '\n' ' ' <"$T/rebase.out")" \
  || bad "offline rebase failed: $(cat "$T/rebase.out")"
# Remove the old base so a stale parent path would fail opens.
rm -f "$T/base.raw"
disk_json() {
  printf '{"name":"ovl","kind":"overlay","path":"%s","overlay":"%s"}' "$T/base-v2.raw" "$OVL"
}
RESP="$(create_vm blockd3)"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "recreate after rebase: $RESP"
V="$B/$ID"
IP=""; for _ in $(seq 1 60); do IP="$(field guest_ip)"; [[ -n "$IP" ]] && break; sleep 3; done
[[ -n "$IP" ]] || bad "no guest_ip after rebase"
sshc() { ssh -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 "velora@$IP" "$@"; }
for _ in $(seq 1 30); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "ssh after rebase"
DEV="$(sshc "lsblk -bdnr -o NAME,SIZE" | awk -v s=$((NEW_MIB * 1024 * 1024)) '$2 == s { print $1; exit }')"
[[ -n "$DEV" ]] || bad "no disk after rebase"
GOT="$(sshc "sudo dd if=/dev/$DEV bs=1 skip=$((32*1024*1024)) count=9 status=none")"
[[ "$GOT" == MARKER-OK ]] && ok "marker intact after offline rebase to base-v2" || bad "marker after rebase: '$GOT'"
# Base head still readable through the new parent (write a temp file: a bare pipe can yield an empty sha).
sshc "sudo dd if=/dev/$DEV of=/tmp/fluxvm-head.bin bs=1M count=1 status=none && sudo chmod a+r /tmp/fluxvm-head.bin" \
  || bad "could not read first MiB from /dev/$DEV after rebase"
HEAD_GUEST="$(sshc "sha256sum /tmp/fluxvm-head.bin" | awk '{print $1}')"
[[ "$HEAD_GUEST" == "$BASE_HEAD" ]] && ok "guest still reads base head through rebased parent" \
  || bad "base head via guest '$HEAD_GUEST' want '$BASE_HEAD'"

curl -fs -X DELETE "$V" >/dev/null || true
echo "vz blockd rebase/resize: PASS"
