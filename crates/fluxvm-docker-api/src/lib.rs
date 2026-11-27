// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Minimal Docker Engine API adapter.
//!
//! Maps a small, stable subset of the Docker Engine API onto the FluxVM control plane so that Docker SDKs and
//! `DOCKER_HOST=… docker` can list, create, start, stop and read the logs of containers (one OCI sandbox microVM
//! each). It is intentionally thin: the HTTP surface lives here ([`router`]), the FluxVM side lives behind the
//! [`DockerAdapter`] trait, so the router is tested against [`StubAdapter`] and the daemon wires in the real
//! control plane (`fluxvm_api::docker_adapter::FluxvmAdapter`).
//!
//! Supported: `GET /_ping`, `GET /version`, `GET /containers/json`, `POST /containers/create`,
//! `GET /containers/{id}/json`, `POST /containers/{id}/start`, `POST /containers/{id}/stop`,
//! `GET /containers/{id}/logs`, each also under a `/v1.NN` prefix. Everything else answers `501` with a
//! Docker-style `{"message": …}` body. See `docs/DOCKER-API.md`.

mod listen;
mod router;
mod stub;

pub use listen::{ListenAddr, serve};
pub use router::{API_VERSION, MIN_API_VERSION, router};
pub use stub::StubAdapter;

use bytes::Bytes;
use futures::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A container as `GET /containers/json` reports it (Docker's `ContainerSummary`, the fields clients read).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Container {
    #[serde(rename = "Id")]
    pub id: String,
    /// Docker names start with `/`.
    #[serde(rename = "Names")]
    pub names: Vec<String>,
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "ImageID", default)]
    pub image_id: String,
    #[serde(rename = "Command", default)]
    pub command: String,
    /// Unix seconds.
    #[serde(rename = "Created", default)]
    pub created: i64,
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
    /// `created`, `running`, `paused`, `exited`.
    #[serde(rename = "State")]
    pub state: String,
    /// Human text, e.g. `Up`, `Exited`.
    #[serde(rename = "Status")]
    pub status: String,
}

impl Container {
    /// Whether `docker ps` (without `-a`) shows this container.
    pub fn is_up(&self) -> bool {
        matches!(self.state.as_str(), "running" | "paused")
    }
}

/// What `GET /containers/{id}/json` needs beyond the summary (the docker CLI reads `Config.Tty` before `logs`).
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerDetails {
    pub summary: Container,
    /// Whether the container has a TTY; decides raw vs multiplexed log framing.
    pub tty: bool,
    pub cmd: Option<Vec<String>>,
    pub exit_code: Option<i32>,
}

/// The `HostConfig` fields the adapter reads. Anything else in the request is ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// `-v /host:/guest`. FluxVM's control plane rejects these (see `docs/DOCKER-API.md`).
    #[serde(rename = "Binds")]
    pub binds: Option<Vec<String>>,
}

/// `POST /containers/create` body (subset of Docker's `ContainerConfig`). The docker CLI sends `null` for empty
/// lists, hence the `Option`s.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CreateRequest {
    #[serde(rename = "Image")]
    pub image: String,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(rename = "Tty")]
    pub tty: bool,
    #[serde(rename = "HostConfig")]
    pub host_config: HostConfig,
}

/// Outcome of `start`/`stop`: Docker answers `304 Not Modified` when the container already was in that state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Changed,
    Unchanged,
}

/// `GET /containers/{id}/logs` options the adapter must honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogsRequest {
    pub stdout: bool,
    pub stderr: bool,
    pub follow: bool,
    /// Last N lines; `None` is `tail=all`.
    pub tail: Option<usize>,
}

/// Which stream a log chunk came from (the multiplexed-stream frame type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout = 1,
    Stderr = 2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogChunk {
    pub stream: LogStream,
    pub data: Bytes,
}

/// Log output: whole lines, oldest first. With `follow` the stream ends when the container stops.
pub struct Logs {
    pub tty: bool,
    pub chunks: BoxStream<'static, Result<LogChunk, AdapterError>>,
}

/// Docker-mapped error class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 400
    BadRequest,
    /// 404
    NotFound,
    /// 409
    Conflict,
    /// 500
    Internal,
    /// 501
    NotImplemented,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    pub kind: ErrorKind,
    pub message: String,
}

impl AdapterError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::BadRequest, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    pub fn no_such_container(id: &str) -> Self {
        Self::not_found(format!("No such container: {id}"))
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotImplemented, message)
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AdapterError {}

pub type AdapterResult<T> = Result<T, AdapterError>;

/// The FluxVM side of the adapter. `id` is whatever the client sent: a full ID, a unique ID prefix or a name
/// (with or without the leading `/`); implementations resolve it the way Docker does (exact name first).
pub trait DockerAdapter: Send + Sync + 'static {
    /// Newest first. `all = false` keeps only running and paused containers.
    fn list(&self, all: bool) -> BoxFuture<'_, AdapterResult<Vec<Container>>>;
    fn inspect(&self, id: String) -> BoxFuture<'_, AdapterResult<ContainerDetails>>;
    /// Returns the new container's ID. A name already in use is [`ErrorKind::Conflict`].
    fn create(
        &self,
        name: Option<String>,
        req: CreateRequest,
    ) -> BoxFuture<'_, AdapterResult<String>>;
    fn start(&self, id: String) -> BoxFuture<'_, AdapterResult<Transition>>;
    /// `timeout` is Docker's `t` (seconds before a hard kill), if the client sent one.
    fn stop(&self, id: String, timeout: Option<u64>) -> BoxFuture<'_, AdapterResult<Transition>>;
    fn logs(&self, id: String, req: LogsRequest) -> BoxFuture<'_, AdapterResult<Logs>>;
}
