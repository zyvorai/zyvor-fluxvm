#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2029
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# scripts/deploy-remote.sh against a real host, from the control machine: --help, --dry-run (changes nothing),
# --preflight-only, --verify-only, then a real --quick deploy twice. The second deploy is the idempotency check:
# same binary, service still active, no new failed units. Restarts the host's fluxvm.service (that is what a deploy is).
#   scripts/e2e/deploy-remote.sh sus@212.8.248.187
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
TARGET="${1:?user@host}"; HOST="${TARGET#*@}"; USER_="${TARGET%@*}"
D=scripts/deploy-remote.sh; T="$(mktemp -d)"; fails=0
trap 'rm -rf "$T"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
R() { ssh -o BatchMode=yes "$TARGET" "$@"; }
snap() { R 'printf "%s %s %s\n" "$(sha256sum /usr/local/bin/fluxctl | cut -c1-16)" "$(systemctl is-active fluxvm)" "$(systemctl --failed --no-legend | wc -l)"'; }

check "--help exits 0 and documents --verify-only" bash -c "$D --help | grep -q -- --verify-only"
check "an unknown option is refused" bash -c "! $D $HOST $USER_ --nonsense >/dev/null 2>&1"
S0="$(snap)"; echo "     | before: $S0"
FLUXVM_DEPLOY_LOG="$T/dry.log" $D "$HOST" "$USER_" --key --dry-run >"$T/dry.out" 2>&1; rc=$?
check "--dry-run succeeds" test $rc -eq 0
check "--dry-run changed nothing on the host" test "$(snap)" = "$S0"
FLUXVM_DEPLOY_LOG="$T/pre.log" $D "$HOST" "$USER_" --key --preflight-only >"$T/pre.out" 2>&1; rc=$?
[[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/pre.out" | tail -15
check "--preflight-only succeeds on the host" test $rc -eq 0
check "--preflight-only changed nothing" test "$(snap)" = "$S0"
FLUXVM_DEPLOY_LOG="$T/ver.log" $D "$HOST" "$USER_" --key --verify-only >"$T/ver.out" 2>&1; rc=$?
[[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/ver.out" | tail -15
check "--verify-only runs the remote preflight (rc=$rc)" test $rc -eq 0
check "--verify-only changed nothing" test "$(snap)" = "$S0"

FLUXVM_DEPLOY_LOG="$T/d1.log" $D "$HOST" "$USER_" --key --quick >"$T/d1.out" 2>&1; rc=$?
[[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/d1.out" | tail -25
check "deploy 1 (--quick) succeeds" test $rc -eq 0
S1="$(snap)"; echo "     | after deploy 1: $S1"
check "service is active after deploy 1" test "$(echo "$S1" | cut -d' ' -f2)" = active
check "fluxctl on the host runs and reports a version" bash -c "ssh -o BatchMode=yes $TARGET '/usr/local/bin/fluxctl --version' | grep -q ."
FLUXVM_DEPLOY_LOG="$T/d2.log" $D "$HOST" "$USER_" --key --quick >"$T/d2.out" 2>&1; rc=$?
[[ $rc -ne 0 ]] && sed 's/^/     | /' "$T/d2.out" | tail -25
check "deploy 2 (idempotent re-run) succeeds" test $rc -eq 0
S2="$(snap)"; echo "     | after deploy 2: $S2"
check "re-run leaves the same binary" test "$(echo "$S1" | cut -d' ' -f1)" = "$(echo "$S2" | cut -d' ' -f1)"
check "service active after the re-run" test "$(echo "$S2" | cut -d' ' -f2)" = active
check "no more failed units than before" test "$(echo "$S2" | cut -d' ' -f3)" -le "$(echo "$S0" | cut -d' ' -f3)"
check "deploy log was written" test -s "$T/d2.log"
echo; echo "$fails check(s) failed"; exit "$fails"
