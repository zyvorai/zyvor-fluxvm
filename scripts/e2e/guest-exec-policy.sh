#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Exec policy and the job supervisor inside a REAL guest (docs/guest-exec-policy.md says "not yet verified on hardware"):
# one QEMU VM with the vsock guest agent, then `fluxctl exec --policy` cases: what the policy allows works, what it does not
# is refused by the guest kernel (Landlock/seccomp), limits and timeouts bite, an unenforceable policy fails closed, and the
# response says what was enforced. Linux with KVM, run as root:
#   sudo scripts/e2e/guest-exec-policy.sh [--image PATH] [--base BASE.img]
# Without --image it builds one from BASE (default /var/lib/fluxvm/images/ubuntu-noble-base.img) with the static guest agent
# from target/release/fluxvm-guest-agent, and removes it at the end. Uses the host's fluxctl and /etc/fluxvm.toml; the VM has a
# unique name and is deleted at the end (also on failure). Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
IMAGE=""; BASE="/var/lib/fluxvm/images/ubuntu-noble-base.img"; CONFIG="/etc/fluxvm.toml"; BUILT=0
while [[ $# -gt 0 ]]; do case "$1" in --image) IMAGE="$2"; shift 2 ;; --base) BASE="$2"; shift 2 ;; *) echo "unknown $1" >&2; exit 2 ;; esac; done
[[ "$(id -u)" == 0 ]] || { echo "run as root (sudo)" >&2; exit 2; }
[[ -e /dev/kvm ]] || { echo "needs /dev/kvm" >&2; exit 2; }
FX="${FLUXVM_BIN:-$(command -v fluxctl)}"; [[ -x "$FX" ]] || { echo "no fluxctl" >&2; exit 2; }
T="$(mktemp -d)"; fails=0; ID=""
eph() { "$FX" --config "$CONFIG" "$@"; }
cleanup() { [[ -n "$ID" ]] && eph delete "$ID" >/dev/null 2>&1; [[ $BUILT == 1 ]] && rm -f "$IMAGE"; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }

if [[ -z "$IMAGE" ]]; then
  GA="target/release/fluxvm-guest-agent"; [[ -x "$GA" ]] || GA="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/fluxvm-guest-agent"
  [[ -x "$GA" && -f "$BASE" ]] || { echo "need a static guest agent ($GA) and a base image ($BASE), or --image" >&2; exit 2; }
  IMAGE="/var/lib/fluxvm/images/fluxvm-exec-policy-test.qcow2"; BUILT=1
  cat > "$T/build.json" <<JSON
{"source":"$BASE","output":"$IMAGE","format":"qcow2",
 "copy_in":[{"src":"$(cd "$(dirname "$GA")" && pwd)/$(basename "$GA")","dest":"/usr/local/bin/fluxvm-guest-agent"},
            {"src":"$PWD/systemd/fluxvm-guest-agent.service","dest":"/etc/systemd/system/fluxvm-guest-agent.service"}],
 "enable_services":["fluxvm-guest-agent"]}
JSON
  eph build-image --spec "$T/build.json" >/dev/null 2>&1 || { echo "image build failed" >&2; exit 2; }
fi
cat > "$T/vm.json" <<JSON
{"name":"fluxvm-e2e-execpolicy","backend":"qemu","image":"$IMAGE","vcpus":1,"memory_mib":768,
 "network":{"mode":"none"},"agent":{"enabled":true,"port":17777},"ttl_seconds":900}
JSON
ID="$(eph create --spec "$T/vm.json" 2>/dev/null | jq -r .id)"
[[ -n "$ID" && "$ID" != null ]] || { echo "create failed" >&2; exit 2; }
up=0; for _ in $(seq 1 30); do eph exec "$ID" -- true >/dev/null 2>&1 && { up=1; break; }; sleep 4; done
check "the guest agent answers over vsock" test $up = 1
[[ $up == 1 ]] || exit 1

X() { eph exec "$ID" "$@" 2>"$T/err"; }                    # raw JSON of an unconfined exec
P() { local pol="$1"; shift; printf '%s' "$pol" > "$T/pol.json"; eph exec --policy "$T/pol.json" "$ID" -- "$@" 2>"$T/err"; }
out() { jq -r '.stdout // empty'; }
code() { jq -r '.exit_code // .status // empty'; }

R="$(X -- sh -c 'echo plain; id -u')"
echo "     | unconfined response keys: $(jq -c 'keys' <<<"$R" 2>/dev/null)"
check "unconfined exec returns stdout" test "$(out <<<"$R" | head -1)" = plain

BASEPOL='"read":["/bin","/usr","/lib","/lib64","/etc","/proc","/dev"],"scope_ipc":false,"clean_env":true,"timeout_secs":20'
# Fail closed: the default policy scopes IPC, which needs Landlock ABI >= 6. A guest kernel that cannot do it must refuse to run
# the command (and say why); one that can runs it confined. Either is correct; running it unconfined is the bug.
R="$(P '{"read":["/bin","/usr","/lib","/lib64","/etc"],"write":["/tmp"]}' sh -c 'touch /tmp/failclosed-marker; echo ran')"
if [[ "$(jq -r .result <<<"$R")" == error ]]; then
  echo "     | default policy refused on this guest: $(jq -r .message <<<"$R" | cut -c1-120)"
  check "fail closed: a policy the guest cannot enforce does not run the command" test "$(X -- sh -c 'test -e /tmp/failclosed-marker && echo yes || echo no' | jq -r .stdout | tr -d '\n')" = no
  check "fail closed: the refusal names what is missing" bash -c "jq -r .message <<<'$R' | grep -q -i -E 'cannot enforce|landlock'"
else
  check "the default policy was enforced and the command ran" test "$(out <<<"$R" | tr -d '\n')" = ran
fi
R="$(P "{$BASEPOL}" cat /etc/hostname)"
echo "     | policy exec raw: $(jq -c . <<<"$R" 2>/dev/null | head -c 700) | stderr: $(head -c 300 "$T/err")"
check "a read inside the policy works" test "$(code <<<"$R")" = 0
check "the response reports what was enforced" bash -c "jq -e '(.enforcement // .enforced // .confinement) != null' <<<'$R' >/dev/null"
echo "     | enforcement: $(jq -c '.enforcement // .enforced // .confinement' <<<"$R" 2>/dev/null | cut -c1-200)"

R="$(P "{$BASEPOL}" sh -c 'ls /root 2>&1; echo rc=$?')"
check "a read outside the policy (/root) is refused by the kernel" bash -c "grep -q -i -E 'permission denied|rc=[12]' <<<\"\$(jq -r .stdout <<<'$R')\""

R="$(P "{$BASEPOL}" sh -c 'touch /tmp/x 2>&1; echo rc=$?')"
check "a write with no write paths is refused" bash -c "grep -q 'rc=[1-9]' <<<\"\$(jq -r .stdout <<<'$R')\""

X -- mkdir -p /tmp/work >/dev/null
R="$(P "{$BASEPOL,\"write\":[\"/tmp/work\"]}" sh -c 'touch /tmp/work/a 2>&1; echo w=$?; touch /tmp/other 2>&1; echo o=$?')"
check "a write inside write[] works" bash -c "grep -q 'w=0' <<<\"\$(jq -r .stdout <<<'$R')\""
check "a write next to it, outside write[], is refused" bash -c "grep -q 'o=[1-9]' <<<\"\$(jq -r .stdout <<<'$R')\""
R="$(P "{$BASEPOL,\"write\":[\"/nonexistent-dir\"]}" true)"
check "a policy naming a path that does not exist fails closed" test "$(jq -r .result <<<"$R")" = error

R="$(P "{$BASEPOL,\"tcp_connect\":\"deny\"}" bash -c 'exec 3<>/dev/tcp/127.0.0.1/22 2>&1; echo rc=$?')"
check "tcp connect is refused when the policy denies it" bash -c "grep -q -E 'rc=[1-9]|denied|refused' <<<\"\$(jq -r '.stdout + .stderr' <<<'$R')\""

T0=$SECONDS; R="$(P "{${BASEPOL/,\"timeout_secs\":20/},\"timeout_secs\":2}" sleep 30)"; EL=$((SECONDS - T0))
echo "     | timeout: ${EL}s, raw: $(jq -c . <<<"$R" 2>/dev/null | head -c 300) stderr: $(head -c 200 "$T/err")"
check "timeout_secs ends a 30 s command in about 2 s (and it did not exit 0)" bash -c "[[ $EL -ge 1 && $EL -lt 15 && \$(jq -r '.exit_code // 1' <<<'$R') != 0 ]]"

R="$(P "{$BASEPOL,\"max_output_bytes\":100}" sh -c 'head -c 5000 /dev/zero | tr "\0" a')"
check "max_output_bytes truncates output" test "$(jq -r .stdout <<<"$R" | wc -c)" -le 400

R="$(P "{$BASEPOL,\"max_memory\":33554432}" python3 -c 'x = bytearray(200 * 1024 * 1024); print(len(x))')"
echo "     | max_memory 32MiB: $(jq -c '[.exit_code,.stdout,.stderr]' <<<"$R" | cut -c1-120)"
check "max_memory stops an allocation above the limit" bash -c "[[ \$(jq -r '.exit_code' <<<'$R') != 0 ]]"

R="$(P "{$BASEPOL,\"seccomp\":\"errno\",\"allow_namespaces\":false}" unshare -U true)"
check "creating a namespace is refused under seccomp" bash -c "[[ \$(jq -r '.exit_code // 1' <<<'$R') != 0 ]]"

R="$(P '{"unknown_future_field":1,"scope_ipc":false,"read":["/bin","/usr","/lib","/lib64","/etc"]}' true)"
check "unknown policy fields are ignored" test "$(code <<<"$R")" = 0

# Executable allow-list (`exec`): read/write paths lose the execute right; only the listed files run.
EXECL='"/bin/sh","/usr/bin/sh","/bin/chmod","/usr/bin/chmod","/bin/true","/usr/bin/true","/lib64/ld-linux-x86-64.so.2","/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2","/lib/ld-linux-aarch64.so.1","/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1"'
EXECPOL="{$BASEPOL,\"write\":[\"/tmp\"],\"best_effort\":true,\"exec\":[$EXECL]}"
R="$(P "$EXECPOL" sh -c 'echo listed')"
check "exec list: a listed program (the shell) runs" test "$(out <<<"$R" | tr -d '\n')" = listed
echo "     | exec list enforcement: $(jq -c '.enforcement | {exec_restricted, filesystem}' <<<"$R" 2>/dev/null)"
check "exec list: the response says it was enforced" test "$(jq -r '.enforcement.exec_restricted' <<<"$R")" = true
R="$(P "$EXECPOL" sh -c "printf '#!/bin/sh\necho pwned\n' > /tmp/e2e-x; chmod +x /tmp/e2e-x; /tmp/e2e-x; echo rc=\$?")"
echo "     | script from /tmp: $(jq -c '[.exit_code,.stdout,.stderr]' <<<"$R" | cut -c1-160)"
check "exec list: a script written into a writable dir cannot be executed" bash -c "! jq -r .stdout <<<'$R' | grep -q pwned && jq -r .stdout <<<'$R' | grep -q 'rc=[1-9]'"
R="$(P "$EXECPOL" cat /etc/hostname)"
check "exec list: a program that is not listed does not start" bash -c "[[ \$(jq -r '.exit_code // 1' <<<'$R') != 0 ]]"
R="$(P "{$BASEPOL,\"exec\":[]}" true)"
check "exec list: an empty list runs nothing" bash -c "[[ \$(jq -r '.exit_code // 1' <<<'$R') != 0 || \$(jq -r .result <<<'$R') == error ]]"

printf '%s' 'not json' > "$T/bad.json"
check "a malformed policy is rejected, not run unconfined" bash -c "! '$FX' --config '$CONFIG' exec --policy '$T/bad.json' '$ID' -- touch /tmp/should-not-exist >/dev/null 2>&1; [[ \$('$FX' --config '$CONFIG' exec '$ID' -- sh -c 'test -e /tmp/should-not-exist && echo yes || echo no' | jq -r .stdout) == no ]]"

check "after all the confined runs the guest is still healthy" bash -c "[[ \$('$FX' --config '$CONFIG' exec '$ID' -- echo alive | jq -r .stdout) == alive ]]"

echo; echo "$fails check(s) failed"; exit "$fails"
