#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Runs every release/fleet suite in one go, locally or on a Linux host over ssh, and prints one line per suite.
#   scripts/test-release-e2e.sh                         on this machine (Linux needed for most suites)
#   scripts/test-release-e2e.sh --host sus@212.8.248.187 [--suite NAME ...] [--keep]
# Suites: static packages bootstrap-tarball bootstrap-deb bootstrap-rpm systemd-deb image-k8s ha enroll-invariants enroll-security
#         central-node agent-central-api agent-central-ha agent-tunnel agents scale   (default: all of these; the two agent-central ones also run on macOS)
#         console-ui  (opt-in: headless Chrome via puppeteer)
#         mutation  (opt-in, slow: breaks each enrollment guard, expects a failure)
#         guest-exec-policy agent-central-vms  (opt-in: root + KVM, builds a guest-agent image; the host's fluxctl and a base image)
# scripts/e2e/deploy-remote.sh and scripts/e2e/fleet-two-host.sh run from the control machine, not through this runner.
# On a host it rsyncs the tree to ~/e2e-work/tree, builds fluxvm-agent once (release) and removes only what it created.
# Needs nfpm and docker for the package suites, curl, jq, openssl >= 3, python3. Exit status = failed suites.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
HOST=""; SUITES=(); KEEP=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --suite) SUITES+=("$2"); shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '5,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
ALL=(static packages bootstrap-tarball bootstrap-deb bootstrap-rpm systemd-deb image-k8s enroll-invariants enroll-security central-node agent-central-api agent-central-ha agent-tunnel agents scale)
[[ ${#SUITES[@]} -gt 0 ]] || SUITES=("${ALL[@]}")

if [[ -n "$HOST" ]]; then
  W='$HOME/e2e-work/tree'
  ssh -o BatchMode=yes "$HOST" "mkdir -p $W" || exit 2
  # The workspace expects guestkit next to the checkout; reuse the one deploy-remote.sh keeps, if there is one.
  ssh -o BatchMode=yes "$HOST" '[ -e "$HOME/e2e-work/guestkit" ] || { [ -d "$HOME/.deployments/guestkit" ] && ln -s "$HOME/.deployments/guestkit" "$HOME/e2e-work/guestkit"; }; true' 
  rsync -a --delete --exclude target --exclude .git ./ "$HOST:e2e-work/tree/" || exit 2
  args=(); for s in "${SUITES[@]}"; do args+=(--suite "$s"); done
  # shellcheck disable=SC2029
  ssh -o BatchMode=yes "$HOST" "cd $W && export PATH=\$HOME/.local/bin:\$HOME/.cargo/bin:\$PATH CARGO_TARGET_DIR=\$HOME/e2e-work/target && scripts/test-release-e2e.sh ${args[*]}; rc=\$?; $([[ $KEEP == 1 ]] || echo "cd; rm -rf $W;") exit \$rc"
  exit $?
fi

mkdir -p target
LOG="target/e2e-$(date +%Y%m%d-%H%M%S).log"
export BIN="${CARGO_TARGET_DIR:-$PWD/target}/release/fluxvm-agent"
export FLUXCTL="${CARGO_TARGET_DIR:-$PWD/target}/release/fluxctl"
# Always ask cargo: a no-op when fresh, and it stops a suite from running against a binary built before the last change.
need_fluxctl() { CARGO_INCREMENTAL=0 cargo build --release -p fluxctl >>"$LOG" 2>&1; }
need_agent() { CARGO_INCREMENTAL=0 cargo build --release -p fluxvm-agent >>"$LOG" 2>&1; }

run() { # SUITE command...
  local name="$1"; shift
  local t0=$SECONDS
  echo "=== $name" >>"$LOG"
  if "$@" >>"$LOG" 2>&1; then printf 'ok    %-20s %4ss\n' "$name" $((SECONDS - t0)); return 0
  else printf 'FAIL  %-20s %4ss  (see %s)\n' "$name" $((SECONDS - t0)) "$LOG"; return 1; fi
}
static() {
  local rc=0
  if rustfmt --version >/dev/null 2>&1; then rustfmt --edition 2024 --check crates/fluxvm-agent/src/*.rs crates/fluxctl/src/*.rs || rc=1
  else echo "static: rustfmt is not installed here; skipped"; fi
  if command -v shellcheck >/dev/null; then shellcheck scripts/fluxvm-bootstrap.sh scripts/make-release-manifest.sh scripts/package-linux-pkgs.sh \
      scripts/test-bootstrap.sh scripts/e2e/*.sh scripts/test-release-e2e.sh scripts/deploy-remote.sh packaging/deb/*.sh || rc=1
  else echo "static: shellcheck is not installed here; skipped"; fi
  for t in test-package-manifest test-vendor-lock test-check-glibc-floor test-check-guestkit-pin verify-systemd-units; do "scripts/$t.sh" || rc=1; done
  need_agent && CARGO_INCREMENTAL=0 cargo test -p fluxvm-agent --lib || rc=1
  return $rc
}
packages() { scripts/test-deb.sh && scripts/test-rpm.sh; }
# Leader election, fencing and takeover against a real Redis (the ignored tests in ha.rs), in a throwaway container.
ha() {
  local port=$((30000 + RANDOM % 20000)) c rc
  c="$(docker run -d --rm -p "127.0.0.1:$port:6379" redis:7-alpine)" || return 1
  sleep 2
  FLUXVM_TEST_REDIS="redis://127.0.0.1:$port" CARGO_INCREMENTAL=0 cargo test -p fluxvm-agent --lib ha:: -- --include-ignored; rc=$?
  docker rm -f "$c" >/dev/null 2>&1
  return $rc
}
# Every agent crate: the fleet agent (central/node/sim), the guest agent and its protocol, the container agent, the vsock client.
agents() { CARGO_INCREMENTAL=0 cargo test --no-fail-fast -p fluxvm-agent -p fluxvm-guest-agent -p fluxvm-guest-protocol -p fluxvm-container-agent \
  -p fluxvm-container-protocol -p fluxvm-vsock-client -p fluxvm-container-client -p fluxctl; }
scale() { need_agent && scripts/bench-fleet-central.sh -b "$BIN" 1000:10:20 5000:10:30 10000:10:40; }

fails=0
for s in "${SUITES[@]}"; do
  case "$s" in
    static) run static static ;;
    packages) run packages packages ;;
    bootstrap-tarball) run bootstrap-tarball scripts/test-bootstrap.sh ;;
    bootstrap-deb) run bootstrap-deb scripts/e2e/bootstrap-pkg.sh deb ;;
    bootstrap-rpm) run bootstrap-rpm scripts/e2e/bootstrap-pkg.sh rpm ;;
    enroll-invariants) need_agent; run enroll-invariants scripts/test-enroll-invariants.sh ;;
    enroll-security) need_agent; run enroll-security scripts/e2e/enroll-security.sh ;;
    central-node) need_agent; run central-node scripts/e2e/central-node.sh ;;
    systemd-deb) run systemd-deb scripts/e2e/systemd-deb.sh ;;
    image-k8s) run image-k8s scripts/e2e/image-k8s.sh ;;
    agents) run agents agents ;;
    mutation) run mutation scripts/e2e/mutation-security.sh ;;
    agent-central-api) need_agent; run agent-central-api scripts/e2e/agent-central-api.sh ;;
    agent-central-ha) need_agent; run agent-central-ha scripts/e2e/agent-central-ha.sh ;;
    console-ui) need_agent; run console-ui scripts/e2e/console-ui.sh ;;
    agent-tunnel) need_agent; need_fluxctl; run agent-tunnel env "BIN=$BIN" "FLUXCTL=$FLUXCTL" scripts/e2e/agent-tunnel.sh ;;
    guest-exec-policy) run guest-exec-policy sudo env "PATH=$PATH" scripts/e2e/guest-exec-policy.sh ;;
    agent-central-vms) need_agent; run agent-central-vms sudo env "PATH=$PATH" "BIN=$BIN" scripts/e2e/agent-central-vms.sh ;;
    ha) run ha ha ;;
    scale) run scale scale ;;
    *) echo "unknown suite $s" >&2; fails=$((fails + 1)); continue ;;
  esac || fails=$((fails + 1))
done
echo; echo "$fails suite(s) failed; log: $LOG"
exit "$fails"
