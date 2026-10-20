#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: what the daemon reports about the Mac host (`GET /v1/host/{apple,apple/capabilities,apple/ipsw,overview,memory,storage,images}`).
# The probes agree with each other and with the machine (CPU count, memory, macOS version), the capability list covers the vz features, and
# a running guest shows up in the overview counts and the storage report; the Linux-only reports (topology, pressure) come back empty, not an error.
#   scripts/vz-host-live-test.sh [/path/to/agent-micro-arm64.raw]
#   scripts/vz-host-live-test.sh [/path/to/agent-micro-arm64.raw]
# Needs Apple silicon macOS 14+, Xcode CLT, Rust, python3. Defaults to ~/Library/Application Support/fluxvm/images/agent-micro-arm64.raw.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-$HOME/Library/Application Support/fluxvm/images/agent-micro-arm64.raw}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-host.XXXXXX")"; PORT="${FLUXVM_LIVE_PORT:-7830}"
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
run_dir = "/tmp/fluxvm-run-host"
[catalog]
path = "$T/catalog.json"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

B="http://127.0.0.1:$PORT/v1/vms"; H="http://127.0.0.1:$PORT/v1/host"
jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
hget() { curl -fs "$H/$1" | jget "$2"; }
code() { curl -s -o /dev/null -w '%{http_code}' "$H/$1"; }

[[ "$(hget apple 'd["cpuCount"]')" == "$(sysctl -n hw.ncpu)" ]] && ok "apple: cpuCount matches this Mac ($(sysctl -n hw.ncpu))" || bad "cpuCount: $(hget apple 'd["cpuCount"]') vs $(sysctl -n hw.ncpu)"
[[ "$(hget apple 'd["physicalMemoryBytes"]')" == "$(sysctl -n hw.memsize)" ]] && ok "apple: physicalMemoryBytes matches hw.memsize" || bad "physicalMemoryBytes"
[[ "$(hget apple 'd["maximumVmCPUs"] >= d["cpuCount"] and d["maximumVmMemoryBytes"] > 0')" == True ]] && ok "apple: the per-guest limits are sane" || bad "limits: $(hget apple 'd')"
[[ "$(hget apple 'd["osVersion"]')" == *"$(sw_vers -productVersion | cut -d. -f1-2)"* ]] && ok "apple: osVersion names this macOS ($(sw_vers -productVersion))" || bad "osVersion: $(hget apple 'd["osVersion"]')"
[[ "$(hget apple 'd["vmNetworkingEntitlement"]')" == False ]] && ok "apple: vmNetworkingEntitlement is false (so bridged/shared vmnet is off; usernet is the path)" || echo "note vmNetworkingEntitlement=$(hget apple 'd["vmNetworkingEntitlement"]')"

[[ "$(hget apple/capabilities 'd["probed"]')" == True && "$(hget apple/capabilities 'd["os_version"]')" == "$(hget apple 'd["osVersion"]')" ]] && ok "capabilities: probed, and reports the same osVersion" || bad "capabilities header"
FEAT="$(hget apple/capabilities 'sorted(i["feature"] for i in d["items"])')"
[[ "$(hget apple/capabilities 'len(d["items"]) >= 5 and all(isinstance(i["supported"], bool) and isinstance(i["min_macos"], int) for i in d["items"])')" == True ]] && ok "capabilities: every item has supported + min_macos ($(hget apple/capabilities 'len(d["items"])') features)" || bad "capability items: $FEAT"
[[ "$(hget apple/capabilities '[i["feature"] for i in d["items"] if i["feature"].startswith("vcpus") and i["supported"] and i["available_here"]]' )" == '["vcpus"]' ]] && ok "capabilities: vcpus is supported and available here" || bad "vcpus capability"
UNAV="$(hget apple/capabilities '[i["feature"] for i in d["items"] if not i["available_here"]]')"
[[ "$(hget apple/capabilities 'all(i["reason"] for i in d["items"] if not i["available_here"])')" == True ]] && ok "capabilities: anything unavailable here carries a reason (unavailable: $UNAV)" || bad "unavailable without reason"

[[ "$(hget apple/ipsw 'd["url"].endswith(".ipsw") and d["min_cpus"] >= 1 and d["min_memory_bytes"] > 0')" == True ]] && ok "ipsw: names a restore image ($(hget apple/ipsw 'd["os_version"]')) with its minimums, nothing downloaded ($(hget apple/ipsw 'd["downloaded"]'))" || bad "ipsw: $(hget apple/ipsw 'd')"

[[ "$(hget overview 'd["host"]["cpu_count"]')" == "$(hget apple 'd["cpuCount"]')" && "$(hget overview 'd["host"]["os_version"]')" == "$(hget apple 'd["osVersion"]')" ]] && ok "overview: host block agrees with /host/apple" || bad "overview host: $(hget overview 'd["host"]')"
[[ "$(hget overview 'd["host"]["memory_total_mib"] * 1048576')" == "$(sysctl -n hw.memsize)" ]] && ok "overview: memory_total_mib is the machine's memory" || bad "memory_total_mib"
[[ "$(hget overview 'd["vms"]["total"]')" == 0 ]] && ok "overview: no guests yet" || bad "overview vms: $(hget overview 'd["vms"]')"
[[ "$(hget overview 'all(d["helpers"][k] for k in ("runner","usernet"))')" == True ]] && ok "overview: the runner and usernet helpers are found" || bad "helpers: $(hget overview 'd["helpers"]')"

M="$(curl -fs "$H/memory")"
[[ "$(echo "$M" | jget '0 < d["mem_available_mib"] <= d["mem_total_mib"]')" == True && "$(echo "$M" | jget 'd["level"] in ("normal","warn","critical","elevated","high")')" == True ]] && ok "memory: available <= total, level $(echo "$M" | jget 'd["level"]')" || bad "memory: $M"
[[ "$(echo "$M" | jget '[d["psi_some_avg10"], d["psi_full_avg10"]]')" == "[null, null]" ]] && ok "memory: no PSI on macOS (null, not zero)" || bad "psi: $M"
[[ "$(code topology)" == 200 && "$(hget topology 'd["devices"]')" == "[]" ]] && ok "topology: answers 200 with no devices on a Mac (it is a Linux/sysfs report)" || bad "topology"
[[ "$(code pressure)" == 200 && "$(hget pressure 'all(v is None for v in d.values())')" == True ]] && ok "pressure: answers 200 with every figure null on a Mac" || bad "pressure: $(hget pressure 'd')"
[[ "$(code images)" == 200 && "$(hget images 'd["builtin_cached"]')" == "[]" ]] && ok "images: nothing cached yet" || bad "images"

FREE0="$(hget storage 'd["volume"]["free_bytes"]')"
[[ "$(hget storage 'd["volume"]["total_bytes"] > d["volume"]["free_bytes"] > 0 and d["vms"] == []')" == True ]] && ok "storage: volume totals are sane and no guest uses space yet" || bad "storage: $(hget storage 'd')"

V="$(curl -sS -X POST "$B" -H 'content-type: application/json' -d '{"name":"hostprobe","backend":"vz","image":"agent-micro","vcpus":2,"memory_mib":1024,"network":{"mode":"user"},"apple":{"usernet":{}}}' | jget 'd["id"]')" || bad "create"
for _ in $(seq 1 30); do [[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && break; sleep 2; done
[[ "$(curl -fs "$B/$V" | jget 'd["status"]')" == running ]] && ok "a vz guest $V is running" || bad "guest not running"

[[ "$(hget overview 'd["vms"]["total"]')" == 1 && "$(hget overview 'd["vms"]["by_backend"].get("vz")')" == 1 && "$(hget overview 'd["vms"]["by_status"].get("running")')" == 1 ]] && ok "overview: counts the guest (total 1, by_backend vz, by_status running)" || bad "overview after create: $(hget overview 'd["vms"]')"
[[ "$(hget storage '[v["vm_id"] for v in d["vms"]]')" == "[\"$V\"]" && "$(hget storage 'd["vms"][0]["disk"]["allocated_bytes"] > 0 and d["vms"][0]["disk"]["allocated_bytes"] <= d["vms"][0]["disk"]["logical_bytes"]')" == True ]] && ok "storage: lists the guest with its disk (allocated <= logical, both non-zero)" || bad "storage vms: $(hget storage 'd["vms"]')"
echo "     storage entry: $(hget storage 'd["vms"][0]')"
MA="$(hget overview 'd["host"]["memory_available_mib"]')"
[[ "$(hget memory '0 < d["mem_available_mib"] <= d["mem_total_mib"]')" == True ]] && ok "memory: still sane with a guest up (available $(hget memory 'd["mem_available_mib"]') MiB, was $MA)" || bad "memory with guest"

curl -fs -X DELETE "$B/$V" >/dev/null || bad "delete"
for _ in $(seq 1 15); do [[ "$(hget overview 'd["vms"]["total"]')" == 0 ]] && break; sleep 1; done
[[ "$(hget overview 'd["vms"]["total"]')" == 0 && "$(hget storage 'd["vms"]')" == "[]" ]] && ok "after delete: overview back to 0 and storage lists no guest" || bad "after delete: $(hget overview 'd["vms"]') $(hget storage 'd["vms"]')"
echo "vz host: PASS"
