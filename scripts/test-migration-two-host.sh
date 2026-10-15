#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Live QEMU migration between two real FluxVM hosts that mount the same shared
# storage at the same path. Drives each host's API (127.0.0.1:7788) over ssh.
#
# Every scenario starts a QEMU VM on a shared raw disk (tap netns, guest agent)
# on SRC, arms an adopt-mode receiver on DST, migrates over TCP, adopts, and
# checks the guest kept counting (no reboot) with the same machine type.
#
#   baseline   plain pre-copy
#   multifd    4 multifd channels
#   postcopy   post-copy
#   tls        x509 on both ends; no tls_hostname given, the relay defaults it
#   cancel     a throttled migration is cancelled mid-transfer: the source must
#              keep running, then a retry with default settings must complete
#   machine    a receiver for a machine type DST cannot boot is refused up front
#   oldqemu    SRC_OLD_QEMU=/usr/bin/qemu-system-x86_64 (older than DST's):
#              temporarily points SRC's /etc/fluxvm.toml at it, restores after
#
#   SRC=sus@175.110.122.71 DST=sus@80.79.5.173 DST_ADDR=80.79.5.173 \
#   [SCENARIOS="baseline multifd postcopy tls cancel machine"] \
#   [SRC_OLD_QEMU=/usr/bin/qemu-system-x86_64] \
#   ./scripts/test-migration-two-host.sh
#
# The image, kernel and initrd are copied into SHARED_ROOT on the source first
# so the destination sees them at the same path. Everything created is removed
# on exit. SCENARIOS defaults to every scenario, oldqemu only when SRC_OLD_QEMU is set.

set -euo pipefail
: "${SRC:?SRC=user@source-host}" "${DST:?DST=user@destination-host}" "${DST_ADDR:?DST_ADDR=address the source reaches the destination on}"
SHARED_ROOT=${SHARED_ROOT:-/srv/kairon-shared}
SRC_IMAGE=${SRC_IMAGE:-/var/lib/fluxvm/images/linux-agent.raw}
SRC_KERNEL=${SRC_KERNEL:-/var/lib/fluxvm/kernels/bionic-vmlinuz-4.15}
SRC_INITRD=${SRC_INITRD:-/var/lib/fluxvm/kernels/bionic-initrd-4.15}
if [[ -z "${SCENARIOS:-}" ]]; then
  SCENARIOS="baseline multifd postcopy tls cancel machine"
  [[ -n "${SRC_OLD_QEMU:-}" ]] && SCENARIOS="$SCENARIOS oldqemu"
fi
WORK="${SHARED_ROOT}/fluxvm-e2e-$$"
# One multiplexed connection per host: the script makes dozens of short calls and a
# sshd with MaxStartups/rate limits otherwise drops some of them.
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -o ControlMaster=auto -o ControlPersist=120
  -o "ControlPath=/tmp/fluxvm-e2e-%C")
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
SRC_VM=; DST_VM=; RECV=; QEMU_CHANGED=
restore_src_qemu() {
  [[ -n "$QEMU_CHANGED" ]] || return 0
  ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n cp /etc/fluxvm.toml.e2e-bak /etc/fluxvm.toml && sudo -n rm -f /etc/fluxvm.toml.e2e-bak && sudo -n systemctl restart fluxvm" \
    >/dev/null 2>&1 && echo "restored ${SRC}'s qemu_binary" || echo "WARNING: restore /etc/fluxvm.toml.e2e-bak on ${SRC} by hand" >&2
  QEMU_CHANGED=
}
drop() { # remove what the current scenario left behind
  [[ -n "$SRC_VM" ]] && rapi "$SRC" DELETE "/v1/vms/${SRC_VM}" >/dev/null 2>&1 || true
  [[ -n "$DST_VM" ]] && rapi "$DST" DELETE "/v1/vms/${DST_VM}" >/dev/null 2>&1 || true
  [[ -n "$RECV" ]] && rapi "$DST" DELETE "/v1/migration/receivers/${RECV}" >/dev/null 2>&1 || true
  SRC_VM=; DST_VM=; RECV=
}
cleanup() {
  drop
  restore_src_qemu
  ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n rm -rf '${WORK}'" >/dev/null 2>&1 || true
  for h in "$SRC" "$DST"; do ssh "${SSH_OPTS[@]}" -O exit "$h" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

uptime_via() { # host vm -> uptime seconds, waiting for the guest agent
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
machine_type_of() { # host vm -> label value, waiting for reconcile to record it
  local v=
  for _ in $(seq 1 40); do
    rapi "$1" GET "/v1/vms/$2"
    v=$(printf '%s' "$OUT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("labels",{}).get("fluxvm.dev/machine-type",""))')
    [[ -n "$v" ]] && break
    sleep 3
  done
  printf '%s' "$v"
}
create_source() { # name -> SRC_VM; each scenario boots its own copy of the disk
  local body disk="${WORK}/disk-$1.raw"
  ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n cp '${WORK}/disk.raw' '${disk}' && sudo -n chmod 0644 '${disk}'" \
    || fail "copying the disk for $1"
  body=$(cat <<EOF
{"name":"$1","backend":"qemu","image":"${disk}","storage":"shared","vcpus":1,"memory_mib":1024,
 "network":{"mode":"tap","netns":true,"mac":"$(mac)"},"agent":{"enabled":true},
 "kernel":"${WORK}/vmlinuz","initrd":"${WORK}/initrd","kernel_args":"console=ttyS0 root=/dev/vda rw"}
EOF
)
  rapi "$SRC" POST "/v1/vms?shared_takeover=true" "$body"
  [[ "$CODE" == 201 ]] || fail "create on source: $CODE $OUT"
  SRC_VM=$(printf '%s' "$OUT" | jget '["id"]')
}
arm_receiver() { # extra-json-object-fields (may be empty) -> RECV TOKEN PORT
  rapi "$SRC" GET "/v1/vms/${SRC_VM}"
  local rec=$OUT payload
  payload=$(REC="$rec" EXTRA="${1:-}" DST_ADDR="$DST_ADDR" python3 -c '
import json, os
d = {"record": json.loads(os.environ["REC"]), "listen_host": "0.0.0.0",
     "advertise_host": os.environ["DST_ADDR"], "listen_port": 0}
if os.environ["EXTRA"]:
    d.update(json.loads(os.environ["EXTRA"]))
print(json.dumps(d))')
  rapi "$DST" POST /v1/migration/receivers "$payload"
  [[ "$CODE" == 201 ]] || fail "receiver on destination: $CODE $OUT"
  RECV=$(printf '%s' "$OUT" | jget '["id"]'); TOKEN=$(printf '%s' "$OUT" | jget '["token"]')
  PORT=$(printf '%s' "$OUT" | jget '["uri"]'); PORT=${PORT##*:}
  rapi "$DST" POST "/v1/migration/receivers/${RECV}/activate" "{\"token\":\"${TOKEN}\"}"
  [[ "$CODE" == 200 ]] || fail "activate: $CODE $OUT"
}
start_migration() { # extra-json-object-fields (may be empty)
  local payload
  payload=$(EXTRA="${1:-}" DEST="tcp:${DST_ADDR}:${PORT}" python3 -c '
import json, os
d = {"destination": os.environ["DEST"]}
if os.environ["EXTRA"]:
    d.update(json.loads(os.environ["EXTRA"]))
print(json.dumps(d))')
  rapi "$SRC" POST "/v1/vms/${SRC_VM}/migration/start" "$payload"
  [[ "$CODE" == 200 ]] || fail "migration start: $CODE $OUT"
}
wait_phase() { # want-phase seconds -> sets phase
  phase=
  for _ in $(seq 1 "$2"); do
    rapi "$SRC" GET "/v1/vms/${SRC_VM}/migration/status"
    phase=$(printf '%s' "$OUT" | jget '["phase"]')
    [[ "$phase" == "$1" ]] && return 0
    [[ "$phase" == failed || "$phase" == cancelled ]] && [[ "$1" != "$phase" ]] && fail "migration ${phase}: $OUT"
    sleep 1
  done
  fail "migration did not reach ${1} (last: ${phase})"
}
finish_and_adopt() { # name up-before machine-type
  rapi "$SRC" POST "/v1/vms/${SRC_VM}/migration/finish"; [[ "$CODE" == 200 ]] || fail "$1: finish: $CODE $OUT"
  SRC_VM=
  rapi "$DST" POST "/v1/migration/receivers/${RECV}/adopt" "{\"token\":\"${TOKEN}\"}"
  [[ "$CODE" == 200 ]] || fail "$1: adopt: $CODE $OUT"
  DST_VM=${RECV}; RECV=
  [[ "$(printf '%s' "$OUT" | jget '["status"]')" == running ]] || fail "$1: adopted VM not running: $OUT"
  local up2 mt2
  up2=$(uptime_via "$DST" "$DST_VM") || fail "$1: guest agent silent after adopt"
  python3 -c "import sys; sys.exit(0 if $up2 > $2 else 1)" || fail "$1: guest rebooted (uptime $2 -> ${up2})"
  mt2=$(machine_type_of "$DST" "$DST_VM")
  [[ "$mt2" == "$3" ]] || fail "$1: machine type changed across the move: $3 -> ${mt2:-<none>}"
  ok "$1: guest kept running on the destination (uptime $2s -> ${up2}s), machine type ${3}"
}
# One scenario: start-extra receiver-extra
run_simple() { # name start-extra receiver-extra
  local name=$1 up1 mt
  create_source "mig-e2e-${name}"
  up1=$(uptime_via "$SRC" "$SRC_VM") || fail "$name: guest agent never answered on source"
  mt=$(machine_type_of "$SRC" "$SRC_VM"); [[ -n "$mt" ]] || fail "$name: no machine-type label on the source"
  arm_receiver "$3"
  start_migration "$2"
  wait_phase completed 240
  finish_and_adopt "$name" "$up1" "$mt"
  drop
}

# --- stage ---------------------------------------------------------------------
ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n mkdir -p '${WORK}' && sudo -n cp '${SRC_IMAGE}' '${WORK}/disk.raw' \
  && sudo -n cp '${SRC_KERNEL}' '${WORK}/vmlinuz' && sudo -n cp '${SRC_INITRD}' '${WORK}/initrd' \
  && sudo -n chmod 0644 '${WORK}'/*" || fail "staging into ${WORK}"
ssh "${SSH_OPTS[@]}" "$DST" "test -r '${WORK}/disk.raw' && test -r '${WORK}/vmlinuz'" \
  || fail "destination does not see ${WORK} (not the same shared mount?)"
ok "staged in ${WORK}, visible on both hosts"

for scenario in $SCENARIOS; do
  case "$scenario" in
  baseline) run_simple baseline '' '' ;;
  multifd)  run_simple multifd '{"multifd_channels":4}' '{"capabilities":{"multifd_channels":4}}' ;;
  postcopy) run_simple postcopy '{"mode":"post-copy"}' '{"capabilities":{"post_copy":true}}' ;;
  tls)
    # One CA-signed cert valid for the destination's address, used at both ends.
    ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n bash -s" <<CERTS || fail "tls: generating certificates"
set -e
cd '${WORK}'
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 1 -subj /CN=fluxvm-e2e-ca 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout key.pem -out req.csr -subj /CN=${DST_ADDR} 2>/dev/null
printf 'subjectAltName=IP:${DST_ADDR}\nextendedKeyUsage=serverAuth,clientAuth\n' > ext.cnf
openssl x509 -req -in req.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out cert.pem -days 1 -extfile ext.cnf 2>/dev/null
chmod 0644 ca.pem cert.pem key.pem
CERTS
    spec="{\"tls\":{\"ca_path\":\"${WORK}/ca.pem\",\"cert_path\":\"${WORK}/cert.pem\",\"key_path\":\"${WORK}/key.pem\"}}"
    run_simple tls "$spec" "$spec" ;;
  cancel)
    create_source mig-e2e-cancel
    up1=$(uptime_via "$SRC" "$SRC_VM") || fail "cancel: guest agent never answered on source"
    mt=$(machine_type_of "$SRC" "$SRC_VM")
    arm_receiver ''
    start_migration '{"bandwidth_mbps":1}'
    wait_phase active 60
    sleep 5
    rapi "$SRC" POST "/v1/vms/${SRC_VM}/migration/cancel"; [[ "$CODE" == 200 ]] || fail "cancel: $CODE $OUT"
    wait_phase cancelled 60
    up_after=$(uptime_via "$SRC" "$SRC_VM") || fail "cancel: source guest stopped answering after the cancel"
    python3 -c "import sys; sys.exit(0 if $up_after > $up1 else 1)" || fail "cancel: source guest did not keep running"
    rapi "$SRC" GET "/v1/vms/${SRC_VM}"
    [[ "$(printf '%s' "$OUT" | jget '["status"]')" == running ]] || fail "cancel: source VM not running: $OUT"
    ok "cancel: cancelled mid-transfer; the source guest is still running (uptime ${up1}s -> ${up_after}s)"
    rapi "$DST" DELETE "/v1/migration/receivers/${RECV}" >/dev/null; RECV=
    # A retry with default settings must not inherit the throttled attempt.
    arm_receiver ''
    start_migration ''
    wait_phase completed 120
    # Not the 1 Mbps of the cancelled attempt: that would need minutes for the guest's pages.
    total_ms=$(printf '%s' "$OUT" | jget '["total_time_ms"]')
    [[ "$total_ms" != None ]] && python3 -c "import sys; sys.exit(0 if $total_ms < 60000 else 1)" \
      || fail "cancel-retry: took ${total_ms} ms, so it likely inherited the earlier throttle"
    ok "cancel-retry: retry with default settings completed in ${total_ms} ms"
    finish_and_adopt cancel-retry "$up_after" "$mt"
    drop ;;
  machine)
    create_source mig-e2e-machine
    rapi "$SRC" GET "/v1/vms/${SRC_VM}"
    bad=$(printf '%s' "$OUT" | python3 -c '
import json, sys
r = json.load(sys.stdin)
r.setdefault("labels", {})["fluxvm.dev/machine-type"] = "pc-q35-99.9"
print(json.dumps({"record": r, "listen_host": "127.0.0.1", "listen_port": 0}))')
    rapi "$DST" POST /v1/migration/receivers "$bad"
    [[ "$CODE" == 4* || "$CODE" == 5* ]] || fail "machine: an unsupported machine type was accepted: $CODE $OUT"
    [[ "$OUT" == *"machine type"* ]] || fail "machine: refusal does not name the machine type: $OUT"
    ok "machine: unsupported machine type refused ($CODE)"
    drop ;;
  oldqemu)
    [[ -n "${SRC_OLD_QEMU:-}" ]] || fail "oldqemu needs SRC_OLD_QEMU=/path/to/older/qemu-system-x86_64"
    ssh "${SSH_OPTS[@]}" "$SRC" "test -x '${SRC_OLD_QEMU}' && '${SRC_OLD_QEMU}' --version | head -1" || fail "oldqemu: ${SRC_OLD_QEMU} not runnable on ${SRC}"
    ssh "${SSH_OPTS[@]}" "$SRC" "sudo -n cp /etc/fluxvm.toml /etc/fluxvm.toml.e2e-bak \
      && sudo -n sed -i -E 's|^qemu_binary *=.*|qemu_binary = \"${SRC_OLD_QEMU}\"|' /etc/fluxvm.toml \
      && sudo -n systemctl restart fluxvm" || fail "oldqemu: pointing ${SRC} at ${SRC_OLD_QEMU}"
    QEMU_CHANGED=1
    for _ in $(seq 1 30); do rapi "$SRC" GET /v1/vms; [[ "$CODE" == 200 ]] && break; sleep 2; done
    run_simple oldqemu '' ''
    restore_src_qemu
    for _ in $(seq 1 30); do rapi "$SRC" GET /v1/vms; [[ "$CODE" == 200 ]] && break; sleep 2; done ;;
  *) fail "unknown scenario ${scenario}" ;;
  esac
done
echo "test-migration-two-host: PASS (${SCENARIOS})"
