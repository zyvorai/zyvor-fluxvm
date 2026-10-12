#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# What does one agent sandbox cost on this Mac? For each profile and count, create that many sandboxes on a running
# daemon, let them idle, and read what the host really holds for them: the runner plus Virtualization XPC service RSS per VM
# (`usage[]` of GET /v1/host/overview) and the drop in host available memory. Prints a Markdown table with the Mac's RAM
# so the numbers can be pasted into docs/agent-density.md. Nothing here is an estimate: every figure is a sample.
#
#   scripts/bench-sandbox-mac.sh                       # tiny and small, 1 2 4 8 sandboxes
#   PROFILES="tiny standard" COUNTS="1 4" scripts/bench-sandbox-mac.sh
#   SETTLE=45 scripts/bench-sandbox-mac.sh             # seconds to idle before sampling (default 30)
#   FLUXVM_URL=http://127.0.0.1:7788 FLUXVM_TOKEN=... # daemon (auth header only if a token is set)
#
# Needs a running daemon on Apple silicon with a Linux image the sandboxes can use (the default `debian-13`), curl and jq.
# It deletes every sandbox it made, also on Ctrl-C. Run it on an otherwise quiet Mac; anything else using memory moves
# the "host available" column (the per-VM RSS column does not depend on it).
set -euo pipefail

URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
PROFILES="${PROFILES:-tiny small}"
COUNTS="${COUNTS:-1 2 4 8}"
SETTLE="${SETTLE:-30}"
AUTH=()
[[ -n "${FLUXVM_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")

command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
call() { curl -sf ${AUTH[@]+"${AUTH[@]}"} -H 'Content-Type: application/json' "$@"; }

ids=()
cleanup() {
  for id in ${ids[@]+"${ids[@]}"}; do call -X DELETE "$URL/v1/vms/$id" >/dev/null 2>&1 || true; done
  ids=()
}
trap cleanup EXIT
trap 'exit 130' INT TERM

overview() { call "$URL/v1/host/overview"; }
avail_mib() { overview | jq -r '.host.memory_available_mib'; }

total_mib="$(overview | jq -r '.host.memory_total_mib')"
echo "host: $(overview | jq -r '"macOS \(.host.os_version), \(.host.cpu_count) CPUs"'), ${total_mib} MiB RAM"
echo "daemon: $URL; idle ${SETTLE}s before sampling"
echo
echo "| Profile | Sandboxes | Create (s, each) | RSS per sandbox, MiB (min / mean / max) | Host available drop, MiB | Per sandbox, MiB | Pressure |"
echo "|---|---|---|---|---|---|---|"

for profile in $PROFILES; do
  for n in $COUNTS; do
    cleanup
    sleep 5
    base="$(avail_mib)"
    t0=$(date +%s)
    for i in $(seq 1 "$n"); do
      id="$(call -X POST "$URL/v1/sandboxes" \
        -d "{\"name\":\"bench-${profile}-${i}-$$\",\"profile\":\"${profile}\",\"ttl_seconds\":1800}" | jq -r .id)"
      ids+=("$id")
    done
    create_s=$(( ($(date +%s) - t0) / n ))
    sleep "$SETTLE"
    ov="$(overview)"
    # Per-VM RSS of exactly the sandboxes made here (runner plus XPC service when attributed).
    rss="$(printf '%s\n' "${ids[@]}" | jq -R . | jq -s --argjson ov "$ov" '
      . as $mine
      | [$ov.usage[] | select(.vm_id as $v | $mine | index($v)) | .rss_bytes / 1048576]
      | if length == 0 then null else {min: min, max: max, mean: (add / length)} end')"
    now="$(echo "$ov" | jq -r '.host.memory_available_mib')"
    pressure="$(echo "$ov" | jq -r '.host.memory_pressure')"
    drop=$(( base - now ))
    if [[ "$rss" == null ]]; then
      cell="n/a"
    else
      cell="$(echo "$rss" | jq -r '"\(.min|floor) / \(.mean|floor) / \(.max|floor)"')"
    fi
    per=$(( drop / n ))
    echo "| $profile | $n | $create_s | $cell | $drop | $per | $pressure |"
  done
done
cleanup
echo
echo "Sampled with $(overview | jq -r '.vms.total') VMs left on the host after cleanup."
