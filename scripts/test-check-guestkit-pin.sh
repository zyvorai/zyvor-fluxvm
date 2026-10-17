#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Tests scripts/check-guestkit-pin.sh against throwaway fixtures (no network).
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="${HERE}/check-guestkit-pin.sh"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

mk() { # dir ref lock_version
  mkdir -p "$1/packaging"
  printf '%s\n' "$2" >"$1/packaging/guestkit.ref"
  printf '[[package]]\nname = "guestkit-agent-protocol"\nversion = "0.1.0"\n\n[[package]]\nname = "guestkit"\nversion = "%s"\n' "$3" >"$1/Cargo.lock"
}
expect() { # name want_exit command...
  local name="$1" want="$2"
  shift 2
  "$@" >/dev/null 2>&1
  local got=$?
  if [[ "$want" == pass && $got -eq 0 ]] || [[ "$want" == fail && $got -ne 0 ]]; then ok "$name"; else bad "$name (exit $got)"; fi
}

mk "$T/match" v1.2.6 1.2.6
expect "ref and lock agree" pass "$CHECK" "$T/match"
mk "$T/skew" v1.2.7 1.2.6
expect "ref ahead of lock is rejected" fail "$CHECK" "$T/skew"
mk "$T/skew2" v1.2.5 1.2.6
expect "lock ahead of ref is rejected" fail "$CHECK" "$T/skew2"
mk "$T/branch" main 1.2.6
expect "a branch name is not a pin" fail "$CHECK" "$T/branch"
mkdir -p "$T/nofile"
expect "missing pin file is rejected" fail "$CHECK" "$T/nofile"
printf '[[package]]\nname = "other"\nversion = "1.0.0"\n' >"$T/match2.lock"
mk "$T/nopkg" v1.2.6 1.2.6
cp "$T/match2.lock" "$T/nopkg/Cargo.lock"
expect "no guestkit package in the lock is rejected" fail "$CHECK" "$T/nopkg"

# checkout verification
mkdir -p "$T/gk"
git -C "$T/gk" init -q
git -C "$T/gk" -c user.name=t -c user.email=t@t commit -q --allow-empty -m init
expect "an untagged checkout is rejected" fail "$CHECK" "$T/match" "$T/gk"
git -C "$T/gk" tag v1.2.6
expect "a checkout at the pinned tag is accepted" pass "$CHECK" "$T/match" "$T/gk"
git -C "$T/gk" -c user.name=t -c user.email=t@t commit -q --allow-empty -m next
expect "a checkout past the tag is rejected" fail "$CHECK" "$T/match" "$T/gk"

if [[ $fail -eq 0 ]]; then echo "all passed"; else echo "FAILED"; exit 1; fi
