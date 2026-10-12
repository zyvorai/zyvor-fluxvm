#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# An agent's loop in a safe, forkable sandbox on a Mac, with real timings:
#   1. an offline sandbox (no network card at all) holds a small project whose test fails;
#   2. it is forked into four running copies that share its memory snapshot;
#   3. each copy tries a different fix and runs the tests, in parallel;
#   4. the copy whose tests pass is kept (its diff is printed) and everything else is deleted.
# The "agent" here is four fixed candidate patches, so the demo is deterministic; swap them for model output (the same
# calls are the MCP tools sandbox_create, sandbox_write_file, vm_fork, sandbox_exec and vm_delete).
#
#   examples/agent-sandbox-demo/run.sh            # FLUXVM_URL (default http://127.0.0.1:7788), FLUXVM_TOKEN if auth is on
# Needs a running daemon on Apple silicon with the default Linux image, curl and jq. Everything it makes is deleted.
# The single-quoted commands below are meant to expand inside the guest (for example $HOME), not here.
# shellcheck disable=SC2016
set -euo pipefail

URL="${FLUXVM_URL:-http://127.0.0.1:7788}"
AUTH=()
[[ -n "${FLUXVM_TOKEN:-}" ]] && AUTH=(-H "Authorization: Bearer ${FLUXVM_TOKEN}")
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
call() { curl -sf ${AUTH[@]+"${AUTH[@]}"} -H 'Content-Type: application/json' "$@"; }
now() { python3 -c 'import time; print(time.time())'; }
since() { python3 -c "import time; print(f'{time.time() - $1:.1f}')"; }
exec_in() { # id command -> the command's stdout (it echoes its own exit status where one is needed)
  call -X POST "$URL/v1/sandboxes/$1/process" -d "$(jq -n --arg c "$2" '{command: $c}')" | jq -r '.stdout'
}

ids=()
cleanup() { for id in ${ids[@]+"${ids[@]}"}; do call -X DELETE "$URL/v1/vms/$id" >/dev/null 2>&1 || true; done; }
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "1. offline sandbox"
t=$(now)
base="$(call -X POST "$URL/v1/sandboxes" \
  -d "{\"name\":\"agent-demo-$$\",\"profile\":\"standard\",\"offline\":true,\"ttl_seconds\":1200}" | jq -r .id)"
ids+=("$base")
echo "   $base ready in $(since "$t") s; network: $(exec_in "$base" 'ip -br a | tr -s " " | tr "\n" ";"')"

echo "2. a project whose test fails"
exec_in "$base" 'mkdir -p "$HOME/work" && cd "$HOME/work" && cat > calc.py <<PY
def average(xs):
    return sum(xs) / len(xs) + 1   # bug: an off-by-one nobody asked for


def clamp(x, lo, hi):
    return min(hi, max(lo, x))
PY
cat > test_calc.py <<PY
import unittest
from calc import average, clamp

class T(unittest.TestCase):
    def test_average(self):
        self.assertEqual(average([2, 4, 6]), 4)
    def test_clamp(self):
        self.assertEqual(clamp(15, 0, 10), 10)
        self.assertEqual(clamp(-5, 0, 10), 0)
PY
cp calc.py calc.py.orig; python3 -m unittest -q 2>&1 | tail -3' >/dev/null
out="$(exec_in "$base" 'cd "$HOME/work" && python3 -m unittest >t.out 2>&1; rc=$?; tail -1 t.out; echo "exit=$rc"')"; echo "   before the fix: ${out//$'\n'/ | }"

echo "3. fork into four running copies"
t=$(now)
fork="$(call -X POST "$URL/v1/vms/$base/fork" -d '{"count":4,"namePrefix":"attempt","identity":"best-effort"}')"
kids=()
while IFS= read -r k; do kids+=("$k"); done < <(echo "$fork" | jq -r '.fork.children[].id')   # bash 3.2: no mapfile
ids+=("${kids[@]}")
echo "   ${#kids[@]} copies running in $(since "$t") s (parent snapshot $(echo "$fork" | jq -r .fork.parent_snapshot_ms) ms, slowest child $(echo "$fork" | jq -r '[.fork.children[].total_ms]|max') ms)"

echo "4. four different fixes, tried in parallel"
patches=(
  "sed -i 's/ + 1   # bug.*//' calc.py"                                  # right
  "sed -i 's/ + 1   # bug.*/ + 0.5/' calc.py"                            # wrong
  "sed -i 's/sum(xs) \\/ len(xs)/sum(xs)/' calc.py"                       # wrong
  "sed -i 's/ + 1   # bug.*//; s/min(hi, max(lo, x))/max(hi, min(lo, x))/' calc.py"  # fixes one bug, breaks another
)
t=$(now)
tmp="$(mktemp -d)"
for i in "${!kids[@]}"; do
  (
    exec_in "${kids[$i]}" "cd \"\$HOME/work\" && ${patches[$i]}" >/dev/null
    res="$(exec_in "${kids[$i]}" 'cd "$HOME/work" && python3 -m unittest -q >/dev/null 2>&1; echo $?')"
    echo "${res//$'\n'/}" > "$tmp/$i"
  ) &
done
wait
echo "   all four tried and tested in $(since "$t") s"
winner=""
for i in "${!kids[@]}"; do
  rc="$(cat "$tmp/$i")"
  echo "   attempt-$((i + 1)): tests $([[ "$rc" == 0 ]] && echo PASS || echo "fail (rc=$rc)")"
  if [[ "$rc" == 0 && -z "$winner" ]]; then winner="${kids[$i]}"; fi
done
rm -rf "$tmp"
[[ -n "$winner" ]] || { echo "no attempt passed" >&2; exit 1; }

echo "5. keep the one that passes"
echo "   its change:"
exec_in "$winner" 'cd "$HOME/work" && diff -u calc.py.orig calc.py | sed "s/^/     /"' || true
for id in "${ids[@]}"; do
  [[ "$id" == "$winner" ]] || { call -X DELETE "$URL/v1/vms/$id" >/dev/null 2>&1 || true; }
done
ids=("$winner")
echo "   the other four are deleted; the winner is $winner (deleted on exit)"
