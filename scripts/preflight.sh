#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Is this host ready for FluxVM? With `fluxctl` on PATH (or built in ./target) this runs `fluxctl doctor --host-only`, so the
# script and the command cannot drift apart. Without it (a fresh machine, before install) the checks below run instead.
#   scripts/preflight.sh [--native-kvm]
set -euo pipefail

profile="${1:-all}"
case "$profile" in
  all|--native-kvm) ;;
  *) echo "usage: $0 [--native-kvm]" >&2; exit 2 ;;
esac

missing=0
here="$(cd "$(dirname "$0")/.." && pwd)"
fluxctl="$(command -v fluxctl 2>/dev/null || true)"
for cand in "$here/target/release/fluxctl" "$here/target/debug/fluxctl"; do
  [[ -z "$fluxctl" && -x "$cand" ]] && fluxctl="$cand"
done

check() { # name required|optional
  if command -v "$1" >/dev/null 2>&1; then
    printf '%-24s %s\n' "$1" "OK ($(command -v "$1"))"
  elif [[ "$2" == required ]]; then
    printf '%-24s %s\n' "$1" "MISSING"; missing=1
  else
    printf '%-24s %s\n' "$1" "optional/not installed"
  fi
}

# A `fluxctl` too old to know `doctor --host-only` falls through to the plain checks.
if [[ -n "$fluxctl" ]] && "$fluxctl" doctor --help 2>/dev/null | grep -q -- --host-only; then
  "$fluxctl" doctor --host-only || missing=1
else
  [[ -n "$fluxctl" ]] && echo "note: $fluxctl has no 'doctor --host-only'; running the basic checks" >&2
  for c in qemu-system-x86_64 qemu-img cloud-localds ip; do check "$c" required; done
  for c in cloud-hypervisor firecracker fluxctl fluxvm-hypervisor bpftool tc dnsmasq; do check "$c" optional; done
  if [[ -e /dev/kvm ]]; then echo "/dev/kvm                 OK"; else echo "/dev/kvm                 MISSING"; missing=1; fi
fi

# The native-KVM profile needs its own binaries on top of the host checks.
if [[ "$profile" == --native-kvm ]]; then
  for c in fluxctl fluxvm-hypervisor cp ip; do check "$c" required; done
  for c in debugfs bpftool tc; do check "$c" optional; done
fi

exit "$missing"
