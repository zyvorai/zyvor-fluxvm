#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# fluxvm-bootstrap: the pull installer. A node holds a mirror URL and the release public key; it fetches the signed
# channel file, decides whether it is in the current rollout wave, downloads and verifies the package named by the
# signed manifest, installs it, checks the node came up, and rolls back by itself when it did not. Nothing is
# compiled on the node and nobody connects to it. Linux only; separate from scripts/install.sh (which also runs on macOS).
#
#   fluxvm-bootstrap install [--dry-run] [--force]    fetch, verify, install if due (what the timer runs)
#   fluxvm-bootstrap check                            same, installs nothing; says what would happen
#   fluxvm-bootstrap rollback                         go back to the previous version and mark this one failed
#   fluxvm-bootstrap status                           print status.json
#   fluxvm-bootstrap pin [VERSION] | unpin           hold the node on a version (default: the installed one)
#   fluxvm-bootstrap verify                           check the installed files against the package's hashes
#
# Configuration: /etc/fluxvm/bootstrap.conf (shell syntax) or the environment.
#   MIRROR_URL         base URL of the mirror (required)         CHANNEL     stable (default)
#   PUBKEY             release public key (default /etc/fluxvm/release.pub, then /usr/share/fluxvm/release.pub)
#   NODE_ID            default: /etc/machine-id                   LABELS      space separated key=value, for rollout rules
#   INSTALL_MODE       deb | rpm | tarball (default: deb when dpkg exists, rpm when rpm and dnf/yum exist)
#   FLUXVM_AUTO_UPGRADE=0   check and report only; never install
#   JITTER_MAX         seconds of start-up delay, stable per node plus random (default 300)
# Testing: FLUXVM_ROOT prefixes every path the script writes (and disables systemctl and root checks).
# Needs bash, curl, openssl >= 3.0, jq, sha256sum, flock, and dpkg/apt-get for deb mode, rpm and dnf (or yum) for rpm mode, or tar for tarball mode.
set -uo pipefail

ROOT="${FLUXVM_ROOT:-}"
CONF="${BOOTSTRAP_CONF:-$ROOT/etc/fluxvm/bootstrap.conf}"
# shellcheck disable=SC1090
[[ -r "$CONF" ]] && . "$CONF"

MIRROR_URL="${MIRROR_URL:-}"; CHANNEL="${CHANNEL:-stable}"
NODE_ID="${NODE_ID:-$(cat "$ROOT/etc/machine-id" 2>/dev/null || true)}"
LABELS="${LABELS:-}"
STATE="${BOOTSTRAP_STATE:-$ROOT/var/lib/fluxvm-bootstrap}"
OPT="$ROOT/opt/fluxvm"
JITTER_MAX="${JITTER_MAX:-300}"
TRIES="${FETCH_TRIES:-6}"
BACKOFF_BASE="${BACKOFF_BASE:-2}"
AUTO="${FLUXVM_AUTO_UPGRADE:-1}"
HEALTH_WAIT="${HEALTH_WAIT:-60}"
if [[ -z "${PUBKEY:-}" ]]; then
  PUBKEY="$ROOT/etc/fluxvm/release.pub"; [[ -r "$PUBKEY" ]] || PUBKEY="$ROOT/usr/share/fluxvm/release.pub"
fi
if [[ -z "${INSTALL_MODE:-}" ]]; then
  if [[ -n "$ROOT" ]]; then INSTALL_MODE=tarball
  elif command -v dpkg >/dev/null 2>&1; then INSTALL_MODE=deb
  elif command -v rpm >/dev/null 2>&1 && { command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; }; then INSTALL_MODE=rpm
  else INSTALL_MODE=tarball; fi
fi

# A tarball install links the binaries into /usr/local/bin; the packages put them in /usr/bin.
if [[ "$INSTALL_MODE" == tarball ]]; then BIN_DIR="$ROOT/usr/local/bin"; else BIN_DIR="$ROOT/usr/bin"; fi

log() { echo "fluxvm-bootstrap: $*" >&2; }
die() { log "$*"; set_status error "$*"; exit 1; }
sha256() { sha256sum "$1" | cut -d' ' -f1; }
now() { date -u +%s; }
version_le() { [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" == "$1" ]]; }

mkdir -p "$STATE" "$STATE/cache" 2>/dev/null || { echo "fluxvm-bootstrap: cannot create $STATE" >&2; exit 1; }

# ---------------------------------------------------------------------------------------------------- status
CUR_FIELDS='{}'
set_status() { # STATE MESSAGE
  local st="$1" msg="${2:-}" cur
  cur="$(current_version 2>/dev/null || true)"
  jq -n --arg s "$st" --arg m "$msg" --arg c "$cur" --arg ch "$CHANNEL" --argjson t "$(now)" \
    --argjson serial "$(cat "$STATE/serial" 2>/dev/null || echo 0)" --argjson extra "$CUR_FIELDS" \
    '{state: $s, message: $m, current: $c, channel: $ch, serial: $serial, time: $t} + $extra' > "$STATE/status.json.tmp" 2>/dev/null \
    && mv "$STATE/status.json.tmp" "$STATE/status.json"
  local prom="$ROOT/var/lib/node_exporter/textfile_collector"
  if [[ -d "$prom" ]]; then
    {
      echo "# TYPE fluxvm_bootstrap_last_run_seconds gauge"; echo "fluxvm_bootstrap_last_run_seconds $(now)"
      echo "# TYPE fluxvm_bootstrap_ok gauge"; case "$st" in error|failed|rolled-back) echo "fluxvm_bootstrap_ok 0" ;; *) echo "fluxvm_bootstrap_ok 1" ;; esac
    } > "$prom/fluxvm_bootstrap.prom.tmp" && mv "$prom/fluxvm_bootstrap.prom.tmp" "$prom/fluxvm_bootstrap.prom"
  fi
}

current_version() {
  if [[ "$INSTALL_MODE" == deb ]]; then dpkg-query -W -f='${Version}' fluxvm 2>/dev/null
  elif [[ "$INSTALL_MODE" == rpm ]]; then rpm -q --qf '%{VERSION}' fluxvm 2>/dev/null | grep -v 'not installed' || true
  else basename "$(readlink "$OPT/current" 2>/dev/null)" 2>/dev/null | grep -v '^\.$' || true; fi
}

# ------------------------------------------------------------------------------------------------- fetching
nap() { local s="$1"; [[ "${FLUXVM_BOOTSTRAP_FAST:-0}" == 1 ]] && s=0; sleep "$s"; }
rand() { echo $((RANDOM % ($1 + 1))); }

# fetch URL OUTFILE [ETAGFILE]: returns 0 ok, 3 not modified (only with an ETAGFILE), 1 failed after retries.
# Retries network errors, 429 and 5xx with exponential backoff plus jitter, and waits at least Retry-After.
fetch() {
  local url="$1" out="$2" etagf="${3:-}" n=0 code hdr="$STATE/hdr.tmp" wait ra args
  while :; do
    args=(-sS -L --connect-timeout 15 --max-time 600 -D "$hdr" -o "$out.dl" -w '%{http_code}')
    [[ -n "$etagf" && -s "$etagf" ]] && args+=(-H "If-None-Match: $(cat "$etagf")")
    code="$(curl "${args[@]}" "$url" 2>/dev/null)" || code=000
    case "$code" in
      200) mv "$out.dl" "$out"; if [[ -n "$etagf" ]]; then grep -i '^etag:' "$hdr" | tail -n1 | cut -d' ' -f2- | tr -d '\r\n' > "$etagf"; fi; return 0 ;;
      304) rm -f "$out.dl"; return 3 ;;
      4??) if [[ "$code" != 429 ]]; then rm -f "$out.dl"; log "GET $url: HTTP $code"; return 1; fi ;;
    esac
    # file:// URLs report 000 on success with no headers; accept when the file arrived.
    if [[ "$code" == 000 && "$url" == file://* && -s "$out.dl" ]]; then mv "$out.dl" "$out"; return 0; fi
    rm -f "$out.dl"
    n=$((n + 1))
    if [[ $n -ge $TRIES ]]; then log "GET $url: HTTP $code after $n tries"; return 1; fi
    wait=$(( (BACKOFF_BASE ** n) + $(rand "$BACKOFF_BASE") )); [[ $wait -gt 300 ]] && wait=300
    ra="$(grep -i '^retry-after:' "$hdr" 2>/dev/null | tail -n1 | tr -d '\r' | awk '{print $2}')"
    if [[ "$ra" =~ ^[0-9]+$ ]]; then [[ $ra -gt 600 ]] && ra=600; [[ $ra -gt $wait ]] && wait=$ra; wait=$((wait + $(rand 2))); fi
    log "GET $url: HTTP $code, retry $n/$TRIES in ${wait}s"
    nap "$wait"
  done
}

# fetch_resumable URL OUTFILE: download to OUTFILE.part, resuming a partial file.
fetch_resumable() {
  local url="$1" out="$2" n=0
  while :; do
    if curl -sS -L --fail --connect-timeout 15 --max-time 3600 -C - -o "$out.part" "$url" 2>"$STATE/curl.err"; then mv "$out.part" "$out"; return 0; fi
    # 416: the .part is already the whole file.
    if grep -q '416' "$STATE/curl.err" 2>/dev/null && [[ -s "$out.part" ]]; then mv "$out.part" "$out"; return 0; fi
    n=$((n + 1)); [[ $n -ge $TRIES ]] && { log "download of $url failed: $(tail -n1 "$STATE/curl.err")"; return 1; }
    nap $(( (BACKOFF_BASE ** n) + $(rand "$BACKOFF_BASE") ))
  done
}

verify_sig() { # FILE (FILE.sig beside it)
  [[ -r "$PUBKEY" ]] || { log "no release public key at $PUBKEY"; return 1; }
  openssl pkeyutl -verify -pubin -inkey "$PUBKEY" -rawin -in "$1" -sigfile "$1.sig" >/dev/null 2>&1
}

# ---------------------------------------------------------------------------------------------------- rollout
node_labels_match() { # "k=v" must be one of LABELS
  local want="$1" l; for l in $LABELS; do [[ "$l" == "$want" ]] && return 0; done; return 1
}
bucket() { # SALT -> 0..9999, stable per (salt, node)
  local h; h="$(printf '%s%s' "$1" "$NODE_ID" | sha256sum | cut -c1-8)"; echo $(( 16#$h % 10000 ))
}
in_wave() { # CHANNEL_JSON -> 0 when this node may install
  local j="$1" pct salt b l
  pct="$(jq -r '.rollout.percent' <<<"$j")"; salt="$(jq -r '.rollout.salt' <<<"$j")"
  while IFS= read -r l; do [[ -n "$l" ]] && ! node_labels_match "$l" && { WHY="missing required label $l"; return 1; }; done < <(jq -r '.rollout.require_labels[]?' <<<"$j")
  while IFS= read -r l; do [[ -n "$l" ]] && node_labels_match "$l" && { WHY="excluded by label $l"; return 1; }; done < <(jq -r '.rollout.exclude_labels[]?' <<<"$j")
  b="$(bucket "$salt")"
  if [[ $b -lt $((pct * 100)) ]]; then return 0; fi
  WHY="bucket $b is outside the $pct% wave"; return 1
}

# --------------------------------------------------------------------------------------------------- actions
arch() { case "$(uname -m)" in x86_64) echo x86_64 ;; aarch64|arm64) echo aarch64 ;; *) echo unsupported ;; esac; }
failed_has() { grep -qxF "$1" "$STATE/failed" 2>/dev/null; }

# A node whose service is not enabled (a fresh install, a container) has nothing to restart; that is not a failure.
service_restart() {
  [[ -n "$ROOT" ]] && return 0
  command -v systemctl >/dev/null 2>&1 || return 0
  systemctl daemon-reload >/dev/null 2>&1
  if systemctl is-enabled --quiet fluxvm.service 2>/dev/null; then systemctl restart fluxvm.service; fi
}
healthy() { # VERSION: the new binary reports it, and the service (when enabled) came up
  local want="$1" i
  for ((i = 0; i < HEALTH_WAIT; i++)); do
    if [[ "$("$BIN_DIR/fluxctl" --version 2>/dev/null)" == *"$want"* ]]; then
      if [[ -n "$ROOT" ]] || ! systemctl is-enabled --quiet fluxvm.service 2>/dev/null || systemctl is-active --quiet fluxvm.service; then return 0; fi
    fi
    nap 1
  done
  return 1
}

# Atomic flip: build the new link beside the old one and rename over it (GNU mv -T; plain ln as a fallback).
flip_current() {
  ln -sfn "$1" "$OPT/.current.new"
  if ! mv -T "$OPT/.current.new" "$OPT/current" 2>/dev/null; then rm -f "$OPT/current.new" "$OPT/current"; ln -sfn "$1" "$OPT/current"; fi
}
install_tarball() { # FILE VERSION
  local f="$1" v="$2" tmp="$OPT/.incoming-$2"
  rm -rf "$tmp"; mkdir -p "$tmp" "$BIN_DIR" || return 1
  tar -xzf "$f" -C "$tmp" || return 1
  local dir; dir="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  [[ -d "$dir/bin" ]] || { log "the tarball has no bin/"; return 1; }
  if [[ -f "$dir/MANIFEST.sha256" ]]; then ( cd "$dir" && sha256sum -c --quiet MANIFEST.sha256 ) || { log "MANIFEST.sha256 does not match the unpacked files"; return 1; }; fi
  rm -rf "${OPT:?}/$v"; mv "$dir" "$OPT/$v"; rm -rf "$tmp"
  flip_current "$v"
  local b; for b in "$OPT/current/bin/"*; do ln -sfn "$OPT/current/bin/$(basename "$b")" "$BIN_DIR/$(basename "$b")"; done
  if [[ -z "$ROOT" && -d "$OPT/current/share/systemd" ]]; then
    local u; for u in "$OPT/current/share/systemd/"*.service; do install -m 0644 "$u" "/etc/systemd/system/$(basename "$u")"; done
  fi
  if [[ -z "$ROOT" && -d "$OPT/current/lib/bpf" ]]; then mkdir -p /usr/lib/fluxvm && ln -sfn "$OPT/current/lib/bpf" /usr/lib/fluxvm/bpf; fi
}
install_deb() { # FILE
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Options::=--force-confold -o Dpkg::Options::=--force-confdef "$1" >"$STATE/install.log" 2>&1
}
install_rpm() { # FILE [--downgrade]
  # dnf resolves the dependencies and keeps edited config files (the package marks them noreplace).
  if [[ "${2:-}" == --downgrade ]]; then rpm -Uvh --oldpackage --replacepkgs "$1" >"$STATE/install.log" 2>&1
  elif command -v dnf >/dev/null 2>&1; then dnf install -y -q "$1" >"$STATE/install.log" 2>&1
  else yum install -y -q "$1" >"$STATE/install.log" 2>&1; fi
}
install_artifact() { # KIND FILE VERSION
  case "$1" in deb) install_deb "$2" ;; rpm) install_rpm "$2" ;; *) install_tarball "$2" "$3" ;; esac
}

# Keep the newest two packages (the one we run and the one to roll back to) and the manifests of the versions still cached.
prune_cache() {
  # shellcheck disable=SC2012
  ls -t "$STATE/cache/"*.deb "$STATE/cache/"*.rpm "$STATE/cache/"*.tar.gz 2>/dev/null | tail -n +3 | xargs -r rm -f
  local m v
  for m in "$STATE/cache/"manifest-*.json "$STATE/cache/"manifest-*.json.sig; do
    [[ -e "$m" ]] || continue
    v="${m##*/manifest-}"; v="${v%.sig}"; v="${v%.json}"
    compgen -G "$STATE/cache/*[_-]${v}[_-]*" >/dev/null || rm -f "$m"
  done
}

preflight() { # SIZE
  local need=$(( $1 * 3 / 1024 )) free
  free="$(df -Pk "$STATE/cache" | awk 'NR==2{print $4}')"
  [[ "$free" -ge "$need" ]] || { WHY="not enough disk: need ${need} KiB, have ${free} KiB"; return 1; }
  if [[ -z "$ROOT" && ! -e /dev/kvm && "${SKIP_KVM_CHECK:-0}" != 1 ]]; then WHY="/dev/kvm is missing: this node cannot run VMs"; return 1; fi
  return 0
}

do_rollback() { # reason
  local prev kind v
  [[ -f "$STATE/previous" ]] || { log "no previous version to go back to"; return 1; }
  IFS=' ' read -r kind v prev < "$STATE/previous"
  log "rolling back to $v"
  if [[ "$kind" == rpm ]]; then
    [[ -f "$prev" ]] || { log "the previous package $prev is gone"; return 1; }
    install_rpm "$prev" --downgrade || return 1
  elif [[ "$kind" == deb ]]; then
    [[ -f "$prev" ]] || { log "the previous package $prev is gone"; return 1; }
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --allow-downgrades -o Dpkg::Options::=--force-confold "$prev" >"$STATE/rollback.log" 2>&1 || return 1
  else
    [[ -d "$OPT/$v" ]] || { log "the previous tree $OPT/$v is gone"; return 1; }
    flip_current "$v"
    local b; for b in "$OPT/current/bin/"*; do ln -sfn "$OPT/current/bin/$(basename "$b")" "$BIN_DIR/$(basename "$b")"; done
  fi
  service_restart
}

# Start-up delay: half of JITTER_MAX is fixed per node (so a fleet spreads the same way every run), the other half
# is random. Always between 0 and JITTER_MAX.
jitter_seconds() {
  local stable=$(( 16#$(printf '%s' "$NODE_ID" | sha256sum | cut -c1-6) % (JITTER_MAX / 2 + 1) ))
  echo $(( stable + $(rand $((JITTER_MAX / 2))) ))
}

cmd_install() { # [--dry-run] [--force]
  local dry=0 force=0 a
  for a in "$@"; do case "$a" in --dry-run) dry=1 ;; --force) force=1 ;; esac; done
  [[ "$AUTO" == 0 ]] && dry=1
  [[ -n "$MIRROR_URL" ]] || die "MIRROR_URL is not set (see $CONF)"
  [[ -n "$NODE_ID" ]] || die "no NODE_ID and no /etc/machine-id"
  for t in curl openssl jq sha256sum flock; do command -v "$t" >/dev/null || die "$t is required"; done
  [[ "$(arch)" != unsupported ]] || die "unsupported architecture $(uname -m)"
  mkdir -p "$OPT" 2>/dev/null

  exec 9>"$STATE/lock"
  flock -n 9 || { log "another run holds the lock"; exit 0; }

  if [[ "$JITTER_MAX" -gt 0 && $force -eq 0 && $dry -eq 0 ]]; then nap "$(jitter_seconds)"; fi

  local base="${MIRROR_URL%/}" chf="$STATE/channel.json" new="$STATE/channel.new.json" rc
  fetch "$base/channels/$CHANNEL.json" "$new" "$STATE/channel.etag"; rc=$?
  if [[ $rc -eq 3 ]]; then
    [[ -f "$chf" ]] || die "mirror said not modified but there is no cached channel file"
    cp "$chf" "$new"; cp "$chf.sig" "$new.sig"
  elif [[ $rc -ne 0 ]]; then die "could not fetch the channel file from $base"
  else
    fetch "$base/channels/$CHANNEL.json.sig" "$new.sig" || die "could not fetch the channel signature"
  fi
  # The signature is checked before the file is parsed or trusted for anything.
  verify_sig "$new" || { rm -f "$new" "$new.sig" "$STATE/channel.etag"; die "BAD signature on the channel file; ignoring it"; }
  local cj; cj="$(cat "$new")"
  jq -e '.schema == 1 and (.serial|type=="number") and (.expires|type=="number") and (.version|type=="string") and (.manifest|type=="string")' <<<"$cj" >/dev/null \
    || die "the channel file is malformed"
  [[ "$(jq -r .channel <<<"$cj")" == "$CHANNEL" ]] || die "the channel file is for another channel"
  local serial expires version last
  serial="$(jq -r .serial <<<"$cj")"; expires="$(jq -r .expires <<<"$cj")"; version="$(jq -r .version <<<"$cj")"
  last="$(cat "$STATE/serial" 2>/dev/null || echo 0)"
  [[ "$expires" -gt "$(now)" ]] || die "the channel file expired"
  [[ "$serial" -ge "$last" ]] || die "channel serial $serial is older than the accepted $last (rollback attack or stale mirror)"
  [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || die "bad version in the channel file"
  mv "$new" "$chf"; mv "$new.sig" "$chf.sig"; echo "$serial" > "$STATE/serial"

  CUR_FIELDS="$(jq -n --arg v "$version" '{target: $v}')"
  local cur; cur="$(current_version || true)"
  if [[ -f "$STATE/pin" && $force -eq 0 ]]; then log "pinned to $(cat "$STATE/pin")"; set_status pinned "pinned to $(cat "$STATE/pin")"; return 0; fi
  if [[ -n "$cur" && "$cur" == "$version" ]]; then log "already on $version"; set_status unchanged "on $version"; return 0; fi
  if jq -e --arg v "$version" '.revoked | index($v)' <<<"$cj" >/dev/null; then die "$version is revoked in the channel file"; fi
  if jq -e --arg c "${cur:-}" '(.revoked | index($c)) != null and $c != ""' <<<"$cj" >/dev/null; then log "installed $cur is revoked; moving off it"; fi
  failed_has "$version" && { log "$version failed on this node before; not retrying"; set_status skipped "$version failed here before"; return 0; }
  local min; min="$(jq -r '.min_upgrade_from // empty' <<<"$cj")"
  if [[ -n "$min" && -n "$cur" ]] && ! version_le "$min" "$cur"; then die "installed $cur is older than min_upgrade_from $min; upgrade through an intermediate version"; fi
  WHY=""
  if [[ $force -eq 0 ]] && ! in_wave "$cj"; then log "not in the wave yet: $WHY"; set_status waiting "$WHY"; return 0; fi
  if [[ $dry -eq 1 ]]; then log "would install $version (installed: ${cur:-none})"; set_status due "would install $version"; return 0; fi

  # The release manifest: its hash is in the signed channel file, and it is signed itself.
  local mf="$STATE/cache/manifest-$version.json"
  local mpath; mpath="$(jq -r .manifest <<<"$cj")"
  fetch "$base/$mpath" "$mf" || die "could not fetch the release manifest"
  fetch "$base/$mpath.sig" "$mf.sig" || die "could not fetch the release manifest signature"
  verify_sig "$mf" || { rm -f "$mf" "$mf.sig"; die "BAD signature on the release manifest"; }
  [[ "$(sha256 "$mf")" == "$(jq -r .manifest_sha256 <<<"$cj")" ]] || { rm -f "$mf"; die "the release manifest does not match the hash in the channel file"; }
  local a kind f h size
  a="$(arch)"; kind=tarball; [[ "$INSTALL_MODE" == deb || "$INSTALL_MODE" == rpm ]] && kind="$INSTALL_MODE"
  f="$(jq -r --arg a "$a" --arg k "$kind" '.artifacts[$a][$k].file // empty' "$mf")"
  h="$(jq -r --arg a "$a" --arg k "$kind" '.artifacts[$a][$k].sha256 // empty' "$mf")"
  size="$(jq -r --arg a "$a" --arg k "$kind" '.artifacts[$a][$k].size // 0' "$mf")"
  [[ -n "$f" && -n "$h" ]] || die "release $version has no $kind for $a"
  [[ "$f" != */* ]] || die "bad artifact name $f"
  WHY=""; preflight "$size" || die "pre-flight failed: $WHY"

  local pkg="$STATE/cache/$f"
  if [[ ! -f "$pkg" || "$(sha256 "$pkg")" != "$h" ]]; then
    rm -f "$pkg"
    fetch_resumable "$base/releases/$version/$f" "$pkg" || die "download of $f failed"
  fi
  [[ "$(sha256 "$pkg")" == "$h" ]] || { rm -f "$pkg" "$pkg.part"; die "$f does not match the signed sha256; discarded"; }

  # Keep the artifact we are leaving, so a rollback needs no network.
  if [[ -n "$cur" ]]; then
    if [[ "$kind" == deb ]]; then
      local pf; pf="$(find "$STATE/cache" -name "fluxvm_${cur}_*.deb" -print -quit)"
      [[ -n "$pf" ]] && echo "deb $cur $pf" > "$STATE/previous.new"
    elif [[ "$kind" == rpm ]]; then
      local pf; pf="$(find "$STATE/cache" -name "fluxvm-${cur}-*.rpm" -print -quit)"
      [[ -n "$pf" ]] && echo "rpm $cur $pf" > "$STATE/previous.new"
    else echo "tarball $cur -" > "$STATE/previous.new"; fi
    # Remember the pointer we are replacing: when this release fails and we land back on $cur, it is still the right one.
    rm -f "$STATE/previous.prev"; [[ -f "$STATE/previous" ]] && cp "$STATE/previous" "$STATE/previous.prev"
    [[ -f "$STATE/previous.new" ]] && mv "$STATE/previous.new" "$STATE/previous"
  fi

  log "installing $version (was ${cur:-none}, $kind)"
  if install_artifact "$kind" "$pkg" "$version" && service_restart && healthy "$version"; then
    echo "$kind $version $pkg" > "$STATE/last-installed"
    log "now on $version"; set_status updated "installed $version"
    prune_cache
    return 0
  fi
  log "$version did not come up healthy"
  echo "$version" >> "$STATE/failed"
  [[ -n "$cur" ]] && rm -f "$pkg"   # a release that failed is not a rollback target; keep the two before it
  prune_cache
  if [[ -n "$cur" ]] && do_rollback; then
    [[ -f "$STATE/previous.prev" ]] && mv "$STATE/previous.prev" "$STATE/previous"
    set_status rolled-back "$version failed; back on $cur"; return 3
  fi
  set_status failed "$version failed and there was nothing to roll back to"; return 1
}

cmd_rollback() {
  local cur; cur="$(current_version || true)"
  do_rollback || die "rollback failed"
  [[ -n "$cur" ]] && echo "$cur" >> "$STATE/failed"
  set_status rolled-back "manual rollback from ${cur:-?}"
}
cmd_pin() { local v="${1:-$(current_version)}"; [[ -n "$v" ]] || die "nothing installed to pin"; echo "$v" > "$STATE/pin"; set_status pinned "pinned to $v"; }
cmd_unpin() { rm -f "$STATE/pin"; set_status unpinned "pin removed"; }
cmd_status() { if [[ -f "$STATE/status.json" ]]; then jq . "$STATE/status.json"; else echo '{"state":"never-run"}'; fi; }
cmd_verify() {
  if [[ "$INSTALL_MODE" == deb ]]; then dpkg -V fluxvm; return $?; fi
  if [[ "$INSTALL_MODE" == rpm ]]; then rpm -V fluxvm; return $?; fi
  [[ -f "$OPT/current/MANIFEST.sha256" ]] || die "no MANIFEST.sha256 in $OPT/current"
  ( cd "$OPT/current" && sha256sum -c --quiet MANIFEST.sha256 ) && log "installed files match the package manifest"
}

# Sourced by scripts/test-bootstrap.sh for the functions; run directly otherwise.
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  check) shift; cmd_install --dry-run "$@" ;;
  rollback) cmd_rollback ;;
  status) cmd_status ;;
  pin) shift; cmd_pin "$@" ;;
  unpin) cmd_unpin ;;
  verify) cmd_verify ;;
  *) sed -n '9,16p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
# Never read past here: an upgrade can replace this very file (the package ships it) while it is running.
exit $?
fi
