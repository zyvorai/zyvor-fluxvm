#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of HTTPS interception and credential grants for a vz agent session (guest agent, `agent-micro`).
# TLS is judged per request by the daemon's egress proxy (`egress_tls_intercept`, guest trusts its CA); a session grant injects a header.
#   scripts/vz-https-live-test.sh /path/to/agent-micro-arm64.raw
# Build the image on Linux first: scripts/build-guest-agent-static.sh + scripts/build-agent-micro.sh (docs/agent-micro.md).
# Needs Apple silicon macOS, the Xcode command line tools, Rust, python3 and curl. Takes a few minutes (cold boots).
# Needs outbound access to example.com and httpbin.org.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || { echo "Apple silicon macOS only" >&2; exit 2; }
IMAGE="${1:-}"
[[ -f "$IMAGE" ]] || { echo "usage: $0 /path/to/agent-micro-arm64.raw" >&2; exit 2; }
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-agent-live.XXXXXX")"
PORT="${FLUXVM_LIVE_PORT:-7797}"; PROXY="${FLUXVM_LIVE_PROXY_PORT:-18097}"
TOKEN="live-$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
U="http://127.0.0.1:$PORT"
cleanup() {
  [[ -n "${DPID:-}" ]] && kill "$DPID" 2>/dev/null || true
  sleep 1
  pkill -f "fluxvm-vz-runner run --config $T" 2>/dev/null || true
  rm -rf "$T"
}
trap cleanup EXIT
pass=0
ok() { echo "ok   $*"; pass=$((pass + 1)); }
bad() { echo "FAIL $*"; [[ -f "$T/daemon.log" ]] && tail -8 "$T/daemon.log"; exit 1; }
api() { curl -s -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
jget() {
  python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"
}

cargo build -p fluxctl -j 4 2>&1 | tail -1
RUNNER="$(find target/debug/build -path '*fluxvm-apple-*/out/fluxvm-vz-runner' -type f -exec ls -t {} + | head -1)"
[[ -x "$RUNNER" ]] || { echo "vz runner not built" >&2; exit 2; }
cat > "$T/fluxvm.toml" <<EOT
listen = "127.0.0.1:$PORT"
state_dir = "$T/state"
run_dir = "/tmp/fluxvm-run-agent-live"
[auth]
require = true
[[auth.tokens]]
name = "live"
token = "$TOKEN"
role = "admin"
[catalog]
path = "$T/catalog.json"
[sandbox]
egress_proxy_listen = "127.0.0.1:$PROXY"
egress_tls_intercept = true
egress_ca_cert = "$T/ca.crt"
egress_ca_key = "$T/ca.key"
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "$U/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "$U/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

# 1. A session limited to GET on two hosts, over TLS.
H="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live-https","policy":{"network":{"allow":[{"host":"example.com","methods":["GET"],"paths":["/*"]},{"host":"httpbin.org","methods":["GET"],"paths":["/*"]}]}}}')"
S="$(echo "$H" | jget 'd.get("id","")')"
[[ -n "$S" && "$(echo "$H" | jget 'd["capabilities"]["http_policy"]')" == True ]] || bad "session: $H"
ok "session with HTTP rules created"
[[ -s "$T/ca.crt" ]] && ok "the proxy created its CA" || bad "no CA at $T/ca.crt"
CA="$(cat "$T/ca.crt")"
xe() { api -X POST "$U/v1/agents/sessions/$S/exec" \
  -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": 60}))' "$1")"; }
xe "cat > /tmp/ca.pem <<'PEM'
$CA
PEM" >/dev/null
code() { xe "curl -s -o /dev/null -m 25 --cacert /tmp/ca.pem -x http://127.0.0.1:3128 -w '%{http_code}' $1" | jget 'd.get("stdout","").strip()'; }

# 2. TLS is judged per request.
[[ "$(code https://example.com/)" == 200 ]] && ok "HTTPS GET example.com allowed" || bad "https GET example.com"
[[ "$(code '-X POST https://example.com/')" == 403 ]] && ok "HTTPS POST refused by the method rule" || bad "https POST"
# A refused CONNECT never reaches a response, so curl reports 000; a refused request inside the tunnel is 403.
IANA="$(code https://iana.org/)"
[[ "$IANA" == 403 || "$IANA" == 000 ]] && ok "an unlisted HTTPS host is refused" || bad "https iana.org returned $IANA"
NOCA="$(xe "curl -s -o /dev/null -m 25 -x http://127.0.0.1:3128 -w '%{http_code}' https://example.com/" | jget 'd.get("stdout","").strip()')"
[[ "$NOCA" == 000 ]] && ok "a client that does not trust the CA fails" || bad "no-CA request returned $NOCA"

# 3. A credential grant: the proxy adds the header, the guest never sees the value.
SECRET="Bearer live-secret-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
G="$(api -X POST "$U/v1/agents/sessions/$S/grants" \
  -d "{\"secret_ref\":\"live\",\"value\":\"$SECRET\",\"hosts\":[\"httpbin.org\"],\"methods\":[\"GET\"],\"paths\":[\"/headers\"],\"ttl_seconds\":300}")"
GID="$(echo "$G" | jget 'd.get("id","")')"
[[ -n "$GID" && "$G" != *"$SECRET"* ]] && ok "grant created; the response has no secret" || bad "grant: $G"
SEEN="$(xe "curl -s -m 25 --cacert /tmp/ca.pem -x http://127.0.0.1:3128 https://httpbin.org/headers" | jget 'd.get("stdout","")')"
[[ "$SEEN" == *"$SECRET"* ]] && ok "the upstream saw the injected Authorization header" || bad "no injected header: $SEEN"
OTHER="$(xe "curl -s -m 25 --cacert /tmp/ca.pem -x http://127.0.0.1:3128 https://httpbin.org/get" | jget 'd.get("stdout","")')"
[[ "$OTHER" != *"$SECRET"* && "$OTHER" == *httpbin* ]] && ok "a path outside the grant carries no credential" || bad "credential leaked to /get: $OTHER"
ENV="$(xe 'env; cat /tmp/ca.pem >/dev/null' | jget 'd.get("stdout","")')"
[[ "$ENV" != *"$SECRET"* ]] && ok "the secret is not in the guest environment" || bad "secret visible in the guest"
LEDGER="$(api "$U/v1/agents/sessions/$S/events")"
[[ "$LEDGER" != *"$SECRET"* ]] && ok "the secret is not in the ledger" || bad "secret in the ledger"
api -X DELETE "$U/v1/agents/sessions/$S/grants/$GID" >/dev/null
AFTER="$(xe "curl -s -m 25 --cacert /tmp/ca.pem -x http://127.0.0.1:3128 https://httpbin.org/headers" | jget 'd.get("stdout","")')"
[[ "$AFTER" != *"$SECRET"* ]] && ok "a revoked grant injects nothing" || bad "revoked grant still injected"

echo "all $pass checks passed"
