#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test: the egress policy the daemon answers for (`POST /v1/egress/check`, `GET /v1/egress/nftables`). With no allowlist every host is
# allowed; with `[sandbox] egress_allow_domains` only the listed hosts and their subdomains are, matching is case-insensitive and ignores a
# trailing dot, and and a configured vault credential is never revealed by the API or the log.
#   scripts/vz-egress-live-test.sh
# Needs Apple silicon macOS 14+ (the daemon is built with the vz runner), Rust, python3. No guest is started.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-egress.XXXXXX")"; P1="${FLUXVM_LIVE_PORT:-7840}"; P2=$((P1 + 1))
cleanup() { [[ -n "${D1:-}" ]] && kill "$D1" 2>/dev/null || true; [[ -n "${D2:-}" ]] && kill "$D2" 2>/dev/null || true; sleep 1; rm -rf "$T"; }
trap cleanup EXIT
ok() { echo "ok   $*"; }; bad() { echo "FAIL $*"; for f in "$T"/d*.log; do [[ -f "$f" ]] && tail -5 "$f"; done; exit 1; }

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/open.toml" <<EOT
listen = "127.0.0.1:$P1"
state_dir = "$T/s1"
run_dir = "/tmp/fluxvm-run-egress1"
EOT
cat > "$T/locked.toml" <<EOT
listen = "127.0.0.1:$P2"
state_dir = "$T/s2"
run_dir = "/tmp/fluxvm-run-egress2"
[sandbox]
egress_allow_domains = ["example.com", ".Pypi.org"]
credential_vault = [{ host = "example.com", authorization = "Bearer s3cret-token" }, { host = "api.github.com", authorization = "Bearer ghp-not-allowed" }]
EOT
export FLUXVM_VZ_RUNNER="$PWD/$RUNNER"
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/open.toml" serve > "$T/d1.log" 2>&1 & D1=$!
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/locked.toml" serve > "$T/d2.log" 2>&1 & D2=$!
for p in $P1 $P2; do for _ in $(seq 1 40); do curl -fs "http://127.0.0.1:$p/healthz" >/dev/null 2>&1 && break; sleep 1; done; done
curl -fs "http://127.0.0.1:$P1/healthz" >/dev/null && curl -fs "http://127.0.0.1:$P2/healthz" >/dev/null && ok "both daemons are up (no allowlist on :$P1, allowlist on :$P2)" || bad "daemons did not start"

jget() { python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"; }
chk() { # <port> <host> <expr over d>
  curl -sS -X POST "http://127.0.0.1:$1/v1/egress/check" -H 'content-type: application/json' \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"host": sys.argv[1]}))' "$2")" | jget "$3"
}

[[ "$(chk $P1 anything.example.net 'd["allow"]')" == True && "$(chk $P1 anything.example.net 'd["reason"]')" == "no allowlist configured" ]] && ok "no allowlist: every host is allowed ('no allowlist configured')" || bad "open: $(chk $P1 anything.example.net 'd')"
[[ "$(chk $P2 example.com 'd["allow"]')" == True && "$(chk $P2 example.com 'd["reason"]')" == "host matched allowlist" ]] && ok "allowlist: an exact host matches" || bad "exact: $(chk $P2 example.com 'd')"
[[ "$(chk $P2 api.example.com 'd["allow"]')" == True ]] && ok "allowlist: a subdomain of a listed domain matches" || bad "subdomain"
[[ "$(chk $P2 pypi.org 'd["allow"]')" == True && "$(chk $P2 files.pypi.org 'd["allow"]')" == True ]] && ok "allowlist: '.Pypi.org' (leading dot, mixed case) matches pypi.org and its subdomains" || bad "pypi"
[[ "$(chk $P2 EXAMPLE.COM. 'd["allow"]')" == True && "$(chk $P2 '  example.com  ' 'd["allow"]')" == True ]] && ok "allowlist: host is case-insensitive, ignores a trailing dot and surrounding spaces" || bad "normalisation"
[[ "$(chk $P2 evil.com 'd["allow"]')" == False && "$(chk $P2 evil.com 'd["reason"]')" == "host evil.com not in egress allowlist" ]] && ok "allowlist: an unlisted host is refused with a reason naming it" || bad "refuse: $(chk $P2 evil.com 'd')"
[[ "$(chk $P2 notexample.com 'd["allow"]')" == False && "$(chk $P2 example.com.evil.com 'd["allow"]')" == False ]] && ok "allowlist: lookalikes (notexample.com, example.com.evil.com) are refused" || bad "lookalike"

raw() { curl -sS -X POST "http://127.0.0.1:$P2/v1/egress/check" -H 'content-type: application/json' -d "{\"host\":\"$1\"}"; }
LEAK=""; for h in example.com api.example.com pypi.org api.github.com evil.com; do r="$(raw "$h")"; [[ "$r" == *s3cret* || "$r" == *ghp-* || "$r" == *Bearer* || "$r" == *authorization* ]] && LEAK="$LEAK $h"; done
[[ -z "$LEAK" ]] && ok "vault: the credential is never in an API answer, allowed host or not (the proxy injects it; the caller cannot read it)" || bad "secret leaked for:$LEAK"
[[ "$(raw example.com | jget 'sorted(d)')" == '["allow", "reason"]' ]] && ok "vault: the answer has exactly allow + reason" || bad "fields: $(raw example.com)"
[[ "$(chk $P2 api.github.com 'd["allow"]')" == False ]] && ok "vault: a credentialed host that is not on the allowlist is still refused" || bad "github: $(chk $P2 api.github.com 'd')"
! grep -qE 's3cret|ghp-not' "$T/d2.log" && ok "vault: the token appears nowhere in the daemon log" || bad "token in daemon log"

C="$(curl -sS -o "$T/bad.out" -w '%{http_code}' -X POST "http://127.0.0.1:$P2/v1/egress/check" -H 'content-type: application/json' -d '{}')"
[[ "$C" == 422 || "$C" == 400 ]] && ok "a body without 'host' is refused ($C)" || bad "missing host gave $C: $(cat "$T/bad.out")"
[[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$P2/v1/egress/check")" == 405 ]] && ok "GET on /egress/check is 405" || bad "GET check"

N="$(curl -fs "http://127.0.0.1:$P2/v1/egress/nftables")"
[[ "$N" == *"table inet fluxvm_egress"* && "$N" == *"redirect to :18080"* && "$N" == *"dport { 80, 443 }"* ]] && ok "nftables: the redirect snippet sends 80/443 to the proxy port 18080" || bad "nftables: $N"
[[ "$(curl -sI "http://127.0.0.1:$P2/v1/egress/nftables" | tr -d '\r' | grep -i '^content-type')" == *"text/plain"* ]] && ok "nftables: served as text/plain" || bad "nftables content-type"
echo "vz egress: PASS"
