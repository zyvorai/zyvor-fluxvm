#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Build a Developer ID signed + notarized macOS package and print the next
# release steps. Fails fast if signing/notary material is missing.
#
#   scripts/notarize-macos-package.sh [version]
#   # default version: Cargo.toml of fluxctl
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n 's/^version = "\([0-9].*\)"/\1/p' crates/fluxctl/Cargo.toml | head -1)"
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]] || { echo "bad version: $VERSION" >&2; exit 2; }

scripts/check-macos-release-secrets.sh || {
  echo >&2
  echo "Cannot notarize until notary credentials and (for CI) GitHub secrets exist." >&2
  echo "Create an App Store Connect API key, then either:" >&2
  echo "  xcrun notarytool store-credentials fluxvm-notary --key … --key-id … --issuer …" >&2
  echo "  export FLUXVM_NOTARIZE=1 FLUXVM_NOTARY_PROFILE=fluxvm-notary" >&2
  echo "or set FLUXVM_NOTARY_KEY / FLUXVM_NOTARY_KEY_ID / FLUXVM_NOTARY_ISSUER." >&2
  echo "Wire CI with MACOS_DEVELOPER_ID_P12* and MACOS_NOTARY_* (docs/macos-signing.md)." >&2
  exit 1
}

: "${FLUXVM_SIGN_IDENTITY:=$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -n1)}"
[[ -n "$FLUXVM_SIGN_IDENTITY" ]] || { echo "no Developer ID identity" >&2; exit 1; }
export FLUXVM_SIGN_IDENTITY FLUXVM_NOTARIZE=1
[[ -n "${FLUXVM_NOTARY_PROFILE:-}" || -n "${FLUXVM_NOTARY_KEY:-}" ]] || export FLUXVM_NOTARY_PROFILE=fluxvm-notary

scripts/package-macos.sh --version "$VERSION"
PKG="target/package/fluxvm-${VERSION}-macos-arm64"
for app in "$PKG"/bin/*.app "$PKG"/libexec/*.app; do
  [[ -d "$app" ]] || continue
  xcrun stapler validate "$app"
  spctl -a -vv -t exec "$app"
done
echo
echo "Notarized package: ${PKG}.tar.gz"
echo "Next: gh release upload v${VERSION} ${PKG}.tar.gz ${PKG}.tar.gz.sha256 --clobber"
echo "      scripts/update-homebrew-formula.sh ${VERSION} ${PKG}.tar.gz"
echo "      # push packaging/homebrew/fluxvm.rb to zyvorai/homebrew-tap Formula/fluxvm.rb"
