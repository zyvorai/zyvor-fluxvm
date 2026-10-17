#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Root-side wrapper for the privileged CI job (.github/workflows/vmm-guard-qos.yml).
# Install it root-owned and not writable by the runner user, and allow only this
# path in sudoers:
#   install -o root -g root -m 0755 scripts/ci-privileged-guard-test.sh /usr/local/sbin/fluxvm-ci-guard-test
#   echo 'ghrunner ALL=(root) NOPASSWD: /usr/local/sbin/fluxvm-ci-guard-test /home/ghrunner/*' \
#     > /etc/sudoers.d/ghrunner-fluxvm-guard-test   # check with visudo -c
#
# It copies the checkout (and the binaries the job built) into a root-owned
# temporary directory, so the runner user cannot change the code while it runs
# as root. It still runs the code of the commit under test as root: the real gate
# is the main-only trigger and the privileged-ebpf environment approval.
set -euo pipefail
[[ ${EUID:-$(id -u)} -eq 0 ]] || { echo "must run as root" >&2; exit 1; }
SRC="$(realpath -e "${1:?usage: fluxvm-ci-guard-test <checkout dir>}")"
case "$SRC" in /home/ghrunner/*) ;; *) echo "checkout must be under /home/ghrunner" >&2; exit 1 ;; esac
[[ -x "$SRC/target/debug/fluxvm-guard" && -x "$SRC/target/debug/fluxctl" ]] || { echo "build fluxvm-guard and fluxctl first" >&2; exit 1; }

WORK="$(mktemp -d /var/tmp/fluxvm-ci-guard.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/src" "$WORK/bin"
rsync -a --exclude .git --exclude target --exclude website "$SRC"/ "$WORK/src"/
cp "$SRC/target/debug/fluxvm-guard" "$SRC/target/debug/fluxctl" "$WORK/bin"/
chown -R root:root "$WORK"
cd "$WORK/src"
FLUXCTL="$WORK/bin/fluxctl" FLUXVM_GUARD_BIN="$WORK/bin/fluxvm-guard" \
  exec ./scripts/test-vmm-guard-exec-patch-host.sh
