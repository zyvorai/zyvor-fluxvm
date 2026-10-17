#!/bin/sh
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# After install and upgrade. POSIX sh, shared by deb and rpm (rpm passes 1 for install and 2 for upgrade as $1).
# Never starts the daemon on a fresh install (the seeded config is a template) and never restarts it on upgrade
# unless /etc/fluxvm/upgrade.conf says RESTART_ON_UPGRADE=yes.
set -e

SHARE=/usr/share/fluxvm
upgrading=0
case "$1" in
    configure) [ -n "$2" ] && upgrading=1 ;;   # deb: $2 is the previously configured version
    2) upgrading=1 ;;                          # rpm
esac

install -d -m 0755 /var/lib/fluxvm /etc/fluxvm

# The node's real config is not a conffile: seed it once and never touch it again.
if [ ! -e /etc/fluxvm.toml ] && [ -f "$SHARE/config.example.toml" ]; then
    install -m 0644 "$SHARE/config.example.toml" /etc/fluxvm.toml
fi
[ -e /etc/fluxvm/upgrade.conf ] || install -m 0644 "$SHARE/upgrade.conf.example" /etc/fluxvm/upgrade.conf

# qemu-bridge-helper refuses every bridge without this file; the daemon already runs as root with CAP_NET_ADMIN,
# so "allow all" adds no isolation (see scripts/deploy-remote.sh).
if [ ! -f /etc/qemu/bridge.conf ]; then
    install -d -m 0755 /etc/qemu
    printf 'allow all\n' > /etc/qemu/bridge.conf
fi

# The swtpm sidecar needs a path AppArmor's stock profile does not allow; only when AppArmor is enforcing.
if [ -d /etc/apparmor.d/local ] && command -v apparmor_parser >/dev/null 2>&1 \
   && [ -r /sys/module/apparmor/parameters/enabled ] && grep -q '^Y' /sys/module/apparmor/parameters/enabled; then
    local_file=/etc/apparmor.d/local/usr.bin.swtpm
    if ! grep -qs 'fluxvm/instances' "$local_file"; then
        cat "$SHARE/apparmor/usr.bin.swtpm.fluxvm" >> "$local_file"
        if [ -f /etc/apparmor.d/usr.bin.swtpm ]; then
            apparmor_parser -r /etc/apparmor.d/usr.bin.swtpm 2>/dev/null || true
        fi
    fi
fi

modprobe nbd max_part=16 2>/dev/null || true

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
    systemctl enable fluxvm.service >/dev/null 2>&1 || true
    if [ "$upgrading" = 1 ] && systemctl is-active --quiet fluxvm.service; then
        RESTART_ON_UPGRADE=no
        # shellcheck disable=SC1091
        [ -r /etc/fluxvm/upgrade.conf ] && . /etc/fluxvm/upgrade.conf
        if [ "$RESTART_ON_UPGRADE" = yes ]; then
            systemctl restart fluxvm.service || true
        else
            echo "fluxvm: the running daemon keeps the old binary until it is restarted (systemctl restart fluxvm)."
        fi
    fi
fi
exit 0
