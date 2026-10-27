#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Checks integrations/raycast: every script parses, is executable, and carries the headers Raycast needs. Does not run
# Raycast or fluxctl.
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
for f in integrations/raycast/*.sh; do
  bash -n "$f" || { echo "FAIL $f: does not parse"; fail=1; }
  [[ -x "$f" ]] || { echo "FAIL $f: not executable"; fail=1; }
  for h in schemaVersion title mode packageName icon; do
    grep -q "^# @raycast\.$h " "$f" || { echo "FAIL $f: missing @raycast.$h"; fail=1; }
  done
  grep -Eq '^# @raycast\.mode (compact|fullOutput|silent|inline)$' "$f" || { echo "FAIL $f: bad mode"; fail=1; }
  grep -q '"\$FLUXCTL"' "$f" || { echo "FAIL $f: does not call fluxctl"; fail=1; }
done
[[ $fail -eq 0 ]] && echo "ok: $(ls integrations/raycast/*.sh | wc -l | tr -d ' ') raycast scripts"
exit $fail
