set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
V=0.4.22
for L in dev.zyvor.fluxvm dev.zyvor.fluxvm-agent dev.zyvor.fluxvm.kube; do
  P="$HOME/Library/LaunchAgents/$L.plist"
  [ -f "$P" ] || continue
  if grep -q "$ROOT/$V/bin/" "$P" && launchctl print "$GUI/$L" >/dev/null 2>&1; then echo "$L already runs $V"; continue; fi
  sed -i '' "s#$ROOT/[^/]*/bin/#$ROOT/$V/bin/#g" "$P"
  launchctl bootout "$GUI/$L" 2>/dev/null || true
  launchctl bootstrap "$GUI" "$P"
  echo "$L restarted on $V"
done
