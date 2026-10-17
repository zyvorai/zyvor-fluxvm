#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Guards the "runs on every supported distro" promise for the prebuilt Linux binaries. A binary built on a
# newer glibc than the oldest node it must run on fails at start with "version GLIBC_2.xx not found". This
# looks at every ELF file under DIR and fails when
#   - any of them references a glibc symbol version newer than MAX, or
#   - any of them has a NEEDED library outside the allowlist (a new dynamic dependency is a packaging change,
#     it must be a conscious one).
# Statically linked files (the musl guest agent) have neither and pass.
#
#   scripts/check-glibc-floor.sh DIR MAX          e.g.  scripts/check-glibc-floor.sh target/package/stage 2.35
#
# ALLOWED_NEEDED overrides the allowlist (an extended regex matched against the whole library name).
# OBJDUMP selects the tool (aarch64-linux-gnu-objdump for a cross-built tree). Needs binutils and GNU sort -V.
set -uo pipefail

DIR="${1:-}"
MAX="${2:-}"
if [[ -z "$DIR" || -z "$MAX" ]]; then
  echo "usage: $0 DIR MAX_GLIBC_VERSION   (for example: $0 target/stage 2.35)" >&2
  exit 2
fi
[[ -d "$DIR" ]] || { echo "check-glibc-floor: no such directory: $DIR" >&2; exit 2; }
[[ "$MAX" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "check-glibc-floor: MAX must look like 2.35, not '$MAX'" >&2; exit 2; }
OBJDUMP="${OBJDUMP:-objdump}"
command -v "$OBJDUMP" >/dev/null 2>&1 || { echo "check-glibc-floor: $OBJDUMP (binutils) is required" >&2; exit 2; }

# What the shipped host binaries link today: libc, libm, libgcc_s, the loader and libhivex (fluxctl). The
# libpthread/libdl/librt/libutil stubs appear in binaries built on glibc older than 2.34; libsystemd covers a
# build with the journald/sd_notify feature.
ALLOWED_NEEDED="${ALLOWED_NEEDED:-^(libc\.so\.6|libm\.so\.6|libgcc_s\.so\.1|ld-linux-[A-Za-z0-9_.-]+\.so\.[0-9]+|libhivex\.so\.0|libsystemd\.so\.0|libpthread\.so\.0|libdl\.so\.2|librt\.so\.1|libutil\.so\.1)$}"

is_elf() { [[ "$(head -c 4 "$1" 2>/dev/null)" == $'\x7fELF' ]]; }
newer_than() { # a b -> true when version a is newer than b
  [[ "$1" != "$2" && "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" == "$1" ]]
}

checked=0
bad=0
while IFS= read -r f; do
  is_elf "$f" || continue
  checked=$((checked + 1))
  rel="${f#"$DIR"/}"

  top="$("$OBJDUMP" -T "$f" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)"
  if [[ -n "$top" ]] && newer_than "$top" "$MAX"; then
    echo "FAIL $rel needs GLIBC_${top}, newer than the floor GLIBC_${MAX}"
    bad=$((bad + 1))
  fi

  while IFS= read -r lib; do
    [[ -z "$lib" ]] && continue
    if ! [[ "$lib" =~ $ALLOWED_NEEDED ]]; then
      echo "FAIL $rel links against $lib, which is not in the allowlist"
      bad=$((bad + 1))
    fi
  done < <("$OBJDUMP" -p "$f" 2>/dev/null | awk '/^[[:space:]]*NEEDED/ {print $2}')
done < <(find "$DIR" -type f | sort)

if [[ $checked -eq 0 ]]; then
  echo "check-glibc-floor: no ELF files under $DIR" >&2
  exit 2
fi
if [[ $bad -ne 0 ]]; then
  echo "check-glibc-floor: $bad problem(s) in $checked ELF file(s) (floor GLIBC_${MAX})"
  exit 1
fi
echo "check-glibc-floor: $checked ELF file(s) ok (floor GLIBC_${MAX})"
