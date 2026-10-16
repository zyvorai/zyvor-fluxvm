#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Report local readiness for a Developer ID + notarized macOS release package.
# Does not print secret values. Exit 0 if signing identity is present; exit 1 if
# notarization material is missing (the usual case until ASC API key is wired).
#
#   scripts/check-macos-release-secrets.sh
set -euo pipefail

fail=0
ok() { echo "ok   $*"; }
miss() { echo "MISS $*"; fail=1; }
warn() { echo "warn $*"; }

ID="$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -n1 || true)"
if [[ -n "$ID" ]]; then
  # Do not print the identity string (contains org name / Team ID).
  ok "Developer ID Application identity present in the login keychain"
else
  miss "Developer ID Application identity in the login keychain"
fi

if [[ -n "${FLUXVM_NOTARY_PROFILE:-}" ]] && xcrun notarytool history --keychain-profile "$FLUXVM_NOTARY_PROFILE" >/dev/null 2>&1; then
  ok "notarytool profile FLUXVM_NOTARY_PROFILE=$FLUXVM_NOTARY_PROFILE"
elif xcrun notarytool history --keychain-profile fluxvm-notary >/dev/null 2>&1; then
  ok "notarytool profile fluxvm-notary"
elif [[ -n "${FLUXVM_NOTARY_KEY:-}" && -n "${FLUXVM_NOTARY_KEY_ID:-}" && -n "${FLUXVM_NOTARY_ISSUER:-}" ]]; then
  ok "notary env FLUXVM_NOTARY_KEY / _KEY_ID / _ISSUER"
else
  miss "notarytool credentials (store-credentials, or FLUXVM_NOTARY_KEY + KEY_ID + ISSUER)"
fi

for name in RUNNER VMNETD USBD; do
  var="FLUXVM_PROFILE_$name"
  if [[ -n "${!var:-}" && -f "${!var}" ]]; then
    ok "$var → ${!var}"
  else
    warn "$var unset (restricted entitlement for $name omitted)"
  fi
done

if command -v gh >/dev/null 2>&1; then
  secrets="$(gh api repos/zyvorai/zyvor-fluxvm/actions/secrets --jq '.secrets[].name' 2>/dev/null || true)"
  for s in MACOS_DEVELOPER_ID_P12 MACOS_DEVELOPER_ID_P12_PASSWORD MACOS_NOTARY_KEY_P8 MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER; do
    if echo "$secrets" | grep -qx "$s"; then
      ok "GitHub secret $s"
    else
      miss "GitHub secret $s (gh secret set … -R zyvorai/zyvor-fluxvm)"
    fi
  done
else
  warn "gh not installed; skipped GitHub secret check"
fi

echo
echo "Package locally when ready:"
echo "  export FLUXVM_SIGN_IDENTITY=\"Developer ID Application: …\""
echo "  export FLUXVM_NOTARIZE=1 FLUXVM_NOTARY_PROFILE=fluxvm-notary   # or FLUXVM_NOTARY_KEY*"
echo "  scripts/package-macos.sh --version 0.4.10"
echo "See docs/macos-signing.md. Mac App Store distribution is a non-goal."

exit "$fail"
