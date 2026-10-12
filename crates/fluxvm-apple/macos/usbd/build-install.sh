#!/bin/bash
# Installs the USB Accessory Access helper as a per-user launch agent.
#   ./build-install.sh                          build from source, ad hoc sign, install to /Applications (sudo)
#   FLUXVM_PACKAGE_DIR=<unpacked release> ./build-install.sh
#                                               use the release's signed libexec/FluxVMUSBAccess.app in place
# Claiming USB accessories needs com.apple.developer.accessory-access.usb, which only a Developer ID signed bundle with a
# provisioning profile granting it gets (docs/macos-signing.md); the ad hoc build cannot claim devices.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PLIST="$HOME/Library/LaunchAgents/dev.zyvor.fluxvm.usbd.plist"
mkdir -p "$HOME/Library/LaunchAgents"
if [[ -n "${FLUXVM_PACKAGE_DIR:-}" ]]; then
  PKG="$(cd "$FLUXVM_PACKAGE_DIR" && pwd)"
  APP="$PKG/libexec/FluxVMUSBAccess.app"
  [[ -d "$APP" ]] || { echo "no $APP in that package" >&2; exit 1; }
  codesign --verify --strict --deep "$APP"
  sed "s#__PREFIX__#$PKG#" "$PKG/share/launchd/dev.zyvor.fluxvm.usbd.plist" > "$PLIST"
else
  APP="${TMPDIR:-/tmp}/FluxVMUSBAccess.app"
  rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS"
  cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
  xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx27.0 "$ROOT/main.swift" \
    -framework AppKit -framework Foundation -framework AccessoryAccess -o "$APP/Contents/MacOS/FluxVMUSBAccess"
  # Ad hoc: the restricted com.apple.developer.accessory-access.usb (Entitlements.plist, for profile-signed builds)
  # would get the helper killed at launch, so no entitlements.
  codesign --force --deep --sign - "$APP"
  sudo rm -rf /Applications/FluxVMUSBAccess.app
  sudo cp -R "$APP" /Applications/FluxVMUSBAccess.app
  cp "$ROOT/dev.zyvor.fluxvm.usbd.plist" "$PLIST"
fi
launchctl bootout "gui/$UID/dev.zyvor.fluxvm.usbd" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$PLIST"
echo "Open the macOS Accessory Access menu and grant the desired USB device to FluxVM USB Access."
