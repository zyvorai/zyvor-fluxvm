#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
root = Path.cwd()
lib = root / "crates/fluxvm-mesh/src/lib.rs"
main = root / "crates/fluxvm-mesh/src/main.rs"
cargo = root / "crates/fluxvm-mesh/Cargo.toml"
for path in (lib, main, cargo, root / "docs/fluxmesh.md"):
    assert path.exists(), path
text = lib.read_text()
for needle in (
    "pub struct OverlayIpam",
    "pub fn classify_nat",
    "pub fn select_paths",
    "pub struct MeshState",
    "pub struct WireGuardPlan",
    "pub struct StateStore",
    "#[cfg(test)]",
):
    assert needle in text, needle
assert "TODO" not in text
assert '"crates/fluxvm-mesh"' in (root / "Cargo.toml").read_text()
print("FluxMesh structural checks: PASS")
PY

if command -v cargo >/dev/null 2>&1; then
    cargo test -p fluxvm-mesh --all-targets
else
    echo "cargo unavailable: SKIP Rust compilation"
fi
