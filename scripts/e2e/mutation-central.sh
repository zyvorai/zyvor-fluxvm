#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Do the central API and tunnel suites notice when a guard is removed? Copies the tree, breaks one guard at a time in the copy
# (the tenant route allow-list, the read-only key check, VM ownership, the audit write, the payment gate, the tunnel port policy
# and its read-only check), builds fluxvm-agent and runs scripts/e2e/agent-central-api.sh and agent-tunnel.sh against it; every
# mutant must make one of them FAIL. The real tree is never touched. A surviving mutant means a guard no test covers.
#   scripts/e2e/mutation-central.sh [ONLY=text to run the matching mutant only]   (Linux or macOS, cargo, minutes per mutant with a warm target dir)
# FLUXCTL points at a built fluxctl (the client is not mutated).
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
SRC="$PWD"; W="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-mut.XXXXXX")"; fails=0
trap 'rm -rf "$W"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
PROFILE="${PROFILE:-release}"; FLAG="--release"; [[ "$PROFILE" == debug ]] && FLAG=""
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"
FLUXCTL="${FLUXCTL:-$CARGO_TARGET_DIR/$PROFILE/fluxctl}"; [[ -x "$FLUXCTL" ]] || { echo "need a built fluxctl (FLUXCTL)" >&2; exit 2; }
rsync -a --exclude target --exclude .git "$SRC/" "$W/tree/" || exit 2
[[ -e "$SRC/../guestkit" ]] && ln -sfn "$(cd "$SRC/../guestkit" && pwd)" "$W/guestkit"

# name ~~ file ~~ exact text ~~ replacement
MUTANTS=(
 'the tenant route allow-list is skipped~~crates/fluxvm-agent/src/central.rs~~&& !tenant_route_allowed(req.method(), req.uri().path())~~&& false && !tenant_route_allowed(req.method(), req.uri().path())'
 'a read-only key may change things~~crates/fluxvm-agent/src/central.rs~~let refusal = if read_only && changes {~~let refusal = if false && changes {'
 'a tenant owns every VM~~crates/fluxvm-agent/src/tenancy.rs~~.is_some_and(|v| v.tenant.as_deref() == Some(t)),~~.is_some_and(|_v| true),'
 'changes are no longer written to the audit trail~~crates/fluxvm-agent/src/central.rs~~    fleet.audit.record(event);~~    let _ = event;'
 'the payment gate lets everyone through~~crates/fluxvm-agent/src/central_signup.rs~~    pending.then(|| {~~    false.then(|| {'
 'a tunnel ignores the port policy~~crates/fluxvm-agent/src/central_tunnel.rs~~if !policy.enabled || !policy.allows_port(port) {~~if false {'
 'a read-only key may open a tunnel~~crates/fluxvm-agent/src/central_tunnel.rs~~            if read_only {~~            if false {'
)
for m in "${MUTANTS[@]}"; do
  [[ -n "${ONLY:-}" && "$m" != *"$ONLY"* ]] && continue
  name="${m%%~~*}"; rest="${m#*~~}"; file="${rest%%~~*}"; rest="${rest#*~~}"; old="${rest%%~~*}"; new="${rest#*~~}"
  cp "$SRC/$file" "$W/tree/$file"
  python3 - "$W/tree/$file" "$old" "$new" <<'PY' || { bad "$name: pattern not found in $file (the mutant did not apply)"; continue; }
import sys
p, old, new = sys.argv[1:4]
s = open(p).read()
if s.count(old) != 1: sys.exit(1)
open(p, "w").write(s.replace(old, new))
PY
  ( cd "$W/tree" && CARGO_INCREMENTAL=0 cargo build $FLAG -p fluxvm-agent >"$W/build.log" 2>&1 ) || { bad "$name: the mutant does not compile"; tail -5 "$W/build.log"; cp "$SRC/$file" "$W/tree/$file"; continue; }
  killed=""
  for s in agent-central-api agent-tunnel; do
    if ! ( cd "$W/tree" && BIN="$CARGO_TARGET_DIR/$PROFILE/fluxvm-agent" FLUXCTL="$FLUXCTL" "scripts/e2e/$s.sh" >"$W/run-$s.log" 2>&1 ); then
      killed="$s ($(grep -c '^FAIL' "$W/run-$s.log") failing: $(grep '^FAIL' "$W/run-$s.log" | head -1 | cut -c6-80))"; break
    fi
  done
  if [[ -n "$killed" ]]; then ok "killed by $killed: $name"; else bad "SURVIVED: $name (both suites passed with the guard removed)"; [[ -n "${VERBOSE:-}" ]] && cat "$W"/run-*.log; fi
  cp "$SRC/$file" "$W/tree/$file"
done
echo; echo "$fails mutant(s) survived or did not apply"; exit "$fails"
