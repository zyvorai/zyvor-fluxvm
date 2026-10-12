#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

./scripts/test-fluxmesh-phase2.sh

python3 - <<'PY'
from pathlib import Path
root = Path('.')
crate = root / 'crates/fluxvm-mesh'
required = {
    'src/stun.rs': ['XOR_MAPPED_ADDRESS', 'probe_many', 'punch_udp'],
    'src/tls_relay.rs': ['serve_tls', 'run_tls_client', 'TlsAcceptor', 'TlsConnector'],
    'src/reconcile.rs': ['reconcile_linux', 'rotate_wireguard_key', 'build_linux_reconcile_commands'],
    'src/fabric.rs': ['compile_network_fabric_cnp', '/v1/network/services', '/v1/network/cnp'],
    'src/macos.rs': ['MacTunnelPlan', 'NetworkExtension', 'write_atomic'],
    'src/agent.rs': ['reconcile_once', 'run_forever', 'PublicUdp'],
    'src/agent_main.rs': ['fluxmesh-agent', 'wg-rotate', 'cnp-plan'],
}
for rel, needles in required.items():
    text = (crate / rel).read_text()
    for needle in needles:
        assert needle in text, f'{rel}: missing {needle}'

cargo = (crate / 'Cargo.toml').read_text()
for needle in ['fluxmesh-agent', 'rustls =', 'rustls-pemfile', 'tokio-rustls']:
    assert needle in cargo, f'Cargo.toml: missing {needle}'

lib = (crate / 'src/lib.rs').read_text()
for module in ['agent', 'fabric', 'http', 'macos', 'reconcile', 'stun', 'tls_relay']:
    assert f'pub mod {module};' in lib, f'lib.rs: missing module {module}'

doc = (root / 'docs/fluxmesh-phase3.md').read_text()
for needle in ['native TLS relay', 'STUN', 'WireGuard reconciliation', 'Network Fabric', 'Service Fabric', 'macOS NetworkExtension']:
    assert needle in doc, f'phase3 doc: missing {needle}'

# Cheap lexical guard useful on builders without Rust. It ignores braces inside
# strings/comments imperfectly, so only reject obvious source damage.
for path in list((crate / 'src').glob('*.rs')):
    text = path.read_text()
    assert '\x00' not in text, f'{path}: NUL byte'
    assert 'TODO: implement' not in text, f'{path}: unfinished implementation marker'

print('FluxMesh phase 3 structural checks: PASS')
PY

if command -v cargo >/dev/null 2>&1; then
  cargo check -p fluxvm-mesh --all-targets
  cargo test -p fluxvm-mesh --all-targets
else
  echo 'cargo unavailable: SKIP Rust compilation/tests'
fi
