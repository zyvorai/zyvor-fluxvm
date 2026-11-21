set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] || { echo "fluxctl is not installed" >&2; exit 1; }
if launchctl print "$GUI/dev.zyvor.fluxvm" >/dev/null 2>&1; then echo "daemon already loaded"; else "$FLUXCTL" service install >/dev/null; fi
i=0
until curl -fsS -m 2 http://127.0.0.1:7788/healthz >/dev/null 2>&1; do
  i=$((i+1)); [ "$i" -le 30 ] || { echo "the daemon did not answer /healthz within 30s (see ~/Library/Logs/fluxvm.log)" >&2; exit 1; }
  sleep 1
done
echo "daemon healthy"
