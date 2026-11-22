#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Build the pre-baked Docker engine image: the Debian 13 cloud image with Docker installed and cloud-init finished, so the
# first boot of a Docker VM (`fluxctl docker up`) does not run apt. Publish the three files it writes (the image, .sha256 and,
# with --sign-key, .sig) at a URL and give that URL to the installer: install.sh --engine-image-url URL (or FLUXVM_ENGINE_IMAGE_URL).
# install.sh refuses the image unless the .sha256 matches, and, when FLUXVM_RELEASE_PUBKEY is set, unless the Ed25519 .sig verifies
# (the same raw-Ed25519-over-the-file scheme as the release manifests).
#
#   scripts/build-engine-image.sh --out DIR [--base debian-13-genericcloud-amd64.qcow2] [--sign-key release.key] [--dry-run]
#
# Needs: qemu-system-x86_64 (or -aarch64), qemu-img, cloud-localds, /dev/kvm for speed, and a network for apt inside the guest.
# Status: written against the documented tools; not run end to end on a real host yet (see CHANGELOG).
set -euo pipefail

OUT=""
BASE=""
SIGN_KEY=""
DRY=0
MEM="${ENGINE_BUILD_MEMORY_MIB:-2048}"
TIMEOUT="${ENGINE_BUILD_TIMEOUT_SECS:-1800}"

die() { printf '%s\n' "$1" >&2; exit "${2:-1}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="${2:-}"; shift 2 ;;
    --base) BASE="${2:-}"; shift 2 ;;
    --sign-key) SIGN_KEY="${2:-}"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) die "unknown option: $1" 64 ;;
  esac
done
[ -n "$OUT" ] || die "--out DIR is required" 64

case "$(uname -m)" in
  aarch64|arm64) ARCH=arm64; QEMU=qemu-system-aarch64; MACHINE="-machine virt -cpu host -bios /usr/share/qemu-efi-aarch64/QEMU_EFI.fd" ;;
  *) ARCH=amd64; QEMU=qemu-system-x86_64; MACHINE="-machine q35" ;;
esac
NAME="fluxvm-engine-$ARCH.qcow2"
[ -n "$BASE" ] || BASE="${XDG_CACHE_HOME:-$HOME/.cache}/fluxvm/images/debian-13-genericcloud-$ARCH.qcow2"

USERDATA='#cloud-config
package_update: true
packages: [docker.io, docker-compose, ca-certificates]
runcmd:
  - [systemctl, enable, docker]
  # leave no first-boot state behind: the seed of whoever boots this image runs cloud-init again from scratch
  - [sh, -c, "apt-get clean && rm -rf /var/lib/apt/lists/*"]
  - [cloud-init, clean, --logs, --seed, --machine-id]
power_state:
  mode: poweroff
  timeout: 600
  condition: true
'

if [ "$DRY" = 1 ]; then
  echo "would copy $BASE and boot it with a cloud-init seed that installs docker.io and powers off"
  echo "would write $OUT/$NAME, $OUT/$NAME.sha256${SIGN_KEY:+ and $OUT/$NAME.sig}"
  printf '%s' "$USERDATA"
  exit 0
fi

for t in "$QEMU" qemu-img cloud-localds; do command -v "$t" >/dev/null 2>&1 || die "$t is not installed (curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --server installs it)"; done
[ -f "$BASE" ] || die "base image $BASE not found: run the installer first, or pass --base"
mkdir -p "$OUT"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-engine-build.XXXXXX")"
trap 'rm -rf "${WORK:?}"' EXIT

cp "$BASE" "$WORK/disk.qcow2"
qemu-img resize "$WORK/disk.qcow2" 8G >/dev/null
printf '%s' "$USERDATA" > "$WORK/user-data"
printf 'instance-id: fluxvm-engine-build\nlocal-hostname: engine-build\n' > "$WORK/meta-data"
cloud-localds "$WORK/seed.img" "$WORK/user-data" "$WORK/meta-data"

ACCEL=""
if [ -w /dev/kvm ]; then ACCEL="-enable-kvm"; else echo "warning: no /dev/kvm: this will be slow" >&2; fi
echo "==> Booting the base image once (cloud-init installs Docker, then the guest powers off)"
# shellcheck disable=SC2086
timeout "$TIMEOUT" "$QEMU" $MACHINE $ACCEL -m "$MEM" -smp 2 -nographic \
  -drive "file=$WORK/disk.qcow2,if=virtio,format=qcow2" \
  -drive "file=$WORK/seed.img,if=virtio,format=raw,readonly=on" \
  -netdev user,id=n0 -device virtio-net-pci,netdev=n0 >"$WORK/console.log" 2>&1 \
  || { tail -n 30 "$WORK/console.log" >&2; die "the build VM did not finish within ${TIMEOUT}s (console above)"; }

echo "==> Compressing"
qemu-img convert -c -O qcow2 "$WORK/disk.qcow2" "$OUT/$NAME"
if command -v sha256sum >/dev/null 2>&1; then SUM="$(sha256sum "$OUT/$NAME" | cut -d' ' -f1)"; else SUM="$(shasum -a 256 "$OUT/$NAME" | cut -d' ' -f1)"; fi
printf '%s  %s\n' "$SUM" "$NAME" > "$OUT/$NAME.sha256"
if [ -n "$SIGN_KEY" ]; then
  openssl pkeyutl -sign -inkey "$SIGN_KEY" -rawin -in "$OUT/$NAME" -out "$OUT/$NAME.sig"
fi
echo "==> $OUT/$NAME ($SUM)"
echo "    Publish it with its .sha256${SIGN_KEY:+ and .sig}, then: install.sh --engine-image-url https://YOUR-MIRROR/$NAME"
