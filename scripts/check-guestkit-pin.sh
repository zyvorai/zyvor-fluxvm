#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# fluxvm depends on the sibling repo zyvorai/guestkit through a relative path. Release builds check
# guestkit out at the tag recorded in packaging/guestkit.ref instead of its default branch, so a guestkit
# release can no longer break (or silently change) a fluxvm build. This script fails when the pin and
# Cargo.lock disagree, which is what a half-done guestkit bump looks like.
#
#   scripts/check-guestkit-pin.sh [FLUXVM_DIR] [GUESTKIT_DIR]
#
# With GUESTKIT_DIR it also checks that the checkout really is at the pinned tag. To bump guestkit:
# change packaging/guestkit.ref, refresh Cargo.lock (cargo metadata, without --locked) and commit both.
set -euo pipefail

FLUXVM_DIR="${1:-.}"
GUESTKIT_DIR="${2:-}"
REF_FILE="${FLUXVM_DIR}/packaging/guestkit.ref"
LOCK="${FLUXVM_DIR}/Cargo.lock"

[[ -f "$REF_FILE" ]] || { echo "check-guestkit-pin: $REF_FILE is missing" >&2; exit 1; }
[[ -f "$LOCK" ]] || { echo "check-guestkit-pin: $LOCK is missing" >&2; exit 1; }

ref="$(tr -d '[:space:]' <"$REF_FILE")"
[[ "$ref" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "check-guestkit-pin: $REF_FILE must hold one release tag like v1.2.6, not '$ref'" >&2
  exit 1
}

lock_version="$(awk '
  $0 == "name = \"guestkit\"" { found = 1; next }
  found && /^version = / { gsub(/version = |"/, ""); print; exit }
' "$LOCK")"
[[ -n "$lock_version" ]] || { echo "check-guestkit-pin: no guestkit package in $LOCK" >&2; exit 1; }

if [[ "v${lock_version}" != "$ref" ]]; then
  echo "check-guestkit-pin: packaging/guestkit.ref pins ${ref} but Cargo.lock records guestkit ${lock_version}." >&2
  echo "check-guestkit-pin: bump both together (edit the ref, refresh Cargo.lock, commit both)." >&2
  exit 1
fi

if [[ -n "$GUESTKIT_DIR" ]]; then
  at="$(git -C "$GUESTKIT_DIR" describe --tags --exact-match HEAD 2>/dev/null || true)"
  [[ "$at" == "$ref" ]] || {
    echo "check-guestkit-pin: ${GUESTKIT_DIR} is at '${at:-no tag}', expected ${ref}" >&2
    exit 1
  }
fi
echo "check-guestkit-pin: guestkit ${ref} (matches Cargo.lock${GUESTKIT_DIR:+ and the checkout})"
