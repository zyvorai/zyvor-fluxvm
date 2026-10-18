#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Does the enrollment security suite notice when a guard is removed? Copies the tree, breaks one guard at a time in the
# copy, builds fluxvm-agent and runs scripts/e2e/enroll-security.sh against it; every mutant must make the suite FAIL.
# The real tree is never touched. A surviving mutant means a guard no test covers.
#   scripts/e2e/mutation-security.sh [ONLY=text to run the matching mutant only]   (Linux, cargo, a few minutes per mutant with a warm target dir)
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
SRC="$PWD"; W="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-mut.XXXXXX")"; fails=0
trap 'rm -rf "$W"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
rsync -a --exclude target --exclude .git "$SRC/" "$W/tree/" || exit 2
# guestkit lives next to the checkout
[[ -e "$SRC/../guestkit" ]] && ln -sfn "$(cd "$SRC/../guestkit" && pwd)" "$W/guestkit"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$SRC/target}"

# name ~~ file ~~ exact text ~~ replacement
MUTANTS=(
 'enrolled mode no longer refuses the fleet token~~crates/fluxvm-agent/src/central.rs~~req.uri().path() == "/fleet/register" && enroll::node_auth() == enroll::NodeAuth::Enrolled~~false'
 "a revoked node's credential still authenticates~~crates/fluxvm-agent/src/central_enroll.rs~~if e.revoked || self.secret.is_empty() {~~if self.secret.is_empty() {"
 'a revoked join token still enrolls~~crates/fluxvm-agent/src/central_enroll.rs~~ || t.revoked || t.expires~~ || t.expires'
 'an expired join token still enrolls~~crates/fluxvm-agent/src/central_enroll.rs~~|| t.expires.is_some_and(|e| e <= now)~~'
 'the credential mac is not checked~~crates/fluxvm-agent/src/central_enroll.rs~~eq(mac, &self.mac(name, node_id, generation)).then(|| name.clone())~~Some(name.clone())'
 'the rotation window never closes~~crates/fluxvm-agent/src/central_enroll.rs~~e.prev_valid_until.is_some_and(|t| t > now)~~e.prev_valid_until.is_some()'
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
  ( cd "$W/tree" && CARGO_INCREMENTAL=0 cargo build --release -p fluxvm-agent >"$W/build.log" 2>&1 ) || { bad "$name: the mutant does not compile"; tail -5 "$W/build.log"; cp "$SRC/$file" "$W/tree/$file"; continue; }
  if ( cd "$W/tree" && BIN="$CARGO_TARGET_DIR/release/fluxvm-agent" scripts/e2e/enroll-security.sh >"$W/run.log" 2>&1 ); then
    # The shell suite cannot wait out an hour-long window; the unit tests can pass the clock in.
    if ( cd "$W/tree" && CARGO_INCREMENTAL=0 cargo test -p fluxvm-agent --lib enroll::tests >"$W/unit.log" 2>&1 ); then
      bad "SURVIVED: $name (the e2e suite and the unit tests passed with the guard removed)"
    else ok "killed by the unit tests only: $name"; fi
  else
    ok "killed: $name ($(grep -c '^FAIL' "$W/run.log") failing checks)"
  fi
  cp "$SRC/$file" "$W/tree/$file"
done
echo; echo "$fails mutant(s) survived or did not apply"; exit "$fails"
