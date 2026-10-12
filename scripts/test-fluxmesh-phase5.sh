#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

python3 - <<'PY'
from pathlib import Path
root=Path.cwd()
required=[
 root/'crates/fluxvm-mesh/src/acme.rs',
 root/'crates/fluxvm-mesh/src/ddns.rs',
 root/'crates/fluxvm-mesh/src/diagnose.rs',
 root/'crates/fluxvm-mesh/src/edge.rs',
 root/'crates/fluxvm-mesh/src/edge_main.rs',
 root/'crates/fluxvm-mesh/src/ingress.rs',
 root/'crates/fluxvm-mesh/src/ingress_main.rs',
 root/'crates/fluxvm-mesh/src/mdns.rs',
 root/'crates/fluxvm-mesh/src/mdns_main.rs',
 root/'crates/fluxvm-mesh/src/pd.rs',
 root/'crates/fluxvm-mesh/src/pd_main.rs',
 root/'crates/fluxvm-mesh/src/public.rs',
 root/'crates/fluxvm-mesh/src/router.rs',
 root/'crates/fluxvm-mesh/src/share.rs',
 root/'crates/fluxvm-mesh/src/wan.rs',
 root/'docs/fluxmesh-phase5.md',
 root/'packaging/systemd/fluxmesh-edge.service',
]
for p in required: assert p.exists(),p
checks={
 'acme.rs':['RFC 8555','http-01','ECDSA_P256_SHA256_FIXED_SIGNING','serialize_request','POST-as-GET','renewal_due'],
 'ingress.rs':['serve_https','share_required','ChallengeStore','Reload per accepted connection'],
 'share.rs':['HMAC_SHA256','share token expired','token_from_path'],
 'ddns.rs':['Cloudflare','Webhook','bearer_auth'],
 'router.rs':['PCP','NAT-PMP','UPnP','AddPortMapping'],
 'mdns.rs':['224.0.0.251','OVERLAY_MDNS_PORT','SeenCache'],
 'pd.rs':['IA_PD','IAPREFIX','expected Advertise','route_plan'],
 'wan.rs':['Multi-WAN','linux_policy_plan','connect_timeout'],
 'edge.rs':['acme_renewed','router::reconcile','serve_https','WanState'],
 'diagnose.rs':['DiagnosticReport','https-ingress','wireguard-key'],
 'public.rs':['PublicCloudConfig','DdnsProvider','RouterProtocol','WanConfig'],
}
base=root/'crates/fluxvm-mesh/src'
for name,needles in checks.items():
    text=(base/name).read_text()
    for needle in needles: assert needle in text,(name,needle)
    for bad in ['todo!(', 'unimplemented!(', 'FIXME']: assert bad not in text,(name,bad)
lib=(base/'lib.rs').read_text()
for m in ['pub mod acme;','pub mod ddns;','pub mod edge;','pub mod ingress;','pub mod mdns;','pub mod pd;','pub mod public;','pub mod router;','pub mod share;','pub mod wan;']:
    assert m in lib,m
cargo=(root/'crates/fluxvm-mesh/Cargo.toml').read_text()
for x in ['name = "fluxmesh-ingress"','name = "fluxmesh-mdns"','name = "fluxmesh-pd"','name = "fluxmesh-edge"','reqwest =','rcgen =','serde_json =']:
    assert x in cargo,x
cloud=(root/'crates/fluxctl/src/cloud.rs').read_text()
for x in ['ServeIngress','DdnsCloudflare','RouterMap','WanAdd','CloudCommand::Diagnose','issue_share_token']:
    assert x in cloud,x
print('FluxMesh Phase 5 structural checks: PASS')
PY

# Delimiter balance is left to rustc and rustfmt (a character count miscounts bracket literals in formatted code).

if command -v cargo >/dev/null 2>&1; then
  cargo check -p fluxvm-mesh --all-targets
  cargo test -p fluxvm-mesh --all-targets
  cargo check -p fluxctl
else
  echo 'cargo unavailable: SKIP Rust compilation'
fi
