set -eu
export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
T="$(umask 077; mktemp -d)"
trap 'rm -rf "$T"' EXIT
curl -fsSL -o "$T/install.sh" 'https://zyvorai.github.io/zyvor-fluxvm/install.sh'
(umask 077; cat > "$T/token")
bash "$T/install.sh" --join 'http://10.0.0.2:7799' --token-file "$T/token" --node-name 'worker-1' --yes --advertise-url 'http://10.0.0.5:7788' --label 'zone=lab' --label 'gpu=true'
