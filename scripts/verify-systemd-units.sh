#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Runs `systemd-analyze verify` over the host units this repo ships (packaging/systemd and deploy/systemd) and
# fails on anything real: unknown directives, bad values, a Requires=/After= on a unit that is not shipped.
#
# Two kinds of output are expected in CI and ignored:
#   - "Command /usr/bin/x is not executable": the binaries are not installed where this runs;
#   - lines about the build host's own units under /usr/lib/systemd or /lib/systemd.
#
#   scripts/verify-systemd-units.sh [UNIT_DIR...]      (default: packaging/systemd deploy/systemd)
#
# Needs systemd-analyze (Linux). Elsewhere it prints a notice and exits 0, unless REQUIRE_SYSTEMD_ANALYZE=1.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if ! command -v systemd-analyze >/dev/null 2>&1; then
  echo "verify-systemd-units: systemd-analyze not found; skipped" >&2
  [[ "${REQUIRE_SYSTEMD_ANALYZE:-0}" == 1 ]] && exit 2
  exit 0
fi

dirs=("$@")
[[ ${#dirs[@]} -eq 0 ]] && dirs=("$ROOT/packaging/systemd" "$ROOT/deploy/systemd")

# Verify against a scratch directory holding every shipped unit side by side, so dependencies between units
# that ship together (fluxvm-memprof -> fluxvm-intelligence) resolve.
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
for d in "${dirs[@]}"; do
  [[ -d "$d" ]] || { echo "verify-systemd-units: no such directory: $d" >&2; exit 2; }
  find "$d" -maxdepth 1 -type f \( -name '*.service' -o -name '*.timer' \) -exec cp {} "$scratch"/ \;
done

fail=0
count=0
for unit in "$scratch"/*.service "$scratch"/*.timer; do
  [[ -e "$unit" ]] || continue
  count=$((count + 1))
  problems="$(systemd-analyze verify "$unit" 2>&1 \
    | grep -Ev '^/(usr/)?lib/systemd/' \
    | grep -Ev 'is not executable: No such file or directory' \
    | grep -v '^[[:space:]]*$' || true)"
  if [[ -n "$problems" ]]; then
    fail=$((fail + 1))
    echo "FAIL $(basename "$unit")"
    # shellcheck disable=SC2001
    echo "$problems" | sed 's/^/     /'
  fi
done

if [[ $count -eq 0 ]]; then
  echo "verify-systemd-units: no units found in: ${dirs[*]}" >&2
  exit 2
fi
if [[ $fail -ne 0 ]]; then
  echo "verify-systemd-units: $fail of $count units have problems"
  exit 1
fi
echo "verify-systemd-units: $count units verified"
