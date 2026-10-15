// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! macOS NetworkExtension handoff contract.
//!
//! macOS does not expose Linux WireGuard or eBPF/tap primitives to FluxVM.
//! FluxMesh therefore emits an atomic desired-state document for a signed
//! `NEPacketTunnelProvider` host application. The provider owns entitlement,
//! utun lifecycle and the userspace WireGuard engine; the cross-platform mesh
//! control plane remains identical.

use crate::ipv6::augment_wireguard_plan;
use crate::{MeshState, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const MAC_PLAN_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacPeerPlan {
    pub node_id: String,
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    pub persistent_keepalive: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacTunnelPlan {
    pub version: u32,
    pub local_node: String,
    pub local_address: String,
    pub local_ipv6: Option<String>,
    pub listen_port: u16,
    pub mtu: u16,
    pub dns_suffix: String,
    pub peers: Vec<MacPeerPlan>,
}

impl MacTunnelPlan {
    pub fn from_mesh(
        mesh: &MeshState,
        listen_port: u16,
        dns_suffix: impl Into<String>,
    ) -> Result<Self> {
        let mut wg =
            mesh.wireguard_plan("utun", listen_port, PathBuf::from("network-extension-key"))?;
        augment_wireguard_plan(mesh, &mut wg)?;
        Ok(Self {
            version: MAC_PLAN_VERSION,
            local_node: mesh.local_node.clone(),
            local_address: format!("{}/32", wg.local_address),
            local_ipv6: wg.local_ipv6.map(|v| format!("{v}/128")),
            listen_port,
            mtu: 1280,
            dns_suffix: dns_suffix.into(),
            peers: wg
                .peers
                .into_iter()
                .map(|peer| MacPeerPlan {
                    node_id: peer.node_id,
                    public_key: peer.public_key,
                    endpoint: peer.endpoint.map(|v| v.to_string()),
                    allowed_ips: peer.allowed_ips,
                    persistent_keepalive: peer.persistent_keepalive.unwrap_or(0),
                })
                .collect(),
        })
    }

    pub fn to_json(&self) -> String {
        let peers = self
            .peers
            .iter()
            .map(|peer| {
                let endpoint = peer
                    .endpoint
                    .as_ref()
                    .map(|value| format!("\"{}\"", json_escape(value)))
                    .unwrap_or_else(|| "null".into());
                let allowed = peer
                    .allowed_ips
                    .iter()
                    .map(|value| format!("\"{}\"", json_escape(value)))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{{\"node_id\":\"{}\",\"public_key\":\"{}\",\"endpoint\":{},\"allowed_ips\":[{}],\"persistent_keepalive\":{}}}",
                    json_escape(&peer.node_id),
                    json_escape(&peer.public_key),
                    endpoint,
                    allowed,
                    peer.persistent_keepalive
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let local_ipv6 = self
            .local_ipv6
            .as_ref()
            .map(|value| format!("\"{}\"", json_escape(value)))
            .unwrap_or_else(|| "null".into());
        format!(
            "{{\"version\":{},\"local_node\":\"{}\",\"local_address\":\"{}\",\"local_ipv6\":{},\"listen_port\":{},\"mtu\":{},\"dns_suffix\":\"{}\",\"peers\":[{}]}}",
            self.version,
            json_escape(&self.local_node),
            json_escape(&self.local_address),
            local_ipv6,
            self.listen_port,
            self.mtu,
            json_escape(&self.dns_suffix),
            peers
        )
    }

    pub fn write_atomic(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, format!("{}\n", self.to_json()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, path)?;
        Ok(())
    }
}

pub fn default_plan_path() -> PathBuf {
    std::env::var_os("FLUXMESH_MACOS_PLAN")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mesh_dir().join("macos-tunnel.v1.json"))
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn mac_plan_contains_overlay_and_peer_routes() {
        let mut mesh = MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "mac-a",
            "mac-a.local",
            "key-a",
        )
        .unwrap();
        mesh.add_node("mac-b", "mac-b.local", "key-b", false)
            .unwrap();
        let plan = MacTunnelPlan::from_mesh(&mesh, 51820, "flux").unwrap();
        assert_eq!(plan.peers.len(), 1);
        assert!(plan.to_json().contains("mac-b"));
        assert!(plan.to_json().contains("allowed_ips"));
    }
}
