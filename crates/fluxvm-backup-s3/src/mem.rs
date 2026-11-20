// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! An in-memory object store, for hermetic tests of code that reads backup chains.

use crate::{DeleteOutcome, ObjectStore, PutOptions};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Default)]
pub struct MemStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.objects.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keys currently stored, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Replaces an object's bytes without going through the trait; tests use it to corrupt one.
    pub fn overwrite(&self, key: &str, data: Vec<u8>) {
        self.lock().insert(key.to_string(), data);
    }
}

#[async_trait]
impl ObjectStore for MemStore {
    async fn put_file(&self, key: &str, path: &Path, _opts: &PutOptions) -> Result<()> {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        self.lock().insert(key.to_string(), data);
        Ok(())
    }

    async fn put_bytes(&self, key: &str, data: Vec<u8>, _opts: &PutOptions) -> Result<()> {
        self.lock().insert(key.to_string(), data);
        Ok(())
    }

    async fn get_file(&self, key: &str, dest: &Path) -> Result<()> {
        let data = self.get_bytes(key).await?;
        tokio::fs::write(dest, data)
            .await
            .with_context(|| format!("writing {}", dest.display()))
    }

    async fn get_bytes(&self, key: &str) -> Result<Vec<u8>> {
        match self.lock().get(key) {
            Some(d) => Ok(d.clone()),
            None => bail!("object {key} not found"),
        }
    }

    async fn delete(&self, key: &str) -> Result<DeleteOutcome> {
        Ok(match self.lock().remove(key) {
            Some(_) => DeleteOutcome::Deleted,
            None => DeleteOutcome::NotFound,
        })
    }
}
