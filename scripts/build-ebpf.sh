#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Build fluxvm_tc.bpf.o, fluxvm_xdp.bpf.o and runtime-intelligence BPF into dist/bpf/ (or OUT_DIR).
# Service Fabric objects are also emitted as map-tier variants
# (fluxvm_service_tier_{S,M,L}.bpf.o and matching xdp/connect ELFs).
# Docs: docs/network-fabric.md · docs/service-fabric-phase6.md · docs/runtime-intelligence.md
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${1:-$ROOT/dist/bpf}"
CLANG="${CLANG:-clang}"
mkdir -p "$OUT_DIR"

# FLUXVM_BPF_ARCH=x86|arm64|... builds for another architecture than the host (a cross-built package).
case "${FLUXVM_BPF_ARCH:-$(uname -m)}" in
  x86|x86_64) BPF_ARCH=x86 ;;
  aarch64|arm64) BPF_ARCH=arm64 ;;
  s390x) BPF_ARCH=s390 ;;
  ppc64le) BPF_ARCH=powerpc ;;
  riscv64) BPF_ARCH=riscv ;;
  *) echo "unsupported BPF build architecture: ${FLUXVM_BPF_ARCH:-$(uname -m)}" >&2; exit 2 ;;
esac

CFLAGS=(
  -target bpf
  -O2
  -g
  -Wall
  -Werror
  "-D__TARGET_ARCH_${BPF_ARCH}"
)

# Debian/Ubuntu place asm/types.h under the multiarch include path of the architecture being built for. For a cross build
# that is the target's (linux-libc-dev:arm64), never the host gcc's: x86's asm/ptrace.h has no struct user_pt_regs.
case "$BPF_ARCH" in
  x86) MULTIARCH=x86_64-linux-gnu ;;
  arm64) MULTIARCH=aarch64-linux-gnu ;;
  *) MULTIARCH="$(gcc -print-multiarch 2>/dev/null || true)" ;;
esac
if [[ -n "$MULTIARCH" && -d "/usr/include/$MULTIARCH" ]]; then
  CFLAGS+=("-I/usr/include/$MULTIARCH")
elif [[ "$BPF_ARCH" != "$(uname -m | sed 's/x86_64/x86/;s/aarch64/arm64/')" ]]; then
  echo "build-ebpf: no /usr/include/${MULTIARCH:-<target>} for a $BPF_ARCH build (install linux-libc-dev for that architecture)" >&2
  exit 2
fi

# Service programs exceed clang 18's default 512-byte BPF stack verifier view;
# raise the compile-time stack size so Maglev/NAT/policy frames fit (runtime
# verifier still enforces the kernel limit via per-CPU scratch where needed).
# Older clang (e.g. bookworm in the container image) rejects the flag.
SERVICE_CFLAGS=("${CFLAGS[@]}")
if "$CLANG" -target bpf -mllvm -bpf-stack-size=768 -x c -c /dev/null -o /dev/null 2>/dev/null; then
  SERVICE_CFLAGS+=(-mllvm -bpf-stack-size=768)
else
  echo "note: clang lacks -bpf-stack-size; building service objects with default stack view" >&2
fi

for src in fluxvm_tc fluxvm_direct fluxvm_vnet fluxvm_pod_ingress fluxvm_xdp fluxvm_intelligence fluxvm_qemu_device fluxvm_qemu_egress; do
  "$CLANG" "${CFLAGS[@]}" -c "$ROOT/bpf/${src}.bpf.c" -o "$OUT_DIR/${src}.bpf.o"
done

# Default M objects keep stable names (fluxvm_service.bpf.o, …).
for src in fluxvm_service fluxvm_service_xdp fluxvm_service_connect; do
  "$CLANG" "${SERVICE_CFLAGS[@]}" -DFLUXVM_MAP_TIER=M \
    -c "$ROOT/bpf/${src}.bpf.c" -o "$OUT_DIR/${src}.bpf.o"
done

# Explicit S/M/L tier ELFs for map_tier object selection.
for tier in S M L; do
  for src in fluxvm_service fluxvm_service_xdp fluxvm_service_connect; do
    "$CLANG" "${SERVICE_CFLAGS[@]}" -DFLUXVM_MAP_TIER="${tier}" \
      -c "$ROOT/bpf/${src}.bpf.c" \
      -o "$OUT_DIR/${src}_tier_${tier}.bpf.o"
  done
done

# Keep BTF sections intact. bpftool/libbpf uses the BTF-described map
# definitions emitted by modern clang. The objects are small enough that
# stripping them buys little and can vary across distro LLVM versions.

echo "built:"
echo "  $OUT_DIR/fluxvm_tc.bpf.o"
echo "  $OUT_DIR/fluxvm_direct.bpf.o"
echo "  $OUT_DIR/fluxvm_vnet.bpf.o"
echo "  $OUT_DIR/fluxvm_pod_ingress.bpf.o"
echo "  $OUT_DIR/fluxvm_xdp.bpf.o"
echo "  $OUT_DIR/fluxvm_intelligence.bpf.o"
echo "  $OUT_DIR/fluxvm_qemu_device.bpf.o"
echo "  $OUT_DIR/fluxvm_qemu_egress.bpf.o"
echo "  $OUT_DIR/fluxvm_service.bpf.o  (default M)"
echo "  $OUT_DIR/fluxvm_service_xdp.bpf.o  (default M)"
echo "  $OUT_DIR/fluxvm_service_connect.bpf.o  (default M)"
for tier in S M L; do
  echo "  $OUT_DIR/fluxvm_service_tier_${tier}.bpf.o"
  echo "  $OUT_DIR/fluxvm_service_xdp_tier_${tier}.bpf.o"
  echo "  $OUT_DIR/fluxvm_service_connect_tier_${tier}.bpf.o"
done
