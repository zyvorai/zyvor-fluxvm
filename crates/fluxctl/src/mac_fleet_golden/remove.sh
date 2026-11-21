set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] && "$FLUXCTL" kube down --purge >/dev/null 2>&1 || true
for L in dev.zyvor.fluxvm.kube dev.zyvor.fluxvm-agent dev.zyvor.fluxvm; do
  launchctl bootout "$GUI/$L" 2>/dev/null || true
  rm -f "$HOME/Library/LaunchAgents/$L.plist"
done
rm -f "$HOME/.config/fluxvm/agent.token" "$HOME/.config/fluxvm/agent.token.new"
for b in "$HOME/.local/bin/"*; do
  [ -L "$b" ] && case "$(readlink "$b")" in "$ROOT"/*) rm -f "$b";; esac
done
rm -rf "$ROOT"
echo "removed FluxVM (VM images and data under ~/Library/Application Support/FluxVM are kept)"
