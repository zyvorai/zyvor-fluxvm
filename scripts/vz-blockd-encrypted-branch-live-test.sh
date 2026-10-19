#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of an ENCRYPTED `fluxvm-blockd` overlay and its `branch` against a `vz` guest: the guest writes a
# marker to an encrypted overlay, the overlay is branched (offline, with the key), and the encrypted child serves the
# parent's data plus its own writes. Neither overlay file may hold the marker in clear; the base stays unchanged;
# branching while the parent is served must be refused.
#   scripts/vz-blockd-encrypted-branch-live-test.sh [image]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust and python3. Defaults to `debian-13`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-blockd.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7805}"; NBDPORT="${FLUXVM_LIVE_NBD_PORT:-10819}"
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
python3 -c 'import os;print(os.urandom(32).hex())' > "$T/key.hex"; chmod 600 "$T/key.hex"
PARENT="$T/parent.ovl"; CHILD="$T/child.ovl"
MARK_A="PARENT-MARKER-$(openssl rand -hex 8)"; MARK_B="CHILD-MARKER-$(openssl rand -hex 8)"

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

# --- 1. encrypted parent overlay: guest writes a marker ---
serve "disk=$T/base.raw,overlay=$PARENT,encrypt,keyfile=$T/key.hex"
boot encbr1
ok "guest sees the encrypted overlay as /dev/$DEV"
gwrite $OFF_A "$MARK_A"
[[ "$(gread $OFF_A "$MARK_A")" == "$MARK_A" ]] && ok "parent marker written and read back" || bad "parent marker readback"

# --- 2. branching while served must refuse ---
set +e; OUT="$($BLOCKD branch --from "$PARENT" --to "$CHILD" --key-file "$T/key.hex" 2>&1)"; RC=$?; set -e
[[ $RC -ne 0 ]] && echo "$OUT" | grep -qi 'in use' && ok "branch refused while served: $(echo "$OUT" | tr '\n' ' ' | head -c 140)" \
  || bad "branch should refuse while served (rc=$RC out='$OUT')"
[[ ! -e "$CHILD" ]] && ok "no child file left behind" || bad "child file exists after refused branch"

# --- 3. stop, branch offline with the key, wrong key must fail ---
stop_vm; stop_serve
WRONG="$T/wrong.hex"; python3 -c 'import os;print(os.urandom(32).hex())' > "$WRONG"
set +e; $BLOCKD branch --from "$PARENT" --to "$T/bad.ovl" --key-file "$WRONG" >"$T/wrong.out" 2>&1; RC=$?; set -e
[[ $RC -ne 0 ]] && ok "branch with the wrong key refused" || bad "branch with wrong key succeeded"
$BLOCKD branch --from "$PARENT" --to "$CHILD" --key-file "$T/key.hex" >"$T/branch.out" 2>&1 \
  && ok "offline encrypted branch: $(tr '\n' ' ' <"$T/branch.out")" || bad "branch failed: $(cat "$T/branch.out")"

# --- 4. serve the encrypted child: reads the parent's marker, takes its own writes ---
serve "disk=$T/base.raw,overlay=$CHILD,encrypt,keyfile=$T/key.hex"
boot encbr2
[[ "$(gread $OFF_A "$MARK_A")" == "$MARK_A" ]] && ok "child reads the parent's marker" || bad "child lost parent marker"
gwrite $OFF_B "$MARK_B"
[[ "$(gread $OFF_B "$MARK_B")" == "$MARK_B" ]] && ok "child marker written and read back" || bad "child marker readback"
stop_vm; stop_serve

# --- 5. on-disk checks: no plaintext anywhere, base unchanged, parent has no child data ---
for f in "$PARENT" "$CHILD"; do
  grep -aq "PARENT-MARKER" "$f" && bad "$(basename "$f") holds the parent marker in clear"
  grep -aq "CHILD-MARKER" "$f" && bad "$(basename "$f") holds the child marker in clear"
done
ok "neither overlay file holds a marker in clear"
[[ "$(shasum -a 256 "$T/base.raw" | awk '{print $1}')" == "$BASE_SHA" ]] && ok "base.raw unchanged" || bad "base.raw changed"

# --- 6. parent is frozen (read-only) and still holds only its own data ---
serve "disk=$T/base.raw,overlay=$PARENT,encrypt,keyfile=$T/key.hex,ro"
boot encbr3
[[ "$(gread $OFF_A "$MARK_A")" == "$MARK_A" ]] && ok "frozen parent still reads its marker" || bad "frozen parent lost marker"
[[ "$(gread $OFF_B "$MARK_B")" != "$MARK_B" ]] && ok "parent has none of the child's data" || bad "child data leaked into parent"
stop_vm; stop_serve
echo "vz blockd encrypted branch: PASS"
