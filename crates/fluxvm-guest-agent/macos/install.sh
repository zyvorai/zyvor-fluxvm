#!/bin/sh
# Installs the FluxVM guest agent as a LaunchDaemon inside a macOS guest.
# Run as root, in the guest:  sudo ./install.sh /path/to/fluxvm-guest-agent [token]
#
# EXPERIMENTAL. Verified on an Apple M4 with a macOS 27.0.1 guest created with `apple.macos_vsock: true`: the daemon
# starts, binds AF_VSOCK and answers ping, exec and file transfer from the host. Without a token the agent refuses
# every request (fail closed). Uninstall:  sudo ./install.sh --uninstall
set -eu

LABEL=dev.zyvor.fluxvm-guest-agent
BIN=/usr/local/libexec/fluxvm-guest-agent
PLIST=/Library/LaunchDaemons/$LABEL.plist
TOKEN=/etc/fluxvm-guest-agent.token
HERE=$(cd "$(dirname "$0")" && pwd)

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
[ "$(uname -s)" = Darwin ] || { echo "this installer is for macOS guests" >&2; exit 1; }

if [ "${1:-}" = "--uninstall" ]; then
  launchctl bootout "system/$LABEL" 2>/dev/null || true
  rm -f "$PLIST" "$BIN" "$TOKEN"
  echo "removed $LABEL"
  exit 0
fi

SRC=${1:?usage: install.sh /path/to/fluxvm-guest-agent [token] | --uninstall}
[ -x "$SRC" ] || { echo "$SRC is not an executable" >&2; exit 1; }

mkdir -p "$(dirname "$BIN")"
install -m 0755 -o root -g wheel "$SRC" "$BIN"
if [ -n "${2:-}" ]; then
  umask 077
  printf '%s\n' "$2" > "$TOKEN"
  chown root:wheel "$TOKEN"
  chmod 0600 "$TOKEN"
elif [ ! -s "$TOKEN" ]; then
  echo "warning: no token at $TOKEN; the agent will refuse every request until one is provisioned" >&2
fi
install -m 0644 -o root -g wheel "$HERE/$LABEL.plist" "$PLIST"

launchctl bootout "system/$LABEL" 2>/dev/null || true
launchctl bootstrap system "$PLIST"
launchctl enable "system/$LABEL"
echo "installed $LABEL (logs: /var/log/fluxvm-guest-agent.log)"
