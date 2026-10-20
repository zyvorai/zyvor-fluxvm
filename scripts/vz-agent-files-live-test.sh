#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: guest file transfer on a vz guest (`POST /v1/vms/{id}/agent/put-file` and `/agent/get-file`). Binary content must
# round-trip byte for byte, the requested mode must be applied, files up to the request body limit must survive and larger ones be refused, and a missing file must
# be refused with an error; put-file creates missing parent directories.
#   scripts/vz-agent-files-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-agentfiles.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7809}"
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
run_dir = "/tmp/fluxvm-run-agentfiles"
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

RESP="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"parent","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}')"
P="$(jget 'd["id"]' <<<"$RESP" 2>/dev/null)" || bad "create: $RESP"
wait_agent "$P" && ok "parent $P is up with the agent" || bad "agent never answered"

pfile() { # <vm> <path> <local file> [mode]
  python3 -c '
import base64,json,sys
d={"path":sys.argv[1],"content_base64":base64.b64encode(open(sys.argv[2],"rb").read()).decode()}
if len(sys.argv)>3: d["mode"]=int(sys.argv[3],8)
print(json.dumps(d))' "$2" "$3" ${4:+"$4"} | curl -sS -X POST "$B/$1/agent/put-file" -H 'content-type: application/json' -d @-
}
gfile() { # <vm> <path>  -> raw JSON
  curl -sS -X POST "$B/$1/agent/get-file" -H 'content-type: application/json' -d "{\"path\":\"$2\"}"
}

head -c 65536 /dev/urandom > "$T/small.bin"
R="$(pfile "$P" /tmp/small.bin "$T/small.bin" 640)"; echo "     put-file: $(echo "$R" | cut -c1-200)"
SUM="$(shasum -a 256 "$T/small.bin" | cut -d' ' -f1)"
[[ "$(gexec "$P" 'sha256sum /tmp/small.bin | cut -d" " -f1')" == "$SUM" ]] && ok "64 KiB binary file landed intact in the guest" || bad "guest checksum differs: $R"
[[ "$(gexec "$P" 'stat -c %a /tmp/small.bin')" == 640 ]] && ok "requested mode 0640 applied" || bad "mode: $(gexec "$P" 'stat -c %a /tmp/small.bin')"

G="$(gfile "$P" /tmp/small.bin)"; echo "     get-file keys: $(echo "$G" | jget 'sorted(d.keys())')"
echo "$G" | python3 -c '
import base64,json,sys
d=json.load(sys.stdin)
k=next(k for k in d if "base64" in k)
open(sys.argv[1],"wb").write(base64.b64decode(d[k]))' "$T/back.bin" 2>/dev/null || bad "get-file response: $(echo "$G" | cut -c1-300)"
[[ "$(shasum -a 256 "$T/back.bin" | cut -d' ' -f1)" == "$SUM" ]] && ok "get-file returns the same bytes" || bad "round-trip differs"

gexec "$P" 'head -c 6291456 /dev/urandom > /tmp/big.bin' >/dev/null
BSUM="$(gexec "$P" 'sha256sum /tmp/big.bin | cut -d" " -f1')"
G="$(gfile "$P" /tmp/big.bin)"
echo "$G" | python3 -c '
import base64,json,sys
d=json.load(sys.stdin)
k=next(k for k in d if "base64" in k)
open(sys.argv[1],"wb").write(base64.b64decode(d[k]))' "$T/big.bin" 2>/dev/null || bad "get-file of 6 MiB: $(echo "$G" | cut -c1-300)"
[[ "$(shasum -a 256 "$T/big.bin" | cut -d' ' -f1)" == "$BSUM" ]] && ok "6 MiB file read back intact" || bad "6 MiB checksum differs"
head -c 1048576 "$T/big.bin" > "$T/mid.bin"; MSUM="$(shasum -a 256 "$T/mid.bin" | cut -d' ' -f1)"
R="$(pfile "$P" /tmp/mid-up.bin "$T/mid.bin")"
[[ "$(gexec "$P" 'sha256sum /tmp/mid-up.bin | cut -d" " -f1')" == "$MSUM" ]] && ok "1 MiB file written intact" || bad "1 MiB upload differs: $(echo "$R" | cut -c1-300)"
# put-file rides one JSON request, so the API's default 2 MiB body limit bounds an upload (get-file has no such cap).
R="$(head -c 3145728 "$T/big.bin" > "$T/over.bin"; pfile "$P" /tmp/over.bin "$T/over.bin")"
[[ "$R" == *"length limit exceeded"* ]] && ok "a 3 MiB upload is refused by the body limit" || bad "3 MiB upload: $(echo "$R" | cut -c1-300)"
[[ "$(gexec "$P" 'test -e /tmp/over.bin && echo yes || echo no')" == no ]] && ok "the refused upload left no file behind" || bad "partial file written"

R="$(pfile "$P" /tmp/newdir-$$/nested.txt "$T/small.bin")"
[[ "$(gexec "$P" "sha256sum /tmp/newdir-$$/nested.txt | cut -d' ' -f1")" == "$SUM" ]] && ok "put-file creates missing parent directories" || bad "nested put-file: $R"
M="$(gfile "$P" /tmp/does-not-exist)"
echo "     get-file of a missing file: $(echo "$M" | cut -c1-200)"
[[ "$M" == *error* || "$M" == *rror* ]] && ok "a missing file is reported as an error" || bad "missing file not refused: $M"
curl -fs -X DELETE "$B/$P" >/dev/null || true
echo "vz guest file transfer: PASS"
