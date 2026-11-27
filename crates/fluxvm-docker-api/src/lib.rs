// Copyright 2026 Zyvor · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0
//! Minimal Docker Engine API adapter.
//!
//! Maps a subset of the Docker Engine API onto the FluxVM control plane so
//! that `docker --context fluxvm` can list, create, start, stop and fetch
//! logs for VMs. This is intentionally thin; full dockerd compatibility is
//! not the goal.

use serde::{Deserialize, Serialize};

/// A container (VM) as seen by the Docker API.
#[derive(Debug, Serialize, Deserialize)]
pub struct Container {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Names")]
    pub names: Vec<String>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "State")]
    pub state: String,
    #[serde(rename = "Status")]
    pub status: String,
}

/// Create request (subset of Docker's ContainerCreate).
#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
}

/// Adapter trait. Implementations map these calls onto the FluxVM API.
pub trait DockerAdapter {
    fn list(&self) -> Result<Vec<Container>, String>;
    fn create(&self, req: CreateRequest) -> Result<String, String>;
    fn start(&self, id: &str) -> Result<(), String>;
    fn stop(&self, id: &str) -> Result<(), String>;
    fn logs(&self, id: &str) -> Result<String, String>;
}

/// In-memory stub used for tests and early wiring.
pub struct StubAdapter {
    pub containers: Vec<Container>,
}

impl DockerAdapter for StubAdapter {
    fn list(&self) -> Result<Vec<Container>, String> {
        Ok(self.containers.clone())
    }
    fn create(&self, _req: CreateRequest) -> Result<String, String> {
        Err("stub: create not wired to FluxVM yet".into())
    }
    fn start(&self, _id: &str) -> Result<(), String> {
        Err("stub: start not wired to FluxVM yet".into())
    }
    fn stop(&self, _id: &str) -> Result<(), String> {
        Err("stub: stop not wired to FluxVM yet".into())
    }
    fn logs(&self, _id: &str) -> Result<String, String> {
        Err("stub: logs not wired to FluxVM yet".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_list_empty() {
        let a = StubAdapter { containers: vec![] };
        assert!(a.list().unwrap().is_empty());
    }
}
