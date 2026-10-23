// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Docker Engine API subset on a unix socket.
//!
//! Maps containers to OCI sandboxes and images to the FluxVM OCI cache.
//! Supported CLI: `docker pull|images|run|ps|rm|logs|exec|stop`.
//! Not supported: `build`, Compose, bind mounts, Docker networks.

use axum::{
    Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json,
};
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_guest_protocol::{AgentResponse, ExecProcess};
use fluxvm_oci_init::{config::ExitPolicy, supervise::RestartPolicy};
use fluxvm_scheduler::{
    oci_images::OciImageEntry,
    oci_sandbox::{OciSandboxSpec, OCI_IMAGE_LABEL},
    SandboxCreateRequest, VmManager,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::UnixListener;
use uuid::Uuid;

const API_VERSION: &str = "1.43";
const DOCKER_VERSION: &str = "24.0.0-fluxvm";
const PLATFORM_NAME: &str = "FluxVM";

#[derive(Clone)]
struct DockerState {
    manager: Arc<VmManager>,
    execs: Arc<Mutex<BTreeMap<String, ExecSession>>>,
}

#[derive(Clone)]
struct ExecSession {
    container: Uuid,
    cmd: Vec<String>,
    env: Vec<String>,
    working_dir: Option<String>,
    user: Option<String>,
}

#[derive(Debug)]
struct DockerError {
    status: StatusCode,
    message: String,
}

impl DockerError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
}

impl From<anyhow::Error> for DockerError {
    fn from(e: anyhow::Error) -> Self {
        Self::bad(format!("{e:#}"))
    }
}

impl IntoResponse for DockerError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "message": self.message }))).into_response()
    }
}

type DockerResult<T> = Result<T, DockerError>;

/// Axum router for the Docker Engine API subset (no FluxVM bearer auth).
pub fn router(manager: Arc<VmManager>) -> Router {
    let state = DockerState {
        manager,
        execs: Arc::new(Mutex::new(BTreeMap::new())),
    };
    Router::new()
        .route("/_ping", get(ping))
        .route("/version", get(version))
        .route("/info", get(info))
        .route("/containers/json", get(containers_list))
        .route("/containers/create", post(containers_create))
        .route("/containers/{id}/json", get(containers_inspect))
        .route("/containers/{id}/start", post(containers_start))
        .route("/containers/{id}/stop", post(containers_stop))
        .route("/containers/{id}/wait", post(containers_wait))
        .route("/containers/{id}/logs", get(containers_logs))
        .route("/containers/{id}/attach", post(containers_attach))
        .route("/containers/{id}/exec", post(containers_exec_create))
        .route("/containers/{id}", delete(containers_delete))
        .route("/exec/{id}/start", post(exec_start))
        .route("/exec/{id}/json", get(exec_inspect))
        .route("/images/json", get(images_list))
        .route("/images/create", post(images_create))
        .route("/images/{name}", delete(images_delete))
        .layer(middleware::from_fn(strip_api_version))
        .with_state(state)
}

/// Bind `{path}` and serve the Docker shim until the process exits.
pub async fn serve_unix(path: PathBuf, manager: Arc<VmManager>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(path = %path.display(), "Docker Engine API listening");
    axum::serve(listener, router(manager)).await?;
    Ok(())
}

/// Spawn [`serve_unix`] when `cfg.docker_sock_path()` is set.
pub fn spawn_if_configured(manager: Arc<VmManager>, cfg: &fluxvm_core::config::Config) {
    let Some(path) = cfg.docker_sock_path() else {
        return;
    };
    tokio::spawn(async move {
        if let Err(e) = serve_unix(path, manager).await {
            tracing::error!(error = %e, "Docker Engine API socket exited");
        }
    });
}

async fn strip_api_version(mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if let Some(stripped) = strip_version_prefix(&path) {
        let new_pq = match req.uri().query() {
            Some(q) => format!("{stripped}?{q}"),
            None => stripped,
        };
        if let Ok(pq) = new_pq.parse::<axum::http::uri::PathAndQuery>() {
            let mut parts = req.uri().clone().into_parts();
            parts.path_and_query = Some(pq);
            if let Ok(uri) = Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    next.run(req).await
}

fn strip_version_prefix(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v")?;
    let (ver, after) = rest.split_once('/')?;
    if !ver.is_empty() && ver.chars().all(|c| c.is_ascii_digit() || c == '.') {
        Some(format!("/{after}"))
    } else {
        None
    }
}

fn docker_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "Api-Version",
        HeaderValue::from_static(API_VERSION),
    );
    h.insert(
        "Docker-Experimental",
        HeaderValue::from_static("false"),
    );
    h.insert(
        "Ostype",
        HeaderValue::from_static("linux"),
    );
    h.insert(
        "Server",
        HeaderValue::from_static("FluxVM"),
    );
    h
}

async fn ping() -> impl IntoResponse {
    let mut res = (StatusCode::OK, "OK").into_response();
    res.headers_mut().extend(docker_headers());
    res
}

async fn version(State(st): State<DockerState>) -> impl IntoResponse {
    let _ = &st;
    let body = json!({
        "Version": DOCKER_VERSION,
        "ApiVersion": API_VERSION,
        "MinAPIVersion": "1.24",
        "GitCommit": "fluxvm",
        "GoVersion": "go1.21",
        "Os": "linux",
        "Arch": std::env::consts::ARCH,
        "KernelVersion": PLATFORM_NAME,
        "Platform": { "Name": PLATFORM_NAME },
        "Components": []
    });
    let mut res = Json(body).into_response();
    res.headers_mut().extend(docker_headers());
    res
}

async fn info(State(st): State<DockerState>) -> impl IntoResponse {
    let containers = oci_containers(&st.manager).await;
    let images = st.manager.oci_list().unwrap_or_default().len();
    let running = containers
        .iter()
        .filter(|v| v.status == VmStatus::Running)
        .count();
    let stopped = containers.len().saturating_sub(running);
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let body = json!({
        "ID": "fluxvm",
        "Containers": containers.len(),
        "ContainersRunning": running,
        "ContainersPaused": 0,
        "ContainersStopped": stopped,
        "Images": images,
        "Driver": "fluxvm-oci",
        "DriverStatus": [["Backend", "vz"], ["Isolation", "microVM"]],
        "MemoryLimit": true,
        "SwapLimit": false,
        "IPv4Forwarding": true,
        "Debug": false,
        "SystemTime": chrono::Utc::now().to_rfc3339(),
        "LoggingDriver": "json-file",
        "CgroupDriver": "none",
        "KernelVersion": PLATFORM_NAME,
        "OperatingSystem": "FluxVM (macOS host)",
        "OSType": "linux",
        "Architecture": std::env::consts::ARCH,
        "IndexServerAddress": "https://index.docker.io/v1/",
        "NCPU": ncpu,
        "MemTotal": 0,
        "DockerRootDir": st.manager.cfg.state_dir.join("oci").display().to_string(),
        "Name": "fluxvm",
        "Labels": ["com.zyvor.fluxvm=true"],
        "ExperimentalBuild": false,
        "ServerVersion": DOCKER_VERSION,
        "DefaultRuntime": "runc",
        "Runtimes": { "runc": { "path": "fluxvm-oci" } },
        "Swarm": { "LocalNodeState": "inactive" },
        "InitBinary": "fluxvm-oci-init",
        "SecurityOptions": ["name=fluxvm"],
        "ProductLicense": "Apache-2.0",
        "Warnings": [
            "FluxVM Docker API shim: containers are OCI sandboxes (one microVM each). build/compose/networks/bind-mounts are not supported."
        ]
    });
    let mut res = Json(body).into_response();
    res.headers_mut().extend(docker_headers());
    res
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    all: bool,
}

async fn containers_list(
    State(st): State<DockerState>,
    Query(q): Query<ListQuery>,
) -> DockerResult<Json<Value>> {
    let mut items = Vec::new();
    for vm in oci_containers(&st.manager).await {
        if !q.all && vm.status != VmStatus::Running && vm.status != VmStatus::Paused {
            continue;
        }
        items.push(container_summary(&vm));
    }
    Ok(Json(Value::Array(items)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateQuery {
    name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ContainerCreateBody {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Value>,
    #[serde(rename = "Env")]
    env: Vec<String>,
    #[serde(rename = "WorkingDir")]
    working_dir: String,
    #[serde(rename = "User")]
    user: String,
    #[serde(rename = "Labels")]
    labels: BTreeMap<String, String>,
    #[serde(rename = "HostConfig")]
    host_config: HostConfigBody,
    #[serde(rename = "ExposedPorts")]
    exposed_ports: BTreeMap<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HostConfigBody {
    #[serde(rename = "PortBindings")]
    port_bindings: BTreeMap<String, Option<Vec<PortBinding>>>,
    #[serde(rename = "Memory")]
    memory: i64,
    #[serde(rename = "NanoCpus")]
    nano_cpus: i64,
    #[serde(rename = "AutoRemove")]
    auto_remove: bool,
    #[serde(rename = "Binds")]
    binds: Vec<String>,
    #[serde(rename = "RestartPolicy")]
    restart_policy: RestartPolicyBody,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PortBinding {
    #[serde(rename = "HostIp")]
    host_ip: String,
    #[serde(rename = "HostPort")]
    host_port: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestartPolicyBody {
    #[serde(rename = "Name")]
    name: String,
}

fn map_create_request(
    body: ContainerCreateBody,
    name: Option<String>,
) -> DockerResult<SandboxCreateRequest> {
    if body.image.trim().is_empty() {
        return Err(DockerError::bad("Image is required"));
    }
    if !body.host_config.binds.is_empty() {
        return Err(DockerError::bad(
            "bind mounts are not supported by the FluxVM Docker shim; use named volumes via fluxctl",
        ));
    }
    let ports = port_bindings_to_specs(&body.host_config.port_bindings)?;
    let expose = exposed_ports_to_u16(&body.exposed_ports)?;
    let entrypoint = parse_string_or_vec(body.entrypoint);
    let memory_mib = if body.host_config.memory > 0 {
        Some((body.host_config.memory as u64).div_ceil(1024 * 1024).max(512))
    } else {
        None
    };
    let vcpus = if body.host_config.nano_cpus > 0 {
        Some(
            ((body.host_config.nano_cpus as u64).div_ceil(1_000_000_000) as u8)
                .max(1),
        )
    } else {
        None
    };
    let restart = match body.host_config.restart_policy.name.as_str() {
        "always" | "unless-stopped" => RestartPolicy::Always,
        "on-failure" => RestartPolicy::OnFailure,
        _ => RestartPolicy::No,
    };
    let exit_policy = if body.host_config.auto_remove {
        ExitPolicy::Poweroff
    } else {
        ExitPolicy::Keep
    };
    let oci = OciSandboxSpec {
        image: body.image,
        platform: None,
        command: body.cmd,
        entrypoint,
        env: body.env,
        workdir: if body.working_dir.is_empty() {
            None
        } else {
            Some(body.working_dir)
        },
        user: if body.user.is_empty() {
            None
        } else {
            Some(body.user)
        },
        read_only_root: true,
        exit_policy,
        ports,
        expose,
        gateway_hosts: Vec::new(),
        restart,
        max_restarts: None,
        healthcheck: None,
        secret_env: BTreeMap::new(),
        networks: Vec::new(),
        hosts: Vec::new(),
    };
    let _ = body.labels; // OCI create path sets its own labels; user labels via PATCH later
    let mut req: SandboxCreateRequest = serde_json::from_value(json!({})).map_err(|e| {
        DockerError::bad(format!("internal sandbox request defaults: {e}"))
    })?;
    req.name = name;
    req.oci = Some(oci);
    req.vcpus = vcpus;
    req.memory_mib = memory_mib;
    Ok(req)
}

async fn containers_create(
    State(st): State<DockerState>,
    Query(q): Query<CreateQuery>,
    Json(body): Json<ContainerCreateBody>,
) -> DockerResult<impl IntoResponse> {
    let req = map_create_request(body, q.name)?;
    let vm = st
        .manager
        .create_sandbox(req, None, Some("docker-api"))
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "Id": docker_id(vm.id),
            "Warnings": []
        })),
    ))
}

async fn containers_inspect(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<Json<Value>> {
    let vm = resolve_container(&st.manager, &id).await?;
    Ok(Json(container_inspect(&vm)))
}

async fn containers_start(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    match vm.status {
        VmStatus::Running | VmStatus::Paused => Ok(StatusCode::NO_CONTENT),
        VmStatus::Creating => Ok(StatusCode::NO_CONTENT),
        VmStatus::Stopped | VmStatus::Failed => {
            st.manager.start(vm.id).await?;
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

#[derive(Deserialize)]
struct StopQuery {
    #[allow(dead_code)]
    t: Option<u64>,
}

async fn containers_stop(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(_q): Query<StopQuery>,
) -> DockerResult<StatusCode> {
    let vm = resolve_container(&st.manager, &id).await?;
    if vm.status == VmStatus::Stopped {
        return Ok(StatusCode::NOT_MODIFIED);
    }
    st.manager.stop(vm.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn containers_wait(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<Json<Value>> {
    let vm = resolve_container(&st.manager, &id).await?;
    let code = wait_exit_code(&st.manager, vm.id).await?;
    Ok(Json(json!({ "StatusCode": code, "Error": null })))
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    stdout: bool,
    #[serde(default)]
    stderr: bool,
    #[serde(default)]
    timestamps: bool,
    #[serde(default)]
    follow: bool,
    #[serde(default = "default_tail")]
    tail: String,
}

fn default_tail() -> String {
    "all".into()
}

async fn containers_logs(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> DockerResult<Response> {
    let _ = (q.stdout, q.stderr, q.timestamps); // console is combined
    let vm = resolve_container(&st.manager, &id).await?;
    let lines = parse_tail(&q.tail);
    if q.follow {
        return Ok(stream_console(&st.manager, vm.id, &vm.log_path, lines, true).await);
    }
    let logs = st.manager.sandbox_logs(vm.id, lines).await?;
    let body = mux_stdout(logs.log.as_bytes());
    Ok(raw_stream_response(body))
}

#[derive(Deserialize)]
struct AttachQuery {
    #[serde(default)]
    stdout: bool,
    #[serde(default)]
    stderr: bool,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    logs: bool,
}

async fn containers_attach(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<AttachQuery>,
) -> DockerResult<Response> {
    let _ = (q.stdout, q.stderr);
    let vm = resolve_container(&st.manager, &id).await?;
    let follow = q.stream;
    let include_logs = q.logs || follow;
    if !include_logs {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok(stream_console(&st.manager, vm.id, &vm.log_path, 10_000, follow).await)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecCreateBody {
    #[serde(rename = "AttachStdin")]
    attach_stdin: bool,
    #[serde(rename = "AttachStdout")]
    attach_stdout: bool,
    #[serde(rename = "AttachStderr")]
    attach_stderr: bool,
    #[serde(rename = "Tty")]
    tty: bool,
    #[serde(rename = "Cmd")]
    cmd: Vec<String>,
    #[serde(rename = "Env")]
    env: Vec<String>,
    #[serde(rename = "WorkingDir")]
    working_dir: String,
    #[serde(rename = "User")]
    user: String,
}

async fn containers_exec_create(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Json(body): Json<ExecCreateBody>,
) -> DockerResult<impl IntoResponse> {
    let _ = (body.attach_stdin, body.attach_stdout, body.attach_stderr, body.tty);
    if body.cmd.is_empty() {
        return Err(DockerError::bad("Cmd is required"));
    }
    let vm = resolve_container(&st.manager, &id).await?;
    let exec_id = Uuid::new_v4().as_simple().to_string();
    st.execs.lock().unwrap().insert(
        exec_id.clone(),
        ExecSession {
            container: vm.id,
            cmd: body.cmd,
            env: body.env,
            working_dir: if body.working_dir.is_empty() {
                None
            } else {
                Some(body.working_dir)
            },
            user: if body.user.is_empty() {
                None
            } else {
                Some(body.user)
            },
        },
    );
    Ok((StatusCode::CREATED, Json(json!({ "Id": exec_id }))))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecStartBody {
    #[serde(rename = "Detach")]
    detach: bool,
    #[serde(rename = "Tty")]
    tty: bool,
}

async fn exec_start(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Json(body): Json<ExecStartBody>,
) -> DockerResult<Response> {
    let session = st
        .execs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| DockerError::not_found(format!("No such exec instance: {id}")))?;
    if body.detach {
        let st2 = st.clone();
        let session2 = session.clone();
        tokio::spawn(async move {
            let _ = run_exec(&st2.manager, &session2).await;
        });
        return Ok(StatusCode::OK.into_response());
    }
    let (stdout, stderr, _code) = run_exec(&st.manager, &session).await?;
    let mut bytes = Vec::new();
    if body.tty {
        bytes.extend_from_slice(stdout.as_bytes());
        bytes.extend_from_slice(stderr.as_bytes());
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/vnd.docker.raw-stream")
            .body(Body::from(bytes))
            .unwrap())
    } else {
        bytes.extend(mux_frame(1, stdout.as_bytes()));
        bytes.extend(mux_frame(2, stderr.as_bytes()));
        Ok(raw_stream_response(bytes))
    }
}

async fn exec_inspect(
    State(st): State<DockerState>,
    Path(id): Path<String>,
) -> DockerResult<Json<Value>> {
    let session = st
        .execs
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| DockerError::not_found(format!("No such exec instance: {id}")))?;
    Ok(Json(json!({
        "ID": id,
        "Running": false,
        "ExitCode": 0,
        "ProcessConfig": {
            "entrypoint": session.cmd.first().cloned().unwrap_or_default(),
            "arguments": session.cmd.get(1..).unwrap_or(&[]),
            "privileged": false,
            "tty": false,
            "user": session.user.unwrap_or_default()
        },
        "OpenStdin": false,
        "OpenStderr": true,
        "OpenStdout": true,
        "ContainerID": docker_id(session.container)
    })))
}

#[derive(Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    v: bool,
}

async fn containers_delete(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> DockerResult<StatusCode> {
    let _ = q.v;
    let vm = resolve_container(&st.manager, &id).await?;
    if vm.status == VmStatus::Running && !q.force {
        return Err(DockerError::new(
            StatusCode::CONFLICT,
            "cannot remove a running container; use force=true or stop first",
        ));
    }
    if vm.status == VmStatus::Running {
        let _ = st.manager.stop(vm.id).await;
    }
    st.manager.delete(vm.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn images_list(State(st): State<DockerState>) -> DockerResult<Json<Value>> {
    let entries = st.manager.oci_list()?;
    let items: Vec<Value> = entries.iter().map(image_summary).collect();
    Ok(Json(Value::Array(items)))
}

#[derive(Deserialize)]
struct ImageCreateQuery {
    #[serde(rename = "fromImage")]
    from_image: Option<String>,
    tag: Option<String>,
    platform: Option<String>,
}

async fn images_create(
    State(st): State<DockerState>,
    Query(q): Query<ImageCreateQuery>,
) -> DockerResult<Response> {
    let image = q.from_image.ok_or_else(|| DockerError::bad("fromImage is required"))?;
    let reference = match q.tag.as_deref() {
        Some(t) if !t.is_empty() && !image.contains(':') && !image.contains('@') => {
            format!("{image}:{t}")
        }
        _ => image,
    };
    let platform = q.platform.filter(|p| !p.is_empty());
    let entry = st
        .manager
        .oci_pull(&reference, platform.as_deref())
        .await?;
    let lines = [
        json!({"status": format!("Pulling from {reference}")}),
        json!({"status": format!("Digest: {}", entry.manifest_digest)}),
        json!({"status": format!("Status: Image is up to date for {reference}")}),
    ];
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(&line).unwrap());
        body.push('\n');
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap())
}

async fn images_delete(
    State(st): State<DockerState>,
    Path(name): Path<String>,
) -> DockerResult<Json<Value>> {
    let name = urlencoding_decode(&name);
    let removed = st.manager.oci_remove(&name)?;
    Ok(Json(json!([
        { "Untagged": removed.references.first().cloned().unwrap_or_default() },
        { "Deleted": removed.manifest_digest }
    ])))
}

fn urlencoding_decode(s: &str) -> String {
    // Docker encodes `/` as `%2F` in image names on the path.
    s.replace("%2F", "/").replace("%2f", "/")
}

async fn oci_containers(m: &VmManager) -> Vec<VmRecord> {
    m.list()
        .await
        .into_iter()
        .filter(|v| v.labels.contains_key(OCI_IMAGE_LABEL))
        .collect()
}

async fn resolve_container(m: &VmManager, id_or_name: &str) -> DockerResult<VmRecord> {
    let all = oci_containers(m).await;
    if let Ok(u) = Uuid::parse_str(id_or_name) {
        return all
            .into_iter()
            .find(|v| v.id == u)
            .ok_or_else(|| DockerError::not_found(format!("No such container: {id_or_name}")));
    }
    let needle = id_or_name.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<VmRecord> = all
        .into_iter()
        .filter(|v| {
            let simple = v.id.as_simple().to_string();
            simple.starts_with(&needle)
                || v.name == id_or_name
                || v.name.trim_start_matches('/') == id_or_name.trim_start_matches('/')
        })
        .collect();
    match matches.len() {
        0 => Err(DockerError::not_found(format!(
            "No such container: {id_or_name}"
        ))),
        1 => Ok(matches.pop().unwrap()),
        _ => Err(DockerError::bad(format!(
            "Multiple containers match prefix {id_or_name}"
        ))),
    }
}

fn docker_id(id: Uuid) -> String {
    id.as_simple().to_string()
}

fn container_summary(vm: &VmRecord) -> Value {
    let image = vm
        .labels
        .get(OCI_IMAGE_LABEL)
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let (state, _) = docker_state(&vm.status);
    let ports = Value::Array(vec![]);
    json!({
        "Id": docker_id(vm.id),
        "Names": [format!("/{}", vm.name)],
        "Image": image,
        "ImageID": vm.labels.get(fluxvm_scheduler::oci_images::OCI_LABEL).cloned().unwrap_or_default(),
        "Command": "",
        "Created": vm.created_at.timestamp(),
        "Ports": ports,
        "Labels": vm.labels,
        "State": state,
        "Status": docker_status_text(vm),
        "HostConfig": { "NetworkMode": "default" },
        "NetworkSettings": { "Networks": {} },
        "Mounts": []
    })
}

fn container_inspect(vm: &VmRecord) -> Value {
    let image = vm
        .labels
        .get(OCI_IMAGE_LABEL)
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let (state, running) = docker_state(&vm.status);
    json!({
        "Id": docker_id(vm.id),
        "Created": vm.created_at.to_rfc3339(),
        "Path": "",
        "Args": [],
        "State": {
            "Status": state,
            "Running": running,
            "Paused": vm.status == VmStatus::Paused,
            "Restarting": false,
            "OOMKilled": false,
            "Dead": false,
            "Pid": vm.pid.unwrap_or(0),
            "ExitCode": 0,
            "Error": vm.error.clone().unwrap_or_default(),
            "StartedAt": vm.created_at.to_rfc3339(),
            "FinishedAt": "0001-01-01T00:00:00Z"
        },
        "Image": image,
        "ResolvConfPath": "",
        "HostnamePath": "",
        "HostsPath": "",
        "LogPath": vm.log_path.display().to_string(),
        "Name": format!("/{}", vm.name),
        "RestartCount": 0,
        "Driver": "fluxvm-oci",
        "Platform": "linux",
        "MountLabel": "",
        "ProcessLabel": "",
        "AppArmorProfile": "",
        "ExecIDs": null,
        "HostConfig": {
            "NetworkMode": "default",
            "PortBindings": {},
            "RestartPolicy": { "Name": "no", "MaximumRetryCount": 0 },
            "AutoRemove": false,
            "Memory": vm.request.memory_mib.saturating_mul(1024 * 1024),
            "NanoCpus": (vm.request.vcpus as i64) * 1_000_000_000
        },
        "GraphDriver": { "Name": "fluxvm-oci", "Data": null },
        "Mounts": [],
        "Config": {
            "Hostname": vm.name,
            "Domainname": "",
            "User": "",
            "AttachStdin": false,
            "AttachStdout": true,
            "AttachStderr": true,
            "ExposedPorts": {},
            "Tty": false,
            "OpenStdin": false,
            "StdinOnce": false,
            "Env": [],
            "Cmd": null,
            "Image": image,
            "Volumes": null,
            "WorkingDir": "",
            "Entrypoint": null,
            "Labels": vm.labels,
        },
        "NetworkSettings": {
            "Bridge": "",
            "SandboxID": "",
            "Ports": {},
            "Networks": {
                "bridge": {
                    "IPAddress": vm.guest_ip.clone().unwrap_or_default(),
                    "Gateway": "",
                    "MacAddress": ""
                }
            }
        }
    })
}

fn docker_state(status: &VmStatus) -> (&'static str, bool) {
    match status {
        VmStatus::Running => ("running", true),
        VmStatus::Paused => ("paused", true),
        VmStatus::Creating => ("created", false),
        VmStatus::Stopped | VmStatus::Failed => ("exited", false),
    }
}

fn docker_status_text(vm: &VmRecord) -> String {
    match vm.status {
        VmStatus::Running => "Up".into(),
        VmStatus::Paused => "Up (paused)".into(),
        VmStatus::Creating => "Created".into(),
        VmStatus::Stopped => "Exited".into(),
        VmStatus::Failed => "Exited (error)".into(),
    }
}

fn image_summary(entry: &OciImageEntry) -> Value {
    let repo_tags: Vec<String> = entry
        .references
        .iter()
        .cloned()
        .filter(|r| !r.contains('@'))
        .collect();
    let repo_digests: Vec<String> = entry
        .references
        .iter()
        .filter(|r| r.contains('@'))
        .cloned()
        .collect();
    let id = if entry.manifest_digest.starts_with("sha256:") {
        entry.manifest_digest.clone()
    } else {
        format!("sha256:{}", entry.manifest_digest)
    };
    json!({
        "Id": id,
        "RepoTags": if repo_tags.is_empty() { Value::Null } else { json!(repo_tags) },
        "RepoDigests": if repo_digests.is_empty() { Value::Null } else { json!(repo_digests) },
        "Created": entry.built_at as i64,
        "Size": entry.size_bytes,
        "SharedSize": -1,
        "VirtualSize": entry.size_bytes,
        "Labels": null,
        "Containers": -1
    })
}

fn parse_string_or_vec(v: Option<Value>) -> Option<Vec<String>> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(vec![s]),
        Some(Value::Array(a)) => Some(
            a.into_iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
        ),
        Some(_) => None,
    }
}

fn port_bindings_to_specs(
    bindings: &BTreeMap<String, Option<Vec<PortBinding>>>,
) -> DockerResult<Vec<String>> {
    let mut out = Vec::new();
    for (key, binds) in bindings {
        let (port_s, proto) = match key.split_once('/') {
            Some((p, proto)) => (p, proto),
            None => (key.as_str(), "tcp"),
        };
        let guest: u16 = port_s
            .parse()
            .map_err(|_| DockerError::bad(format!("invalid port key {key}")))?;
        let Some(binds) = binds else { continue };
        for b in binds {
            let host: u16 = if b.host_port.is_empty() {
                guest
            } else {
                b.host_port
                    .parse()
                    .map_err(|_| DockerError::bad(format!("invalid HostPort {}", b.host_port)))?
            };
            if !b.host_ip.is_empty() && b.host_ip != "0.0.0.0" && b.host_ip != "127.0.0.1" {
                return Err(DockerError::bad(
                    "only HostIp 0.0.0.0 or 127.0.0.1 is supported",
                ));
            }
            out.push(if proto.eq_ignore_ascii_case("udp") {
                format!("{host}:{guest}/udp")
            } else {
                format!("{host}:{guest}")
            });
        }
    }
    Ok(out)
}

fn exposed_ports_to_u16(exposed: &BTreeMap<String, Value>) -> DockerResult<Vec<u16>> {
    let mut out = Vec::new();
    for key in exposed.keys() {
        let port_s = key.split('/').next().unwrap_or(key.as_str());
        let p: u16 = port_s
            .parse()
            .map_err(|_| DockerError::bad(format!("invalid ExposedPorts key {key}")))?;
        out.push(p);
    }
    Ok(out)
}

fn parse_tail(tail: &str) -> usize {
    if tail.eq_ignore_ascii_case("all") {
        10_000
    } else {
        tail.parse().unwrap_or(200).clamp(1, 10_000)
    }
}

fn mux_frame(stream_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.push(stream_type);
    out.extend_from_slice(&[0, 0, 0]);
    let len = payload.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn mux_stdout(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() {
        return Vec::new();
    }
    mux_frame(1, payload)
}

fn raw_stream_response(body: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.docker.raw-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn stream_console(
    m: &Arc<VmManager>,
    id: Uuid,
    log_path: &FsPath,
    _tail_lines: usize,
    follow: bool,
) -> Response {
    let path = log_path.to_path_buf();
    let manager = m.clone();
    let stream = async_stream::stream! {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut offset = 0u64;
        loop {
            match tokio::fs::File::open(&path).await {
                Ok(mut f) => {
                    if f.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                        let mut buf = Vec::new();
                        if f.read_to_end(&mut buf).await.is_ok() && !buf.is_empty() {
                            offset += buf.len() as u64;
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from(mux_stdout(&buf)));
                        }
                    }
                }
                Err(_) => {}
            }
            if let Ok(logs) = manager.sandbox_logs(id, 1).await {
                if logs.exit_code.is_some() {
                    break;
                }
            }
            if !follow {
                break;
            }
            let vm = manager.get(id).await;
            if let Ok(vm) = vm {
                if matches!(vm.status, VmStatus::Stopped | VmStatus::Failed) {
                    break;
                }
            } else {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.docker.raw-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn wait_exit_code(m: &VmManager, id: Uuid) -> DockerResult<i32> {
    for _ in 0..6000 {
        let logs = m.sandbox_logs(id, 1).await?;
        if let Some(code) = logs.exit_code {
            return Ok(code);
        }
        let vm = m.get(id).await?;
        if matches!(vm.status, VmStatus::Stopped | VmStatus::Failed) {
            return Ok(if vm.status == VmStatus::Failed { 1 } else { 0 });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(DockerError::new(
        StatusCode::REQUEST_TIMEOUT,
        "timed out waiting for container exit",
    ))
}

async fn run_exec(
    m: &Arc<VmManager>,
    session: &ExecSession,
) -> DockerResult<(String, String, i32)> {
    m.ensure_running_for_request(session.container).await?;
    let process = ExecProcess {
        argv: session.cmd.clone(),
        env: session.env.clone(),
        clean_env: false,
        cwd: session.working_dir.clone(),
        user: None,
    };
    let resp = m
        .exec_process(session.container, process, Some(120))
        .await?;
    match resp {
        AgentResponse::Exec {
            exit_code,
            stdout,
            stderr,
            ..
        } => Ok((stdout, stderr, exit_code)),
        other => Err(DockerError::bad(format!("unexpected agent response: {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_engine_api_version_prefix() {
        assert_eq!(
            strip_version_prefix("/v1.43/containers/json").as_deref(),
            Some("/containers/json")
        );
        assert_eq!(strip_version_prefix("/version"), None);
        assert_eq!(strip_version_prefix("/_ping"), None);
        assert_eq!(
            strip_version_prefix("/v1.24/images/create").as_deref(),
            Some("/images/create")
        );
    }

    #[test]
    fn maps_port_bindings_like_docker_run_p() {
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "80/tcp".into(),
            Some(vec![PortBinding {
                host_ip: String::new(),
                host_port: "8080".into(),
            }]),
        );
        bindings.insert(
            "53/udp".into(),
            Some(vec![PortBinding {
                host_ip: "127.0.0.1".into(),
                host_port: "5353".into(),
            }]),
        );
        let ports = port_bindings_to_specs(&bindings).unwrap();
        assert!(ports.contains(&"8080:80".into()));
        assert!(ports.contains(&"5353:53/udp".into()));
    }

    #[test]
    fn create_maps_env_cmd_and_memory() {
        let body = ContainerCreateBody {
            image: "alpine:3.22".into(),
            cmd: Some(vec!["echo".into(), "hi".into()]),
            env: vec!["FOO=bar".into()],
            working_dir: "/tmp".into(),
            host_config: HostConfigBody {
                memory: 512 * 1024 * 1024,
                nano_cpus: 2_000_000_000,
                auto_remove: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let req = map_create_request(body, Some("demo".into())).unwrap();
        assert_eq!(req.name.as_deref(), Some("demo"));
        let oci = req.oci.unwrap();
        assert_eq!(oci.image, "alpine:3.22");
        assert_eq!(oci.command.as_ref().unwrap(), &["echo", "hi"]);
        assert_eq!(oci.env, ["FOO=bar"]);
        assert_eq!(oci.workdir.as_deref(), Some("/tmp"));
        assert_eq!(oci.exit_policy, ExitPolicy::Poweroff);
        assert_eq!(req.memory_mib, Some(512));
        assert_eq!(req.vcpus, Some(2));
    }

    #[test]
    fn rejects_bind_mounts() {
        let body = ContainerCreateBody {
            image: "alpine:3.22".into(),
            host_config: HostConfigBody {
                binds: vec!["/tmp:/tmp".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let err = map_create_request(body, None).unwrap_err();
        assert!(err.message.contains("bind mounts"));
    }

    #[test]
    fn mux_frame_has_docker_header() {
        let frame = mux_frame(1, b"hi\n");
        assert_eq!(frame[0], 1);
        assert_eq!(&frame[1..4], &[0, 0, 0]);
        assert_eq!(u32::from_be_bytes(frame[4..8].try_into().unwrap()), 3);
        assert_eq!(&frame[8..], b"hi\n");
    }
}
