#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Tests scripts/check-glibc-floor.sh against a tiny dynamically linked C program built on the spot.
# Linux with gcc and objdump only; elsewhere it prints a notice and exits 0.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECK="${HERE}/check-glibc-floor.sh"
if [[ "$(uname -s)" != Linux ]] || ! command -v gcc >/dev/null 2>&1 || ! command -v objdump >/dev/null 2>&1; then
  echo "test-check-glibc-floor: needs Linux, gcc and objdump; skipped"
  exit 0
fi
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
expect() { # name pass|fail command...
  local name="$1" want="$2"
  shift 2
  "$@" >"$T/out" 2>&1
  local got=$?
  if [[ "$want" == pass && $got -eq 0 ]] || [[ "$want" == fail && $got -ne 0 ]]; then ok "$name"; else bad "$name (exit $got)"; sed 's/^/     /' "$T/out"; fi
}

mkdir -p "$T/stage/bin" "$T/stage/share"
printf '#include <stdio.h>\nint main(void){puts("hi");return 0;}\n' >"$T/hello.c"
gcc -o "$T/stage/bin/hello" "$T/hello.c" || { echo "test-check-glibc-floor: gcc failed"; exit 1; }
echo "not an elf" >"$T/stage/share/readme.txt"

top="$(objdump -T "$T/stage/bin/hello" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)"
echo "     (the test binary's own highest glibc symbol is GLIBC_${top})"

expect "a floor equal to the binary's highest symbol passes" pass "$CHECK" "$T/stage" "$top"
expect "a much higher floor passes" pass "$CHECK" "$T/stage" 99.0
expect "a floor of 1.0 fails" fail "$CHECK" "$T/stage" 1.0
expect "a floor just below the highest symbol fails" fail "$CHECK" "$T/stage" 2.0
ALLOWED_NEEDED='^$' expect "an empty NEEDED allowlist fails" fail "$CHECK" "$T/stage" 99.0
expect "a non-ELF-only directory is reported, not passed" fail "$CHECK" "$T/stage/share" 99.0
expect "a missing directory is an error" fail "$CHECK" "$T/nope" 2.35
expect "a malformed version is an error" fail "$CHECK" "$T/stage" two
expect "missing arguments are an error" fail "$CHECK"

if gcc -static -o "$T/stage/bin/hello-static" "$T/hello.c" 2>/dev/null; then
  mkdir -p "$T/static" && cp "$T/stage/bin/hello-static" "$T/static/"
  expect "a statically linked binary passes at any floor" pass "$CHECK" "$T/static" 1.0
else
  echo "     (no static libc here; static case skipped)"
fi

if [[ $fail -eq 0 ]]; then echo "all passed"; else echo "FAILED"; exit 1; fi
