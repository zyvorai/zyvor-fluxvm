// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The per-backup manifest: what the backup holds and what it builds on.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManifestKind {
    Full,
    Incremental,
    /// A chain flattened into one standalone layer at the same point as the
    /// incremental it replaces; later incrementals can build on it.
    SyntheticFull,
}

impl ManifestKind {
    /// Whether restoring needs the parent chain.
    pub fn needs_parent(self) -> bool {
        self == ManifestKind::Incremental
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentRef {
    pub backup_id: String,
    pub sequence: u64,
    /// Key of the parent's manifest.
    pub manifest_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerEncryption {
    /// `aes256-gcm`.
    pub algorithm: String,
    /// Caller's name for the key, so a restore can ask for the right one.
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskLayer {
    /// `root` or a data disk name.
    pub name: String,
    pub key: String,
    pub virtual_size: u64,
    /// Bytes stored in the bucket (compressed, and encrypted if enabled).
    pub stored_size: u64,
    /// Guest bytes this layer carries: everything for a full backup, the
    /// changed clusters for an incremental.
    pub changed_bytes: u64,
    /// SHA-256 of the plaintext qcow2 layer, checked after download.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<LayerEncryption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub backup_id: String,
    pub chain_id: String,
    pub sequence: u64,
    pub kind: ManifestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentRef>,
    pub vm_name: String,
    pub created_at: DateTime<Utc>,
    pub quiesced: bool,
    pub disks: Vec<DiskLayer>,
    /// Backups a synthetic full replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consolidates: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until: Option<DateTime<Utc>>,
}

impl Manifest {
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let m: Manifest = serde_json::from_slice(data).context("parsing backup manifest")?;
        if m.version > MANIFEST_VERSION {
            bail!(
                "backup manifest version {} is newer than this FluxVM understands ({MANIFEST_VERSION})",
                m.version
            );
        }
        if m.kind.needs_parent() != m.parent.is_some() {
            bail!(
                "backup manifest {}: a {:?} backup {} a parent",
                m.backup_id,
                m.kind,
                if m.kind.needs_parent() {
                    "needs"
                } else {
                    "can't have"
                }
            );
        }
        Ok(m)
    }

    pub fn changed_bytes(&self) -> u64 {
        self.disks.iter().map(|d| d.changed_bytes).sum()
    }

    pub fn stored_bytes(&self) -> u64 {
        self.disks.iter().map(|d| d.stored_size).sum()
    }

    pub fn disk(&self, name: &str) -> Option<&DiskLayer> {
        self.disks.iter().find(|d| d.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(kind: ManifestKind, parent: Option<ParentRef>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            backup_id: "b2".into(),
            chain_id: "c1".into(),
            sequence: 2,
            kind,
            parent,
            vm_name: "db".into(),
            created_at: Utc::now(),
            quiesced: true,
            disks: vec![DiskLayer {
                name: "root".into(),
                key: "p/c1/00000002-b2/root.qcow2".into(),
                virtual_size: 1 << 30,
                stored_size: 4096,
                changed_bytes: 8 << 20,
                sha256: "00".repeat(32),
                encryption: None,
            }],
            consolidates: vec![],
            retain_until: None,
        }
    }

    #[test]
    fn round_trips_and_checks_the_parent_rule() {
        let parent = ParentRef {
            backup_id: "b1".into(),
            sequence: 1,
            manifest_key: "p/c1/00000001-b1/manifest.json".into(),
        };
        let m = manifest(ManifestKind::Incremental, Some(parent.clone()));
        let back = Manifest::from_bytes(&m.to_bytes().unwrap()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.changed_bytes(), 8 << 20);
        let json = String::from_utf8(m.to_bytes().unwrap()).unwrap();
        assert!(json.contains("\"kind\": \"incremental\""), "{json}");

        let orphan = manifest(ManifestKind::Incremental, None);
        assert!(Manifest::from_bytes(&orphan.to_bytes().unwrap()).is_err());
        let full_with_parent = manifest(ManifestKind::Full, Some(parent));
        assert!(Manifest::from_bytes(&full_with_parent.to_bytes().unwrap()).is_err());
        let mut newer = manifest(ManifestKind::Full, None);
        newer.version = MANIFEST_VERSION + 1;
        assert!(Manifest::from_bytes(&newer.to_bytes().unwrap()).is_err());
    }
}
