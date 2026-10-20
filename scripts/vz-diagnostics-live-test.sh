#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: agent ping and incident bundles on a vz guest (`POST /v1/vms/{id}/agent/ping`, `/v1/vms/{id}/diagnostics`). A bundle job
# finishes, its checksum matches the download, the agent token is redacted by default and present with `redact:false`, bad requests
# (pcap over 30 s, a memory dump on vz) are refused, and a bundle can be listed and deleted.
#   scripts/vz-diagnostics-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-diag.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7817}"
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
run_dir = "/tmp/fluxvm-run-diag"
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

TOKEN="DIAGSECRET$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d "{\"name\":\"parent\",\"backend\":\"vz\",\"image\":\"agent-micro\",\"vcpus\":2,\"memory_mib\":1024,\"network\":{\"mode\":\"user\"},\"apple\":{\"usernet\":{}},\"agent\":{\"enabled\":true,\"token\":\"$TOKEN\"}}")"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "guest $P is up" || bad "agent never answered"

[[ "$(curl -sS -X POST "$B/$P/agent/ping" | jget 'd.get("ok")')" == True ]] && ok "agent/ping answers ok" || bad "ping: $(curl -sS -X POST "$B/$P/agent/ping")"

diag() { # <json body> -> prints the bundle token once done
  local R tok st
  R="$(curl -sS -X POST "$B/$P/diagnostics" -H 'content-type: application/json' -d "$1")"
  tok="$(jget 'd["token"]' <<<"$R" 2>/dev/null)" || { echo "START-FAILED $R"; return 1; }
  for _ in $(seq 1 60); do st="$(curl -fs "$B/$P/diagnostics/$tok" | jget 'd["state"]')"; [[ "$st" == done || "$st" == failed ]] && break; sleep 1; done
  [[ "$st" == done ]] && echo "$tok" || { echo "STATE-$st $(curl -fs "$B/$P/diagnostics/$tok" | jget 'd.get("error")')"; return 1; }
}
unpack() { rm -rf "$T/b$2"; mkdir "$T/b$2"; curl -fs "$B/$P/diagnostics/$1/bundle" -o "$T/b$2.tgz" && tar -xzf "$T/b$2.tgz" -C "$T/b$2"; }

T1="$(diag '{"log_lines":200}')" || bad "bundle 1: $T1"
ok "a default bundle (redact on) finished: $T1"
J="$(curl -fs "$B/$P/diagnostics/$T1")"; echo "     $(echo "$J" | cut -c1-500)"
unpack "$T1" 1 || bad "download/unpack of bundle 1"
[[ "$(jget 'd["sha256"]' <<<"$J")" == "$(shasum -a 256 "$T/b1.tgz" | cut -d' ' -f1)" ]] && ok "the recorded sha256 matches the downloaded bundle" || bad "sha256 differs"
[[ "$(jget 'd["size_bytes"]' <<<"$J")" == "$(wc -c < "$T/b1.tgz" | tr -d ' ')" ]] && ok "the recorded size matches the download" || bad "size differs"
echo "     bundle files:"; (cd "$T/b1" && find . -type f | sort | head -30 | sed 's/^/       /')
[[ "$(find "$T/b1" -type f | wc -l | tr -d ' ')" -gt 0 ]] && ok "the bundle holds files" || bad "empty bundle"
! grep -rqF "$TOKEN" "$T/b1" && ok "redact on: the agent token does not appear in the bundle" || bad "the agent token leaked into a redacted bundle"

T2="$(diag '{"redact":false,"log_lines":50}')" || bad "bundle 2: $T2"
unpack "$T2" 2 || bad "unpack 2"
grep -rqF "$TOKEN" "$T/b2" && ok "redact off: the same token does appear (so redaction is what removed it)" || bad "token absent even with redact:false"

CODE="$(curl -s -o "$T/o.json" -w '%{http_code}' -X POST "$B/$P/diagnostics" -H 'content-type: application/json' -d '{"pcap_seconds":31}')"
[[ "$CODE" == 400 ]] && ok "pcap_seconds 31 is refused (400)" || bad "pcap 31: $CODE $(cat "$T/o.json")"
CODE="$(curl -s -o "$T/o.json" -w '%{http_code}' -X POST "$B/$P/diagnostics" -H 'content-type: application/json' -d '{"memory_dump":true}')"
echo "     memory dump on vz: $CODE $(cut -c1-200 "$T/o.json")"
[[ "$CODE" == 400 ]] && ok "a memory dump is refused on vz (400)" || bad "memory dump: $CODE"

[[ "$(curl -fs "$B/$P/diagnostics" | jget 'len(d["items"])')" == 2 ]] && ok "both bundles are listed" || bad "list: $(curl -fs "$B/$P/diagnostics" | cut -c1-300)"
curl -fs -X DELETE "$B/$P/diagnostics/$T1" >/dev/null && [[ "$(curl -fs "$B/$P/diagnostics" | jget 'len(d["items"])')" == 1 ]] && ok "a bundle is deleted and drops off the list" || bad "delete"
CODE="$(curl -s -o /dev/null -w '%{http_code}' "$B/$P/diagnostics/$T1")"; [[ "$CODE" -ge 400 ]] && ok "a deleted bundle is gone ($CODE)" || bad "deleted bundle still served: $CODE"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz agent ping and diagnostics: PASS"
