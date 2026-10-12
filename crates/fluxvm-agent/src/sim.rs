// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A simulated Mac for trying the fleet without any Macs: `fluxvm-agent sim-node`.
//!
//! It serves the slice of a node's `fluxctl serve` API that central uses (`/v1/vms` list, create, show, delete and
//! guest-agent exec) from memory, and heartbeats to central like `fluxvm-agent node`, reporting a Mac's
//! Virtualization.framework capabilities. VMs "run" the moment they are created and never boot anything. The macOS
//! guest limit of two per host is enforced, as Virtualization.framework does on a real Mac.
//!
//! Nothing here is used by a real node. It exists for demos (`scripts/fleet-sim.sh`), for developing the console and
//! clients on Linux, and for end-to-end tests of central.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::node::{NodeConfig, SimHost};

/// Apple's limit on concurrently running macOS guests per host.
const MAX_MACOS_GUESTS: usize = fluxvm_scheduler::apple_placement::MAX_MACOS_GUESTS as usize;

/// `fluxvm-agent sim-node` settings.
#[derive(Debug, Clone)]
pub struct SimConfig {
    pub name: String,
    /// Central URL, or a comma-separated list with HA.
    pub central_url: String,
    pub token: Option<String>,
    /// Where the simulated node API listens (`127.0.0.1:0` picks a free port).
    pub listen: SocketAddr,
    pub host: SimHost,
    pub labels: std::collections::HashMap<String, String>,
    pub interval: Duration,
    /// Where the node's overlay.json and keys are written, so overlay sync can be watched; `None` turns it off.
    pub overlay_config: Option<PathBuf>,
}

/// The in-memory VM table of one simulated Mac.
#[derive(Clone, Default)]
pub struct SimNode {
    vms: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl SimNode {
    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/v1/vms", get(list_vms).post(create_vm))
            .route("/v1/vms/{id}", get(get_vm).delete(delete_vm))
            .route("/v1/vms/{id}/agent", post(exec_vm))
            .route("/v1/vms/{id}/screenshot", get(screenshot))
            .with_state(self.clone())
    }

    /// Drops VMs whose `ttl_seconds` has run out, like the real daemon's reaper.
    async fn reap(&self) {
        let now = Utc::now();
        self.vms.lock().await.retain(|_, vm| {
            vm.get("expires_at")
                .and_then(Value::as_str)
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| t.with_timezone(&Utc) > now)
        });
    }
}

fn is_macos(request: &Value) -> bool {
    request
        .get("apple")
        .and_then(|a| a.get("guest_os"))
        .and_then(Value::as_str)
        == Some("macos")
}

fn error(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

async fn list_vms(State(node): State<SimNode>) -> Json<Value> {
    node.reap().await;
    let items: Vec<Value> = node.vms.lock().await.values().cloned().collect();
    Json(json!({ "items": items }))
}

async fn create_vm(
    State(node): State<SimNode>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !request.is_object() {
        return error(StatusCode::BAD_REQUEST, "request must be a JSON object");
    }
    node.reap().await;
    let mut vms = node.vms.lock().await;
    if is_macos(&request)
        && vms
            .values()
            .filter(|vm| vm.get("request").is_some_and(is_macos))
            .count()
            >= MAX_MACOS_GUESTS
    {
        return error(
            StatusCode::CONFLICT,
            format!(
                "this Mac already runs {MAX_MACOS_GUESTS} macOS guests (Apple's licence limit)"
            ),
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let expires_at = request
        .get("ttl_seconds")
        .and_then(Value::as_u64)
        .and_then(|s| chrono::TimeDelta::try_seconds(s.min(i64::MAX as u64) as i64))
        .and_then(|d| now.checked_add_signed(d))
        .map(|t| t.to_rfc3339());
    let name = request
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("vm-{}", &id[..8]));
    let record = json!({
        "id": id,
        "name": name,
        "backend": request.get("backend").cloned().unwrap_or(json!("vz")),
        "status": "running",
        "state": "running",
        "pid": null,
        "created_at": now.to_rfc3339(),
        "expires_at": expires_at,
        "error": null,
        "request": request,
        "simulated": true,
    });
    vms.insert(id, record.clone());
    (StatusCode::CREATED, Json(record))
}

async fn get_vm(State(node): State<SimNode>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    node.reap().await;
    match node.vms.lock().await.get(&id) {
        Some(vm) => (StatusCode::OK, Json(vm.clone())),
        None => error(StatusCode::NOT_FOUND, format!("no VM '{id}'")),
    }
}

async fn delete_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    match node.vms.lock().await.remove(&id) {
        Some(_) => (StatusCode::OK, Json(json!({ "deleted": id }))),
        None => error(StatusCode::NOT_FOUND, format!("no VM '{id}'")),
    }
}

/// A drawn stand-in for the guest's display (SVG, 1280x800): a desktop with the VM's name and the time, so the console's
/// screen view has something to show.
async fn screenshot(
    State(node): State<SimNode>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    node.reap().await;
    let Some(vm) = node.vms.lock().await.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'")).into_response();
    };
    let name: String = vm
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("vm")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let now = Utc::now().format("%H:%M:%S");
    let svg = if vm.get("request").is_some_and(is_macos) {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="800" viewBox="0 0 1280 800">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#2b5876"/><stop offset="1" stop-color="#4e4376"/></linearGradient></defs>
<rect width="1280" height="800" fill="url(#g)"/>
<rect width="1280" height="28" fill="#000" fill-opacity="0.35"/>
<text x="18" y="19" font-family="Helvetica, Arial" font-size="14" fill="#fff">Finder   File   Edit   View   Go   Window   Help</text>
<text x="1262" y="19" font-family="Helvetica, Arial" font-size="14" fill="#fff" text-anchor="end">{now}</text>
<rect x="340" y="220" width="600" height="320" rx="12" fill="#1e1e1e" fill-opacity="0.92"/>
<circle cx="364" cy="244" r="6" fill="#ff5f57"/><circle cx="384" cy="244" r="6" fill="#febc2e"/><circle cx="404" cy="244" r="6" fill="#28c840"/>
<text x="364" y="300" font-family="Menlo, monospace" font-size="16" fill="#e6e6e6">{name} (simulated)</text>
<text x="364" y="330" font-family="Menlo, monospace" font-size="16" fill="#8fc7ff">$ sw_vers</text>
<text x="364" y="356" font-family="Menlo, monospace" font-size="16" fill="#e6e6e6">ProductName: macOS</text>
<text x="364" y="382" font-family="Menlo, monospace" font-size="16" fill="#e6e6e6">ProductVersion: 27.0</text>
<rect x="490" y="740" width="300" height="44" rx="12" fill="#fff" fill-opacity="0.25"/>
</svg>"##
        )
    } else {
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="640" viewBox="0 0 1024 640">
<rect width="1024" height="640" fill="#101010"/>
<text x="16" y="28" font-family="Menlo, monospace" font-size="16" fill="#d0d0d0">{name} login: fluxvm (simulated)</text>
<text x="16" y="54" font-family="Menlo, monospace" font-size="16" fill="#d0d0d0">Last login: {now}</text>
<text x="16" y="80" font-family="Menlo, monospace" font-size="16" fill="#8fe388">fluxvm@{name}:~$ _</text>
</svg>"##
        )
    };
    let (w, hgt) = if vm.get("request").is_some_and(is_macos) {
        ("1280", "800")
    } else {
        ("1024", "640")
    };
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (
                axum::http::HeaderName::from_static("x-fluxvm-screen-width"),
                w,
            ),
            (
                axum::http::HeaderName::from_static("x-fluxvm-screen-height"),
                hgt,
            ),
        ],
        svg,
    )
        .into_response()
}

/// Answers like a guest agent that ran the command and printed it back.
async fn exec_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    node.reap().await;
    let Some(vm) = node.vms.lock().await.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    let command = body.get("command").and_then(Value::as_str).unwrap_or("");
    let name = vm.get("name").and_then(Value::as_str).unwrap_or("vm");
    (
        StatusCode::OK,
        Json(json!({
            "exit_code": 0,
            "stdout": format!("[simulated {name}] {command}\n"),
            "stderr": "",
            "timed_out": false,
        })),
    )
}

/// Serves the simulated node API and heartbeats to central until the process ends.
pub async fn run(cfg: SimConfig) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let app = SimNode::default().router();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "simulated node API stopped");
        }
    });
    tracing::info!(node = %cfg.name, %url, "simulated Mac serving");
    crate::node::run(NodeConfig {
        name: cfg.name,
        central_url: cfg.central_url,
        fluxvm_url: url.clone(),
        advertise_url: url,
        interval: cfg.interval,
        token: cfg.token,
        labels: cfg.labels,
        overlay_config: cfg.overlay_config,
        sim: Some(cfg.host),
    })
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn macos(name: &str) -> Value {
        json!({"backend": "vz", "name": name, "apple": {"guest_os": "macos"}})
    }

    #[tokio::test]
    async fn create_list_exec_delete() {
        let app = SimNode::default().router();
        let (s, vm) = call(&app, "POST", "/v1/vms", macos("m1")).await;
        assert_eq!(s, StatusCode::CREATED);
        let id = vm["id"].as_str().unwrap().to_owned();
        assert_eq!(vm["state"], "running");

        let (_, list) = call(&app, "GET", "/v1/vms", Value::Null).await;
        assert_eq!(list["items"].as_array().unwrap().len(), 1);

        let (s, out) = call(
            &app,
            "POST",
            &format!("/v1/vms/{id}/agent"),
            json!({"command": "uname"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(out["exit_code"], 0);
        assert!(out["stdout"].as_str().unwrap().contains("uname"));

        let (s, _) = call(&app, "DELETE", &format!("/v1/vms/{id}"), Value::Null).await;
        assert!(s.is_success());
        let (s, _) = call(&app, "GET", &format!("/v1/vms/{id}"), Value::Null).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn screenshot_draws_the_vm() {
        let app = SimNode::default().router();
        let (_, vm) = call(&app, "POST", "/v1/vms", macos("m1")).await;
        let id = vm["id"].as_str().unwrap();
        let req = Request::builder()
            .uri(format!("/v1/vms/{id}/screenshot"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["content-type"], "image/svg+xml");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("m1"));
    }

    #[tokio::test]
    async fn third_macos_guest_is_refused() {
        let app = SimNode::default().router();
        for n in ["a", "b"] {
            assert_eq!(
                call(&app, "POST", "/v1/vms", macos(n)).await.0,
                StatusCode::CREATED
            );
        }
        assert_eq!(
            call(&app, "POST", "/v1/vms", macos("c")).await.0,
            StatusCode::CONFLICT
        );
        let linux = json!({"backend": "vz", "name": "l", "apple": {"guest_os": "linux"}});
        assert_eq!(
            call(&app, "POST", "/v1/vms", linux).await.0,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn ttl_expires() {
        let node = SimNode::default();
        let app = node.router();
        let (_, vm) = call(
            &app,
            "POST",
            "/v1/vms",
            json!({"name": "t", "ttl_seconds": 0}),
        )
        .await;
        assert!(vm["expires_at"].is_string());
        tokio::time::sleep(Duration::from_millis(5)).await;
        let (_, list) = call(&app, "GET", "/v1/vms", Value::Null).await;
        assert!(list["items"].as_array().unwrap().is_empty());
    }

    /// Central and a simulated Mac over real HTTP: the Mac registers by heartbeat, a tenant leases it, creates
    /// macOS VMs until Apple's limit, runs a command through central, and gets an overlay synced to the Mac.
    #[tokio::test]
    async fn central_with_a_simulated_mac() {
        let dir = tempfile::tempdir().unwrap();
        let central = crate::central::router(crate::central::CentralConfig {
            state_dir: dir.path().join("central"),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: None,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let central_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, central).await.unwrap() });

        let overlay = dir.path().join("mac/overlay.json");
        tokio::spawn(run(SimConfig {
            name: "sim-mac".into(),
            central_url: central_url.clone(),
            token: Some("admin".into()),
            listen: "127.0.0.1:0".parse().unwrap(),
            host: SimHost {
                vcpus: 12,
                memory_mib: 32 * 1024,
                nested_virtualization: true,
                vmnet: true,
            },
            labels: Default::default(),
            interval: Duration::from_millis(50),
            overlay_config: Some(overlay.clone()),
        }));

        let http = reqwest::Client::new();
        let req = |method: reqwest::Method, path: &str, token: &str, body: Value| {
            http.request(method, format!("{central_url}{path}"))
                .bearer_auth(token)
                .json(&body)
                .send()
        };
        let mut registered = false;
        for _ in 0..100 {
            let v: Value = req(reqwest::Method::GET, "/fleet/nodes", "admin", Value::Null)
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if v.to_string().contains("sim-mac") {
                registered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(registered, "the simulated Mac never registered");

        let v: Value = req(
            reqwest::Method::POST,
            "/fleet/tenants",
            "admin",
            json!({"id": "acme"}),
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let token = v["token"].as_str().expect("tenant token").to_owned();

        let r = req(reqwest::Method::POST, "/fleet/leases", &token, json!({}))
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["lease"]["node"], "sim-mac");

        let mut ids = Vec::new();
        for n in ["m1", "m2"] {
            let r = req(reqwest::Method::POST, "/fleet/vms", &token, macos(n))
                .await
                .unwrap();
            let status = r.status();
            let v: Value = r.json().await.unwrap();
            assert!(status.is_success(), "{v}");
            ids.push(v["vm"]["id"].as_str().unwrap().to_owned());
        }
        let r = req(reqwest::Method::POST, "/fleet/vms", &token, macos("m3"))
            .await
            .unwrap();
        assert!(!r.status().is_success(), "a third macOS guest was accepted");

        let r = req(
            reqwest::Method::POST,
            &format!("/fleet/vms/sim-mac/{}/exec", ids[0]),
            &token,
            json!({"command": "sw_vers"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert!(v["stdout"].as_str().unwrap().contains("sw_vers"), "{v}");

        let r = req(
            reqwest::Method::POST,
            "/fleet/overlays",
            &token,
            json!({"name": "acme-net"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
        let mut synced = false;
        for _ in 0..100 {
            if std::fs::read_to_string(&overlay).is_ok_and(|s| s.contains("acme-net")) {
                synced = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(synced, "overlay.json was never written");

        // The VM's screen comes through central, and only for its owner.
        let r = req(
            reqwest::Method::GET,
            &format!("/fleet/vms/sim-mac/{}/screen", ids[0]),
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "image/svg+xml");

        // Every change was recorded; the tenant sees its own, with the VM's name and no request body.
        let v: Value = req(reqwest::Method::GET, "/fleet/events", &token, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = v["items"].as_array().unwrap();
        assert!(events.iter().all(|e| e["actor"] == "acme"), "{v}");
        assert!(
            events
                .iter()
                .any(|e| e["path"] == "/fleet/vms" && e["target"] == "m1" && e["status"] == 200),
            "{v}"
        );
        assert!(
            events.iter().any(|e| e["status"].as_u64() >= Some(400)),
            "the refused third VM is recorded: {v}"
        );
        let v: Value = req(reqwest::Method::GET, "/fleet/events", "admin", Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["actor"] == "operator"),
            "{v}"
        );
        assert!(
            !v.to_string().contains("/fleet/register"),
            "heartbeats are not recorded"
        );
    }
}
