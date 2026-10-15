#!/bin/bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# FluxVM macOS guest first-boot helper. Run it once inside the guest as an admin (or from a LaunchDaemon baked into the
# template). It reads the FluxVM first-boot share (apple.firstboot), installs the keys as a root-owned AuthorizedKeysFile
# outside any home directory, so fresh clones accept them before anyone has logged in, and turns on Remote Login.
# Optionally creates a local admin account and enables auto-login (fallback when native VZMacGuestProvisioningOptions
# is unavailable).
#
#   bash "/Volumes/My Shared Files/firstboot/firstboot.sh" [share-dir]

set -euo pipefail

SRC="${1:-/Volumes/My Shared Files/firstboot}"
KEYS_DST=/etc/ssh/fluxvm_authorized_keys
SSHD_DROPIN=/etc/ssh/sshd_config.d/050-fluxvm.conf

if [[ $EUID -ne 0 ]]; then
  exec sudo "$0" "$@"
fi

if [[ ! -f "$SRC/authorized_keys" ]]; then
  echo "firstboot: no authorized_keys under $SRC" >&2
  exit 1
fi

# Optional local admin account (sysadminctl). Safe to re-run: -addUser fails if the user exists.
if [[ -f "$SRC/username" ]]; then
  USERNAME=$(tr -d '[:space:]' < "$SRC/username")
  FULL_NAME="${USERNAME}"
  [[ -f "$SRC/full_name" ]] && FULL_NAME=$(tr -d '\n' < "$SRC/full_name")
  PASSWORD=""
  [[ -f "$SRC/password" ]] && PASSWORD=$(tr -d '\n' < "$SRC/password")
  if ! id "$USERNAME" >/dev/null 2>&1; then
    if [[ -n "$PASSWORD" ]]; then
      sysadminctl -addUser "$USERNAME" -fullName "$FULL_NAME" -password "$PASSWORD" -admin >/dev/null
    else
      sysadminctl -addUser "$USERNAME" -fullName "$FULL_NAME" -admin >/dev/null || true
    fi
  fi
  if [[ -f "$SRC/auto_login" && -n "$USERNAME" ]]; then
    # Best-effort auto-login. Requires the password and a writable kcpassword.
    if [[ -n "$PASSWORD" ]] && command -v sysadminctl >/dev/null; then
      sysadminctl -autologin set -userName "$USERNAME" -password "$PASSWORD" >/dev/null 2>&1 || true
    fi
  fi
fi

install -m 0644 -o root -g wheel "$SRC/authorized_keys" "$KEYS_DST"
mkdir -p "$(dirname "$SSHD_DROPIN")"
printf 'AuthorizedKeysFile .ssh/authorized_keys %s\n' "$KEYS_DST" >"$SSHD_DROPIN"
chmod 0644 "$SSHD_DROPIN"

if [[ -f "$SRC/enable_remote_login" ]]; then
  systemsetup -setremotelogin on >/dev/null 2>&1 || launchctl enable system/com.openssh.sshd || true
fi
launchctl kickstart -k system/com.openssh.sshd 2>/dev/null || true
echo "firstboot: installed $(grep -c . "$KEYS_DST" || true) key(s) into $KEYS_DST"
