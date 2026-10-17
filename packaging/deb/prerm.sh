#!/bin/sh
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Before remove. On upgrade the daemon is left alone (deb passes "upgrade" as $1, rpm passes 1); on a real removal
# it is stopped and disabled. KillMode=process means running VMs survive the stop.
set -e
case "$1" in
    remove|0)
        if [ -d /run/systemd/system ]; then
            systemctl stop fluxvm.service >/dev/null 2>&1 || true
            systemctl disable fluxvm.service >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
