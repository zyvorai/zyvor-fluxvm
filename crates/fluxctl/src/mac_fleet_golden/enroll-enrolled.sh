set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] || { echo "fluxctl is not installed" >&2; exit 1; }
TOKF="$HOME/.config/fluxvm/agent.token"
CRED="$HOME/Library/Application Support/FluxVM/agent/node-credential"
"$FLUXCTL" node-agent install --name 'mini-1' --central 'https://fleet.example:7790' --label 'zone=lab' >/dev/null
echo "node agent installed (already enrolled)"
