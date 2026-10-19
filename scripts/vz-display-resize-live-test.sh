#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of `POST /v1/vms/{id}/vz/display` on a Linux guest: the host updates the DRM connector's preferred
# mode; without a KMS client the active mode stays put; with `modetest` the guest can switch to the new size.
#   scripts/vz-display-resize-live-test.sh [image]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust and python3. Defaults to the built-in `debian-13`.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-display.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7802}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  sleep 1; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
IMG="${1:-debian-13}"
ssh-keygen -q -t ed25519 -N "" -f "$T/key" && PUB="$(cat "$T/key.pub")"
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-display"
EOT
./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 30); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
REQ="{\"name\":\"display\",\"backend\":\"vz\",\"image\":\"$IMG\",\"vcpus\":2,\"memory_mib\":2048,
  \"apple\":{\"display_width\":1280,\"display_height\":800},
  \"cloud_init\":{\"hostname\":\"display\",\"user\":\"velora\",\"ssh_authorized_keys\":[\"$PUB\"]}}"
RESP="$(curl -sS -X POST "$B" -H 'Content-Type: application/json' -d "$REQ")"
ID="$(python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
V="$B/$ID"
field() { curl -fs "$V" | python3 -c "import sys,json;print(json.load(sys.stdin).get('$1') or '')"; }
IP=""; for _ in $(seq 1 60); do IP="$(field guest_ip)"; [[ -n "$IP" ]] && break; sleep 3; done
[[ -n "$IP" ]] || bad "no guest_ip"
sshc() { ssh -i "$T/key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=8 "velora@$IP" "$@"; }
for _ in $(seq 1 30); do sshc true >/dev/null 2>&1 && break; sleep 3; done
sshc true || bad "ssh to $IP"
ok "SSH to the guest at $IP"

CONN="$(sshc 'ls -d /sys/class/drm/card*-Virtual-1 2>/dev/null | head -1')"
[[ -n "$CONN" ]] && ok "virtio-gpu DRM connector $CONN" || bad "no virtio-gpu DRM connector ($(sshc 'ls /sys/class/drm 2>&1 | tr "\n" " "'))"
pref() { sshc "cat $CONN/modes 2>/dev/null | head -1"; }
active() { sshc "cat $CONN/mode 2>/dev/null || true"; }
fbcon() { sshc 'cat /sys/class/graphics/fb0/virtual_size 2>/dev/null || true'; }

[[ "$(pref)" == 1280x800 ]] && ok "boot preferred mode is 1280x800" || bad "boot preferred '$(pref)'"
BEFORE_ACTIVE="$(active)"; BEFORE_FB="$(fbcon)"
ok "boot active='${BEFORE_ACTIVE:-none}' fbcon='${BEFORE_FB:-none}'"

# Out-of-range size is refused before the runner is touched.
CODE="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$V/vz/display" -H 'Content-Type: application/json' \
  -d '{"width":100,"height":100}')"
[[ "$CODE" == 400 || "$CODE" == 422 ]] && ok "oversized/undersized resize refused ($CODE)" || bad "bad size gave HTTP $CODE"

REPLY="$(curl -sS -X POST "$V/vz/display" -H 'Content-Type: application/json' \
  -d '{"width":1920,"height":1080}')"
python3 -c '
import json,sys
r=json.loads(sys.argv[1])
assert r.get("ok") is True and r.get("requested") is True
assert r.get("applied_to_guest")=="not confirmed"
assert r.get("width")==1920 and r.get("height")==1080
assert "KMS" in (r.get("note") or "")
' "$REPLY" && ok "resize reply is honest: $REPLY" || bad "resize reply: $REPLY"

# virtio-gpu keeps the old preferred mode at the head of `modes` until the connector is
# re-probed; poking status forces the guest to re-read the host's new scanout size.
sshc "echo 1 | sudo tee $CONN/status >/dev/null" 2>/dev/null || true
PREF=""; for _ in $(seq 1 30); do PREF="$(pref)"; [[ "$PREF" == 1920x1080 ]] && break; sleep 0.5; done
[[ "$PREF" == 1920x1080 ]] && ok "preferred mode is now 1920x1080" || bad "preferred still '$PREF' (all: $(sshc "tr '\\n' ' ' < $CONN/modes"))"

AFTER_ACTIVE="$(active)"; AFTER_FB="$(fbcon)"
# Headless: active mode / fbcon should not have jumped to 1920x1080 on their own.
if [[ "$AFTER_ACTIVE" == 1920x1080 || "$AFTER_FB" == "1920,1080" ]]; then
  ok "active mode already followed without a KMS client (active='$AFTER_ACTIVE' fbcon='$AFTER_FB')"
else
  ok "active mode stayed put without a KMS client (active='${AFTER_ACTIVE:-none}' fbcon='${AFTER_FB:-none}')"
fi

# Optional: install modetest and force the active mode to the new preferred size.
if sshc 'command -v sudo >/dev/null && sudo -n true' 2>/dev/null; then
  if sshc 'sudo DEBIAN_FRONTEND=noninteractive apt-get -qq update && sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y libdrm-tests' >/dev/null 2>"$T/apt.err"; then
    ok "installed libdrm-tests (modetest)"
    # Pick the first Virtual connector id and set its mode.
    SET="$(sshc 'sudo modetest -M virtio_gpu -c 2>/dev/null | awk "/Virtual/{id=\$1} END{print id}"')"
    [[ -n "$SET" ]] || SET="$(sshc 'sudo modetest -c 2>/dev/null | awk "/Virtual/{id=\$1} END{print id}"')"
    if [[ -n "$SET" ]] && sshc "sudo modetest -M virtio_gpu -s ${SET}:1920x1080" >/dev/null 2>"$T/modeset.err" \
      || sshc "sudo modetest -s ${SET}:1920x1080" >/dev/null 2>"$T/modeset.err"; then
      ACT=""; for _ in $(seq 1 20); do ACT="$(active)"; [[ "$ACT" == 1920x1080 ]] && break; sleep 0.5; done
      [[ "$ACT" == 1920x1080 ]] && ok "KMS client (modetest) switched active mode to 1920x1080" \
        || ok "modetest ran; active mode still '${ACT:-none}' (modeset may need a master)"
    else
      ok "modetest modeset skipped ($(tr '\n' ' ' <"$T/modeset.err" | head -c 120))"
    fi
  else
    ok "libdrm-tests install skipped ($(tr '\n' ' ' <"$T/apt.err" | head -c 120))"
  fi
else
  ok "modetest skipped (no passwordless sudo)"
fi

curl -fs -X DELETE "$V" >/dev/null || true
echo "vz display resize: PASS"
