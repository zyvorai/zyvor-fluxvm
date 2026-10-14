#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of an agent session on a macOS guest (template clone, SSH engine). Builds a template from an IPSW
# (scripts/macos-session-template.sh) into `FLUXVM_MACOS_TEMPLATE_DIR` (default: the run's temp dir), or reuses one there: set FLUXVM_MACOS_TEMPLATE_DIR to a directory that holds
# `<name>/spec.json` and pass FLUXVM_MACOS_TEMPLATE=<name>.
#   scripts/vz-macos-session-live-test.sh /path/to/restore.ipsw
# Needs Apple silicon macOS, an UNLOCKED login session (a memory restore fails with the screen locked), about 60 GB
# free on an APFS volume, python3 and curl. To reuse a template across runs set FLUXVM_MACOS_STATE_DIR too. The first run takes about 10 minutes (the install).
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IPSW="${1:-}"
[[ -f "$IPSW" || -n "${FLUXVM_MACOS_TEMPLATE_DIR:-}" ]] || { echo "usage: $0 /path/to/restore.ipsw" >&2; exit 2; }
if ioreg -n Root -d1 -a | grep -q CGSSessionScreenIsLocked && ioreg -n Root -d1 -a | grep -A1 CGSSessionScreenIsLocked | grep -q true; then
  echo "the screen is locked: unlock it first (a memory restore needs a login session)" >&2; exit 2
fi
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-macos-live.XXXXXX")"
PORT="${FLUXVM_LIVE_PORT:-7799}"
TOKEN="live-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
U="http://127.0.0.1:$PORT"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 2
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT
pass=0
ok() { echo "ok   $*"; pass=$((pass + 1)); }
bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }
api() { curl -s -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
jget() {
  python3 -I -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"
}

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
# A template authorizes one daemon's SSH key, so reusing a template needs the same state dir (it holds that key).
STATE="${FLUXVM_MACOS_STATE_DIR:-$T/state}"; TPL_DIR="${FLUXVM_MACOS_TEMPLATE_DIR:-$STATE/templates}"
mkdir -p "$STATE"
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$STATE"
run_dir = "/tmp/fluxvm-run-macos-live"
[auth]
require = true
[[auth.tokens]]
name = "live"
token = "$TOKEN"
role = "admin"
[sandbox]
templates_dir = "$TPL_DIR"
EOT
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "$U/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "$U/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

NAME="${FLUXVM_MACOS_TEMPLATE:-live-macos}"
if [[ ! -f "$TPL_DIR/$NAME/spec.json" ]]; then
  FLUXVM_URL="$U" FLUXVM_TOKEN="$TOKEN" FLUXVM_TEMPLATES_DIR="$TPL_DIR" \
    scripts/macos-session-template.sh "$NAME" "$IPSW" flux "$STATE" > "$T/template.log" 2>&1 \
    || { tail -15 "$T/template.log"; bad "template build failed"; }
  ok "template built"
fi
[[ -f "$TPL_DIR/$NAME/spec.json" ]] || bad "no template $NAME in $TPL_DIR"
USER_NAME="$(jget '(d.get("cloud_init") or {}).get("user") or d["apple"].get("provision_username","")' < "$TPL_DIR/$NAME/spec.json")"

# 1. Refusals come before anything boots.
R="$(api -w ' %{http_code}' -X POST "$U/v1/agents/sessions" \
  -d "{\"agent\":\"live\",\"runtime\":{\"template\":\"$NAME\"},\"policy\":{\"preset\":\"offline\"}}")"
[[ "${R##* }" == 400 && "$R" == *"macOS guest"* ]] && ok "an offline policy is refused for a macOS guest" || bad "offline: $R"

# 2. A session on a clone of the template.
SESSION="$(api -X POST "$U/v1/agents/sessions" -d "{\"agent\":\"live\",\"runtime\":{\"template\":\"$NAME\"},\"workspace\":{\"mode\":\"clone\"}}")"
S="$(echo "$SESSION" | jget 'd.get("id","")')"
[[ -n "$S" ]] || bad "session create failed: $SESSION"
ok "session created from the template"
[[ "$(echo "$SESSION" | jget 'd["capabilities"]["guest_os"]')" == macos \
  && "$(echo "$SESSION" | jget 'd["capabilities"]["engine"]')" == ssh \
  && "$(echo "$SESSION" | jget 'd["capabilities"]["fork"]')" == False ]] \
  && ok "capabilities: macos, ssh engine, no fork" || bad "capabilities: $SESSION"
echo "memory_checkpoint: $(echo "$SESSION" | jget 'd["capabilities"].get("memory_checkpoint")')"

exec_json() {
  api -X POST "$U/v1/agents/sessions/$S/exec" \
    -d "$(python3 -I -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": int(sys.argv[2])}))' "$1" "${2:-30}")"
}
V="$(exec_json 'sw_vers -productVersion' | jget 'd.get("stdout","").strip()')"
[[ -n "$V" ]] && ok "exec reached macOS $V" || bad "exec returned nothing"
W="/Users/$USER_NAME/workspace"
[[ "$(exec_json "test -d $W && echo yes" | jget 'd.get("stdout","").strip()')" == yes ]] \
  && ok "the workspace is under the guest user's home" || bad "no workspace at $W"

# 3. Jobs run as shell scripts and keep their exit code.
J="$(api -X POST "$U/v1/agents/sessions/$S/jobs" -d '{"command":"echo tick; exit 7"}' | jget 'd.get("job","")')"
[[ -n "$J" ]] || bad "job start failed"
for _ in $(seq 1 20); do
  JS="$(api "$U/v1/agents/sessions/$S/jobs" | jget '[j for j in d["items"] if j["job"]=="'"$J"'"][0]["state"]')"
  [[ "$JS" == exited ]] && break; sleep 1
done
[[ "$(api "$U/v1/agents/sessions/$S/jobs" | jget '[j for j in d["items"] if j["job"]=="'"$J"'"][0].get("exit_code")')" == 7 ]] \
  && ok "job exit code kept" || bad "job did not exit 7: $(api "$U/v1/agents/sessions/$S/jobs")"

# 4. The display.
CODE="$(api -o "$T/shot.png" -w '%{http_code}' "$U/v1/agents/sessions/$S/screenshot")"
[[ "$CODE" == 200 && "$(head -c 4 "$T/shot.png" | tail -c 3)" == PNG ]] && ok "screenshot returns a PNG" || bad "screenshot http=$CODE"

# 5. Fork is refused with the reason.
F="$(api -w ' %{http_code}' -X POST "$U/v1/agents/sessions/$S/fork" -d '{"count":1}')"
[[ "${F##* }" == 409 && "$F" == *"macOS guest cannot be forked"* ]] && ok "fork refused with the reason" || bad "fork: $F"

# 6. Memory checkpoint, change, restore.
exec_json "echo before > $W/marker" >/dev/null
C="$(api -w ' %{http_code}' -X POST "$U/v1/agents/sessions/$S/checkpoint" -d '{"tag":"c1","memory":true}')"
[[ "${C##* }" == 201 || "${C##* }" == 200 ]] && ok "memory checkpoint taken" || bad "checkpoint: $C"
exec_json "echo after > $W/marker" >/dev/null
RS="$(api -w ' %{http_code}' -X POST "$U/v1/agents/sessions/$S/restore" -d '{"tag":"c1"}')"
[[ "${RS##* }" == 200 ]] || bad "restore: $RS"
for _ in $(seq 1 30); do
  M="$(exec_json "cat $W/marker" | jget 'd.get("stdout","").strip()')"
  [[ -n "$M" ]] && break; sleep 2
done
[[ "$M" == before ]] && ok "restore returned the earlier file state" || bad "marker is '$M' after restore"

# 7. Delete frees the clone.
api -X DELETE "$U/v1/agents/sessions/$S" >/dev/null
GONE="$(api -w ' %{http_code}' "$U/v1/agents/sessions/$S")"
[[ "${GONE##* }" == 404 || "$GONE" == *'"state":"deleted"'* ]] && ok "session deleted" || bad "session still there: $GONE"

echo "all $pass checks passed"
