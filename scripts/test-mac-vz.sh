#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Live acceptance test of `fluxctl mac` on a real Mac: every command in docs/macos-containers.md that is only unit-tested
# so far. It imports a template you provide, then runs --rm, detach + wait-ready, exec, ps/inspect --json, cp both ways,
# stop/start, commit, build of a tiny Macfile, the --queue wait, push/pull against a local registry, and prune. It removes
# everything it created (its own temporary image store, instances named acc-*, and a registry container it started).
#
# STATUS: written, NOT yet run on hardware. `--self-test` runs this same script against a fake `fluxctl`
# (no VM, no Mac needed) and is what CI can run; a pass there proves the script's logic, not `fluxctl mac`.
#
# Needs (live run): macOS on Apple silicon, an unlocked login session, a FluxVM daemon on this Mac (set FLUXVM_URL, and
# FLUXVM_TOKEN if it has auth), a prepared macOS template directory (disk.raw, hardware.bin, auxiliary.bin; docs/macos.md)
# whose guest accepts your SSH key for FLUXVM_MAC_USER, and TWO free macOS VM slots (the Mac runs at most 2). TMPDIR and the
# daemon's state directory must be on one APFS volume, or each clone copies 20+ GB (`fluxctl mac doctor` says so).
# Registry: set FLUXVM_E2E_REGISTRY=host:port of a local OCI registry, or have docker so one is started on :15000.
# Without either, the push/pull checks are skipped and listed.
#
# Usage:
#   FLUXCTL=./target/release/fluxctl FLUXVM_MAC_TEMPLATE_DIR=/Volumes/X/macos-15 scripts/test-mac-vz.sh [--skip-queue] [--skip-build] [--keep]
#   scripts/test-mac-vz.sh --self-test
#
# Exit status: 0 every check passed, 1 any failed, 2 a precondition is missing. Skipped checks are listed.
set -uo pipefail

SELF_SCRIPT="$0"
FLUXCTL="${FLUXCTL:-fluxctl}"
TEMPLATE_DIR="${FLUXVM_MAC_TEMPLATE_DIR:-}"
GUEST_USER="${FLUXVM_MAC_USER:-admin}"
REGISTRY="${FLUXVM_E2E_REGISTRY:-}"
BOOT_WAIT="${FLUXVM_E2E_BOOT_WAIT:-900}"        # seconds a clone may take to answer on SSH
QUEUE_WAIT="${FLUXVM_E2E_QUEUE_WAIT:-8}"        # seconds the queued run must still be waiting
QUEUE_TIMEOUT="${FLUXVM_E2E_QUEUE_TIMEOUT:-300}"
IMG="acc-base:1"
DO_QUEUE=1 DO_BUILD=1 KEEP=0 SELF_TEST=0 LIVE=1

while [ $# -gt 0 ]; do
    case "$1" in
        --skip-queue) DO_QUEUE=0 ;;
        --skip-build) DO_BUILD=0 ;;
        --keep) KEEP=1 ;;
        --self-test) SELF_TEST=1 ;;
        -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

# ---- the fake `fluxctl` for --self-test --------------------------------------------------------------------------------
# A directory-backed stand-in: an image and an instance are directories; "the guest's /tmp" is a per-disk directory.
write_fake() {
    cat >"$1" <<'FAKE'
#!/usr/bin/env bash
# Fake fluxctl for scripts/test-mac-vz.sh --self-test. Not a model of the real daemon: just enough to exercise the script.
S="${FAKE_STATE:?}"; mkdir -p "$S/images" "$S/inst"
key() { printf '%s' "$1" | tr '/:' '__'; }
img() { echo "$S/images/$(key "$1")"; }
inst() { echo "$S/inst/$1"; }
fail() { echo "fake: $*" >&2; exit 1; }
active() { local n=0 d; for d in "$S"/inst/*; do [ -f "$d/status" ] && [ "$(cat "$d/status")" = running ] && n=$((n+1)); done; echo "$n"; }
# Run text in a "guest": /tmp/ is the disk's own directory, sw_vers answers like macOS.
guest() { local disk="$1"; shift; local c="${*//\/tmp\//$disk/tmp/}"; mkdir -p "$disk/tmp"
    ( cd "$disk" && bash -c "sw_vers() { if [ \"\$1\" = -productName ]; then echo macOS; else echo 27.0; fi; }; $c" ); }
[ "${1:-}" = mac ] || fail "only mac is faked"; shift
sub="${1:-}"; shift || true
case "$sub" in
doctor) echo "[  ok] fake doctor"; [ -n "${FAKE_DOCTOR_FAIL:-}" ] && { echo "[FAIL] fake"; exit 1; }; exit 0 ;;
import) d="$(img "$2")"; rm -rf "$d"; mkdir -p "$d/tmp"; cp "$1/disk.raw" "$1/hardware.bin" "$1/auxiliary.bin" "$d/" || fail "bad template"; echo "$2" >"$d/.ref"; echo "imported $1 as $2" ;;
images) if [ "${1:-}" = --json ]; then printf '['; sep=''; for d in "$S"/images/*; do [ -f "$d/.ref" ] || continue; printf '%s{"reference":"%s"}' "$sep" "$(cat "$d/.ref")"; sep=','; done; echo ']'
        else for d in "$S"/images/*; do [ -f "$d/.ref" ] && cat "$d/.ref"; done; fi ;;
rmi) d="$(img "$1")"; [ -d "$d" ] || fail "no such image $1"; rm -rf "$d" ;;
tag) cp -R "$(img "$1")" "$(img "$2")"; echo "$2" >"$(img "$2")/.ref" ;;
push) [ -d "$(img "$1")" ] || fail "no such image $1"; mkdir -p "$S/registry"; rm -rf "$S/registry/$(key "$2")"; cp -R "$(img "$1")" "$S/registry/$(key "$2")"; echo "{\"pushed\":\"$2\"}" ;;
pull) [ -d "$S/registry/$(key "$1")" ] || fail "manifest unknown: $1"; rm -rf "$(img "$1")"; cp -R "$S/registry/$(key "$1")" "$(img "$1")"; echo "$1" >"$(img "$1")/.ref"; echo "pulled $1" ;;
run)
    rm=0 detach=0 name="mac-$$" json=0 queue=0 qt=3600
    while [ $# -gt 0 ]; do case "$1" in
        --rm) rm=1 ;; -d) detach=1 ;; --json) json=1 ;; --queue) queue=1 ;; --name) name="$2"; shift ;;
        --queue-timeout) qt="$2"; shift ;; --label|--wait|--cpus|--memory|-e|-p|-v|-w|--user|--restart) shift ;;
        --) shift; break ;; -*) ;; *) break ;; esac; shift; done
    image="$1"; shift; [ "${1:-}" = -- ] && shift
    [ -d "$(img "$image")" ] || fail "no image $image"
    if [ "$(active)" -ge 2 ]; then
        [ "$queue" = 1 ] || fail "no room: this Mac already runs 2 macOS VMs (Apple's licence limit)"
        t=0; while [ "$(active)" -ge 2 ]; do sleep 0.2; t=$((t+1)); [ "$t" -gt $((qt*5)) ] && fail "no macOS slot became free within ${qt}s"; done
    fi
    di="$(inst "$name")"; rm -rf "$di"; mkdir -p "$di"; cp -R "$(img "$image")" "$di/disk"; echo "$image" >"$di/image"; echo running >"$di/status"
    if [ "$FAKE_BREAK" = run ]; then fail "run broken"; fi
    if [ $# -gt 0 ]; then guest "$di/disk" "$*" || { [ $rm = 1 ] && rm -rf "$di"; exit 1; }; fi
    if [ $rm = 1 ]; then rm -rf "$di"; st=removed; rmd=true; else st=running; rmd=false; fi
    if [ $json = 1 ]; then echo "{\"schema_version\":1,\"name\":\"$name\",\"image\":\"$image\",\"status\":\"$st\",\"removed\":$rmd,\"ip\":\"192.168.64.9\"}"
    elif [ $rm = 0 ]; then echo "$name"; fi ;;
wait-ready) name="$1"; shift; json=0; chk=""
    while [ $# -gt 0 ]; do case "$1" in --json) json=1 ;; --check) chk="$2"; shift ;; --timeout|--user) shift ;; esac; shift; done
    [ -f "$(inst "$name")/status" ] || fail "no instance $name"
    [ "$(cat "$(inst "$name")/status")" = running ] || fail "$name is not running"
    [ -z "$chk" ] || guest "$(inst "$name")/disk" "$chk" || fail "check failed"
    if [ $json = 1 ]; then echo "{\"schema_version\":1,\"name\":\"$name\",\"ready\":true,\"ip\":\"192.168.64.9\",\"waited_secs\":0}"; else echo "ready: $name 192.168.64.9"; fi ;;
exec) name="$1"; shift; while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done; shift
    [ "${FAKE_BREAK:-}" = exec ] && fail "exec broken"
    [ "$(cat "$(inst "$name")/status" 2>/dev/null)" = running ] || fail "$name is not running"
    guest "$(inst "$name")/disk" "$*" ;;
ps) json=0 all=0; for a in "$@"; do case "$a" in --json) json=1 ;; -a) all=1 ;; esac; done
    printf '['; sep=''; for d in "$S"/inst/*; do [ -f "$d/status" ] || continue; st="$(cat "$d/status")"; [ $all = 0 ] && [ "$st" = stopped ] && continue
        printf '%s{"schema_version":1,"name":"%s","status":"%s","ip":"192.168.64.9","image":"%s","ports":[],"labels":{"acc":"1"}}' "$sep" "$(basename "$d")" "$st" "$(cat "$d/image")"; sep=','; done; echo ']' ;;
inspect) [ -f "$(inst "$1")/status" ] || fail "no instance $1"
    echo "[{\"schema_version\":1,\"name\":\"$1\",\"state\":{\"status\":\"$(cat "$(inst "$1")/status")\"},\"network\":{\"ip\":\"192.168.64.9\"}}]" ;;
cp) src="$1"; dst="$2"; map() { case "$1" in *:*) n="${1%%:*}"; p="${1#*:}"; echo "$(inst "$n")/disk${p}" ;; *) echo "$1" ;; esac; }
    s="$(map "$src")"; d="$(map "$dst")"; mkdir -p "$(dirname "$d")"; cp "$s" "$d" || fail "cp failed" ;;
stop) [ "$(cat "$(inst "$1")/status" 2>/dev/null)" = running ] || fail "$1 is not running"; echo stopped >"$(inst "$1")/status" ;;
start) [ -f "$(inst "$1")/status" ] || fail "no instance $1"; [ "$(active)" -lt 2 ] || fail "no room: 2 macOS VMs"; echo running >"$(inst "$1")/status" ;;
commit) name="$1"; [ "$(cat "$(inst "$name")/status" 2>/dev/null)" = stopped ] || fail "$name is not stopped"
    rm -rf "$(img "$2")"; cp -R "$(inst "$name")/disk" "$(img "$2")"; echo "$2" >"$(img "$2")/.ref"; echo "committed $name as $2" ;;
build) tag=""; file=""; ctx="."; while [ $# -gt 0 ]; do case "$1" in -t) tag="$2"; shift ;; -f) file="$2"; shift ;; --wait|--build-arg) shift ;; -*) ;; *) ctx="$1" ;; esac; shift; done
    [ -n "$file" ] || file="$ctx/Macfile"; work="$S/build-$$"; rm -rf "$work"
    while IFS= read -r line; do case "$line" in
        FROM\ *) cp -R "$(img "${line#FROM }")" "$work" || fail "no FROM image" ;;
        RUN\ *) guest "$work" "${line#RUN }" || fail "RUN failed: $line" ;;
        COPY\ *) set -- ${line#COPY }; mkdir -p "$work$(dirname "$2")"; cp "$ctx/$1" "$work/${2#/}" || fail "COPY failed" ;;
        esac; done <"$file"
    rm -rf "$(img "$tag")"; mv "$work" "$(img "$tag")"; echo "$tag" >"$(img "$tag")/.ref"; echo "built $tag" ;;
rm) [ -f "$(inst "$1")/status" ] || fail "no instance $1"; rm -rf "$(inst "$1")"; echo "removed $1" ;;
prune) for d in "$S"/inst/*; do [ -f "$d/status" ] && [ "$(cat "$d/status")" = stopped ] && rm -rf "$d"; done; echo pruned ;;
system) echo '{"images":[],"instances":[]}' ;;
*) fail "unknown fake command: $sub" ;;
esac
FAKE
    chmod +x "$1"
}

# ---- self-test: this script, against the fake -------------------------------------------------------------------------
if [ "$SELF_TEST" = 1 ]; then
    ST="$(mktemp -d "${TMPDIR:-/tmp}/test-mac-vz-self.XXXXXX")" || exit 2
    trap 'rm -rf "$ST"' EXIT
    write_fake "$ST/fluxctl"
    mkdir -p "$ST/template"; for f in disk.raw hardware.bin auxiliary.bin; do echo x >"$ST/template/$f"; done
    base_env() { env FAKE_STATE="$ST/state" FAKE_BREAK="${1:-}" FLUXCTL="$ST/fluxctl" FLUXVM_MAC_TEMPLATE_DIR="$ST/template" \
        FLUXVM_E2E_LIVE=0 FLUXVM_E2E_REGISTRY=registry.fake:5000 FLUXVM_E2E_QUEUE_WAIT=1 FLUXVM_E2E_QUEUE_TIMEOUT=30 FLUXVM_E2E_BOOT_WAIT=5 \
        TMPDIR="$ST" "$SELF_SCRIPT" "${@:2}"; }
    rc=0
    echo "== self-test: happy path"
    OUT="$(base_env "" 2>&1)"; R=$?
    echo "$OUT" | tail -8
    if [ $R -ne 0 ]; then echo "SELF-TEST FAIL: happy path exited $R"; echo "$OUT"; rc=1; fi
    echo "$OUT" | grep -q "FAIL" && { echo "SELF-TEST FAIL: a check failed against the fake"; rc=1; }
    N="$(echo "$OUT" | grep -c '^  ok ')"
    [ "$N" -ge 20 ] || { echo "SELF-TEST FAIL: only $N checks passed (want >= 20)"; rc=1; }
    echo "== self-test: a broken exec must be reported as a failure"
    rm -rf "$ST/state"
    OUT="$(base_env exec 2>&1)"; R=$?
    [ $R -eq 1 ] && echo "$OUT" | grep -q "FAIL" || { echo "SELF-TEST FAIL: broken exec gave exit $R without a FAIL line"; rc=1; }
    echo "== self-test: a missing template is a precondition failure (exit 2)"
    OUT="$(env FAKE_STATE="$ST/state" FLUXCTL="$ST/fluxctl" FLUXVM_E2E_LIVE=0 FLUXVM_MAC_TEMPLATE_DIR="$ST/nope" "$SELF_SCRIPT" 2>&1)"; R=$?
    [ $R -eq 2 ] || { echo "SELF-TEST FAIL: missing template gave exit $R, want 2"; rc=1; }
    echo "== self-test: a failing doctor stops the run (exit 2)"
    OUT="$(env FAKE_STATE="$ST/state" FAKE_DOCTOR_FAIL=1 FLUXCTL="$ST/fluxctl" FLUXVM_E2E_LIVE=0 FLUXVM_MAC_TEMPLATE_DIR="$ST/template" "$SELF_SCRIPT" 2>&1)"; R=$?
    [ $R -eq 2 ] || { echo "SELF-TEST FAIL: failing doctor gave exit $R, want 2"; rc=1; }
    [ $rc -eq 0 ] && echo "SELF-TEST PASS (the script's logic only; nothing here ran a VM)"
    exit $rc
fi

# FLUXVM_E2E_LIVE=0 is set by --self-test only: it skips the Apple-silicon check.
[ "${FLUXVM_E2E_LIVE:-1}" = 0 ] && LIVE=0

PASS=() FAIL=() SKIP=()
pass() { PASS+=("$1"); echo "  ok    $1"; }
fail() { FAIL+=("$1: $2"); echo "  FAIL  $1: $2"; }
skip() { SKIP+=("$1: $2"); echo "  skip  $1: $2"; }
LAST=""
# check NAME CMD...: passes when the command exits 0; its combined output is left in $LAST.
check() {
    local name="$1"; shift
    if LAST="$("$@" 2>&1)"; then pass "$name"; else fail "$name" "$(echo "$LAST" | tail -3 | tr '\n' ' ')"; fi
}
# expect NAME PATTERN: passes when $LAST contains PATTERN (fixed string).
expect() {
    if printf '%s' "$LAST" | grep -qF -- "$2"; then pass "$1"; else fail "$1" "expected '$2' in: $(echo "$LAST" | tail -3 | tr '\n' ' ')"; fi
}
mac() { "$FLUXCTL" mac "$@"; }
# jq-free JSON probe: jx 'JSON' 'python expression over d'. Prints the value; nothing (and rc 1) on any error.
jx() { printf '%s' "$1" | python3 -I -c 'import sys,json; d=json.loads(sys.stdin.read()); print(eval(sys.argv[1]))' "$2" 2>/dev/null; }
# jcheck NAME JSON EXPR EXPECTED
jcheck() {
    local got; got="$(jx "$2" "$3")"
    if [ "$got" = "$4" ]; then pass "$1"; else fail "$1" "$3 = '$got', want '$4'"; fi
}

# ---- preconditions ---------------------------------------------------------------------------------------------------
if [ "$LIVE" = 1 ]; then
    [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "needs macOS on Apple silicon" >&2; exit 2; }
fi
command -v "$FLUXCTL" >/dev/null 2>&1 || [ -x "$FLUXCTL" ] || { echo "fluxctl not found (set FLUXCTL)" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "python3 not found (used to read JSON)" >&2; exit 2; }
[ -n "$TEMPLATE_DIR" ] || { echo "set FLUXVM_MAC_TEMPLATE_DIR to a template directory (disk.raw, hardware.bin, auxiliary.bin)" >&2; exit 2; }
for f in disk.raw hardware.bin auxiliary.bin; do
    [ -f "$TEMPLATE_DIR/$f" ] || { echo "$TEMPLATE_DIR/$f is missing" >&2; exit 2; }
done
WORK="$(mktemp -d "${TMPDIR:-/tmp}/test-mac-vz.XXXXXX")" || exit 2
export FLUXVM_MAC_STORE="${FLUXVM_E2E_STORE:-$WORK/store}"
echo "store: $FLUXVM_MAC_STORE   template: $TEMPLATE_DIR   user: $GUEST_USER"
DOCTOR="$(mac doctor 2>&1)"; DRC=$?
if [ $DRC -ne 0 ]; then echo "fluxctl mac doctor reports problems; fix them first:"; echo "$DOCTOR"; rm -rf "$WORK"; exit 2; fi

REG_CONTAINER=""
cleanup() {
    if [ "$KEEP" = 0 ]; then
        echo "cleaning up"
        local n
        for n in acc-1 acc-q1 acc-q2 acc-build; do mac rm "$n" >/dev/null 2>&1; done
        for n in acc-base:1 acc-commit:v1 acc-built:v1 "$REGISTRY/acc/macos:test"; do mac rmi "$n" >/dev/null 2>&1; done
        [ -n "$REG_CONTAINER" ] && docker rm -f "$REG_CONTAINER" >/dev/null 2>&1
        rm -rf "$WORK"
    else
        echo "--keep: left instances acc-*, the store $FLUXVM_MAC_STORE and $WORK"
    fi
}
trap cleanup EXIT

# ---- import ----------------------------------------------------------------------------------------------------------
echo "== import"
check "import: template into the store" mac import "$TEMPLATE_DIR" "$IMG"
check "images --json lists it" mac images --json
expect "images --json names $IMG" "$IMG"

# ---- run --rm ----------------------------------------------------------------------------------------------------------
echo "== run --rm"
check "run --rm: command runs in a fresh guest" mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG" -- sw_vers -productName
expect "run --rm: the guest is macOS" "macOS"
check "run --rm --json" mac run --rm --json --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG" -- true
RUNJ="$(printf '%s\n' "$LAST" | tail -1)"
jcheck "run --json: schema_version is 1" "$RUNJ" 'd["schema_version"]' 1
jcheck "run --json: removed after --rm" "$RUNJ" 'd["removed"]' True
LAST="$(mac ps -a --json 2>&1)"
jcheck "run --rm left no instance behind" "$LAST" 'len([r for r in d if r["image"]=="'"$IMG"'"])' 0

# ---- detached instance: wait-ready, exec, ps/inspect --json, cp --------------------------------------------------------
echo "== detached instance"
check "run -d --label" mac run -d --name acc-1 --label acc=1 --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG"
check "wait-ready --json" mac wait-ready acc-1 --timeout "$BOOT_WAIT" --user "$GUEST_USER" --check 'true' --json
jcheck "wait-ready: ready" "$LAST" 'd["ready"]' True
check "exec: command in the guest" mac exec acc-1 --user "$GUEST_USER" -- 'echo hello-from-guest'
expect "exec: output came back" "hello-from-guest"
check "ps --json" mac ps --json
PSJ="$LAST"
jcheck "ps --json: schema_version" "$PSJ" '[r for r in d if r["name"]=="acc-1"][0]["schema_version"]' 1
jcheck "ps --json: status running" "$PSJ" '[r for r in d if r["name"]=="acc-1"][0]["status"]' running
jcheck "ps --json: has an address" "$PSJ" 'bool([r for r in d if r["name"]=="acc-1"][0]["ip"])' True
jcheck "ps --json: user label" "$PSJ" '[r for r in d if r["name"]=="acc-1"][0]["labels"]["acc"]' 1
check "inspect" mac inspect acc-1
jcheck "inspect: state running" "$LAST" 'd[0]["state"]["status"]' running
jcheck "inspect: schema_version" "$LAST" 'd[0]["schema_version"]' 1
echo "payload-$$" >"$WORK/up.txt"
check "cp: host to guest" mac cp "$WORK/up.txt" "acc-1:/tmp/up.txt"
check "cp: guest to host" mac cp "acc-1:/tmp/up.txt" "$WORK/down.txt"
if cmp -s "$WORK/up.txt" "$WORK/down.txt"; then pass "cp: round trip is identical"; else fail "cp round trip" "files differ"; fi

# ---- stop / start keep the disk; commit --------------------------------------------------------------------------------
echo "== stop / start / commit"
check "exec: write a marker" mac exec acc-1 --user "$GUEST_USER" -- 'echo marker-$$ > /tmp/acc-marker; cat /tmp/acc-marker'
MARK="$(printf '%s' "$LAST" | tail -1)"
check "stop" mac stop acc-1
LAST="$(mac ps -a --json 2>&1)"
jcheck "ps -a: stopped" "$LAST" '[r for r in d if r["name"]=="acc-1"][0]["status"]' stopped
check "start" mac start acc-1
check "wait-ready after start" mac wait-ready acc-1 --timeout "$BOOT_WAIT" --user "$GUEST_USER"
check "exec: the marker survived stop/start" mac exec acc-1 --user "$GUEST_USER" -- 'cat /tmp/acc-marker'
expect "marker content" "$MARK"
check "stop before commit" mac stop acc-1
check "commit" mac commit acc-1 acc-commit:v1 -m "acceptance"
check "images: committed image listed" mac images --json
expect "committed image name" "acc-commit:v1"
check "run --rm from the committed image sees the marker" mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" acc-commit:v1 -- 'cat /tmp/acc-marker'
expect "marker is in the committed image" "$MARK"
check "rm the instance" mac rm acc-1

# ---- build ---------------------------------------------------------------------------------------------------------------
if [ "$DO_BUILD" = 1 ]; then
    echo "== build"
    mkdir -p "$WORK/ctx"
    echo "from-context-$$" >"$WORK/ctx/hello.txt"
    printf 'FROM %s\nUSER %s\nRUN echo built-ok > /tmp/built-marker\nCOPY hello.txt /tmp/hello.txt\n' "$IMG" "$GUEST_USER" >"$WORK/ctx/Macfile"
    check "build --dry-run (client side)" mac build --dry-run -t acc-built:v1 "$WORK/ctx"
    check "build: tiny Macfile" mac build -t acc-built:v1 --wait "$BOOT_WAIT" "$WORK/ctx"
    check "run --rm built image: RUN step took effect" mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" acc-built:v1 -- 'cat /tmp/built-marker'
    expect "RUN output" "built-ok"
    check "run --rm built image: COPY step took effect" mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" acc-built:v1 -- 'cat /tmp/hello.txt'
    expect "COPY content" "from-context-$$"
else
    skip "build" "--skip-build"
fi

# ---- queue: both slots taken --------------------------------------------------------------------------------------------
if [ "$DO_QUEUE" = 1 ]; then
    echo "== queue"
    check "fill slot 1" mac run -d --name acc-q1 --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG"
    check "fill slot 2" mac run -d --name acc-q2 --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG"
    LAST="$(mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG" -- true 2>&1)"; RC=$?
    if [ $RC -ne 0 ]; then pass "run without --queue fails when both slots are taken"; else fail "no-queue" "expected a failure"; fi
    expect "the failure names the 2 VM limit" "2 macOS VM"
    mac run --rm --queue --queue-timeout "$QUEUE_TIMEOUT" --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG" -- 'echo queued-ran' >"$WORK/queued.out" 2>&1 &
    QPID=$!
    sleep "$QUEUE_WAIT"
    if kill -0 "$QPID" 2>/dev/null; then pass "run --queue is still waiting while both slots are taken"; else fail "queue" "the queued run ended early: $(tail -2 "$WORK/queued.out" | tr '\n' ' ')"; fi
    mac rm acc-q1 >/dev/null 2>&1
    wait "$QPID"; QRC=$?
    if [ $QRC -eq 0 ] && grep -q "queued-ran" "$WORK/queued.out"; then pass "run --queue ran once a slot was freed"
    else fail "queue" "exit $QRC: $(tail -3 "$WORK/queued.out" | tr '\n' ' ')"; fi
    mac rm acc-q2 >/dev/null 2>&1
else
    skip "queue" "--skip-queue"
fi

# ---- push / pull against a local registry -------------------------------------------------------------------------------
echo "== registry"
if [ -z "$REGISTRY" ] && command -v docker >/dev/null 2>&1; then
    REG_CONTAINER="fluxvm-acc-registry-$$"
    if docker run -d --rm --name "$REG_CONTAINER" -p 127.0.0.1:15000:5000 registry:2 >/dev/null 2>&1; then
        REGISTRY="localhost:15000"; sleep 2
    else REG_CONTAINER=""; fi
fi
if [ -n "$REGISTRY" ]; then
    REMOTE="$REGISTRY/acc/macos:test"
    check "push to $REGISTRY" mac push "$IMG" "$REMOTE"
    # `push` does not add the remote name locally, so the pull below adds a new name (and a new directory, or the same one
    # when the digest is already stored).
    check "pull it back" mac pull "$REMOTE"
    check "images --json after pull" mac images --json
    expect "pulled image is listed" "$REMOTE"
    check "run --rm from the pulled image" mac run --rm --wait "$BOOT_WAIT" --user "$GUEST_USER" "$REMOTE" -- sw_vers -productName
    expect "pulled image boots macOS" "macOS"
else
    skip "push/pull" "no registry: set FLUXVM_E2E_REGISTRY=host:port, or install docker"
fi

# ---- prune ----------------------------------------------------------------------------------------------------------------
echo "== prune"
check "stopped instance to prune" mac run -d --name acc-1 --wait "$BOOT_WAIT" --user "$GUEST_USER" "$IMG"
check "stop it" mac stop acc-1
check "prune -f" mac prune -f
LAST="$(mac ps -a --json 2>&1)"
jcheck "prune removed the stopped instance" "$LAST" 'len([r for r in d if r["name"]=="acc-1"])' 0
check "system df --json" mac system df --json
jcheck "system df is JSON" "$LAST" 'type(d).__name__' dict
check "doctor still green at the end" mac doctor

# ---- summary ----------------------------------------------------------------------------------------------------------------
echo
echo "passed ${#PASS[@]}, failed ${#FAIL[@]}, skipped ${#SKIP[@]}"
i=0; while [ $i -lt ${#SKIP[@]} ]; do echo "  skipped: ${SKIP[$i]}"; i=$((i+1)); done
if [ ${#FAIL[@]} -gt 0 ]; then
    i=0; while [ $i -lt ${#FAIL[@]} ]; do echo "  FAILED: ${FAIL[$i]}"; i=$((i+1)); done
    exit 1
fi
echo "PASS"
