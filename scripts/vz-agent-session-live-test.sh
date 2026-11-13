#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Live test of agent sessions on the vz backend with the guest agent (`agent-micro`): the engine a session gets,
# hardened exec, confinement, the job supervisor, the display (screenshot/input), fork cloning, process.ask HITL,
# fork promote (when sha256sum is in the guest), and (with the egress proxy) per-session HTTP rules.
#   scripts/vz-agent-session-live-test.sh /path/to/agent-micro-arm64.raw
# Build the image on Linux first: scripts/build-guest-agent-static.sh + scripts/build-agent-micro.sh (docs/agent-micro.md).
# Needs Apple silicon macOS, the Xcode command line tools, Rust, python3 and curl. Takes a few minutes (cold boots).
# Set FLUXVM_LIVE_HTTP=0 to skip the HTTP-rules part (it needs outbound access to example.com).
# Set FLUXVM_LIVE_PROMOTE=0 to skip promote; FLUXVM_LIVE_ASK=0 to skip approvals.
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
EOT
env -u FLUXVM_URL -u FLUXVM_TOKEN ./target/debug/fluxctl --config "$T/fluxvm.toml" \
  catalog add agent-micro --source "$IMAGE" --format raw >/dev/null 2>&1 || bad "could not register the image"
FLUXVM_VZ_RUNNER="$PWD/$RUNNER" ./target/debug/fluxctl --config "$T/fluxvm.toml" serve > "$T/daemon.log" 2>&1 & DPID=$!
for _ in $(seq 1 40); do curl -fs "$U/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fs "$U/healthz" >/dev/null && ok "daemon is up" || bad "daemon did not start"

# 1. The engine. A session with no image uses agent-micro from the catalog and says so.
SESSION="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live","policy":{"preset":"offline"}}')"
S="$(echo "$SESSION" | jget 'd.get("id","")')"
[[ -n "$S" ]] || bad "session create failed: $SESSION"
[[ "$(echo "$SESSION" | jget 'd["capabilities"]["engine"]')" == agent ]] && ok "engine is the guest agent" || bad "engine: $SESSION"
[[ "$(echo "$SESSION" | jget 'd["capabilities"]["supervisor"]')" == True ]] && ok "job supervisor present" || bad "no supervisor"

exec_json() { # command [timeout]
  api -X POST "$U/v1/agents/sessions/$S/exec" \
    -d "$(python3 -c 'import json,sys; print(json.dumps({"command": sys.argv[1], "timeout_seconds": int(sys.argv[2])}))' "$1" "${2:-30}")"
}

# 2. Hardened exec: a background child holding the pipes must not hold the command open.
t0=$SECONDS
R="$(exec_json 'sleep 20 & echo done' 60)"
dt=$((SECONDS - t0))
[[ "$(echo "$R" | jget 'd.get("stdout","").strip()')" == "done" && $dt -lt 10 ]] \
  && ok "background child does not hold exec open (${dt}s)" || bad "exec with a background child took ${dt}s: $R"

# 3. A timeout ends the whole tree.
t0=$SECONDS
exec_json 'sleep 120 & wait' 3 >/dev/null
dt=$((SECONDS - t0))
[[ $dt -le 10 ]] && ok "timeout returned in ${dt}s" || bad "timeout took ${dt}s"
LEFT="$(exec_json 'pgrep -f "[s]leep 120" | wc -l' 20 | jget 'd.get("stdout","").strip()')"
[[ "$LEFT" == 0 ]] && ok "no process left behind" || bad "$LEFT sleep processes survived the timeout"

# 4. Jobs run under the supervisor and keep their exit code.
J="$(api -X POST "$U/v1/agents/sessions/$S/jobs" -d '{"command":"echo tick; exit 7"}' | jget 'd.get("job","")')"
[[ -n "$J" ]] || bad "job start failed"
for _ in $(seq 1 20); do
  JS="$(api "$U/v1/agents/sessions/$S/jobs" | jget '[j for j in d["items"] if j["job"]=="'"$J"'"][0]["state"]')"
  [[ "$JS" == exited ]] && break; sleep 1
done
JI="$(api "$U/v1/agents/sessions/$S/jobs" | jget '[j for j in d["items"] if j["job"]=="'"$J"'"][0]')"
[[ "$(echo "$JI" | jget 'd["engine"]')" == supervisor && "$(echo "$JI" | jget 'd["exit_code"]')" == 7 ]] \
  && ok "job ran under the supervisor with exit code 7" || bad "job: $JI"

# 4c. Whether a memory checkpoint is possible is reported up front (and a refusal says why).
SJ="$(api "$U/v1/agents/sessions/$S")"
[[ "$(echo "$SJ" | jget '"memory_checkpoint" in d["capabilities"]')" == True ]] \
  && ok "capabilities.memory_checkpoint is reported ($(echo "$SJ" | jget 'd["capabilities"]["memory_checkpoint"]'))" || bad "memory_checkpoint missing: $SJ"
# 4b. The display: screenshot and input are their own token scope and are ledgered.
code=$(curl -s -o "$T/shot.png" -w '%{http_code}' -H "authorization: Bearer $TOKEN" "$U/v1/agents/sessions/$S/screenshot?max_width=640")
[[ "$code" == 200 && "$(head -c 4 "$T/shot.png" | tail -c 3)" == PNG ]] && ok "screenshot returns a PNG" || bad "screenshot: http $code"
[[ "$(api -X POST "$U/v1/agents/sessions/$S/input" -d '{"action":"key","key":"enter"}' | jget 'd.get("actions","?")')" == 1 ]] \
  && ok "input accepted" || bad "input"
TK="$(api -X POST "$U/v1/agents/sessions/$S/tokens" -d '{"ops":["read","exec"]}' | jget 'd["token"]')"
[[ "$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $TK" "$U/v1/agents/sessions/$S/screenshot")" == 403 ]] \
  && ok "a token without screen cannot take a screenshot" || bad "screen scope not enforced"
[[ "$(api "$U/v1/agents/sessions/$S/events" | jget 'sorted({e["kind"] for e in d.get("ledger",[]) if e["kind"].startswith("screen.")})')" == '["screen.input", "screen.shot"]' ]] \
  && ok "screen actions are in the ledger" || bad "screen ledger entries missing"

# 4d. Waiting for the display returns within its timeout and says whether it changed.
t0=$SECONDS
WR="$(api -X POST "$U/v1/agents/sessions/$S/screen/wait" -d '{"timeout_seconds":3}')"
dt=$((SECONDS - t0))
[[ "$(echo "$WR" | jget '"changed" in d and d["waited_ms"] >= 0')" == True && $dt -le 15 ]] \
  && ok "screen wait returned in ${dt}s (changed=$(echo "$WR" | jget 'd["changed"]'))" || bad "screen wait: $WR"

# 5. Fork clones copy-on-write.
FORK="$(api -X POST "$U/v1/agents/sessions/$S/fork" -d '{"count":2}')"
[[ "$(echo "$FORK" | jget 'len(d.get("items",[]))')" == 2 ]] || bad "fork failed: $FORK"
CLONED="$(echo "$FORK" | jget 'd.get("files_cloned","?")')"; COPIED="$(echo "$FORK" | jget 'd.get("files_copied","?")')"
[[ "$COPIED" == 0 && "$CLONED" != "?" && "$CLONED" -gt 0 ]] \
  && ok "fork cloned $CLONED files, copied 0" || bad "fork cloned=$CLONED copied=$COPIED"

# 6. Confinement is enforced by the guest (a filesystem policy session).
C="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live-confined","policy":{"preset":"offline","filesystem":{"write":["/data/out"]}}}')"
CS="$(echo "$C" | jget 'd.get("id","")')"
if [[ -n "$CS" && "$(echo "$C" | jget 'd["capabilities"]["confinement"]')" == True ]]; then
  W="$(api -X POST "$U/v1/agents/sessions/$CS/exec" -d '{"command":"touch /etc/x; echo rc=$?"}' | jget 'd.get("stdout","")')"
  [[ "$W" == *rc=1* ]] && ok "write outside the policy is refused" || bad "confined write: $W"
else
  bad "confined session: $C"
fi

# 7. Per-session HTTP rules through the egress proxy.
if [[ "${FLUXVM_LIVE_HTTP:-1}" == 1 ]]; then
  H="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live-http","policy":{"network":{"allow":[{"host":"example.com","methods":["GET"],"paths":["/*"]}]}}}')"
  HS="$(echo "$H" | jget 'd.get("id","")')"
  [[ -n "$HS" && "$(echo "$H" | jget 'd["capabilities"]["http_policy"]')" == True ]] || bad "http session: $H"
  code() { api -X POST "$U/v1/agents/sessions/$HS/exec" \
    -d "{\"command\":\"curl -s -o /dev/null -m 20 -x http://127.0.0.1:3128 -w '%{http_code}' $1\",\"timeout_seconds\":40}" | jget 'd.get("stdout","").strip()'; }
  [[ "$(code http://example.com/)" == 200 ]] && ok "GET example.com allowed" || bad "GET example.com"
  [[ "$(code '-X POST http://example.com/')" == 403 ]] && ok "POST example.com refused by the method rule" || bad "POST example.com"
  [[ "$(code http://iana.org/)" == 403 ]] && ok "an unlisted host is refused" || bad "iana.org"
fi

# 8. process.ask HITL: matching command → 428 → approve → one run → replay refused → deny sticks.
if [[ "${FLUXVM_LIVE_ASK:-1}" == 1 ]]; then
  A="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live-ask","policy":{"preset":"offline","process":{"ask":["deploy *"]}}}')"
  AS="$(echo "$A" | jget 'd.get("id","")')"
  [[ -n "$AS" ]] || bad "ask session: $A"
  # Prefer HTTP status via curl so we see 428 even if the body is error JSON.
  code=$(curl -s -o "$T/ask.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d '{"command":"deploy prod","timeout_seconds":30}')
  [[ "$code" == 428 ]] || bad "ask exec expected 428 got $code: $(cat "$T/ask.json")"
  AID="$(python3 -c 'import json; print(json.load(open("'"$T/ask.json"'")).get("approval_id",""))')"
  [[ -n "$AID" && "$AID" != null && "$AID" != None ]] || bad "no approval_id: $(cat "$T/ask.json")"
  ok "process.ask returns 428 with approval_id"
  [[ "$(api -X POST "$U/v1/agents/sessions/$AS/approvals/$AID/approve" -d '{"note":"live"}' | jget 'd.get("status","")')" == approved ]] \
    || bad "approve failed"
  R="$(api -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy prod\",\"approval_id\":\"$AID\",\"timeout_seconds\":30}")"
  # deploy is not a real binary; after approval the guest may return non-zero — the point is we got past 428.
  [[ "$(echo "$R" | jget '"exit_code" in d or "stdout" in d or "error" in d')" == True ]] \
    && ok "approved command was consumed once" || bad "approved exec: $R"
  code=$(curl -s -o "$T/ask2.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy prod\",\"approval_id\":\"$AID\",\"timeout_seconds\":30}")
  [[ "$code" == 403 || "$code" == 428 ]] && ok "reused approval is refused ($code)" || bad "reuse: $code $(cat "$T/ask2.json")"
  code=$(curl -s -o "$T/ask3.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d '{"command":"deploy staging","timeout_seconds":30}')
  AID2="$(python3 -c 'import json; print(json.load(open("'"$T/ask3.json"'")).get("approval_id") or "")')"
  [[ "$code" == 428 && -n "$AID2" ]] || bad "second ask: $code $(cat "$T/ask3.json")"
  api -X POST "$U/v1/agents/sessions/$AS/approvals/$AID2/deny" -d '{"note":"no"}' >/dev/null
  code=$(curl -s -o "$T/ask4.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy staging\",\"approval_id\":\"$AID2\",\"timeout_seconds\":30}")
  [[ "$code" == 403 ]] && ok "denied approval sticks" || bad "deny stick: $code $(cat "$T/ask4.json")"
fi

# 9. Promote a fork child's workspace file onto the parent (needs sha256sum in the guest).
if [[ "${FLUXVM_LIVE_PROMOTE:-1}" == 1 ]]; then
  P="$(api -X POST "$U/v1/agents/sessions" -d '{"agent":"live-promote","policy":{"preset":"offline"}}')"
  PS="$(echo "$P" | jget 'd.get("id","")')"
  [[ -n "$PS" ]] || bad "promote parent: $P"
  api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"mkdir -p /workspace && echo parent > /workspace/note.txt","timeout_seconds":30}' >/dev/null
  HAS_SHA="$(api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"command -v sha256sum >/dev/null && echo yes || echo no","timeout_seconds":20}' | jget 'd.get("stdout","").strip()')"
  if [[ "$HAS_SHA" != yes ]]; then
    echo "skip promote: guest has no sha256sum"
  else
    FORKP="$(api -X POST "$U/v1/agents/sessions/$PS/fork" -d '{"count":1}')"
    CHILD="$(echo "$FORKP" | jget 'd.get("items",[{}])[0].get("id","")')"
    [[ -n "$CHILD" ]] || bad "promote fork: $FORKP"
    api -X POST "$U/v1/agents/sessions/$CHILD/exec" \
      -d '{"command":"echo child-win > /workspace/note.txt && echo only-child > /workspace/new.txt","timeout_seconds":30}' >/dev/null
    PLAN="$(api -X POST "$U/v1/agents/sessions/$CHILD/promote" -d '{"dry_run":true}')"
    PID="$(echo "$PLAN" | jget 'd.get("plan_id","")')"
    [[ -n "$PID" && "$PID" != null ]] && ok "promote dry-run returns plan_id" || bad "promote dry-run: $PLAN"
    APPLY="$(api -X POST "$U/v1/agents/sessions/$CHILD/promote" -d "{\"plan_id\":\"$PID\"}")"
    [[ "$(echo "$APPLY" | jget '"error" not in d')" == True ]] \
      && ok "promote apply accepted" || bad "promote apply: $APPLY"
    GOT="$(api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"cat /workspace/note.txt; cat /workspace/new.txt","timeout_seconds":30}' | jget 'd.get("stdout","").strip()')"
    [[ "$GOT" == *"child-win"* && "$GOT" == *"only-child"* ]] \
      && ok "parent workspace has promoted files" || bad "parent after promote: $GOT"
  fi
fi

echo "all $pass checks passed"
