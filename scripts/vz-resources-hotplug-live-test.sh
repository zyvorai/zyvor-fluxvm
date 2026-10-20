#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the live-resize routes a vz guest does not support (`POST /v1/vms/{id}/resources`, `/hotplug/{cpu,memory,nic,nic/unplug,share}`) and the
# read-only `GET /v1/vms/{id}/guard`. Each unsupported change is refused with 400 and a message naming the reason, malformed requests are told
# apart from unsupported ones, an unknown VM is refused, and through all of it the guest keeps running with the same vCPUs, memory and agent.
#   scripts/vz-resources-hotplug-live-test.sh [/path/to/agent-micro-arm64.raw]
#   scripts/vz-resources-hotplug-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-rh.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7850}"
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
run_dir = "/tmp/fluxvm-run-rh"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"; NIL=00000000-0000-0000-0000-000000000000
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
V="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"rh","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}},"agent":{"enabled":true}}' | jget 'd["id"]')" || bad "create"
for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "vz guest $V is running (2 vCPUs, 1024 MiB)" || bad "guest not running"
gexec() { curl -sS -X POST "$B/$V/agent" -H 'content-type: application/json' -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 30}))' "$1")" | jget 'd.get("stdout","").strip()'; }
for _ in $(seq 1 60); do [[ "$(gexec 'echo up' 2>/dev/null)" == up ]] && break; sleep 2; done
CPU0="$(gexec 'nproc')"; MEM0="$(gexec "awk '/MemTotal/{print int(\$2/1024)}' /proc/meminfo")"
[[ "$CPU0" == 2 ]] && ok "the guest sees 2 CPUs and ${MEM0} MiB before we try anything" || bad "nproc=$CPU0"

post() { # <path> <body>  -> sets CODE and BODY
  CODE="$(curl -sS -o "$T/r.out" -w '%{http_code}' -X POST "$B/${3:-$V}/$1" -H 'content-type: application/json' -d "$2")"; BODY="$(cat "$T/r.out")"
}
refused() { # <desc> <path> <body> <substring>
  post "$2" "$3"; [[ "$CODE" == 400 && "$BODY" == *"$4"* ]] && ok "$1 -> 400 \"$4\"" || bad "$1: $CODE $BODY"
}
refused "resources cpu_quota_percent"  resources '{"cpu_quota_percent":50}'      "no cgroup"
refused "resources memory_max_bytes"   resources '{"memory_max_bytes":536870912}' "no cgroup"
refused "hotplug cpu"                  hotplug/cpu '{"add_vcpus":1}'             "not supported for backend Vz"
refused "hotplug memory"               hotplug/memory '{"add_memory_mib":256}'   "not supported for backend Vz"
refused "hotplug nic (bridge)"         hotplug/nic '{"bridge":"br0"}'            "QEMU backend only"
refused "hotplug nic with no target"   hotplug/nic '{}'                          "needs either"
refused "nic unplug with no target"    hotplug/nic/unplug '{}'                   "exactly one of"
post hotplug/share '{}'; [[ "$CODE" == 422 && "$BODY" == *host_path* ]] && ok "hotplug share without host_path -> 422 naming host_path" || bad "share {}: $CODE $BODY"
post hotplug/share '{"host_path":"relative/dir"}'; [[ "$CODE" == 400 ]] && ok "hotplug share with a relative path -> 400 ($BODY)" || bad "share relative: $CODE $BODY"
post hotplug/share '{"host_path":"/tmp"}'; echo "     hotplug share of /tmp: $CODE $(echo "$BODY" | cut -c1-160)"
[[ "$CODE" == 400 && "$BODY" == *"QEMU and Cloud Hypervisor backends only"* ]] && ok "hotplug share of a real dir -> 400 'QEMU and Cloud Hypervisor backends only'" || bad "share /tmp: $CODE $BODY"

for r in resources hotplug/cpu hotplug/memory; do
  post "$r" '{"io_weight":10,"add_vcpus":1,"add_memory_mib":1}' "$NIL"
  [[ "$CODE" == 400 && "$BODY" == *"not found"* ]] || bad "$r on an unknown VM: $CODE $BODY"
done
ok "resources / hotplug cpu / hotplug memory on an unknown VM -> 400 'not found'"

G="$(curl -sS "$B/$V/guard")"
[[ "$(echo "$G" | jget '[d["configured"], d["kernel_policy_present"], d["expired_packs"]]')" == '[false, false, []]' ]] && ok "guard: a vz guest has no VMM Guard policy (configured=false, no packs)" || bad "guard: $G"
[[ "$(curl -s -o /dev/null -w '%{http_code}' "$B/$NIL/guard")" == 400 ]] && ok "guard on an unknown VM -> 400" || bad "guard unknown"

[[ "$(curl -fs "$B/$V" | jget '[d["status"], d["request"]["vcpus"], d["request"]["memory_mib"]]')" == '["running", 2, 1024]' ]] && ok "the record still says running, 2 vCPUs, 1024 MiB" || bad "record changed: $(curl -fs "$B/$V")"
[[ "$(gexec nproc)" == "$CPU0" && "$(gexec "awk '/MemTotal/{print int(\$2/1024)}' /proc/meminfo")" == "$MEM0" ]] && ok "and the guest itself still sees $CPU0 CPUs and ${MEM0} MiB, and its agent answers" || bad "guest changed"
echo "vz resources/hotplug: PASS"
