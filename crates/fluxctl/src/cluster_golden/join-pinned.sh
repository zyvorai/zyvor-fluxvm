set -eu
export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
T="$(umask 077; mktemp -d)"
trap 'rm -rf "$T"' EXIT
curl -fsSL -o "$T/install.sh" 'https://mirror.example/install.sh'
if command -v sha256sum >/dev/null 2>&1; then G="$(sha256sum "$T/install.sh" | cut -d' ' -f1)"; else G="$(shasum -a 256 "$T/install.sh" | cut -d' ' -f1)"; fi
[ "$G" = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' ] || { echo "install.sh does not match the pinned SHA-256 (got $G); not running it" >&2; exit 1; }
(umask 077; cat > "$T/token")
bash "$T/install.sh" --join 'http://10.0.0.2:7799' --token-file "$T/token" --node-name 'worker-1' --yes --label 'zone=lab' --label 'gpu=true' --no-selftest --with-k3s
