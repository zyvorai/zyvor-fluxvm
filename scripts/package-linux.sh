#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Builds fluxctl, fluxvm-hypervisor and fluxvm-agent and packs them for a GitHub release:
#   target/package/fluxvm-<version>-linux-<arch>.tar.gz  (+ .sha256)
#   scripts/package-linux.sh [--profile release|debug] [--version X.Y.Z]
# Needs Linux, Rust, the packages ci.yml installs (libsystemd-dev libhivex-dev clang llvm libbpf-dev linux-libc-dev),
# and guestkit checked out next to this repository (fluxvm-image uses it). Same package shape as package-macos.sh:
# one directory holding bin/, share/config.example.toml and share/systemd/, which scripts/install.sh unpacks.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Linux ]] || { echo "Linux only" >&2; exit 2; }

PROFILE=release
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/fluxctl/Cargo.toml | head -n1)"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    *) echo "usage: $0 [--profile release|debug] [--version X.Y.Z]" >&2; exit 2 ;;
  esac
done
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || { echo "not a version: $VERSION" >&2; exit 2; }

case "$(uname -m)" in
  x86_64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac

if [[ "$PROFILE" == release ]]; then FLAG=--release; else FLAG=; fi
# shellcheck disable=SC2086
cargo build $FLAG -p fluxctl -p fluxvm-hypervisor -p fluxvm-agent

NAME="fluxvm-$VERSION-linux-$ARCH"
OUT="target/package"; STAGE="$OUT/$NAME"
rm -rf "$STAGE" "$OUT/$NAME.tar.gz" "$OUT/$NAME.tar.gz.sha256"
mkdir -p "$STAGE/bin" "$STAGE/share/systemd"
for b in fluxctl fluxvm-hypervisor fluxvm-agent; do
  cp "target/$PROFILE/$b" "$STAGE/bin/$b"
  # Release binaries are stripped to keep the download small; a debug package is left as built.
  if [[ "$PROFILE" == release ]]; then strip "$STAGE/bin/$b" 2>/dev/null || true; fi
done
cp config.example.toml "$STAGE/share/config.example.toml"
cp packaging/systemd/fluxvm.service "$STAGE/share/systemd/fluxvm.service"
cp LICENSE "$STAGE/LICENSE" 2>/dev/null || true

tar -czf "$OUT/$NAME.tar.gz" -C "$OUT" "$NAME"
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
echo "$OUT/$NAME.tar.gz"
cat "$OUT/$NAME.tar.gz.sha256"
