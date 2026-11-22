#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

# Offline test of scripts/stack-check.sh with fake curl, kubectl and fluxctl (no daemon, cluster or network needed).
# shellcheck disable=SC2034,SC2120  # OUT and RC are read inside the eval'd expectations; run takes optional arguments
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="$HERE/stack-check.sh"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin"
FAILS=0
ok() { echo "  ok    $1"; }
bad() { echo "  FAIL  $1"; FAILS=$((FAILS + 1)); }
expect() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# curl succeeds for every URL listed in $UP (space separated URL prefixes), else fails like curl -f.
cat > "$T/bin/curl" <<'SH'
#!/bin/sh
for a in "$@"; do url="$a"; done
for u in $UP; do case "$url" in "$u"*) exit 0 ;; esac; done
exit 22
SH
# kubectl: "get nodes" prints $NODES (fails when empty); "api-resources" prints $RES; "get nodes -l ..." prints $MACNODES.
cat > "$T/bin/kubectl" <<'SH'
#!/bin/sh
case "$*" in
  *api-resources*) printf '%s' "$RES" ;;
  *"-l kairon.zyvor.dev/backend.vz=true"*) printf '%s' "$MACNODES" ;;
  *"get nodes"*) [ -n "$NODES" ] && printf '%s\n' "$NODES" || exit 1 ;;
esac
SH
cat > "$T/bin/fluxctl" <<'SH'
#!/bin/sh
printf '%s\n' "$DOCTOR"
SH
chmod +x "$T/bin/"*

run() { PATH="$T/bin:$PATH" STACK_CHECK_TIMEOUT=1 FLUXCTL="$T/bin/fluxctl" "$CHECK" "$@" 2>&1; }
export UP="http://127.0.0.1:7788 http://127.0.0.1:32302 http://127.0.0.1:5151"
export DOCTOR="ok    login session          a user is logged in and the screen is unlocked"
export NODES="mac-1 Ready <none> 1d v1.30"
export RES="machines true Machine"
export MACNODES="mac-1 Ready <none> 1d v1.30"

OUT="$(run)"; RC=$?
expect "everything up: exit 0" '[ "$RC" -eq 0 ]'
expect "everything up: no layer failed" '[[ "$OUT" == *"no layer failed"* && "$OUT" != *FAIL* ]]'
expect "everything up: the console URL is shown" '[[ "$OUT" == *"/console"* ]]'

OUT="$(UP="http://127.0.0.1:5151" run)"; RC=$?
expect "no daemon: exit 1 with its fix" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"fluxctl service install"* ]]'

export DOCTOR="warn  login session          the screen is locked"
OUT="$(run)"; RC=$?
expect "locked screen fails the session layer and says how to fix it" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"locked or nobody logged in"* && "$OUT" == *"auto-login"* ]]'
export DOCTOR="ok    login session          fine"

OUT="$(NODES="" run)"; RC=$?
expect "no Kubernetes API: fails, and the layers that need it are skipped, not failed" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"fluxctl kube up"* && "$OUT" == *"skip  crds"* && "$OUT" == *"skip  node"* ]]'

OUT="$(RES="" run)"; RC=$?
expect "no Kairon CRDs: names the apply command" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"deploy/crd.yaml"* ]]'

OUT="$(MACNODES="" run)"; RC=$?
expect "no Mac node: points at macos-node.sh" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"macos-node.sh install"* ]]'

OUT="$(MACNODES="mac-1 NotReady <none> 1d v1" run)"; RC=$?
expect "a NotReady node fails" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"not Ready"* ]]'

OUT="$(UP="http://127.0.0.1:7788 http://127.0.0.1:5151" run)"; RC=$?
expect "kairon-node not installed is a skip, not a failure" '[ "$RC" -eq 0 ] && [[ "$OUT" == *"skip  kairon-node"* ]]'

OUT="$(UP="http://127.0.0.1:7788 http://127.0.0.1:32302/healthz http://127.0.0.1:5151" run)"; RC=$?
expect "kairon-node up but not ready fails with the FluxVM hint" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"cannot reach the FluxVM daemon"* ]]'

expect "an unknown option is refused" '! "$CHECK" --frobnicate >/dev/null 2>&1'

echo
if [ "$FAILS" -eq 0 ]; then echo "all passed"; else echo "$FAILS failed"; exit 1; fi
