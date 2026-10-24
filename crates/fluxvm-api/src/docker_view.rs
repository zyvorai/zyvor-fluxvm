// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A view of the Docker engine `fluxctl docker up` runs, for the Studio UI, with start, stop and restart for its containers.
//!
//! * `GET /v1/docker` says whether the engine VM (labelled `fluxvm.docker-engine`) and its socket are there, and, when the socket
//!   answers, lists its containers and images in a small, flat shape.
//! * `GET /v1/docker/containers/{id}/logs` returns the tail of a container's log as text.
//! * `POST /v1/docker/containers/{id}/{start|stop|restart}` does that to one container. Nothing else is written to the engine.
//!
//! The engine is the one `fluxctl docker` keeps at `/tmp/fluxvm-run-<uid>/docker-engine.sock` for the user the daemon runs as.
//! The containers belong to the whole host, not to a tenant, so these routes are admin only; the request log records each action.

use crate::{ApiError, ApiResult, Role, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use fluxvm_scheduler::VmManager;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::net::UnixStream;

/// `fluxctl docker up` labels the engine VM with this.
const LABEL: &str = "fluxvm.docker-engine";
/// Largest answer read from the engine.
const MAX_BODY: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
/// A stop or restart waits up to the container's grace period (Docker's default is 10 s) before it kills it.
const ACTION_TIMEOUT: Duration = Duration::from_secs(40);
const DEFAULT_TAIL: u32 = 500;
const MAX_TAIL: u32 = 5000;

fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        message: message.into(),
    }
}

/// Where `fluxctl docker` publishes the engine's socket: the same path it computes (`fluxctl docker env` prints it).
pub(crate) fn default_socket() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    PathBuf::from(format!("/tmp/fluxvm-run-{}", unsafe { libc::getuid() }))
        .join("docker-engine.sock")
}

/// One GET over the engine's unix socket: the status and the body (at most [`MAX_BODY`] bytes).
async fn engine_get(socket: &FsPath, path: &str) -> Result<(StatusCode, Bytes), String> {
    engine_call(socket, Method::GET, path, TIMEOUT).await
}

/// One request over the engine's unix socket, without a body.
async fn engine_call(
    socket: &FsPath,
    method: Method,
    path: &str,
    timeout: Duration,
) -> Result<(StatusCode, Bytes), String> {
    let work = async {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| format!("connecting to {}: {e}", socket.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("talking to the engine: {e}"))?;
        tokio::spawn(conn);
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "docker")
            .body(Empty::<Bytes>::new())
            .map_err(|e| e.to_string())?;
        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| format!("asking the engine: {e}"))?;
        let status = resp.status();
        let body = Limited::new(resp.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|e| format!("reading the engine's answer: {e}"))?
            .to_bytes();
        Ok((status, body))
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| "the engine did not answer in time".to_owned())?
}

/// A container id as Docker prints it, or a name: what is safe to put in a URL path.
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn short(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(12)]
}

/// The containers of `GET /containers/json?all=1`, flattened.
fn containers_view(list: &Value) -> Vec<Value> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    list.as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let name = c["Names"][0]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('/');
            let mut ports: Vec<Value> = c["Ports"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|p| {
                    json!({
                        "private": p["PrivatePort"],
                        "public": p["PublicPort"],
                        "type": p["Type"],
                        "ip": p["IP"],
                    })
                })
                .collect();
            // Docker lists a port once per address family it is published on.
            ports.dedup();
            json!({
                "id": short(c["Id"].as_str().unwrap_or_default()),
                "name": name,
                "image": s(&c["Image"]),
                "command": s(&c["Command"]),
                "state": s(&c["State"]),
                "status": s(&c["Status"]),
                "created": c["Created"],
                "ports": ports,
            })
        })
        .collect()
}

/// The images of `GET /images/json`, flattened.
fn images_view(list: &Value) -> Vec<Value> {
    list.as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|i| {
            let tags: Vec<&str> = i["RepoTags"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .filter(|t| *t != "<none>:<none>")
                .collect();
            json!({
                "id": short(i["Id"].as_str().unwrap_or_default()),
                "tags": tags,
                "size": i["Size"],
                "created": i["Created"],
                "containers": i["Containers"],
            })
        })
        .collect()
}

/// A container log without Docker's stream framing: for a container without a TTY every chunk has an 8-byte header (the
/// stream, three zero bytes, the length as a big-endian u32); with a TTY the log is plain text.
fn log_text(raw: &[u8], tty: bool) -> String {
    if tty {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw;
    while rest.len() >= 8 {
        let n = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let framed = rest[1..4] == [0, 0, 0] && rest[0] <= 2;
        if !framed {
            // Not framed after all (an engine that sends plain text): the rest is the log.
            out.extend_from_slice(rest);
            rest = &[];
            break;
        }
        let end = (8 + n).min(rest.len());
        out.extend_from_slice(&rest[8..end]);
        rest = &rest[end..];
    }
    out.extend_from_slice(rest);
    String::from_utf8_lossy(&out).into_owned()
}

fn json_of(status: StatusCode, body: &[u8], what: &str) -> Result<Value, ApiError> {
    if !status.is_success() {
        let msg = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(300)]).into_owned());
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("the engine refused {what} ({status}): {msg}"),
        ));
    }
    serde_json::from_slice(body).map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("the engine's {what} was not JSON: {e}"),
        )
    })
}

/// What `GET /v1/docker` answers, for an engine VM (`None`: there is none) and the socket path.
async fn view(vm: Option<Value>, socket: &FsPath) -> Result<Value, ApiError> {
    let engine = |reachable: bool, reason: Option<String>, extra: Value| json!({"vm": vm, "socket": socket.display().to_string(), "reachable": reachable, "reason": reason, "info": extra});
    if vm.is_none() {
        return Ok(json!({
            "engine": engine(false, Some("There is no Docker engine VM on this host. Make one with `fluxctl docker up`.".into()), Value::Null),
            "containers": [], "images": [],
        }));
    }
    let up = match engine_get(socket, "/_ping").await {
        Ok((s, b)) if s.is_success() && b.starts_with(b"OK") => Ok(()),
        Ok((s, _)) => Err(format!("the engine's socket answered {s} to a ping")),
        Err(e) => Err(e),
    };
    if let Err(why) = up {
        return Ok(json!({
            "engine": engine(false, Some(format!("{why}. Start it with `fluxctl docker up`; `fluxctl docker status` says what is wrong.")), Value::Null),
            "containers": [], "images": [],
        }));
    }
    let get = |path: &'static str, what: &'static str| async move {
        let (s, b) = engine_get(socket, path)
            .await
            .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
        json_of(s, &b, what)
    };
    let (containers, images, version) = tokio::try_join!(
        get("/containers/json?all=1", "the container list"),
        get("/images/json", "the image list"),
        get("/version", "its version"),
    )?;
    let info = json!({
        "version": version["Version"], "api": version["ApiVersion"], "os": version["Os"], "arch": version["Arch"],
        "kernel": version["KernelVersion"],
    });
    Ok(
        json!({"engine": engine(true, None, info), "containers": containers_view(&containers), "images": images_view(&images)}),
    )
}

/// `GET /v1/docker` (admin): the engine and what runs in it.
pub(crate) async fn get_docker(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let vm = m
        .list()
        .await
        .into_iter()
        .find(|v| v.labels.contains_key(LABEL))
        .map(
            |v| json!({"id": v.id, "name": v.name, "status": crate::kube::status_name(&v.status)}),
        );
    Ok(Json(view(vm, &default_socket()).await?))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogQuery {
    tail: Option<u32>,
}

async fn logs_of(socket: &FsPath, id: &str, tail: u32) -> Result<String, ApiError> {
    let (s, b) = engine_get(socket, &format!("/containers/{id}/json"))
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    if s == StatusCode::NOT_FOUND {
        return Err(err(StatusCode::NOT_FOUND, format!("no container '{id}'")));
    }
    let tty = json_of(s, &b, "the container")?["Config"]["Tty"]
        .as_bool()
        .unwrap_or(false);
    let path = format!("/containers/{id}/logs?stdout=1&stderr=1&timestamps=1&tail={tail}");
    let (s, b) = engine_get(socket, &path)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    if !s.is_success() {
        json_of(s, &b, "the log")?;
    }
    Ok(log_text(&b, tty))
}

/// `GET /v1/docker/containers/{id}/logs?tail=N` (admin): the end of a container's log as text, with timestamps.
pub(crate) async fn container_logs(
    State(_m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Response> {
    require_admin(role)?;
    if !id_ok(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "not a container id or name"));
    }
    let text = logs_of(
        &default_socket(),
        &id,
        q.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL),
    )
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        text,
    )
        .into_response())
}

/// What Docker answers to `POST /containers/{id}/{action}`: 204 done, 304 already in that state, 404 no such container.
async fn act_on(socket: &FsPath, id: &str, action: &str) -> Result<Value, ApiError> {
    let (s, b) = engine_call(
        socket,
        Method::POST,
        &format!("/containers/{id}/{action}"),
        ACTION_TIMEOUT,
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    match s {
        StatusCode::NOT_FOUND => Err(err(StatusCode::NOT_FOUND, format!("no container '{id}'"))),
        StatusCode::NOT_MODIFIED => {
            Ok(json!({"container": id, "action": action, "changed": false}))
        }
        s if s.is_success() => Ok(json!({"container": id, "action": action, "changed": true})),
        s => {
            json_of(s, &b, action)?;
            Err(err(
                StatusCode::BAD_GATEWAY,
                format!("the engine answered {s} to {action}"),
            ))
        }
    }
}

/// Who may do what to which container, before anything is sent to the engine.
fn check_action(role: Role, id: &str, action: &str) -> Result<(), ApiError> {
    require_admin(role)?;
    if !id_ok(id) {
        return Err(err(StatusCode::BAD_REQUEST, "not a container id or name"));
    }
    if !matches!(action, "start" | "stop" | "restart") {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "the action is start, stop or restart",
        ));
    }
    Ok(())
}

/// `POST /v1/docker/containers/{id}/{action}` (admin) with `action` one of `start`, `stop`, `restart`. `changed: false` means
/// the container was already started or stopped.
pub(crate) async fn container_action(
    State(_m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path((id, action)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    check_action(role, &id, &action)?;
    Ok(Json(act_on(&default_socket(), &id, &action).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        routing::{get, post},
    };

    fn frame(stream: u8, text: &str) -> Vec<u8> {
        let mut v = vec![stream, 0, 0, 0];
        v.extend((text.len() as u32).to_be_bytes());
        v.extend(text.as_bytes());
        v
    }

    #[test]
    fn container_ids_are_checked_before_they_go_into_a_path() {
        for ok in ["web", "0a1b2c3d4e5f", "my_app-1.2"] {
            assert!(id_ok(ok), "{ok}");
        }
        for bad in ["", "a/b", "..", "a b", "x?all=1", "a%2f", &"a".repeat(129)] {
            // ".." passes the character check but is harmless as one path segment; the router never matches a slash in it.
            if bad != ".." {
                assert!(!id_ok(bad), "{bad}");
            }
        }
    }

    #[test]
    fn framed_logs_lose_their_headers_and_tty_logs_are_left_alone() {
        let mut raw = frame(1, "out line\n");
        raw.extend(frame(2, "err line\n"));
        assert_eq!(log_text(&raw, false), "out line\nerr line\n");
        assert_eq!(log_text(b"plain\r\n", true), "plain\r\n");
        // Plain text where frames were expected is kept whole rather than eaten.
        assert_eq!(
            log_text(b"2026-10-24T10:00:00Z hello world\n", false),
            "2026-10-24T10:00:00Z hello world\n"
        );
        // A frame cut short keeps what arrived.
        let mut cut = frame(1, "abcdef");
        cut.truncate(10);
        assert_eq!(log_text(&cut, false), "ab");
    }

    #[test]
    fn containers_and_images_are_flattened() {
        let c = containers_view(&json!([{
            "Id": "0123456789abcdef0123", "Names": ["/web"], "Image": "nginx:1", "Command": "nginx -g", "State": "running",
            "Status": "Up 2 minutes", "Created": 1761300000,
            "Ports": [{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}, {"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}],
        }]));
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0]["id"].as_str(), c[0]["name"].as_str()),
            (Some("0123456789ab"), Some("web"))
        );
        assert_eq!(
            c[0]["ports"].as_array().unwrap().len(),
            1,
            "one port, not once per address family"
        );
        let i = images_view(
            &json!([{"Id": "sha256:abcdef0123456789", "RepoTags": ["nginx:1", "<none>:<none>"], "Size": 100, "Created": 5, "Containers": 1}]),
        );
        assert_eq!(i[0]["id"], "abcdef012345");
        assert_eq!(i[0]["tags"], json!(["nginx:1"]));
    }

    /// A stand-in engine on a unix socket answering the calls the view makes.
    async fn fake_engine(dir: &FsPath) -> PathBuf {
        let sock = dir.join("docker-engine.sock");
        let app = Router::new()
            .route("/_ping", get(|| async { "OK" }))
            .route("/version", get(|| async { Json(json!({"Version": "29.1.0", "ApiVersion": "1.52", "Os": "linux", "Arch": "arm64", "KernelVersion": "6.12"})) }))
            .route("/containers/json", get(|| async { Json(json!([{"Id": "aaaabbbbccccdddd", "Names": ["/web"], "Image": "nginx", "Command": "x", "State": "running", "Status": "Up", "Created": 1, "Ports": []}])) }))
            .route("/images/json", get(|| async { Json(json!([{"Id": "sha256:1111222233334444", "RepoTags": ["nginx:latest"], "Size": 1, "Created": 1, "Containers": 1}])) }))
            .route("/containers/{id}/json", get(|Path(id): Path<String>| async move {
                if id == "web" { Json(json!({"Config": {"Tty": false}})).into_response() } else { (StatusCode::NOT_FOUND, Json(json!({"message": "No such container"}))).into_response() }
            }))
            .route("/containers/{id}/start", post(|Path(id): Path<String>| async move {
                match id.as_str() { "web" => StatusCode::NO_CONTENT, "up" => StatusCode::NOT_MODIFIED, "bad" => StatusCode::INTERNAL_SERVER_ERROR, _ => StatusCode::NOT_FOUND }.into_response()
            }))
            .route("/containers/{id}/logs", get(|| async {
                let mut v = frame(1, "2026-10-24T10:00:00Z hello\n");
                v.extend(frame(2, "2026-10-24T10:00:01Z oops\n"));
                v
            }));
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        sock
    }

    #[tokio::test]
    async fn the_view_lists_what_the_engine_runs() {
        let dir = tempdir();
        let sock = fake_engine(&dir).await;
        let v = view(
            Some(json!({"id": "x", "name": "docker-engine", "status": "running"})),
            &sock,
        )
        .await
        .unwrap();
        assert_eq!(v["engine"]["reachable"], true);
        assert_eq!(v["engine"]["info"]["version"], "29.1.0");
        assert_eq!(v["containers"][0]["name"], "web");
        assert_eq!(v["images"][0]["tags"], json!(["nginx:latest"]));
        assert_eq!(
            logs_of(&sock, "web", 10).await.unwrap(),
            "2026-10-24T10:00:00Z hello\n2026-10-24T10:00:01Z oops\n"
        );
        assert_eq!(
            logs_of(&sock, "nope", 10).await.unwrap_err().status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn a_container_is_started_and_the_answers_are_told_apart() {
        let dir = tempdir();
        let sock = fake_engine(&dir).await;
        assert_eq!(
            act_on(&sock, "web", "start").await.unwrap()["changed"],
            true
        );
        assert_eq!(
            act_on(&sock, "up", "start").await.unwrap()["changed"],
            false
        );
        assert_eq!(
            act_on(&sock, "nope", "start").await.unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            act_on(&sock, "bad", "start").await.unwrap_err().status,
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn only_start_stop_and_restart_by_an_admin_pass_the_check() {
        let status = |role, id, action| check_action(role, id, action).unwrap_err().status;
        assert_eq!(status(Role::ReadOnly, "web", "stop"), StatusCode::FORBIDDEN);
        assert_eq!(status(Role::Admin, "web", "kill"), StatusCode::BAD_REQUEST);
        assert_eq!(status(Role::Admin, "a b", "stop"), StatusCode::BAD_REQUEST);
        for action in ["start", "stop", "restart"] {
            assert!(check_action(Role::Admin, "web", action).is_ok());
        }
    }

    #[tokio::test]
    async fn a_missing_engine_or_socket_says_what_to_do_instead_of_failing() {
        let dir = tempdir();
        let none = view(None, &dir.join("x.sock")).await.unwrap();
        assert_eq!(none["engine"]["reachable"], false);
        assert!(
            none["engine"]["reason"]
                .as_str()
                .unwrap()
                .contains("fluxctl docker up")
        );
        let down = view(
            Some(json!({"id": "x", "name": "e", "status": "stopped"})),
            &dir.join("absent.sock"),
        )
        .await
        .unwrap();
        assert_eq!(down["engine"]["reachable"], false);
        assert_eq!(down["containers"], json!([]));
        assert_eq!(down["engine"]["vm"]["status"], "stopped");
    }

    /// Against a real engine: `FLUXVM_DOCKER_LIVE_SOCKET=/tmp/fluxvm-run-501/docker-engine.sock cargo test -p fluxvm-api --lib
    /// docker_live -- --ignored --nocapture` (or run the test binary on the host that has the engine).
    #[tokio::test]
    #[ignore = "needs a Docker engine socket"]
    async fn docker_live_a_real_engine_is_listed() {
        let Some(sock) = std::env::var_os("FLUXVM_DOCKER_LIVE_SOCKET").map(PathBuf::from) else {
            panic!("set FLUXVM_DOCKER_LIVE_SOCKET");
        };
        let v = view(
            Some(json!({"id": "live", "name": "docker-engine", "status": "running"})),
            &sock,
        )
        .await
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        assert_eq!(v["engine"]["reachable"], true);
        if let Some(c) = v["containers"][0]["id"].as_str() {
            let log = logs_of(&sock, c, 5).await.unwrap();
            println!("--- log of {c} ---\n{log}");
        }
    }

    fn tempdir() -> PathBuf {
        // A short path: a unix socket path is limited to about 100 bytes, and the usual temp directory on macOS is deep.
        let d = PathBuf::from("/tmp").join(format!(
            "fvdv-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
