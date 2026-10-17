#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# Tests the manifest checker of scripts/package-linux.sh (`--check-manifest`) with good and bad manifests.
# Runs on any OS: it never builds anything.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

check() { # name want(pass|fail) manifest-text
  printf '%s\n' "$3" >"$T/m.txt"
  PACKAGE_MANIFEST="$T/m.txt" scripts/package-linux.sh --check-manifest >"$T/out" 2>&1
  local rc=$?
  if [[ "$2" == pass && $rc -eq 0 ]] || [[ "$2" == fail && $rc -ne 0 ]]; then ok "$1"; else bad "$1 (exit $rc)"; sed 's/^/     /' "$T/out"; fi
}

GOOD='core bin fluxctl/fluxctl bin/fluxctl 0755
core link fluxctl bin/fluxvm 0777
core file config.example.toml share/config.example.toml 0644
core bpf fluxvm_tc.bpf.o lib/bpf/fluxvm_tc.bpf.o 0644
core bpf fluxvm_service_tier_M.bpf.o lib/bpf/fluxvm_service_tier_M.bpf.o 0644
core guest fluxvm-guest-agent share/guest/fluxvm-guest-agent 0755 optional
core completions bash share/completions/fluxctl.bash 0644 optional'

# The real manifest must always be valid.
if scripts/package-linux.sh --check-manifest >"$T/out" 2>&1; then ok "the shipped packaging/files.txt is valid"; else bad "the shipped packaging/files.txt is valid"; cat "$T/out"; fi

check "a small good manifest passes" pass "$GOOD"
check "comments and blank lines are ignored" pass "# a comment

$GOOD
   # indented comment"
check "an unknown group is rejected" fail "${GOOD/core bin/weird bin}"
check "an unknown kind is rejected" fail "$GOOD
core wat x y 0644"
check "a non-octal mode is rejected" fail "${GOOD/0755/rwx}"
check "a missing column is rejected" fail "core bin fluxctl/fluxctl bin/fluxctl"
check "trailing junk is rejected" fail "core bin fluxctl/fluxctl bin/fluxctl 0755 required"
check "two entries for one DEST are rejected" fail "$GOOD
core file LICENSE bin/fluxctl 0644"
check "an absolute DEST is rejected" fail "core file LICENSE /etc/LICENSE 0644"
check "a DEST that escapes is rejected" fail "core file LICENSE ../LICENSE 0644"
check "a binary that does not exist is rejected" fail "core bin fluxctl/not-a-binary bin/x 0755"
check "a package that does not exist is rejected" fail "core bin no-such-crate/no-such-crate bin/x 0755"
check "a bin source without a package is rejected" fail "core bin fluxctl bin/x 0755"
check "a link to something not in the manifest is rejected" fail "core link nothere bin/fluxvm 0777"
check "a required file that does not exist is rejected" fail "core file no/such/file share/x 0644"
check "an optional file may be absent" pass "core file no/such/file share/x 0644 optional"
check "a bpf object with no source is rejected" fail "core bpf fluxvm_nonexistent.bpf.o lib/bpf/x.bpf.o 0644"
check "an unsupported guest agent name is rejected" fail "core guest other share/guest/other 0755"
check "an unknown completion shell is rejected" fail "core completions powershell share/c 0644"
check "an empty manifest is an error" fail ""
check "a manifest of only comments is an error" fail "# nothing here"

# Also: the BPF arch override is understood by the build script.
if grep -q 'FLUXVM_BPF_ARCH' scripts/build-ebpf.sh; then ok "build-ebpf.sh honours FLUXVM_BPF_ARCH"; else bad "build-ebpf.sh honours FLUXVM_BPF_ARCH"; fi

if [[ $fail -eq 0 ]]; then echo "all passed"; else echo "FAILED"; exit 1; fi
