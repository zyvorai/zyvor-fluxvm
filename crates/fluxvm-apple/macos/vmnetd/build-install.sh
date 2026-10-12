#!/bin/bash
# Installs the vmnet broker as a per-user launch agent.
#   ./build-install.sh                          build from source, ad hoc sign, install to /usr/local/libexec (sudo)
#   FLUXVM_PACKAGE_DIR=<unpacked release> ./build-install.sh
#                                               use the release's signed libexec/FluxVMNetworking.app in place
# Only a Developer ID signed bundle with a provisioning profile for com.apple.vm.networking can use vmnet networks
# (docs/macos-signing.md); the ad hoc build starts but VMs report "Internal Network Error".
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
PLIST="$HOME/Library/LaunchAgents/dev.zyvor.fluxvm.vmnetd.plist"
mkdir -p "$HOME/Library/LaunchAgents"
if [[ -n "${FLUXVM_PACKAGE_DIR:-}" ]]; then
  PKG="$(cd "$FLUXVM_PACKAGE_DIR" && pwd)"
  APP="$PKG/libexec/FluxVMNetworking.app"
  [[ -d "$APP" ]] || { echo "no $APP in that package" >&2; exit 1; }
  codesign --verify --strict --deep "$APP"
  sed "s#__PREFIX__#$PKG#" "$PKG/share/launchd/dev.zyvor.fluxvm.vmnetd.plist" > "$PLIST"
else
  OUT="${TMPDIR:-/tmp}/fluxvm-vmnetd"
  xcrun swiftc -swift-version 5 -O -target arm64-apple-macosx26.0 "$ROOT/main.swift" "$ROOT/../../runner/VmnetOptions.swift" \
      -framework Foundation -framework vmnet -o "$OUT"
  # Ad hoc: the restricted com.apple.vm.networking would get the broker killed at launch, so only virtualization.
  codesign --force --sign - --entitlements "$ROOT/../../runner/Entitlements.plist" "$OUT"
  sudo install -m 0755 "$OUT" /usr/local/libexec/fluxvm-vmnetd
  cp "$ROOT/dev.zyvor.fluxvm.vmnetd.plist" "$PLIST"
fi
launchctl bootout "gui/$UID/dev.zyvor.fluxvm.vmnetd" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$PLIST"
