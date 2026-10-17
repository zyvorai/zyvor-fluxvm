#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Checks packaging/vendor.lock is well formed and that the install scripts accept --lock / --version / --sha256 /
# --from-file. Offline: no download, no GitHub API call, runs on any OS (the scripts' --dry-run does not need Linux).
# Whether the pinned URLs and hashes still match the upstream assets is a separate, networked check:
#   scripts/test-vendor-lock.sh --online
set -euo pipefail
cd "$(dirname "$0")/.."

LOCK="${VENDOR_LOCK:-packaging/vendor.lock}"
ONLINE=false
[[ "${1:-}" == "--online" ]] && ONLINE=true
fails=0
bad() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

# --- the lock file ---------------------------------------------------------------------------------------------
[[ -r "$LOCK" ]] || { echo "FAIL: $LOCK is missing" >&2; exit 1; }
lines=0
seen=" "
while read -r comp arch ver sha url extra; do
  [[ -z "${comp:-}" || "$comp" == \#* ]] && continue
  lines=$((lines + 1))
  [[ -z "${extra:-}" ]] || bad "$comp $arch: more than five fields"
  case "$comp" in firecracker|cloud-hypervisor|ch-remote|hypervisor-fw) ;; *) bad "unknown component '$comp'" ;; esac
  case "$arch" in x86_64|aarch64) ;; *) bad "$comp: arch '$arch' is not x86_64 or aarch64" ;; esac
  [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || bad "$comp $arch: sha256 is not 64 lowercase hex digits"
  [[ "$url" == https://github.com/*/releases/download/"$ver"/* ]] || bad "$comp $arch: url is not a pinned github release download for $ver"
  [[ "$url" != *latest* ]] || bad "$comp $arch: url mentions 'latest'"
  [[ "$seen" != *" $comp/$arch "* ]] || bad "$comp $arch: listed twice"
  seen="$seen$comp/$arch "
done < "$LOCK"
for c in firecracker cloud-hypervisor ch-remote hypervisor-fw; do
  for a in x86_64 aarch64; do [[ "$seen" == *" $c/$a "* ]] || bad "$c $a: no entry"; done
done
# A component's versions must agree across architectures, or one arch would silently ship a different release.
for c in firecracker cloud-hypervisor ch-remote hypervisor-fw; do
  n="$(awk -v c="$c" '$1 == c { print $3 }' "$LOCK" | sort -u | wc -l | tr -d ' ')"
  [[ "$n" == 1 ]] || bad "$c: versions differ between architectures"
done
[[ "$(awk '$1 == "cloud-hypervisor" { print $3; exit }' "$LOCK")" == "$(awk '$1 == "ch-remote" { print $3; exit }' "$LOCK")" ]] \
  || bad "cloud-hypervisor and ch-remote are pinned to different versions"

# --- the scripts accept the flags (dry run: no network, no install) -------------------------------------------
FC=scripts/install-firecracker.sh
CH=scripts/install-cloud-hypervisor.sh
SHA_X="$(awk '$1 == "firecracker" && $2 == "x86_64" { print $4 }' "$LOCK")"
FCV="$(awk '$1 == "firecracker" && $2 == "x86_64" { print $3 }' "$LOCK")"
CHV="$(awk '$1 == "cloud-hypervisor" && $2 == "x86_64" { print $3 }' "$LOCK")"

expect_ok() { # <label> <expected output substring> -- cmd...
  local label="$1" want="$2" out; shift 3
  if out="$("$@" 2>&1)"; then
    [[ "$out" == *"$want"* ]] || bad "$label: output lacks '$want'"
  else
    bad "$label: exited non-zero: $out"
  fi
}
expect_fail() { # <label> <expected message substring> -- cmd...
  local label="$1" want="$2" out; shift 3
  if out="$("$@" 2>&1)"; then bad "$label: should have failed"; else [[ "$out" == *"$want"* ]] || bad "$label: message lacks '$want': $out"; fi
}

for s in "$FC" "$CH"; do
  out="$(bash "$s" --help)"
  for flag in --lock --from-file --sha256 --version --dry-run; do
    [[ "$out" == *"$flag"* ]] || bad "$s --help does not mention $flag"
  done
done

for arch in x86_64 aarch64; do
  expect_ok "firecracker --lock $arch" "firecracker-${FCV}-${arch}.tgz" -- bash "$FC" --lock "$LOCK" --arch "$arch" --dry-run
  expect_ok "cloud-hypervisor --lock $arch" "hypervisor-fw sha256=" -- bash "$CH" --lock "$LOCK" --arch "$arch" --dry-run
done
expect_ok "firecracker --version --sha256" "sha256=$SHA_X" -- bash "$FC" --version "$FCV" --sha256 "$SHA_X" --arch x86_64 --dry-run
expect_ok "firecracker --from-file" "source=/tmp/x.tgz" -- bash "$FC" --version "$FCV" --sha256 "$SHA_X" --from-file /tmp/x.tgz --arch x86_64 --dry-run
expect_ok "cloud-hypervisor --from-file" "source=/tmp/ch" -- bash "$CH" --version "$CHV" --sha256 "$SHA_X" --from-file /tmp/ch --arch x86_64 --dry-run
# Unverified or ambiguous requests are refused, and a dry run never resolves "latest" over the network.
expect_fail "firecracker --from-file without a digest" "refusing to install an unverified" -- bash "$FC" --version "$FCV" --from-file /tmp/x.tgz --arch x86_64 --dry-run
expect_fail "cloud-hypervisor --from-file without a digest" "refusing to install an unverified" -- bash "$CH" --version "$CHV" --from-file /tmp/x --arch x86_64 --dry-run
expect_fail "firecracker bad digest" "not a SHA-256 digest" -- bash "$FC" --version "$FCV" --sha256 abc --arch x86_64 --dry-run
expect_fail "firecracker digest without version" "needs --version" -- bash "$FC" --sha256 "$SHA_X" --arch x86_64 --dry-run
expect_fail "firecracker lock version mismatch" "pins" -- bash "$FC" --lock "$LOCK" --version v0.0.1 --arch x86_64 --dry-run
expect_fail "firecracker digest vs lock" "disagrees" -- bash "$FC" --lock "$LOCK" --sha256 "$(printf '0%.0s' {1..64})" --arch x86_64 --dry-run
expect_fail "cloud-hypervisor lock version mismatch" "pins" -- bash "$CH" --lock "$LOCK" --version v0.0.1 --arch x86_64 --dry-run
expect_fail "firecracker dry run without a version" "will not call the GitHub API" -- bash "$FC" --arch x86_64 --dry-run
expect_fail "cloud-hypervisor dry run without a version" "will not call the GitHub API" -- bash "$CH" --arch x86_64 --dry-run
expect_fail "firecracker unknown option" "unknown option" -- bash "$FC" --bogus

# --- optional: the pinned hashes still match upstream ---------------------------------------------------------
if $ONLINE; then
  tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  while read -r comp arch ver sha url _; do
    [[ -z "${comp:-}" || "$comp" == \#* ]] && continue
    curl -fsSL -o "$tmp/f" "$url" || { bad "$comp $arch: cannot download $url"; continue; }
    if command -v sha256sum >/dev/null; then got="$(sha256sum "$tmp/f" | awk '{print $1}')"; else got="$(shasum -a 256 "$tmp/f" | awk '{print $1}')"; fi
    [[ "$got" == "$sha" ]] || bad "$comp $arch $ver: hash is $got, lock says $sha"
  done < "$LOCK"
fi

if (( fails )); then echo "test-vendor-lock: $fails failure(s)" >&2; exit 1; fi
echo "test-vendor-lock: ok ($lines entries)"
