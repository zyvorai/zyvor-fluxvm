#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Times the AgentSession lifecycle against a running daemon, with curl so no CLI start-up is counted:
#   create -> first exec -> checkpoint (memory+disk) -> restore -> delete, N runs, median and worst.
#
#   FLUXVM_URL=http://127.0.0.1:7788 scripts/bench-agent-session.sh [runs=5] [policy=open]
#
# Works with macOS's bash 3.2. Needs curl and python3. It creates and deletes real sandboxes.
# Fork is not measured: it is only available on the in-tree KVM engine, not on vz.
set -euo pipefail

URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
RUNS="${1:-5}"
POLICY="${2:-open}"
AUTH=()
[ -n "${FLUXVM_TOKEN:-}" ] && AUTH=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-bench-agent.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

curl -fsS ${AUTH[@]+"${AUTH[@]}"} "$URL/healthz" >/dev/null || { echo "no daemon at $URL" >&2; exit 1; }

# post PATH BODY OUTFILE -> prints seconds
req() {
  local method="$1" path="$2" body="${3:-}" out="$4"
  if [ -n "$body" ]; then
    curl -sS ${AUTH[@]+"${AUTH[@]}"} -o "$out" -w '%{time_total}' -X "$method" -H 'Content-Type: application/json' -d "$body" "$URL$path"
  else
    curl -sS ${AUTH[@]+"${AUTH[@]}"} -o "$out" -w '%{time_total}' -X "$method" "$URL$path"
  fi
}

: > "$WORK/create"; : > "$WORK/exec"; : > "$WORK/checkpoint"; : > "$WORK/restore"; : > "$WORK/delete"
for i in $(seq 1 "$RUNS"); do
  t=$(req POST /v1/agents/sessions "{\"agent\":\"bench\",\"policy\":{\"preset\":\"$POLICY\"},\"ttl_seconds\":600}" "$WORK/s.json")
  ID=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["id"])' "$WORK/s.json")
  echo "$t" >> "$WORK/create"
  echo "$(req POST "/v1/agents/sessions/$ID/exec" '{"command":"true"}' "$WORK/e.json")" >> "$WORK/exec"
  echo "$(req POST "/v1/agents/sessions/$ID/checkpoint" '{"tag":"b1"}' "$WORK/c.json")" >> "$WORK/checkpoint"
  echo "$(req POST "/v1/agents/sessions/$ID/restore" '{"tag":"b1"}' "$WORK/r.json")" >> "$WORK/restore"
  echo "$(req DELETE "/v1/agents/sessions/$ID" '' "$WORK/d.json")" >> "$WORK/delete"
  echo "run $i/$RUNS done" >&2
done

python3 - "$WORK" "$RUNS" "$POLICY" <<'PY'
import sys, statistics
work, runs, policy = sys.argv[1], int(sys.argv[2]), sys.argv[3]
print(f"AgentSession lifecycle, {runs} runs, policy={policy} (seconds; median / worst)")
for step in ("create", "exec", "checkpoint", "restore", "delete"):
    xs = [float(l) for l in open(f"{work}/{step}") if l.strip()]
    print(f"  {step:<11} {statistics.median(xs):7.3f} / {max(xs):7.3f}")
PY
