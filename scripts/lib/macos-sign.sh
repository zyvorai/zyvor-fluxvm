# SPDX-License-Identifier: Apache-2.0
# Shared signing helpers for FluxVM's macOS binaries. Source it; every function is prefixed `fs_`.
#
#   FLUXVM_SIGN_IDENTITY   codesign identity: "-" (ad hoc, the default) or e.g.
#                          "Developer ID Application: Your Org Name (TEAMID)"
#
# Ad hoc signing works for everything except Apple's restricted entitlements (com.apple.vm.networking,
# com.apple.developer.accessory-access.usb): those are only honoured for a Developer ID signed app bundle that embeds a
# provisioning profile granting them, so `fs_sign_bundle` builds such bundles. See docs/macos-signing.md.

FS_IDENTITY="${FLUXVM_SIGN_IDENTITY:--}"

fs_die() { echo "macos-sign: $*" >&2; exit 1; }

fs_is_adhoc() { [[ "$FS_IDENTITY" == "-" ]]; }

# Sets FS_FLAGS. Hardened runtime and a secure timestamp are required for notarization; ad hoc signatures get neither.
# (Plain arrays only: macOS ships bash 3.2, so no mapfile, and empty arrays are expanded with ${a[@]+"${a[@]}"}.)
fs_set_flags() {
  if fs_is_adhoc; then
    FS_FLAGS=(--force --sign -)
  else
    FS_FLAGS=(--force --options runtime --timestamp --sign "$FS_IDENTITY")
  fi
}

# fs_sign_binary <path> [entitlements.plist]
fs_sign_binary() {
  local bin="$1" ent="${2:-}"
  [[ -f "$bin" ]] || fs_die "no such binary: $bin"
  fs_set_flags
  if [[ -n "$ent" ]]; then
    codesign "${FS_FLAGS[@]}" --entitlements "$ent" "$bin"
  else
    codesign "${FS_FLAGS[@]}" "$bin"
  fi
  codesign --verify --strict "$bin" || fs_die "signature of $bin does not verify"
}

# fs_profile_value <profile> <PlistBuddy key path>   e.g. ":Entitlements:com.apple.vm.networking"
fs_profile_value() {
  local profile="$1" key="$2" tmp
  tmp="$(mktemp)"
  security cms -D -i "$profile" > "$tmp" 2>/dev/null || { rm -f "$tmp"; fs_die "cannot decode provisioning profile $profile"; }
  /usr/libexec/PlistBuddy -c "Print $key" "$tmp" 2>/dev/null
  local rc=$?
  rm -f "$tmp"
  return $rc
}

# fs_check_profile <profile> <bundle id> <restricted entitlement>...
# Fails unless the profile is for this bundle id (or a matching wildcard) and grants every listed entitlement.
fs_check_profile() {
  local profile="$1" bundle_id="$2"; shift 2
  [[ -f "$profile" ]] || fs_die "no such provisioning profile: $profile"
  local app_id team
  # shellcheck disable=SC2310
  app_id="$(fs_profile_value "$profile" ':Entitlements:com.apple.application-identifier')" \
    || fs_die "$profile has no application identifier"
  team="$(fs_profile_value "$profile" ':TeamIdentifier:0')" || fs_die "$profile has no team identifier"
  case "$app_id" in
    "$team.$bundle_id" | "$team.*") ;;
    *) fs_die "$profile is for $app_id, not $team.$bundle_id" ;;
  esac
  local ent
  for ent in "$@"; do
    fs_profile_value "$profile" ":Entitlements:$ent" >/dev/null \
      || fs_die "$profile does not grant $ent (ask Apple for it on your team, then regenerate the profile)"
  done
  if ! fs_is_adhoc && [[ "$FS_IDENTITY" != *"($team)"* ]]; then
    fs_die "identity '$FS_IDENTITY' is not from team $team, which issued $profile"
  fi
  echo "$team"
}

# fs_entitlements <out.plist> <team or empty> <bundle id or empty> <entitlement>...
# Writes an entitlements plist with every listed boolean entitlement, plus the application and team identifiers a
# provisioning profile requires when a team is given.
fs_entitlements() {
  local out="$1" team="$2" bundle_id="$3"; shift 3
  cat > "$out" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict></dict></plist>
PLIST
  local ent
  for ent in "$@"; do
    /usr/libexec/PlistBuddy -c "Add :$ent bool true" "$out" >/dev/null
  done
  if [[ -n "$team" ]]; then
    /usr/libexec/PlistBuddy -c "Add :com.apple.application-identifier string $team.$bundle_id" "$out" >/dev/null
    /usr/libexec/PlistBuddy -c "Add :com.apple.developer.team-identifier string $team" "$out" >/dev/null
  fi
}

# fs_make_bundle <app dir> <bundle id> <executable name> <display name> <min macOS>
# A minimal background (no Dock icon) app bundle around one executable, which the caller copies in afterwards.
fs_make_bundle() {
  local app="$1" bundle_id="$2" exe="$3" name="$4" min_os="$5"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleExecutable</key><string>$exe</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${FLUXVM_VERSION:-0.0.0}</string>
  <key>CFBundleVersion</key><string>${FLUXVM_VERSION:-0.0.0}</string>
  <key>LSMinimumSystemVersion</key><string>$min_os</string>
  <key>LSUIElement</key><true/>
</dict></plist>
PLIST
}

# fs_sign_bundle <app dir> <bundle id> <profile or empty> <entitlement>...
# Embeds the profile (when given), writes matching entitlements and signs the bundle. Without a profile the
# restricted entitlements are left out: an ad hoc signed binary claiming them is killed by AMFI at launch.
fs_sign_bundle() {
  local app="$1" bundle_id="$2" profile="$3"; shift 3
  local team="" ent_file e
  local ents=()
  if [[ -n "$profile" ]]; then
    fs_is_adhoc && fs_die "a provisioning profile needs a Developer ID identity (FLUXVM_SIGN_IDENTITY)"
    team="$(fs_check_profile "$profile" "$bundle_id" "$@")" || exit 1
    cp "$profile" "$app/Contents/embedded.provisionprofile"
    ents=("$@")
  else
    for e in "$@"; do
      case "$e" in
        com.apple.vm.networking | com.apple.developer.*) ;;
        *) ents+=("$e") ;;
      esac
    done
  fi
  ent_file="$(mktemp)"
  fs_entitlements "$ent_file" "$team" "$bundle_id" ${ents[@]+"${ents[@]}"}
  fs_set_flags
  codesign "${FS_FLAGS[@]}" --entitlements "$ent_file" "$app"
  rm -f "$ent_file"
  codesign --verify --strict --deep "$app" || fs_die "signature of $app does not verify"
}

# fs_has_entitlement <path> <entitlement>
fs_has_entitlement() {
  # Not piped into grep -q: under pipefail an early grep exit can fail codesign with SIGPIPE.
  local xml
  xml="$(codesign -d --entitlements - --xml "$1" 2>/dev/null)" || return 1
  grep -qF "<key>$2</key>" <<<"$xml"
}

# fs_notarize <file.zip>
# Uses an App Store Connect API key (FLUXVM_NOTARY_KEY, _KEY_ID, _ISSUER) or a stored keychain profile
# (FLUXVM_NOTARY_PROFILE, from `xcrun notarytool store-credentials`).
fs_notarize() {
  local zip="$1"
  local auth=()
  if [[ -n "${FLUXVM_NOTARY_PROFILE:-}" ]]; then
    auth=(--keychain-profile "$FLUXVM_NOTARY_PROFILE")
  elif [[ -n "${FLUXVM_NOTARY_KEY:-}" && -n "${FLUXVM_NOTARY_KEY_ID:-}" && -n "${FLUXVM_NOTARY_ISSUER:-}" ]]; then
    auth=(--key "$FLUXVM_NOTARY_KEY" --key-id "$FLUXVM_NOTARY_KEY_ID" --issuer "$FLUXVM_NOTARY_ISSUER")
  else
    fs_die "notarizing needs FLUXVM_NOTARY_PROFILE, or FLUXVM_NOTARY_KEY + FLUXVM_NOTARY_KEY_ID + FLUXVM_NOTARY_ISSUER"
  fi
  local out
  out="$(xcrun notarytool submit "$zip" "${auth[@]}" --wait --output-format json)" || fs_die "notarytool failed: $out"
  echo "$out"
  if ! grep -q '"status" *: *"Accepted"' <<<"$out"; then
    local id
    id="$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' <<<"$out" | head -n1)"
    if [[ -n "$id" ]]; then xcrun notarytool log "$id" "${auth[@]}" >&2 || true; fi
    fs_die "notarization was not accepted"
  fi
}
