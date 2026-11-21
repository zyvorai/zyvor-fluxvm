set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] || { echo "fluxctl is not installed" >&2; exit 1; }
if launchctl print "$GUI/dev.zyvor.fluxvm.kube" >/dev/null 2>&1; then echo "k3s agent already loaded"; else "$FLUXCTL" kube agent install --cpus 2 --memory-mib 4096 --disk-gib 30 >/dev/null; fi
i=0
until "$FLUXCTL" kube status 2>/dev/null | grep -q '"api_answers": true'; do
  i=$((i+5)); [ "$i" -le 300 ] || { echo "k3s did not answer within 300s; it keeps starting in the background (fluxctl kube status)" >&2; exit 1; }
  sleep 5
done
echo "k3s API answers"
