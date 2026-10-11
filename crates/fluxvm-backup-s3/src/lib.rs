// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Where changed-block-tracking backups live: one manifest per backup and
//! one qcow2 layer per disk, in an S3-compatible bucket (or a directory).
//!
//! A chain is a full backup followed by incrementals, each layer holding
//! only the clusters that changed since its parent. Keys are
//! `<prefix>/<chain>/<sequence>-<backup>/{manifest.json,<disk>.qcow2}`, so a
//! listing sorts by sequence.

pub mod crypto;
pub mod dir;
pub mod manifest;
pub mod s3;

pub use manifest::{DiskLayer, LayerEncryption, Manifest, ManifestKind, ParentRef};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What happened to a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteOutcome {
    Deleted,
    /// Already gone.
    NotFound,
    /// Object Lock retention hasn't expired.
    RetentionHeld,
}

/// Write options for one object.
#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// S3 Object Lock COMPLIANCE retention: the object can't be deleted or
    /// overwritten before this instant, by anyone.
    pub retain_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// An object store holding backup layers and manifests.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put_file(&self, key: &str, path: &Path, opts: &PutOptions) -> Result<()>;
    async fn put_bytes(&self, key: &str, data: Vec<u8>, opts: &PutOptions) -> Result<()>;
    async fn get_file(&self, key: &str, dest: &Path) -> Result<()>;
    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>>;
    async fn delete(&self, key: &str) -> Result<DeleteOutcome>;
}

/// A backup destination as callers describe it. Credentials and keys travel
/// with each request and are never written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Target {
    S3(s3::S3Target),
    /// A local or mounted directory; mainly for tests and air-gapped hosts.
    Dir {
        path: std::path::PathBuf,
    },
}

impl Target {
    pub fn open(&self) -> Result<Box<dyn ObjectStore>> {
        Ok(match self {
            Target::S3(t) => Box::new(s3::S3Store::new(t.clone())?),
            Target::Dir { path } => Box::new(dir::DirStore::new(path.clone())),
        })
    }
}

/// Validates one `/`-separated key segment chosen by a caller (prefix
/// parts, chain and backup ids): no empty, `.`/`..`, or unusual bytes.
pub fn validate_key_segment(what: &str, s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        anyhow::bail!("invalid {what} {s:?}: use 1-128 of [A-Za-z0-9._-]");
    }
    Ok(())
}

/// Validates a caller-chosen key prefix (`a/b/c`, each segment checked).
pub fn validate_prefix(prefix: &str) -> Result<()> {
    for seg in prefix.split('/') {
        validate_key_segment("key prefix segment", seg)?;
    }
    Ok(())
}

/// The key directory of one backup.
pub fn backup_dir(prefix: &str, chain_id: &str, sequence: u64, backup_id: &str) -> String {
    format!("{prefix}/{chain_id}/{sequence:08}-{backup_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_sort_by_sequence_and_reject_traversal() {
        assert_eq!(
            backup_dir("ns/db/uid", "c1", 7, "b-9"),
            "ns/db/uid/c1/00000007-b-9"
        );
        assert!(validate_prefix("ns/db/uid").is_ok());
        assert!(validate_prefix("ns/../x").is_err());
        assert!(validate_prefix("ns//x").is_err());
        assert!(validate_key_segment("chain", "a b").is_err());
    }
}
