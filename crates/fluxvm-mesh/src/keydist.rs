// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Two-phase WireGuard public-key distribution.
//!
//! A node first publishes a pending public key. Every currently enrolled peer
//! acknowledges the generation. Only then can the owner activate it. This is
//! the coordination primitive Phase 3 intentionally lacked.

use crate::{MeshError, MeshState, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyProposal {
    pub node_id: String,
    pub generation: u64,
    pub public_key: String,
    pub acknowledgements: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyDistribution {
    pub proposals: BTreeMap<String, KeyProposal>,
}

impl KeyDistribution {
    pub fn propose(&mut self, mesh: &MeshState, node_id: &str, public_key: String) -> Result<u64> {
        mesh.node(node_id)?;
        if public_key.is_empty() || public_key.chars().any(char::is_whitespace) {
            return Err(MeshError::InvalidState(
                "pending public key is empty or contains whitespace".into(),
            ));
        }
        let generation = self
            .proposals
            .get(node_id)
            .map(|p| p.generation.saturating_add(1))
            .unwrap_or(1);
        self.proposals.insert(
            node_id.to_owned(),
            KeyProposal {
                node_id: node_id.to_owned(),
                generation,
                public_key,
                acknowledgements: BTreeSet::from([node_id.to_owned()]),
            },
        );
        Ok(generation)
    }

    pub fn acknowledge(
        &mut self,
        mesh: &MeshState,
        owner: &str,
        generation: u64,
        peer: &str,
    ) -> Result<()> {
        mesh.node(peer)?;
        let proposal = self
            .proposals
            .get_mut(owner)
            .ok_or_else(|| MeshError::InvalidState(format!("no pending key for {owner}")))?;
        if proposal.generation != generation {
            return Err(MeshError::InvalidState("key generation mismatch".into()));
        }
        proposal.acknowledgements.insert(peer.to_owned());
        Ok(())
    }

    pub fn ready(&self, mesh: &MeshState, owner: &str) -> Result<bool> {
        let proposal = self
            .proposals
            .get(owner)
            .ok_or_else(|| MeshError::InvalidState(format!("no pending key for {owner}")))?;
        Ok(mesh
            .nodes
            .keys()
            .all(|node| proposal.acknowledgements.contains(node)))
    }

    pub fn activate(&mut self, mesh: &mut MeshState, owner: &str) -> Result<String> {
        if !self.ready(mesh, owner)? {
            return Err(MeshError::InvalidState(
                "key proposal is not acknowledged by every node".into(),
            ));
        }
        let proposal = self
            .proposals
            .remove(owner)
            .ok_or_else(|| MeshError::InvalidState(format!("no pending key for {owner}")))?;
        mesh.node_mut(owner)?.public_key = proposal.public_key.clone();
        Ok(proposal.public_key)
    }
}

pub struct KeyDistributionStore {
    path: PathBuf,
}

impl KeyDistributionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<KeyDistribution> {
        let text = match fs::read_to_string(&self.path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(KeyDistribution::default());
            }
            Err(e) => return Err(e.into()),
        };
        let mut out = KeyDistribution::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() != 5 || fields[0] != "KEY" {
                return Err(MeshError::InvalidState(
                    "invalid key distribution store".into(),
                ));
            }
            let node_id = fields[1].to_owned();
            let generation = fields[2]
                .parse()
                .map_err(|_| MeshError::InvalidState("invalid key generation".into()))?;
            let public_key = fields[3].to_owned();
            let acknowledgements = if fields[4].is_empty() {
                BTreeSet::new()
            } else {
                fields[4].split(',').map(str::to_owned).collect()
            };
            out.proposals.insert(
                node_id.clone(),
                KeyProposal {
                    node_id,
                    generation,
                    public_key,
                    acknowledgements,
                },
            );
        }
        Ok(out)
    }

    pub fn save(&self, state: &KeyDistribution) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut text = String::new();
        for p in state.proposals.values() {
            text.push_str(&format!(
                "KEY\t{}\t{}\t{}\t{}\n",
                p.node_id,
                p.generation,
                p.public_key,
                p.acknowledgements
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn default_keydist_path() -> PathBuf {
    std::env::var_os("FLUXMESH_KEYDIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mesh_dir().join("keys.v1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_POOL;

    #[test]
    fn activation_waits_for_every_peer_ack() {
        let mut mesh = MeshState::new(DEFAULT_POOL.parse().unwrap(), "a", "a", "ka").unwrap();
        mesh.add_node("b", "b", "kb", false).unwrap();
        let mut keys = KeyDistribution::default();
        let generation = keys.propose(&mesh, "a", "ka2".into()).unwrap();
        assert!(!keys.ready(&mesh, "a").unwrap());
        keys.acknowledge(&mesh, "a", generation, "b").unwrap();
        assert!(keys.ready(&mesh, "a").unwrap());
        keys.activate(&mut mesh, "a").unwrap();
        assert_eq!(mesh.node("a").unwrap().public_key, "ka2");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RotationStep {
    NotDue,
    Proposed {
        generation: u64,
        public_key: String,
    },
    AwaitingAcks {
        generation: u64,
        missing: Vec<String>,
    },
    Activated {
        generation: u64,
        public_key: String,
    },
}

/// Coordinate a safe local key rotation. The active private key is untouched
/// until every current mesh member has acknowledged the pending public key.
pub fn coordinated_local_rotation(
    mesh_store: &crate::StateStore,
    key_store: &KeyDistributionStore,
    key_path: &Path,
    max_age: std::time::Duration,
    dry_run: bool,
) -> Result<RotationStep> {
    use crate::reconcile::{key_rotation_due, rotate_wireguard_key};
    let mut mesh = mesh_store.load()?;
    let local = mesh.local_node.clone();
    let mut keys = key_store.load()?;
    let pending_path = key_path.with_extension("pending");

    if let Some(proposal) = keys.proposals.get(&local).cloned() {
        if keys.ready(&mesh, &local)? {
            if dry_run {
                return Ok(RotationStep::Activated {
                    generation: proposal.generation,
                    public_key: proposal.public_key,
                });
            }
            if !pending_path.exists() {
                return Err(MeshError::InvalidState(
                    "pending private key disappeared before activation".into(),
                ));
            }
            if let Some(parent) = key_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let backup_path = key_path.with_extension("previous");
            fs::remove_file(&backup_path).ok();
            if key_path.exists() {
                fs::rename(key_path, &backup_path)?;
            }
            if let Err(e) = fs::rename(&pending_path, key_path) {
                if backup_path.exists() {
                    let _ = fs::rename(&backup_path, key_path);
                }
                return Err(e.into());
            }
            let public_key = keys.activate(&mut mesh, &local)?;
            let persist = key_store.save(&keys).and_then(|_| mesh_store.save(&mesh));
            if let Err(e) = persist {
                let _ = fs::remove_file(key_path);
                if backup_path.exists() {
                    let _ = fs::rename(&backup_path, key_path);
                }
                return Err(e);
            }
            fs::remove_file(&backup_path).ok();
            return Ok(RotationStep::Activated {
                generation: proposal.generation,
                public_key,
            });
        }
        let missing = mesh
            .nodes
            .keys()
            .filter(|n| !proposal.acknowledgements.contains(*n))
            .cloned()
            .collect();
        return Ok(RotationStep::AwaitingAcks {
            generation: proposal.generation,
            missing,
        });
    }

    if !key_rotation_due(key_path, max_age, std::time::SystemTime::now()) {
        return Ok(RotationStep::NotDue);
    }
    if dry_run {
        return Ok(RotationStep::Proposed {
            generation: 0,
            public_key: "<dry-run>".into(),
        });
    }
    let public_key = rotate_wireguard_key(&pending_path)?;
    let generation = keys.propose(&mesh, &local, public_key.clone())?;
    key_store.save(&keys)?;
    Ok(RotationStep::Proposed {
        generation,
        public_key,
    })
}
