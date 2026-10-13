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
# With policy=offline (or developer-safe) a fork of 2 children is timed too, then the children are deleted:
# fork needs a session with no network card of its own, so it is skipped for policy=open.
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
    curl -sS ${AUTH[@]+"${AUTH[@]}"} -o "$out" -m 180 -w '%{time_total}' -X "$method" -H 'Content-Type: application/json' -d "$body" "$URL$path"
  else
    curl -sS ${AUTH[@]+"${AUTH[@]}"} -o "$out" -m 180 -w '%{time_total}' -X "$method" "$URL$path"
  fi
}

FAILS=0
: > "$WORK/create"; : > "$WORK/exec"; : > "$WORK/checkpoint"; : > "$WORK/restore"; : > "$WORK/fork"; : > "$WORK/delete"
for i in $(seq 1 "$RUNS"); do
  t=$(req POST /v1/agents/sessions "{\"agent\":\"bench\",\"policy\":{\"preset\":\"$POLICY\"},\"ttl_seconds\":600}" "$WORK/s.json")
  ID=$(python3 -c 'import json,sys
try: print(json.load(open(sys.argv[1]))["id"])
except Exception: print("")' "$WORK/s.json")
  if [ -z "$ID" ]; then
    # A failed create is a result, not a reason to stop: count it and show why.
    FAILS=$((FAILS + 1))
    echo "run $i/$RUNS: create failed after ${t}s: $(head -c 300 "$WORK/s.json")" >&2
    continue
  fi
  echo "$t" >> "$WORK/create"
  echo "$(req POST "/v1/agents/sessions/$ID/exec" '{"command":"true"}' "$WORK/e.json")" >> "$WORK/exec"
  echo "$(req POST "/v1/agents/sessions/$ID/checkpoint" '{"tag":"b1"}' "$WORK/c.json")" >> "$WORK/checkpoint"
  echo "$(req POST "/v1/agents/sessions/$ID/restore" '{"tag":"b1"}' "$WORK/r.json")" >> "$WORK/restore"
  if [ "$POLICY" != "open" ]; then
    echo "$(req POST "/v1/agents/sessions/$ID/fork" '{"count":2}' "$WORK/f.json")" >> "$WORK/fork"
    for kid in $(python3 -c 'import json,sys;print(" ".join(c["id"] for c in json.load(open(sys.argv[1])).get("items", [])))' "$WORK/f.json"); do
      req DELETE "/v1/agents/sessions/$kid" '' "$WORK/k.json" >/dev/null
    done
  fi
  echo "$(req DELETE "/v1/agents/sessions/$ID" '' "$WORK/d.json")" >> "$WORK/delete"
  echo "run $i/$RUNS done" >&2
done

python3 - "$WORK" "$RUNS" "$POLICY" "$FAILS" <<'PY'
import sys, statistics
work, runs, policy, fails = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
print(f"AgentSession lifecycle, {runs} runs, policy={policy} (seconds; median / worst; {fails} create failures)")
for step in ("create", "exec", "checkpoint", "restore", "fork", "delete"):
    xs = [float(l) for l in open(f"{work}/{step}") if l.strip()]
    if not xs:
        print(f"  {step:<11}     n/a (not measured for this policy)")
        continue
    print(f"  {step:<11} {statistics.median(xs):7.3f} / {max(xs):7.3f}")
PY
