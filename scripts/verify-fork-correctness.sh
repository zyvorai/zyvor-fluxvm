#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Checks that fork does the right thing, not how fast it is. Against a running daemon (flux-vm
# backend, fluxvm_engine = "kvm") and an image whose guest agent is current (see
# scripts/build-guest-agent-static.sh), it verifies that a fork child:
#   - has its own hostname and machine-id (identity reset applied),
#   - keeps the parent's disk state and still has the parent's running process (memory state),
#   - is isolated from the parent in both directions after the fork,
#   - works with the rotated agent token, and keeps working after the parent is deleted;
# that `count: 3` yields three distinct identities; that a failed multi-child fork (optional, needs an
# image with an OLD agent) leaves no VM records, instance directories or VMM processes behind; and that
# repeated fork + delete does not leak.
#
#   FLUXVM_API=http://127.0.0.1:7788 IMAGE=/var/lib/fluxvm/images/new-agent.ext4 \
#     [OLD_IMAGE=/var/lib/fluxvm/images/old-agent.ext4] [STATE_DIR=/var/lib/fluxvm] \
#     ./scripts/verify-fork-correctness.sh
#
# Needs curl and jq. It only deletes VMs it created. The leak checks compare counts before and after, so
# run it while nothing else creates or deletes VMs on that daemon. Exit status is the number of failed checks
# (0 = all passed).
set -uo pipefail

API="${FLUXVM_API:-http://127.0.0.1:7788}"
IMAGE="${IMAGE:?set IMAGE to a rootfs whose guest agent is current}"
OLD_IMAGE="${OLD_IMAGE:-}"
STATE_DIR="${STATE_DIR:-}"
MEM_MIB="${MEM_MIB:-256}"
LOOPS="${LOOPS:-5}"
LEAK_LIMIT_KIB="${LEAK_LIMIT_KIB:-262144}"
AUTH_HDR=()
[[ -n "${FLUXVM_TOKEN:-}" ]] && AUTH_HDR=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")

for tool in curl jq; do
  command -v "$tool" >/dev/null || { echo "verify-fork-correctness: $tool is required" >&2; exit 2; }
done
curl -sf "${API}/readyz" "${AUTH_HDR[@]}" >/dev/null || { echo "verify-fork-correctness: ${API}/readyz is not up" >&2; exit 2; }

PASS=0
FAIL=0
CREATED=()

# shellcheck disable=SC2329  # called by the EXIT trap
cleanup() {
  local id
  for id in "${CREATED[@]:-}"; do
    [[ -n "$id" ]] && curl -s -m 60 -X DELETE "${API}/v1/vms/${id}" "${AUTH_HDR[@]}" -o /dev/null || true
  done
}
trap cleanup EXIT

eq() { # name actual expected: PASS when equal, and shows what was actually returned if not
  chk "$1" "$([[ "$2" == "$3" ]] && echo 1 || echo 0)" "got '${2:0:120}', want '$3'"
}

chk() { # name ok(1/0) [evidence]
  if [[ "$2" == 1 ]]; then
    PASS=$((PASS + 1)); echo "PASS  $1"
  else
    FAIL=$((FAIL + 1)); echo "FAIL  $1${3:+   [$3]}"
  fi
}

post() { curl -s -m "${3:-120}" -X POST "${API}$1" "${AUTH_HDR[@]}" -H 'Content-Type: application/json' -d "$2"; }

create() { # name image -> id
  local id
  id=$(post /v1/sandboxes "$(jq -nc --arg n "$1" --arg i "$2" --argjson m "$MEM_MIB" \
    '{name:$n,spec:{name:$n,backend:"flux-vm",image:$i,vcpus:1,memory_mib:$m,network:{mode:"none"}}}')" 180 | jq -r '.id // empty')
  echo "$id"
}

ex() { # vm command -> stdout (or the error text)
  post "/v1/vms/$1/agent" "$(jq -nc --arg c "$2" '{command:$c,timeout_seconds:25}')" 40 | jq -r '.stdout // .error // "NOEXEC"'
}

wait_agent() { # vm
  local _
  for _ in $(seq 1 60); do
    [[ "$(ex "$1" 'echo up')" == up ]] && return 0
    sleep 1
  done
  return 1
}

forget() { # drop an id from CREATED after deleting it
  local keep=() x
  for x in "${CREATED[@]:-}"; do [[ "$x" != "$1" ]] && keep+=("$x"); done
  CREATED=("${keep[@]:-}")
}

del() { curl -s -m 60 -X DELETE "${API}/v1/vms/$1" "${AUTH_HDR[@]}" -o /dev/null; forget "$1"; }
vm_count() { curl -s "${API}/v1/vms" "${AUTH_HDR[@]}" | jq 'if type=="array" then length else (.items|length) end'; }
inst_dirs() { [[ -n "$STATE_DIR" ]] && find "$STATE_DIR/instances" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l || echo 0; }
vmm_procs() { [[ -n "$STATE_DIR" ]] && pgrep -f "${STATE_DIR}/instances" | wc -l || echo 0; }
used_kib() { [[ -n "$STATE_DIR" ]] && df -k "$STATE_DIR" | tail -1 | awk '{print $3}' || echo 0; }

echo "== A. one child: identity, state, isolation =="
P=$(create fvc-parent "$IMAGE"); CREATED+=("$P") # registered here: create runs in a subshell
[[ -n "$P" ]] || { echo "could not create the parent VM" >&2; exit 2; }
wait_agent "$P" || { echo "the parent's guest agent never answered" >&2; exit 2; }
ex "$P" 'echo parent-before > /root/m1; nohup sleep 3000 >/dev/null 2>&1 & echo $! > /root/sleeppid; sync' >/dev/null
PH=$(ex "$P" hostname | tr -d '\n'); PM=$(ex "$P" 'cat /etc/machine-id 2>/dev/null' | tr -d '\n'); SP=$(ex "$P" 'cat /root/sleeppid' | tr -d '\n')
R=$(post "/v1/vms/${P}/fork" '{"count":1}')
C=$(echo "$R" | jq -r '.items[0].id // empty')
[[ -n "$C" ]] || { echo "fork failed: $R" >&2; exit 2; }
CREATED+=("$C")
chk "child's agent answers (the rotated token works)" "$(wait_agent "$C" && echo 1 || echo 0)"
CH=$(ex "$C" hostname | tr -d '\n'); CM=$(ex "$C" 'cat /etc/machine-id 2>/dev/null' | tr -d '\n')
chk "child hostname differs from the parent's ($PH vs $CH)" "$([[ -n "$CH" && "$CH" != "$PH" ]] && echo 1 || echo 0)"
if [[ -n "$PM" ]]; then
  chk "child machine-id differs from the parent's" "$([[ -n "$CM" && "$CM" != "$PM" ]] && echo 1 || echo 0)" "$PM / $CM"
else
  echo "N/A   machine-id: this image has none"
fi
eq "child has the parent's pre-fork file (disk state)" "$(ex "$C" 'cat /root/m1' | tr -d '\n')" parent-before
eq "child still has the parent's running process (memory state)" "$(ex "$C" "kill -0 $SP 2>/dev/null && echo alive" | tr -d '\n')" alive
ex "$C" 'echo child-only > /root/m2; sync' >/dev/null
ex "$P" 'echo parent-after > /root/m3; sync' >/dev/null
eq "child's own write reads back" "$(ex "$C" 'cat /root/m2' | tr -d '\n')" child-only
eq "child's write is not visible in the parent" "$(ex "$P" 'cat /root/m2 2>/dev/null || echo none' | tr -d '\n')" none
eq "parent's later write is not visible in the child" "$(ex "$C" 'cat /root/m3 2>/dev/null || echo none' | tr -d '\n')" none

echo "== B. three children from one fork =="
R=$(post "/v1/vms/${P}/fork" '{"count":3}' 180)
IDS=$(echo "$R" | jq -r '.items[].id // empty')
for id in $IDS; do CREATED+=("$id"); done
chk "fork count=3 returned 3 children" "$([[ "$(echo "$IDS" | grep -c .)" == 3 ]] && echo 1 || echo 0)" "$(echo "$R" | head -c 200)"
HN=(); MI=()
for id in $IDS; do wait_agent "$id"; HN+=("$(ex "$id" hostname | tr -d '\n')"); MI+=("$(ex "$id" 'cat /etc/machine-id 2>/dev/null' | tr -d '\n')"); done
chk "the three children have three distinct hostnames (${HN[*]:-})" "$([[ "$(printf '%s\n' "${HN[@]:-}" | sort -u | grep -c .)" == 3 ]] && echo 1 || echo 0)"
[[ -n "$PM" ]] && chk "the three children have three distinct machine-ids" "$([[ "$(printf '%s\n' "${MI[@]:-}" | sort -u | grep -c .)" == 3 ]] && echo 1 || echo 0)"

echo "== C. children outlive the parent =="
del "$P"
eq "a child still works and still has the parent's file after the parent is deleted" "$(ex "$C" 'cat /root/m1' | tr -d '\n')" parent-before
for id in $C $IDS; do del "$id"; done

if [[ -n "$OLD_IMAGE" ]]; then
  echo "== D. a failed multi-child fork leaves nothing behind (old agent) =="
  O=$(create fvc-old "$OLD_IMAGE"); CREATED+=("$O"); sleep 20
  V0=$(vm_count); D0=$(inst_dirs); K0=$(vmm_procs)
  CODE=$(curl -s -m 120 -X POST "${API}/v1/vms/${O}/fork" "${AUTH_HDR[@]}" -H 'Content-Type: application/json' -d '{"count":3}' -o /dev/null -w '%{http_code}')
  sleep 3
  chk "count=3 unique fork on an old agent is refused (HTTP $CODE)" "$([[ "$CODE" == 400 ]] && echo 1 || echo 0)"
  chk "no VM records left behind ($V0 -> $(vm_count))" "$([[ "$(vm_count)" == "$V0" ]] && echo 1 || echo 0)"
  if [[ -n "$STATE_DIR" ]]; then
    chk "no instance directories left behind ($D0 -> $(inst_dirs))" "$([[ "$(inst_dirs)" == "$D0" ]] && echo 1 || echo 0)"
    chk "no VMM processes left behind ($K0 -> $(vmm_procs))" "$([[ "$(vmm_procs)" == "$K0" ]] && echo 1 || echo 0)"
  else
    echo "N/A   instance-directory and process checks: set STATE_DIR"
  fi
  del "$O"
fi

echo "== E. repeated fork + delete does not leak ($LOOPS rounds) =="
N0=$(create fvc-loop "$IMAGE"); CREATED+=("$N0"); wait_agent "$N0"; sleep 2
V0=$(vm_count); D0=$(inst_dirs); K0=$(vmm_procs); U0=$(used_kib)
for _ in $(seq 1 "$LOOPS"); do
  c=$(post "/v1/vms/${N0}/fork" '{"count":1}' | jq -r '.items[0].id // empty')
  [[ -n "$c" ]] && { CREATED+=("$c"); wait_agent "$c"; del "$c"; }
done
sleep 3
chk "VM records unchanged ($V0 -> $(vm_count))" "$([[ "$(vm_count)" == "$V0" ]] && echo 1 || echo 0)"
if [[ -n "$STATE_DIR" ]]; then
  chk "instance directories unchanged ($D0 -> $(inst_dirs))" "$([[ "$(inst_dirs)" == "$D0" ]] && echo 1 || echo 0)"
  chk "VMM processes unchanged ($K0 -> $(vmm_procs))" "$([[ "$(vmm_procs)" == "$K0" ]] && echo 1 || echo 0)"
  GROW=$(( $(used_kib) - U0 ))
  chk "disk use grew by ${GROW} KiB (limit ${LEAK_LIMIT_KIB})" "$([[ "$GROW" -lt "$LEAK_LIMIT_KIB" ]] && echo 1 || echo 0)"
else
  echo "N/A   instance-directory, process and disk checks: set STATE_DIR"
fi
del "$N0"

echo "== ${PASS} passed, ${FAIL} failed =="
exit "$FAIL"
