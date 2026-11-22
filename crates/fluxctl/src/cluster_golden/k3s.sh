set -eu
export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
command -v fluxctl >/dev/null 2>&1 || { echo "fluxctl is not installed here" >&2; exit 1; }
fluxctl kube up
