#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
#
# Install FluxVM from a GitHub release, in one command, with no sudo and no package manager:
#
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash
#   curl -fsSL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --version 0.4.11 --client claude-code
#
# What it does: finds the release (the latest published one unless --version), downloads the package for this
# machine and its .sha256, REFUSES TO CONTINUE if the checksum differs, unpacks it under PREFIX/share/fluxvm/<version>,
# and links the binaries into PREFIX/bin. Nothing outside PREFIX is written, except what you ask for (--service,
# --client). Read it before you pipe it: it is this file, served unchanged from the docs site.
#
#   --version X|latest   install this release (default: latest published)
#   --prefix DIR         install under DIR (default: ~/.local)
#   --client NAME        also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...) with write tools
#   --service            macOS: run `fluxctl service install` (a LaunchAgent). Linux: set up systemd or OpenRC (run as root;
#                        otherwise the exact `sudo` commands are printed, not run)
#   --agent              Linux, with --service: also run the fleet node agent (`fluxvm-agent node`); needs the four options below
#   --central URL        the central registry (`fluxvm-agent central`), for --agent
#   --node-name NAME     this node's name in the fleet, for --agent
#   --advertise-url URL  how central reaches this node's daemon (not a loopback address), for --agent
#   --token-file FILE    the fleet or join token, copied to /etc/fluxvm/agent.token (mode 0600), for --agent
#   --detect             print what was found (distro family, package manager, init system, libc, missing prerequisites) and stop
#   --no-deps            do not try to install the runtime library the package needs (macOS: hivex via Homebrew)
#   --brew               macOS only: use Homebrew (`brew install zyvorai/tap/fluxvm`) instead of downloading
#   --uninstall          remove what this script installed (VM images and state are left alone)
#   --dry-run            print what would run and stop
#   -h, --help
#
# A cluster in two commands, like k3s but with FluxVM's own fleet (no Kubernetes needed; docs/cluster-quickstart.md):
#
#   curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --server
#   curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | FLUXVM_TOKEN=fvjt_... bash -s -- --join http://HOST:7799
#
#   --server             (alias --central, when no URL follows) install FluxVM, run the fleet central (`fluxvm-agent central`)
#                        as a service next to the daemon, make a short-lived join token, print ONE join command and the
#                        dashboard URL. The token is also written to ~/.config/fluxvm/join.token (mode 0600)
#   --join URL           install FluxVM, enroll this machine with the central at URL, run the node agent as a service and
#                        wait until central accepts it. The token comes from --token-file FILE, $FLUXVM_TOKEN or
#                        --token-stdin, never from an argument (it would show in `ps` and in shell history)
#   --label K=V          a node label (repeatable); --node-name NAME; --advertise-url URL
#   --port N             --server: central's port (default 7799)    --public-url URL: how nodes reach this server
#   --token-ttl SECS     --server: lifetime of the join token (default 3600)
#   --with-k3s           optional add-on, never a dependency: also set up the k3s layer on this node (`fluxctl kube up`)
#   --yes                do not ask before installing prerequisites (needed when there is no terminal)
#   --no-deps            do not install prerequisites (qemu, qemu-utils, cloud-image-utils, kvm group)
#   --no-image           do not fetch the Debian 13 base image     --no-selftest   skip the boot-a-VM self-test
#   --print-deps [--from-source]   print the packages this host needs and stop
#   --engine-image-url URL   a pre-baked Docker engine image (digest-verified); without one Docker VMs install Docker on first boot
#   --mirror URL --pubkey FILE   install through the signed channel (`fluxvm-bootstrap`, Ed25519) instead of the GitHub release
#   --skip-download      use the FluxVM that is already installed (what `fluxctl cluster init|join` does on Linux)
#
# Token print policy: the join command (with the token in it) is printed once, to the terminal, when stdout is a terminal;
# when stdout is a pipe or a log the command refers to the token file instead and the token is not printed. The token
# is never in the argument list of a process: the node reads it from a file, the environment or stdin.
#
# Environment: FLUXVM_RELEASE_BASE overrides https://github.com/zyvorai/zyvor-fluxvm/releases (a mirror, or file://
# for tests). Packages exist for macOS on Apple silicon (arm64); Linux x86_64 and aarch64 where a release has them.
# FLUXVM_OS_RELEASE, FLUXVM_INIT (systemd|openrc|none) and FLUXVM_GLIBC_VERSION override what is detected on Linux (tests,
# containers). Linux families: Debian/Ubuntu, Fedora/RHEL, Arch, openSUSE/SLES (musl systems such as Alpine cannot run the
# glibc package). Works with macOS's bash 3.2.
set -euo pipefail

BASE="${FLUXVM_RELEASE_BASE:-https://github.com/zyvorai/zyvor-fluxvm/releases}"
BASE="${BASE%/}"
PREFIX="${HOME:-/tmp}/.local"
VERSION="latest"
CLIENT=""
SERVICE=0
AGENT=0
CENTRAL=""
NODE_NAME=""
ADVERTISE=""
TOKEN_FILE=""
DETECT=0
DEPS=1
BREW=0
UNINSTALL=0
DRY=0
FORMULA="zyvorai/tap/fluxvm"
URL="http://127.0.0.1:7788"
SERVER=0
JOIN_URL=""
CLUSTER=0
TOKEN_STDIN=0
YES=0
PORT=7799
PUBLIC_URL=""
TOKEN_TTL="${FLUXVM_TOKEN_TTL:-3600}"
LABELS=""
WITH_K3S=0
IMAGE=1
SELFTEST=1
PRINT_DEPS=0
FROM_SOURCE=0
ENGINE_IMAGE_URL="${FLUXVM_ENGINE_IMAGE_URL:-}"
MIRROR=""
PUBKEY=""
SKIP_DOWNLOAD=0

usage() {
  cat <<'EOUSAGE'
Install FluxVM from a GitHub release (no sudo, no package manager).
  --version X|latest   install this release (default: latest published)
  --prefix DIR         install under DIR (default: ~/.local)
  --client NAME        also wire an MCP client (claude-code, codex, gemini, cursor, vscode, ...)
  --service            macOS: install the LaunchAgent; Linux: set up systemd or OpenRC (as root; else print the sudo steps)
  --agent              Linux, with --service: also run the fleet node agent (needs --central, --node-name,
                       --advertise-url and --token-file)
  --detect             print the distro family, package manager, init system, libc and missing prerequisites, and stop
  --server             a cluster server: install, run fleet central as a service, print the join command (alias: --central)
  --join URL           join the cluster whose central is at URL (token: --token-file, $FLUXVM_TOKEN or --token-stdin)
  --label K=V          node label (repeatable); also --node-name, --advertise-url, --port, --public-url, --token-ttl
  --with-k3s           optional add-on: set up the k3s layer too (never required)
  --yes                do not ask before installing prerequisites; --no-deps skips them
  --no-image           do not fetch the Debian 13 base image; --no-selftest skips the boot-a-VM test
  --print-deps         print the packages this host needs (--from-source adds the build libraries) and stop
  --no-deps            do not install the runtime library the package needs (macOS: hivex via Homebrew)
  --brew               macOS only: use Homebrew instead of downloading
  --uninstall          remove what this script installed (VM images are kept)
  --dry-run            print what would run and stop
  -h, --help
EOUSAGE
}

die() { printf '%s\n' "$1" >&2; exit "${2:-1}"; }
say() { printf '==> %s\n' "$*"; }
run() {
  if [ "$DRY" = 1 ]; then printf '    would run: %s\n' "$*"; return 0; fi
  printf '    $ %s\n' "$*"
  "$@"
}


OS="${FLUXVM_FORCE_OS:-$(uname -s)}"; ARCH="${FLUXVM_FORCE_ARCH:-$(uname -m)}"
# Where system files go. Empty on a real host; the tests point it at a temp directory (FLUXVM_ROOT).
ROOTFS="${FLUXVM_ROOT:-}"

# ---------------------------------------------------------------------------------------------- Linux flavours
OS_RELEASE="${FLUXVM_OS_RELEASE:-/etc/os-release}"
GLIBC_FLOOR="2.35"

os_release_field() { # KEY: its value in os-release, without quotes
  [ -r "$OS_RELEASE" ] || return 0
  sed -n "s/^$1=//p" "$OS_RELEASE" | head -n1 | tr -d "\"'"
}

linux_family() { # debian | rhel | arch | suse | alpine | unknown, from ID and ID_LIKE
  local w
  for w in $(os_release_field ID) $(os_release_field ID_LIKE); do
    case "$w" in
      debian|ubuntu|linuxmint|pop|raspbian|kali|devuan) echo debian; return 0 ;;
      fedora|rhel|centos|rocky|almalinux|ol|amzn) echo rhel; return 0 ;;
      arch|archarm|manjaro|endeavouros|cachyos|artix) echo arch; return 0 ;;
      suse|sles|sled|opensuse*) echo suse; return 0 ;;
      alpine|postmarketos) echo alpine; return 0 ;;
    esac
  done
  echo unknown
}

linux_pkg_manager() { # the package manager this family uses and this host has, or none
  case "$1" in
    debian) command -v apt-get >/dev/null 2>&1 && { echo apt-get; return 0; } ;;
    rhel) if command -v dnf >/dev/null 2>&1; then echo dnf; return 0; fi
          if command -v yum >/dev/null 2>&1; then echo yum; return 0; fi ;;
    arch) command -v pacman >/dev/null 2>&1 && { echo pacman; return 0; } ;;
    suse) command -v zypper >/dev/null 2>&1 && { echo zypper; return 0; } ;;
    alpine) command -v apk >/dev/null 2>&1 && { echo apk; return 0; } ;;
  esac
  echo none
}

linux_dep_command() { # FAMILY PKGMGR: the command that installs the runtime libraries (empty when there is none)
  case "$1" in
    debian) echo "$2 install -y libhivex0 libsystemd0" ;;
    rhel) echo "$2 install -y hivex systemd-libs" ;;
    arch) echo "$2 -S --needed --noconfirm hivex systemd-libs" ;;
    suse) echo "$2 --non-interactive install libhivex0 libsystemd0" ;;
  esac
}

linux_init() { # systemd | openrc | none
  case "${FLUXVM_INIT:-}" in systemd|openrc|none) echo "$FLUXVM_INIT"; return 0 ;; esac
  if [ -d /run/systemd/system ]; then echo systemd
  elif command -v rc-service >/dev/null 2>&1; then echo openrc
  else echo none; fi
}

linux_libc() { # glibc | musl | unknown
  if [ -n "${FLUXVM_GLIBC_VERSION:-}" ]; then echo glibc; return 0; fi
  local first
  first="$(ldd --version 2>&1 | head -n1 || true)"
  case "$first" in
    *[Mm]usl*) echo musl ;;
    *GLIBC*|*glibc*|*"GNU libc"*) echo glibc ;;
    *) echo unknown ;;
  esac
}

glibc_version() { # 2.35, or empty when unknown
  if [ -n "${FLUXVM_GLIBC_VERSION:-}" ]; then echo "$FLUXVM_GLIBC_VERSION"; return 0; fi
  ldd --version 2>&1 | head -n1 | grep -Eo '[0-9]+\.[0-9]+' | tail -n1 || true
}

version_lt() { # A B: is the dotted version A lower than B (major.minor)
  local a1="${1%%.*}" a2="${1#*.}" b1="${2%%.*}" b2="${2#*.}"
  [ "$a1" -lt "$b1" ] 2>/dev/null && return 0
  [ "$a1" -eq "$b1" ] 2>/dev/null && [ "${a2%%.*}" -lt "${b2%%.*}" ] 2>/dev/null
}

print_detect() {
  local fam
  if [ "$OS" != Linux ]; then printf 'os=%s\n' "$OS"; return 0; fi
  fam="$(linux_family)"
  printf 'os=linux\nid=%s\nfamily=%s\npackage_manager=%s\ninit=%s\nlibc=%s\nglibc=%s\n' \
    "$(os_release_field ID)" "$fam" "$(linux_pkg_manager "$fam")" "$(linux_init)" "$(linux_libc)" "$(glibc_version)"
  printf 'missing=%s\npackages=%s\n' "$(prereq_missing_binaries)" "$(prereq_packages "$fam" "$(prereq_missing_binaries)")"
}

IS_ROOT=0
if [ "$(id -u)" = 0 ]; then IS_ROOT=1; fi
# svc COMMAND...: a step that needs root: run when this is root (or a dry run prints it). Otherwise, when sudo was set up
# for a cluster install (SUDO is set), run it with sudo; else print it with sudo and leave it to the person.
SUDO=""
svc() {
  if [ "$IS_ROOT" = 1 ]; then run "$@"
  elif [ -n "$SUDO" ]; then
    # shellcheck disable=SC2086  # SUDO is "sudo" or "sudo -n"
    run $SUDO "$@"
  else printf '    sudo %s\n' "$*"; fi
}
# svc_cat FILE: print a root-only file (empty when it is missing).
svc_cat() {
  if [ "$IS_ROOT" = 1 ]; then cat "$1" 2>/dev/null || true
  elif [ -n "$SUDO" ]; then
    # shellcheck disable=SC2086
    $SUDO cat "$1" 2>/dev/null || true
  fi
}

# ---------------------------------------------------------------------------------------------- prerequisites
have_cmd() { # NAME: is the program on PATH (tests: FLUXVM_FORCE_MISSING="a b" pretends they are not, until they appear in FLUXVM_FORCE_MISSING_DIR)
  case " ${FLUXVM_FORCE_MISSING:-} " in
    *" $1 "*) [ -x "${FLUXVM_FORCE_MISSING_DIR:-/nonexistent}/$1" ]; return ;;
  esac
  command -v "$1" >/dev/null 2>&1
}

host_arch() { case "$ARCH" in aarch64|arm64) echo arm64 ;; *) echo x86_64 ;; esac; }

qemu_system_binary() { if [ "$(host_arch)" = arm64 ]; then echo qemu-system-aarch64; else echo qemu-system-x86_64; fi; }

prereq_missing_binaries() { # the programs VM creation needs that are not on PATH
  local b out=""
  for b in "$(qemu_system_binary)" qemu-img cloud-localds; do have_cmd "$b" || out="$out $b"; done
  echo "${out# }"
}

prereq_packages() { # FAMILY MISSING-BINARIES: the packages that provide them (additive installs only)
  local fam="$1" miss="$2" b out=""
  for b in $miss; do
    case "$fam/$b" in
      debian/qemu-system-x86_64) out="$out qemu-system-x86" ;;
      debian/qemu-system-aarch64) out="$out qemu-system-arm qemu-efi-aarch64" ;;
      debian/qemu-img) out="$out qemu-utils" ;;
      debian/cloud-localds) out="$out cloud-image-utils" ;;
      rhel/qemu-system-*) out="$out qemu-kvm" ;;
      rhel/qemu-img) out="$out qemu-img" ;;
      rhel/cloud-localds) out="$out cloud-utils" ;;
      arch/qemu-system-x86_64) out="$out qemu-system-x86" ;;
      arch/qemu-system-aarch64) out="$out qemu-system-aarch64" ;;
      arch/qemu-img) out="$out qemu-img" ;;
      suse/qemu-system-x86_64) out="$out qemu-x86" ;;
      suse/qemu-system-aarch64) out="$out qemu-arm" ;;
      suse/qemu-img) out="$out qemu-tools" ;;
    esac
  done
  # each name once, in the order given
  echo "$out" | tr ' ' '\n' | awk 'NF && !seen[$0]++ { printf "%s%s", sep, $0; sep = " " } END { print "" }'
}

source_packages() { # FAMILY: the development libraries a source build needs (the installer does not install these)
  case "$1" in
    debian) echo "libsystemd-dev libhivex-dev pkg-config" ;;
    rhel) echo "systemd-devel hivex-devel pkgconf-pkg-config" ;;
    arch) echo "systemd-libs hivex pkgconf" ;;
    suse) echo "systemd-devel libhivex-devel pkg-config" ;;
  esac
}

prereq_note() { # FAMILY MISSING: what the package manager cannot be trusted to supply
  case "$1" in
    arch) case " $2 " in *" cloud-localds "*) echo "cloud-localds is not in Arch's repositories: install cloud-image-utils from the AUR." ;; esac ;;
    suse) case " $2 " in *" cloud-localds "*) echo "cloud-localds: install cloud-utils from your repositories or the cloud-init sources." ;; esac ;;
    rhel) case " $2 " in *" cloud-localds "*) echo "cloud-localds comes from cloud-utils (EPEL on RHEL/Rocky/Alma)." ;; esac ;;
  esac
}

pkg_install_words() { # FAMILY PKGMGR PKGS...: the install command (non-interactive)
  local fam="$1" pm="$2"; shift 2
  case "$fam" in
    debian) echo "env DEBIAN_FRONTEND=noninteractive $pm install -y --no-install-recommends $*" ;;
    rhel) echo "$pm install -y $*" ;;
    arch) echo "$pm -S --needed --noconfirm $*" ;;
    suse) echo "$pm --non-interactive install --no-recommends $*" ;;
  esac
}

pkg_simulate_words() { # FAMILY PKGMGR PKGS...: the same, changing nothing
  local fam="$1" pm="$2"; shift 2
  case "$fam" in
    debian) echo "$pm install -s -y --no-install-recommends $*" ;;
    rhel) echo "$pm install --assumeno $*" ;;
    arch) echo "$pm -S --needed --print $*" ;;
    suse) echo "$pm --non-interactive install --dry-run --no-recommends $*" ;;
  esac
}

sim_is_additive() { # FAMILY: reads the simulation on stdin; fails when it would remove or upgrade something
  local fam="$1" out; out="$(cat)"
  case "$fam" in
    debian) ! printf '%s\n' "$out" | grep -Eq '^(Remv |Inst [^ ]+ \[)' ;;
    rhel) ! printf '%s\n' "$out" | grep -Eq '^(Upgrading|Removing|Downgrading):' ;;
    suse) ! printf '%s\n' "$out" | grep -Eqi 'going to be (REMOVED|upgraded|downgraded)' ;;
    *) return 0 ;;
  esac
}

primary_ip() { # the address other machines reach this one on (FLUXVM_HOST_IP overrides)
  if [ -n "${FLUXVM_HOST_IP:-}" ]; then echo "$FLUXVM_HOST_IP"; return 0; fi
  local ip=""
  ip="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -n1 || true)"
  [ -n "$ip" ] || ip="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
  [ -n "$ip" ] || ip="$(ipconfig getifaddr en0 2>/dev/null || true)"
  echo "${ip:-127.0.0.1}"
}

default_node_name() {
  local n; n="$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo node)"
  n="$(printf '%s' "$n" | tr 'A-Z_' 'a-z-' | tr -cd 'a-z0-9.-' | sed 's/^[-.]*//' | cut -c1-63)"
  echo "${n:-node}"
}

valid_label() { # KEY=VALUE with nothing a shell, a unit file or a plist would treat specially
  case "$1" in
    =*|*[!A-Za-z0-9._/=:-]*) return 1 ;;
    *=*) return 0 ;;
  esac
  return 1
}

confirm() { # QUESTION: yes with --yes; otherwise ask on the terminal; with neither, say how to proceed
  [ "$YES" = 1 ] && return 0
  local a=""
  if { true </dev/tty; } 2>/dev/null; then
    printf '%s [Y/n] ' "$1" >&2
    read -r a </dev/tty || a=""
    case "$a" in ""|y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
  fi
  die "$1
There is no terminal to ask on: run again with --yes to accept, or --no-deps to skip prerequisites." 64
}

setup_sudo() { # one sudo prompt (none when passwordless); the later steps reuse it
  [ "$IS_ROOT" = 1 ] && return 0
  [ -z "$SUDO" ] || return 0
  if [ "$DRY" = 1 ]; then SUDO="sudo"; return 0; fi
  have_cmd sudo || die "This step needs root and sudo is not installed. Run this script as root (su -c ...), or install sudo." 1
  if sudo -n true 2>/dev/null; then SUDO="sudo -n"; return 0; fi
  say "sudo will ask for your password once"
  if sudo -v; then SUDO="sudo"; return 0; fi
  die "sudo did not authorise. Run as root, or as a user with passwordless sudo, or run the printed steps yourself." 1
}

NEWLOGIN=0
ensure_prereqs() { # Linux: install what VM creation needs, after printing exactly what
  [ "$OS" = Linux ] || return 0
  if [ "$DEPS" != 1 ]; then say "--no-deps: not installing prerequisites"; return 0; fi
  local fam pm miss pkgs words sim note
  fam="$(linux_family)"; pm="$(linux_pkg_manager "$fam")"
  miss="$(prereq_missing_binaries)"
  if [ -n "$miss" ]; then
    pkgs="$(prereq_packages "$fam" "$miss")"
    note="$(prereq_note "$fam" "$miss")"
    say "Missing programs: $miss"
    if [ "$pm" = none ] || [ -z "$pkgs" ]; then
      die "No supported package manager found for '$fam'. Install by hand: qemu ($(qemu_system_binary), qemu-img) and cloud-localds (cloud-image-utils), then run this again with --no-deps. $note" 1
    fi
    say "Will install (additive only, nothing is removed or upgraded): $pkgs"
    if [ -n "$note" ]; then printf '    note: %s\n' "$note"; fi
    if [ "$DRY" = 1 ]; then
      # shellcheck disable=SC2086
      printf '    would run: %s\n' "$(pkg_install_words "$fam" "$pm" $pkgs)"
    else
      confirm "Install $pkgs with $pm (uses sudo)?" || die "Not installing prerequisites. Install them yourself, then run again with --no-deps." 1
      setup_sudo
      # shellcheck disable=SC2086
      words="$(pkg_simulate_words "$fam" "$pm" $pkgs)"
      # shellcheck disable=SC2086
      if [ "$IS_ROOT" = 1 ]; then sim="$($words 2>&1 || true)"; else sim="$($SUDO $words 2>&1 || true)"; fi
      if ! printf '%s\n' "$sim" | sim_is_additive "$fam"; then
        die "Installing $pkgs would remove or upgrade other packages on this machine; not doing that. Review with: $words" 1
      fi
      # shellcheck disable=SC2086
      words="$(pkg_install_words "$fam" "$pm" $pkgs)"
      # shellcheck disable=SC2086
      svc $words || die "Installing $pkgs failed. Manual command: sudo $words" 1
      miss="$(prereq_missing_binaries)"
      if [ -n "$miss" ]; then die "Still missing after the install: $miss. $note" 1; fi
    fi
  else
    say "Prerequisites: qemu, qemu-img and cloud-localds are present"
  fi
  ensure_kvm_group
}

ensure_kvm_group() {
  [ "$IS_ROOT" = 1 ] && return 0
  if [ ! -e "$ROOTFS/dev/kvm" ] && [ -z "${FLUXVM_FORCE_KVM_GROUP:-}" ]; then
    printf 'warning: /dev/kvm does not exist: no hardware virtualisation (or it is off in the firmware). VMs will not start here.\n' >&2
    return 0
  fi
  local me="${USER:-$(id -un)}"
  if id -nG 2>/dev/null | tr ' ' '\n' | grep -qx kvm; then return 0; fi
  say "Adding $me to the kvm group (so VMs run without root)"
  setup_sudo
  svc usermod -aG kvm "$me" || printf 'warning: could not add %s to kvm: run: sudo usermod -aG kvm %s\n' "$me" "$me" >&2
  NEWLOGIN=1
}

# ---------------------------------------------------------------------------------------------- images
IMAGE_PATH=""
image_dir() {
  if [ "$IS_ROOT" = 1 ]; then echo "$ROOTFS/var/lib/fluxvm/images"; else echo "${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/fluxvm/images"; fi
}

sha_of() { # ALGO FILE (256 or 512)
  if command -v "sha${1}sum" >/dev/null 2>&1; then "sha${1}sum" "$2" | cut -d' ' -f1
  else shasum -a "$1" "$2" | cut -d' ' -f1; fi
}

fetch_verified() { # URL DEST SUMS-URL ALGO NAME-IN-SUMS: resumable download checked against the vendor's sums file
  local url="$1" dest="$2" sums="$3" algo="$4" name="$5" want got sumsfile
  sumsfile="$(mktemp "${TMPDIR:-/tmp}/fluxvm-sums.XXXXXX")"
  if ! curl -fsSL -o "$sumsfile" "$sums"; then rm -f "${sumsfile:?}"; printf 'warning: could not fetch %s\n' "$sums" >&2; return 1; fi
  want="$(awk -v f="$name" '$2==f || $2=="*"f {print $1; exit}' "$sumsfile")"
  rm -f "${sumsfile:?}"
  if [ -z "$want" ]; then printf 'warning: %s is not listed in %s\n' "$name" "$sums" >&2; return 1; fi
  if [ -f "$dest" ] && [ "$(sha_of "$algo" "$dest")" = "$want" ]; then say "Cached and verified: $dest"; return 0; fi
  say "Downloading $name (resumable; run again to resume if it is interrupted)"
  if ! curl -fL -C - --retry 5 --retry-delay 2 --retry-all-errors "$PROGRESS" -o "$dest.part" "$url"; then
    printf 'warning: download failed (%s); run again to resume\n' "$url" >&2; return 1
  fi
  got="$(sha_of "$algo" "$dest.part")"
  if [ "$got" != "$want" ]; then
    rm -f "${dest:?}.part"
    printf 'error: checksum mismatch for %s (expected %s, got %s); the file was removed\n' "$name" "$want" "$got" >&2
    return 1
  fi
  mv "$dest.part" "$dest"
  say "Verified sha$algo: $dest"
}

ensure_base_image() { # the Debian 13 cloud image for this machine's architecture
  local base="${FLUXVM_IMAGE_BASE:-https://cloud.debian.org/images/cloud/trixie/latest}" arch name dir
  base="${base%/}"
  if [ "$(host_arch)" = arm64 ]; then arch=arm64; else arch=amd64; fi
  name="debian-13-genericcloud-$arch.qcow2"
  dir="$(image_dir)"
  if [ "$DRY" = 1 ]; then printf '    would fetch %s/%s and verify it against %s/SHA512SUMS into %s\n' "$base" "$name" "$base" "$dir"; return 0; fi
  mkdir -p "$dir"
  if fetch_verified "$base/$name" "$dir/$name" "$base/SHA512SUMS" 512 "$name"; then IMAGE_PATH="$dir/$name"
  else printf 'warning: no base image: pass one to fluxctl run IMAGE, or run this again with a network.\n' >&2; fi
}

ensure_engine_image() { # a pre-baked Docker engine image, when a URL is configured; digest from URL.sha256, optional Ed25519 .sig
  local dir name
  if [ -z "$ENGINE_IMAGE_URL" ]; then
    say "Engine image: none configured (--engine-image-url); the first boot of a Docker VM installs Docker with apt (slower, needs internet in the guest). scripts/build-engine-image.sh builds one."
    return 0
  fi
  name="${ENGINE_IMAGE_URL##*/}"
  dir="$(image_dir)"
  if [ "$DRY" = 1 ]; then printf '    would fetch %s and verify %s.sha256\n' "$ENGINE_IMAGE_URL" "$ENGINE_IMAGE_URL"; return 0; fi
  mkdir -p "$dir"
  if ! fetch_verified "$ENGINE_IMAGE_URL" "$dir/$name" "$ENGINE_IMAGE_URL.sha256" 256 "$name"; then
    printf 'warning: engine image not usable; Docker VMs will install Docker on first boot.\n' >&2
    return 0
  fi
  if [ -n "${FLUXVM_RELEASE_PUBKEY:-}" ] && have_cmd openssl; then
    if curl -fsSL -o "$dir/$name.sig" "$ENGINE_IMAGE_URL.sig" \
       && openssl pkeyutl -verify -pubin -inkey "$FLUXVM_RELEASE_PUBKEY" -rawin -in "$dir/$name" -sigfile "$dir/$name.sig" >/dev/null 2>&1; then
      say "Engine image signature verified"
    else
      rm -f "${dir:?}/$name"
      printf 'warning: the engine image signature did not verify; removed it. Docker VMs will install Docker on first boot.\n' >&2
      return 0
    fi
  fi
  say "Engine image ready: $dir/$name"
}

# ---------------------------------------------------------------------------------------------- self-test
selftest_hint() { # OUTPUT: the fix most likely to matter
  case "$1" in
    *"waiting for the guest's address"*) echo "fluxctl run cannot reach a guest on Linux's user-mode network yet (no port forwards); the fix is in the kube-unified work. The cluster itself is fine; re-run: fluxctl cluster selftest after upgrading." ;;
    *kvm*ermission*|*ermission*kvm*) echo "Your user cannot open /dev/kvm: sudo usermod -aG kvm \$USER, then log out and in (or: newgrp kvm)." ;;
    */dev/kvm*|*KVM*) echo "No usable /dev/kvm: enable virtualisation in the firmware; on a cloud VM use an instance type with nested virtualisation." ;;
    *cloud-localds*) echo "Install cloud-image-utils (cloud-localds): sudo apt-get install cloud-image-utils (Fedora: cloud-utils)." ;;
    *qemu*"not found"*|*qemu*"No such file"*) echo "Install qemu: sudo apt-get install qemu-system-x86 qemu-utils (Fedora: qemu-kvm qemu-img)." ;;
    */run/fluxvm*|*"Permission denied"*) echo "A system path is root-owned: use the per-user config the installer wrote (export FLUXVM_CONFIG=\$HOME/.config/fluxvm/config.toml)." ;;
    *"No such file"*|*"not found"*) echo "The base image is missing: run the installer again (it downloads and verifies Debian 13)." ;;
    *) echo "Run: fluxctl doctor (every failure prints its fix), then: fluxctl cluster selftest" ;;
  esac
}

CLI_CONFIG="${FLUXVM_CONFIG:-}"
write_user_config() { # a per-user config, because /run/fluxvm and /var/lib/fluxvm are root-owned
  [ "$IS_ROOT" = 1 ] && return 0
  CLI_CONFIG="${XDG_CONFIG_HOME:-${HOME:-/tmp}/.config}/fluxvm/config.toml"
  if [ -e "$CLI_CONFIG" ]; then return 0; fi
  if [ "$DRY" = 1 ]; then printf '    would write %s (per-user run_dir and state_dir)\n' "$CLI_CONFIG"; return 0; fi
  mkdir -p "$(dirname "$CLI_CONFIG")"
  {
    printf 'listen = "127.0.0.1:7788"\n'
    printf 'state_dir = "%s"\n' "${XDG_DATA_HOME:-${HOME:-/tmp}/.local/share}/fluxvm"
    printf 'run_dir = "%s"\n' "${XDG_RUNTIME_DIR:-/tmp}/fluxvm-run-$(id -u)"
    printf 'qemu_binary = "%s"\n' "$(qemu_system_binary)"
  } > "$CLI_CONFIG"
  say "Wrote $CLI_CONFIG (as a normal user: export FLUXVM_CONFIG=$CLI_CONFIG)"
}

# shellcheck disable=SC2016  # the arithmetic is for the guest's shell, not this one
SELFTEST_CMD='echo fluxvm-selftest-$((6*7))'
SELFTEST_FAILED=0
JOIN_FAILED=0
run_selftest() { # boot a tiny VM, run a command in it, delete it; print PASS or FAIL with the fix
  [ "$SELFTEST" = 1 ] || return 0
  [ "$OS" = Linux ] || return 0
  local out="" rc=0 pre=""
  if [ "$DRY" = 1 ]; then printf '    would boot a tiny VM from the base image, run a command in it and delete it (self-test)\n'; return 0; fi
  if [ -z "$IMAGE_PATH" ]; then echo "SELF-TEST SKIPPED: no base image (see the warning above)"; return 0; fi
  if [ "$NEWLOGIN" = 1 ]; then
    echo "SELF-TEST SKIPPED: you were just added to the kvm group; log out and in (or run: newgrp kvm), then: fluxctl cluster selftest"
    return 0
  fi
  if have_cmd timeout; then pre="timeout ${FLUXVM_SELFTEST_TIMEOUT:-300}"; fi
  say "Self-test: booting a tiny VM, running a command in it, deleting it"
  if [ -n "$CLI_CONFIG" ]; then
    # shellcheck disable=SC2086
    out="$($pre "$FLUXCTL" --config "$CLI_CONFIG" run "$IMAGE_PATH" --cpus 1 --memory-mib 512 -- sh -c "$SELFTEST_CMD" 2>&1)" || rc=$?
  else
    # shellcheck disable=SC2086
    out="$($pre "$FLUXCTL" run "$IMAGE_PATH" --cpus 1 --memory-mib 512 -- sh -c "$SELFTEST_CMD" 2>&1)" || rc=$?
  fi
  case "$out" in
    *fluxvm-selftest-42*) echo "SELF-TEST PASS: a VM booted, ran a command and was deleted." ;;
    *) printf 'SELF-TEST FAIL (exit %s)\n%s\nFix: %s\n' "$rc" "$(printf '%s\n' "$out" | tail -n 8)" "$(selftest_hint "$out")"
       SELFTEST_FAILED=1 ;;
  esac
}

TMP=""; STAGE=""; SECRETS=""
cleanup() {
  if [ -n "$TMP" ]; then rm -rf "${TMP:?}"; fi
  if [ -n "$SECRETS" ]; then rm -rf "${SECRETS:?}"; fi
  if [ -n "$STAGE" ]; then rm -rf "${STAGE:?}"; fi
}
secrets_dir() { # makes SECRETS, a private (0700) directory for token files; removed on exit (call it, do not capture it)
  if [ -z "$SECRETS" ]; then SECRETS="$(umask 077; mktemp -d "${TMPDIR:-/tmp}/fluxvm-secrets.XXXXXX")"; fi
}

if [ "${FLUXVM_INSTALL_SOURCE_ONLY:-}" = 1 ]; then return 0 2>/dev/null || exit 0; fi
trap cleanup EXIT

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; [ -n "$VERSION" ] || die "--version needs a value" 64; shift 2 ;;
    --prefix) PREFIX="${2:-}"; [ -n "$PREFIX" ] || die "--prefix needs a directory" 64; shift 2 ;;
    --client) CLIENT="${2:-}"; [ -n "$CLIENT" ] || die "--client needs a name" 64; shift 2 ;;
    --service) SERVICE=1; shift ;;
    --agent) AGENT=1; shift ;;
    --central)
      # `--central URL` names the registry for --agent; a bare `--central` is the alias of --server.
      case "${2:-}" in
        ""|-*) SERVER=1; shift ;;
        *) CENTRAL="$2"; shift 2 ;;
      esac ;;
    --server) SERVER=1; shift ;;
    --join) JOIN_URL="${2:-}"; [ -n "$JOIN_URL" ] || die "--join needs the central's URL (http://HOST:7799)" 64; shift 2 ;;
    --token-stdin) TOKEN_STDIN=1; shift ;;
    --token) die "There is no --token option on purpose: an argument shows in \`ps\` and in shell history. Use --token-file FILE, the FLUXVM_TOKEN environment variable or --token-stdin." 64 ;;
    --label)
      valid_label "${2:-}" || die "--label needs key=value (letters, digits and . _ - / : only)" 64
      LABELS="$LABELS$2
"; shift 2 ;;
    --port) PORT="${2:-}"; case "$PORT" in ""|*[!0-9]*) die "--port needs a number" 64 ;; esac; shift 2 ;;
    --public-url) PUBLIC_URL="${2:-}"; [ -n "$PUBLIC_URL" ] || die "--public-url needs a URL" 64; shift 2 ;;
    --token-ttl) TOKEN_TTL="${2:-}"; case "$TOKEN_TTL" in ""|*[!0-9]*) die "--token-ttl needs seconds" 64 ;; esac; shift 2 ;;
    --with-k3s) WITH_K3S=1; shift ;;
    --yes|-y) YES=1; shift ;;
    --no-image) IMAGE=0; shift ;;
    --no-selftest) SELFTEST=0; shift ;;
    --print-deps) PRINT_DEPS=1; shift ;;
    --from-source) FROM_SOURCE=1; shift ;;
    --engine-image-url) ENGINE_IMAGE_URL="${2:-}"; [ -n "$ENGINE_IMAGE_URL" ] || die "--engine-image-url needs a URL" 64; shift 2 ;;
    --mirror) MIRROR="${2:-}"; [ -n "$MIRROR" ] || die "--mirror needs a URL" 64; shift 2 ;;
    --pubkey) PUBKEY="${2:-}"; [ -n "$PUBKEY" ] || die "--pubkey needs a file" 64; shift 2 ;;
    --skip-download) SKIP_DOWNLOAD=1; shift ;;
    --node-name) NODE_NAME="${2:-}"; [ -n "$NODE_NAME" ] || die "--node-name needs a name" 64; shift 2 ;;
    --advertise-url) ADVERTISE="${2:-}"; [ -n "$ADVERTISE" ] || die "--advertise-url needs a URL" 64; shift 2 ;;
    --token-file) TOKEN_FILE="${2:-}"; [ -n "$TOKEN_FILE" ] || die "--token-file needs a path" 64; shift 2 ;;
    --detect) DETECT=1; shift ;;
    --no-service) SERVICE=0; shift ;;
    --no-deps) DEPS=0; shift ;;
    --brew) BREW=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $1 (try --help)" 64 ;;
  esac
done
VERSION="${VERSION#v}"

if [ "$DETECT" = 1 ]; then print_detect; exit 0; fi
if [ "$PRINT_DEPS" = 1 ]; then
  if [ "$OS" != Linux ]; then echo "macOS needs no packages from this script."; exit 0; fi
  if [ "$FROM_SOURCE" = 1 ]; then source_packages "$(linux_family)"; else prereq_packages "$(linux_family)" "$(qemu_system_binary) qemu-img cloud-localds"; fi
  exit 0
fi

if [ "$SERVER" = 1 ] && [ -n "$JOIN_URL" ]; then die "--server and --join are different machines' jobs: run --server on the first one, --join on the others." 64; fi
if [ "$SERVER" = 1 ] || [ -n "$JOIN_URL" ]; then CLUSTER=1; secrets_dir; fi
if [ "$CLUSTER" = 1 ] && [ "$UNINSTALL" != 1 ]; then
  SERVICE=1
  if [ -n "$JOIN_URL" ]; then
    case "$JOIN_URL" in http://*|https://*) ;; *) die "--join must be an http(s):// URL (http://HOST:7799)." 64 ;; esac
    JOIN_URL="${JOIN_URL%/}"
  fi
fi

case "$OS/$ARCH" in
  Darwin/arm64) PLATFORM="macos-arm64" ;;
  Linux/x86_64) PLATFORM="linux-x86_64" ;;
  Linux/aarch64|Linux/arm64) PLATFORM="linux-aarch64" ;;
  Darwin/*) die "FluxVM's Mac package is Apple silicon only (found $ARCH)." 2 ;;
  *) die "No FluxVM package for $OS/$ARCH. Build from source: cargo build --release -p fluxctl (see the README, Quickstart)." 2 ;;
esac
# macOS: the cluster commands are fluxctl's (LaunchAgents, no root, no packages): hand over to the one already installed.
if [ "$OS" = "Darwin" ] && [ "$CLUSTER" = 1 ] && [ "$UNINSTALL" != 1 ]; then
  MACCTL="$(command -v fluxctl || true)"
  [ -n "$MACCTL" ] || { [ -x "$PREFIX/bin/fluxctl" ] && MACCTL="$PREFIX/bin/fluxctl"; } || true
  [ -n "$MACCTL" ] || die "On macOS install FluxVM for Mac first (https://github.com/zyvorai/zyvor-fluxvm/releases/tag/mac-v0.1.0), then run: fluxctl cluster $([ "$SERVER" = 1 ] && echo init || echo "join $JOIN_URL")" 2
  set -- cluster
  if [ "$SERVER" = 1 ]; then
    set -- "$@" init --port "$PORT" --token-ttl "$TOKEN_TTL"
    [ -z "$PUBLIC_URL" ] || set -- "$@" --public-url "$PUBLIC_URL"
  else
    set -- "$@" join "$JOIN_URL"
    [ -z "$NODE_NAME" ] || set -- "$@" --name "$NODE_NAME"
    [ -z "$ADVERTISE" ] || set -- "$@" --advertise-url "$ADVERTISE"
    [ -z "$TOKEN_FILE" ] || set -- "$@" --token-file "$TOKEN_FILE"
    [ "$TOKEN_STDIN" = 0 ] || set -- "$@" --token-stdin
  fi
  for l in $LABELS; do set -- "$@" --label "$l"; done
  [ "$WITH_K3S" = 0 ] || set -- "$@" --with-k3s
  [ "$YES" = 0 ] || set -- "$@" --yes
  [ "$DRY" = 0 ] || { printf '    would run: %s %s\n' "$MACCTL" "$*"; exit 0; }
  exec "$MACCTL" "$@"
fi
if [ "$OS" = "Darwin" ] && [ "$UNINSTALL" != 1 ]; then
  die "Prebuilt macOS packages are no longer published. Use FluxVM for Mac (30-day trial): https://github.com/zyvorai/zyvor-fluxvm/releases/tag/mac-v0.1.0 or build from source: xcode-select --install && brew install hivex && cargo build -p fluxctl (see the README, Quickstart)." 2
fi
if [ "$OS" = "Darwin" ]; then
  MAJOR="$(sw_vers -productVersion | cut -d. -f1)"
  if [ "$MAJOR" -lt 14 ] 2>/dev/null; then
    die "macOS 14 (Sonoma) or newer is required (found $(sw_vers -productVersion))." 2
  fi
fi

read_join_token() { # sets TOKEN_FILE to a file holding the token: --token-file, $FLUXVM_TOKEN, --token-stdin, or a prompt
  local tok="" f
  if [ -n "$TOKEN_FILE" ]; then
    [ -f "$TOKEN_FILE" ] || die "--token-file $TOKEN_FILE does not exist." 64
    tok="$(tr -d ' \t\r\n' < "$TOKEN_FILE")"
  elif [ -n "${FLUXVM_TOKEN:-}" ]; then tok="$(printf '%s' "$FLUXVM_TOKEN" | tr -d ' \t\r\n')"
  elif [ "$TOKEN_STDIN" = 1 ]; then tok="$(tr -d ' \t\r\n')"
  elif { true </dev/tty; } 2>/dev/null; then
    printf 'Join token (input is hidden): ' >&2
    read -rs tok </dev/tty || tok=""; printf '\n' >&2
    tok="$(printf '%s' "$tok" | tr -d ' \t\r\n')"
  fi
  [ -n "$tok" ] || die "No join token. Give it as --token-file FILE, in \$FLUXVM_TOKEN, or on stdin with --token-stdin (never as an argument). Make one on the server: fluxctl cluster token create" 64
  case "$tok" in fvjt_*) ;; *) die "That does not look like a join token (they start with fvjt_). A fleet or operator token is not accepted here." 64 ;; esac
  case "$tok" in *[!A-Za-z0-9._-]*) die "The join token has characters a token never has." 64 ;; esac
  f="$SECRETS/join.token"
  printf '%s\n' "$tok" > "$f"
  TOKEN_FILE="$f"
}

# Is this machine already a node of this central (the agent service is up with the same settings)?
joined_already() {
  [ -r "$ROOTFS/etc/fluxvm/agent.env" ] || return 1
  grep -q "^CENTRAL_URL=$CENTRAL\$" "$ROOTFS/etc/fluxvm/agent.env" 2>/dev/null || return 1
  grep -q "^NODE_NAME=$NODE_NAME\$" "$ROOTFS/etc/fluxvm/agent.env" 2>/dev/null || return 1
  case "$(linux_init)" in
    systemd) systemctl is-active --quiet fluxvm-agent 2>/dev/null ;;
    openrc) rc-service fluxvm-agent status >/dev/null 2>&1 ;;
    *) return 1 ;;
  esac
}

if [ -n "$JOIN_URL" ] && [ "$UNINSTALL" != 1 ]; then
  [ "$OS" = Linux ] || die "--join on macOS is: fluxctl cluster join URL (install FluxVM for Mac first)." 64
  AGENT=1; CENTRAL="$JOIN_URL"
  [ -n "$NODE_NAME" ] || NODE_NAME="$(default_node_name)"
  [ -n "$ADVERTISE" ] || ADVERTISE="http://$(primary_ip):7788"
  if joined_already && [ -z "$TOKEN_FILE" ] && [ -z "${FLUXVM_TOKEN:-}" ] && [ "$TOKEN_STDIN" = 0 ]; then
    JOINED=1
    TOKEN_FILE="$SECRETS/none"; : > "$TOKEN_FILE"
  elif [ "$DRY" = 1 ]; then TOKEN_FILE="$SECRETS/dry"; : > "$TOKEN_FILE"
  else read_join_token; fi
fi
JOINED="${JOINED:-0}"

if [ "$AGENT" = 1 ] && [ "$UNINSTALL" != 1 ]; then
  [ "$OS" = Linux ] || die "--agent is for Linux. On a Mac use: fluxctl node-agent install (see deploy/kairon-node-mac.md)." 64
  [ "$SERVICE" = 1 ] || die "--agent needs --service (the agent runs as a service next to the daemon)." 64
  [ -n "$CENTRAL" ] && [ -n "$NODE_NAME" ] && [ -n "$ADVERTISE" ] && [ -n "$TOKEN_FILE" ] \
    || die "--agent needs --central, --node-name, --advertise-url and --token-file." 64
  case "$CENTRAL" in http://*|https://*) ;; *) die "--central must be an http(s):// URL." 64 ;; esac
  case "$ADVERTISE" in
    http://127.*|https://127.*|http://localhost*|https://localhost*|http://\[::1\]*|https://\[::1\]*)
      die "--advertise-url must be an address central can reach, not a loopback one." 64 ;;
    http://*|https://*) ;;
    *) die "--advertise-url must be an http(s):// URL." 64 ;;
  esac
  case "$NODE_NAME" in
    *[!a-z0-9.-]*|-*|.*|"") die "--node-name: lowercase letters, digits, dashes and dots only." 64 ;;
  esac
  [ -f "$TOKEN_FILE" ] || die "--token-file $TOKEN_FILE does not exist." 64
fi

SHARE="$PREFIX/share/fluxvm"
BIN="$PREFIX/bin"
CUR="$SHARE/current"
FLUXCTL_OVERRIDE=""
if [ "$SKIP_DOWNLOAD" = 1 ]; then
  # this script is share/install.sh of an installed package: its parent is the package root
  CUR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
  [ -x "$CUR/bin/fluxctl" ] || die "--skip-download: no bin/fluxctl next to this script ($CUR); it is only for the copy in an installed package." 64
  FLUXCTL_OVERRIDE="$CUR/bin/fluxctl"
fi

# ---------------------------------------------------------------------------------------------- Homebrew method
if [ "$BREW" = 1 ]; then
  [ "$OS" = "Darwin" ] || die "--brew is for macOS." 2
  command -v brew >/dev/null 2>&1 || die "Homebrew is required for --brew: https://brew.sh" 2
  if [ "$UNINSTALL" = 1 ]; then
    say "Stopping the service and removing FluxVM (Homebrew)"
    run brew services stop "$FORMULA" || true
    run brew uninstall "$FORMULA"
    exit 0
  fi
  say "Installing FluxVM ($FORMULA)"
  run brew install "$FORMULA"
  if [ "$SERVICE" = 1 ]; then
    run brew services start "$FORMULA"
  fi
  if [ -n "$CLIENT" ]; then
    run fluxctl mcp install "$CLIENT" --allow-write
  fi
  say "Done. The Homebrew package is built from a tagged release; omit --brew to download a release directly."
  exit 0
fi

# ---------------------------------------------------------------------------------------------- uninstall
if [ "$UNINSTALL" = 1 ]; then
  say "Removing FluxVM from $PREFIX"
  if [ "$OS" = "Darwin" ] && [ -x "$BIN/fluxctl" ] && [ "$DRY" = 0 ]; then
    "$BIN/fluxctl" service uninstall >/dev/null 2>&1 || true
  fi
  if [ -d "$BIN" ]; then
    for l in "$BIN"/*; do
      [ -L "$l" ] || continue
      case "$(readlink "$l")" in
        *share/fluxvm/*) run rm -f "$l" ;;
      esac
    done
  fi
  if [ -d "$SHARE" ]; then run rm -rf "$SHARE"; fi
  if [ "$OS" = Linux ] && { [ "$CLUSTER" = 1 ] || [ "$SERVICE" = 1 ] || [ -e "$ROOTFS/etc/fluxvm/agent.env" ] || [ -e "$ROOTFS/etc/systemd/system/fluxvm-central.service" ]; }; then
    say "Stopping and removing the FluxVM services this script installed (state in /var/lib/fluxvm* and VM images are kept)"
    setup_sudo
    case "$(linux_init)" in
      systemd)
        for u in fluxvm-agent fluxvm-central fluxvm; do
          if [ -e "$ROOTFS/etc/systemd/system/$u.service" ]; then
            svc systemctl disable --now "$u" || true
            svc rm -f "$ROOTFS/etc/systemd/system/$u.service"
          fi
        done
        svc systemctl daemon-reload || true ;;
      openrc)
        for u in fluxvm-agent fluxvm-central fluxvm; do
          if [ -e "$ROOTFS/etc/init.d/$u" ]; then
            svc rc-service "$u" stop || true
            svc rc-update del "$u" default || true
            svc rm -f "$ROOTFS/etc/init.d/$u"
          fi
        done ;;
    esac
    svc rm -f "$ROOTFS/etc/fluxvm/agent.env" "$ROOTFS/etc/fluxvm/agent.token" "$ROOTFS/etc/fluxvm/central.env"
    for b in fluxctl fluxvm-agent; do
      if [ -L "$ROOTFS/usr/local/bin/$b" ] || [ -f "$ROOTFS/usr/local/bin/$b" ]; then svc rm -f "$ROOTFS/usr/local/bin/$b"; fi
    done
  fi
  if [ "$OS" = Darwin ] && [ -x "$BIN/fluxctl" ] && [ "$DRY" = 0 ]; then "$BIN/fluxctl" node-agent uninstall >/dev/null 2>&1 || true; fi
  say "Done. VM images and state stay where they are (Linux: /var/lib/fluxvm, /var/lib/fluxvm-agent, /var/lib/fluxvm-central; macOS: ~/Library/Application Support/FluxVM)."
  exit 0
fi

# ---------------------------------------------------------------------------------------------- tools
command -v curl >/dev/null 2>&1 || die "curl is required." 2
command -v tar >/dev/null 2>&1 || die "tar is required." 2
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "sha256sum or shasum is required to verify the download." 2
fi

# ---------------------------------------------------------------------------------------------- version
if [ "$VERSION" = "latest" ] && [ "$SKIP_DOWNLOAD" = 0 ] && [ -z "$MIRROR" ]; then
  case "$BASE" in
    file://*) die "cannot resolve 'latest' from a file:// base; pass --version." 64 ;;
  esac
  # GitHub redirects /releases/latest to /releases/tag/<tag>; a draft is never "latest".
  final="$(curl -fsSIL -o /dev/null -w '%{url_effective}' "$BASE/latest")" \
    || die "Could not reach $BASE/latest (is there a published release?)."
  VERSION="${final##*/}"
  VERSION="${VERSION#v}"
  if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ]; then
    die "Could not work out the latest version from $final."
  fi
fi
case "$VERSION" in
  *[!0-9A-Za-z._-]*|"") die "not a version: $VERSION" 64 ;;
esac

ASSET="fluxvm-$VERSION-$PLATFORM.tar.gz"
ASSET_URL="$BASE/download/v$VERSION/$ASSET"
if [ "$SKIP_DOWNLOAD" = 1 ]; then say "Using the FluxVM that is already installed (--skip-download)"
elif [ -n "$MIRROR" ]; then say "FluxVM from the signed channel at $MIRROR (fluxvm-bootstrap, Ed25519 key $PUBKEY)"
else
  say "FluxVM $VERSION for $PLATFORM"
  say "from $ASSET_URL"
fi
if [ "$CLUSTER" = 1 ] && [ "$OS" = Linux ]; then ensure_prereqs; fi

if [ "$DRY" = 1 ]; then
  if [ "$SKIP_DOWNLOAD" = 0 ]; then printf '    would download: %s\n    would download: %s.sha256\n' "$ASSET_URL" "$ASSET_URL"; fi
  printf '    would verify the checksum, unpack to %s/%s, link %s/current/bin/* into %s\n' "$SHARE" "$VERSION" "$SHARE" "$BIN"
  if [ "$SERVICE" = 1 ] && [ "$OS" = Linux ]; then
    printf '    would set up the service for %s (%s)\n' "$(linux_init)" "$(linux_family)"
    if [ "$AGENT" = 1 ]; then printf '    would also set up the fleet node agent as %s\n' "$NODE_NAME"; fi
  elif [ "$SERVICE" = 1 ]; then printf '    would set up the service\n'; fi
  if [ -n "$CLIENT" ]; then printf '    would run: fluxctl mcp install %s --allow-write\n' "$CLIENT"; fi
  if [ "$SERVER" = 1 ]; then
    printf '    would run fluxvm-agent central as a service on port %s, make a join token (ttl %ss) and print the join command and dashboard URL\n' "$PORT" "$TOKEN_TTL"
  fi
  if [ -n "$JOIN_URL" ]; then
    # shellcheck disable=SC2016
    printf '    would enroll this machine as %s with %s (token from a file, $FLUXVM_TOKEN or stdin), wait until it is Ready\n' "$NODE_NAME" "$JOIN_URL"
  fi
  if [ "$CLUSTER" = 1 ]; then
    write_user_config
    if [ "$IMAGE" = 1 ]; then ensure_base_image; fi
    ensure_engine_image
    run_selftest
    if [ "$WITH_K3S" = 1 ]; then printf '    would then set up the k3s add-on: fluxctl kube up\n'; fi
  fi
  exit 0
fi

# ---------------------------------------------------------------------------------------------- download + verify
TMP="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-install.XXXXXX")"
# A progress bar only when someone is watching; piped into a log it is noise.
PROGRESS="-sS"
if [ -t 2 ]; then PROGRESS="--progress-bar"; fi
# The signed channel: the same Ed25519 chain `fluxvm-bootstrap` verifies on every node (channel file, release manifest, package).
bootstrap_install() {
  [ "$OS" = Linux ] || die "--mirror is for Linux." 64
  [ -n "$PUBKEY" ] && [ -r "$PUBKEY" ] || die "--mirror needs --pubkey FILE (the release public key)." 64
  command -v openssl >/dev/null 2>&1 || die "openssl 3 is required to verify the signed channel." 2
  setup_sudo
  curl -fsSL -o "$TMP/fluxvm-bootstrap" "${MIRROR%/}/fluxvm-bootstrap" || die "Could not fetch ${MIRROR%/}/fluxvm-bootstrap" 1
  printf 'MIRROR_URL=%s\nCHANNEL=%s\nINSTALL_MODE=tarball\n' "${MIRROR%/}" "${FLUXVM_CHANNEL:-stable}" > "$TMP/bootstrap.conf"
  if [ -n "$ROOTFS" ]; then mkdir -p "$ROOTFS/etc/fluxvm" "$ROOTFS/usr/local/bin"; fi
  svc install -d -m 0755 "$ROOTFS/etc/fluxvm"
  svc install -m 0644 "$TMP/bootstrap.conf" "$ROOTFS/etc/fluxvm/bootstrap.conf"
  svc install -m 0644 "$PUBKEY" "$ROOTFS/etc/fluxvm/release.pub"
  svc install -m 0755 "$TMP/fluxvm-bootstrap" "$ROOTFS/usr/local/bin/fluxvm-bootstrap"
  svc "$ROOTFS/usr/local/bin/fluxvm-bootstrap" install || die "fluxvm-bootstrap refused or failed (a bad signature, an expired channel and a revoked version all end here). Nothing was installed." 1
  CUR="$ROOTFS/opt/fluxvm/current"
  FLUXCTL_OVERRIDE="$ROOTFS/usr/local/bin/fluxctl"
}

if [ "$SKIP_DOWNLOAD" = 1 ]; then
  : # CUR was set to the package this script came from
elif [ -n "$MIRROR" ]; then
  bootstrap_install
else
  if ! curl -fL "$PROGRESS" -o "$TMP/$ASSET" "$ASSET_URL"; then
    die "Download failed: $ASSET_URL
  This release may not have a package for $PLATFORM. Releases: $BASE" 1
  fi
  if ! curl -fsSL -o "$TMP/$ASSET.sha256" "$ASSET_URL.sha256"; then
    die "Could not download the checksum $ASSET_URL.sha256; not installing an unverified package." 1
  fi
  want="$(awk '{print $1; exit}' "$TMP/$ASSET.sha256")"
  got="$(sha256_of "$TMP/$ASSET")"
  case "$want" in
    *[!0-9a-f]*|"") die "The checksum file does not hold a SHA-256: $want" 1 ;;
  esac
  if [ "${#want}" != 64 ] || [ "$want" != "$got" ]; then
    die "Checksum mismatch for $ASSET:
    expected $want
    got      $got
  Nothing was installed." 1
  fi
  say "Checksum verified ($got)"

  # ---------------------------------------------------------------------------------------------- unpack + link
  mkdir -p "$SHARE" "$BIN"
  STAGE="$SHARE/.staging-$VERSION.$$"
  rm -rf "$STAGE"; mkdir -p "$STAGE"
  tar -xzf "$TMP/$ASSET" -C "$STAGE"
  # The package is one directory (fluxvm-<ver>-<platform>/) holding bin/, share/, ...
  INNER="$(find "$STAGE" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  if [ -z "$INNER" ] || [ ! -d "$INNER/bin" ]; then
    die "The package has no bin/ directory; not installing."
  fi
  rm -rf "${SHARE:?}/$VERSION"
  mv "$INNER" "$SHARE/$VERSION"
  ln -sfn "$VERSION" "$SHARE/current"

  for f in "$SHARE/current/bin"/*; do
    name="$(basename "$f")"
    # Replace only our own links or nothing: never overwrite a real file of someone else's.
    if [ -e "$BIN/$name" ] && [ ! -L "$BIN/$name" ]; then
      printf '    skipped %s: %s/%s exists and is not a link\n' "$name" "$BIN" "$name" >&2
      continue
    fi
    ln -sfn "../share/fluxvm/current/bin/$name" "$BIN/$name"
  done
  say "Installed to $SHARE/$VERSION (linked from $BIN)"
fi

# ---------------------------------------------------------------------------------------------- runtime dependencies
if [ "$OS" = "Darwin" ]; then
  # Downloads made with curl are not quarantined; clear the flag in case they were fetched another way.
  xattr -dr com.apple.quarantine "$SHARE/$VERSION" 2>/dev/null || true
  if [ "$DEPS" = 1 ] && command -v otool >/dev/null 2>&1; then
    if otool -L "$CUR/bin/fluxctl" 2>/dev/null | grep -q hivex; then
      if [ ! -e /opt/homebrew/lib/libhivex.dylib ] && [ ! -e /usr/local/lib/libhivex.dylib ]; then
        if command -v brew >/dev/null 2>&1; then
          say "Installing the hivex library the package links (brew install hivex)"
          brew install hivex || printf 'warning: brew install hivex failed; fluxctl may not start until it is installed\n' >&2
        else
          printf 'warning: fluxctl needs libhivex. Install Homebrew (https://brew.sh) and run: brew install hivex\n' >&2
        fi
      fi
    fi
  fi
elif [ "$OS" = Linux ]; then
  FAMILY="$(linux_family)"
  case "$(linux_libc)" in
    musl)
      printf 'warning: this is a musl system (%s). The package is built for glibc %s or newer and will not run here.\n' "${FAMILY:-linux}" "$GLIBC_FLOOR" >&2
      printf '         Build from source instead: cargo build --release -p fluxctl (see the README, Quickstart).\n' >&2 ;;
    glibc)
      GV="$(glibc_version)"
      if [ -n "$GV" ] && version_lt "$GV" "$GLIBC_FLOOR"; then
        printf 'warning: this host has glibc %s; the package needs %s or newer (Ubuntu 22.04+, Debian 12+, Fedora, RHEL-family 10).\n' "$GV" "$GLIBC_FLOOR" >&2
        printf '         RHEL/Rocky/Alma 9 are not supported. Upgrade the OS, or build from source.\n' >&2
      fi ;;
  esac
  if command -v ldd >/dev/null 2>&1; then
    missing="$(ldd "$CUR/bin/fluxctl" 2>/dev/null | grep 'not found' || true)"
    if [ -n "$missing" ]; then
      printf 'warning: missing shared libraries:\n%s\n' "$missing" >&2
      DEPCMD="$(linux_dep_command "$FAMILY" "$(linux_pkg_manager "$FAMILY")")"
      if [ -z "$DEPCMD" ]; then
        printf 'Install the hivex and systemd shared libraries with your package manager.\n' >&2
      elif [ "$IS_ROOT" = 1 ] && [ "$DEPS" = 1 ]; then
        say "Installing the runtime libraries ($DEPCMD)"
        # shellcheck disable=SC2086  # the command is a fixed word list built above
        $DEPCMD || printf 'warning: that failed; fluxctl may not start until the libraries are installed\n' >&2
      else
        printf 'Install them with: sudo %s\n' "$DEPCMD" >&2
      fi
    fi
  fi
fi

# ---------------------------------------------------------------------------------------------- service, client
FLUXCTL="${FLUXCTL_OVERRIDE:-$BIN/fluxctl}"

install_unit() { # NAME INIT: put the unit or init script in place and start it
  local n="$1"
  case "$2" in
    systemd)
      svc install -m 0644 "$CUR/share/systemd/$n.service" "$ROOTFS/etc/systemd/system/$n.service"
      svc systemctl daemon-reload
      svc systemctl enable --now "$n" ;;
    openrc)
      [ -f "$CUR/share/openrc/$n" ] || die "This package has no OpenRC script (share/openrc/$n); it is in releases after 0.4.21." 1
      svc install -m 0755 "$CUR/share/openrc/$n" "$ROOTFS/etc/init.d/$n"
      svc rc-update add "$n" default
      svc rc-service "$n" start ;;
  esac
}

wait_http() { # URL SECONDS: wait until the URL answers
  local i=0
  until curl -fsS -o /dev/null "$1" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -le "$2" ] || return 1
    sleep 1
  done
}

# api METHOD URL [BODY]: a call with the bearer token read from a header file (the token is never an argument of curl)
api() {
  if [ -n "${3:-}" ]; then curl -fsS -X "$1" -H "@$HDR_FILE" -H 'Content-Type: application/json' -d "$3" "$2"
  else curl -fsS -X "$1" -H "@$HDR_FILE" "$2"; fi
}

server_finish() { # central is installed and running: make the join token and say what to do next
  local base="http://127.0.0.1:$PORT" optok resp tok tokfile public dash printed
  public="${PUBLIC_URL:-http://$(primary_ip):$PORT}"
  say "Waiting for central on $base"
  wait_http "$base/fleet/ha" 60 || die "Central did not answer on $base within 60 s. See: journalctl -u fluxvm-central (or /var/log/fluxvm-central.log)" 1
  optok=""
  local i=0
  while [ -z "$optok" ] && [ "$i" -lt 30 ]; do
    optok="$(svc_cat "$ROOTFS/var/lib/fluxvm-central/operator-token" | tr -d ' \t\r\n')"
    [ -n "$optok" ] || { i=$((i + 1)); sleep 1; }
  done
  [ -n "$optok" ] || die "Central started but its operator token is not in /var/lib/fluxvm-central/operator-token." 1
  HDR_FILE="$SECRETS/hdr"
  ( umask 077; printf 'Authorization: Bearer %s\n' "$optok" > "$HDR_FILE" )
  resp="$(api POST "$base/fleet/enroll/tokens" "{\"ttl_secs\":$TOKEN_TTL,\"note\":\"install.sh --server\"}")" \
    || die "Central refused to make a join token (is --node-auth legacy set?)." 1
  tok="$(printf '%s' "$resp" | sed -n 's/.*"token":"\(fvjt_[^"]*\)".*/\1/p')"
  [ -n "$tok" ] || die "Central's answer had no join token." 1
  tokfile="${XDG_CONFIG_HOME:-${HOME:-/tmp}/.config}/fluxvm/join.token"
  mkdir -p "$(dirname "$tokfile")"
  ( umask 077; printf '%s\n' "$tok" > "$tokfile" )
  dash="http://127.0.0.1:$PORT/console/"
  printed="FLUXVM_TOKEN=\"\$(cat join.token)\""
  if [ -t 1 ] || [ "${FLUXVM_PRINT_TOKEN:-}" = 1 ]; then printed="FLUXVM_TOKEN=$tok"; fi
  cat <<EOSERVER

==> FluxVM server is Ready.
    Central:    $public   (node auth: join tokens; operator token: /var/lib/fluxvm-central/operator-token, root only)
    Dashboard:  $dash   (FluxVM Studio; paste the operator token)
                it listens on every interface; from a laptop use:  ssh -L $PORT:127.0.0.1:$PORT USER@$(primary_ip)
                then open $dash. Put TLS in front before exposing it beyond a trusted network (--tls-cert/--tls-key).

    Add a machine (Linux or macOS), one command, token valid for $((TOKEN_TTL / 60)) minutes:

      curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | $printed bash -s -- --join $public

    The token is also in $tokfile (mode 0600): use it on another machine with --token-file.
    $([ "$printed" = "FLUXVM_TOKEN=$tok" ] || echo "(stdout is not a terminal, so the token was not printed here.)")
    More tokens: fluxctl cluster token create     Nodes: fluxctl cluster nodes     Dashboard: $dash
    High availability: run a second central with --ha-redis (docs/fleet-ha.md); this installer sets up one.
EOSERVER
}

join_wait_ready() { # the node's own credential works at central: the node is enrolled and accepted
  local credf="$ROOTFS/var/lib/fluxvm-agent/node-credential" cred="" i=0 code="" tries="${FLUXVM_READY_TRIES:-60}"
  say "Waiting for $CENTRAL to accept $NODE_NAME"
  while [ "$i" -lt "$tries" ]; do
    cred="$(svc_cat "$credf" | tr -d ' \t\r\n')"
    if [ -n "$cred" ]; then
      HDR_FILE="$SECRETS/hdr"
      ( umask 077; printf 'Authorization: Bearer %s\n' "$cred" > "$HDR_FILE" )
      code="$(curl -s -o /dev/null -w '%{http_code}' -H "@$HDR_FILE" "$CENTRAL/fleet/nodes/$NODE_NAME/overlays" 2>/dev/null || true)"
      if [ "$code" = 200 ]; then return 0; fi
    fi
    i=$((i + 1)); sleep 2
  done
  return 1
}

if [ "$SERVICE" = 1 ]; then
  if [ "$OS" = "Darwin" ]; then
    say "Installing the LaunchAgent (starts at login)"
    "$FLUXCTL" service install
    i=0
    until curl -fsS "$URL/healthz" >/dev/null 2>&1; do
      i=$((i + 1))
      if [ "$i" -gt 60 ]; then die "The daemon did not answer on $URL within 60 s. See: fluxctl service status" 1; fi
      sleep 1
    done
    say "Daemon is up: $(curl -fsS "$URL/healthz")"
  else
    INIT="$(linux_init)"
    if [ "$CLUSTER" = 1 ]; then setup_sudo; write_user_config; fi
    if [ "$IS_ROOT" = 1 ] || [ -n "$SUDO" ]; then say "Linux service ($INIT)"; else say "Linux service ($INIT): this needs root, so the steps are printed, not run"; fi
    if [ -n "$ROOTFS" ]; then mkdir -p "$ROOTFS/usr/local/bin" "$ROOTFS/etc/systemd/system" "$ROOTFS/etc/init.d" "$ROOTFS/etc/fluxvm"; fi
    if [ -n "$FLUXCTL_OVERRIDE" ] && [ "$CUR" = "$ROOTFS/opt/fluxvm/current" ]; then :
    else svc install -m 0755 "$CUR/bin/fluxctl" "$ROOTFS/usr/local/bin/fluxctl"; fi
    if [ -e "$ROOTFS/etc/fluxvm.toml" ]; then say "/etc/fluxvm.toml already exists; leaving it"; else svc install -m 0644 "$CUR/share/config.example.toml" "$ROOTFS/etc/fluxvm.toml"; fi
    case "$INIT" in
      systemd|openrc) install_unit fluxvm "$INIT" ;;
      *) say "No systemd or OpenRC found. Run the daemon yourself: fluxctl --config /etc/fluxvm.toml serve" ;;
    esac
    if [ "$SERVER" = 1 ]; then
      case "$INIT" in
        systemd|openrc) ;;
        *) die "--server needs systemd or OpenRC to keep central running. Run it by hand: fluxvm-agent central --listen 0.0.0.0:$PORT" 1 ;;
      esac
      say "Fleet central on port $PORT"
      printf 'LISTEN=0.0.0.0:%s\n' "$PORT" > "$SECRETS/central.env"
      svc install -d -m 0755 "$ROOTFS/etc/fluxvm"
      svc install -m 0644 "$SECRETS/central.env" "$ROOTFS/etc/fluxvm/central.env"
      svc install -m 0755 "$CUR/bin/fluxvm-agent" "$ROOTFS/usr/local/bin/fluxvm-agent"
      install_unit fluxvm-central "$INIT"
    fi
    if [ "$AGENT" = 1 ]; then
      LABEL_ARGS=""
      for l in $LABELS; do LABEL_ARGS="$LABEL_ARGS --label $l"; done
      secrets_dir
      AGENT_ENV="$SECRETS/agent.env"
      printf 'CENTRAL_URL=%s\nNODE_NAME=%s\nADVERTISE_URL=%s\nLABEL_ARGS="%s"\n' "$CENTRAL" "$NODE_NAME" "$ADVERTISE" "${LABEL_ARGS# }" > "$AGENT_ENV"
      say "Fleet node agent: $NODE_NAME -> $CENTRAL"
      svc install -d -m 0755 "$ROOTFS/etc/fluxvm"
      svc install -m 0644 "$AGENT_ENV" "$ROOTFS/etc/fluxvm/agent.env"
      if [ "$JOINED" = 1 ]; then say "Already enrolled: keeping the existing token and credential"
      else svc install -m 0600 "$TOKEN_FILE" "$ROOTFS/etc/fluxvm/agent.token"; fi
      svc install -m 0755 "$CUR/bin/fluxvm-agent" "$ROOTFS/usr/local/bin/fluxvm-agent"
      case "$INIT" in
        systemd|openrc)
          install_unit fluxvm-agent "$INIT"
          # a restart picks up changed labels or advertise URL on a re-run
          if [ "$JOINED" = 1 ]; then
            case "$INIT" in systemd) svc systemctl restart fluxvm-agent ;; openrc) svc rc-service fluxvm-agent restart ;; esac
          fi ;;
        *) say "No systemd or OpenRC found. Run it yourself: fluxvm-agent node --advertise-url $ADVERTISE (with CENTRAL_URL, NODE_NAME and FLUXVM_AGENT_TOKEN_FILE set)" ;;
      esac
    fi
  fi
fi

if [ -n "$CLIENT" ]; then
  say "Wiring $CLIENT to FluxVM over MCP (write tools enabled)"
  "$FLUXCTL" mcp install "$CLIENT" --allow-write
  "$FLUXCTL" mcp status || true
fi

if [ "$CLUSTER" = 1 ] && [ "$OS" = Linux ]; then
  if [ "$IMAGE" = 1 ]; then ensure_base_image; fi
  ensure_engine_image
  if [ "$SERVER" = 1 ]; then server_finish; fi
  if [ -n "$JOIN_URL" ]; then
    if join_wait_ready; then
      printf '\n==> %s is Ready: central accepted it. On the server: fluxctl cluster nodes\n' "$NODE_NAME"
    else
      printf '\nwarning: %s did not become Ready within 2 minutes. Check: journalctl -u fluxvm-agent (the token may have expired: make a new one on the server with fluxctl cluster token create)\n' "$NODE_NAME" >&2
      JOIN_FAILED=1
    fi
  fi
  if [ "$WITH_K3S" = 1 ]; then
    say "Optional add-on: k3s (fluxctl kube up). The cluster above does not need it."
    "$FLUXCTL" kube up || printf 'warning: the k3s add-on did not start (fluxctl kube up failed); the cluster itself is unaffected.\n' >&2
  fi
  run_selftest
fi

INSTALLED="$("$FLUXCTL" --version 2>/dev/null || true)"
if [ -n "$INSTALLED" ]; then
  say "Installed: $INSTALLED"
else
  printf 'warning: fluxctl did not run; see the dependency note above\n' >&2
fi

case ":$PATH:" in
  *":$BIN:"*) ;;
  *)
    # shellcheck disable=SC2016  # $PATH is meant literally: it is the line the user will paste
    printf '\nAdd it to your PATH:\n  export PATH="%s:$PATH"      # put this line in ~/.zshrc or ~/.bashrc\n' "$BIN" ;;
esac

# Tab completion is one line per shell; offer the one for the shell this was run from.
case "${SHELL##*/}" in
  zsh)  COMPLETE='mkdir -p ~/.zfunc && fluxctl completions zsh > ~/.zfunc/_fluxctl   # and put fpath+=~/.zfunc before compinit in ~/.zshrc' ;;
  bash) COMPLETE='mkdir -p ~/.local/share/bash-completion/completions && fluxctl completions bash > ~/.local/share/bash-completion/completions/fluxctl' ;;
  fish) COMPLETE='mkdir -p ~/.config/fish/completions && fluxctl completions fish > ~/.config/fish/completions/fluxctl.fish' ;;
  *)    COMPLETE='fluxctl completions bash|zsh|fish   # prints the script for your shell' ;;
esac

cat <<EONEXT

Next:
  fluxctl doctor                                      # is this host ready? (every failure prints its fix)
  fluxctl run                                         # a throwaway VM and a shell
  fluxctl mcp install claude-code --allow-write       # let an agent drive it
  fluxctl agent run --policy developer-safe           # an agent session (releases that include it)
Tab completion:  $COMPLETE
Upgrade: run the same command again. Pin or roll back: --version X. Remove: --uninstall.
Docs: https://zyvorai.github.io/zyvor-fluxvm/docs/install
EONEXT
if [ "$SELFTEST_FAILED" = 1 ]; then printf 'The install finished, but the self-test failed (see above).\n'; fi
if [ "$JOIN_FAILED" = 1 ]; then exit 1; fi
