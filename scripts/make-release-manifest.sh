#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Turns a directory of built packages into the signed tree nodes pull from a static mirror:
#
#   <out>/releases/<version>/<artifacts>, SHA256SUMS, manifest.json, manifest.json.sig
#   <out>/channels/<channel>.json, <channel>.json.sig
#
#   scripts/make-release-manifest.sh --version 0.4.12 --artifacts target/package --key release.key --out dist/mirror \
#       [--channel stable] [--serial N] [--expires-days 30] [--expires-at EPOCH] [--percent 100] [--salt S] \
#       [--min-upgrade-from X.Y.Z] [--revoke 0.4.9,0.4.10] [--require-label k=v] [--exclude-label k=v] [--sbom FILE]
#   scripts/make-release-manifest.sh --promote --version 0.4.12 --key release.key --out dist/mirror --channel stable --percent 20
#       (write only a new signed channel file for a release already under <out>/releases/<version>/, to move a
#        channel or widen its wave; <out> is a copy of the mirror, and --serial must exceed the channel's current one)
#   scripts/make-release-manifest.sh --verify <file> --pubkey release.pub     (check one signed file)
#
# Signatures are raw Ed25519 over the exact bytes of the file (openssl >= 3.0, `pkeyutl -rawin`), kept next to it as
# <file>.sig. The channel file points at the release manifest and carries the rollout rules; it is written last so a
# node never sees a channel that names files the mirror does not have yet. `serial` only goes up (default: the
# current time in seconds); nodes refuse a channel whose serial is lower than the one they last accepted.
#
# Make a key once, keep it offline or in the CI secret store:
#   openssl genpkey -algorithm ed25519 -out release.key && openssl pkey -in release.key -pubout -out release.pub
# Needs openssl >= 3.0, jq and sha256sum (or shasum).
set -euo pipefail

VERSION=""; ARTIFACTS=""; KEY=""; OUT=""; CHANNEL=stable; SERIAL=""; EXPIRES_DAYS=30; PERCENT=100; SALT=""
EXPIRES_AT=""; MIN_FROM=""; REVOKE=""; SBOM=""; PROMOTE=0; VERIFY=""; PUBKEY=""; REQ=(); EXC=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;       --artifacts) ARTIFACTS="$2"; shift 2 ;;
    --key) KEY="$2"; shift 2 ;;               --out) OUT="$2"; shift 2 ;;
    --channel) CHANNEL="$2"; shift 2 ;;       --serial) SERIAL="$2"; shift 2 ;;
    --expires-days) EXPIRES_DAYS="$2"; shift 2 ;;   --percent) PERCENT="$2"; shift 2 ;;
    --expires-at) EXPIRES_AT="$2"; shift 2 ;;
    --salt) SALT="$2"; shift 2 ;;             --min-upgrade-from) MIN_FROM="$2"; shift 2 ;;
    --revoke) REVOKE="$2"; shift 2 ;;         --sbom) SBOM="$2"; shift 2 ;;
    --require-label) REQ+=("$2"); shift 2 ;;  --exclude-label) EXC+=("$2"); shift 2 ;;
    --promote) PROMOTE=1; shift ;;
    --verify) VERIFY="$2"; shift 2 ;;         --pubkey) PUBKEY="$2"; shift 2 ;;
    *) sed -n '5,22p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
  esac
done
die() { echo "make-release-manifest: $*" >&2; exit 1; }
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
for t in openssl jq; do command -v "$t" >/dev/null || die "needs $t (openssl >= 3.0)"; done

if [[ -n "$VERIFY" ]]; then
  [[ -f "$VERIFY" && -f "$VERIFY.sig" && -f "$PUBKEY" ]] || die "--verify needs the file, its .sig and --pubkey"
  openssl pkeyutl -verify -pubin -inkey "$PUBKEY" -rawin -in "$VERIFY" -sigfile "$VERIFY.sig" >/dev/null 2>&1 \
    || die "BAD signature on $VERIFY"
  echo "signature ok: $VERIFY"; exit 0
fi

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || die "--version must be X.Y.Z"
[[ -f "$KEY" && -n "$OUT" ]] || die "--key FILE and --out DIR are required"
[[ $PROMOTE -eq 1 || -d "$ARTIFACTS" ]] || die "--artifacts DIR is required"
[[ "$CHANNEL" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "bad channel name"
[[ "$PERCENT" =~ ^[0-9]+$ && "$PERCENT" -le 100 ]] || die "--percent must be 0-100"
[[ "$EXPIRES_DAYS" =~ ^[0-9]+$ && "$EXPIRES_DAYS" -gt 0 ]] || die "--expires-days must be a positive number"
NOW="$(date -u +%s)"
SERIAL="${SERIAL:-$NOW}"; [[ "$SERIAL" =~ ^[0-9]+$ ]] || die "--serial must be an integer"
EXPIRES="${EXPIRES_AT:-$((NOW + EXPIRES_DAYS * 86400))}"  # --expires-at exists so tests can make an expired channel
SALT="${SALT:-$CHANNEL-$VERSION}"

sign() { openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$1" -out "$1.sig"; }

REL="$OUT/releases/$VERSION"
mkdir -p "$OUT/channels"
n=0
if [[ $PROMOTE -eq 1 ]]; then
  [[ -f "$REL/manifest.json" && -f "$REL/manifest.json.sig" ]] || die "--promote: no signed release $VERSION under $OUT/releases"
else
rm -rf "$REL"; mkdir -p "$REL"

# Artifacts: fluxvm-<ver>-linux-<arch>.tar.gz and fluxvm_<ver>_<amd64|arm64>.deb and fluxvm-<ver>-1.<x86_64|aarch64>.rpm, each with its sha256.
arts='{}'
add() { # ARCH KIND FILE
  cp "$3" "$REL/"
  local f; f="$(basename "$3")"
  arts="$(jq --arg a "$1" --arg k "$2" --arg f "$f" --arg h "$(sha256 "$3")" --argjson n "$(wc -c <"$3" | tr -d ' ')" \
    '.[$a][$k] = {file: $f, sha256: $h, size: $n}' <<<"$arts")"
}
try() { if [[ -f "$3" ]]; then add "$1" "$2" "$3"; n=$((n + 1)); fi; }
try x86_64 tarball "$ARTIFACTS/fluxvm-$VERSION-linux-x86_64.tar.gz"
try aarch64 tarball "$ARTIFACTS/fluxvm-$VERSION-linux-aarch64.tar.gz"
try x86_64 deb "$ARTIFACTS/fluxvm_${VERSION}_amd64.deb"
try aarch64 deb "$ARTIFACTS/fluxvm_${VERSION}_arm64.deb"
try x86_64 rpm "$ARTIFACTS/fluxvm-${VERSION}-1.x86_64.rpm"
try aarch64 rpm "$ARTIFACTS/fluxvm-${VERSION}-1.aarch64.rpm"
[[ $n -gt 0 ]] || die "no fluxvm $VERSION packages in $ARTIFACTS"
if [[ -n "$SBOM" ]]; then [[ -f "$SBOM" ]] || die "--sbom file missing"; cp "$SBOM" "$REL/sbom.cdx.json"; fi

# manifest.json does not exist yet, so everything in the directory is a package or the SBOM.
( cd "$REL" && for f in *; do printf '%s  %s\n' "$(sha256 "$f")" "$f"; done > ../SHA256SUMS.tmp && mv ../SHA256SUMS.tmp SHA256SUMS )
jq -n --arg v "$VERSION" --argjson a "$arts" --arg built "$(date -u -r "$NOW" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d "@$NOW" +%Y-%m-%dT%H:%M:%SZ)" \
  '{schema: 1, version: $v, built_at: $built, artifacts: $a}' > "$REL/manifest.json"
sign "$REL/manifest.json"
fi

csv() { jq -R 'split(",") | map(select(length > 0))' <<<"$1"; }
labels() { if [[ $# -eq 0 ]]; then echo '[]'; else printf '%s\n' "$@" | jq -R . | jq -s .; fi; }
jq -n --arg ch "$CHANNEL" --argjson serial "$SERIAL" --argjson expires "$EXPIRES" --arg v "$VERSION" \
  --arg mh "$(sha256 "$REL/manifest.json")" --arg min "$MIN_FROM" --argjson revoked "$(csv "$REVOKE")" \
  --argjson pct "$PERCENT" --arg salt "$SALT" --argjson req "$(labels ${REQ[@]+"${REQ[@]}"})" --argjson exc "$(labels ${EXC[@]+"${EXC[@]}"})" \
  '{schema: 1, channel: $ch, serial: $serial, expires: $expires, version: $v,
    manifest: ("releases/" + $v + "/manifest.json"), manifest_sha256: $mh,
    min_upgrade_from: (if $min == "" then null else $min end), revoked: $revoked,
    rollout: {percent: $pct, salt: $salt, require_labels: $req, exclude_labels: $exc}}' > "$OUT/channels/$CHANNEL.json.new"
sign "$OUT/channels/$CHANNEL.json.new"
# Written last: the signature first, then the channel file, so a node that sees the file also finds its signature.
mv "$OUT/channels/$CHANNEL.json.new.sig" "$OUT/channels/$CHANNEL.json.sig"
mv "$OUT/channels/$CHANNEL.json.new" "$OUT/channels/$CHANNEL.json"
echo "release $VERSION: $([[ $PROMOTE -eq 1 ]] && echo promoted || echo "$n package(s)"), channel $CHANNEL serial $SERIAL, $PERCENT% , expires $(date -u -r "$EXPIRES" +%FT%TZ 2>/dev/null || date -u -d "@$EXPIRES" +%FT%TZ)"
