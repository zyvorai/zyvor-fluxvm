#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Builds the .deb (and, once M2 lands, the .rpm) from the tree scripts/package-linux.sh stages, so the tarball and
# the packages hold the same files.
#
#   scripts/package-linux-pkgs.sh --stage target/package/fluxvm-0.4.12-linux-x86_64 [--format deb] [--out DIR]
#
# Needs nfpm (https://nfpm.goreleaser.com) and a staged tree (package-linux.sh --stage-only). Runs on any OS,
# because it only moves files. Layout: bin/ -> /usr/bin, lib/ -> /usr/lib/fluxvm, share/ -> /usr/share/fluxvm,
# and the unit -> /usr/lib/systemd/system with ExecStart pointing at /usr/bin (the tarball installs to /usr/local).
set -euo pipefail
cd "$(dirname "$0")/.."

STAGE=""; FORMAT=deb; OUT=target/package
while [[ $# -gt 0 ]]; do
  case "$1" in
    --stage) STAGE="$2"; shift 2 ;;
    --format) FORMAT="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    *) echo "usage: $0 --stage DIR [--format deb] [--out DIR]" >&2; exit 2 ;;
  esac
done
[[ -d "$STAGE" ]] || { echo "package-linux-pkgs: --stage must be a staged tree" >&2; exit 2; }
[[ "$FORMAT" == deb ]] || { echo "package-linux-pkgs: format '$FORMAT' is not supported yet (deb only)" >&2; exit 2; }
command -v nfpm >/dev/null || { echo "package-linux-pkgs: nfpm is not installed" >&2; exit 2; }

NAME="$(basename "$STAGE")"             # fluxvm-<version>-linux-<arch>
VERSION="${NAME#fluxvm-}"; VERSION="${VERSION%%-linux-*}"
case "${NAME##*-linux-}" in
  x86_64) ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) echo "package-linux-pkgs: cannot tell the architecture from $NAME" >&2; exit 2 ;;
esac
for f in bin/fluxctl share/systemd/fluxvm.service share/config.example.toml; do
  [[ -e "$STAGE/$f" ]] || { echo "package-linux-pkgs: $STAGE/$f is missing" >&2; exit 1; }
done

ROOT="$OUT/pkgroot-$ARCH"
rm -rf "$ROOT"
mkdir -p "$ROOT/usr/lib/systemd/system" "$ROOT/usr/share/fluxvm"
cp -a "$STAGE/bin" "$ROOT/usr/bin"
[[ -d "$STAGE/lib" ]] && cp -a "$STAGE/lib" "$ROOT/usr/lib/fluxvm"
mkdir -p "$ROOT/usr/lib/fluxvm"
# Everything under share/ except the unit, which goes to the systemd directory.
( cd "$STAGE/share" && find . -mindepth 1 -maxdepth 1 ! -name systemd -exec cp -a {} "$ROOT/usr/share/fluxvm/" \; )
mkdir -p "$ROOT/usr/share/fluxvm/apparmor"
cp packaging/apparmor/usr.bin.swtpm.fluxvm "$ROOT/usr/share/fluxvm/apparmor/"
[[ -f "$STAGE/LICENSE" ]] && cp "$STAGE/LICENSE" "$ROOT/usr/share/fluxvm/LICENSE"
# Completions belong where the shells look for them.
if [[ -d "$ROOT/usr/share/fluxvm/completions" ]]; then
  c="$ROOT/usr/share/fluxvm/completions"
  place() { [[ -f "$c/$1" ]] || return 0; mkdir -p "$ROOT/$2"; cp "$c/$1" "$ROOT/$2/$3"; chmod 0644 "$ROOT/$2/$3"; }
  place fluxctl.bash usr/share/bash-completion/completions fluxctl
  place _fluxctl usr/share/zsh/vendor-completions _fluxctl
  place fluxctl.fish usr/share/fish/vendor_completions.d fluxctl.fish
  rm -rf "$c"
fi
# Every unit in the package, with the tarball's /usr/local/bin paths turned into /usr/bin.
for u in "$STAGE"/share/systemd/*.service "$STAGE"/share/systemd/*.timer; do
  [ -e "$u" ] || continue   # an unmatched glob stays literal; a stage without timers has none
  sed 's|/usr/local/bin/|/usr/bin/|g' "$u" > "$ROOT/usr/lib/systemd/system/$(basename "$u")"
done
grep -q '^ExecStart=/usr/bin/fluxctl ' "$ROOT/usr/lib/systemd/system/fluxvm.service" \
  || { echo "package-linux-pkgs: the unit's ExecStart is not /usr/local/bin/fluxctl; update this script" >&2; exit 1; }

# Kernel module settings for qemu-nbd, and the upgrade policy template postinst copies to /etc/fluxvm/.
mkdir -p "$ROOT/usr/lib/modules-load.d" "$ROOT/usr/lib/modprobe.d"
cp packaging/deb/modules-load-fluxvm.conf "$ROOT/usr/lib/modules-load.d/fluxvm.conf"
cp packaging/deb/modprobe-fluxvm.conf "$ROOT/usr/lib/modprobe.d/fluxvm.conf"
cp packaging/deb/upgrade.conf "$ROOT/usr/share/fluxvm/upgrade.conf.example"

mkdir -p "$OUT"
# The tree entries in packaging/nfpm.yaml are absolute-from-root, so give nfpm absolute paths.
# nfpm expands the variables in the header fields but not in content paths, so the root is substituted here.
CONF="$OUT/nfpm-$ARCH.yaml"
sed "s|\${FLUXVM_ROOT}|$(cd "$ROOT" && pwd)|g" packaging/nfpm.yaml > "$CONF"
FLUXVM_VERSION="$VERSION" FLUXVM_ARCH="$ARCH" \
  nfpm package --config "$CONF" --packager "$FORMAT" --target "$OUT/fluxvm_${VERSION}_${ARCH}.$FORMAT"
PKG="fluxvm_${VERSION}_${ARCH}.$FORMAT"
if command -v sha256sum >/dev/null; then ( cd "$OUT" && sha256sum "$PKG" > "$PKG.sha256" ); else ( cd "$OUT" && shasum -a 256 "$PKG" > "$PKG.sha256" ); fi
echo "$OUT/$PKG"
