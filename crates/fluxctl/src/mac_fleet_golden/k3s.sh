set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] || { echo "fluxctl is not installed" >&2; exit 1; }
if launchctl print "$GUI/dev.zyvor.fluxvm.kube" >/dev/null 2>&1; then echo "k3s agent already loaded"; else "$FLUXCTL" kube agent install --cpus 2 --memory-mib 4096 --disk-gib 30 >/dev/null; fi
echo "k3s is starting in the background; fluxctl mac-fleet status shows when it answers"
