#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

./scripts/test-fluxmesh.sh

python3 - <<'PY'
from pathlib import Path
root = Path.cwd()
src = root / "crates/fluxvm-mesh/src"
required = {
    "service.rs": ["pub struct ServiceCatalog", "pub struct ServiceFabricPlan", "to_json"],
    "dns.rs": ["pub struct PrivateDns", "answer_packet", "serve_udp"],
    "acl.rs": ["pub struct AclRule", "pub fn evaluate", "AclAction::Deny"],
    "gateway.rs": ["pub struct GatewayPolicies", "linux_plan", "policy drop"],
    "relay.rs": ["pub struct RelayFrame", "serve_tcp", "run_tcp_client"],
    "catalog.rs": ["pub struct CloudCatalog", "pub struct CatalogStore", "CATALOG_VERSION"],
    "relay_main.rs": ["fluxmesh-relay", "serve", "client"],
}
for name, needles in required.items():
    text = (src / name).read_text()
    for needle in needles:
        assert needle in text, (name, needle)
    assert "TODO" not in text, name

main = (src / "main.rs").read_text()
for cmd in (
    '"service-add"', '"service-fabric-plan"', '"dns-serve"', '"acl-add"',
    '"exit-enable"', '"subnet-policy"', '"gateway-plan"', '"relay-add"',
):
    assert cmd in main, cmd

cargo = (root / "crates/fluxvm-mesh/Cargo.toml").read_text()
assert 'name = "fluxmesh-relay"' in cargo
assert 'path = "src/relay_main.rs"' in cargo
print("FluxMesh Phase 2 structural checks: PASS")
PY

if command -v cargo >/dev/null 2>&1; then
    cargo test -p fluxvm-mesh --all-targets
else
    echo "cargo unavailable: SKIP Rust compilation"
fi
