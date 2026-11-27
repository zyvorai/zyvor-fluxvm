// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! [`fluxvm_docker_api::DockerAdapter`] over the real FluxVM control plane ([`VmManager`]).
//!
//! Containers are OCI sandboxes (one microVM each), exactly the records the built-in `docker.sock` shim
//! ([`crate::docker_engine`]) shows; the create mapping and the ID/name resolution are shared with it so the two
//! Docker surfaces never disagree. Opt-in: the daemon serves it only when `FLUXVM_DOCKER_API` names a unix socket
//! or a loopback address ([`spawn_if_enabled`]). See `docs/DOCKER-API.md`.

use crate::docker_engine::{
    docker_id, docker_state, docker_status_text, oci_containers, resolve_container,
    sandbox_request_from_json,
};
use axum::http::StatusCode;
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_docker_api::{
    AdapterError, AdapterResult, Container, ContainerDetails, CreateRequest, DockerAdapter,
    ErrorKind, ListenAddr, LogChunk, LogStream, Logs, LogsRequest, Transition,
};
use fluxvm_scheduler::{VmManager, oci_images::OCI_LABEL, oci_sandbox::OCI_IMAGE_LABEL};
use futures::{FutureExt as _, future::BoxFuture};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};

/// Environment variable that turns the adapter on: `unix:///path.sock`, `/path.sock` or `tcp://127.0.0.1:PORT`.
pub const ENV_LISTEN: &str = "FLUXVM_DOCKER_API";
/// Most console lines one `logs` call returns (`tail=all`).
const MAX_LOG_LINES: usize = 10_000;
/// How often `logs --follow` polls the console.
const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// The Docker adapter backed by the daemon's [`VmManager`].
#[derive(Clone)]
pub struct FluxvmAdapter {
    manager: Arc<VmManager>,
}

impl FluxvmAdapter {
    pub fn new(manager: Arc<VmManager>) -> Self {
        Self { manager }
    }
}

fn from_shim_error(e: crate::docker_engine::DockerError) -> AdapterError {
    let (status, message) = e.into_parts();
    let kind = match status {
        StatusCode::NOT_FOUND => ErrorKind::NotFound,
        StatusCode::CONFLICT => ErrorKind::Conflict,
        StatusCode::NOT_IMPLEMENTED => ErrorKind::NotImplemented,
        s if s.is_client_error() => ErrorKind::BadRequest,
        _ => ErrorKind::Internal,
    };
    AdapterError::new(kind, message)
}

fn internal(e: anyhow::Error) -> AdapterError {
    AdapterError::internal(format!("{e:#}"))
}

/// `GET /containers/json` row for a sandbox record.
fn summary(vm: &VmRecord) -> Container {
    let (state, _) = docker_state(&vm.status);
    Container {
        id: docker_id(vm.id),
        names: vec![format!("/{}", vm.name.trim_start_matches('/'))],
        image: vm
            .labels
            .get(OCI_IMAGE_LABEL)
            .cloned()
            .unwrap_or_else(|| "unknown".into()),
        image_id: vm.labels.get(OCI_LABEL).cloned().unwrap_or_default(),
        command: String::new(),
        created: vm.created_at.timestamp(),
        labels: vm.labels.clone(),
        state: state.into(),
        status: docker_status_text(vm),
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim_start_matches('/') == b.trim_start_matches('/')
}

impl DockerAdapter for FluxvmAdapter {
    fn list(&self, all: bool) -> BoxFuture<'_, AdapterResult<Vec<Container>>> {
        async move {
            let mut vms = oci_containers(&self.manager).await;
            if !all {
                vms.retain(|vm| matches!(vm.status, VmStatus::Running | VmStatus::Paused));
            }
            vms.sort_unstable_by_key(|vm| std::cmp::Reverse((vm.created_at, vm.id)));
            Ok(vms.iter().map(summary).collect())
        }
        .boxed()
    }

    fn inspect(&self, id: String) -> BoxFuture<'_, AdapterResult<ContainerDetails>> {
        async move {
            let vm = resolve_container(&self.manager, &id)
                .await
                .map_err(from_shim_error)?;
            let exit_code = self
                .manager
                .sandbox_logs(vm.id, 1)
                .await
                .ok()
                .and_then(|l| l.exit_code);
            Ok(ContainerDetails {
                summary: summary(&vm),
                tty: false,
                cmd: None,
                exit_code,
            })
        }
        .boxed()
    }

    fn create(
        &self,
        name: Option<String>,
        req: CreateRequest,
    ) -> BoxFuture<'_, AdapterResult<String>> {
        async move {
            if req.tty {
                return Err(AdapterError::not_implemented(
                    "TTY containers are not supported: a FluxVM sandbox console is a plain log stream (drop -t)",
                ));
            }
            if let Some(n) = &name
                && self
                    .manager
                    .list()
                    .await
                    .iter()
                    .any(|vm| same_name(&vm.name, n))
            {
                return Err(AdapterError::conflict(format!(
                    "Conflict. The container name \"/{}\" is already in use.",
                    n.trim_start_matches('/')
                )));
            }
            // Through the same mapping as the docker.sock shim (bind mounts are refused there with a 400).
            let body = json!({
                "Image": req.image,
                "Cmd": req.cmd,
                "Env": req.env,
                "WorkingDir": req.working_dir,
                "HostConfig": { "Binds": req.host_config.binds },
            });
            let sandbox = sandbox_request_from_json(body, name).map_err(from_shim_error)?;
            let vm = self
                .manager
                .create_sandbox(sandbox, None, Some("docker-api"))
                .await
                .map_err(|e| AdapterError::bad_request(format!("{e:#}")))?;
            Ok(docker_id(vm.id))
        }
        .boxed()
    }

    fn start(&self, id: String) -> BoxFuture<'_, AdapterResult<Transition>> {
        async move {
            let vm = resolve_container(&self.manager, &id)
                .await
                .map_err(from_shim_error)?;
            match vm.status {
                VmStatus::Running | VmStatus::Paused => Ok(Transition::Unchanged),
                // `create_sandbox` boots the VM; `docker run` (create + start) must not fail on that.
                VmStatus::Creating => Ok(Transition::Changed),
                VmStatus::Stopped | VmStatus::Failed => {
                    self.manager.start(vm.id).await.map_err(internal)?;
                    Ok(Transition::Changed)
                }
            }
        }
        .boxed()
    }

    fn stop(&self, id: String, _timeout: Option<u64>) -> BoxFuture<'_, AdapterResult<Transition>> {
        async move {
            let vm = resolve_container(&self.manager, &id)
                .await
                .map_err(from_shim_error)?;
            if matches!(vm.status, VmStatus::Stopped | VmStatus::Failed) {
                return Ok(Transition::Unchanged);
            }
            // A graceful guest shutdown, then the VMM is killed; Docker's `t` is not configurable here.
            self.manager.stop(vm.id).await.map_err(internal)?;
            Ok(Transition::Changed)
        }
        .boxed()
    }

    fn logs(&self, id: String, req: LogsRequest) -> BoxFuture<'_, AdapterResult<Logs>> {
        async move {
            let vm = resolve_container(&self.manager, &id)
                .await
                .map_err(from_shim_error)?;
            let lines = req.tail.unwrap_or(MAX_LOG_LINES).min(MAX_LOG_LINES);
            // Where `follow` continues from: everything before this was covered by the tail below.
            let offset = tokio::fs::metadata(&vm.log_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            let initial = if lines == 0 {
                String::new()
            } else {
                self.manager
                    .sandbox_logs(vm.id, lines)
                    .await
                    .map_err(internal)?
                    .log
            };
            let first = chunk(initial.into_bytes());
            if !req.follow {
                return Ok(Logs {
                    tty: false,
                    chunks: Box::pin(futures::stream::iter([Ok(first)])),
                });
            }
            Ok(Logs {
                tty: false,
                chunks: Box::pin(follow_console(
                    self.manager.clone(),
                    vm.id,
                    vm.log_path,
                    offset,
                    first,
                )),
            })
        }
        .boxed()
    }
}

/// The console is one combined stream; Docker clients get it as stdout.
fn chunk(data: Vec<u8>) -> LogChunk {
    LogChunk {
        stream: LogStream::Stdout,
        data: data.into(),
    }
}

/// `first`, then whatever the console gains after `offset`, until the container exits or its VM stops.
fn follow_console(
    manager: Arc<VmManager>,
    id: uuid::Uuid,
    path: PathBuf,
    mut offset: u64,
    first: LogChunk,
) -> impl futures::Stream<Item = AdapterResult<LogChunk>> + Send + 'static {
    async_stream::stream! {
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        yield Ok(first);
        loop {
            if let Ok(mut f) = tokio::fs::File::open(&path).await
                && f.seek(std::io::SeekFrom::Start(offset)).await.is_ok()
            {
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).await.is_ok() && !buf.is_empty() {
                    offset += buf.len() as u64;
                    yield Ok(chunk(buf));
                }
            }
            match manager.get(id).await {
                Ok(vm) if matches!(vm.status, VmStatus::Running | VmStatus::Paused | VmStatus::Creating) => {}
                _ => break,
            }
            if manager.sandbox_logs(id, 1).await.ok().and_then(|l| l.exit_code).is_some() {
                break;
            }
            tokio::time::sleep(FOLLOW_POLL).await;
        }
    }
}

/// Parse `raw` and refuse the built-in `docker.sock` path (serving there would replace that shim's socket).
pub fn listen_addr(raw: &str, docker_sock: Option<PathBuf>) -> Result<ListenAddr, String> {
    let addr = ListenAddr::parse(raw)?;
    if let (ListenAddr::Unix(p), Some(sock)) = (&addr, docker_sock)
        && *p == sock
    {
        return Err(format!(
            "{ENV_LISTEN}={} is the built-in docker_sock path; pick another socket",
            p.display()
        ));
    }
    Ok(addr)
}

/// Serve the adapter when [`ENV_LISTEN`] is set; off by default. Errors are logged, never fatal to the daemon.
pub fn spawn_if_enabled(manager: Arc<VmManager>, cfg: &fluxvm_core::config::Config) {
    let raw = std::env::var(ENV_LISTEN).unwrap_or_default();
    if raw.trim().is_empty() {
        return;
    }
    let addr = match listen_addr(&raw, cfg.docker_sock_path()) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "Docker API adapter not started");
            return;
        }
    };
    tracing::info!(docker_host = %addr.docker_host(), "starting Docker API adapter");
    let app = fluxvm_docker_api::router(Arc::new(FluxvmAdapter::new(manager)));
    tokio::spawn(async move {
        if let Err(e) = fluxvm_docker_api::serve(addr, app).await {
            tracing::error!(error = %e, "Docker API adapter exited");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_engine::vm_fixture;
    use axum::{body::Body, http::Request};
    use fluxvm_core::config::Config;
    use futures::StreamExt as _;
    use serde_json::Value;
    use tower::ServiceExt as _;

    /// A real `VmManager` over a temporary store holding `vms` (name, status, OCI?).
    async fn manager(vms: &[(&str, VmStatus, bool)]) -> (Arc<VmManager>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            state_dir: dir.path().join("state"),
            run_dir: dir.path().join("run"),
            ..Config::default()
        };
        let m = VmManager::new(cfg).unwrap();
        for (i, (name, status, oci)) in vms.iter().enumerate() {
            let mut vm = vm_fixture(name, *status, *oci);
            vm.created_at += chrono::Duration::seconds(i as i64);
            vm.log_path = dir.path().join(format!("{name}.log"));
            m.store.insert(vm).await.unwrap();
        }
        (m, dir)
    }

    async fn id_of(m: &VmManager, name: &str) -> String {
        let vm = m.list().await.into_iter().find(|v| v.name == name).unwrap();
        docker_id(vm.id)
    }

    async fn collect(logs: Logs) -> String {
        let parts: Vec<_> = logs.chunks.collect().await;
        parts
            .into_iter()
            .map(|c| String::from_utf8(c.unwrap().data.to_vec()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn list_shows_oci_sandboxes_newest_first() {
        let (m, _d) = manager(&[
            ("old", VmStatus::Running, true),
            ("plain-vm", VmStatus::Running, false),
            ("gone", VmStatus::Stopped, true),
            ("new", VmStatus::Running, true),
        ])
        .await;
        let a = FluxvmAdapter::new(m.clone());
        let up = a.list(false).await.unwrap();
        let names: Vec<_> = up.iter().map(|c| c.names[0].as_str()).collect();
        assert_eq!(
            names,
            ["/new", "/old"],
            "running OCI sandboxes only, newest first"
        );
        assert_eq!(up[0].image, "alpine:3.22");
        assert_eq!(up[0].state, "running");
        assert_eq!(up[0].id, id_of(&m, "new").await);
        let all = a.list(true).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].state, "exited");
    }

    #[tokio::test]
    async fn router_over_the_control_plane_lists_containers() {
        let (m, _d) = manager(&[("web", VmStatus::Running, true)]).await;
        let app = fluxvm_docker_api::router(Arc::new(FluxvmAdapter::new(m.clone())));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1.44/containers/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v[0]["Names"][0], "/web");
        assert_eq!(v[0]["Id"], id_of(&m, "web").await);
    }

    #[tokio::test]
    async fn create_checks_names_and_shares_the_shim_mapping() {
        let (m, _d) = manager(&[("taken", VmStatus::Running, false)]).await;
        let a = FluxvmAdapter::new(m.clone());
        let req = CreateRequest {
            image: "alpine:3.22".into(),
            ..Default::default()
        };
        let e = a
            .create(Some("taken".into()), req.clone())
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Conflict);
        assert!(e.message.contains("\"/taken\" is already in use"));

        let mut binds = req.clone();
        binds.host_config.binds = Some(vec!["/etc:/host-etc".into()]);
        let e = a.create(Some("b1".into()), binds).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::BadRequest);
        assert!(e.message.contains("bind mounts"), "{}", e.message);

        let mut tty = req;
        tty.tty = true;
        let e = a.create(None, tty).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotImplemented);
        assert_eq!(m.list().await.len(), 1, "nothing was created");
    }

    #[tokio::test]
    #[ignore = "boots a real sandbox VM (KVM/hypervisor + image pull); run on a VM-capable host with --ignored"]
    async fn create_reaches_the_control_plane() {
        // End-to-end through create_sandbox: on a VM-capable host this boots the sandbox.
        let (m, _d) = manager(&[]).await;
        let a = FluxvmAdapter::new(m.clone());
        let req = CreateRequest {
            image: "alpine:3.22".into(),
            cmd: Some(vec!["echo".into(), "hi".into()]),
            env: Some(vec!["A=1".into()]),
            ..Default::default()
        };
        match a.create(Some("fresh".into()), req).await {
            Ok(id) => assert_eq!(id, id_of(&m, "fresh").await),
            Err(e) => {
                assert_eq!(e.kind, ErrorKind::BadRequest);
                assert!(
                    !e.message.contains("invalid container config"),
                    "{}",
                    e.message
                );
            }
        }
    }

    #[tokio::test]
    async fn start_and_stop_report_no_op_transitions() {
        let (m, _d) = manager(&[
            ("up", VmStatus::Running, true),
            ("down", VmStatus::Stopped, true),
            ("booting", VmStatus::Creating, true),
        ])
        .await;
        let a = FluxvmAdapter::new(m.clone());
        assert_eq!(a.start("up".into()).await.unwrap(), Transition::Unchanged);
        assert_eq!(
            a.start("booting".into()).await.unwrap(),
            Transition::Changed
        );
        assert_eq!(
            a.stop("down".into(), Some(3)).await.unwrap(),
            Transition::Unchanged
        );
        let prefix = id_of(&m, "down").await[..8].to_owned();
        assert_eq!(a.stop(prefix, None).await.unwrap(), Transition::Unchanged);
        let e = a.start("missing".into()).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.message, "No such container: missing");
        let e = a.stop("missing".into(), None).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
    }

    #[tokio::test]
    #[ignore = "boots a real sandbox VM (KVM/hypervisor + image pull); run on a VM-capable host with --ignored"]
    async fn start_boots_a_stopped_sandbox_through_the_manager() {
        let (m, _d) = manager(&[("down", VmStatus::Stopped, true)]).await;
        let a = FluxvmAdapter::new(m.clone());
        // End-to-end through VmManager::start: boots the VM on a VM-capable host, otherwise a 500.
        match a.start("down".into()).await {
            Ok(t) => assert_eq!(t, Transition::Changed),
            Err(e) => assert_eq!(e.kind, ErrorKind::Internal, "{}", e.message),
        }
    }

    #[tokio::test]
    async fn logs_tail_and_follow_the_console() {
        let (m, d) = manager(&[("app", VmStatus::Stopped, true)]).await;
        std::fs::write(d.path().join("app.log"), "one\ntwo\nthree\n").unwrap();
        let a = FluxvmAdapter::new(m.clone());
        let req = LogsRequest {
            stdout: true,
            stderr: true,
            follow: false,
            tail: None,
        };
        let all = collect(a.logs("app".into(), req).await.unwrap()).await;
        assert!(all.contains("one") && all.contains("three"), "{all:?}");
        let last = collect(
            a.logs(
                "app".into(),
                LogsRequest {
                    tail: Some(1),
                    ..req
                },
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(last.contains("three") && !last.contains("one"), "{last:?}");
        let none = collect(
            a.logs(
                "app".into(),
                LogsRequest {
                    tail: Some(0),
                    ..req
                },
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(none, "");
        // The VM is stopped, so follow ends after the tail instead of hanging.
        let followed = tokio::time::timeout(
            Duration::from_secs(5),
            collect(
                a.logs(
                    "app".into(),
                    LogsRequest {
                        follow: true,
                        tail: Some(2),
                        ..req
                    },
                )
                .await
                .unwrap(),
            ),
        )
        .await
        .expect("follow of a stopped container ends");
        assert!(
            followed.contains("two") && followed.contains("three") && !followed.contains("one")
        );
        let e = a.logs("nope".into(), req).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::NotFound);
    }

    #[test]
    fn listen_addr_refuses_the_docker_sock_path() {
        let sock = PathBuf::from("/run/fluxvm/docker.sock");
        assert!(listen_addr("unix:///run/fluxvm/docker.sock", Some(sock.clone())).is_err());
        assert_eq!(
            listen_addr("/run/fluxvm/docker-api.sock", Some(sock)),
            Ok(ListenAddr::Unix("/run/fluxvm/docker-api.sock".into()))
        );
        assert!(listen_addr("0.0.0.0:2375", None).is_err());
    }
}
