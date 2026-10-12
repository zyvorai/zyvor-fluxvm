#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
python3 - <<'PY'
from pathlib import Path
root=Path('.')
required=[
 'crates/fluxvm-mesh/src/public_relay.rs','crates/fluxvm-mesh/src/public_relay_main.rs',
 'crates/fluxvm-mesh/src/dns01.rs','crates/fluxvm-mesh/src/telemetry.rs',
 'crates/fluxvm-mesh/src/metrics_main.rs','docs/fluxmesh-phase6.md',
 'packaging/systemd/fluxmesh-public-relay.service','packaging/systemd/fluxmesh-metrics.service',
 'docs/examples/fluxmesh-relay-server.json'
]
for f in required: assert (root/f).is_file(),f
lib=(root/'crates/fluxvm-mesh/src/lib.rs').read_text()
for x in ['pub mod dns01;','pub mod public_relay;','pub mod telemetry;']: assert x in lib,x
cargo=(root/'crates/fluxvm-mesh/Cargo.toml').read_text()
for x in ['name = "fluxmesh-public-relay"','name = "fluxmesh-metrics"','hyper =','hyper-util =','http-body-util =']: assert x in cargo,x
ing=(root/'crates/fluxvm-mesh/src/ingress.rs').read_text()
for x in ['alpn_protocols','b"h2"','proxy_h2_connection','serve_connection_with_upgrades']: assert x in ing,x
relay=(root/'crates/fluxvm-mesh/src/public_relay.rs').read_text()
for x in ['REGISTER\\t','DATA\\t','parse_client_hello_sni','copy_bidirectional','heartbeat_fresh','sites: Vec<String>']: assert x in relay,x
acme=(root/'crates/fluxvm-mesh/src/acme.rs').read_text()
for x in ['obtain_dns01','fulfill_dns01','_acme-challenge','validate_domains_dns01']: assert x in acme,x
pub=(root/'crates/fluxvm-mesh/src/public.rs').read_text()
for x in ['Dns01Provider','AcmeChallenge','PublicRelayConfig','public_relay: Option']: assert x in pub,x
mdns=(root/'crates/fluxvm-mesh/src/mdns.rs').read_text()
for x in ['run_bridge_dual','ff02::fb','ipv6_ifindex']: assert x in mdns,x
cloud=(root/'crates/fluxctl/src/cloud.rs').read_text()
for x in ['AcmeDns01Cloudflare','RelayConfigure','RelayConnect','CloudCommand::Telemetry']: assert x in cloud,x
print('FluxMesh Phase 6 structural checks: PASS')
PY
python3 - <<'PY'
from pathlib import Path
files=[
 'public_relay.rs','public_relay_main.rs','dns01.rs','telemetry.rs','metrics_main.rs','public.rs','acme.rs','ingress.rs','mdns.rs','mdns_main.rs','edge.rs','edge_main.rs','lib.rs'
]
files=[Path('crates/fluxvm-mesh/src')/x for x in files]+[Path('crates/fluxctl/src/cloud.rs')]
for p in files:
 s=p.read_text()
 assert s.count('{')==s.count('}'),f'brace mismatch {p}'
 assert s.count('(')==s.count(')'),f'paren mismatch {p}'
print('Rust delimiter sanity checks: PASS')
PY
if command -v cargo >/dev/null 2>&1; then
  cargo check -p fluxvm-mesh --all-targets
  cargo test -p fluxvm-mesh --all-targets
  cargo check -p fluxctl
else
  echo 'SKIP: cargo not installed in this environment'
fi
