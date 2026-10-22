#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Static checks on deploy/apparmor/fluxvm: rules the Kairon node and
# storage=shared disks depend on must stay in both enforced profiles.
set -euo pipefail
PROFILE="$(cd "$(dirname "$0")/.." && pwd)/deploy/apparmor/fluxvm"
fail=0

# Prints the body of one profile.
body() { awk -v n="$1" '$1=="profile" && $2==n {p=1} p {print} p && /^}/ {exit}' "$PROFILE"; }

for prof in fluxctl fluxvm; do
    b="$(body "$prof")"
    [ -n "$b" ] || { echo "FAIL: profile $prof not found"; fail=1; continue; }
    # kairon-node image cache is read by FluxVM as the disk's backing file
    if grep -qE '^\s*/var/lib/kairon-node/image-cache/\*\* r?k?[rk]+,' <<<"$b"; then
        echo "ok:   $prof reads the kairon-node image cache"
    else
        echo "FAIL: $prof lacks /var/lib/kairon-node/image-cache/** rk"; fail=1
    fi
    # image lock on a storage=shared disk
    if grep -qE '^\s*owner /\*\* rwk,' <<<"$b"; then
        echo "ok:   $prof allows file locks on owner files"
    else
        echo "FAIL: $prof lacks 'owner /** rwk'"; fail=1
    fi
done

if command -v apparmor_parser >/dev/null 2>&1; then
    # -K: this check runs unprivileged, so it must not try to write the policy cache in /var/cache/apparmor
    apparmor_parser -QK "$PROFILE" && echo "ok:   apparmor_parser accepts the profile" \
        || { echo "FAIL: apparmor_parser rejected the profile"; fail=1; }
fi
exit $fail
