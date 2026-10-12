// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Deterministic IPv6 ULA overlay addressing.
//!
//! FluxMesh keeps IPv4 IPAM as the durable allocator. IPv6 host addresses are
//! derived from the mesh IPv4 pool plus node id, so every node independently
//! computes the same /64 and /128s without another allocation database.

use crate::{MeshError, MeshState, Result, WireGuardPlan};
use std::net::Ipv6Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv6Overlay {
    pub prefix: Ipv6Addr,
    pub prefix_len: u8,
}

impl Ipv6Overlay {
    pub fn for_mesh(mesh: &MeshState) -> Self {
        let hash = fnv1a64(mesh.ipam.pool.to_string().as_bytes());
        let h = hash.to_be_bytes();
        let bytes = [
            0xfd, h[0], h[1], h[2], h[3], h[4], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        Self {
            prefix: Ipv6Addr::from(bytes),
            prefix_len: 64,
        }
    }

    pub fn address_for(self, node_id: &str) -> Result<Ipv6Addr> {
        if node_id.is_empty() || node_id.len() > 63 {
            return Err(MeshError::InvalidNodeId(node_id.to_owned()));
        }
        let mut bytes = self.prefix.octets();
        let mut iid = fnv1a64(node_id.as_bytes());
        if iid == 0 {
            iid = 1;
        }
        bytes[8..].copy_from_slice(&iid.to_be_bytes());
        // Clear the universal/local bit. This is an opaque stable host id,
        // not an EUI-64 derived from hardware identity.
        bytes[8] &= !0x02;
        Ok(Ipv6Addr::from(bytes))
    }

    pub fn cidr(self) -> String {
        format!("{}/{}", self.prefix, self.prefix_len)
    }
}

/// Add deterministic IPv6 host routes to an already-built WireGuard plan.
pub fn augment_wireguard_plan(mesh: &MeshState, plan: &mut WireGuardPlan) -> Result<()> {
    let overlay = Ipv6Overlay::for_mesh(mesh);
    plan.local_ipv6 = Some(overlay.address_for(&mesh.local_node)?);
    for peer in &mut plan.peers {
        let ip = overlay.address_for(&peer.node_id)?;
        let cidr = format!("{ip}/128");
        if !peer.allowed_ips.contains(&cidr) {
            peer.allowed_ips.push(cidr);
        }
    }
    Ok(())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn ula_is_stable_and_unique_per_node() {
        let mut mesh = MeshState::new(DEFAULT_POOL.parse().unwrap(), "a", "a", "ka").unwrap();
        mesh.add_node("b", "b", "kb", false).unwrap();
        let overlay = Ipv6Overlay::for_mesh(&mesh);
        let a = overlay.address_for("a").unwrap();
        let b = overlay.address_for("b").unwrap();
        assert!(a.is_unique_local());
        assert!(b.is_unique_local());
        assert_ne!(a, b);
        assert_eq!(overlay.address_for("a").unwrap(), a);
    }

    #[test]
    fn wireguard_plan_gets_v6_host_routes() {
        let mut mesh = MeshState::new(DEFAULT_POOL.parse().unwrap(), "a", "a", "ka").unwrap();
        mesh.add_node("b", "b", "kb", false).unwrap();
        let mut plan = mesh.wireguard_plan("fm0", 51820, "/tmp/key").unwrap();
        augment_wireguard_plan(&mesh, &mut plan).unwrap();
        assert!(plan.local_ipv6.is_some());
        assert!(plan.peers[0].allowed_ips.iter().any(|v| v.contains(':')));
    }
}
