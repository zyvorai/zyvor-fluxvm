#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: a macOS guest cloned from a template that has the guest agent installed WITHOUT a token gets its token from the
# host through the read-only `firstboot` share (`fluxvm-guest-agent --token-seed`), with nothing provisioned by hand.
#   1. install macOS from an IPSW with the unattended provision_* options (about 10 minutes, about 30 GB);
#   2. over SSH put the guest agent in as a LaunchDaemon with no token (the agent then refuses every request);
#   3. stop it and use its disk as a template;
#   4. clone the template with agent.enabled and apple.macos_vsock: the host writes agent-token into the share, and the
#      agent adopts it. Checked: the guest's token file equals the VM's token, the guest log says where it came from, and
#      `POST /v1/vms/{id}/agent` runs a command;
#   5. a clone with the agent off gets no share and no token.
#   FLUXVM_MACOS_IPSW=~/Library/Application\ Support/FluxVM/images/macos-26A434.ipsw scripts/vz-macos-agent-seed-live-test.sh
# The temp directory must be on the volume with enough space (APFS clones are used for the template).
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IPSW="${FLUXVM_MACOS_IPSW:-$(ls "$HOME"/Library/Application\ Support/FluxVM/images/macos-*.ipsw 2>/dev/null | head -1)}"
[[ -f "$IPSW" ]] || { echo "set FLUXVM_MACOS_IPSW to a macOS restore image" >&2; exit 2; }
USER_NAME="${FLUXVM_MACOS_USER:-flux}"; PORT="${FLUXVM_LIVE_PORT:-7796}"
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-macos-seed.XXXXXX")"; chmod 700 "$T"
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }
cleanup() { [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"; }
trap cleanup EXIT
jget() { python3 -I -c 'import sys,json; d=json.load(sys.stdin); print(eval(sys.argv[1], {"d": d}) or "")' "$1"; }

cargo build -p fluxctl -p fluxvm-guest-agent -j 4 2>&1 | tail -1
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-macos-seed"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
SERVER="http://127.0.0.1:$PORT"; B="$SERVER/v1/vms"
api() { curl -sf -H 'content-type: application/json' "$@"; }
ok "daemon is up"

umask 077
PW="$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
printf '%s' "$PW" > "$T/pw"; cp "$T/pw" "$T/pw-once"
printf '#!/bin/sh\ncat %s\n' "$T/pw" > "$T/askpass"; chmod 700 "$T/askpass"
export SSH_ASKPASS="$T/askpass" SSH_ASKPASS_REQUIRE=force
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10
          -o PreferredAuthentications=password,keyboard-interactive -o PubkeyAuthentication=no)
gs() { ssh "${SSH_OPTS[@]}" "$USER_NAME@$IP" "$@"; }
wait_ssh() { # vm-id -> sets IP
  IP=""
  for _ in $(seq 1 90); do
    IP="$(api "$B/$1" | jget 'd.get("guest_ip")')"
    [[ -n "$IP" ]] && nc -z -w2 "$IP" 22 2>/dev/null && return 0
    sleep 5
  done
  return 1
}

echo "installing macOS from $IPSW (several minutes)"
START=$SECONDS
REQ="$(python3 -I - "$IPSW" "$USER_NAME" "$T/pw-once" <<'PY'
import json, sys
ipsw, user, pwf = sys.argv[1:4]
print(json.dumps({"name": "seed-base", "backend": "vz", "image": ipsw, "vcpus": 4, "memory_mib": 8192,
  "disk_size_gib": 64, "network": {"mode": "user"},
  "apple": {"guest_os": "macos", "install": True, "macos_vsock": True, "provision_full_name": "Flux Agent",
            "provision_username": user, "provision_password_file": pwf, "provision_auto_login": True,
            "provision_remote_login": True}}))
PY
)"
R="$(curl -sf -m 7200 -H 'content-type: application/json' -d "$REQ" "$B")" || bad "macOS install request failed"
BASE="$(jget 'd["id"]' <<<"$R")"; WORK="$(jget 'd["workspace"]' <<<"$R")"
ok "macOS installed and booted in $((SECONDS-START))s ($BASE)"
wait_ssh "$BASE" && ok "guest is up at $IP with sshd" || bad "the guest never reported an address with sshd up"

# The daemon's own key lets `POST /v1/vms/{id}/agent` fall back to SSH; authorize it as the session-template script does.
api -X POST "$B/$BASE/agent" -d '{"command":"true","timeout_seconds":20}' >/dev/null 2>&1 || true
STATE="$T/state"
[[ -f "$STATE/sandbox_ed25519.pub" ]] && gs "mkdir -p ~/.ssh && chmod 700 ~/.ssh && echo '$(cat "$STATE/sandbox_ed25519.pub")' >> ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys" \
  && ok "daemon SSH key authorized" || bad "could not authorize the daemon's key"

# The agent goes in with NO token.
scp "${SSH_OPTS[@]}" -q target/debug/fluxvm-guest-agent crates/fluxvm-guest-agent/macos/install.sh \
  crates/fluxvm-guest-agent/macos/dev.zyvor.fluxvm-guest-agent.plist "$USER_NAME@$IP:/tmp/" || bad "copying the agent into the guest"
gs "mkdir -p /tmp/agent && mv /tmp/fluxvm-guest-agent /tmp/install.sh /tmp/dev.zyvor.fluxvm-guest-agent.plist /tmp/agent/ \
    && echo '$PW' | sudo -S sh /tmp/agent/install.sh /tmp/agent/fluxvm-guest-agent" >/dev/null 2>&1 || bad "installing the agent in the guest"
sleep 5
LOG="$(gs "echo '$PW' | sudo -S cat /var/log/fluxvm-guest-agent.log 2>/dev/null")"
[[ "$LOG" == *"no token"* ]] && ok "the template's agent has no token (it refuses every request)" || bad "unexpected agent log: $LOG"
[[ "$(gs "echo '$PW' | sudo -S test -s /etc/fluxvm-guest-agent.token && echo present || echo absent" 2>/dev/null | tail -1)" == absent ]] \
  && ok "no token file in the template" || bad "the template already has a token file"

echo "stopping the base guest and cloning its disk as a template"
api -X POST "$B/$BASE/stop" >/dev/null
for _ in $(seq 1 60); do [[ "$(api "$B/$BASE" | jget 'd["status"]')" == stopped ]] && break; sleep 2; done
mkdir -p "$T/tpl"
for f in root.raw hardware.bin identity.bin auxiliary.bin; do
  [[ -f "$WORK/$f" ]] || continue
  dest="$f"; [[ "$f" == root.raw ]] && dest=disk.raw
  cp -c "$WORK/$f" "$T/tpl/$dest"
done
[[ -f "$T/tpl/disk.raw" ]] || bad "no disk in $WORK"
api -X DELETE "$B/$BASE" >/dev/null || true
ok "template ready"

clone() { # name agent-json -> id
  api -X POST "$B" -d "{\"name\":\"$1\",\"backend\":\"vz\",\"image\":\"$T/tpl/disk.raw\",\"vcpus\":4,\"memory_mib\":8192,
    \"network\":{\"mode\":\"user\"},\"apple\":{\"guest_os\":\"macos\",\"macos_vsock\":true},\"agent\":$2}" | jget 'd["id"]'
}

# 4. A clone with the host agent on: the token arrives through the share.
CL="$(clone seed-clone '{"enabled":true}')" || bad "creating the clone"
ok "clone created ($CL)"
wait_ssh "$CL" && ok "clone is up at $IP" || bad "the clone never came up"
RECORD_TOKEN="$(api "$B/$CL" | jget 'd["request"]["agent"]["token"]')"
[[ -n "$RECORD_TOKEN" ]] || RECORD_TOKEN="$(cat "$T/state/vms.json" | jget '[v for v in d.get("vms", d) if v.get("id")=="'"$CL"'"][0]["request"]["agent"]["token"]')"
[[ -n "$RECORD_TOKEN" ]] || bad "could not read the clone's agent token from the daemon"
GOT=""
for _ in $(seq 1 30); do
  GOT="$(gs "echo '$PW' | sudo -S cat /etc/fluxvm-guest-agent.token 2>/dev/null" | tail -1 | tr -d '[:space:]')" || true
  [[ -n "$GOT" ]] && break
  # The seed is read on a request; poke the agent through the API so it looks.
  api -X POST "$B/$CL/agent" -d '{"command":"true","timeout_seconds":15}' >/dev/null 2>&1 || true
  sleep 3
done
[[ "$GOT" == "$RECORD_TOKEN" ]] && ok "the guest's token file equals the VM's token (delivered through the firstboot share)" \
  || bad "guest token '${GOT:0:8}…' != record token '${RECORD_TOKEN:0:8}…'"
LOG="$(gs "echo '$PW' | sudo -S cat /var/log/fluxvm-guest-agent.log 2>/dev/null")"
[[ "$LOG" == *"took the token from /Volumes/My Shared Files/firstboot/agent-token"* ]] \
  && ok "the agent log says it took the token from the share" || bad "agent log: $LOG"
OUT="$(api -X POST "$B/$CL/agent" -d '{"command":"uname -sm","timeout_seconds":30}')" || bad "exec through the agent"
[[ "$OUT" == *"Darwin arm64"* ]] && ok "exec through POST /v1/vms/{id}/agent: Darwin arm64" || bad "exec output: $OUT"
api -X DELETE "$B/$CL" >/dev/null || true

# 5. A clone with the host agent off gets no share and no token.
OFF="$(clone seed-off '{"enabled":false}')" || bad "creating the agent-off clone"
wait_ssh "$OFF" && ok "agent-off clone is up" || bad "the agent-off clone never came up"
[[ "$(gs "ls '/Volumes/My Shared Files/firstboot' >/dev/null 2>&1 && echo share || echo none")" == none ]] \
  && ok "no firstboot share without the host agent" || bad "a firstboot share was attached with the agent off"
[[ "$(gs "echo '$PW' | sudo -S test -s /etc/fluxvm-guest-agent.token && echo present || echo absent" 2>/dev/null | tail -1)" == absent ]] \
  && ok "and no token file" || bad "a token file appeared with the agent off"
api -X DELETE "$B/$OFF" >/dev/null || true
echo "PASS"
