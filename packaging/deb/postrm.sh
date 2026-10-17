#!/bin/sh
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# After remove and purge. State (/var/lib/fluxvm: VMs, images, keys) is never deleted, not even on purge:
# a package manager should not be able to destroy a node's VMs. /etc/fluxvm.toml and /etc/fluxvm stay too.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi
case "$1" in
    purge)
        # Only what the package added to a file it does not own.
        if [ -f /etc/apparmor.d/local/usr.bin.swtpm ] && grep -q 'fluxvm/instances' /etc/apparmor.d/local/usr.bin.swtpm; then
            echo "fluxvm: left the fluxvm lines in /etc/apparmor.d/local/usr.bin.swtpm; remove them by hand if unwanted."
        fi
        ;;
esac
exit 0
