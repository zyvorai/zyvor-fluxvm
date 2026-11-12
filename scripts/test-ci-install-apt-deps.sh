#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Tests scripts/ci-install-apt-deps.sh with fake dpkg-query/sudo/apt-get on PATH (no real apt, no real sudo).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$ROOT/scripts/ci-install-apt-deps.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin"
LOG="$TMP/calls.log"

# dpkg-query: a package is installed iff its name is listed in $FAKE_INSTALLED.
cat > "$TMP/bin/dpkg-query" <<'SH'
#!/usr/bin/env bash
pkg="${*: -1}"
for p in $FAKE_INSTALLED; do [[ $p == "$pkg" ]] && { printf installed; exit 0; }; done
exit 1
SH
# sudo: refuses unless $FAKE_SUDO_OK=1 (the sudoers message of the zyvor-build-c..f runners), else runs the command.
cat > "$TMP/bin/sudo" <<'SH'
#!/usr/bin/env bash
echo "sudo $*" >> "$FAKE_LOG"
if [[ ${FAKE_SUDO_OK:-0} != 1 ]]; then echo "sudo: I'm sorry ghrunner. I'm afraid I can't do that" >&2; exit 1; fi
[[ $1 == -n ]] && shift
exec "$@"
SH
cat > "$TMP/bin/apt-get" <<'SH'
#!/usr/bin/env bash
echo "apt-get $*" >> "$FAKE_LOG"
SH
chmod +x "$TMP/bin/"*
export FAKE_LOG="$LOG" PATH="$TMP/bin:$PATH"

fail() { echo "FAIL: $*" >&2; exit 1; }

# 1. Everything present: success, and neither sudo nor apt-get is touched.
: > "$LOG"
FAKE_INSTALLED="a b c" FAKE_SUDO_OK=0 "$SCRIPT" a b c > "$TMP/out" 2>&1 || fail "all-installed case exited non-zero: $(cat "$TMP/out")"
[[ ! -s $LOG ]] || fail "all-installed case called sudo/apt-get: $(cat "$LOG")"
grep -q 'already installed' "$TMP/out" || fail "all-installed case did not say so"

# 2. Something missing and sudo works: only the missing packages are installed.
: > "$LOG"
FAKE_INSTALLED="a" FAKE_SUDO_OK=1 "$SCRIPT" a b c > "$TMP/out" 2>&1 || fail "install case exited non-zero: $(cat "$TMP/out")"
grep -q 'apt-get install -y -qq b c$' "$LOG" || fail "install case did not install exactly 'b c': $(cat "$LOG")"
if grep -q 'apt-get install.* a\b' "$LOG"; then fail "install case reinstalled an installed package: $(cat "$LOG")"; fi

# 3. Something missing and sudo refuses: fail with the remediation, not a bare sudo refusal.
: > "$LOG"
if FAKE_INSTALLED="a" FAKE_SUDO_OK=0 "$SCRIPT" a b > "$TMP/out" 2>&1; then fail "no-sudo case should fail"; fi
grep -q '::error::cannot install b' "$TMP/out" || fail "no-sudo case lacks the error: $(cat "$TMP/out")"
grep -q 'apt-get install -y b' "$TMP/out" || fail "no-sudo case lacks the remediation: $(cat "$TMP/out")"

# 4. No arguments is a usage error.
if "$SCRIPT" > /dev/null 2>&1; then fail "no-argument case should fail"; fi

echo "ci-install-apt-deps tests passed"
