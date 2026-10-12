// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! FluxDNS HA placement and zone serial generation.

use crate::{MeshState, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsReplicaSet {
    pub primary: String,
    pub replicas: Vec<String>,
    pub serial: u64,
}

pub fn elect(mesh: &MeshState, replica_count: usize, generation: u64) -> Result<DnsReplicaSet> {
    mesh.node(&mesh.local_node)?;
    let mut scored = mesh
        .nodes
        .keys()
        .map(|node| (rendezvous(&mesh.ipam.pool.to_string(), node), node.clone()))
        .collect::<Vec<_>>();
    scored.sort_by_key(|(score, node)| (std::cmp::Reverse(*score), node.clone()));
    let replicas = scored
        .into_iter()
        .map(|(_, n)| n)
        .take(replica_count.max(1))
        .collect::<Vec<_>>();
    let primary = replicas
        .first()
        .cloned()
        .ok_or_else(|| crate::MeshError::InvalidState("no DNS replicas".into()))?;
    Ok(DnsReplicaSet {
        primary,
        replicas,
        serial: generation.max(1),
    })
}

fn rendezvous(mesh_seed: &str, node: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in mesh_seed
        .as_bytes()
        .iter()
        .chain([0xff].iter())
        .chain(node.as_bytes())
    {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn election_is_deterministic() {
        let mut mesh = MeshState::new(DEFAULT_POOL.parse().unwrap(), "a", "a", "ka").unwrap();
        mesh.add_node("b", "b", "kb", false).unwrap();
        mesh.add_node("c", "c", "kc", false).unwrap();
        assert_eq!(elect(&mesh, 2, 5).unwrap(), elect(&mesh, 2, 5).unwrap());
        assert_eq!(elect(&mesh, 2, 5).unwrap().replicas.len(), 2);
    }
}
