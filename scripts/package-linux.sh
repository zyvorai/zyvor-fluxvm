#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Builds the Linux package for a GitHub release and for the node installers:
#   target/package/fluxvm-<version>-linux-<arch>.tar.gz  (+ .sha256)
#
#   scripts/package-linux.sh [--profile release|debug] [--version X.Y.Z] [--target aarch64-unknown-linux-gnu]
#                            [--groups core] [--stage-only] [--check-manifest]
#
# What goes in is listed in packaging/files.txt (one line per file); this script builds exactly that and fails when
# a required file is missing, so the package cannot silently lose a binary. Output is one directory holding bin/,
# lib/bpf/, share/ (config.example.toml, systemd/fluxvm.service, guest/, completions/), LICENSE and MANIFEST.sha256
# (a sha256 of every file), which scripts/install.sh unpacks. The same staged tree feeds the .deb/.rpm builds.
#
# --check-manifest  validate packaging/files.txt against the repository and exit (any OS, no build; CI runs it)
# --stage-only      build and stage the tree, but do not create the tarball
# --groups          comma separated manifest groups to include (default: core)
# --target          cross-compiles on an x86_64 host (needs the target's Rust std, gcc-aarch64-linux-gnu and the
#                   arm64 development packages; see .github/workflows/release-linux.yml). A cross build cannot run
#                   the binaries it makes, so shell completions are left out of it.
# Needs Linux, Rust, the packages ci.yml installs (libsystemd-dev libhivex-dev clang llvm libbpf-dev linux-libc-dev),
# and guestkit checked out next to this repository (fluxvm-image uses it; packaging/guestkit.ref pins the tag).
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=release
TARGET=""
GROUPS_WANTED="core"
STAGE_ONLY=0
CHECK_ONLY=0
MANIFEST="${PACKAGE_MANIFEST:-packaging/files.txt}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/fluxctl/Cargo.toml | head -n1)"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --target) TARGET="$2"; shift 2 ;;
    --groups) GROUPS_WANTED="$2"; shift 2 ;;
    --stage-only) STAGE_ONLY=1; shift ;;
    --check-manifest) CHECK_ONLY=1; shift ;;
    *) echo "usage: $0 [--profile release|debug] [--version X.Y.Z] [--target aarch64-unknown-linux-gnu] [--groups core] [--stage-only] [--check-manifest]" >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------------------------------------- the manifest
M_GROUP=(); M_KIND=(); M_SRC=(); M_DEST=(); M_MODE=(); M_OPT=()

read_manifest() {
  local line n=0 g k s d m o extra
  [[ -f "$MANIFEST" ]] || { echo "package-linux: $MANIFEST is missing" >&2; return 1; }
  while IFS= read -r line || [[ -n "$line" ]]; do
    n=$((n + 1))
    line="${line%%#*}"
    [[ -z "${line//[[:space:]]/}" ]] && continue
    read -r g k s d m o extra <<<"$line"
    if [[ -z "$g" || -z "$k" || -z "$s" || -z "$d" || -z "$m" ]]; then
      echo "package-linux: $MANIFEST:$n: need GROUP KIND SOURCE DEST MODE [optional]" >&2; return 1
    fi
    if [[ -n "${extra:-}" || ( -n "${o:-}" && "$o" != optional ) ]]; then
      echo "package-linux: $MANIFEST:$n: unexpected trailing text '${o:-} ${extra:-}'" >&2; return 1
    fi
    M_GROUP+=("$g"); M_KIND+=("$k"); M_SRC+=("$s"); M_DEST+=("$d"); M_MODE+=("$m"); M_OPT+=("${o:-}")
  done <"$MANIFEST"
}

# A cargo package <pkg> has a binary <bin> when its Cargo.toml says so, or it is the package's default binary.
bin_exists() {
  local pkg="$1" bin="$2" dir="crates/$1"
  [[ -f "$dir/Cargo.toml" ]] || return 1
  grep -q "^name = \"$pkg\"" "$dir/Cargo.toml" || return 1
  if grep -A3 '^\[\[bin\]\]' "$dir/Cargo.toml" | grep -q "name = \"$bin\""; then return 0; fi
  [[ "$bin" == "$pkg" && -f "$dir/src/main.rs" ]] && return 0
  [[ -f "$dir/src/bin/$bin.rs" ]] && return 0
  return 1
}

check_manifest() {
  local i bad=0 k s d m base dests=" "
  [[ ${#M_KIND[@]} -gt 0 ]] || { echo "package-linux: $MANIFEST has no entries" >&2; return 1; }
  for i in "${!M_KIND[@]}"; do
    k="${M_KIND[$i]}"; s="${M_SRC[$i]}"; d="${M_DEST[$i]}"; m="${M_MODE[$i]}"
    err() { echo "package-linux: $MANIFEST: ${M_GROUP[$i]} $k $s -> $d: $*" >&2; bad=$((bad + 1)); }
    case "${M_GROUP[$i]}" in core|extras) ;; *) err "unknown group" ;; esac
    [[ "$m" =~ ^[0-7]{3,4}$ ]] || err "mode '$m' is not octal"
    [[ "$d" != /* && "$d" != *..* ]] || err "DEST must be relative and stay inside the package"
    [[ "$dests" != *" $d "* ]] || err "DEST is listed twice"
    dests+="$d "
    case "$k" in
      bin)
        [[ "$s" == */* ]] || { err "SOURCE must be <cargo package>/<binary>"; continue; }
        bin_exists "${s%%/*}" "${s#*/}" || err "no binary '${s#*/}' in crates/${s%%/*}"
        ;;
      link)
        [[ " ${M_DEST[*]} " == *" $(dirname "$d")/$s "* ]] || err "link target $(dirname "$d")/$s is not in the manifest"
        ;;
      file) [[ -e "$s" ]] || [[ "${M_OPT[$i]}" == optional ]] || err "file does not exist" ;;
      bpf)
        base="${s%.bpf.o}"; base="${base%_tier_[SML]}"
        [[ -f "bpf/$base.bpf.c" ]] || err "no bpf/$base.bpf.c to build it from"
        grep -q "${base}" scripts/build-ebpf.sh || err "scripts/build-ebpf.sh does not build $base"
        ;;
      guest) [[ "$s" == fluxvm-guest-agent ]] || err "only fluxvm-guest-agent is supported" ;;
      completions) [[ "$s" == bash || "$s" == zsh || "$s" == fish ]] || err "shell must be bash, zsh or fish" ;;
      *) err "unknown kind" ;;
    esac
  done
  [[ $bad -eq 0 ]] || { echo "package-linux: $bad problem(s) in $MANIFEST" >&2; return 1; }
  echo "package-linux: $MANIFEST ok (${#M_KIND[@]} entries)"
}

read_manifest || exit 1
if [[ $CHECK_ONLY -eq 1 ]]; then check_manifest; exit $?; fi
check_manifest || exit 1

# ------------------------------------------------------------------------------------------------- the build
[[ "$(uname -s)" == Linux ]] || { echo "Linux only" >&2; exit 2; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || { echo "not a version: $VERSION" >&2; exit 2; }

case "${TARGET:-$(uname -m)}" in
  x86_64|x86_64-unknown-linux-gnu) ARCH=x86_64; BPF_ARCH=x86 ;;
  aarch64|arm64|aarch64-unknown-linux-gnu) ARCH=aarch64; BPF_ARCH=arm64 ;;
  *) echo "unsupported architecture ${TARGET:-$(uname -m)}" >&2; exit 2 ;;
esac

BUILD_DIR="target/$PROFILE"
STRIP="strip"
CROSS=0
if [[ -n "$TARGET" ]]; then
  BUILD_DIR="target/$TARGET/$PROFILE"
  if [[ "$ARCH" == aarch64 && "$(uname -m)" != aarch64 ]]; then STRIP=aarch64-linux-gnu-strip; CROSS=1; fi
fi
if [[ "$PROFILE" == release ]]; then FLAG=--release; else FLAG=; fi

wanted() { [[ ",$GROUPS_WANTED," == *",$1,"* ]]; }

# Cargo packages to build: the distinct <pkg> of every wanted `bin` entry.
packages=()
for i in "${!M_KIND[@]}"; do
  wanted "${M_GROUP[$i]}" && [[ "${M_KIND[$i]}" == bin ]] || continue
  p="${M_SRC[$i]%%/*}"
  [[ " ${packages[*]:-} " == *" $p "* ]] || packages+=("$p")
done
[[ ${#packages[@]} -gt 0 ]] || { echo "package-linux: no binaries selected by --groups $GROUPS_WANTED" >&2; exit 2; }
pargs=(); for p in "${packages[@]}"; do pargs+=(-p "$p"); done
# shellcheck disable=SC2086
cargo build $FLAG ${TARGET:+--target "$TARGET"} "${pargs[@]}"

NAME="fluxvm-$VERSION-linux-$ARCH"
OUT="target/package"; STAGE="$OUT/$NAME"
rm -rf "$STAGE" "$OUT/$NAME.tar.gz" "$OUT/$NAME.tar.gz.sha256"
mkdir -p "$STAGE"

# BPF objects are built once, for the target architecture (the build host's can differ in a cross build).
need_bpf=0
for i in "${!M_KIND[@]}"; do wanted "${M_GROUP[$i]}" && [[ "${M_KIND[$i]}" == bpf ]] && need_bpf=1; done
BPF_OUT="$OUT/bpf-$ARCH"
if [[ $need_bpf -eq 1 ]]; then
  rm -rf "$BPF_OUT"
  FLUXVM_BPF_ARCH="$BPF_ARCH" scripts/build-ebpf.sh "$BPF_OUT" >/dev/null
fi

missing_required=(); missing_optional=()
put() { # SOURCE-FILE DEST MODE
  mkdir -p "$STAGE/$(dirname "$2")"
  cp "$1" "$STAGE/$2"
  chmod "$3" "$STAGE/$2"
}

for i in "${!M_KIND[@]}"; do
  wanted "${M_GROUP[$i]}" || continue
  k="${M_KIND[$i]}"; s="${M_SRC[$i]}"; d="${M_DEST[$i]}"; m="${M_MODE[$i]}"; opt="${M_OPT[$i]}"
  ok=0
  case "$k" in
    bin)
      b="${s#*/}"
      if [[ -f "$BUILD_DIR/$b" ]]; then
        put "$BUILD_DIR/$b" "$d" "$m"
        # Release binaries are stripped to keep the download small; a debug package is left as built.
        if [[ "$PROFILE" == release ]]; then "$STRIP" "$STAGE/$d" 2>/dev/null || true; fi
        ok=1
      fi
      ;;
    link)
      mkdir -p "$STAGE/$(dirname "$d")"
      ln -sfn "$s" "$STAGE/$d"
      ok=1
      ;;
    file)
      if [[ -e "$s" ]]; then put "$s" "$d" "$m"; ok=1; fi
      ;;
    bpf)
      if [[ -f "$BPF_OUT/$s" ]]; then put "$BPF_OUT/$s" "$d" "$m"; ok=1; fi
      ;;
    guest)
      gt="${ARCH}-unknown-linux-musl"
      tmp="$OUT/guest-$ARCH"
      mkdir -p "$tmp"
      # A cross build links with the cross gcc; the musl target brings its own C runtime.
      cross_env=()
      [[ $CROSS -eq 1 ]] && cross_env=("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc")
      if env "${cross_env[@]}" GUEST_AGENT_TARGET="$gt" scripts/build-guest-agent-static.sh "$tmp/$s" >/dev/null 2>"$tmp/build.log" && [[ -f "$tmp/$s" ]]; then
        put "$tmp/$s" "$d" "$m"
        ok=1
      else
        echo "package-linux: static guest agent not built ($gt): $(tail -n 1 "$tmp/build.log" 2>/dev/null)" >&2
      fi
      ;;
    completions)
      if [[ $CROSS -eq 0 && -x "$STAGE/bin/fluxctl" ]]; then
        mkdir -p "$STAGE/$(dirname "$d")"
        if "$STAGE/bin/fluxctl" completions "$s" >"$STAGE/$d" 2>/dev/null && [[ -s "$STAGE/$d" ]]; then
          chmod "$m" "$STAGE/$d"
          ok=1
        else
          rm -f "$STAGE/$d"
        fi
      fi
      ;;
  esac
  if [[ $ok -eq 0 ]]; then
    if [[ "$opt" == optional ]]; then missing_optional+=("$d"); else missing_required+=("$d"); fi
  fi
done

if [[ ${#missing_required[@]} -gt 0 ]]; then
  printf 'package-linux: required file missing from the package: %s\n' "${missing_required[@]}" >&2
  exit 1
fi
[[ ${#missing_optional[@]} -gt 0 ]] && printf 'package-linux: left out (optional): %s\n' "${missing_optional[*]}" >&2

# A hash of every file, so a node (or a test) can check what it unpacked.
( cd "$STAGE" && find . -type f | LC_ALL=C sort | sed 's|^\./||' | xargs sha256sum > "../$NAME.sha256-files" )
mv "$OUT/$NAME.sha256-files" "$STAGE/MANIFEST.sha256"

if [[ $STAGE_ONLY -eq 1 ]]; then
  echo "$STAGE"
  exit 0
fi
tar -czf "$OUT/$NAME.tar.gz" -C "$OUT" "$NAME"
( cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
echo "$OUT/$NAME.tar.gz"
cat "$OUT/$NAME.tar.gz.sha256"
