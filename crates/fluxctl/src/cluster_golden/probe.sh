export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
yn() { if "$@" >/dev/null 2>&1; then echo yes; else echo no; fi; }
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "distro=$(. /etc/os-release 2>/dev/null; echo "${ID:-}")"
echo "user=$(id -un)"
if [ "$(id -u)" = 0 ]; then S=root
elif sudo -n true >/dev/null 2>&1; then S=passwordless
elif command -v sudo >/dev/null 2>&1; then S=password
else S=none; fi
echo "sudo=$S"
if [ -d /run/systemd/system ]; then I=systemd; elif command -v rc-service >/dev/null 2>&1; then I=openrc; else I=none; fi
echo "init=$I"
echo "kvm=$(yn test -e /dev/kvm)"
echo "curl=$(yn command -v curl)"
echo "fluxctl_version=$(fluxctl --version 2>/dev/null | awk '{print $2}')"
echo "agent_active=$(yn systemctl is-active --quiet fluxvm-agent)"
echo "enrolled=$(yn test -e /var/lib/fluxvm-agent/node-credential)"
