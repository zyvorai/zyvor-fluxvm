// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Docker Engine API subset on a unix socket.
//!
//! Maps containers to OCI sandboxes (a microVM each, or a native Linux container) and images to the FluxVM OCI cache.
//! Supported CLI: `docker pull|images|run|ps|rm|logs|exec|stop`.
//! Also: `restart|kill|pause|unpause|top|stats|rename|events|network ls|volume ls`.
//! `docker build` is forwarded to the FluxVM Mac builder (`docker_build`). Not supported (a Docker-shaped error says what to do): Compose, bind mounts, user-defined networks, named volumes.

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use fluxvm_core::model::{VmPatch, VmRecord, VmStatus};
use fluxvm_guest_protocol::{AgentResponse, ExecProcess, ExecRunAs};
use fluxvm_oci_init::{config::ExitPolicy, supervise::RestartPolicy};
use fluxvm_scheduler::{
    SandboxCreateRequest, VmManager,
    oci_images::OciImageEntry,
    oci_sandbox::{OCI_IMAGE_LABEL, OciSandboxSpec, ProcessState, process_state},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::UnixListener;
use uuid::Uuid;

// 1.44 is the oldest API the current docker CLI (v29) accepts ("API version 1.43 is not supported by this client").
const API_VERSION: &str = "1.44";
const DOCKER_VERSION: &str = "24.0.0-fluxvm";
const PLATFORM_NAME: &str = "FluxVM";

#[derive(Clone)]
struct DockerState {
    manager: Arc<VmManager>,
    execs: Arc<Mutex<BTreeMap<String, ExecSession>>>,
    build: Arc<crate::docker_build::BuildEngine>,
    containers: lifecycle::Tracker,
}

#[derive(Clone)]
struct ExecSession {
    container: Uuid,
    cmd: Vec<String>,
    env: Vec<String>,
    working_dir: Option<String>,
    user: Option<String>,
    running: bool,
    exit_code: Option<i32>,
}

#[derive(Debug)]
pub(crate) struct DockerError {
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
    /// HTTP status and message (for adapters that re-map shim errors).
    pub(crate) fn into_parts(self) -> (StatusCode, String) {
        (self.status, self.message)
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

pub(crate) type DockerResult<T> = Result<T, DockerError>;

/// Axum router for the Docker Engine API subset (no FluxVM bearer auth).
pub fn router(manager: Arc<VmManager>) -> Router {
    let engine = crate::docker_build::BuildEngine::new(
        Arc::new(crate::docker_build::FluxctlBuilder::new(None, None)),
        Arc::new(crate::docker_build::History::new(None)),
    );
    router_with_build(manager, Arc::new(engine))
}

/// [`router`] with the builder and the build history to use for `docker build` (tests pass a fake builder).
pub fn router_with_build(
    manager: Arc<VmManager>,
    build: Arc<crate::docker_build::BuildEngine>,
) -> Router {
    routes(DockerState::new(manager, build))
}

impl DockerState {
    fn new(manager: Arc<VmManager>, build: Arc<crate::docker_build::BuildEngine>) -> Self {
        Self {
            manager,
            execs: Arc::new(Mutex::new(BTreeMap::new())),
            build,
            containers: lifecycle::Tracker::default(),
        }
    }
}

fn routes(state: DockerState) -> Router {
    let routes = Router::new()
        .route("/_ping", get(ping))
        .route("/version", get(version))
        .route("/info", get(info))
        .route("/containers/json", get(containers_list))
        .route("/containers/create", post(containers_create))
        .route("/containers/{id}/json", get(containers_inspect))
        .route("/containers/{id}/start", post(lifecycle::containers_start))
        .route("/containers/{id}/stop", post(lifecycle::containers_stop))
        .route("/containers/{id}/wait", post(lifecycle::containers_wait))
        .route("/containers/{id}/logs", get(lifecycle::containers_logs))
        .route(
            "/containers/{id}/attach",
            post(lifecycle::containers_attach),
        )
        .route("/containers/{id}/exec", post(containers_exec_create))
        .route("/containers/{id}", delete(lifecycle::containers_delete))
        // There is no terminal to resize; the CLI reports an error answer to the user.
        .route("/containers/{id}/resize", post(resize))
        .route("/exec/{id}/resize", post(resize))
        .route("/exec/{id}/start", post(lifecycle::exec_start))
        .route("/exec/{id}/json", get(exec_inspect))
        .route("/images/json", get(images_list))
        .route("/images/create", post(images_create))
        .route("/images/{name}", delete(images_delete))
        .route("/containers/{id}/restart", post(extra::containers_restart))
        .route("/containers/{id}/kill", post(extra::containers_kill))
        .route("/containers/{id}/pause", post(extra::containers_pause))
        .route("/containers/{id}/unpause", post(extra::containers_unpause))
        .route("/containers/{id}/top", get(extra::containers_top))
        .route("/containers/{id}/stats", get(extra::containers_stats))
        .route("/containers/{id}/rename", post(extra::containers_rename))
        .route("/events", get(extra::events_route))
        .route("/networks", get(extra::networks_list))
        .route("/networks/create", post(extra::networks_create))
        .route("/networks/{id}", get(extra::networks_inspect))
        .route("/volumes", get(extra::volumes_list))
        .route("/volumes/create", post(extra::volumes_create))
        .route(
            "/volumes/{name}",
            get(extra::volume_missing).delete(extra::volume_missing),
        )
        .route(
            "/build",
            post(build::build).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/grpc", post(build::buildkit))
        .route("/session", post(build::buildkit))
        .fallback(extra::fallback)
        .method_not_allowed_fallback(extra::fallback)
        .with_state(state);
    // The version prefix (`/v1.43/...`) must be removed before routing, so the middleware wraps the whole router.
    use tower_layer::Layer as _;
    Router::new().fallback_service(middleware::from_fn(strip_api_version).layer(routes))
}

/// Bind `{path}` and serve the Docker shim until the process exits.
pub async fn serve_unix(
    path: PathBuf,
    manager: Arc<VmManager>,
    build: Arc<crate::docker_build::BuildEngine>,
) -> anyhow::Result<()> {
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
    let state = DockerState::new(manager, build);
    lifecycle::resume_auto_remove(state.clone()).await;
    axum::serve(listener, routes(state)).await?;
    Ok(())
}

/// Spawn [`serve_unix`] when `cfg.docker_sock_path()` is set.
pub fn spawn_if_configured(manager: Arc<VmManager>, cfg: &fluxvm_core::config::Config) {
    let Some(path) = cfg.docker_sock_path() else {
        return;
    };
    // `docker build` goes to `fluxctl mac build` against this daemon: its address and an admin token travel in the child's
    // environment, never on a command line.
    let url = {
        let scheme = if cfg.tls.enabled() { "https" } else { "http" };
        let listen = cfg
            .listen
            .replace("0.0.0.0", "127.0.0.1")
            .replace("[::]", "[::1]");
        (!listen.is_empty()).then(|| format!("{scheme}://{listen}"))
    };
    let token = cfg
        .auth
        .tokens
        .iter()
        .find(|t| t.role == fluxvm_core::config::Role::Admin)
        .map(|t| t.token.clone());
    let history = crate::docker_build::History::new(
        path.parent()
            .map(|d| d.join(crate::docker_build::HISTORY_FILE)),
    );
    let mut engine = crate::docker_build::BuildEngine::new(
        Arc::new(crate::docker_build::FluxctlBuilder::new(url, token)),
        Arc::new(history),
    );
    engine.socket = Some(path.clone());
    let engine = Arc::new(engine);
    crate::docker_build::set_global(engine.clone());
    tokio::spawn(async move {
        if let Err(e) = serve_unix(path, manager, engine).await {
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
    h.insert("Api-Version", HeaderValue::from_static(API_VERSION));
    h.insert("Docker-Experimental", HeaderValue::from_static("false"));
    h.insert("Ostype", HeaderValue::from_static("linux"));
    h.insert("Server", HeaderValue::from_static("FluxVM"));
    // The classic builder: the docker CLI then sends a plain `POST /build` and never tries BuildKit.
    h.insert("Builder-Version", HeaderValue::from_static("1"));
    h
}

async fn resize() -> StatusCode {
    StatusCode::OK
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
    let containers = containers_with_state(&st.manager).await;
    let images = st.manager.oci_list().unwrap_or_default().len();
    let running = containers
        .iter()
        .filter(|(_, s)| *s == ProcessState::Running)
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
        "ProductLicense": "LicenseRef-Zyvor-Production-2.0",
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
    all: Option<String>,
    limit: Option<i64>,
}

async fn containers_list(
    State(st): State<DockerState>,
    Query(q): Query<ListQuery>,
) -> DockerResult<Json<Value>> {
    let all = match q.all.as_deref() {
        None | Some("false" | "0" | "") => false,
        Some("true" | "1") => true,
        _ => return Err(DockerError::bad("all must be true, false, 1 or 0")),
    };
    let mut vms = containers_with_state(&st.manager).await;
    let limit = q.limit.filter(|n| *n > 0);
    if !all && limit.is_none() {
        vms.retain(|(_, state)| lifecycle::is_up(*state));
    }
    vms.sort_unstable_by_key(|(vm, _)| std::cmp::Reverse((vm.created_at, vm.id)));
    if let Some(n) = limit {
        vms.truncate(usize::try_from(n).unwrap_or(usize::MAX));
    }
    let items = vms
        .iter()
        .map(|(vm, state)| container_summary(vm, *state))
        .collect();
    Ok(Json(Value::Array(items)))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateQuery {
    name: Option<String>,
}

/// The docker CLI (v29) sends `null` for empty lists and maps (`"Env": null`); read that as the default.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A query flag as Docker reads it: absent, empty, `0`, `no`, `false` and `none` are false, anything else is true.
/// The CLI sends `1`.
fn docker_flag(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "no" | "false" | "none"
        )
    })
}

fn docker_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(docker_flag(Option::<String>::deserialize(d)?.as_deref()))
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
    #[serde(rename = "Env", deserialize_with = "null_default")]
    env: Vec<String>,
    #[serde(rename = "WorkingDir", deserialize_with = "null_default")]
    working_dir: String,
    #[serde(rename = "User", deserialize_with = "null_default")]
    user: String,
    #[serde(rename = "Labels", deserialize_with = "null_default")]
    labels: BTreeMap<String, String>,
    #[serde(rename = "HostConfig", deserialize_with = "null_default")]
    host_config: HostConfigBody,
    #[serde(rename = "Tty", deserialize_with = "null_default")]
    tty: bool,
    #[serde(rename = "ExposedPorts", deserialize_with = "null_default")]
    exposed_ports: BTreeMap<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HostConfigBody {
    #[serde(rename = "PortBindings", deserialize_with = "null_default")]
    port_bindings: BTreeMap<String, Option<Vec<PortBinding>>>,
    #[serde(rename = "Memory", deserialize_with = "null_default")]
    memory: i64,
    #[serde(rename = "NanoCpus", deserialize_with = "null_default")]
    nano_cpus: i64,
    #[serde(rename = "AutoRemove", deserialize_with = "null_default")]
    auto_remove: bool,
    #[serde(rename = "Mounts", deserialize_with = "null_default")]
    mounts: Vec<Value>,
    #[serde(rename = "Binds", deserialize_with = "null_default")]
    binds: Vec<String>,
    #[serde(rename = "RestartPolicy", deserialize_with = "null_default")]
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
    if !body.host_config.binds.is_empty() || !body.host_config.mounts.is_empty() {
        return Err(DockerError::bad(
            "bind mounts are not supported by the FluxVM Docker shim; use named volumes via fluxctl",
        ));
    }
    let ports = port_bindings_to_specs(&body.host_config.port_bindings)?;
    let expose = exposed_ports_to_u16(&body.exposed_ports)?;
    let entrypoint = parse_string_or_vec(body.entrypoint);
    let memory_mib = if body.host_config.memory > 0 {
        Some(
            (body.host_config.memory as u64)
                .div_ceil(1024 * 1024)
                .max(512),
        )
    } else {
        None
    };
    let vcpus = if body.host_config.nano_cpus > 0 {
        Some(((body.host_config.nano_cpus as u64).div_ceil(1_000_000_000) as u8).max(1))
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
        policy_override: false,
    };
    let _ = body.labels; // OCI create path sets its own labels; user labels via PATCH later
    let mut req: SandboxCreateRequest = serde_json::from_value(json!({}))
        .map_err(|e| DockerError::bad(format!("internal sandbox request defaults: {e}")))?;
    req.name = name;
    req.oci = Some(oci);
    req.vcpus = vcpus;
    req.memory_mib = memory_mib;
    Ok(req)
}

/// Map a Docker `ContainerConfig` JSON body to a sandbox request, exactly as `POST /containers/create` here does
/// (shared with [`crate::docker_adapter`] so both Docker surfaces agree).
pub(crate) fn sandbox_request_from_json(
    body: Value,
    name: Option<String>,
) -> DockerResult<SandboxCreateRequest> {
    let body: ContainerCreateBody = serde_json::from_value(body)
        .map_err(|e| DockerError::bad(format!("invalid container config: {e}")))?;
    map_create_request(body, name)
}

async fn containers_create(
    State(st): State<DockerState>,
    Query(q): Query<CreateQuery>,
    Json(body): Json<ContainerCreateBody>,
) -> DockerResult<impl IntoResponse> {
    if build::is_buildkit_image(&body.image) {
        return Err(build::buildkit_refusal());
    }
    if st.build.history.image_for(&body.image).is_some() {
        return Err(DockerError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "{} is a macOS image that `docker build` made through FluxVM, and the Docker API cannot start macOS images yet. \
                 Run it with `fluxctl mac run {}` (add `-d` to keep it running).",
                body.image, body.image
            ),
        ));
    }
    let name = q
        .name
        .map(|n| n.trim_start_matches('/').to_string())
        .filter(|n| !n.is_empty());
    if let Some(name) = &name
        && let Some(other) = oci_containers(&st.manager)
            .await
            .into_iter()
            .find(|v| &v.name == name)
    {
        return Err(DockerError::new(
            StatusCode::CONFLICT,
            format!(
                "Conflict. The container name \"/{name}\" is already in use by container \"{}\". You have to remove (or rename) that container to be able to reuse that name.",
                docker_id(other.id)
            ),
        ));
    }
    let mut labels = BTreeMap::new();
    if body.tty {
        labels.insert(lifecycle::TTY_LABEL.to_string(), Some("true".to_string()));
    }
    if body.host_config.auto_remove {
        labels.insert(
            lifecycle::AUTO_REMOVE_LABEL.to_string(),
            Some("true".to_string()),
        );
    }
    let mut req = map_create_request(body, name)?;
    if let Some(oci) = req.oci.as_mut() {
        // A Docker container ends when its process does.
        oci.exit_policy = ExitPolicy::Poweroff;
    }
    // `docker run` attaches and waits between create and start, so create must not run the process.
    let vm = st
        .manager
        .create_sandbox_unstarted(req, None, Some("docker-api"))
        .await?;
    if !labels.is_empty()
        && let Err(e) = st
            .manager
            .patch(vm.id, VmPatch { name: None, labels })
            .await
    {
        let _ = st.manager.delete(vm.id).await;
        return Err(e.into());
    }
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
    let state = process_state(&vm).await;
    Ok(Json(container_inspect(&vm, state)))
}

#[derive(Deserialize)]
struct StopQuery {
    t: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecCreateBody {
    #[serde(rename = "Cmd", deserialize_with = "null_default")]
    cmd: Vec<String>,
    #[serde(rename = "Env", deserialize_with = "null_default")]
    env: Vec<String>,
    #[serde(rename = "WorkingDir", deserialize_with = "null_default")]
    working_dir: String,
    #[serde(rename = "User", deserialize_with = "null_default")]
    user: String,
}

async fn containers_exec_create(
    State(st): State<DockerState>,
    Path(id): Path<String>,
    Json(body): Json<ExecCreateBody>,
) -> DockerResult<impl IntoResponse> {
    if body.cmd.is_empty() {
        return Err(DockerError::bad("Cmd is required"));
    }
    let vm = resolve_container(&st.manager, &id).await?;
    lifecycle::running_container(&st.manager, vm.id).await?;
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
            running: false,
            exit_code: None,
        },
    );
    Ok((StatusCode::CREATED, Json(json!({ "Id": exec_id }))))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ExecStartBody {
    #[serde(rename = "Detach", deserialize_with = "null_default")]
    detach: bool,
    #[serde(rename = "Tty", deserialize_with = "null_default")]
    tty: bool,
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
        "Running": session.running,
        "ExitCode": session.exit_code.unwrap_or(0),
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

async fn images_list(State(st): State<DockerState>) -> DockerResult<Json<Value>> {
    let entries = st.manager.oci_list()?;
    let mut items: Vec<Value> = entries.iter().map(image_summary).collect();
    // Images built through this socket (macOS images in FluxVM's store), so `docker images` shows what `docker build` made.
    for (id, tags, created) in st.build.history.images() {
        items.push(json!({
            "Id": id, "RepoTags": tags, "RepoDigests": null, "Created": created as i64,
            "Size": 0, "SharedSize": -1, "VirtualSize": 0,
            "Labels": {"fluxvm.image": "macos"}, "Containers": -1
        }));
    }
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
    let image = q
        .from_image
        .ok_or_else(|| DockerError::bad("fromImage is required"))?;
    if build::is_buildkit_image(&image) {
        return Err(build::buildkit_refusal());
    }
    let reference = match q.tag.as_deref() {
        Some(t) if !t.is_empty() && !image.contains(':') && !image.contains('@') => {
            format!("{image}:{t}")
        }
        _ => image,
    };
    let platform = q.platform.filter(|p| !p.is_empty());
    let entry = st.manager.oci_pull(&reference, platform.as_deref()).await?;
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

pub(crate) async fn oci_containers(m: &VmManager) -> Vec<VmRecord> {
    m.list()
        .await
        .into_iter()
        .filter(|v| v.labels.contains_key(OCI_IMAGE_LABEL))
        .collect()
}

/// Every container with what its process is doing now.
async fn containers_with_state(m: &VmManager) -> Vec<(VmRecord, ProcessState)> {
    let mut out = Vec::new();
    for vm in oci_containers(m).await {
        let state = process_state(&vm).await;
        out.push((vm, state));
    }
    out
}

pub(crate) async fn resolve_container(m: &VmManager, id_or_name: &str) -> DockerResult<VmRecord> {
    let all = oci_containers(m).await;
    if let Ok(u) = Uuid::parse_str(id_or_name) {
        return all
            .into_iter()
            .find(|v| v.id == u)
            .ok_or_else(|| DockerError::not_found(format!("No such container: {id_or_name}")));
    }
    // Docker resolves an exact name first and falls back to an ID prefix only when no name matches. Matching both at once
    // made a container named `c` clash with any other container whose random ID happened to start with `c`.
    let name_of = |v: &VmRecord| {
        v.name == id_or_name || v.name.trim_start_matches('/') == id_or_name.trim_start_matches('/')
    };
    if all.iter().filter(|v| name_of(v)).count() == 1 {
        return Ok(all.into_iter().find(|v| name_of(v)).unwrap());
    }
    let needle = id_or_name.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<VmRecord> = all
        .into_iter()
        .filter(|v| v.id.as_simple().to_string().starts_with(&needle) || name_of(v))
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

pub(crate) fn docker_id(id: Uuid) -> String {
    id.as_simple().to_string()
}

/// Docker's word for a state, and its `Status` column.
fn state_words(state: ProcessState) -> (&'static str, String) {
    match state {
        ProcessState::Created => ("created", "Created".into()),
        ProcessState::Running => ("running", "Up".into()),
        ProcessState::Paused => ("paused", "Up (paused)".into()),
        ProcessState::Exited(code) => ("exited", format!("Exited ({code})")),
    }
}

fn container_summary(vm: &VmRecord, process: ProcessState) -> Value {
    let image = vm
        .labels
        .get(OCI_IMAGE_LABEL)
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let (state, status) = state_words(process);
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
        "Status": status,
        "HostConfig": { "NetworkMode": "default" },
        "NetworkSettings": { "Networks": {} },
        "Mounts": []
    })
}

fn container_inspect(vm: &VmRecord, process: ProcessState) -> Value {
    let image = vm
        .labels
        .get(OCI_IMAGE_LABEL)
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let (state, _) = state_words(process);
    let running = lifecycle::is_up(process);
    let exit_code = match process {
        ProcessState::Exited(code) => code,
        _ => 0,
    };
    json!({
        "Id": docker_id(vm.id),
        "Created": vm.created_at.to_rfc3339(),
        "Path": "",
        "Args": [],
        "State": {
            "Status": state,
            "Running": running,
            "Paused": process == ProcessState::Paused,
            "Restarting": false,
            "OOMKilled": false,
            "Dead": false,
            "Pid": if running { vm.pid.unwrap_or(0) } else { 0 },
            "ExitCode": exit_code,
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
            "AutoRemove": vm.labels.contains_key(lifecycle::AUTO_REMOVE_LABEL),
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
            "Tty": vm.labels.contains_key(lifecycle::TTY_LABEL),
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

pub(crate) fn docker_state(status: &VmStatus) -> (&'static str, bool) {
    match status {
        VmStatus::Running => ("running", true),
        VmStatus::Paused => ("paused", true),
        VmStatus::Creating => ("created", false),
        VmStatus::Stopped | VmStatus::Failed => ("exited", false),
    }
}

pub(crate) fn docker_status_text(vm: &VmRecord) -> String {
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
        .filter(|r| !r.contains('@'))
        .cloned()
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

async fn run_exec(
    m: &Arc<VmManager>,
    session: &ExecSession,
) -> DockerResult<(String, String, i32)> {
    m.ensure_running_for_request(session.container).await?;
    let user = session.user.as_deref().map(exec_user).transpose()?;
    let process = ExecProcess {
        argv: session.cmd.clone(),
        env: session.env.clone(),
        clean_env: false,
        cwd: session.working_dir.clone(),
        user,
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
        AgentResponse::Error { message } => Err(DockerError::bad(message)),
        other => Err(DockerError::bad(format!(
            "unexpected agent response: {other:?}"
        ))),
    }
}

/// `docker exec -u`: `root` or a numeric `uid[:gid]`. A name would need the image's `/etc/passwd`; running the
/// command as another user than asked is worse than refusing.
fn exec_user(user: &str) -> DockerResult<ExecRunAs> {
    let (uid, gid) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let id = |s: &str| {
        if s == "root" {
            Some(0)
        } else {
            s.parse::<u32>().ok()
        }
    };
    let refuse = || {
        DockerError::bad(format!(
            "exec -u {user:?}: the built-in engine takes root or a numeric uid[:gid]"
        ))
    };
    let uid = id(uid).ok_or_else(refuse)?;
    let gid = match gid {
        Some(g) => id(g).ok_or_else(refuse)?,
        None => uid,
    };
    Ok(ExecRunAs { uid, gid })
}

mod build;
mod extra;
mod lifecycle;

/// A container record for tests outside this module (e.g. `docker_adapter`).
#[cfg(test)]
pub(crate) use extra::tests::fixture as vm_fixture;

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
    fn create_accepts_the_nulls_docker_29_sends() {
        let b: ContainerCreateBody = serde_json::from_value(json!({
            "Image": "alpine", "Cmd": null, "Env": null, "WorkingDir": null, "User": null,
            "Labels": null, "ExposedPorts": null,
            "HostConfig": {"Binds": null, "PortBindings": null, "Memory": 0}
        }))
        .unwrap();
        assert!(b.env.is_empty() && b.host_config.binds.is_empty());
        assert!(map_create_request(b, None).is_ok());
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
