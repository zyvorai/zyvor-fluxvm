#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build a macOS guest template that agent sessions can start from (`runtime.template`): installs from an IPSW with
# the unattended `provision_*` options, authorizes the daemon's SSH key for the guest user, turns off sleep and the
# screen lock, shuts the guest down, and writes `<templates_dir>/<name>/spec.json` with the disk and identity files
# beside it (APFS clones, so the templates directory must be on the same volume as the daemon's state).
#   FLUXVM_URL=http://127.0.0.1:7788 FLUXVM_TOKEN=... \
#     scripts/macos-session-template.sh <name> <path/to/restore.ipsw> [guest-user] [state_dir]
# `state_dir` defaults to the daemon's `~/Library/Application Support/fluxvm`; templates go to `<state_dir>/templates`
# unless `sandbox.templates_dir` is set (pass it as FLUXVM_TEMPLATES_DIR). Takes about 10 minutes; needs about 30 GB.
set -euo pipefail
NAME="${1:?usage: $0 <name> <ipsw> [guest-user] [state_dir]}"
IPSW="${2:?usage: $0 <name> <ipsw> [guest-user] [state_dir]}"
GUEST_USER="${3:-flux}"
STATE="${4:-$HOME/Library/Application Support/fluxvm}"
URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
TOKEN="${FLUXVM_TOKEN:?set FLUXVM_TOKEN to an admin token}"
TEMPLATES="${FLUXVM_TEMPLATES_DIR:-$STATE/templates}"
[[ -f "$IPSW" ]] || { echo "no such IPSW: $IPSW" >&2; exit 2; }
[[ "$NAME" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$ ]] || { echo "template name must be 1-64 of [A-Za-z0-9._-]" >&2; exit 2; }
[[ -e "$TEMPLATES/$NAME" ]] && { echo "template $NAME already exists at $TEMPLATES/$NAME" >&2; exit 2; }
api() { curl -sf -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
jget() { python3 -I -c 'import sys,json; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}) or "")' "$1"; }

W="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-macos-tmpl.XXXXXX")"; chmod 700 "$W"
VM=""
cleanup() { [[ -n "$VM" ]] && api -X DELETE "$URL/v1/vms/$VM" >/dev/null 2>&1 || true; rm -rf "$W"; }
trap cleanup EXIT
umask 077
PW="$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
printf '%s' "$PW" > "$W/pw"; cp "$W/pw" "$W/pw-once"
printf '#!/bin/sh\ncat %s\n' "$W/pw" > "$W/askpass"; chmod 700 "$W/askpass"

echo "installing macOS from $IPSW (this takes several minutes)"
REQ="$(python3 -I - "$NAME" "$IPSW" "$GUEST_USER" "$W/pw-once" <<'PY'
import json, sys
name, ipsw, user, pwf = sys.argv[1:5]
print(json.dumps({"name": f"template-build-{name}", "backend": "vz", "image": ipsw, "vcpus": 4, "memory_mib": 8192,
  "disk_size_gib": 64, "network": {"mode": "user"},
  "apple": {"guest_os": "macos", "install": True, "provision_full_name": "Flux Agent", "provision_username": user,
            "provision_password_file": pwf, "provision_auto_login": True, "provision_remote_login": True}}))
PY
)"
R="$(curl -sf -m 7200 -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$REQ" "$URL/v1/vms")"
VM="$(echo "$R" | jget 'd["id"]')"
WORK="$(echo "$R" | jget 'd["workspace"]')"
echo "installed: $VM"

IP=""
for _ in $(seq 1 60); do
  IP="$(api "$URL/v1/vms/$VM" | jget 'd.get("guest_ip")')"
  [[ -n "$IP" ]] && nc -z -w2 "$IP" 22 2>/dev/null && break
  sleep 5
done
[[ -n "$IP" ]] || { echo "the guest never reported an address with sshd up" >&2; exit 1; }

export SSH_ASKPASS="$W/askpass" SSH_ASKPASS_REQUIRE=force
gs() { ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
  -o PreferredAuthentications=password,keyboard-interactive -o PubkeyAuthentication=no "$GUEST_USER@$IP" "$@"; }
# The daemon creates its key on first use; an exec (which fails: the key is not authorized yet) makes it.
if [[ ! -f "$STATE/sandbox_ed25519.pub" ]]; then
  api -X POST "$URL/v1/vms/$VM/agent" -d '{"command":"true","timeout_seconds":20}' >/dev/null 2>&1 || true
fi
[[ -f "$STATE/sandbox_ed25519.pub" ]] || { echo "no $STATE/sandbox_ed25519.pub: pass the daemon's state_dir" >&2; exit 1; }
PUB="$(cat "$STATE/sandbox_ed25519.pub")"
gs "mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo '$PUB' >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys"
echo "daemon key authorized for $GUEST_USER"
# A session must not find the guest asleep or locked. Best effort: each setting may be refused on a given release.
gs "echo '$PW' | sudo -S pmset -a sleep 0 displaysleep 0 disksleep 0 >/dev/null 2>&1; \
    defaults write com.apple.screensaver idleTime 0 2>/dev/null; \
    echo '$PW' | sudo -S sysadminctl -screenLock off -password '$PW' >/dev/null 2>&1; true"
echo "stopping the guest"
api -X POST "$URL/v1/vms/$VM/stop" >/dev/null
for _ in $(seq 1 60); do
  [[ "$(api "$URL/v1/vms/$VM" | jget 'd["status"]')" == stopped ]] && break
  sleep 2
done

mkdir -p "$TEMPLATES/$NAME"
for f in root.raw hardware.bin identity.bin auxiliary.bin; do
  [[ -f "$WORK/$f" ]] || continue
  dest="$f"; [[ "$f" == root.raw ]] && dest=disk.raw
  cp -c "$WORK/$f" "$TEMPLATES/$NAME/$dest"
done
[[ -f "$TEMPLATES/$NAME/disk.raw" ]] || { echo "no disk found in $WORK" >&2; exit 1; }
python3 -I - "$TEMPLATES/$NAME" "$GUEST_USER" <<'PY'
import json, sys
d, user = sys.argv[1:3]
spec = {"name": "template", "backend": "vz", "image": f"{d}/disk.raw", "vcpus": 4, "memory_mib": 8192,
        "network": {"mode": "user"},
        "apple": {"guest_os": "macos"}, "cloud_init": {"user": user}}
open(f"{d}/spec.json", "w").write(json.dumps(spec, indent=2) + "\n")
PY
echo "template ready: $TEMPLATES/$NAME"
echo "use it: POST /v1/agents/sessions {\"agent\":\"a\",\"runtime\":{\"template\":\"$NAME\"}}"
