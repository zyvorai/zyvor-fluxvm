#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

python3 - <<'PY'
from pathlib import Path
root=Path.cwd()
required=[
 root/'crates/fluxvm-mesh/src/enroll.rs',
 root/'crates/fluxvm-mesh/src/enroll_tls.rs',
 root/'crates/fluxvm-mesh/src/enroll_main.rs',
 root/'crates/fluxvm-mesh/src/keydist.rs',
 root/'crates/fluxvm-mesh/src/ipv6.rs',
 root/'crates/fluxvm-mesh/src/dns_ha.rs',
 root/'crates/fluxvm-mesh/src/relay_ha.rs',
 root/'crates/fluxvm-mesh/src/multihome.rs',
 root/'crates/fluxvm-mesh/src/cloud.rs',
 root/'crates/fluxvm-mesh/src/dns_ha_main.rs',
 root/'crates/fluxctl/src/cloud.rs',
 root/'native/fluxmesh-macos/Sources/FluxMeshTunnel/PacketTunnelProvider.swift',
 root/'docs/fluxmesh-phase4.md',
]
for p in required:
    assert p.exists(), p

checks={
 'enroll.rs':['HMAC_SHA256','UsedNonceStore','token expired'],
 'enroll_tls.rs':['Native TLS zero-touch enrollment','JOIN','TlsAcceptor','TlsConnector'],
 'enroll_main.rs':['fluxmesh-enroll','wireguard_public_key'],
 'keydist.rs':['coordinated_local_rotation','AwaitingAcks','pending private key'],
 'ipv6.rs':['Ipv6Overlay','augment_wireguard_plan','/128'],
 'dns_ha.rs':['rendezvous','DnsReplicaSet'],
 'relay_ha.rs':['preferred_regions','capacity_percent','ordered'],
 'multihome.rs':['MultiHomeState','failures >= 3'],
 'cloud.rs':['pub fn up','issue_join_token','pending_key_rotations'],
}
base=root/'crates/fluxvm-mesh/src'
for name, needles in checks.items():
    text=(base/name).read_text()
    for needle in needles: assert needle in text,(name,needle)
    for bad in ['todo!(', 'unimplemented!(']: assert bad not in text,(name,bad)

lib=(base/'lib.rs').read_text()
for m in ['pub mod cloud;','pub mod dns_ha;','pub mod enroll;','pub mod enroll_tls;','pub mod ipv6;','pub mod keydist;','pub mod multihome;','pub mod relay_ha;']:
    assert m in lib,m
cargo=(root/'crates/fluxvm-mesh/Cargo.toml').read_text()
assert 'name = "fluxmesh-dns-ha"' in cargo
assert 'name = "fluxmesh-enroll"' in cargo
assert 'ring = "0.17"' in cargo
main=(base/'main.rs').read_text()
for c in ['"cloud-up"','"enroll-token"','"key-ack"','"ipv6-status"']:
    assert c in main,c
cloud=(root/'crates/fluxctl/src/cloud.rs').read_text()
for c in ['pub async fn run','ServeEnrollment','CloudCommand::Join','issue_open_join_token','wireguard_public_key']:
    assert c in cloud,c
swift=(root/'native/fluxmesh-macos/Sources/FluxMeshTunnel/PacketTunnelProvider.swift').read_text()
for x in ['NEPacketTunnelProvider','NEIPv6Settings','NEDNSSettings','EngineBridge']:
    assert x in swift,x
print('FluxMesh Phase 4 structural checks: PASS')
PY

# Lightweight delimiter sanity check. Cargo/rustc remains the authoritative compiler gate.
python3 - <<'PY2'
from pathlib import Path
files=[
 Path('crates/fluxvm-mesh/src/enroll.rs'), Path('crates/fluxvm-mesh/src/enroll_tls.rs'),
 Path('crates/fluxvm-mesh/src/enroll_main.rs'), Path('crates/fluxvm-mesh/src/keydist.rs'),
 Path('crates/fluxvm-mesh/src/ipv6.rs'), Path('crates/fluxvm-mesh/src/dns_ha.rs'),
 Path('crates/fluxvm-mesh/src/relay_ha.rs'), Path('crates/fluxvm-mesh/src/multihome.rs'),
 Path('crates/fluxvm-mesh/src/cloud.rs'), Path('crates/fluxvm-mesh/src/dns_ha_main.rs'),
 Path('crates/fluxctl/src/cloud.rs')]
for p in files:
    s=p.read_text()
    assert s.count('{')==s.count('}'), f'brace mismatch {p}'
    assert s.count('(')==s.count(')'), f'paren mismatch {p}'
    assert s.count('[')==s.count(']'), f'bracket mismatch {p}'
print('Rust delimiter sanity checks: PASS')
PY2

if command -v cargo >/dev/null 2>&1; then
  cargo check -p fluxvm-mesh --all-targets
  cargo test -p fluxvm-mesh --all-targets
  cargo check -p fluxctl
else
  echo 'cargo unavailable: SKIP Rust compilation'
fi

if command -v swiftc >/dev/null 2>&1; then
  swiftc -parse native/fluxmesh-macos/Sources/FluxMeshTunnel/*.swift
  echo 'Swift syntax parse: PASS'
else
  echo 'swiftc unavailable: SKIP NetworkExtension syntax parse'
fi
