// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A directory as an object store: keys map to files below `root`.

use crate::{DeleteOutcome, ObjectStore, PutOptions};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        if key.is_empty()
            || key.starts_with('/')
            || key
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            bail!("invalid object key {key:?}");
        }
        Ok(self.root.join(key))
    }

    /// Writes via a temp file and rename, so a reader never sees half an
    /// object.
    async fn write(&self, key: &str, write: impl AsyncFnOnce(&Path) -> Result<()>) -> Result<()> {
        let dst = self.path(key)?;
        if let Some(dir) = dst.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut tmp = dst.clone().into_os_string();
        tmp.push(".part");
        let tmp = PathBuf::from(tmp);
        write(&tmp).await?;
        tokio::fs::rename(&tmp, &dst)
            .await
            .with_context(|| format!("storing {}", dst.display()))
    }
}

#[async_trait]
impl ObjectStore for DirStore {
    async fn put_file(&self, key: &str, path: &Path, _opts: &PutOptions) -> Result<()> {
        self.write(key, async |tmp| {
            tokio::fs::copy(path, tmp)
                .await
                .with_context(|| format!("copying {}", path.display()))?;
            Ok(())
        })
        .await
    }

    async fn put_bytes(&self, key: &str, data: Vec<u8>, _opts: &PutOptions) -> Result<()> {
        self.write(key, async |tmp| {
            tokio::fs::write(tmp, &data).await?;
            Ok(())
        })
        .await
    }

    async fn get_file(&self, key: &str, dest: &Path) -> Result<()> {
        let src = self.path(key)?;
        tokio::fs::copy(&src, dest)
            .await
            .with_context(|| format!("reading object {key}"))?;
        Ok(())
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        tokio::fs::read(self.path(key)?)
            .await
            .with_context(|| format!("reading object {key}"))
    }

    async fn delete(&self, key: &str) -> Result<DeleteOutcome> {
        match tokio::fs::remove_file(self.path(key)?).await {
            Ok(()) => Ok(DeleteOutcome::Deleted),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeleteOutcome::NotFound),
            Err(e) => Err(e).with_context(|| format!("deleting object {key}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_reads_and_deletes_objects() {
        let dir = tempfile::tempdir().unwrap();
        let s = DirStore::new(dir.path().to_path_buf());
        s.put_bytes("a/b/manifest.json", b"{}".to_vec(), &PutOptions::default())
            .await
            .unwrap();
        assert_eq!(s.get_bytes("a/b/manifest.json").await.unwrap(), b"{}");
        assert_eq!(
            s.delete("a/b/manifest.json").await.unwrap(),
            DeleteOutcome::Deleted
        );
        assert_eq!(
            s.delete("a/b/manifest.json").await.unwrap(),
            DeleteOutcome::NotFound
        );
        assert!(s.get_bytes("../etc/passwd").await.is_err());
    }
}
