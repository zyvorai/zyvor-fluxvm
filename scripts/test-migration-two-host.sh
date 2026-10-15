#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Live QEMU migration between two real FluxVM hosts that mount the same shared
# storage at the same path. Drives each host's API (127.0.0.1:7788) over ssh:
#
#   1. source: QEMU VM on a shared raw disk (tap netns, guest agent); its
#      concrete machine type must be recorded as a label.
#   2. destination: adopt-mode receiver from the source record, activate.
#   3. source: migration/start to the destination over TCP, poll, finish.
#   4. destination: adopt. The guest's uptime must keep counting (no reboot)
#      and the adopted VM must report the same machine type.
#   5. a second migration is attempted against a receiver whose machine type
#      the destination's QEMU cannot boot, and must be refused up front.
#
#   SRC=sus@175.110.122.71 DST=sus@80.79.5.173 DST_ADDR=80.79.5.173 \
#   SHARED_ROOT=/srv/kairon-shared \
#   SRC_IMAGE=/var/lib/fluxvm/images/linux-agent.raw \
#   SRC_KERNEL=/var/lib/fluxvm/kernels/bionic-vmlinuz-4.15 \
#   SRC_INITRD=/var/lib/fluxvm/kernels/bionic-initrd-4.15 \
#   ./scripts/test-migration-two-host.sh
#
# The image, kernel and initrd are copied into SHARED_ROOT on the source host
# first so the destination sees them at the same path. Everything created is
# removed on exit.

set -euo pipefail
: "${SRC:?SRC=user@source-host}" "${DST:?DST=user@destination-host}" "${DST_ADDR:?DST_ADDR=address the source reaches the destination on}"
SHARED_ROOT=${SHARED_ROOT:-/srv/kairon-shared}
SRC_IMAGE=${SRC_IMAGE:-/var/lib/fluxvm/images/linux-agent.raw}
SRC_KERNEL=${SRC_KERNEL:-/var/lib/fluxvm/kernels/bionic-vmlinuz-4.15}
SRC_INITRD=${SRC_INITRD:-/var/lib/fluxvm/kernels/bionic-initrd-4.15}
WORK="${SHARED_ROOT}/fluxvm-e2e-$$"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15)
CODE=; OUT=
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok   $*"; }
rapi() { # host method path [body] -> OUT (body), CODE (HTTP status)
  local raw
  raw=$(printf '%s' "${4:-}" | ssh "${SSH_OPTS[@]}" "$1" \
    "curl -s -w '\n%{http_code}' -X $2 http://127.0.0.1:7788$3 -H 'Content-Type: application/json' ${4:+-d @-}")
  CODE=${raw##*$'\n'}
  OUT=${raw%$'\n'*}
}
jget() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval("d"+sys.argv[1]))' "$1"; }
mac() { printf '52:54:00:%02x:%02x:%02x' $((RANDOM % 256)) $((RANDOM % 256)) $((RANDOM % 256)); }
SRC_VM=; DST_VM=; RECV=
cleanup() {
  [[ -n "$SRC_VM" ]] && rapi "$SRC" DELETE "/v1/vms/${SRC_VM}" >/dev/null 2>&1 || true
  [[ -n "$DST_VM" ]] && rapi "$DST" DELETE "/v1/vms/${DST_VM}" >/dev/null 2>&1 || true
  [[ -n "$RECV" ]] && rapi "$DST" DELETE "/v1/migration/receivers/${RECV}" >/dev/null 2>&1 || true
  ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n rm -rf '${WORK}'" >/dev/null 2>&1 || true
}
trap cleanup EXIT
uptime_via() { # host vm -> uptime seconds, waiting for the guest agent
  local out
  for _ in $(seq 1 90); do
    rapi "$1" POST "/v1/vms/$2/agent" '{"command":"cat /proc/uptime","timeout_seconds":5}'
    if [[ "$CODE" == 200 ]]; then
      printf '%s' "$OUT" | python3 -c '
import json, sys
def find(v):
    if isinstance(v, dict):
        if isinstance(v.get("stdout"), str):
            return v["stdout"]
        for x in v.values():
            r = find(x)
            if r is not None:
                return r
print(float(find(json.load(sys.stdin)).split()[0]))'
      return 0
    fi
    sleep 2
  done
  return 1
}

# --- stage ---------------------------------------------------------------------
ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n mkdir -p '${WORK}' && sudo -n cp '${SRC_IMAGE}' '${WORK}/disk.raw' \
  && sudo -n cp '${SRC_KERNEL}' '${WORK}/vmlinuz' && sudo -n cp '${SRC_INITRD}' '${WORK}/initrd' \
  && sudo -n chmod 0644 '${WORK}'/*" || fail "staging into ${WORK}"
ssh "${SSH_OPTS[@]}" "$DST" "test -r '${WORK}/disk.raw' && test -r '${WORK}/vmlinuz'" \
  || fail "destination does not see ${WORK} (not the same shared mount?)"
ok "staged in ${WORK}, visible on both hosts"

# --- 1. source VM -----------------------------------------------------------------
body=$(cat <<EOF
{"name":"mig-e2e","backend":"qemu","image":"${WORK}/disk.raw","storage":"shared","vcpus":1,"memory_mib":1024,
 "network":{"mode":"tap","netns":true,"mac":"$(mac)"},"agent":{"enabled":true},
 "kernel":"${WORK}/vmlinuz","initrd":"${WORK}/initrd","kernel_args":"console=ttyS0 root=/dev/vda rw"}
EOF
)
rapi "$SRC" POST /v1/vms "$body"; [[ "$CODE" == 201 ]] || fail "create on source: $CODE $OUT"
SRC_VM=$(printf '%s' "$OUT" | jget '["id"]')
up1=$(uptime_via "$SRC" "$SRC_VM") || fail "guest agent never answered on source"
ok "source VM ${SRC_VM} up ${up1}s"
mt=
for _ in $(seq 1 40); do   # reconcile records the machine type on its next pass
  rapi "$SRC" GET "/v1/vms/${SRC_VM}"
  mt=$(printf '%s' "$OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("labels",{}).get("fluxvm.dev/machine-type",""))')
  [[ -n "$mt" ]] && break
  sleep 3
done
[[ -n "$mt" ]] || fail "source never recorded a fluxvm.dev/machine-type label"
ok "source machine type recorded: ${mt}"

# --- 2. receiver on the destination -------------------------------------------------
rapi "$SRC" GET "/v1/vms/${SRC_VM}"; record=$OUT
rapi "$DST" POST /v1/migration/receivers "{\"record\":${record},\"listen_host\":\"0.0.0.0\",\"advertise_host\":\"${DST_ADDR}\",\"listen_port\":0}"
[[ "$CODE" == 201 ]] || fail "receiver on destination: $CODE $OUT"
RECV=$(printf '%s' "$OUT" | jget '["id"]'); TOKEN=$(printf '%s' "$OUT" | jget '["token"]')
URI=$(printf '%s' "$OUT" | jget '["uri"]')
PORT=${URI##*:}
rapi "$DST" POST "/v1/migration/receivers/${RECV}/activate" "{\"token\":\"${TOKEN}\"}"
[[ "$CODE" == 200 ]] || fail "activate: $CODE $OUT"
ok "receiver ${RECV} armed on ${DST_ADDR}:${PORT}"

# --- 3. migrate ------------------------------------------------------------------
rapi "$SRC" POST "/v1/vms/${SRC_VM}/migration/start" "{\"destination\":\"tcp:${DST_ADDR}:${PORT}\"}"
[[ "$CODE" == 200 ]] || fail "migration start: $CODE $OUT"
phase=
for _ in $(seq 1 180); do
  rapi "$SRC" GET "/v1/vms/${SRC_VM}/migration/status"
  phase=$(printf '%s' "$OUT" | jget '["phase"]')
  [[ "$phase" == completed ]] && break
  [[ "$phase" == failed || "$phase" == cancelled ]] && fail "migration ${phase}: $OUT"
  sleep 1
done
[[ "$phase" == completed ]] || fail "migration did not complete (${phase})"
ok "migration completed over TCP"
rapi "$SRC" POST "/v1/vms/${SRC_VM}/migration/finish"; [[ "$CODE" == 200 ]] || fail "finish: $CODE $OUT"
SRC_VM=

# --- 4. adopt on the destination -------------------------------------------------
rapi "$DST" POST "/v1/migration/receivers/${RECV}/adopt" "{\"token\":\"${TOKEN}\"}"
[[ "$CODE" == 200 ]] || fail "adopt: $CODE $OUT"
DST_VM=${RECV}; RECV=
[[ "$(printf '%s' "$OUT" | jget '["status"]')" == running ]] || fail "adopted VM not running: $OUT"
up2=$(uptime_via "$DST" "$DST_VM") || fail "guest agent silent after adopt"
python3 -c "import sys; sys.exit(0 if $up2 > $up1 else 1)" || fail "guest rebooted (uptime ${up1} -> ${up2})"
ok "adopted ${DST_VM} on the destination: same guest (uptime ${up1}s -> ${up2}s)"
for _ in $(seq 1 40); do
  rapi "$DST" GET "/v1/vms/${DST_VM}"
  mt2=$(printf '%s' "$OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("labels",{}).get("fluxvm.dev/machine-type",""))')
  [[ -n "$mt2" ]] && break
  sleep 3
done
[[ "$mt2" == "$mt" ]] || fail "machine type changed across the move: ${mt} -> ${mt2:-<none>}"
ok "destination runs the same machine type (${mt})"

# --- 5. a machine type the destination cannot boot is refused up front ------------
bad=$(printf '%s' "$record" | python3 -c '
import json, sys
r = json.load(sys.stdin)
r.setdefault("labels", {})["fluxvm.dev/machine-type"] = "pc-q35-99.9"
print(json.dumps(r))')
rapi "$DST" POST /v1/migration/receivers "{\"record\":${bad},\"listen_host\":\"127.0.0.1\",\"listen_port\":0}"
[[ "$CODE" == 4* || "$CODE" == 5* ]] || fail "an unsupported machine type was accepted: $CODE $OUT"
[[ "$OUT" == *"machine type"* ]] || fail "refusal does not name the machine type: $OUT"
ok "unsupported machine type refused ($CODE)"
echo "test-migration-two-host: PASS"
