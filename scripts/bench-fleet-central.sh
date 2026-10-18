#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Measures how `fluxvm-agent central` copes with N simulated nodes heartbeating it (docs/fleet-scale.md).
# For each scenario it starts a fresh central on a loopback port with an empty state directory, runs
# `fluxvm-agent sim-fleet` against it, and prints one line of numbers: latency, failures, how many nodes ended
# up registered and healthy, central's CPU time and memory, and the size of the persisted registry.
#
#   scripts/bench-fleet-central.sh [-b path/to/fluxvm-agent] [NODES:INTERVAL_SECS:DURATION_SECS ...]
#   scripts/bench-fleet-central.sh 1000:10:30 3000:10:40 5000:10:40       (these are the defaults)
#
# Build the binary with `cargo build --release -p fluxvm-agent`; a debug build says nothing about speed.
# Raise the open-file limit first for many nodes (ulimit -n 10240). Exit status is the number of scenarios
# whose run failed (a failed beat, a missing or unhealthy node). Read docs/fleet-scale.md for what the numbers
# do and do not show.
set -uo pipefail

BIN="${BIN:-target/release/fluxvm-agent}"
if [[ "${1:-}" == "-b" ]]; then
  BIN="${2:?-b needs a path}"
  shift 2
fi
[[ -x "$BIN" ]] || { echo "bench-fleet-central: $BIN is not an executable (cargo build --release -p fluxvm-agent)" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "bench-fleet-central: python3 is required to read the reports" >&2; exit 2; }

scenarios=("$@")
[[ ${#scenarios[@]} -eq 0 ]] && scenarios=(1000:10:30 3000:10:40 5000:10:40)

pids=()
dirs=()
# shellcheck disable=SC2329  # called by the EXIT trap
cleanup() {
  local p d
  for p in "${pids[@]:-}"; do [[ -n "$p" ]] && kill "$p" 2>/dev/null; done
  for d in "${dirs[@]:-}"; do [[ -n "$d" ]] && rm -rf "$d"; done
}
trap cleanup EXIT

echo "host: $(uname -sm), $(getconf _NPROCESSORS_ONLN 2>/dev/null || echo ?) cpus, open-file limit $(ulimit -n); binary: $BIN"
failed=0
for sc in "${scenarios[@]}"; do
  IFS=: read -r nodes interval duration <<<"$sc"
  [[ -n "${nodes:-}" && -n "${interval:-}" && -n "${duration:-}" ]] || { echo "bad scenario '$sc' (want NODES:INTERVAL:DURATION)" >&2; exit 2; }
  d="$(mktemp -d)"
  dirs+=("$d")
  port=$((20000 + RANDOM % 20000))
  "$BIN" central --listen "127.0.0.1:${port}" --state-dir "$d" --token bench >"$d/central.log" 2>&1 &
  cp=$!
  pids+=("$cp")
  # Wait until central answers (it does not need to be healthy for the scenario to start).
  for _ in $(seq 1 50); do
    curl -s -o /dev/null -H 'Authorization: Bearer bench' "http://127.0.0.1:${port}/fleet/nodes" && break
    sleep 0.2
  done
  "$BIN" sim-fleet --central "http://127.0.0.1:${port}" --token bench --nodes "$nodes" \
    --interval-secs "$interval" --duration-secs "$duration" >"$d/report.json" 2>"$d/sim.err"
  rc=$?
  usage="$(/bin/ps -o rss=,time= -p "$cp" 2>/dev/null | awk '{printf "rss=%dMiB cpu=%s", $1/1024, $2}')"
  size="$(wc -c <"$d/fleet-nodes.json" 2>/dev/null | tr -d ' ')"
  kill "$cp" 2>/dev/null
  wait "$cp" 2>/dev/null
  echo "== ${nodes} nodes, ${interval}s interval, ${duration}s (target $((nodes / interval)) beats/s): central ${usage:-?}, fleet-nodes.json ${size:-none} bytes"
  python3 - "$d/report.json" "$d/sim.err" <<'PY'
import json, sys
try:
    r = json.load(open(sys.argv[1]))
except Exception as e:
    print("   no report:", e)
    try:
        print("   " + open(sys.argv[2]).read().strip()[:300])
    except Exception:
        pass
    sys.exit()
l, c = r["latency_ms"], r["check"]
print("   beats sent=%d ok=%d timeouts/transport=%d 429=%d 5xx=%d, achieved %.0f/s" % (
    r["beats_sent"], r["beats_ok"], r["transport_errors"], r["http_429"], r["http_5xx"], r["beats_per_sec"]))
print("   latency ms p50=%d p90=%d p99=%d max=%d; registered %d/%d, missing %d, duplicates %d, unhealthy %d -> %s" % (
    l["p50"], l["p90"], l["p99"], l["max"], c["registered"], r["nodes"], c["missing"], c["duplicates"], c["unhealthy"],
    "PASS" if r["beats_ok"] == r["beats_sent"] and c["ok"] else "FAIL"))
PY
  [[ $rc -ne 0 ]] && failed=$((failed + 1))
done
exit "$failed"
