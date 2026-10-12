#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Checkout / return latency of the macOS warm pool, against a local fluxctl serve on a Mac.
#
# Needs a macOS guest that is set up and logged in, and an unlocked login session (restore fails on a locked screen).
# Enrol it once (timed here when ENROLL_VM is set), or enrol it yourself with `fluxctl macos-pool enroll <id>`.
#
# Optional:
#   FLUXVM_API=http://127.0.0.1:7788   FLUXVM_TOKEN=...   (never commit tokens)
#   BENCH_N=5                          checkout/return rounds
#   ENROLL_VM=<vm id>                  enrol this running guest first and time it
#   SETTLE_SECS=20                     wait after checkout before sampling memory and returning
#
# Prints per-round timings and min/median/max. Numbers are for the Mac it runs on; record the model and macOS build
# beside them if they go into docs/mac-cloud.md.

set -euo pipefail

API="${FLUXVM_API:-http://127.0.0.1:7788}"
N="${BENCH_N:-5}"
SETTLE="${SETTLE_SECS:-20}"
AUTH_HDR=()
if [[ -n "${FLUXVM_TOKEN:-}" ]]; then
  AUTH_HDR=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "bench-macos-pool: macOS host required (skipped on $(uname -s))" >&2
  exit 0
fi
for tool in curl jq python3; do
  command -v "$tool" >/dev/null || { echo "bench-macos-pool: $tool is required" >&2; exit 1; }
done

now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

BODY=$(mktemp)
trap 'rm -f "$BODY"' EXIT

# post PATH -> prints the response body; on a non-2xx status prints it to stderr and fails.
post() {
  local code
  code=$(curl -s -o "$BODY" -w '%{http_code}' -X POST "${API}$1" "${AUTH_HDR[@]}") || code=000
  if [[ "$code" != 2* ]]; then
    echo "POST $1 -> ${code}: $(cat "$BODY")" >&2
    return 1
  fi
  cat "$BODY"
}

stats() { # name, then ms values
  local name="$1"; shift
  printf '%s\n' "$@" | sort -n | awk -v n="$name" '
    { v[NR] = $1 }
    END { printf "%-9s n=%d min=%dms median=%dms max=%dms\n", n, NR, v[1], v[int((NR + 1) / 2)], v[NR] }'
}

echo "== FluxVM macOS pool benchmark (n=${N}, api=${API}, host=$(sysctl -n hw.model), macOS $(sw_vers -productVersion)) =="

if [[ -n "${ENROLL_VM:-}" ]]; then
  t0=$(now_ms)
  post "/v1/macos-pool/${ENROLL_VM}/enroll" >/dev/null
  echo "enroll    $(( $(now_ms) - t0 ))ms (snapshot + stop)"
fi

checkouts=(); returns=()
for i in $(seq 1 "$N"); do
  t0=$(now_ms)
  body=$(post /v1/macos-pool/checkout)
  t1=$(now_ms)
  id=$(jq -r '.id' <<<"$body")
  pid=$(jq -r '.pid // empty' <<<"$body")
  sleep "$SETTLE"
  # The guest's memory is held by the runner and Virtualization's XPC service; the runner RSS is the lower bound.
  rss_kib=""
  [[ -n "$pid" ]] && rss_kib=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ' || true)
  t2=$(now_ms)
  post "/v1/macos-pool/${id}/return" >/dev/null
  t3=$(now_ms)
  checkouts+=("$((t1 - t0))"); returns+=("$((t3 - t2))")
  echo "round ${i}: checkout $((t1 - t0))ms return $((t3 - t2))ms runner_rss_mib=${rss_kib:+$((rss_kib / 1024))}"
done

stats checkout "${checkouts[@]}"
stats return "${returns[@]}"
