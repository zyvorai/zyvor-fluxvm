#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Install Firecracker (+ jailer) from upstream GitHub releases, verified against a SHA-256 digest: the one GitHub
# records for the release asset, or one you pin.
#
# Usage:
#   ./scripts/install-firecracker.sh                    # latest release (calls the GitHub API)
#   ./scripts/install-firecracker.sh v1.17.0            # pinned version (digest from the GitHub API)
#   ./scripts/install-firecracker.sh --lock packaging/vendor.lock
#                                                       # release builds: version, URL and SHA-256 from the lock; no API
#   ./scripts/install-firecracker.sh --version v1.17.0 --sha256 HEX    # pinned digest, no API
#   ./scripts/install-firecracker.sh --version v1.17.0 --sha256 HEX --from-file firecracker-v1.17.0-x86_64.tgz
#                                                       # already downloaded archive, verified, no network at all
#   --dry-run prints what would be installed and exits (no network, no install; works on any OS; --arch x86_64|aarch64).
#
# Env:
#   FIRECRACKER_VERSION   Pin a version (overridden by --version or a positional arg)
#   INSTALL_DIR           Where to install binaries (default /usr/local/bin)
set -euo pipefail

REPO="firecracker-microvm/firecracker"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
VERSION="${FIRECRACKER_VERSION:-}"
LOCK=""; FROM_FILE=""; SHA256_PIN=""; DRY_RUN=false; ARCH_OPT=""

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        --lock) LOCK="${2:?--lock needs a file}"; shift 2 ;;
        --from-file) FROM_FILE="${2:?--from-file needs a file}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --sha256) SHA256_PIN="${2:?--sha256 needs a value}"; shift 2 ;;
        --arch) ARCH_OPT="${2:?--arch needs a value}"; shift 2 ;;
        --dry-run) DRY_RUN=true; shift ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) VERSION="$1"; shift ;;
    esac
done

info() { echo "  [*] $*"; }
ok()   { echo "  [ok] $*"; }
fail() { echo "  [FAIL] $*" >&2; exit 1; }

[ "$DRY_RUN" = true ] || [ "$(uname -s)" = "Linux" ] || fail "Firecracker only runs on Linux/KVM"
case "${ARCH_OPT:-$(uname -m)}" in
    x86_64)  ARCH="x86_64" ;;
    aarch64) ARCH="aarch64" ;;
    *) fail "unsupported architecture: ${ARCH_OPT:-$(uname -m)}" ;;
esac

SHA_CMD=""
if command -v sha256sum >/dev/null; then SHA_CMD="sha256sum"; elif command -v shasum >/dev/null; then SHA_CMD="shasum -a 256"; fi
if [ "$DRY_RUN" != true ]; then
    command -v curl >/dev/null || fail "curl is required"
    command -v tar >/dev/null || fail "tar is required"
    [ -n "$SHA_CMD" ] || fail "sha256sum or shasum is required"
fi

SUDO=""
[ "$(id -u)" -ne 0 ] && [ ! -w "$INSTALL_DIR" ] && SUDO="sudo"

api() { curl -fsSL -H "Accept: application/vnd.github+json" "$@"; }

# lock_get <component> <field>: field 3 version, 4 sha256, 5 url, for this architecture.
lock_get() { awk -v c="$1" -v a="$ARCH" -v f="$2" '$1 !~ /^#/ && $1 == c && $2 == a { print $f; exit }' "$LOCK"; }

ASSET=""; DOWNLOAD_URL=""; DIGEST=""; OFFLINE=false
if [ -n "$LOCK" ]; then
    [ -r "$LOCK" ] || fail "cannot read lock file ${LOCK}"
    LV="$(lock_get firecracker 3)"
    [ -n "$LV" ] || fail "${LOCK} has no firecracker entry for ${ARCH}"
    if [ -n "$VERSION" ] && [ "$VERSION" != "$LV" ]; then
        fail "requested ${VERSION} but ${LOCK} pins ${LV}"
    fi
    VERSION="$LV"
    DOWNLOAD_URL="$(lock_get firecracker 5)"
    DIGEST="$(lock_get firecracker 4)"
fi
if [ -n "$SHA256_PIN" ]; then
    [ -z "$DIGEST" ] || [ "$DIGEST" = "$SHA256_PIN" ] || fail "--sha256 disagrees with ${LOCK}"
    DIGEST="$SHA256_PIN"
fi
if [ -n "$DIGEST" ]; then
    printf '%s' "$DIGEST" | grep -Eq '^[0-9a-f]{64}$' || fail "not a SHA-256 digest: ${DIGEST}"
    [ -n "$VERSION" ] || fail "--sha256 needs --version (a digest only means something for one release)"
    OFFLINE=true
fi
if [ -n "$FROM_FILE" ]; then
    [ "$OFFLINE" = true ] || fail "--from-file needs --sha256 or --lock; refusing to install an unverified archive"
    [ "$DRY_RUN" = true ] || [ -f "$FROM_FILE" ] || fail "no such file: ${FROM_FILE}"
fi

if [ -z "$VERSION" ]; then
    [ "$DRY_RUN" != true ] || fail "--dry-run needs --version or --lock (it will not call the GitHub API)"
    info "Resolving latest firecracker release..."
    VERSION="$(api "https://api.github.com/repos/${REPO}/releases/latest" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tag_name"])')"
fi
[ -n "$VERSION" ] || fail "could not resolve a firecracker version"
info "Target version: ${VERSION}"
ASSET="firecracker-${VERSION}-${ARCH}.tgz"

if [ "$DRY_RUN" = true ]; then
    info "dry run: ${ASSET} sha256=${DIGEST:-<from GitHub API>} source=${FROM_FILE:-${DOWNLOAD_URL:-<GitHub release ${VERSION}>}} -> ${INSTALL_DIR}"
    exit 0
fi

if command -v firecracker >/dev/null 2>&1; then
    CURRENT="$(firecracker --version 2>/dev/null | awk '{print $2}')"
    if [ "v${CURRENT}" = "$VERSION" ]; then
        ok "firecracker ${CURRENT} already installed"
        exit 0
    fi
fi

if [ "$OFFLINE" != true ]; then
    RELEASE_JSON="$(api "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"
    DOWNLOAD_URL="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${ASSET}':
        print(a['browser_download_url'])
        break
")"
    DIGEST="$(echo "$RELEASE_JSON" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '${ASSET}':
        print((a.get('digest') or '').removeprefix('sha256:'))
        break
")"
elif [ -z "$DOWNLOAD_URL" ]; then
    # A pinned digest and version name the asset exactly, so the URL needs no API lookup.
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
fi
[ -n "$DOWNLOAD_URL" ] || [ -n "$FROM_FILE" ] || fail "asset ${ASSET} not found in release ${VERSION}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
if [ -n "$FROM_FILE" ]; then
    info "Using ${FROM_FILE}"
    cp "$FROM_FILE" "${TMP}/${ASSET}"
else
    info "Downloading ${ASSET}..."
    curl -fsSL -o "${TMP}/${ASSET}" "$DOWNLOAD_URL"
fi

if [ -n "$DIGEST" ]; then
    GOT="$(${SHA_CMD} "${TMP}/${ASSET}" | awk '{print $1}')"
    [ "$GOT" = "$DIGEST" ] || fail "checksum mismatch for ${ASSET}: expected ${DIGEST}, got ${GOT}"
    ok "checksum verified"
else
    echo "  [WARN] GitHub did not report a digest for ${ASSET}; installing unverified" >&2
fi

tar -xzf "${TMP}/${ASSET}" -C "$TMP"
RELDIR="${TMP}/release-${VERSION}-${ARCH}"
[ -d "$RELDIR" ] || RELDIR="$(find "$TMP" -maxdepth 1 -type d -name 'release-*' | head -1)"
[ -d "$RELDIR" ] || fail "unexpected archive layout"

$SUDO install -m755 "${RELDIR}/firecracker-${VERSION}-${ARCH}" "${INSTALL_DIR}/firecracker"
ok "installed ${INSTALL_DIR}/firecracker"

if [ -f "${RELDIR}/jailer-${VERSION}-${ARCH}" ]; then
    $SUDO install -m755 "${RELDIR}/jailer-${VERSION}-${ARCH}" "${INSTALL_DIR}/jailer"
    ok "installed ${INSTALL_DIR}/jailer"
fi

"${INSTALL_DIR}/firecracker" --version 2>/dev/null || true
