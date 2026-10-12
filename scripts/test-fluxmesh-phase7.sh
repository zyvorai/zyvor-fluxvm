#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 - <<'PY1'
from pathlib import Path
root=Path('.')
required=[
 'crates/fluxvm-mesh/src/http3.rs','crates/fluxvm-mesh/src/dnssec.rs','crates/fluxvm-mesh/src/trace.rs',
 'crates/fluxvm-mesh/src/public_relay.rs','crates/fluxvm-mesh/src/edge.rs','crates/fluxvm-mesh/src/ingress.rs',
 'docs/fluxmesh-phase7.md','docs/examples/fluxmesh-relay-server.json'
]
for f in required: assert (root/f).is_file(),f
cargo=(root/'crates/fluxvm-mesh/Cargo.toml').read_text()
for x in ['h3 = "0.0.8"','h3-quinn = "0.0.10"','quinn = { version = "0.11.12"']: assert x in cargo,x
lib=(root/'crates/fluxvm-mesh/src/lib.rs').read_text()
for x in ['pub mod http3;','pub mod dnssec;','pub mod trace;']: assert x in lib,x
h3=(root/'crates/fluxvm-mesh/src/http3.rs').read_text()
for x in ['h3::server','H3QuinnConnection','QuicServerConfig','b"h3"','send_response','send_data','set_server_config','fluxmesh.ingress.request']: assert x in h3,x
ing=(root/'crates/fluxvm-mesh/src/ingress.rs').read_text()
for x in ['alt-svc','h3=\\\":443\\\"','http.protocol','"h2"']: assert x in ing,x
relay=(root/'crates/fluxvm-mesh/src/public_relay.rs').read_text()
for x in ['RelayTransport','Auto','Quic','TlsTcp','RELAY_QUIC_ALPN','serve_quic_tunnel','rendezvous_score','SITE_LEASE_SECS','touch_session']: assert x in relay,x
dns=(root/'crates/fluxvm-mesh/src/dnssec.rs').read_text()
for x in ['TYPE_OPT','DNSSEC_OK','FLAG_AD','require_ad','verify_txt']: assert x in dns,x
acme=(root/'crates/fluxvm-mesh/src/acme.rs').read_text()
for x in ['require_dnssec_ad','validation_resolver','verify_txt']: assert x in acme,x
trace=(root/'crates/fluxvm-mesh/src/trace.rs').read_text()
for x in ['resourceSpans','traceId','spanId','service.name','Duration::from_secs(5)']: assert x in trace,x
tele=(root/'crates/fluxvm-mesh/src/telemetry.rs').read_text()
for x in ['http3_connections','http3_requests','http3_failures','fluxmesh_edge_http3_requests_total']: assert x in tele,x
cloud=(root/'crates/fluxctl/src/cloud.rs').read_text()
for x in ['validation_resolver','require_dnssec_ad','quic_relay_addr','RelayTransport','CloudCommand::Otlp']: assert x in cloud,x
example=(root/'docs/examples/fluxmesh-relay-server.json').read_text()
for x in ['quic_tunnel_listen','active-active']: assert x in example,x
print('FluxMesh Phase 7 structural checks: PASS')
PY1
python3 - <<'PY2'
from pathlib import Path

def clean(src: str) -> str:
    out=[]; i=0; n=len(src); state='code'; block_depth=0
    while i<n:
        c=src[i]; d=src[i+1] if i+1<n else ''
        if state=='code':
            if c=='/' and d=='/': state='line'; out.extend('  '); i+=2; continue
            if c=='/' and d=='*': state='block'; block_depth=1; out.extend('  '); i+=2; continue
            if c=='"': state='string'; out.append(' '); i+=1; continue
            if c=="'":
                # Recognize only actual char literals; leave Rust lifetimes alone.
                if i+2<n and src[i+2]=="'": state='char'; out.append(' '); i+=1; continue
                if d=='\\' and i+3<n and src[i+3]=="'": state='char'; out.append(' '); i+=1; continue
            out.append(c); i+=1; continue
        if state=='line':
            if c=='\n': state='code'; out.append('\n')
            else: out.append(' ')
            i+=1; continue
        if state=='block':
            if c=='/' and d=='*': block_depth+=1; out.extend('  '); i+=2; continue
            if c=='*' and d=='/':
                block_depth-=1; out.extend('  '); i+=2
                if block_depth==0: state='code'
                continue
            out.append('\n' if c=='\n' else ' '); i+=1; continue
        if state=='string':
            if c=='\\' and i+1<n: out.extend('  '); i+=2; continue
            if c=='"': state='code'
            out.append(' '); i+=1; continue
        if state=='char':
            if c=='\\' and i+1<n: out.extend('  '); i+=2; continue
            if c=="'": state='code'
            out.append(' '); i+=1; continue
    return ''.join(out)

files=[
 Path('crates/fluxvm-mesh/src/http3.rs'), Path('crates/fluxvm-mesh/src/dnssec.rs'),
 Path('crates/fluxvm-mesh/src/trace.rs'), Path('crates/fluxvm-mesh/src/public_relay.rs'),
 Path('crates/fluxvm-mesh/src/edge.rs'), Path('crates/fluxvm-mesh/src/ingress.rs'),
 Path('crates/fluxvm-mesh/src/public.rs'), Path('crates/fluxvm-mesh/src/acme.rs'),
 Path('crates/fluxvm-mesh/src/telemetry.rs'), Path('crates/fluxctl/src/cloud.rs')]
for p in files:
    s=clean(p.read_text())
    for a,b in [('(',')'),('{','}'),('[',']')]:
        assert s.count(a)==s.count(b), f'{a}{b} mismatch {p}: {s.count(a)} != {s.count(b)}'
print('Rust delimiter sanity checks: PASS')
PY2
if grep -R -nE 'TODO|FIXME|unimplemented!|todo!' \
    crates/fluxvm-mesh/src/http3.rs crates/fluxvm-mesh/src/dnssec.rs crates/fluxvm-mesh/src/trace.rs; then
  echo 'unexpected unfinished marker' >&2
  exit 1
fi
if command -v cargo >/dev/null 2>&1; then
  cargo check -p fluxvm-mesh --all-targets
  cargo test -p fluxvm-mesh --all-targets
  cargo check -p fluxctl
else
  echo 'SKIP: cargo not installed in this environment'
fi
