// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! HTTP surface: Docker request/response shapes over a [`DockerAdapter`].

use crate::{
    AdapterError, Container, ContainerDetails, CreateRequest, DockerAdapter, ErrorKind, LogChunk,
    LogsRequest, Transition,
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Reported API version. 1.44 is the oldest the current docker CLI accepts.
pub const API_VERSION: &str = "1.44";
/// Oldest API version clients may negotiate down to.
pub const MIN_API_VERSION: &str = "1.24";
const SERVER: &str = "FluxVM";
const ENGINE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-fluxvm");
const SUPPORTED: &str = "GET /_ping, GET /version, GET /containers/json, POST /containers/create, \
    GET /containers/{id}/json, POST /containers/{id}/start, POST /containers/{id}/stop, GET /containers/{id}/logs";

type AppState = Arc<dyn DockerAdapter>;

impl IntoResponse for AdapterError {
    fn into_response(self) -> Response {
        let status = match self.kind {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::NotImplemented => StatusCode::NOT_IMPLEMENTED,
        };
        (status, Json(json!({ "message": self.message }))).into_response()
    }
}

type ApiResult<T> = Result<T, AdapterError>;

/// The Docker Engine API router over `adapter`. No authentication: serve it only on a `0600` unix socket or
/// loopback (see [`crate::serve`]).
pub fn router(adapter: Arc<dyn DockerAdapter>) -> Router {
    let routes = Router::new()
        .route("/_ping", get(ping).head(ping))
        .route("/version", get(version))
        .route("/containers/json", get(containers_list))
        .route("/containers/create", post(containers_create))
        .route("/containers/{id}/json", get(containers_inspect))
        .route("/containers/{id}/start", post(containers_start))
        .route("/containers/{id}/stop", post(containers_stop))
        .route("/containers/{id}/logs", get(containers_logs))
        .fallback(not_implemented)
        .method_not_allowed_fallback(not_implemented)
        .with_state(adapter)
        .layer(middleware::map_response(docker_headers));
    // The `/v1.NN` prefix must go before routing, so the middleware wraps the whole router.
    use tower_layer::Layer as _;
    Router::new().fallback_service(middleware::from_fn(strip_api_version).layer(routes))
}

async fn strip_api_version(mut req: Request, next: Next) -> Response {
    if let Some(stripped) = strip_version_prefix(req.uri().path()) {
        let pq = match req.uri().query() {
            Some(q) => format!("{stripped}?{q}"),
            None => stripped,
        };
        if let Ok(pq) = pq.parse::<axum::http::uri::PathAndQuery>() {
            let mut parts = req.uri().clone().into_parts();
            parts.path_and_query = Some(pq);
            if let Ok(uri) = Uri::from_parts(parts) {
                *req.uri_mut() = uri;
            }
        }
    }
    next.run(req).await
}

/// `/v1.44/containers/json` → `/containers/json`; `None` when there is no version prefix.
fn strip_version_prefix(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/v")?;
    let (ver, after) = rest.split_once('/')?;
    let (major, minor) = ver.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(major) && digits(minor)).then(|| format!("/{after}"))
}

async fn docker_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert("Api-Version", HeaderValue::from_static(API_VERSION));
    h.insert("Docker-Experimental", HeaderValue::from_static("false"));
    h.insert("Ostype", HeaderValue::from_static("linux"));
    h.insert(header::SERVER, HeaderValue::from_static(SERVER));
    res
}

async fn ping() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        "OK",
    )
}

async fn version() -> Json<Value> {
    Json(json!({
        "Platform": { "Name": SERVER },
        "Components": [{
            "Name": "Engine",
            "Version": ENGINE_VERSION,
            "Details": { "ApiVersion": API_VERSION, "MinAPIVersion": MIN_API_VERSION, "Os": "linux" }
        }],
        "Version": ENGINE_VERSION,
        "ApiVersion": API_VERSION,
        "MinAPIVersion": MIN_API_VERSION,
        "GitCommit": "",
        "GoVersion": "",
        "Os": "linux",
        "Arch": docker_arch(),
        "KernelVersion": SERVER,
        "BuildTime": ""
    }))
}

fn docker_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

async fn not_implemented(method: Method, uri: Uri) -> AdapterError {
    AdapterError::not_implemented(format!(
        "{method} {} is not implemented by the FluxVM Docker API adapter. Supported: {SUPPORTED}. \
         See docs/DOCKER-API.md for the full Docker socket and `fluxctl docker`.",
        uri.path()
    ))
}

/// Docker's boolean query values: `1`/`true`/`0`/`false` (empty is false).
fn query_bool(name: &str, v: Option<&str>) -> ApiResult<bool> {
    match v {
        None | Some("" | "0" | "false" | "False" | "FALSE") => Ok(false),
        Some("1" | "true" | "True" | "TRUE") => Ok(true),
        Some(other) => Err(AdapterError::bad_request(format!(
            "invalid value for {name}: {other:?} (want 1, 0, true or false)"
        ))),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ListQuery {
    all: Option<String>,
    limit: Option<i64>,
    size: Option<String>,
    filters: Option<String>,
}

async fn containers_list(
    State(adapter): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Container>>> {
    let all = query_bool("all", q.all.as_deref())?;
    let _ = query_bool("size", q.size.as_deref())?; // sizes are not reported
    if let Some(f) = q.filters.as_deref().map(str::trim)
        && !f.is_empty()
        && f != "{}"
    {
        return Err(AdapterError::not_implemented(
            "container list filters are not supported by the FluxVM Docker API adapter yet",
        ));
    }
    // Like Docker, a positive `limit` returns the newest N containers in any state.
    let limit = q
        .limit
        .filter(|n| *n > 0)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let mut items = adapter.list(all || limit.is_some()).await?;
    if !all && limit.is_none() {
        items.retain(Container::is_up);
    }
    if let Some(n) = limit {
        items.truncate(n);
    }
    Ok(Json(items))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateQuery {
    name: Option<String>,
    platform: Option<String>,
}

/// Docker's container name rule: `/?[a-zA-Z0-9][a-zA-Z0-9_.-]+`.
fn valid_container_name(name: &str) -> bool {
    let name = name.strip_prefix('/').unwrap_or(name);
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && name.len() >= 2
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

async fn containers_create(
    State(adapter): State<AppState>,
    Query(q): Query<CreateQuery>,
    body: Bytes,
) -> ApiResult<Response> {
    let req: CreateRequest = serde_json::from_slice(&body)
        .map_err(|e| AdapterError::bad_request(format!("invalid container config: {e}")))?;
    if req.image.trim().is_empty() {
        return Err(AdapterError::bad_request("config: Image is required"));
    }
    if q.platform.as_deref().is_some_and(|p| !p.is_empty()) {
        return Err(AdapterError::not_implemented(
            "the platform parameter is not supported by the FluxVM Docker API adapter",
        ));
    }
    let name = match q.name.filter(|n| !n.is_empty()) {
        Some(n) if !valid_container_name(&n) => {
            return Err(AdapterError::bad_request(format!(
                "Invalid container name ({n}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
            )));
        }
        Some(n) => Some(n.trim_start_matches('/').to_owned()),
        None => None,
    };
    let id = adapter.create(name, req).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "Id": id, "Warnings": [] })),
    )
        .into_response())
}

async fn containers_inspect(
    State(adapter): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let d = adapter.inspect(id).await?;
    Ok(Json(inspect_json(&d)))
}

fn inspect_json(d: &ContainerDetails) -> Value {
    let c = &d.summary;
    let name = c.names.first().cloned().unwrap_or_default();
    json!({
        "Id": c.id,
        "Name": name,
        "Image": c.image_id,
        "Created": c.created,
        "State": {
            "Status": c.state,
            "Running": c.state == "running" || c.state == "paused",
            "Paused": c.state == "paused",
            "Restarting": false,
            "OOMKilled": false,
            "Dead": false,
            "ExitCode": d.exit_code.unwrap_or(0),
            "Error": ""
        },
        "Config": {
            "Image": c.image,
            "Cmd": d.cmd,
            "Tty": d.tty,
            "Labels": c.labels,
            "AttachStdout": true,
            "AttachStderr": true
        },
        "HostConfig": { "NetworkMode": "default" },
        "Mounts": []
    })
}

fn transition_status(t: Transition) -> StatusCode {
    match t {
        Transition::Changed => StatusCode::NO_CONTENT,
        Transition::Unchanged => StatusCode::NOT_MODIFIED,
    }
}

async fn containers_start(
    State(adapter): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    Ok(transition_status(adapter.start(id).await?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StopQuery {
    t: Option<i64>,
    signal: Option<String>,
}

async fn containers_stop(
    State(adapter): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StopQuery>,
) -> ApiResult<StatusCode> {
    if q.signal.as_deref().is_some_and(|s| !s.is_empty()) {
        return Err(AdapterError::not_implemented(
            "stop signals are not supported by the FluxVM Docker API adapter; the guest is shut down cleanly",
        ));
    }
    // Docker reads a negative `t` as "wait forever".
    let timeout = q.t.and_then(|t| u64::try_from(t).ok());
    Ok(transition_status(adapter.stop(id, timeout).await?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsQuery {
    stdout: Option<String>,
    stderr: Option<String>,
    follow: Option<String>,
    timestamps: Option<String>,
    tail: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

fn parse_tail(tail: Option<&str>) -> ApiResult<Option<usize>> {
    match tail.map(str::trim) {
        None | Some("" | "all") => Ok(None),
        Some(n) => match n.parse::<i64>() {
            Ok(n) if n < 0 => Ok(None),
            Ok(n) => Ok(Some(usize::try_from(n).unwrap_or(usize::MAX))),
            Err(_) => Err(AdapterError::bad_request(format!(
                "invalid tail {n:?} (want a number or \"all\")"
            ))),
        },
    }
}

/// Docker's multiplexed-stream frame: `[stream, 0, 0, 0, len_be32] payload`.
pub(crate) fn mux_frame(chunk: &LogChunk) -> Bytes {
    let mut out = Vec::with_capacity(8 + chunk.data.len());
    out.push(chunk.stream as u8);
    out.extend_from_slice(&[0, 0, 0]);
    let len = u32::try_from(chunk.data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk.data[..len as usize]);
    Bytes::from(out)
}

async fn containers_logs(
    State(adapter): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult<Response> {
    let req = LogsRequest {
        stdout: query_bool("stdout", q.stdout.as_deref())?,
        stderr: query_bool("stderr", q.stderr.as_deref())?,
        follow: query_bool("follow", q.follow.as_deref())?,
        tail: parse_tail(q.tail.as_deref())?,
    };
    if !req.stdout && !req.stderr {
        return Err(AdapterError::bad_request(
            "Bad parameters: you must choose at least one stream",
        ));
    }
    if query_bool("timestamps", q.timestamps.as_deref())? {
        return Err(AdapterError::not_implemented(
            "log timestamps are not recorded by FluxVM; drop --timestamps",
        ));
    }
    let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty() && s != "0");
    if set(&q.since) || set(&q.until) {
        return Err(AdapterError::not_implemented(
            "log since/until are not supported by the FluxVM Docker API adapter; use --tail",
        ));
    }
    let logs = adapter.logs(id, req).await?;
    let tty = logs.tty;
    let body = logs.chunks.filter_map(move |chunk| async move {
        match chunk {
            Ok(c) if c.data.is_empty() => None,
            Ok(c) => {
                let wanted = match c.stream {
                    crate::LogStream::Stdout => req.stdout,
                    crate::LogStream::Stderr => req.stderr,
                };
                // With a TTY Docker sends the raw bytes, otherwise one frame per chunk.
                wanted.then(|| Ok::<_, AdapterError>(if tty { c.data } else { mux_frame(&c) }))
            }
            Err(e) => Some(Err(e)),
        }
    });
    let content_type = if tty {
        "application/vnd.docker.raw-stream"
    } else {
        "application/vnd.docker.multiplexed-stream"
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(body))
        .expect("static response parts"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LogStream, StubAdapter};
    use axum::body::to_bytes;
    use tower::ServiceExt as _;

    fn app(stub: &Arc<StubAdapter>) -> Router {
        router(stub.clone())
    }

    async fn call(
        app: Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Response) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = app.oneshot(req.body(body).unwrap()).await.unwrap();
        (res.status(), res)
    }

    async fn body_bytes(res: Response) -> Vec<u8> {
        to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    async fn body_json(res: Response) -> Value {
        serde_json::from_slice(&body_bytes(res).await).unwrap()
    }

    /// Decode a multiplexed stream into `(stream, payload)` frames.
    fn demux(mut b: &[u8]) -> Vec<(u8, String)> {
        let mut out = Vec::new();
        while b.len() >= 8 {
            let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
            out.push((b[0], String::from_utf8(b[8..8 + len].to_vec()).unwrap()));
            b = &b[8 + len..];
        }
        assert!(b.is_empty(), "trailing bytes after the last frame");
        out
    }

    #[test]
    fn version_prefix_is_stripped_only_for_versions() {
        assert_eq!(
            strip_version_prefix("/v1.44/containers/json").as_deref(),
            Some("/containers/json")
        );
        assert_eq!(
            strip_version_prefix("/v1.24/_ping").as_deref(),
            Some("/_ping")
        );
        assert_eq!(strip_version_prefix("/containers/json"), None);
        assert_eq!(strip_version_prefix("/volumes/x"), None);
        assert_eq!(strip_version_prefix("/v1/containers"), None);
        assert_eq!(strip_version_prefix("/vx.1/containers"), None);
    }

    #[test]
    fn container_names_follow_docker_rules() {
        assert!(valid_container_name("web"));
        assert!(valid_container_name("/web-1.a_b"));
        assert!(!valid_container_name("w"));
        assert!(!valid_container_name("-web"));
        assert!(!valid_container_name("we b"));
        assert!(!valid_container_name("we/b"));
    }

    #[tokio::test]
    async fn ping_and_version_handshake_with_and_without_prefix() {
        let stub = Arc::new(StubAdapter::default());
        for uri in ["/_ping", "/v1.44/_ping"] {
            let (status, res) = call(app(&stub), "GET", uri, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(res.headers()["Api-Version"], API_VERSION);
            assert_eq!(res.headers()["Ostype"], "linux");
            assert_eq!(body_bytes(res).await, b"OK");
        }
        let (status, _) = call(app(&stub), "HEAD", "/_ping", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, res) = call(app(&stub), "GET", "/v1.41/version", None).await;
        assert_eq!(status, StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["ApiVersion"], API_VERSION);
        assert_eq!(v["MinAPIVersion"], MIN_API_VERSION);
        assert_eq!(v["Os"], "linux");
    }

    #[tokio::test]
    async fn create_start_list_stop_round_trip() {
        let stub = Arc::new(StubAdapter::default());
        let (status, res) = call(
            app(&stub),
            "POST",
            "/v1.44/containers/create?name=web",
            Some(json!({ "Image": "nginx:alpine", "Cmd": ["nginx", "-g", "daemon off;"], "Env": null })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let created = body_json(res).await;
        let id = created["Id"].as_str().unwrap().to_owned();
        assert_eq!(created["Warnings"], json!([]));
        let req = stub.last_create().unwrap();
        assert_eq!(req.0.as_deref(), Some("web"));
        assert_eq!(req.1.image, "nginx:alpine");
        assert_eq!(
            req.1.cmd.as_deref(),
            Some(&["nginx".to_owned(), "-g".into(), "daemon off;".into()][..])
        );
        assert_eq!(req.1.env, None);

        // Created but not started: `docker ps` hides it, `docker ps -a` shows it.
        let (_, res) = call(app(&stub), "GET", "/containers/json", None).await;
        assert_eq!(body_json(res).await, json!([]));
        let (_, res) = call(app(&stub), "GET", "/containers/json?all=1", None).await;
        let list = body_json(res).await;
        assert_eq!(list[0]["Id"], id);
        assert_eq!(list[0]["Names"], json!(["/web"]));
        assert_eq!(list[0]["Image"], "nginx:alpine");
        assert_eq!(list[0]["State"], "created");

        let (status, _) = call(app(&stub), "POST", "/containers/web/start", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            app(&stub),
            "POST",
            &format!("/containers/{}/start", &id[..6]),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_MODIFIED,
            "starting a running container is a 304"
        );
        let (_, res) = call(app(&stub), "GET", "/containers/json", None).await;
        let list = body_json(res).await;
        assert_eq!(list[0]["State"], "running");
        assert_eq!(list[0]["Status"], "Up");

        let (status, _) = call(app(&stub), "POST", "/containers/web/stop?t=5", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(stub.last_stop_timeout(), Some(5));
        let (status, _) = call(app(&stub), "POST", "/containers/web/stop", None).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        let (_, res) = call(app(&stub), "GET", "/containers/json", None).await;
        assert_eq!(body_json(res).await, json!([]));
    }

    #[tokio::test]
    async fn list_limit_returns_newest_in_any_state() {
        let stub = Arc::new(StubAdapter::default());
        for name in ["a1", "b2", "c3"] {
            let (s, _) = call(
                app(&stub),
                "POST",
                &format!("/containers/create?name={name}"),
                Some(json!({ "Image": "alpine" })),
            )
            .await;
            assert_eq!(s, StatusCode::CREATED);
        }
        let (_, res) = call(app(&stub), "GET", "/containers/json?limit=2", None).await;
        let list = body_json(res).await;
        let names: Vec<_> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["Names"][0].clone())
            .collect();
        assert_eq!(names, vec![json!("/c3"), json!("/b2")]);
        let (s, res) = call(app(&stub), "GET", "/containers/json?all=maybe", None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(
            body_json(res).await["message"]
                .as_str()
                .unwrap()
                .contains("all")
        );
        let (s, _) = call(app(&stub), "GET", "/containers/json?filters=%7B%7D", None).await;
        assert_eq!(s, StatusCode::OK, "empty filters are no filters");
        let (s, _) = call(
            app(&stub),
            "GET",
            "/containers/json?filters=%7B%22status%22%3A%5B%22exited%22%5D%7D",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn create_errors_are_docker_shaped() {
        let stub = Arc::new(StubAdapter::default());
        let (s, res) = call(
            app(&stub),
            "POST",
            "/containers/create",
            Some(json!({ "Cmd": ["true"] })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["message"], "config: Image is required");

        let (s, _) = call(
            app(&stub),
            "POST",
            "/containers/create?name=-bad",
            Some(json!({ "Image": "a" })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let (s, _) = call(
            app(&stub),
            "POST",
            "/containers/create?name=dup",
            Some(json!({ "Image": "a" })),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        let (s, res) = call(
            app(&stub),
            "POST",
            "/containers/create?name=/dup",
            Some(json!({ "Image": "a" })),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(
            body_json(res).await["message"]
                .as_str()
                .unwrap()
                .contains("already in use")
        );

        let req = Request::builder()
            .method("POST")
            .uri("/containers/create")
            .body(Body::from("{not json"))
            .unwrap();
        let res = app(&stub).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(
            body_json(res).await["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid container config")
        );
    }

    #[tokio::test]
    async fn unknown_container_is_404() {
        let stub = Arc::new(StubAdapter::default());
        for (m, uri) in [
            ("POST", "/containers/nope/start"),
            ("POST", "/containers/nope/stop"),
            ("GET", "/containers/nope/json"),
            ("GET", "/containers/nope/logs?stdout=1"),
        ] {
            let (s, res) = call(app(&stub), m, uri, None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{m} {uri}");
            assert_eq!(body_json(res).await["message"], "No such container: nope");
        }
    }

    #[tokio::test]
    async fn unimplemented_endpoints_answer_501_with_a_message() {
        let stub = Arc::new(StubAdapter::default());
        for (m, uri) in [
            ("GET", "/info"),
            ("GET", "/v1.44/images/json"),
            ("DELETE", "/containers/x"),
            ("POST", "/containers/x/kill"),
            ("PUT", "/containers/json"),
        ] {
            let (s, res) = call(app(&stub), m, uri, None).await;
            assert_eq!(s, StatusCode::NOT_IMPLEMENTED, "{m} {uri}");
            assert_eq!(res.headers()["Api-Version"], API_VERSION);
            let msg = body_json(res).await["message"].as_str().unwrap().to_owned();
            assert!(msg.contains("not implemented"), "{msg}");
            assert!(
                !msg.contains("/v1.44"),
                "the message names the unprefixed path: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn logs_are_multiplexed_and_filtered_by_stream() {
        let stub = Arc::new(StubAdapter::default());
        let (_, res) = call(
            app(&stub),
            "POST",
            "/containers/create?name=app",
            Some(json!({ "Image": "a" })),
        )
        .await;
        let id = body_json(res).await["Id"].as_str().unwrap().to_owned();
        stub.push_log(&id, LogStream::Stdout, "one\n");
        stub.push_log(&id, LogStream::Stderr, "oops\n");
        stub.push_log(&id, LogStream::Stdout, "two\n");

        let (s, res) = call(
            app(&stub),
            "GET",
            "/v1.44/containers/app/logs?stdout=1&stderr=1",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "application/vnd.docker.multiplexed-stream"
        );
        assert_eq!(
            demux(&body_bytes(res).await),
            vec![
                (1, "one\n".into()),
                (2, "oops\n".into()),
                (1, "two\n".into())
            ]
        );

        let (_, res) = call(app(&stub), "GET", "/containers/app/logs?stderr=true", None).await;
        assert_eq!(demux(&body_bytes(res).await), vec![(2, "oops\n".into())]);

        let (_, res) = call(
            app(&stub),
            "GET",
            "/containers/app/logs?stdout=1&stderr=1&tail=1",
            None,
        )
        .await;
        assert_eq!(demux(&body_bytes(res).await), vec![(1, "two\n".into())]);
        let (_, res) = call(
            app(&stub),
            "GET",
            "/containers/app/logs?stdout=1&tail=0",
            None,
        )
        .await;
        assert!(body_bytes(res).await.is_empty());
        let (_, res) = call(
            app(&stub),
            "GET",
            "/containers/app/logs?stdout=1&tail=all&follow=1",
            None,
        )
        .await;
        assert_eq!(demux(&body_bytes(res).await).len(), 2);
        assert_eq!(stub.last_logs_request().map(|r| r.follow), Some(true));
    }

    #[tokio::test]
    async fn tty_logs_are_raw() {
        let stub = Arc::new(StubAdapter::default());
        let (_, res) = call(
            app(&stub),
            "POST",
            "/containers/create?name=tty",
            Some(json!({ "Image": "a", "Tty": true })),
        )
        .await;
        let id = body_json(res).await["Id"].as_str().unwrap().to_owned();
        stub.push_log(&id, LogStream::Stdout, "hello\r\n");
        let (_, res) = call(app(&stub), "GET", "/containers/tty/json", None).await;
        assert_eq!(body_json(res).await["Config"]["Tty"], true);
        let (_, res) = call(app(&stub), "GET", "/containers/tty/logs?stdout=1", None).await;
        assert_eq!(
            res.headers()[header::CONTENT_TYPE],
            "application/vnd.docker.raw-stream"
        );
        assert_eq!(body_bytes(res).await, b"hello\r\n");
    }

    #[tokio::test]
    async fn logs_parameter_errors() {
        let stub = Arc::new(StubAdapter::default());
        call(
            app(&stub),
            "POST",
            "/containers/create?name=app",
            Some(json!({ "Image": "a" })),
        )
        .await;
        for (uri, want) in [
            ("/containers/app/logs", StatusCode::BAD_REQUEST),
            (
                "/containers/app/logs?stdout=0&stderr=0",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/containers/app/logs?stdout=1&tail=ten",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/containers/app/logs?stdout=1&timestamps=1",
                StatusCode::NOT_IMPLEMENTED,
            ),
            (
                "/containers/app/logs?stdout=1&since=1700000000",
                StatusCode::NOT_IMPLEMENTED,
            ),
        ] {
            let (s, res) = call(app(&stub), "GET", uri, None).await;
            assert_eq!(s, want, "{uri}");
            assert!(body_json(res).await["message"].is_string());
        }
        let (s, _) = call(
            app(&stub),
            "GET",
            "/containers/app/logs?stdout=1&since=0",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "since=0 is the docker CLI default");
    }

    #[tokio::test]
    async fn stop_with_signal_is_501() {
        let stub = Arc::new(StubAdapter::default());
        call(
            app(&stub),
            "POST",
            "/containers/create?name=app",
            Some(json!({ "Image": "a" })),
        )
        .await;
        let (s, _) = call(
            app(&stub),
            "POST",
            "/containers/app/stop?signal=SIGINT",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn mux_frame_header() {
        let f = mux_frame(&LogChunk {
            stream: LogStream::Stderr,
            data: Bytes::from_static(b"hi"),
        });
        assert_eq!(&f[..], &[2, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
    }
}
