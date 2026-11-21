set -eu
umask 077
D="$HOME/.config/fluxvm"
mkdir -p "$D"
cat > "$D/agent.token.new"
mv "$D/agent.token.new" "$D/agent.token"
