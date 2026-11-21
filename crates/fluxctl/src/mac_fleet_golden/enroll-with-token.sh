set -eu
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
[ -n "$FLUXCTL" ] || { echo "fluxctl is not installed" >&2; exit 1; }
TOKF="$HOME/.config/fluxvm/agent.token"
CRED="$HOME/Library/Application Support/FluxVM/agent/node-credential"
"$FLUXCTL" node-agent install --name 'mini-1' --central 'https://fleet.example:7790' --token-file "$TOKF" --label 'zone=lab' >/dev/null
i=0
until [ -f "$CRED" ] || [ "$i" -ge 30 ]; do sleep 2; i=$((i+1)); done
if [ -f "$CRED" ]; then rm -f "$TOKF"; echo "enrolled; join token removed from this Mac"
else echo "the node did not enroll within 60s; the join token file is kept at $TOKF (see ~/Library/Logs/fluxvm-agent.log)" >&2; exit 1; fi
