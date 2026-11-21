#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Hardware acceptance of `fluxctl kube apple`: everything docs/apple-k3s.md says was only unit-tested against a fake host.
# Boots a throwaway cluster of persistent ARM64 Linux guests on Apple Virtualization, drives the real lifecycle (create, join,
# status, persist, etcd snapshots and the schedule, token rotation, failover with three servers, node removal, rebuild of a lost
# journal, the tunnel LaunchAgent, delete) and removes it again.
#
# Needs: macOS on Apple Silicon, an unlocked login session, a running FluxVM daemon (`fluxctl doctor` green), kubectl, an official
# k3s-arm64 release binary with its sha256sum-arm64.txt, and about 3 x 4 GiB RAM / 3 x 30 GiB disk with --servers 3. It uses its
# own cluster name and API port range (eight ports from --api-port) and never touches another cluster.
#
# Usage:
#   FLUXCTL=./target/release/fluxctl K3S_BINARY=./k3s-arm64 K3S_SHA256_FILE=./sha256sum-arm64.txt \
#     ./scripts/test-kube-apple.sh [--servers 1|3] [--agent] [--s3] [--keep] [--yes]
#   ./scripts/test-kube-apple.sh --self-test        # offline: runs this script against fake fluxctl and kubectl; no Mac needed
#
#   --servers N   control-plane nodes (default 3: the HA, failover and server-removal checks run only when N > 1)
#   --agent       also install, check and remove the tunnel LaunchAgent (label dev.zyvor.fluxvm.k3s-tunnel.NAME)
#   --s3          also point the snapshot schedule at S3 (S3_ENDPOINT, S3_BUCKET and S3_CREDENTIALS_FILE, a mode-600 file, or
#                 AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY in the environment; never passed as arguments)
#   --keep        leave the cluster in place at the end
#   --yes         required: this creates and deletes VMs
#
# Exit status: 0 when every check passed, 1 when any failed, 2 when a precondition is missing. Skipped checks are listed.
set -uo pipefail

FLUXCTL="${FLUXCTL:-fluxctl}"
KUBECTL="${KUBECTL:-kubectl}"
NAME="${FLUXVM_E2E_NAME:-k3s-accept}"
API_PORT="${FLUXVM_E2E_API_PORT:-16543}"
SERVERS=3 DO_AGENT=0 DO_S3=0 KEEP=0 YES=0 SELF_TEST=0

while [ $# -gt 0 ]; do
    case "$1" in
        --servers) SERVERS="${2:-}"; shift ;;
        --agent) DO_AGENT=1 ;;
        --s3) DO_S3=1 ;;
        --keep) KEEP=1 ;;
        --yes) YES=1 ;;
        --self-test) SELF_TEST=1 ;;
        -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done
case "$SERVERS" in 1|3) ;; *) echo "--servers must be 1 or 3" >&2; exit 2 ;; esac

# ---- offline self-test -------------------------------------------------------------------------------------------------
# Runs the real script against a fake fluxctl and kubectl and checks what it did: that it ran the phases in order, that a
# failing command is reported and fails the run, that a missing precondition exits 2, that destructive runs need --yes, and
# that the S3 secret never reaches an argument list.
if [ "$SELF_TEST" = 1 ]; then
    SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
    T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
    mkdir -p "$T/bin" "$T/home"
    FAILS=0
    ok() { echo "  ok    $1"; }
    bad() { echo "  FAIL  $1"; FAILS=$((FAILS + 1)); }
    expect() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

    # The fake fluxctl logs its arguments and answers like the real one. It exits 1 for any call whose arguments contain
    # $FAKE_FAIL_ON, and prints "ok    daemon" for `doctor`.
    cat > "$T/bin/fluxctl" <<'SH'
#!/bin/sh
echo "$*" >> "$FAKE_LOG"
case "$*" in
  *"$FAKE_FAIL_ON"*) [ -n "$FAKE_FAIL_ON" ] && { echo "boom" >&2; exit 1; } ;;
esac
case "$*" in
  *"kube apple create"*) n="$(echo "$*" | sed 's/.*kube apple create \([^ ]*\).*/\1/')"; echo '{}' > "$FLUXVM_APPLE_K3S_HOME/$n.json" ;;
  *"kube apple rebuild"*"--accept-host-keys"*) n="$(echo "$*" | sed 's/.*kube apple rebuild \([^ ]*\).*/\1/')"; echo '{}' > "$FLUXVM_APPLE_K3S_HOME/$n.json" ;;
esac
case "$*" in
  "doctor") echo "ok    daemon                 running"; echo "ok    login session          unlocked" ;;
  *"kube apple status"*) grep -q "kube apple create" "$FAKE_LOG" || exit 1; echo '{"healthy": true, "problems": []}' ;;
  *"kube apple doctor"*) echo "ok    quorum             all servers Ready" ;;
  *"kube apple list"*) echo '[{"name": "k3s-accept", "phase": "ready"}]' ;;
  *"kube apple snapshot ls"*) echo "fluxvm-accept-1  1MiB" ;;
  *"kube apple snapshot schedule"*"--show"*) echo '{"servers": [{"newest_age_secs": 5}]}' ;;
  *"kube apple tunnel"*"--agent-status"*) echo '{"installed": true, "loaded": true}' ;;
  *"kube apple rebuild"*"--accept-host-keys"*) echo '{"written": true}' ;;
  *"kube apple rebuild"*) echo '{"written": false}' ;;
  *"kube apple token rotate"*) echo '{"agent_secret_rotated": true}' ;;
esac
exit 0
SH
    cat > "$T/bin/kubectl" <<'SH'
#!/bin/sh
echo "kubectl $*" >> "$FAKE_LOG"
case "$*" in *"get nodes"*) printf 'node-1 Ready control-plane\nnode-2 Ready control-plane\nnode-3 Ready control-plane\nnode-4 Ready <none>\nnode-5 Ready <none>\n' ;; esac
exit 0
SH
    chmod +x "$T/bin/"*
    printf 'k3s' > "$T/k3s-arm64"
    printf '%s  k3s-arm64\n' "$(printf 'k3s' | shasum -a 256 | cut -d' ' -f1)" > "$T/sums.txt"
    printf 'access_key=AKIASELFTESTKEY\nsecret_key=SELFTESTSECRETVALUE\n' > "$T/s3creds"; chmod 600 "$T/s3creds"

    run() {
        : > "$T/log"
        FAKE_LOG="$T/log" PATH="$T/bin:$PATH" FLUXCTL="$T/bin/fluxctl" KUBECTL="$T/bin/kubectl" KUBE_APPLE_SELFTEST=1 \
        FLUXVM_APPLE_K3S_HOME="$T/home" K3S_BINARY="${OVERRIDE_BIN:-$T/k3s-arm64}" K3S_SHA256_FILE="$T/sums.txt" \
        S3_ENDPOINT=s3.example:9000 S3_BUCKET=accept-snaps S3_CREDENTIALS_FILE="$T/s3creds" \
        "$SELF" "$@" 2>&1
    }
    first_line() { grep -n -- "$1" "$T/log" | head -1 | cut -d: -f1; }

    OUT="$(run --yes --agent --s3)"; RC=$?
    expect "a full run against healthy fakes: exit 0 and no failure" '[ "$RC" -eq 0 ] && [[ "$OUT" != *"  FAIL  "* ]]'
    for phase in "kube apple create" "kube apple join" "kube apple persist cycle" "kube apple snapshot save" \
                 "kube apple snapshot schedule" "kube apple token rotate" "kube apple remove" "kube apple rebuild" \
                 "kube apple doctor" "kube apple delete"; do
        expect "phase ran: $phase" '[ -n "$(first_line "$phase")" ]'
    done
    expect "create comes first, delete comes after the rebuild" '[ "$(first_line "kube apple create")" -lt "$(first_line "kube apple join")" ] && [ "$(grep -n "kube apple delete" "$T/log" | tail -1 | cut -d: -f1)" -gt "$(first_line "kube apple rebuild")" ]'
    expect "the agent phase installs, checks and removes the LaunchAgent" 'grep -q -- "--install-agent" "$T/log" && grep -q -- "--agent-status" "$T/log" && grep -q -- "--remove-agent" "$T/log"'
    expect "the S3 secret and key are not in any argument" '! grep -q "SELFTESTSECRETVALUE\|AKIASELFTESTKEY" "$T/log"'
    expect "the S3 credentials file is passed by path" 'grep -q -- "--s3-credentials-file $T/s3creds" "$T/log"'
    expect "three servers: the failover check stops a server's VM" 'grep -q "^stop k3s-k3s-accept-s2" "$T/log"'
    expect "the summary counts the checks" '[[ "$OUT" == *"passed"* ]]'

    OUT="$(run --yes --servers 1)"; RC=$?
    expect "one server: exit 0, HA checks skipped and said so" '[ "$RC" -eq 0 ] && [[ "$OUT" == *"skip  failover"* ]] && ! grep -q "^stop " "$T/log"'

    OUT="$(FAKE_FAIL_ON="snapshot save" run --yes)"; RC=$?
    expect "a failing command is reported and fails the run" '[ "$RC" -eq 1 ] && [[ "$OUT" == *"FAIL  snapshot save"* ]]'
    expect "a failed run still deletes the cluster it made" 'grep -q "kube apple delete" "$T/log"'

    OUT="$(run)"; RC=$?
    expect "without --yes nothing is created" '[ "$RC" -eq 2 ] && [ ! -s "$T/log" ]'

    OUT="$(OVERRIDE_BIN=/nonexistent run --yes)"; RC=$?
    expect "a missing k3s binary exits 2 before anything runs" '[ "$RC" -eq 2 ] && [ ! -s "$T/log" ]'

    OUT="$(FAKE_FAIL_ON="doctor" run --yes)"; RC=$?
    expect "an unhealthy daemon exits 2" '[ "$RC" -eq 2 ]'

    OUT="$("$SELF" --servers 2 2>&1)"; RC=$?
    expect "an unsupported server count is refused" '[ "$RC" -eq 2 ]'

    echo
    if [ "$FAILS" -eq 0 ]; then echo "self-test: all passed"; exit 0; fi
    echo "self-test: $FAILS failed"; exit 1
fi

# ---- the acceptance run ------------------------------------------------------------------------------------------------
PASS=() FAIL=() SKIP=()
pass() { PASS+=("$1"); echo "  ok    $1"; }
fail() { FAIL+=("$1: $2"); echo "  FAIL  $1: $2"; }
skip() { SKIP+=("$1: $2"); echo "  skip  $1: $2"; }
# check NAME CMD...: passes when the command exits 0, and shows its output when it does not.
check() {
    local name="$1"; shift
    local out
    if out="$("$@" 2>&1)"; then pass "$name"; else fail "$name" "$(echo "$out" | tail -3 | tr '\n' ' ')"; fi
}
# check_out NAME PATTERN CMD...: passes when the command succeeds and its output matches the pattern.
check_out() {
    local name="$1" pat="$2"; shift 2
    local out
    if out="$("$@" 2>&1)" && echo "$out" | grep -qE -- "$pat"; then pass "$name"; else fail "$name" "$(echo "$out" | tail -3 | tr '\n' ' ')"; fi
}
KA() { "$FLUXCTL" kube apple "$@"; }
# kubectl through the tunnel: the kubeconfig `create` wrote.
KCFG="${HOME}/.kube/fluxvm-apple-${NAME}.yaml"
kc() { "$KUBECTL" --kubeconfig "$KCFG" "$@"; }
wait_for() { # wait_for SECONDS CMD...
    local n="$1"; shift
    while [ "$n" -gt 0 ]; do "$@" >/dev/null 2>&1 && return 0; sleep 2; n=$((n - 2)); done
    return 1
}
# At least N nodes Ready (a node still being removed must not fail a wait for the others).
nodes_ready() { local want="$1" got; got="$(kc get nodes --no-headers 2>/dev/null | awk '$2=="Ready"' | wc -l | tr -d ' ')"; [ "$got" -ge "$want" ]; }
node_absent() { ! kc get nodes --no-headers 2>/dev/null | awk '{print $1}' | grep -qx "$1"; }

# ---- preconditions -----------------------------------------------------------------------------------------------------
if [ "$YES" != 1 ]; then echo "this creates and deletes VMs on this Mac: add --yes" >&2; exit 2; fi
if [ -z "${KUBE_APPLE_SELFTEST:-}" ]; then
    [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "needs macOS on Apple Silicon" >&2; exit 2; }
fi
command -v "$FLUXCTL" >/dev/null 2>&1 || [ -x "$FLUXCTL" ] || { echo "fluxctl not found (set FLUXCTL)" >&2; exit 2; }
command -v "$KUBECTL" >/dev/null 2>&1 || [ -x "$KUBECTL" ] || { echo "kubectl not found" >&2; exit 2; }
[ -f "${K3S_BINARY:-}" ] || { echo "set K3S_BINARY to an official k3s-arm64 release binary" >&2; exit 2; }
[ -f "${K3S_SHA256_FILE:-}" ] || { echo "set K3S_SHA256_FILE to the release's sha256sum-arm64.txt" >&2; exit 2; }
if [ "$DO_S3" = 1 ]; then
    [ -n "${S3_ENDPOINT:-}" ] && [ -n "${S3_BUCKET:-}" ] || { echo "--s3 needs S3_ENDPOINT and S3_BUCKET" >&2; exit 2; }
    if [ -n "${S3_CREDENTIALS_FILE:-}" ]; then
        [ -f "$S3_CREDENTIALS_FILE" ] || { echo "S3_CREDENTIALS_FILE does not exist" >&2; exit 2; }
    elif [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
        echo "--s3 needs S3_CREDENTIALS_FILE (mode 600) or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY" >&2; exit 2
    fi
fi
DOCTOR="$("$FLUXCTL" doctor 2>&1)" || { echo "fluxctl doctor failed:"; echo "$DOCTOR"; exit 2; }
echo "$DOCTOR" | grep -q "^ok    daemon" || { echo "the FluxVM daemon is not healthy:"; echo "$DOCTOR"; exit 2; }
if echo "$DOCTOR" | grep -qE "^warn  login session"; then echo "the login session is locked: unlock the Mac first"; exit 2; fi
if KA status "$NAME" >/dev/null 2>&1; then echo "a cluster named $NAME already exists; set FLUXVM_E2E_NAME" >&2; exit 2; fi

CREATED=0
cleanup() {
    if [ "$CREATED" = 1 ] && [ "$KEEP" != 1 ]; then
        echo "-- deleting $NAME"
        KA tunnel "$NAME" --stop >/dev/null 2>&1 || true
        [ "$DO_AGENT" = 1 ] && KA tunnel "$NAME" --remove-agent >/dev/null 2>&1 || true
        KA delete "$NAME" --yes >/dev/null 2>&1 || KA delete "$NAME" --yes 2>&1 | tail -3
    fi
}
trap cleanup EXIT

SNAP_ARGS=(--cron "*/5 * * * *" --retention 3 --compress)
BIN_ARGS=(--binary "$K3S_BINARY" --sha256-file "$K3S_SHA256_FILE")

# ---- 1. create ---------------------------------------------------------------------------------------------------------
echo "-- create ($SERVERS server(s))"
CREATED=1
check "create" KA create "$NAME" --embedded-etcd --servers "$SERVERS" --api-port "$API_PORT" "${BIN_ARGS[@]}"
check "tunnel up" KA tunnel "$NAME"
check "kubectl through the tunnel" wait_for 120 nodes_ready "$SERVERS"
check_out "status is healthy" '"healthy": true' KA status "$NAME"
check "doctor passes" KA doctor "$NAME"
check_out "list knows the cluster" "$NAME" KA list

# ---- 2. agents and persistence -----------------------------------------------------------------------------------------
echo "-- join, persistence"
check "join an agent" KA join "$NAME"
check "agent becomes Ready" wait_for 240 nodes_ready $((SERVERS + 1))
check "persist cycle (stop, start, verify)" KA persist cycle "$NAME"
check "tunnel after the restart" KA tunnel "$NAME"
check "cluster Ready again" wait_for 240 nodes_ready $((SERVERS + 1))

# ---- 3. snapshots ------------------------------------------------------------------------------------------------------
echo "-- etcd snapshots and the schedule"
check "snapshot save" KA snapshot save "$NAME" accept
check_out "snapshot ls shows it" "accept" KA snapshot ls "$NAME"
check "snapshot pull" KA snapshot pull "$NAME" "$(KA snapshot ls "$NAME" 2>/dev/null | awk '/accept/ {print $1; exit}')" --output "${TMPDIR:-/tmp}/$NAME-snap.tar.gz"
rm -f "${TMPDIR:-/tmp}/$NAME-snap.tar.gz" "${TMPDIR:-/tmp}/$NAME-snap.tar.gz.sha256"
check "snapshot schedule (local)" KA snapshot schedule "$NAME" "${SNAP_ARGS[@]}"
check "cluster still Ready after the servers restarted" wait_for 240 nodes_ready $((SERVERS + 1))
check_out "schedule is shown" "newest_age_secs" KA snapshot schedule "$NAME" --show
if [ "$DO_S3" = 1 ]; then
    S3_ARGS=(--s3-endpoint "$S3_ENDPOINT" --s3-bucket "$S3_BUCKET" --s3-folder "$NAME")
    [ -n "${S3_CREDENTIALS_FILE:-}" ] && S3_ARGS+=(--s3-credentials-file "$S3_CREDENTIALS_FILE")
    check "snapshot schedule with an S3 target" KA snapshot schedule "$NAME" "${SNAP_ARGS[@]}" "${S3_ARGS[@]}"
    check "an on-demand snapshot reaches S3 (look for it in the bucket)" KA snapshot save "$NAME" s3check
else
    skip "S3 target" "not requested (--s3)"
fi
check "snapshot schedule reset" KA snapshot schedule "$NAME" --reset

# ---- 4. token rotation -------------------------------------------------------------------------------------------------
echo "-- token rotation"
check "rotate the agent token" KA token rotate "$NAME" --agent
check "cluster Ready after the agent token rotation" wait_for 240 nodes_ready $((SERVERS + 1))
check "a new agent joins with the rotated token" KA join "$NAME"
check "new agent Ready" wait_for 240 nodes_ready $((SERVERS + 2))
check "rotate the server token" KA token rotate "$NAME" --server
check "cluster Ready after the server token rotation" wait_for 240 nodes_ready $((SERVERS + 2))
check "status healthy after both rotations" KA status "$NAME"

# ---- 5. failover (three servers) ---------------------------------------------------------------------------------------
if [ "$SERVERS" -gt 1 ]; then
    echo "-- failover"
    check "forwarder answers" wait_for 30 kc get nodes
    # Stop one server's VM through the daemon: the API must keep answering through the forwarder.
    "$FLUXCTL" stop "k3s-$NAME-s2" >/dev/null 2>&1
    check "the API still answers with one server down" wait_for 60 kc get nodes
    check_out "doctor reports the degraded quorum" "quorum" KA doctor "$NAME"
    "$FLUXCTL" start "k3s-$NAME-s2" >/dev/null 2>&1
    check "the server comes back" wait_for 240 nodes_ready $((SERVERS + 2))
else
    skip "failover" "one server"
fi

# ---- 6. node removal ---------------------------------------------------------------------------------------------------
echo "-- node removal"
check "remove an agent" KA remove "$NAME" "$NAME-a2" --yes
check "the agent is gone from the cluster" wait_for 60 node_absent "$NAME-a2"
if [ "$SERVERS" -gt 1 ]; then
    check "remove a server" KA remove "$NAME" "$NAME-s3" --yes
    check "the server is gone from the cluster" wait_for 60 node_absent "$NAME-s3"
    check "two servers and the agent are Ready" wait_for 120 nodes_ready 3
    check_out "status healthy with the smaller control plane" '"healthy": true' KA status "$NAME"
else
    skip "server removal" "one server"
fi

# ---- 7. tunnel LaunchAgent ---------------------------------------------------------------------------------------------
if [ "$DO_AGENT" = 1 ]; then
    echo "-- tunnel LaunchAgent"
    KA tunnel "$NAME" --stop >/dev/null 2>&1
    check "install the agent" KA tunnel "$NAME" --install-agent
    check "the agent brings the tunnel up" wait_for 60 kc get nodes
    check_out "the agent is loaded" '"loaded": true' KA tunnel "$NAME" --agent-status
    check "remove the agent" KA tunnel "$NAME" --remove-agent
    check "tunnel again by hand" KA tunnel "$NAME"
else
    skip "tunnel LaunchAgent" "not requested (--agent)"
fi

# ---- 8. rebuild of a lost journal --------------------------------------------------------------------------------------
echo "-- rebuild"
ROOT="${FLUXVM_APPLE_K3S_HOME:-$HOME/.fluxvm/apple-k3s}"
KA tunnel "$NAME" --stop >/dev/null 2>&1
mkdir -p "$ROOT/.accept-backup" && cp -p "$ROOT/$NAME.json" "$ROOT/.accept-backup/$NAME.json" && mv "$ROOT/$NAME.json" "$ROOT/$NAME.json.lost"
check_out "rebuild plans without writing" '"written": false' KA rebuild "$NAME"
[ ! -f "$ROOT/$NAME.json" ] && pass "nothing was written without --accept-host-keys" || fail "no write without acceptance" "a journal appeared"
check_out "rebuild writes the journal when the host keys are accepted" '"written": true' KA rebuild "$NAME" --accept-host-keys --api-port "$API_PORT"
check "status healthy on the rebuilt journal" KA status "$NAME"
check "tunnel on the rebuilt journal" KA tunnel "$NAME"
check "kubectl works again" wait_for 60 kc get nodes
rm -f "$ROOT/$NAME.json.lost"

# ---- 9. delete ---------------------------------------------------------------------------------------------------------
echo "-- delete"
if [ "$KEEP" != 1 ]; then
    KA tunnel "$NAME" --stop >/dev/null 2>&1
    check "delete" KA delete "$NAME" --yes
    CREATED=0
else
    skip "delete" "--keep"
fi

echo
echo "passed: ${#PASS[@]}  failed: ${#FAIL[@]}  skipped: ${#SKIP[@]}"
for s in "${SKIP[@]+"${SKIP[@]}"}"; do [ -n "$s" ] && echo "  skipped: $s"; done
for f in "${FAIL[@]+"${FAIL[@]}"}"; do [ -n "$f" ] && echo "  FAILED:  $f"; done
[ "${#FAIL[@]}" -eq 0 ]
