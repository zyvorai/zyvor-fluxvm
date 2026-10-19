#!/usr/bin/env bash
# shellcheck disable=SC2015,SC2016,SC2034,SC2154,SC2329,SC2086
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Tunnels through central with real processes (Linux and macOS): `fluxvm-agent central`, a `sim-node`, and the `fluxctl fleet`
# client. A tenant leases the simulated Mac, starts a VM, and its bytes come back from the VM's echo server through
# `tunnel-stdio` and `port-forward`; `fleet tunnels` lists the open one; a port outside the policy, a read-only key, another
# tenant and a tunnel with no token are refused; `tunnels close` ends a forwarded connection. The WebSocket relay itself is
# covered in-process by `sim.rs`; this checks the client binaries and the HTTP surface against real sockets.
# Needs curl, jq, target/release/fluxvm-agent and fluxctl (BIN, FLUXCTL). Exit status = failed checks.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
BIN="${BIN:-target/release/fluxvm-agent}"; FX="${FLUXCTL:-target/release/fluxctl}"
[[ -x "$BIN" && -x "$FX" ]] || { echo "build $BIN and $FX first (or set BIN, FLUXCTL)" >&2; exit 2; }
D="$(mktemp -d)"; PORT=$((20000 + RANDOM % 20000)); URL="http://127.0.0.1:$PORT"; LP=$((PORT + 1)); fails=0; PIDS=()
trap 'for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null; done; rm -rf "$D"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); dump; }
# CI keeps no files: show what the client and central said, once, the first time a check fails
dump() { [[ -n "${DUMPED:-}" ]] && return; DUMPED=1; local f; for f in stdio.err ref.err pf.log sim.log; do [[ -s "$D/$f" ]] && { echo "--- $f"; tail -n 15 "$D/$f"; }; done; echo "--- central.log"; tail -n 15 "$D/central.log"; echo "--- env"; env | grep -i proxy; echo "--- fluxctl"; "$FX" --version 2>&1 | head -2; }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
adm() { curl -s -H 'authorization: Bearer adm' -H 'content-type: application/json' "$@"; }
as() { local k="$1"; shift; curl -s -H "authorization: Bearer $k" -H 'content-type: application/json' "$@"; }
code() { local k="$1"; shift; curl -s -o /dev/null -w '%{http_code}' -H "authorization: Bearer $k" -H 'content-type: application/json' "$@"; }
waitfor() { local i; for i in $(seq 1 "${2:-40}"); do eval "$1" && return 0; sleep 0.5; done; return 1; }
# refused CODE TOKEN VM PORT: the client exits non-zero and central answered CODE (not just any text containing a word)
refused() { local c="$1" tok="$2" vm="$3" port="$4" rc; (printf x; sleep 1) | "$FX" fleet --central "$URL" --token "$tok" tunnel-stdio "$vm" "$port" >/dev/null 2>"$D/ref.err"; rc=${PIPESTATUS[1]}; [[ $rc != 0 ]] && grep -q "failed ($c)" "$D/ref.err"; }
fl() { local tok="$1"; shift; "$FX" fleet --central "$URL" --token "$tok" "$@"; }

"$BIN" central --listen "127.0.0.1:$PORT" --state-dir "$D/central" --token adm >"$D/central.log" 2>&1 & PIDS+=($!)
check "central is up" waitfor "curl -sf $URL/healthz >/dev/null" 40
"$BIN" sim-node --name sim-mac --central "$URL" --token adm --interval-secs 1 >"$D/sim.log" 2>&1 & PIDS+=($!)
check "the simulated Mac registers" waitfor '[[ "$(adm '$URL'/fleet/nodes | jq -r "[.items[].name]|join(\",\")")" == sim-mac ]]' 40

TK="$(adm -XPOST -d '{"id":"acme"}' "$URL/fleet/tenants" | jq -r .token)"
OT="$(adm -XPOST -d '{"id":"globex"}' "$URL/fleet/tenants" | jq -r .token)"
check "two tenants with their keys" test -n "$TK" -a -n "$OT" -a "$TK" != "$OT"
check "a lease on the simulated Mac" test "$(code "$TK" -XPOST -d '{}' "$URL/fleet/leases")" = 201
R="$(as "$TK" -XPOST -d '{"backend":"vz","name":"m1","apple":{"guest_os":"macos"}}' "$URL/fleet/vms")"
ID="$(jq -r '.vm.id // empty' <<<"$R")"
check "a VM is started on the leased Mac" test -n "$ID"
[[ -n "$ID" ]] || { echo "no VM: $R" >&2; exit 1; }
VM="sim-mac/$ID"

# --- tunnel-stdio: the ProxyCommand form `fluxctl fleet ssh` uses --------------------------------------------------
GOT=""
for _ in 1 2 3; do   # the first connection can land before the node has the new VM in its list
  GOT="$( (printf 'SSH-2.0-e2e\r\n'; sleep 2) | fl "$TK" tunnel-stdio "$VM" 22 2>"$D/stdio.err" | head -c 13 | tr -d '\r\n')"
  [[ "$GOT" == "SSH-2.0-e2e" ]] && break
done
check "tunnel-stdio: bytes come back from the VM's port 22 through central" test "$GOT" = "SSH-2.0-e2e"
GOT="$( (printf 'by-name'; sleep 2) | fl "$TK" tunnel-stdio m1 22 2>/dev/null | head -c 7)"
check "tunnel-stdio finds the VM by its name" test "$GOT" = "by-name"
check "a port outside the policy (80) is refused (403)" refused 403 "$TK" "$VM" 80
check "another tenant cannot reach it (404)" refused 404 "$OT" "$VM" 22
check "no token: refused (401)" refused 401 "" "$VM" 22
RO="$(as "$TK" -XPOST -d '{"name":"viewer","read_only":true}' "$URL/fleet/tenants/me/keys" | jq -r .token)"
check "a read-only key may not open a tunnel (403, says why)" bash -c "$(declare -f refused); FX='$FX'; URL='$URL'; D='$D'; refused 403 '$RO' '$VM' 22 && grep -q 'is read-only' '$D/ref.err'"

# --- port-forward: a local port that carries one TCP connection per client ---------------------------------------
"$FX" fleet --central "$URL" --token "$TK" port-forward "$VM" "$LP:22" >"$D/pf.log" 2>&1 & PF=$!; PIDS+=("$PF")
check "port-forward listens on the local port" waitfor "(exec 3<>/dev/tcp/127.0.0.1/$LP) 2>/dev/null" 20
exec 4<>"/dev/tcp/127.0.0.1/$LP" && printf 'through-forward' >&4
read -r -t 5 -n 15 FW <&4; exec 4>&- 2>/dev/null
check "bytes sent to the local port come back from the VM" test "$FW" = "through-forward"
# a second concurrent connection
exec 5<>"/dev/tcp/127.0.0.1/$LP" && printf 'second-conn' >&5
read -r -t 5 -n 11 FW2 <&5
check "a second connection through the same forward works" test "$FW2" = "second-conn"
N="$(as "$TK" "$URL/fleet/tunnels" | jq '.items|length')"
check "fleet tunnels lists the open connection for its owner" test "$N" -ge 1
check "the listing shows the VM name, port and bytes" test "$(as "$TK" "$URL/fleet/tunnels" | jq -r '[.items[]|select(.vm_name=="m1" and .port==22 and .bytes_sent>0)]|length')" -ge 1
check "the other tenant sees none" test "$(as "$OT" "$URL/fleet/tunnels" | jq '.items|length')" = 0
check "the operator sees every tunnel" test "$(adm "$URL/fleet/tunnels" | jq '.items|length')" -ge 1
TID="$(as "$TK" "$URL/fleet/tunnels" | jq -r '.items[0].id')"
check "another tenant cannot close it (404)" test "$(code "$OT" -XDELETE "$URL/fleet/tunnels/$TID")" = 404
check "the owner closes it" test "$(code "$TK" -XDELETE "$URL/fleet/tunnels/$TID")" = 200
exec 5>&- 2>/dev/null
check "a closed tunnel leaves the listing" waitfor '[[ "$(as '$TK' '$URL'/fleet/tunnels | jq -r "[.items[].id]|index(\"'$TID'\") // \"none\"")" == none ]]' 20
check "opening and closing are in the activity log" bash -c "curl -s -H 'authorization: Bearer adm' '$URL/fleet/events' | jq -e '[.items[]|select(.path|test(\"tunnel\"))]|length > 0' >/dev/null"
kill "$PF" 2>/dev/null; wait "$PF" 2>/dev/null

# --- the VM goes away: tunnels to it are refused --------------------------------------------------------------------
check "the VM is deleted" test "$(code "$TK" -XDELETE "$URL/fleet/vms/sim-mac/$ID")" -lt 300
check "a tunnel to a deleted VM is refused (404)" refused 404 "$TK" "$VM" 22
check "no key or token in the central log" bash -c "! grep -q -F -e '$TK' -e '$OT' -e '$RO' '$D/central.log'"

echo; echo "$fails check(s) failed"; exit "$fails"
