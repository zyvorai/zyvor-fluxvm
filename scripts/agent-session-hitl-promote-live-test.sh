#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Prove process.ask HITL and fork promote against a running FluxVM daemon (Linux KVM or Mac vz).
#   FLUXVM_URL=http://127.0.0.1:7788 FLUXVM_TOKEN=… IMAGE=agent-micro \
#     scripts/agent-session-hitl-promote-live-test.sh
# Optional: FLUXVM_LIVE_ASK=0 / FLUXVM_LIVE_PROMOTE=0 to skip a section.
set -euo pipefail
U="${FLUXVM_URL:?set FLUXVM_URL}"
TOKEN="${FLUXVM_TOKEN:?set FLUXVM_TOKEN}"
IMAGE="${IMAGE:-agent-micro}"
T="$(mktemp -d "${TMPDIR:-/tmp}/fluxvm-hitl-promote.XXXXXX")"
trap 'rm -rf "$T"' EXIT
pass=0
ok() { echo "ok   $*"; pass=$((pass + 1)); }
bad() { echo "FAIL $*"; exit 1; }
api() { curl -s -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
jget() {
  python3 -c 'import sys,json; d=json.load(sys.stdin); r=eval(sys.argv[1], {"d": d}); print(json.dumps(r) if isinstance(r,(dict,list)) else r)' "$1"
}

curl -fsS -m 5 "$U/healthz" >/dev/null || bad "daemon not healthy at $U"

if [[ "${FLUXVM_LIVE_ASK:-1}" == 1 ]]; then
  A="$(api -X POST "$U/v1/agents/sessions" -d "{\"agent\":\"hitl-ask\",\"runtime\":{\"image\":\"$IMAGE\"},\"policy\":{\"preset\":\"offline\",\"process\":{\"ask\":[\"deploy *\"]}}}")"
  AS="$(echo "$A" | jget 'd.get("id","")')"
  [[ -n "$AS" && "$AS" != null ]] || bad "ask session: $A"
  code=$(curl -s -o "$T/ask.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d '{"command":"deploy prod","timeout_seconds":30}')
  [[ "$code" == 428 ]] || bad "expected 428 got $code: $(cat "$T/ask.json")"
  AID="$(python3 -c 'import json; print(json.load(open("'"$T/ask.json"'")).get("approval_id") or "")')"
  [[ -n "$AID" ]] || bad "no approval_id"
  ok "process.ask → 428"
  api -X POST "$U/v1/agents/sessions/$AS/approvals/$AID/approve" -d '{"note":"live"}' >/dev/null
  api -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy prod\",\"approval_id\":\"$AID\",\"timeout_seconds\":30}" >/dev/null || true
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy prod\",\"approval_id\":\"$AID\",\"timeout_seconds\":30}")
  [[ "$code" == 403 || "$code" == 428 ]] && ok "approval single-use ($code)" || bad "reuse $code"
  code=$(curl -s -o "$T/ask2.json" -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d '{"command":"deploy staging","timeout_seconds":30}')
  AID2="$(python3 -c 'import json; print(json.load(open("'"$T/ask2.json"'")).get("approval_id") or "")')"
  [[ "$code" == 428 && -n "$AID2" ]] || bad "second ask"
  api -X POST "$U/v1/agents/sessions/$AS/approvals/$AID2/deny" -d '{}' >/dev/null
  code=$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -X POST "$U/v1/agents/sessions/$AS/exec" -d "{\"command\":\"deploy staging\",\"approval_id\":\"$AID2\",\"timeout_seconds\":30}")
  [[ "$code" == 403 ]] && ok "deny sticks" || bad "deny $code"
  api -X DELETE "$U/v1/agents/sessions/$AS" >/dev/null || true
fi

if [[ "${FLUXVM_LIVE_PROMOTE:-1}" == 1 ]]; then
  P="$(api -X POST "$U/v1/agents/sessions" -d "{\"agent\":\"hitl-promote\",\"runtime\":{\"image\":\"$IMAGE\"},\"policy\":{\"preset\":\"offline\"}}")"
  PS="$(echo "$P" | jget 'd.get("id","")')"
  [[ -n "$PS" && "$PS" != null ]] || bad "promote parent: $P"
  api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"mkdir -p /workspace && echo parent > /workspace/note.txt","timeout_seconds":60}' >/dev/null
  HAS_SHA="$(api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"command -v sha256sum >/dev/null && echo yes || echo no","timeout_seconds":30}' | jget 'd.get("stdout","").strip()')"
  if [[ "$HAS_SHA" != yes ]]; then
    echo "skip promote: guest has no sha256sum (image=$IMAGE)"
  else
    FORKP="$(api -X POST "$U/v1/agents/sessions/$PS/fork" -d '{"count":1}')"
    CHILD="$(echo "$FORKP" | jget 'd.get("items",[{}])[0].get("id","")')"
    [[ -n "$CHILD" && "$CHILD" != null ]] || bad "fork: $FORKP"
    api -X POST "$U/v1/agents/sessions/$CHILD/exec" \
      -d '{"command":"echo child-win > /workspace/note.txt && echo only-child > /workspace/new.txt","timeout_seconds":60}' >/dev/null
    PLAN="$(api -X POST "$U/v1/agents/sessions/$CHILD/promote" -d '{"dry_run":true}')"
    PID="$(echo "$PLAN" | jget 'd.get("plan_id","")')"
    [[ -n "$PID" && "$PID" != null && "$PID" != "" ]] && ok "promote dry-run" || bad "dry-run: $PLAN"
    APPLY="$(api -X POST "$U/v1/agents/sessions/$CHILD/promote" -d "{\"plan_id\":\"$PID\"}")"
    [[ "$(echo "$APPLY" | jget '"error" not in d')" == True ]] && ok "promote apply" || bad "apply: $APPLY"
    GOT="$(api -X POST "$U/v1/agents/sessions/$PS/exec" -d '{"command":"cat /workspace/note.txt; cat /workspace/new.txt","timeout_seconds":30}' | jget 'd.get("stdout","").strip()')"
    [[ "$GOT" == *"child-win"* && "$GOT" == *"only-child"* ]] && ok "parent has promoted files" || bad "parent: $GOT"
  fi
  api -X DELETE "$U/v1/agents/sessions/$PS" >/dev/null || true
fi

echo "all $pass checks passed"
