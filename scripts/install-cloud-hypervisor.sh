#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Install Cloud Hypervisor (+ ch-remote and the optional Rust Hypervisor Firmware) from upstream GitHub releases,
# verified against a SHA-256 digest: the one GitHub records for the release asset, or one you pin.
#
# Usage:
#   ./scripts/install-cloud-hypervisor.sh               # latest release (calls the GitHub API)
#   ./scripts/install-cloud-hypervisor.sh v54.0         # pinned version (digests from the GitHub API)
#   ./scripts/install-cloud-hypervisor.sh --no-firmware # skip hypervisor-fw
#   ./scripts/install-cloud-hypervisor.sh --lock packaging/vendor.lock
#                                                       # release builds: versions, URLs and SHA-256 from the lock
#                                                       # for the binary, ch-remote and the firmware; no API call
#   ./scripts/install-cloud-hypervisor.sh --version v54.0 --sha256 HEX --from-file cloud-hypervisor-static
#                                                       # an already downloaded binary, verified (no network for it);
#                                                       # --sha256/--from-file apply to the cloud-hypervisor binary only
#   --dry-run prints what would be installed and exits (no network, no install; any OS; --arch x86_64|aarch64).
#
# Env:
#   CLOUD_HYPERVISOR_VERSION   Pin a version (overridden by --version or a positional arg)
#   INSTALL_DIR                Where to install the binary (default /usr/local/bin)
#   FIRMWARE_DIR               Where to install hypervisor-fw (default /usr/local/share/fluxvm)
set -euo pipefail

REPO="cloud-hypervisor/cloud-hypervisor"
FW_REPO="cloud-hypervisor/rust-hypervisor-firmware"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
FIRMWARE_DIR="${FIRMWARE_DIR:-/usr/local/share/fluxvm}"
VERSION="${CLOUD_HYPERVISOR_VERSION:-}"
WITH_FIRMWARE=true
LOCK=""; FROM_FILE=""; SHA256_PIN=""; DRY_RUN=false; ARCH_OPT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --no-firmware) WITH_FIRMWARE=false; shift ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
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

[ "$DRY_RUN" = true ] || [ "$(uname -s)" = "Linux" ] || fail "Cloud Hypervisor only runs on Linux/KVM"
case "${ARCH_OPT:-$(uname -m)}" in
    x86_64)  ARCH=""; LARCH="x86_64" ;;
    aarch64) ARCH="-aarch64"; LARCH="aarch64" ;;
    *) fail "unsupported architecture: ${ARCH_OPT:-$(uname -m)}" ;;
esac

SHA_CMD=""
if command -v sha256sum >/dev/null; then SHA_CMD="sha256sum"; elif command -v shasum >/dev/null; then SHA_CMD="shasum -a 256"; fi
if [ "$DRY_RUN" != true ]; then
    command -v curl >/dev/null || fail "curl is required"
    [ -n "$SHA_CMD" ] || fail "sha256sum or shasum is required"
fi

SUDO=""
[ "$(id -u)" -ne 0 ] && [ ! -w "$INSTALL_DIR" ] && SUDO="sudo"

api() { curl -fsSL -H "Accept: application/vnd.github+json" "$@"; }

# lock_get <component> <field>: field 3 version, 4 sha256, 5 url, for this architecture.
lock_get() { awk -v c="$1" -v a="$LARCH" -v f="$2" '$1 !~ /^#/ && $1 == c && $2 == a { print $f; exit }' "$LOCK"; }

check_digest() { printf '%s' "$1" | grep -Eq '^[0-9a-f]{64}$' || fail "not a SHA-256 digest: $1"; }

# json_asset <json> <asset> <field>: a field of a named asset in a GitHub release document.
json_asset() {
    echo "$1" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for a in d.get('assets', []):
    if a['name'] == '$2':
        v = a.get('$3') or ''
        print(v.removeprefix('sha256:'))
        break
"
}

# fetch_verified <dest> <url> <digest> <label>: download, then refuse a file whose SHA-256 differs from the digest.
fetch_verified() {
    curl -fsSL -o "$1" "$2"
    if [ -n "$3" ]; then
        GOT="$(${SHA_CMD} "$1" | awk '{print $1}')"
        [ "$GOT" = "$3" ] || fail "checksum mismatch for $4: expected $3, got ${GOT}"
        ok "$4 checksum verified"
    else
        echo "  [WARN] GitHub did not report a digest for $4; installing unverified" >&2
    fi
}

CH_URL=""; CH_DIGEST=""; REMOTE_URL=""; REMOTE_DIGEST=""; FW_URL=""; FW_DIGEST=""; STRICT=false
if [ -n "$LOCK" ]; then
    [ -r "$LOCK" ] || fail "cannot read lock file ${LOCK}"
    STRICT=true
    LV="$(lock_get cloud-hypervisor 3)"
    [ -n "$LV" ] || fail "${LOCK} has no cloud-hypervisor entry for ${LARCH}"
    if [ -n "$VERSION" ] && [ "$VERSION" != "$LV" ] && [ "$VERSION" != "${LV#v}" ]; then
        fail "requested ${VERSION} but ${LOCK} pins ${LV}"
    fi
    VERSION="$LV"
    CH_URL="$(lock_get cloud-hypervisor 5)"; CH_DIGEST="$(lock_get cloud-hypervisor 4)"
    REMOTE_URL="$(lock_get ch-remote 5)"; REMOTE_DIGEST="$(lock_get ch-remote 4)"
    [ -n "$REMOTE_URL" ] || fail "${LOCK} has no ch-remote entry for ${LARCH}"
    if [ "$WITH_FIRMWARE" = true ]; then
        FW_URL="$(lock_get hypervisor-fw 5)"; FW_DIGEST="$(lock_get hypervisor-fw 4)"
        [ -n "$FW_URL" ] || fail "${LOCK} has no hypervisor-fw entry for ${LARCH}"
    fi
fi
if [ -n "$SHA256_PIN" ]; then
    [ -z "$CH_DIGEST" ] || [ "$CH_DIGEST" = "$SHA256_PIN" ] || fail "--sha256 disagrees with ${LOCK}"
    CH_DIGEST="$SHA256_PIN"
fi
if [ -n "$CH_DIGEST" ]; then
    check_digest "$CH_DIGEST"
    [ -n "$VERSION" ] || fail "--sha256 needs --version (a digest only means something for one release)"
fi
if [ -n "$FROM_FILE" ]; then
    [ -n "$CH_DIGEST" ] || fail "--from-file needs --sha256 or --lock; refusing to install an unverified binary"
    [ "$DRY_RUN" = true ] || [ -f "$FROM_FILE" ] || fail "no such file: ${FROM_FILE}"
fi

if [ -z "$VERSION" ]; then
    [ "$DRY_RUN" != true ] || fail "--dry-run needs --version or --lock (it will not call the GitHub API)"
    info "Resolving latest cloud-hypervisor release..."
    VERSION="$(api "https://api.github.com/repos/${REPO}/releases/latest" | python3 -c 'import json,sys;print(json.load(sys.stdin)["tag_name"])')"
fi
[ -n "$VERSION" ] || fail "could not resolve a cloud-hypervisor version"
info "Target version: ${VERSION}"

ASSET="cloud-hypervisor-static${ARCH}"
CH_REMOTE_ASSET="ch-remote-static${ARCH}"

if [ "$DRY_RUN" = true ]; then
    info "dry run: ${ASSET} sha256=${CH_DIGEST:-<from GitHub API>} source=${FROM_FILE:-${CH_URL:-<GitHub release ${VERSION}>}}"
    info "dry run: ${CH_REMOTE_ASSET} sha256=${REMOTE_DIGEST:-<from GitHub API>}"
    [ "$WITH_FIRMWARE" != true ] || info "dry run: hypervisor-fw sha256=${FW_DIGEST:-<latest release, from GitHub API>}"
    info "dry run: -> ${INSTALL_DIR} (firmware: ${FIRMWARE_DIR})"
    exit 0
fi

SKIP_CH_BINARY=false
if command -v cloud-hypervisor >/dev/null 2>&1; then
    CURRENT="$(cloud-hypervisor --version 2>/dev/null | awk '{print $2}')"
    if [ "$CURRENT" = "${VERSION#v}" ] || [ "v${CURRENT}" = "$VERSION" ]; then
        ok "cloud-hypervisor ${CURRENT} already installed"
        SKIP_CH_BINARY=true
    fi
fi

# The release document is only needed when something is not pinned.
RELEASE_JSON=""
if { [ "$SKIP_CH_BINARY" != true ] && [ -z "$CH_DIGEST" ]; } || { [ "$STRICT" != true ] && [ -z "$REMOTE_URL" ]; }; then
    RELEASE_JSON="$(api "https://api.github.com/repos/${REPO}/releases/tags/${VERSION}")"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ "$SKIP_CH_BINARY" != true ]; then
    if [ -z "$CH_DIGEST" ]; then
        CH_URL="$(json_asset "$RELEASE_JSON" "$ASSET" browser_download_url)"
        CH_DIGEST="$(json_asset "$RELEASE_JSON" "$ASSET" digest)"
    elif [ -z "$CH_URL" ]; then
        # A pinned digest and version name the asset exactly, so the URL needs no API lookup.
        CH_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
    fi
    [ -n "$CH_URL" ] || [ -n "$FROM_FILE" ] || fail "asset ${ASSET} not found in release ${VERSION}"

    if [ -n "$FROM_FILE" ]; then
        info "Using ${FROM_FILE}"
        cp "$FROM_FILE" "${TMP}/cloud-hypervisor"
        GOT="$(${SHA_CMD} "${TMP}/cloud-hypervisor" | awk '{print $1}')"
        [ "$GOT" = "$CH_DIGEST" ] || fail "checksum mismatch for ${FROM_FILE}: expected ${CH_DIGEST}, got ${GOT}"
        ok "checksum verified"
    else
        info "Downloading ${ASSET} (${VERSION})..."
        fetch_verified "${TMP}/cloud-hypervisor" "$CH_URL" "$CH_DIGEST" "$ASSET"
    fi

    chmod +x "${TMP}/cloud-hypervisor"
    $SUDO install -m755 "${TMP}/cloud-hypervisor" "${INSTALL_DIR}/cloud-hypervisor"
    ok "installed ${INSTALL_DIR}/cloud-hypervisor ($("${INSTALL_DIR}/cloud-hypervisor" --version 2>/dev/null || echo "$VERSION"))"
fi

# ch-remote is a separate asset in the same release — needed for
# pause/resume/shutdown (fluxvm shells out to it rather than talking to
# the API socket directly).
if command -v ch-remote >/dev/null 2>&1; then
    ok "ch-remote already installed"
else
    if [ -z "$REMOTE_URL" ]; then
        REMOTE_URL="$(json_asset "$RELEASE_JSON" "$CH_REMOTE_ASSET" browser_download_url)"
        REMOTE_DIGEST="$(json_asset "$RELEASE_JSON" "$CH_REMOTE_ASSET" digest)"
    fi
    if [ -n "$REMOTE_URL" ]; then
        fetch_verified "${TMP}/ch-remote" "$REMOTE_URL" "$REMOTE_DIGEST" "$CH_REMOTE_ASSET"
        chmod +x "${TMP}/ch-remote"
        $SUDO install -m755 "${TMP}/ch-remote" "${INSTALL_DIR}/ch-remote"
        ok "installed ${INSTALL_DIR}/ch-remote"
    else
        echo "  [WARN] could not find ${CH_REMOTE_ASSET} asset; pause/resume/shutdown will not work" >&2
    fi
fi

if [ "$WITH_FIRMWARE" = true ]; then
    FW_ASSET="hypervisor-fw"
    [ "$ARCH" = "-aarch64" ] && FW_ASSET="hypervisor-fw-aarch64"
    if [ -z "$FW_URL" ]; then
        info "Resolving latest rust-hypervisor-firmware release..."
        FW_JSON="$(api "https://api.github.com/repos/${FW_REPO}/releases/latest")"
        FW_URL="$(json_asset "$FW_JSON" "$FW_ASSET" browser_download_url)"
        FW_DIGEST="$(json_asset "$FW_JSON" "$FW_ASSET" digest)"
    fi
    if [ -n "$FW_URL" ]; then
        fetch_verified "${TMP}/hypervisor-fw" "$FW_URL" "$FW_DIGEST" "$FW_ASSET"
        $SUDO install -d -m755 "${FIRMWARE_DIR}"
        $SUDO install -m644 "${TMP}/hypervisor-fw" "${FIRMWARE_DIR}/hypervisor-fw"
        ok "installed ${FIRMWARE_DIR}/hypervisor-fw"
        echo "  Set cloud_hypervisor_firmware = \"${FIRMWARE_DIR}/hypervisor-fw\" in your fluxvm config to use firmware boot."
    else
        echo "  [WARN] could not find ${FW_ASSET} asset; skipping firmware install" >&2
    fi
fi
