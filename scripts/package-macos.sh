#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Builds fluxctl, fluxvm-vz-switch, fluxvm-blockd (FluxDisk), fluxvm-usernet, the signed Virtualization.framework runner and
# the vmnet broker / USB Accessory Access helpers, signs everything and packs it for Homebrew / a GitHub release:
#   target/package/fluxvm-<version>-macos-arm64.tar.gz  (+ .sha256)
#   scripts/package-macos.sh [--profile release|debug] [--version X.Y.Z]
# Needs Apple silicon, the Xcode command line tools, Rust, and guestkit checked out next to this repo (fluxvm-image uses it).
#
# Signing (docs/macos-signing.md); everything is optional, the default is ad hoc:
#   FLUXVM_SIGN_IDENTITY     "Developer ID Application: … (TEAMID)" to sign for distribution (hardened runtime)
#   FLUXVM_PROFILE_RUNNER    provisioning profile for dev.zyvor.fluxvm.runner granting com.apple.vm.networking:
#                            adds bin/FluxVMRunner.app, the runner that can use vmnet and bridged networking
#   FLUXVM_PROFILE_VMNETD    profile for dev.zyvor.fluxvm.vmnetd (com.apple.vm.networking)
#   FLUXVM_PROFILE_USBD      profile for dev.zyvor.fluxvm.usbd (com.apple.developer.accessory-access.usb)
#   FLUXVM_NOTARIZE=1        notarize (needs FLUXVM_NOTARY_PROFILE or FLUXVM_NOTARY_KEY/_KEY_ID/_ISSUER) and staple
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }

PROFILE=release
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/fluxctl/Cargo.toml | head -n1)"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    *) echo "usage: $0 [--profile release|debug] [--version X.Y.Z]" >&2; exit 2 ;;
  esac
done
[[ "$PROFILE" == release || "$PROFILE" == debug ]] || { echo "--profile must be release or debug" >&2; exit 2; }
[[ -n "$VERSION" ]] || { echo "could not work out the version" >&2; exit 2; }
export FLUXVM_VERSION="$VERSION"
# shellcheck source=lib/macos-sign.sh
source scripts/lib/macos-sign.sh
if [[ "${FLUXVM_NOTARIZE:-0}" == 1 ]] && fs_is_adhoc; then
  echo "FLUXVM_NOTARIZE=1 needs FLUXVM_SIGN_IDENTITY (a Developer ID Application identity)" >&2; exit 2
fi

if [[ "$PROFILE" == release ]]; then cargo build --release -p fluxctl -p fluxvm-vz-switch -p fluxvm-blockd -p fluxvm-usernet; else cargo build -p fluxctl -p fluxvm-vz-switch -p fluxvm-blockd -p fluxvm-usernet; fi

# build.rs of fluxvm-apple compiles and signs the runner into its OUT_DIR; take the newest one.
RUNNER="$(find "target/$PROFILE/build" -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -print0 | xargs -0 ls -t | head -n1)"
[[ -n "$RUNNER" ]] || { echo "the Swift runner was not built (install the Xcode command line tools)" >&2; exit 1; }
codesign -d --entitlements - "$RUNNER" 2>&1 | grep -q com.apple.security.virtualization \
  || { echo "the runner lacks the com.apple.security.virtualization entitlement" >&2; exit 1; }

NAME="fluxvm-$VERSION-macos-arm64"
OUT="target/package"; STAGE="$OUT/$NAME"
rm -rf "$STAGE" "$OUT/$NAME.tar.gz" "$OUT/$NAME.tar.gz.sha256"
mkdir -p "$STAGE/bin"
cp "target/$PROFILE/fluxctl" "$STAGE/bin/fluxctl"
# Private VM-to-VM networks: started on demand, one process per network (docs/macos.md).
cp "target/$PROFILE/fluxvm-vz-switch" "$STAGE/bin/fluxvm-vz-switch"
# FluxDisk: the daemon starts one per VM that has an `overlay` disk and looks for it next to fluxctl.
cp "target/$PROFILE/fluxvm-blockd" "$STAGE/bin/fluxvm-blockd"
# The userspace gateway behind `apple.usernet`: the daemon supervises one per VM and looks for it next to fluxctl.
cp "target/$PROFILE/fluxvm-usernet" "$STAGE/bin/fluxvm-usernet"
# Not stripped: stripping would invalidate the runner's signature, and the entitlement is what lets it start VMs.
cp "$RUNNER" "$STAGE/bin/fluxvm-vz-runner"
cp LICENSE NOTICE "$STAGE/" 2>/dev/null || cp LICENSE "$STAGE/"
cp config.example.toml "$STAGE/"

APPLE=crates/fluxvm-apple
HELPERS="target/$PROFILE/macos-helpers"
mkdir -p "$HELPERS" "$STAGE/libexec" "$STAGE/share/launchd"

# The vmnet broker (macOS 26+): shared named vmnet networks across runner processes (docs/VMNET_BROKER.md).
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx26.0 "$APPLE/macos/vmnetd/main.swift" \
  "$APPLE/runner/VmnetOptions.swift" -framework Foundation -framework vmnet -o "$HELPERS/fluxvm-vmnetd"
fs_make_bundle "$STAGE/libexec/FluxVMNetworking.app" dev.zyvor.fluxvm.vmnetd fluxvm-vmnetd "FluxVM Networking" 26.0
cp "$HELPERS/fluxvm-vmnetd" "$STAGE/libexec/FluxVMNetworking.app/Contents/MacOS/fluxvm-vmnetd"
fs_sign_bundle "$STAGE/libexec/FluxVMNetworking.app" dev.zyvor.fluxvm.vmnetd "${FLUXVM_PROFILE_VMNETD:-}" \
  com.apple.security.virtualization com.apple.vm.networking

# USB Accessory Access helper (macOS 27+): owns the user's consent for physical USB passthrough.
rm -rf "$STAGE/libexec/FluxVMUSBAccess.app"
mkdir -p "$STAGE/libexec/FluxVMUSBAccess.app/Contents/MacOS"
cp "$APPLE/macos/usbd/Info.plist" "$STAGE/libexec/FluxVMUSBAccess.app/Contents/Info.plist"
xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx27.0 "$APPLE/macos/usbd/main.swift" \
  -framework AppKit -framework Foundation -framework AccessoryAccess \
  -o "$STAGE/libexec/FluxVMUSBAccess.app/Contents/MacOS/FluxVMUSBAccess"
fs_sign_bundle "$STAGE/libexec/FluxVMUSBAccess.app" dev.zyvor.fluxvm.usbd "${FLUXVM_PROFILE_USBD:-}" \
  com.apple.developer.accessory-access.usb

# Launch agents pointing into the bundles; `fluxctl service install` and the helper install scripts substitute
# __PREFIX__ with where the package was unpacked.
sed 's#/usr/local/libexec/fluxvm-vmnetd#__PREFIX__/libexec/FluxVMNetworking.app/Contents/MacOS/fluxvm-vmnetd#' \
  "$APPLE/macos/vmnetd/dev.zyvor.fluxvm.vmnetd.plist" > "$STAGE/share/launchd/dev.zyvor.fluxvm.vmnetd.plist"
sed 's#/Applications/FluxVMUSBAccess.app#__PREFIX__/libexec/FluxVMUSBAccess.app#' \
  "$APPLE/macos/usbd/dev.zyvor.fluxvm.usbd.plist" > "$STAGE/share/launchd/dev.zyvor.fluxvm.usbd.plist"

# The networking runner: only with a profile granting com.apple.vm.networking (Apple's approval), because an ad hoc
# runner claiming it is killed at launch. The daemon prefers bin/FluxVMRunner.app over bin/fluxvm-vz-runner.
if [[ -n "${FLUXVM_PROFILE_RUNNER:-}" ]]; then
  fs_make_bundle "$STAGE/bin/FluxVMRunner.app" dev.zyvor.fluxvm.runner fluxvm-vz-runner "FluxVM Runner" 14.0
  cp "$RUNNER" "$STAGE/bin/FluxVMRunner.app/Contents/MacOS/fluxvm-vz-runner"
  fs_sign_bundle "$STAGE/bin/FluxVMRunner.app" dev.zyvor.fluxvm.runner "$FLUXVM_PROFILE_RUNNER" \
    com.apple.security.virtualization com.apple.vm.networking
fi

# Re-sign the plain binaries with the chosen identity (the runner keeps its virtualization entitlement).
for b in fluxctl fluxvm-vz-switch fluxvm-blockd fluxvm-usernet; do fs_sign_binary "$STAGE/bin/$b"; done
fs_sign_binary "$STAGE/bin/fluxvm-vz-runner" "$APPLE/runner/Entitlements.plist"
fs_has_entitlement "$STAGE/bin/fluxvm-vz-runner" com.apple.security.virtualization \
  || { echo "the re-signed runner lost com.apple.security.virtualization" >&2; exit 1; }

if [[ "${FLUXVM_NOTARIZE:-0}" == 1 ]]; then
  ditto -c -k --keepParent "$STAGE" "$OUT/$NAME-notarize.zip"
  fs_notarize "$OUT/$NAME-notarize.zip"
  rm -f "$OUT/$NAME-notarize.zip"
  # Bundles can carry their ticket; bare binaries are checked online by Gatekeeper.
  for app in "$STAGE"/bin/*.app "$STAGE"/libexec/*.app; do
    if [[ -d "$app" ]]; then xcrun stapler staple "$app"; fi
  done
fi

tar -C "$OUT" -czf "$OUT/$NAME.tar.gz" "$NAME"
( cd "$OUT" && shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
echo "$OUT/$NAME.tar.gz"
cat "$OUT/$NAME.tar.gz.sha256"
