#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: renaming and labelling vz guests (`PATCH /v1/vms/{id}`) and finding them again (`GET /v1/vms?name=&label=`). A rename shows in
# the record and the name filter; labels are added and removed; the selector forms (k=v, k!=v, k, !k, and several joined by a comma) pick
# the right guests; bad names, label keys and values are refused with 400 and a failed patch changes nothing; the change is an event.
#   scripts/vz-patch-labels-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-patch.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7820}"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1; pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true; rm -rf "$T"
}
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -10 "$T/daemon.log"; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-patch"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
status() { curl -fs "$B/$1" | jget 'd.get("status","")'; }
gexec() { # <vm> <command>
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 30}))' "$2")" | jget 'd.get("stdout","").strip()'
}
wait_agent() {
  for _ in $(seq 1 60); do [[ "$(gexec "$1" 'echo up' 2>/dev/null)" == up ]] && return 0; sleep 2; done
  curl -sS -X POST "$B/$1/agent" -H 'content-type: application/json' -d '{"command":"echo up","timeout_seconds":10}' | cut -c1-400 >&2; return 1
}

mkvm() { curl -sS -X POST "$B" -H 'content-type: application/json' -d "{\"name\":\"$1\",\"backend\":\"vz\",\"image\":\"agent-micro\",\"vcpus\":1,\"memory_mib\":512,\"network\":{\"mode\":\"user\"},\"apple\":{\"usernet\":{}}}"; }
A="$(mkvm alpha | jget 'd["id"]')" || bad "create alpha"
Z="$(mkvm beta | jget 'd["id"]')" || bad "create beta"
for _ in $(seq 1 30); do [[ "$(status "$A")" == running && "$(status "$Z")" == running ]] && break; sleep 2; done
[[ "$(status "$A")" == running && "$(status "$Z")" == running ]] && ok "two guests, alpha $A and beta $Z, are running" || bad "guests not running"
patch() { curl -sS -o "$T/patch.out" -w '%{http_code}' -X PATCH "$B/$1" -H 'content-type: application/json' -d "$2"; }
list() { curl -fsG "$B" "$@" | jget 'sorted(v["name"] for v in d["items"])'; }
rec() { curl -fs "$B/$1" | jget "$2"; }

[[ "$(patch "$A" '{"name":"gamma"}')" == 200 && "$(rec "$A" 'd["name"]')" == gamma ]] && ok "PATCH renames the guest" || bad "rename: $(cat "$T/patch.out")"
[[ "$(rec "$A" 'd["request"]["name"]')" == gamma ]] && ok "the stored create request carries the new name too" || bad "request.name not updated"
[[ "$(list --data-urlencode name=gamma)" == '["gamma"]' && "$(list --data-urlencode name=alpha)" == '[]' ]] && ok "the name filter finds the new name and not the old" || bad "name filter: $(list --data-urlencode name=gamma) / $(list --data-urlencode name=alpha)"

patch "$A" '{"labels":{"env":"prod","tier":"web"}}' >/dev/null; patch "$Z" '{"labels":{"env":"dev"}}' >/dev/null
[[ "$(rec "$A" 'sorted(d["labels"].items())')" == '[["env", "prod"], ["tier", "web"]]' ]] && ok "labels are set on gamma" || bad "labels: $(rec "$A" 'd["labels"]')"

sel() { list --data-urlencode "label=$1"; }
[[ "$(sel 'env=prod')" == '["gamma"]' ]] && ok "label=env=prod picks gamma" || bad "env=prod: $(sel 'env=prod')"
[[ "$(sel 'env!=prod')" == '["beta"]' ]] && ok "label=env!=prod picks beta" || bad "env!=prod: $(sel 'env!=prod')"
[[ "$(sel 'env')" == '["beta", "gamma"]' ]] && ok "label=env (key exists) picks both" || bad "env: $(sel 'env')"
[[ "$(sel '!tier')" == '["beta"]' ]] && ok "label=!tier (key absent) picks beta" || bad "!tier: $(sel '!tier')"
[[ "$(sel 'env=prod,tier=web')" == '["gamma"]' && "$(sel 'env=prod,tier=api')" == '[]' ]] && ok "joined terms must all match" || bad "joined: $(sel 'env=prod,tier=web') / $(sel 'env=prod,tier=api')"
CODE="$(curl -s -o /dev/null -w '%{http_code}' -G "$B" --data-urlencode 'label==broken=')"; echo "     invalid selector: $CODE"
[[ "$CODE" -ge 400 ]] && ok "an invalid selector is refused ($CODE)" || bad "invalid selector accepted: $CODE"

[[ "$(patch "$A" '{"labels":{"tier":null}}')" == 200 && "$(rec "$A" 'sorted(d["labels"])')" == '["env"]' ]] && ok "a null value removes a label" || bad "remove: $(rec "$A" 'd["labels"]')"
[[ "$(sel 'tier')" == '[]' ]] && ok "the removed label no longer matches" || bad "tier still matches"
[[ "$(patch "$A" '{"labels":{"never-set":null}}')" == 200 ]] && ok "removing a label that was never set is a no-op (200)" || bad "no-op removal: $(cat "$T/patch.out")"

[[ "$(patch "$A" '{"name":"bad name!"}')" == 400 ]] && ok "a name with a space and a bang is refused (400)" || bad "bad name: $(cat "$T/patch.out")"
[[ "$(patch "$A" "{\"name\":\"$(printf 'x%.0s' $(seq 1 64))\"}")" == 400 ]] && ok "a 64-character name is refused (400)" || bad "long name"
[[ "$(patch "$A" '{"labels":{"bad key":"x"}}')" == 400 ]] && ok "a label key with a space is refused (400)" || bad "bad key: $(cat "$T/patch.out")"
[[ "$(patch "$A" "{\"labels\":{\"k\":\"$(printf 'v%.0s' $(seq 1 254))\"}}")" == 400 ]] && ok "a 254-character label value is refused (400)" || bad "long value"
[[ "$(patch "$A" '{"name":"half-applied","labels":{"bad key":"x"}}')" == 400 && "$(rec "$A" 'd["name"]')" == gamma ]] && ok "a refused patch changes nothing (the rename was not applied)" || bad "partial patch: name is $(rec "$A" 'd["name"]')"
[[ "$(patch "00000000-0000-0000-0000-000000000000" '{"name":"x"}')" -ge 400 ]] && ok "patching an unknown VM is refused" || bad "unknown vm"

DUP="$(patch "$Z" '{"name":"gamma"}')"; echo "     renaming beta to the name gamma: $DUP, now $(list --data-urlencode name=gamma)"

EV="$(curl -fs "http://127.0.0.1:$PORT/v1/events?vm=$A&event=vm.patch")"
echo "     $(echo "$EV" | jget '[(e["event"], e.get("fields")) for e in d["items"]]' | cut -c1-400)"
[[ "$(echo "$EV" | jget 'len(d["items"]) >= 3 and any("env=prod" in e["fields"].get("changes","") for e in d["items"]) and any("name:alpha->gamma" in e["fields"].get("changes","") for e in d["items"])')" == True ]] \
  && ok "each change is a vm.patch event naming what changed" || bad "events: $EV"
for V in "$A" "$Z"; do curl -fs -X DELETE "$B/$V" >/dev/null || true; done
echo "vz patch and labels: PASS"
