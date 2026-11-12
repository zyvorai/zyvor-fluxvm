#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 scripts/test-microvm-policy.py
cargo test -p fluxvm-microvm
bin=target/debug/fluxvm-microvm
[[ -x $bin ]] || { cargo build -p fluxvm-microvm; }
out="$("$bin" --print-crd)"
# Match against a here-string, not `echo "$out" | grep -q ...`. grep -q exits at its first match; with
# `set -o pipefail` the writer then dies on the closed pipe ("echo: write error: Broken pipe", exit 1) whenever the
# CRD is larger than what it had already written, which is a race that failed CI on loaded runners. A here-string has
# no writer process whose status can leak into the result.
has() { grep -Eq -- "$1" <<<"$out"; }
has MicroVM
has MicroVMJob
has MicroVMPool
has GuestImage
has microvm.fluxvm.zyvor.io
has 'microvms\.microvm\.fluxvm\.zyvor\.io|"group": "microvm\.fluxvm\.zyvor\.io"'
echo "microvm gates passed"
