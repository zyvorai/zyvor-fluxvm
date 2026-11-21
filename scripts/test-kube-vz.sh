#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Real-cluster check of the `fluxctl kube` features that are only unit-tested so far (CHANGELOG 0.4.17/0.4.18 and
# Unreleased): checkpoint + diff + timeline, trim, git, host, --rosetta, supervise --active, and the LaunchAgent
# status/guard. Boots a throwaway k3s cluster in a Virtualization.framework VM on an Apple Silicon Mac, and removes it.
#
# Needs: macOS on Apple Silicon, an unlocked login session (a sleeping cluster cannot be restored on a locked Mac),
# a running FluxVM daemon (`fluxctl doctor` is green), kubectl, ~4 GiB RAM and ~30 GiB disk for the VM, and network
# access for the first k3s download. It uses its own cluster name and API port, so a cluster named fluxvm-kube is left
# alone. It does NOT install a LaunchAgent unless --agent is given (that replaces nothing: the label is
# dev.zyvor.fluxvm.kube).
#
# Usage:
#   FLUXCTL=./target/release/fluxctl ./scripts/test-kube-vz.sh [--agent] [--skip-rosetta] [--skip-supervise] [--keep]
#
# Exit status: 0 when every check passed, 1 when any failed, 2 when a precondition is missing. Skipped checks are listed.
set -uo pipefail

FLUXCTL="${FLUXCTL:-fluxctl}"
NAME="${FLUXVM_E2E_NAME:-fluxvm-e2e}"
API_PORT="${FLUXVM_E2E_API_PORT:-16443}"
HOST_PORT="${FLUXVM_E2E_HOST_PORT:-18099}"
DO_AGENT=0 DO_ROSETTA=1 DO_SUPERVISE=1 KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --agent) DO_AGENT=1 ;;
        --skip-rosetta) DO_ROSETTA=0 ;;
        --skip-supervise) DO_SUPERVISE=0 ;;
        --keep) KEEP=1 ;;
        -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

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
KUBE=("$FLUXCTL" kube)
C=(--name "$NAME" --api-port "$API_PORT")
kubec() { "${KUBE[@]}" "$1" "${C[@]}" "${@:2}"; }

# ---- preconditions ---------------------------------------------------------------------------
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo "needs macOS on Apple Silicon" >&2; exit 2; }
command -v "$FLUXCTL" >/dev/null 2>&1 || [ -x "$FLUXCTL" ] || { echo "fluxctl not found (set FLUXCTL)" >&2; exit 2; }
command -v kubectl >/dev/null 2>&1 || { echo "kubectl not found" >&2; exit 2; }
DOCTOR="$("$FLUXCTL" doctor 2>&1 || true)"
echo "$DOCTOR" | grep -q "^ok    daemon" || { echo "the FluxVM daemon is not healthy:"; echo "$DOCTOR"; exit 2; }
if echo "$DOCTOR" | grep -qE "^warn  login session"; then
    echo "the login session is locked or absent: unlock the Mac first (sleep/wake restores need it)"; exit 2
fi
KUBECONFIG_FILE="$(mktemp -t fluxvm-e2e-kubeconfig)"
WORK="$(mktemp -d -t fluxvm-e2e)"
SUPERVISE_PID=""
HOST_PID=""

cleanup() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$SUPERVISE_PID" ] && kill "$SUPERVISE_PID" 2>/dev/null
    if [ "$KEEP" = 0 ]; then
        echo "tearing down $NAME"
        "${KUBE[@]}" down "${C[@]}" --purge >/dev/null 2>&1
    else
        echo "--keep: left cluster $NAME running (api port $API_PORT)"
    fi
    rm -rf "$WORK" "$KUBECONFIG_FILE"
}
trap cleanup EXIT

# ---- up ----------------------------------------------------------------------------------------
echo "== up"
if kubec up --kubeconfig "$KUBECONFIG_FILE" --cni flannel >"$WORK/up.log" 2>&1; then
    pass "up: single-node cluster is Ready"
else
    fail "up" "$(tail -3 "$WORK/up.log" | tr '\n' ' ')"
    echo "cannot continue without a cluster"; printf '%s\n' "${FAIL[@]}"; exit 1
fi
export KUBECONFIG="$KUBECONFIG_FILE"
check "status: API answers" kubec status
check "kubectl sees a Ready node" bash -c "kubectl get nodes --no-headers | grep -q ' Ready'"

# ---- checkpoint, diff, timeline ----------------------------------------------------------------
echo "== checkpoint / diff / timeline"
check "checkpoint before" kubec checkpoint before
kubectl create namespace e2e-diff >/dev/null 2>&1
kubectl -n e2e-diff create deployment web --image=nginx:alpine --replicas=1 >/dev/null 2>&1
check "checkpoint after" kubec checkpoint after
D="$(kubec diff before after 2>&1)"
if echo "$D" | grep -q "e2e-diff" && echo "$D" | grep -qi "web"; then pass "diff before..after names the added deployment"
else fail "diff" "added deployment not in output: $(echo "$D" | head -3 | tr '\n' ' ')"; fi
T="$(kubec timeline 2>&1)"
if echo "$T" | grep -q "before" && echo "$T" | grep -q "after"; then pass "timeline lists both checkpoints"
else fail "timeline" "$(echo "$T" | head -3 | tr '\n' ' ')"; fi

# ---- trim ---------------------------------------------------------------------------------------
echo "== trim"
TR="$(kubec trim 2>&1)"; RC=$?
if [ $RC -eq 0 ]; then pass "trim gave space back"
elif echo "$TR" | grep -qi "discard"; then skip "trim" "refused with a reason, as designed: $(echo "$TR" | head -1)"
else fail "trim" "$(echo "$TR" | head -2 | tr '\n' ' ')"; fi

# ---- git ----------------------------------------------------------------------------------------
echo "== git"
( cd "$WORK" && git init -q repo && cd repo && git -c user.email=e2e@example.invalid -c user.name=e2e commit -q --allow-empty -m init ) >/dev/null 2>&1
( cd "$WORK/repo" && "$FLUXCTL" kube git install "${C[@]}" ) >"$WORK/git.log" 2>&1
if [ -x "$WORK/repo/.git/hooks/post-checkout" ]; then pass "git install writes the post-checkout hook"; else fail "git install" "no hook: $(tail -2 "$WORK/git.log" | tr '\n' ' ')"; fi
check "git status runs" bash -c "cd '$WORK/repo' && '$FLUXCTL' kube git status --name '$NAME' --api-port '$API_PORT'"
( cd "$WORK/repo" && "$FLUXCTL" kube git uninstall "${C[@]}" ) >/dev/null 2>&1
[ ! -e "$WORK/repo/.git/hooks/post-checkout" ] && pass "git uninstall removes the hook" || fail "git uninstall" "hook still there"

# ---- host: a Mac program as a Service in the cluster ---------------------------------------------
echo "== host"
mkdir -p "$WORK/www" && echo "from-the-mac" >"$WORK/www/index.html"
( cd "$WORK/www" && exec python3 -m http.server "$HOST_PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
HOST_PID=$!
"${KUBE[@]}" host "${C[@]}" macweb "$HOST_PORT" >"$WORK/host.log" 2>&1 &
RELAY_PID=$!
sleep 15
GOT="$(kubectl run e2e-curl --rm -i --restart=Never --image=curlimages/curl --quiet -- curl -s -m 10 "http://macweb:$HOST_PORT/" 2>&1 || true)"
if echo "$GOT" | grep -q "from-the-mac"; then pass "a pod reaches the Mac's server as macweb:$HOST_PORT"
else fail "host" "pod got: $(echo "$GOT" | head -2 | tr '\n' ' ')"; fi
kill "$RELAY_PID" 2>/dev/null; kill "$HOST_PID" 2>/dev/null; HOST_PID=""

# ---- rosetta ------------------------------------------------------------------------------------
echo "== rosetta"
if [ "$DO_ROSETTA" = 1 ]; then
    if /usr/bin/pgrep -q oahd 2>/dev/null || [ -d /Library/Apple/usr/libexec/oah ]; then
        kubec down >/dev/null 2>&1
        if kubec up --kubeconfig "$KUBECONFIG_FILE" --cni flannel --rosetta >"$WORK/rosetta.log" 2>&1; then
            A="$(kubectl run e2e-amd64 --rm -i --restart=Never --quiet --image=amd64/alpine --overrides='{"spec":{"nodeSelector":{"kubernetes.io/arch":"arm64"}}}' -- uname -m 2>&1 || true)"
            if echo "$A" | grep -q "x86_64"; then pass "an amd64 image runs under Rosetta"
            else fail "rosetta" "uname said: $(echo "$A" | head -2 | tr '\n' ' ')"; fi
        else fail "rosetta up" "$(tail -3 "$WORK/rosetta.log" | tr '\n' ' ')"; fi
    else skip "rosetta" "Rosetta is not installed on this Mac (softwareupdate --install-rosetta)"; fi
else skip "rosetta" "--skip-rosetta"; fi

# ---- supervise --active ------------------------------------------------------------------------
echo "== supervise --active"
if [ "$DO_SUPERVISE" = 1 ]; then
    # A window that excludes now: one minute, an hour from now. The cluster should be put to sleep after 2 quiet minutes.
    START="$(date -v+1H +%H:%M)"; END="$(date -v+1H -v+1M +%H:%M)"
    "${KUBE[@]}" supervise "${C[@]}" --active "$START-$END" --every 10 >"$WORK/supervise.log" 2>&1 &
    SUPERVISE_PID=$!
    ASLEEP=0
    for _ in $(seq 1 30); do   # up to 5 minutes
        sleep 10
        if "$FLUXCTL" kube prompt --name "$NAME" 2>/dev/null | grep -q "💤"; then ASLEEP=1; break; fi
    done
    [ $ASLEEP = 1 ] && pass "supervise --active puts the cluster to sleep outside the window" \
        || fail "supervise --active" "never asleep; log: $(tail -3 "$WORK/supervise.log" | tr '\n' ' ')"
    # Touching the API port must wake it (wake-on-first-connection).
    if [ $ASLEEP = 1 ]; then
        ( exec 3<>/dev/tcp/127.0.0.1/"$API_PORT" ) 2>/dev/null
        woke=0
        for _ in $(seq 1 18); do sleep 10; kubectl get nodes >/dev/null 2>&1 && { woke=1; break; }; done
        [ $woke = 1 ] && pass "a client connection wakes the sleeping cluster" || fail "wake on connect" "API never answered again"
    fi
    kill "$SUPERVISE_PID" 2>/dev/null; SUPERVISE_PID=""
else skip "supervise --active" "--skip-supervise"; fi

# ---- LaunchAgent (opt-in: it installs dev.zyvor.fluxvm.kube) ------------------------------------
echo "== launch agent"
if [ "$DO_AGENT" = 1 ]; then
    if "$FLUXCTL" kube agent status 2>/dev/null | grep -q '"installed": true'; then
        skip "kube agent" "dev.zyvor.fluxvm.kube is already installed here; not replacing it"
    else
        check "agent install" "${KUBE[@]}" agent install "${C[@]}" --restart on-failure --throttle-secs 15
        sleep 5
        S="$("$FLUXCTL" kube agent status 2>/dev/null)"
        echo "$S" | grep -q '"loaded": true' && pass "agent status reports loaded" || fail "agent status" "$S"
        grep -q "SuccessfulExit" "$HOME/Library/LaunchAgents/dev.zyvor.fluxvm.kube.plist" && pass "plist carries --restart on-failure" || fail "plist" "no SuccessfulExit key"
        # Brew-style: the plist must name a path that survives an upgrade (opt/), never a versioned Cellar path.
        grep -q "/Cellar/" "$HOME/Library/LaunchAgents/dev.zyvor.fluxvm.kube.plist" && fail "stable path" "plist names a Cellar path" || pass "plist names no versioned Cellar path"
        # A second supervisor outside launchd must be refused while the first runs (the lock).
        "${KUBE[@]}" supervise "${C[@]}" --every 10 >"$WORK/dup.log" 2>&1 &
        DUP=$!; sleep 8
        grep -q "already running" "$WORK/dup.log" && pass "a duplicate supervisor is refused" || fail "duplicate supervisor" "$(tail -2 "$WORK/dup.log" | tr '\n' ' ')"
        kill "$DUP" 2>/dev/null
        check "agent uninstall" "${KUBE[@]}" agent uninstall
    fi
else skip "launch agent" "pass --agent to install and exercise dev.zyvor.fluxvm.kube"; fi

# ---- report -------------------------------------------------------------------------------------
echo
echo "passed ${#PASS[@]}, failed ${#FAIL[@]}, skipped ${#SKIP[@]}"
for s in "${SKIP[@]}"; do echo "  skipped: $s"; done
for f in "${FAIL[@]}"; do echo "  FAILED:  $f"; done
[ ${#FAIL[@]} -eq 0 ]
