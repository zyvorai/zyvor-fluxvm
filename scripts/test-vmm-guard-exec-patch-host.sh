#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Privileged test for the VMM Guard exec allow-list and signed rule packs.
# Needs root, a BPF-LSM kernel ('bpf' in /sys/kernel/security/lsm), libbpf-dev,
# clang, bpftool, cgroup v2, and fluxctl (for a throwaway signing key).
#
# Everything runs in disposable cgroups; the guard only acts on cgroups that
# have a policy, so the rest of the host is untouched. Three runs:
#   1. audit   + exec policy + rule pack: everything succeeds, audit events recorded
#   2. enforce + exec policy + rule pack: unlisted exec, patched exec/open/device -> EPERM
#   3. enforce + exec policy only:        only the unlisted exec is blocked
#   4. enforce, default deny-all exec:    every exec is blocked (regression check)
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
skip() { echo "SKIP: $*"; exit 0; }
[[ ${EUID:-$(id -u)} -eq 0 ]] || skip "root required"
for c in bpftool clang cc pkg-config python3; do command -v "$c" >/dev/null || skip "$c missing"; done
FLUXCTL="${FLUXCTL:-$(command -v fluxctl || true)}"
[[ -n "$FLUXCTL" ]] || skip "fluxctl missing (needed to generate a signing key)"
pkg-config --exists libbpf || skip "libbpf-dev missing"
grep -Eq '(^|,)bpf(,|$)' /sys/kernel/security/lsm 2>/dev/null || skip "BPF LSM is not active"
[[ -f /sys/fs/cgroup/cgroup.controllers ]] || skip "cgroup v2 required"
mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf

GUARD="${FLUXVM_GUARD_BIN:-}"
if [[ -z "$GUARD" ]]; then
  for g in "$ROOT/dist/bin/fluxvm-guard" "$ROOT/target/release/fluxvm-guard" "$ROOT/target/debug/fluxvm-guard"; do
    [[ -x "$g" ]] && GUARD="$g" && break
  done
fi
[[ -x "${GUARD:-}" ]] || skip "fluxvm-guard not built (cargo build -p fluxvm-intelligence --bin fluxvm-guard)"

TMP="$(mktemp -d)"
PIN="/sys/fs/bpf/fluxvm/guard-exec-patch-test-$$"
LOADER="$TMP/fluxvm-guard-loader"
UUID='aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee'
CGS=()
cleanup() {
  set +e
  [[ -n "${CHILD:-}" ]] && kill "$CHILD" >/dev/null 2>&1
  "$LOADER" --unload "$PIN" >/dev/null 2>&1
  for cg in "${CGS[@]}"; do rmdir "$cg" >/dev/null 2>&1; done
  rm -rf "$TMP"
}
trap cleanup EXIT

bash "$ROOT/scripts/build-vmm-guard.sh" "$TMP/bpf" >/dev/null
cc -O2 -Wall -Wextra -Werror "$ROOT/tools/fluxvm-guard-loader.c" -o "$LOADER" $(pkg-config --cflags --libs libbpf)
"$LOADER" --load "$TMP/bpf/fluxvm_guard.bpf.o" "$PIN"

# --- probe: joins the cgroup, waits for the policy, then tries each operation.
cat > "$TMP/probe.c" <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
static int try_exec(const char *path) {
    int p[2];
    if (pipe2(p, O_CLOEXEC)) return -1;
    pid_t c = fork();
    if (c == 0) {
        execl(path, path, (char *)0);
        int e = errno;
        (void)!write(p[1], &e, sizeof e);
        _exit(127);
    }
    close(p[1]);
    int e = 0;
    ssize_t n = read(p[0], &e, sizeof e);
    close(p[0]);
    int st;
    waitpid(c, &st, 0);
    return n == (ssize_t)sizeof e ? e : 0;
}
static int try_open(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return errno;
    close(fd);
    return 0;
}
int main(int argc, char **argv) {
    if (argc != 7) return 20;
    FILE *f = fopen(argv[1], "w"); if (!f) return 21; fprintf(f, "%d\n", getpid()); fclose(f);
    int fd = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC, 0600); if (fd < 0) return 22; dprintf(fd, "ready\n"); close(fd);
    while (access(argv[3], F_OK) != 0) usleep(10000);
    FILE *r = fopen(argv[3], "r"); /* GO file names the report path */
    char out[512] = {0}; if (!r || !fgets(out, sizeof out, r)) return 23; fclose(r);
    out[strcspn(out, "\n")] = 0;
    int rc_true = try_exec(argv[4]);
    int rc_uname = try_exec(argv[5]);
    int rc_id = try_exec(argv[6]);
    int rc_host = try_open("/etc/hostname");
    int rc_hosts = try_open("/etc/hosts");
    int rc_null = try_open("/dev/null");
    int rc_zero = try_open("/dev/zero");
    FILE *o = fopen(out, "w"); if (!o) return 24;
    fprintf(o, "exec_listed=%d exec_unlisted=%d exec_patched=%d open_patched=%d open_control=%d dev_patched=%d dev_control=%d\n",
            rc_true, rc_uname, rc_id, rc_host, rc_hosts, rc_null, rc_zero);
    fclose(o);
    return 0;
}
C
cc -O2 -Wall -Wextra -Werror "$TMP/probe.c" -o "$TMP/probe"

# Three distinct executables. Distro binaries can share an inode (multicall
# coreutils: id and uname are one file), and the guard matches by inode.
mkdir "$TMP/bin"
for n in allowed unlisted patched; do
  printf 'int main(void){return 0;}\n' > "$TMP/bin/$n.c"
  cc -O2 "$TMP/bin/$n.c" -o "$TMP/bin/$n"
done

# --- signed inputs
KG="$("$FLUXCTL" catalog keygen 2>/dev/null)"
echo "$KG" | grep -A1 '^private key' | tail -1 | tr -d ' ' > "$TMP/key"
PUB="$(echo "$KG" | grep public_key | sed 's/.*"\(.*\)"/\1/')"
printf 'version = 1\npaths = ["%s", "%s"]\n' "$TMP/bin/allowed" "$TMP/bin/patched" > "$TMP/exec.toml"
cat > "$TMP/pack.toml" <<P
version = 1
id = "HOST-TEST-1"
cve = ["CVE-0000-0000"]
valid_until = "2099-01-01T00:00:00Z"
[[rules]]
action = "deny-exec"
path = "$TMP/bin/patched"
[[rules]]
action = "deny-open"
path = "/etc/hostname"
[[rules]]
action = "deny-device"
path = "/dev/null"
P
"$GUARD" sign-exec-policy "$TMP/exec.toml" "$TMP/key" >/dev/null
"$GUARD" sign-rule-pack "$TMP/pack.toml" "$TMP/key" >/dev/null

export FLUXVM_GUARD_PIN_ROOT="$PIN" FLUXVM_GUARD_STATE_ROOT="$TMP/state"

# stat_count ACTION DECISION: events recorded in guard_stats (summed over CPUs).
stat_count() {
  bpftool -j map dump pinned "$PIN/maps/guard_stats" | python3 -c '
import json, sys
want = (int(sys.argv[1]), int(sys.argv[2]))
def raw(b):
    return bytes(int(x, 16) if isinstance(x, str) else x for x in b)
def le(b, o, n): return int.from_bytes(raw(b)[o:o+n], "little")
total = 0
for e in json.load(sys.stdin):
    k = e["key"]
    ad = (k["action"], k["decision"]) if isinstance(k, dict) else (le(k, 8, 4), le(k, 12, 4))
    if ad != want:
        continue
    for v in e.get("values", []):
        val = v["value"]
        total += val["events"] if isinstance(val, dict) else le(val, 0, 8)
print(total)
' "$1" "$2"
}

# run_case NAME MODE WITH_PACK -> writes $TMP/report.NAME
run_case() {
  local name="$1" mode="$2" with_pack="$3" exec_mode="${4:-policy}"
  local cg="/sys/fs/cgroup/fluxvm-guard-ep-test-$$-$name"
  mkdir "$cg"; CGS+=("$cg")
  rm -f "$TMP/ready" "$TMP/go"
  "$TMP/probe" "$cg/cgroup.procs" "$TMP/ready" "$TMP/go" \
    "$TMP/bin/allowed" "$TMP/bin/unlisted" "$TMP/bin/patched" >"$TMP/probe.out" 2>&1 & CHILD=$!
  for _ in $(seq 1 200); do [[ -f "$TMP/ready" ]] && break; sleep 0.02; done
  [[ -f "$TMP/ready" ]] || { echo "probe did not enter cgroup" >&2; exit 1; }
  local args=(apply "$UUID" "$CHILD" --mode "$mode" --no-device-allowlist --allow-wx)
  [[ "$exec_mode" == policy ]] && args+=(--exec-policy "$TMP/exec.toml" --trusted-key "$PUB")
  [[ "$with_pack" == yes ]] && args+=(--rule-pack "$TMP/pack.toml")
  "$GUARD" "${args[@]}" >/dev/null 2>"$TMP/apply.err" || { cat "$TMP/apply.err" >&2; exit 1; }
  echo "$TMP/report.$name" > "$TMP/go"
  wait "$CHILD"; CHILD=''
  "$GUARD" remove "$UUID" >/dev/null
  [[ -f "$TMP/report.$name" ]] || { echo "no report for $name" >&2; cat "$TMP/probe.out" >&2; exit 1; }
  echo "$name: $(cat "$TMP/report.$name")"
}

fail=0
expect() { # NAME KEY=VALUE...
  local name="$1"; shift
  for kv in "$@"; do
    grep -q "$kv\b" "$TMP/report.$name" || { echo "  FAIL [$name] expected $kv"; fail=1; }
  done
}
expect_stat() { # LABEL ACTION DECISION MIN
  local n; n="$(stat_count "$2" "$3")"
  if (( n >= $4 )); then echo "  ok   events $1 = $n"; else echo "  FAIL events $1 = $n (want >= $4)"; fail=1; fi
}

EPERM=1
run_case audit audit yes
expect audit exec_listed=0 exec_unlisted=0 exec_patched=0 open_patched=0 open_control=0 dev_patched=0 dev_control=0
expect_stat "EXEC_UNLISTED/audit" 5 1 1
expect_stat "PATCH_EXEC/audit" 6 1 1
expect_stat "PATCH_OPEN/audit" 7 1 1
expect_stat "PATCH_DEVICE/audit" 8 1 1

run_case enforce enforce yes
expect enforce exec_listed=0 exec_unlisted=$EPERM exec_patched=$EPERM open_patched=$EPERM open_control=0 dev_patched=$EPERM dev_control=0
expect_stat "EXEC_UNLISTED/deny" 5 2 1
expect_stat "PATCH_EXEC/deny" 6 2 1
expect_stat "PATCH_OPEN/deny" 7 2 1
expect_stat "PATCH_DEVICE/deny" 8 2 1

run_case execonly enforce no
expect execonly exec_listed=0 exec_unlisted=$EPERM exec_patched=0 open_patched=0 open_control=0 dev_patched=0 dev_control=0

run_case denyall enforce no denyall
expect denyall exec_listed=$EPERM exec_unlisted=$EPERM exec_patched=$EPERM open_patched=0 open_control=0 dev_patched=0 dev_control=0
expect_stat "EXEC/deny" 1 2 3

if (( fail )); then echo "VMM Guard exec allow-list + rule pack host test: FAIL"; exit 1; fi
echo "VMM Guard exec allow-list + rule pack host test: PASS"
