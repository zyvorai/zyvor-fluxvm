#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The Python and Go SDKs against a REAL daemon, not the mock servers their unit tests use, so the SDKs cannot drift from the
# REST API unnoticed. Starts an isolated `fluxctl serve` (loopback, own port and state dir, [sandbox.procbox] enabled; Linux)
# and runs python/tests/live_procbox.py and `go test -tags live -run LiveProcbox`. With FLUXVM_LIVE_URL set it uses that daemon
# instead (it must have procbox enabled), e.g. through an ssh tunnel from a machine that has Go. Exit status = failed suites.
#   scripts/e2e/sdk-live.sh        (needs fluxctl on PATH or FLUXVM_BIN, python3; go is optional)
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
T="$(mktemp -d)"; fails=0; DP=""
cleanup() { [[ -n "$DP" ]] && kill "$DP" 2>/dev/null; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
URL="${FLUXVM_LIVE_URL:-}"
if [[ -z "$URL" ]]; then
  FX="${FLUXVM_BIN:-$(command -v fluxctl)}"; [[ -x "$FX" ]] || { echo "needs fluxctl" >&2; exit 2; }
  [[ "$(uname -s)" == Linux ]] || { echo "procbox is Linux-only: set FLUXVM_LIVE_URL to a Linux daemon" >&2; exit 2; }
  PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; mkdir -p "$T/state" "$T/run"
  cat > "$T/fluxvm.toml" <<TOML
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "$T/run"
[sandbox.procbox]
enabled = true
TOML
  "$FX" --config "$T/fluxvm.toml" serve >"$T/serve.log" 2>&1 & DP=$!
  for _ in $(seq 1 30); do curl -sf "$URL/healthz" >/dev/null && break; sleep 1; done
  curl -sf "$URL/healthz" >/dev/null && ok "isolated daemon up at $URL (procbox enabled)" || { bad "daemon did not start: $(tail -3 "$T/serve.log")"; exit 1; }
fi
export FLUXVM_LIVE_URL="$URL"
if python3 -I -m unittest discover -v -s python/tests -p 'live_procbox.py' >"$T/py.log" 2>&1; then ok "python SDK: live procbox flow ($(grep -c '\.\.\. ok' "$T/py.log") test(s))"
else bad "python SDK live procbox flow"; sed 's/^/     | /' "$T/py.log" | tail -15; fi
if command -v go >/dev/null; then
  if (cd go && go test -count=1 -tags live -run 'LiveProcbox' ./... >"$T/go.log" 2>&1); then ok "go SDK: live procbox flow"
  else bad "go SDK live procbox flow"; sed 's/^/     | /' "$T/go.log" | tail -15; fi
else echo "skip go SDK: go is not installed here"; fi
echo; echo "$fails check(s) failed"; exit "$fails"
