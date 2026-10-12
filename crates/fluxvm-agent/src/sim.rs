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
    /// The simulated Mac's size (vCPUs, memory MiB) for `GET /v1/stats`; 12 vCPUs and 32 GiB when unset.
    size: Arc<std::sync::OnceLock<(u32, u64)>>,
    /// Snapshots per VM id, by tag.
    snapshots: Arc<Mutex<BTreeMap<String, BTreeMap<String, Value>>>>,
    /// Volumes by name, with their size in GiB.
    volumes: Arc<Mutex<BTreeMap<String, u64>>>,
    /// The echo server tunnels connect to, started on first use.
    echo: Arc<tokio::sync::OnceCell<SocketAddr>>,
}

impl SimNode {
    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/v1/vms", get(list_vms).post(create_vm))
            .route(
                "/v1/vms/{id}",
                get(get_vm).delete(delete_vm).patch(patch_vm),
            )
            .route("/v1/vms/{id}/agent", post(exec_vm))
            .route("/v1/vms/{id}/screenshot", get(screenshot))
            .route("/v1/vms/{id}/input", post(input))
            .route("/v1/vms/{id}/logs", get(logs))
            .route("/v1/stats", get(stats))
            .route("/v1/vms/{id}/tunnel", get(tunnel))
            .route(
                "/v1/vms/{id}/start",
                post(|s: State<SimNode>, p: Path<String>| power(s, p, "running")),
            )
            .route(
                "/v1/vms/{id}/restart",
                post(|s: State<SimNode>, p: Path<String>| power(s, p, "running")),
            )
            .route(
                "/v1/vms/{id}/resume",
                post(|s: State<SimNode>, p: Path<String>| power(s, p, "running")),
            )
            .route(
                "/v1/vms/{id}/stop",
                post(|s: State<SimNode>, p: Path<String>| power(s, p, "stopped")),
            )
            .route(
                "/v1/vms/{id}/pause",
                post(|s: State<SimNode>, p: Path<String>| power(s, p, "paused")),
            )
            // ---- snapshots ----
            .route("/v1/vms/{id}/snapshot", post(snapshot_vm))
            .route("/v1/vms/{id}/snapshots", get(list_snapshots))
            .route(
                "/v1/vms/{id}/snapshots/{tag}",
                axum::routing::delete(delete_snapshot),
            )
            .route("/v1/vms/{id}/restore", post(restore_vm))
            .route("/v1/vms/{id}/clone", post(clone_vm))
            .route("/v1/volumes", get(list_volumes).post(create_volume))
            .route("/v1/volumes/{name}", get(get_volume).delete(delete_volume))
            .route("/v1/volumes/{name}/resize", post(resize_volume))
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
    node.snapshots.lock().await.remove(&id);
    match node.vms.lock().await.remove(&id) {
        Some(_) => (StatusCode::OK, Json(json!({ "deleted": id }))),
        None => error(StatusCode::NOT_FOUND, format!("no VM '{id}'")),
    }
}

/// Start, stop, restart, pause and resume: the VM's status changes at once.
async fn power(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    status: &'static str,
) -> (StatusCode, Json<Value>) {
    let mut vms = node.vms.lock().await;
    let Some(vm) = vms.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    vm["status"] = json!(status);
    vm["state"] = json!(status);
    (StatusCode::OK, Json(vm.clone()))
}

/// Keyboard and mouse input: typed text and the last click show on the drawn screen.
async fn input(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut vms = node.vms.lock().await;
    let Some(vm) = vms.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    if vm.get("status").and_then(Value::as_str) != Some("running") {
        return error(StatusCode::CONFLICT, "the VM is not running");
    }
    let actions: Vec<Value> = match body.get("actions").and_then(Value::as_array) {
        Some(a) => a.clone(),
        None => vec![body.clone()],
    };
    let mut typed = vm
        .get("sim_typed")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    for a in &actions {
        match a.get("action").and_then(Value::as_str) {
            Some("type") => typed.push_str(a.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("key") => match a.get("key").and_then(Value::as_str) {
                Some("backspace" | "delete") => {
                    typed.pop();
                }
                Some("enter" | "return") => typed.clear(),
                Some(k) if k.chars().count() == 1 => typed.push_str(k),
                _ => {}
            },
            Some("click" | "double_click" | "right_click") => {
                let scale = a
                    .get("screen_width")
                    .and_then(Value::as_f64)
                    .map_or(1.0, |w| 1280.0 / w.max(1.0));
                let x = a.get("x").and_then(Value::as_f64).unwrap_or(0.0) * scale;
                let y = a.get("y").and_then(Value::as_f64).unwrap_or(0.0) * scale;
                vm["sim_click"] = json!([x.round(), y.round()]);
            }
            _ => {}
        }
    }
    let keep: String = typed
        .chars()
        .rev()
        .take(48)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    vm["sim_typed"] = json!(keep);
    (StatusCode::OK, Json(json!({ "sent": actions.len() })))
}

/// A few log lines, like a guest's console log.
async fn logs(State(node): State<SimNode>, Path(id): Path<String>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(vm) = node.vms.lock().await.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'")).into_response();
    };
    let name = vm.get("name").and_then(Value::as_str).unwrap_or("vm");
    let created = vm.get("created_at").and_then(Value::as_str).unwrap_or("");
    let status = vm
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!(
        "{created} fluxvm: creating {name} (simulated)\n{created} vz: boot loader started\n{created} vz: guest agent connected\n{} fluxvm: {name} is {status}\n",
        Utc::now().to_rfc3339()
    )
    .into_response()
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
    // Only characters that are safe inside SVG text.
    let typed: String = vm
        .get("sim_typed")
        .and_then(Value::as_str)
        .unwrap_or("sw_vers")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || " -_.,:/=+()".contains(*c))
        .collect();
    let click = vm
        .get("sim_click")
        .and_then(Value::as_array)
        .map(|p| {
            let x = p.first().and_then(Value::as_f64).unwrap_or(0.0);
            let y = p.get(1).and_then(Value::as_f64).unwrap_or(0.0);
            format!(r##"<circle cx="{x}" cy="{y}" r="10" fill="none" stroke="#ffcc00" stroke-width="3"/>"##)
        })
        .unwrap_or_default();
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
<text x="364" y="330" font-family="Menlo, monospace" font-size="16" fill="#8fc7ff">$ {typed}</text>{click}
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

// ---- metrics ----

/// `∫₀ᵗ (mean + amp·sin(2πs/period + phase)) ds`: a counter whose rate wanders smoothly around `mean` and never goes
/// negative (as long as `amp <= mean`), so successive samples always give plausible, varying rates.
fn wave_integral(t: f64, mean: f64, amp: f64, period: f64, phase: f64) -> f64 {
    let w = std::f64::consts::TAU / period;
    mean * t + amp * (phase.cos() - (w * t + phase).cos()) / w
}

fn wave(t: f64, mean: f64, amp: f64, period: f64, phase: f64) -> f64 {
    mean + amp * (std::f64::consts::TAU * t / period + phase).sin()
}

/// A stable per-VM number in [0, 1) from its id, so each VM has its own load pattern.
fn seed(id: &str) -> f64 {
    let h = id.bytes().fold(2166136261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16777619)
    });
    f64::from(h % 10_000) / 10_000.0
}

/// When this simulated host "booted" (the first stats request), for its CPU tick counters.
static SIM_BOOT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// One sample in the shape of a real node's `GET /v1/stats`: counters that grow at gently varying rates, so the
/// collector, the graphs and `GET /metrics` have something plausible to show. Stopped VMs report nothing.
fn sim_stats(vms: &BTreeMap<String, Value>, size: (u32, u64), now: chrono::DateTime<Utc>) -> Value {
    let (cpus, mem_mib) = size;
    let uptime = SIM_BOOT
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f64();
    let mut items = Vec::new();
    let (mut vm_busy, mut vm_mem, mut running) = (0f64, 0f64, 0u32);
    for (id, vm) in vms {
        let status = vm
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("running");
        let req = vm.get("request").cloned().unwrap_or(Value::Null);
        let vcpus = req.get("vcpus").and_then(Value::as_u64).unwrap_or(2).max(1) as f64;
        let memory_mib = req
            .get("memory_mib")
            .and_then(Value::as_u64)
            .unwrap_or(4096);
        let created = vm
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or(now);
        let t = ((now - created).num_milliseconds() as f64 / 1000.0).max(0.0);
        let k = seed(id);
        let p = k * std::f64::consts::TAU;
        let mut row = json!({
            "id": id,
            "name": vm.get("name").cloned().unwrap_or(json!("")),
            "status": status,
            "backend": vm.get("backend").cloned().unwrap_or(json!("vz")),
            "vcpus": vcpus as u32,
            "memory_mib": memory_mib,
            "source": "none",
        });
        if status == "running" {
            running += 1;
            // Busy share of its vCPUs: 15-55% on average, a 10-minute swell and a faster ripple on top.
            let mean = 0.15 + 0.4 * k;
            let cpu = vcpus
                * (wave_integral(t, mean, 0.6 * mean, 600.0, p)
                    + wave_integral(t, 0.04, 0.04, 97.0, p * 3.0));
            let mem = memory_mib as f64 * wave(t, 0.45 + 0.2 * k, 0.08, 900.0, p) * 1_048_576.0;
            let mb = 1_048_576.0;
            row["source"] = json!("vz-process");
            row["cpu_seconds_total"] = json!(cpu);
            row["memory_used_bytes"] = json!(mem as u64);
            row["disk_read_bytes_total"] =
                json!(
                    wave_integral(t, 2.0 * mb * (0.5 + k), 1.5 * mb * (0.5 + k), 300.0, p) as u64
                );
            row["disk_write_bytes_total"] =
                json!(wave_integral(t, 0.8 * mb, 0.6 * mb, 240.0, p + 1.0) as u64);
            // Simulated guests report network too, so the network graph can be tried; real vz guests cannot.
            row["net_rx_bytes_total"] =
                json!(wave_integral(t, 0.4 * mb, 0.35 * mb, 180.0, p + 2.0) as u64);
            row["net_tx_bytes_total"] =
                json!(wave_integral(t, 0.15 * mb, 0.12 * mb, 210.0, p + 0.5) as u64);
            vm_busy += cpu;
            vm_mem += mem / 1_048_576.0;
        }
        items.push(row);
    }
    let hz = 100.0;
    let total = f64::from(cpus) * uptime * hz;
    // Host load: a little background work plus the guests' recent CPU, never more than the whole machine.
    let busy = (0.04 * total + wave_integral(uptime, 0.3, 0.25, 420.0, 0.0) * hz + vm_busy * hz)
        .min(total * 0.97);
    let used = (2048.0 + vm_mem).min(mem_mib as f64 * 0.95);
    json!({
        "ts": now,
        "host": {
            "cpus": cpus,
            "cpu_busy_ticks": busy as u64,
            "cpu_total_ticks": total as u64,
            "memory_total_mib": mem_mib,
            "memory_available_mib": (mem_mib as f64 - used) as u64,
            "disk_total_bytes": 994_662_584_320u64,
            "disk_free_bytes": (994_662_584_320u64 / 10 * 6)
                .saturating_sub(vms.len() as u64 * 21_474_836_480),
            "vm_count": vms.len(),
            "vms_running": running,
        },
        "vms": items,
    })
}

async fn stats(State(node): State<SimNode>) -> Json<Value> {
    node.reap().await;
    let size = node.size.get().copied().unwrap_or((12, 32 * 1024));
    let vms = node.vms.lock().await;
    Json(sim_stats(&vms, size, Utc::now()))
}
// ---- snapshots ----

/// A simulated VM's disk, as far as snapshot sizes go.
const SIM_DISK_BYTES: u64 = 3 << 29;

/// `POST /v1/vms/{id}/snapshot {"tag", "memory"}`, as the real node answers it: memory needs a running or paused VM.
/// The guest's "state" kept is what was typed on its screen.
async fn snapshot_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(vm) = node.vms.lock().await.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    let Some(tag) = body.get("tag").and_then(Value::as_str).map(str::to_owned) else {
        return error(StatusCode::BAD_REQUEST, "tag is required");
    };
    let memory = body.get("memory").and_then(Value::as_bool).unwrap_or(true);
    let status = vm.get("status").and_then(Value::as_str).unwrap_or("");
    if memory && !matches!(status, "running" | "paused") {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("snapshot requires a running or paused VM (status={status})"),
        );
    }
    let mut snaps = node.snapshots.lock().await;
    let of_vm = snaps.entry(id).or_default();
    if of_vm.contains_key(&tag) {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("snapshot {tag:?} already exists for this VM"),
        );
    }
    let mem_bytes = vm
        .pointer("/request/memory_mib")
        .and_then(Value::as_u64)
        .unwrap_or(2048)
        << 20;
    let size = SIM_DISK_BYTES + if memory { mem_bytes } else { 0 };
    of_vm.insert(
        tag.clone(),
        json!({
            "tag": tag,
            "created_at": Utc::now().to_rfc3339(),
            "size_bytes": size,
            "has_memory": memory,
            "sim_typed": vm.get("sim_typed").cloned().unwrap_or(Value::Null),
        }),
    );
    (
        StatusCode::OK,
        Json(json!({"ok": true, "tag": tag, "memory": memory})),
    )
}

async fn list_snapshots(
    State(node): State<SimNode>,
    Path(id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if !node.vms.lock().await.contains_key(&id) {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    }
    let items: Vec<Value> = node
        .snapshots
        .lock()
        .await
        .get(&id)
        .map(|m| {
            m.values()
                .map(|s| {
                    let mut s = s.clone();
                    if let Some(o) = s.as_object_mut() {
                        o.remove("sim_typed");
                    }
                    s
                })
                .collect()
        })
        .unwrap_or_default();
    (StatusCode::OK, Json(json!({ "items": items })))
}

async fn delete_snapshot(
    State(node): State<SimNode>,
    Path((id, tag)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    match node
        .snapshots
        .lock()
        .await
        .get_mut(&id)
        .and_then(|m| m.remove(&tag))
    {
        Some(_) => (StatusCode::NO_CONTENT, Json(Value::Null)),
        None => error(
            StatusCode::NOT_FOUND,
            format!("snapshot {tag:?} not found for VM {id}"),
        ),
    }
}

/// Stop, put the snapshot back, start: the VM runs again with the state it had.
async fn restore_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let tag = body.get("tag").and_then(Value::as_str).unwrap_or("");
    let Some(snap) = node
        .snapshots
        .lock()
        .await
        .get(&id)
        .and_then(|m| m.get(tag))
        .cloned()
    else {
        return error(
            StatusCode::NOT_FOUND,
            format!("VM {id} has no snapshot {tag:?}"),
        );
    };
    let mut vms = node.vms.lock().await;
    let Some(vm) = vms.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    match snap.get("sim_typed") {
        Some(Value::String(t)) => vm["sim_typed"] = json!(t),
        _ => {
            if let Some(o) = vm.as_object_mut() {
                o.remove("sim_typed");
            }
        }
    }
    vm["status"] = json!("running");
    vm["state"] = json!("running");
    (StatusCode::OK, Json(vm.clone()))
}

/// A new VM from a snapshot, or from a stopped VM. Apple's two-macOS-guests limit applies to it too.
async fn clone_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(name) = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    else {
        return error(StatusCode::BAD_REQUEST, "name is required");
    };
    let snapshot = body.get("snapshot").and_then(Value::as_str);
    let typed = match snapshot {
        Some(tag) => match node
            .snapshots
            .lock()
            .await
            .get(&id)
            .and_then(|m| m.get(tag))
        {
            Some(s) => s.get("sim_typed").cloned(),
            None => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("snapshot {tag:?} not found for VM {id}"),
                );
            }
        },
        None => None,
    };
    let mut vms = node.vms.lock().await;
    let Some(src) = vms.get(&id).cloned() else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    let status = src.get("status").and_then(Value::as_str).unwrap_or("");
    if snapshot.is_none() && status != "stopped" {
        return error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "clone needs a stopped VM for a consistent disk (status={status}); stop it first, or clone from a snapshot"
            ),
        );
    }
    let mut request = src.get("request").cloned().unwrap_or_else(|| json!({}));
    request["name"] = json!(name);
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
    let new_id = uuid::Uuid::new_v4().to_string();
    let mut record = json!({
        "id": new_id,
        "name": name,
        "backend": src.get("backend").cloned().unwrap_or(json!("vz")),
        "status": "running",
        "state": "running",
        "pid": null,
        "created_at": Utc::now().to_rfc3339(),
        "expires_at": null,
        "error": null,
        "request": request,
        "simulated": true,
    });
    if let Some(Value::String(t)) = typed.or_else(|| src.get("sim_typed").cloned()) {
        record["sim_typed"] = json!(t);
    }
    vms.insert(new_id, record.clone());
    (StatusCode::CREATED, Json(record))
}
// ---- volumes ----
// Like `fluxctl serve`'s `/v1/volumes`: a volume is a name and a size (nothing is written), attached while a VM's
// `apple.extra_disks` names its path.

fn sim_volume_path(name: &str) -> String {
    format!("/sim/volumes/{name}.raw")
}

/// The VMs whose request names `path` as an extra disk.
fn volume_users(vms: &BTreeMap<String, Value>, path: &str) -> Vec<String> {
    vms.iter()
        .filter(|(_, vm)| {
            vm.pointer("/request/apple/extra_disks")
                .and_then(Value::as_array)
                .is_some_and(|d| {
                    d.iter()
                        .any(|x| x.get("path").and_then(Value::as_str) == Some(path))
                })
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn volume_json(name: &str, gib: u64, vms: &BTreeMap<String, Value>) -> Value {
    let path = sim_volume_path(name);
    json!({
        "name": name,
        "path": path,
        "format": "raw",
        "size_gib": gib,
        "size_bytes": gib << 30,
        "allocated_bytes": 0,
        "attached_to": volume_users(vms, &path),
    })
}

async fn list_volumes(State(node): State<SimNode>) -> Json<Value> {
    let vms = node.vms.lock().await;
    let items: Vec<Value> = node
        .volumes
        .lock()
        .await
        .iter()
        .map(|(n, g)| volume_json(n, *g, &vms))
        .collect();
    Json(json!({ "dir": "/sim/volumes", "items": items }))
}

async fn create_volume(
    State(node): State<SimNode>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = body.get("name").and_then(Value::as_str).unwrap_or("");
    let gib = body.get("size_gib").and_then(Value::as_u64).unwrap_or(0);
    let ok_name = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok_name || !(1..=16 * 1024).contains(&gib) {
        return error(StatusCode::BAD_REQUEST, "bad volume name or size_gib");
    }
    let vms = node.vms.lock().await;
    let mut vols = node.volumes.lock().await;
    if vols.contains_key(name) {
        return error(
            StatusCode::CONFLICT,
            format!("volume {name:?} already exists"),
        );
    }
    vols.insert(name.to_owned(), gib);
    (StatusCode::CREATED, Json(volume_json(name, gib, &vms)))
}

async fn get_volume(
    State(node): State<SimNode>,
    Path(name): Path<String>,
) -> (StatusCode, Json<Value>) {
    let vms = node.vms.lock().await;
    match node.volumes.lock().await.get(&name) {
        Some(g) => (StatusCode::OK, Json(volume_json(&name, *g, &vms))),
        None => error(StatusCode::NOT_FOUND, format!("no volume {name:?}")),
    }
}

async fn delete_volume(
    State(node): State<SimNode>,
    Path(name): Path<String>,
) -> (StatusCode, Json<Value>) {
    let vms = node.vms.lock().await;
    let mut vols = node.volumes.lock().await;
    if !vols.contains_key(&name) {
        return error(StatusCode::NOT_FOUND, format!("no volume {name:?}"));
    }
    let users = volume_users(&vms, &sim_volume_path(&name));
    if !users.is_empty() {
        return error(
            StatusCode::CONFLICT,
            format!(
                "volume {name:?} is attached to VM {}; delete the VM first",
                users.join(", ")
            ),
        );
    }
    vols.remove(&name);
    (StatusCode::OK, Json(json!({ "deleted": name })))
}

async fn resize_volume(
    State(node): State<SimNode>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let gib = body.get("size_gib").and_then(Value::as_u64).unwrap_or(0);
    let vms = node.vms.lock().await;
    let mut vols = node.volumes.lock().await;
    let Some(cur) = vols.get(&name).copied() else {
        return error(StatusCode::NOT_FOUND, format!("no volume {name:?}"));
    };
    if !volume_users(&vms, &sim_volume_path(&name)).is_empty() {
        return error(
            StatusCode::CONFLICT,
            format!("volume {name:?} is attached; resize it while detached"),
        );
    }
    if gib < cur || gib > 16 * 1024 {
        return error(StatusCode::BAD_REQUEST, "volumes only grow");
    }
    vols.insert(name.clone(), gib);
    (StatusCode::OK, Json(volume_json(&name, gib, &vms)))
}
// ---- tunnel ----

/// `GET /v1/vms/{id}/tunnel?port=N` (WebSocket): like a node's, but every port of every running VM is a local echo
/// server, so bytes sent through central come straight back.
async fn tunnel(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if q.get("port")
        .and_then(|p| p.parse::<u16>().ok())
        .is_none_or(|p| p == 0)
    {
        return error(StatusCode::BAD_REQUEST, "port must be 1-65535").into_response();
    }
    node.reap().await;
    let running = match node.vms.lock().await.get(&id) {
        None => return error(StatusCode::NOT_FOUND, format!("no VM '{id}'")).into_response(),
        Some(vm) => vm.get("status").and_then(Value::as_str) == Some("running"),
    };
    if !running {
        return error(StatusCode::CONFLICT, "the VM is not running").into_response();
    }
    let addr = match node.echo.get_or_try_init(start_echo).await {
        Ok(a) => *a,
        Err(e) => {
            return error(StatusCode::BAD_GATEWAY, format!("echo server: {e}")).into_response();
        }
    };
    let tcp = match tokio::net::TcpStream::connect(addr).await {
        Ok(t) => t,
        Err(e) => {
            return error(StatusCode::BAD_GATEWAY, format!("connecting: {e}")).into_response();
        }
    };
    ws.on_upgrade(move |socket| relay_tcp(socket, tcp))
}

/// A TCP server on a free loopback port that sends back whatever it gets.
async fn start_echo() -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = conn.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    Ok(addr)
}

/// Binary WebSocket messages to the TCP connection and its bytes back, until either side closes.
async fn relay_tcp(socket: axum::extract::ws::WebSocket, tcp: tokio::net::TcpStream) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut rd, mut wr) = tcp.into_split();
    let (mut tx, mut rx) = socket.split();
    let up = async {
        while let Some(Ok(msg)) = rx.next().await {
            let ok = match msg {
                Message::Binary(b) => wr.write_all(&b).await.is_ok(),
                Message::Text(t) => wr.write_all(t.as_str().as_bytes()).await.is_ok(),
                Message::Close(_) => false,
                Message::Ping(_) | Message::Pong(_) => true,
            };
            if !ok {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}
// ---- warmpool ----

/// `PATCH /v1/vms/{id}`: rename and edit labels (`{"name": "…", "labels": {"k": "v", "gone": null}}`), as the real
/// daemon does. Central uses it to hand a warm VM over.
async fn patch_vm(
    State(node): State<SimNode>,
    Path(id): Path<String>,
    Json(patch): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let mut vms = node.vms.lock().await;
    let Some(vm) = vms.get_mut(&id) else {
        return error(StatusCode::NOT_FOUND, format!("no VM '{id}'"));
    };
    if let Some(name) = patch.get("name").and_then(Value::as_str) {
        let ok = !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !ok {
            return error(StatusCode::BAD_REQUEST, format!("invalid VM name {name:?}"));
        }
        vm["name"] = json!(name);
        if let Some(req) = vm.get_mut("request").and_then(Value::as_object_mut) {
            req.insert("name".into(), json!(name));
        }
    }
    if let Some(changes) = patch.get("labels").and_then(Value::as_object) {
        if !vm.get("labels").is_some_and(Value::is_object) {
            vm["labels"] = json!({});
        }
        if let Some(labels) = vm["labels"].as_object_mut() {
            for (k, v) in changes {
                match v.as_str() {
                    Some(v) => {
                        labels.insert(k.clone(), json!(v));
                    }
                    None => {
                        labels.remove(k);
                    }
                }
            }
        }
    }
    (StatusCode::OK, Json(vm.clone()))
}

/// Serves the simulated node API and heartbeats to central until the process ends.
pub async fn run(cfg: SimConfig) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let node = SimNode::default();
    let _ = node.size.set((cfg.host.vcpus, cfg.host.memory_mib));
    let app = node.router();
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

    #[test]
    fn simulated_counters_only_grow_and_stopped_vms_report_nothing() {
        let created = Utc::now() - chrono::TimeDelta::seconds(600);
        let mut vms = BTreeMap::new();
        vms.insert(
            "a".to_owned(),
            json!({"name": "a", "status": "running", "created_at": created.to_rfc3339(),
                   "request": {"vcpus": 4, "memory_mib": 8192}}),
        );
        vms.insert(
            "b".to_owned(),
            json!({"name": "b", "status": "stopped", "created_at": created.to_rfc3339(), "request": {}}),
        );
        let t0 = Utc::now();
        let s0: fluxvm_scheduler::node_stats::NodeStats =
            serde_json::from_value(sim_stats(&vms, (12, 32768), t0)).unwrap();
        let s1: fluxvm_scheduler::node_stats::NodeStats = serde_json::from_value(sim_stats(
            &vms,
            (12, 32768),
            t0 + chrono::TimeDelta::seconds(15),
        ))
        .unwrap();
        let (a0, a1) = (&s0.vms[0], &s1.vms[0]);
        let cpu = (a1.cpu_seconds_total.unwrap() - a0.cpu_seconds_total.unwrap()) / 15.0 / 4.0;
        assert!(cpu > 0.0 && cpu <= 1.0, "{cpu}");
        assert!(a1.disk_read_bytes_total > a0.disk_read_bytes_total);
        assert!(a1.net_tx_bytes_total > a0.net_tx_bytes_total);
        assert!(a1.memory_used_bytes.unwrap() < 8192 * 1_048_576);
        assert_eq!(s1.vms[1].cpu_seconds_total, None);
        assert_eq!(s1.vms[1].source, "none");
        assert_eq!(s1.host.vms_running, 1);
        assert!(s1.host.cpu_busy_ticks <= s1.host.cpu_total_ticks);
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

        // ---- metrics ----
        // Two samples a moment apart give rates; the owner sees its VM's and its Mac's graphs.
        for _ in 0..2 {
            let r = req(
                reqwest::Method::POST,
                "/fleet/metrics/collect",
                "admin",
                Value::Null,
            )
            .await
            .unwrap();
            assert_eq!(r.status(), 200);
            let v: Value = r.json().await.unwrap();
            assert_eq!(v["polled"], 1, "{v}");
            assert_eq!(v["failed"], 0, "{v}");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let path = format!("/fleet/metrics/vms/sim-mac/{}?range=1h", ids[0]);
        let r = req(reqwest::Method::GET, &path, &token, Value::Null)
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["name"], "m1", "{v}");
        let cpu = v["latest"]["cpu_percent"].as_f64().expect("a CPU rate");
        assert!((0.0..=100.0).contains(&cpu), "{v}");
        assert!(
            v["latest"]["memory_used_mib"].as_f64().unwrap() > 0.0,
            "{v}"
        );
        assert!(v["latest"]["disk_read_bps"].as_f64().unwrap() > 0.0, "{v}");
        assert!(!v["points"].as_array().unwrap().is_empty(), "{v}");
        let r = req(
            reqwest::Method::GET,
            &format!("/fleet/metrics/vms/sim-mac/{}?range=7d", ids[0]),
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 400, "an unknown range is refused");
        let r = req(
            reqwest::Method::GET,
            "/fleet/metrics/nodes/sim-mac?range=6h",
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200, "a tenant sees the Mac it leases");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["cpus"], 12, "{v}");
        assert!(v["latest"]["cpu_percent"].is_number(), "{v}");
        let v: Value = req(
            reqwest::Method::GET,
            "/fleet/metrics/summary",
            &token,
            Value::Null,
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(v["scope"], "tenant");
        assert_eq!(v["vms"], 2, "{v}");
        assert_eq!(v["nodes_reporting"], 1, "{v}");
        // Another tenant sees neither the VM nor the Mac.
        let other: Value = req(
            reqwest::Method::POST,
            "/fleet/tenants",
            "admin",
            json!({"id": "globex"}),
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let other = other["token"].as_str().unwrap().to_owned();
        for path in [path.as_str(), "/fleet/metrics/nodes/sim-mac"] {
            let r = req(reqwest::Method::GET, path, &other, Value::Null)
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{path}");
        }
        let v: Value = req(
            reqwest::Method::GET,
            "/fleet/metrics/vms",
            &other,
            Value::Null,
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert!(v["items"].as_array().unwrap().is_empty(), "{v}");
        let r = req(
            reqwest::Method::POST,
            "/fleet/metrics/collect",
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 403, "tenants cannot drive the collector");
        // Prometheus: the operator token reads it, a tenant token does not.
        let r = req(reqwest::Method::GET, "/metrics", &token, Value::Null)
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        let r = req(reqwest::Method::GET, "/metrics", "admin", Value::Null)
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            r.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        let text = r.text().await.unwrap();
        let vm_labels = format!("vm=\"{}\",name=\"m1\",tenant=\"acme\"", ids[0]);
        for want in [
            "# TYPE fluxvm_vm_cpu_seconds_total counter",
            vm_labels.as_str(),
            "fluxvm_node_up{node=\"sim-mac\"} 1",
            "fluxvm_tenant_leases_active{tenant=\"acme\"} 1",
            "fluxvm_central_ha_leader 1",
            "fluxvm_central_http_requests_total{method=\"GET\",route=\"/fleet/metrics/summary\",status=\"200\"}",
        ] {
            assert!(text.contains(want), "missing {want} in:\n{text}");
        }

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

        // Power, input and logs go through central too.
        for (action, want) in [("stop", "stopped"), ("start", "running")] {
            let r = req(
                reqwest::Method::POST,
                &format!("/fleet/vms/sim-mac/{}/{action}", ids[0]),
                &token,
                Value::Null,
            )
            .await
            .unwrap();
            assert_eq!(r.status(), 200, "{action}");
            let v: Value = r.json().await.unwrap();
            assert_eq!(v["status"], want);
        }
        let r = req(
            reqwest::Method::POST,
            &format!("/fleet/vms/sim-mac/{}/input", ids[0]),
            &token,
            json!({"actions": [{"action": "type", "text": "hello"}, {"action": "click", "x": 10, "y": 10}]}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let r = req(
            reqwest::Method::GET,
            &format!("/fleet/vms/sim-mac/{}/logs?lines=50", ids[0]),
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("m1"));
        let r = req(
            reqwest::Method::POST,
            &format!("/fleet/vms/sim-mac/{}/explode", ids[0]),
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert!(!r.status().is_success(), "unknown actions are refused");

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

        // ---- tunnel ----
        // Bytes sent into a tunnel come back from the simulated VM's echo server, through central and the node.
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
        let ws_base = central_url.replacen("http://", "ws://", 1);
        let open = |port: u16, tok: &str| {
            let mut r = format!("{ws_base}/fleet/vms/sim-mac/{}/tunnel?port={port}", ids[0])
                .into_client_request()
                .unwrap();
            r.headers_mut()
                .insert("authorization", format!("Bearer {tok}").parse().unwrap());
            tokio_tungstenite::connect_async(r)
        };
        let refused = |e: Option<tungstenite::Error>| match e {
            Some(tungstenite::Error::Http(r)) => r.status().as_u16(),
            Some(e) => panic!("unexpected tunnel error: {e}"),
            None => panic!("the tunnel was accepted"),
        };
        let (mut ws, _) = open(22, &token).await.expect("the tunnel opens");
        let hello = b"SSH-2.0-OpenSSH_test\r\n".to_vec();
        ws.send(Message::Binary(hello.clone().into()))
            .await
            .unwrap();
        let mut got = Vec::new();
        while got.len() < hello.len() {
            match tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("the echo comes back in time")
            {
                Some(Ok(Message::Binary(b))) => got.extend_from_slice(&b),
                Some(Ok(_)) => {}
                other => panic!("the tunnel ended early: {other:?}"),
            }
        }
        assert_eq!(got, hello);

        // Listed for its owner with the bytes counted; another tenant neither sees nor closes nor opens it.
        let mut listed = Value::Null;
        for _ in 0..50 {
            listed = req(reqwest::Method::GET, "/fleet/tunnels", &token, Value::Null)
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if listed["items"][0]["bytes_received"] == hello.len() as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let items = listed["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{listed}");
        assert_eq!(items[0]["vm_name"], "m1");
        assert_eq!(items[0]["port"], 22);
        assert_eq!(items[0]["bytes_sent"], hello.len() as u64);
        let tunnel_id = items[0]["id"].as_str().unwrap().to_owned();
        // `other` is globex, created for the metrics checks above (creating it twice would be a conflict).
        let v: Value = req(reqwest::Method::GET, "/fleet/tunnels", &other, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(v["items"].as_array().unwrap().is_empty(), "{v}");
        let r = req(
            reqwest::Method::DELETE,
            &format!("/fleet/tunnels/{tunnel_id}"),
            &other,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(refused(open(22, &other).await.err()), 404);
        let v: Value = req(reqwest::Method::GET, "/fleet/tunnels", "admin", Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            v["items"].as_array().unwrap().len(),
            1,
            "the operator sees every tunnel"
        );

        // Ports outside the policy, and read-only keys, are refused.
        assert_eq!(refused(open(80, &token).await.err()), 403);
        let v: Value = req(
            reqwest::Method::POST,
            "/fleet/tenants/me/keys",
            &token,
            json!({"name": "dashboard", "read_only": true}),
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let read_only = v["token"].as_str().unwrap().to_owned();
        assert_eq!(refused(open(22, &read_only).await.err()), 403);

        // The per-tenant limit counts open tunnels.
        let mut policy: Value = req(reqwest::Method::GET, "/fleet/policy", "admin", Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        policy["tunnels"]["max_per_tenant"] = json!(1);
        let r = req(
            reqwest::Method::PUT,
            "/fleet/policy",
            "admin",
            policy.clone(),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(refused(open(2222, &token).await.err()), 429);
        policy["tunnels"]["ports"] = json!(["70000"]);
        let r = req(reqwest::Method::PUT, "/fleet/policy", "admin", policy)
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "a bad port rule is refused");

        // Closing it ends the client's connection, frees the slot and is recorded with the traffic.
        let r = req(
            reqwest::Method::DELETE,
            &format!("/fleet/tunnels/{tunnel_id}"),
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        loop {
            match tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("the tunnel closes in time")
            {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
        let mut freed = false;
        for _ in 0..50 {
            let v: Value = req(reqwest::Method::GET, "/fleet/tunnels", &token, Value::Null)
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if v["items"].as_array().unwrap().is_empty() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(freed, "the closed tunnel is still listed");
        let v: Value = req(reqwest::Method::GET, "/fleet/events", &token, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = v["items"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["method"] == "OPEN"
                && e["status"] == 101
                && e["target"] == "m1 port 22"),
            "{v}"
        );
        assert!(
            events.iter().any(|e| {
                e["method"] == "CLOSE"
                    && e["target"].as_str().is_some_and(|t| {
                        t.contains("22 B sent") && t.contains("closed by the tenant")
                    })
            }),
            "{v}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["method"] == "OPEN" && e["status"] == 403),
            "refusals are recorded: {v}"
        );

        // ---- warmpool ----
        // A tenant pool keeps one Linux VM booted on the tenant's Mac; a launch of that image takes it at once, and the
        // pool boots a replacement.
        let r = req(
            reqwest::Method::PUT,
            "/fleet/images/debian-13",
            "admin",
            json!({"guest_os": "linux", "image": "/vms/debian-13/disk.raw", "vcpus": 2, "memory_mib": 2048}),
        )
        .await
        .unwrap();
        assert!(r.status().is_success());
        let r = req(
            reqwest::Method::POST,
            "/fleet/pools",
            &token,
            json!({"name": "ci", "image": "debian-13", "min_ready": 1}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
        let pool_ready = |want_claims: u64| {
            let req = &req;
            let token = token.clone();
            async move {
                for _ in 0..100 {
                    let v: Value = req(
                        reqwest::Method::GET,
                        "/fleet/pools/ci/status",
                        &token,
                        Value::Null,
                    )
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                    if v["ready"] == 1 && v["claimed_24h"].as_u64() == Some(want_claims) {
                        return v;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("pool ci never had a ready VM");
            }
        };
        pool_ready(0).await;
        let r = req(
            reqwest::Method::POST,
            "/fleet/vms",
            &token,
            json!({"name": "job-1", "backend": "vz", "image": "debian-13", "apple": {"guest_os": "linux"}}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["warm"], true, "{v}");
        assert_eq!(v["pool"], "ci");
        assert_eq!(v["vm"]["name"], "job-1");
        let st = pool_ready(1).await;
        assert!(st["avg_claim_ms"].is_u64(), "{st}");
        let v: Value = req(reqwest::Method::GET, "/fleet/vms", &token, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|vm| vm["name"] == "job-1" && vm["claimed_from"] == "ci"),
            "{v}"
        );
        assert!(
            items
                .iter()
                .any(|vm| vm["warm_pool"] == "ci" && vm["warm_state"] == "ready"),
            "{v}"
        );
        let r = req(
            reqwest::Method::DELETE,
            "/fleet/pools/ci",
            &token,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
    }

    // ---- snapshots ----

    #[tokio::test]
    async fn sim_snapshots_restore_and_clone() {
        let app = SimNode::default().router();
        let linux = json!({"backend": "vz", "name": "l", "apple": {"guest_os": "linux"}});
        let (_, vm) = call(&app, "POST", "/v1/vms", linux).await;
        let id = vm["id"].as_str().unwrap().to_owned();
        let snap = |tag: &str, memory: bool| json!({"tag": tag, "memory": memory});
        let path = |p: &str| format!("/v1/vms/{id}/{p}");
        assert_eq!(
            call(&app, "POST", &path("snapshot"), snap("s1", true))
                .await
                .0,
            StatusCode::OK
        );
        assert!(
            !call(&app, "POST", &path("snapshot"), snap("s1", true))
                .await
                .0
                .is_success()
        );
        call(&app, "POST", &path("stop"), Value::Null).await;
        let (s, v) = call(&app, "POST", &path("snapshot"), snap("s2", true)).await;
        assert!(!s.is_success());
        assert!(v["error"].as_str().unwrap().contains("running or paused"));
        assert_eq!(
            call(&app, "POST", &path("snapshot"), snap("d1", false))
                .await
                .0,
            StatusCode::OK
        );
        let (_, list) = call(&app, "GET", &path("snapshots"), Value::Null).await;
        let items = list["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.get("sim_typed").is_none()));
        let (s, v) = call(&app, "POST", &path("restore"), json!({"tag": "s1"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["status"], "running");
        let (s, v) = call(&app, "POST", &path("clone"), json!({"name": "copy"})).await;
        assert!(
            !s.is_success(),
            "a running VM is cloned only from a snapshot"
        );
        assert!(v["error"].as_str().unwrap().contains("stopped"));
        let (s, v) = call(
            &app,
            "POST",
            &path("clone"),
            json!({"name": "copy", "snapshot": "d1"}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        assert_eq!(v["name"], "copy");
        assert!(
            call(&app, "DELETE", &path("snapshots/d1"), Value::Null)
                .await
                .0
                .is_success()
        );
        assert_eq!(
            call(&app, "DELETE", &path("snapshots/d1"), Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    /// Snapshot, restore and clone through central, against a simulated Mac over real HTTP: names, the per-VM limit,
    /// memory needing a running VM, ownership, quota on clones, the activity log and notifications.
    #[tokio::test]
    async fn snapshots_through_central() {
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
            overlay_config: None,
        }));
        let http = reqwest::Client::new();
        let req = |method: reqwest::Method, path: &str, token: &str, body: Value| {
            let r = http
                .request(method, format!("{central_url}{path}"))
                .bearer_auth(token);
            let r = if body.is_null() { r } else { r.json(&body) };
            r.send()
        };
        use reqwest::Method as M;
        let mut registered = false;
        for _ in 0..100 {
            let v: Value = req(M::GET, "/fleet/nodes", "admin", Value::Null)
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
        let mut tokens = Vec::new();
        for (id, quota) in [
            ("acme", json!({"max_vms": 2, "max_snapshots_per_vm": 2})),
            ("globex", json!({})),
        ] {
            let v: Value = req(
                M::POST,
                "/fleet/tenants",
                "admin",
                json!({"id": id, "quota": quota}),
            )
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
            tokens.push(v["token"].as_str().expect("tenant token").to_owned());
        }
        let (acme, globex) = (tokens[0].clone(), tokens[1].clone());
        let r = req(
            M::POST,
            "/fleet/vms",
            &acme,
            json!({"backend": "vz", "name": "l1", "vcpus": 2, "memory_mib": 2048, "apple": {"guest_os": "linux"}}),
        )
        .await
        .unwrap();
        assert!(r.status().is_success());
        let v: Value = r.json().await.unwrap();
        let id = v["vm"]["id"].as_str().unwrap().to_owned();
        let vm = format!("/fleet/vms/sim-mac/{id}");
        let typed = |text: &str| json!({"actions": [{"action": "type", "text": text}]});
        assert_eq!(
            req(M::POST, &format!("{vm}/input"), &acme, typed("hello"))
                .await
                .unwrap()
                .status(),
            200
        );

        // A snapshot with memory of the running VM.
        let r = req(
            M::POST,
            &format!("{vm}/snapshots"),
            &acme,
            json!({"name": "before", "include_memory": true}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["snapshot"]["name"], "before");
        assert_eq!(v["snapshot"]["memory"], true);
        assert!(v["snapshot"]["size_bytes"].as_u64().unwrap() > 0, "{v}");
        // Names are checked, and unique per VM.
        for (body, want) in [
            (json!({"name": "before"}), 409u16),
            (json!({"name": "../etc"}), 400),
            (json!({"nmae": "typo"}), 400),
        ] {
            let r = req(M::POST, &format!("{vm}/snapshots"), &acme, body.clone())
                .await
                .unwrap();
            assert_eq!(r.status(), want, "{body}");
        }
        // Memory needs a running VM; a disk-only snapshot does not.
        req(M::POST, &format!("{vm}/stop"), &acme, Value::Null)
            .await
            .unwrap();
        let r = req(
            M::POST,
            &format!("{vm}/snapshots"),
            &acme,
            json!({"name": "m2", "include_memory": true}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 409);
        let v: Value = r.json().await.unwrap();
        assert!(
            v["error"].as_str().unwrap().contains("running or paused"),
            "{v}"
        );
        let r = req(M::POST, &format!("{vm}/snapshots"), &acme, Value::Null)
            .await
            .unwrap();
        assert_eq!(r.status(), 201, "a name is made up when none is given");
        let v: Value = r.json().await.unwrap();
        let auto = v["snapshot"]["name"].as_str().unwrap().to_owned();
        assert!(auto.starts_with("snap-"), "{auto}");
        // The tenant's limit is two per VM.
        let r = req(
            M::POST,
            &format!("{vm}/snapshots"),
            &acme,
            json!({"name": "third"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 429);
        let v: Value = req(M::GET, &format!("{vm}/snapshots"), &acme, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["limit"], 2);
        let names: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{v}");
        assert!(names.contains(&"before") && names.contains(&auto.as_str()));

        // Someone else's VM does not exist, as far as they can tell.
        for (m, p) in [
            (M::GET, format!("{vm}/snapshots")),
            (M::POST, format!("{vm}/snapshots/before/restore")),
            (M::DELETE, format!("{vm}/snapshots/before")),
        ] {
            let r = req(m, &p, &globex, Value::Null).await.unwrap();
            assert_eq!(r.status(), 404, "{p}");
        }
        let r = req(
            M::POST,
            &format!("{vm}/clone"),
            &globex,
            json!({"name": "stolen"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 404);

        // Restore: the VM runs again with what it had when the snapshot was taken.
        req(M::POST, &format!("{vm}/start"), &acme, Value::Null)
            .await
            .unwrap();
        req(M::POST, &format!("{vm}/input"), &acme, typed(" world"))
            .await
            .unwrap();
        let r = req(
            M::POST,
            &format!("{vm}/snapshots/before/restore"),
            &acme,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["vm"]["status"], "running");
        assert_eq!(v["vm"]["sim_typed"], "hello");
        let r = req(
            M::POST,
            &format!("{vm}/snapshots/nope/restore"),
            &acme,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 404);

        // Clone: a running VM only from a snapshot; the clone is the tenant's and counts against its quota.
        let r = req(
            M::POST,
            &format!("{vm}/clone"),
            &acme,
            json!({"name": "l1-copy"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 409);
        let r = req(
            M::POST,
            &format!("{vm}/clone"),
            &acme,
            json!({"name": "l1-copy", "snapshot": "before"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 201);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["node"], "sim-mac");
        assert_eq!(v["vm"]["name"], "l1-copy");
        let copy = v["vm"]["id"].as_str().unwrap().to_owned();
        let me: Value = req(M::GET, "/fleet/tenants/me", &acme, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me["in_use"]["vms"], 2, "{me}");
        let r = req(
            M::POST,
            &format!("{vm}/clone"),
            &acme,
            json!({"name": "l1-copy2", "snapshot": "before"}),
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 429, "the clone is over the quota of two VMs");
        // The clone is the tenant's to run like any VM.
        let r = req(
            M::POST,
            &format!("/fleet/vms/sim-mac/{copy}/stop"),
            &acme,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 200);

        // Delete frees the slot.
        let r = req(
            M::DELETE,
            &format!("{vm}/snapshots/{auto}"),
            &acme,
            Value::Null,
        )
        .await
        .unwrap();
        assert_eq!(r.status(), 204);
        let v: Value = req(M::GET, &format!("{vm}/snapshots"), &acme, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 1);

        // The activity log names the snapshot; notifications tell the tenant.
        let v: Value = req(M::GET, "/fleet/events", &acme, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let events = v["items"].as_array().unwrap();
        assert!(
            events.iter().any(|e| e["path"]
                .as_str()
                .unwrap()
                .ends_with("/snapshots/before/restore")
                && e["target"] == "before"
                && e["status"] == 200),
            "{v}"
        );
        assert!(
            events.iter().any(
                |e| e["path"].as_str().unwrap().ends_with("/clone") && e["target"] == "l1-copy"
            ),
            "{v}"
        );
        let v: Value = req(M::GET, "/fleet/notifications", &acme, Value::Null)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for kind in ["vm.snapshot", "vm.restored", "vm.cloned"] {
            assert!(
                v["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["type"] == kind),
                "{kind}: {v}"
            );
        }
        assert!(
            v.to_string().contains("l1 was restored to snapshot before"),
            "{v}"
        );
    }
    // ---- volumes ----

    /// Persistent volumes end to end over real HTTP: a tenant creates one, launches a VM with it (which lands on the
    /// volume's Mac with the volume as a virtio disk), cannot delete, grow or reuse it while attached, deletes the VM,
    /// then grows and deletes the volume, and the file is gone from the Mac.
    #[tokio::test]
    async fn volumes_with_a_simulated_mac() {
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
            overlay_config: None,
        }));
        let http = reqwest::Client::new();
        let call = |method: reqwest::Method, path: &str, token: &str, body: Value| {
            let req = http
                .request(method, format!("{central_url}{path}"))
                .bearer_auth(token)
                .json(&body);
            async move {
                let r = req.send().await.unwrap();
                let status = r.status().as_u16();
                (status, r.json::<Value>().await.unwrap_or(Value::Null))
            }
        };
        use reqwest::Method as M;
        let mut node_url = None;
        for _ in 0..100 {
            let (_, v) = call(M::GET, "/fleet/nodes", "admin", Value::Null).await;
            node_url = v["items"]
                .as_array()
                .and_then(|a| a.iter().find(|n| n["name"] == "sim-mac"))
                .and_then(|n| n["fluxvm_url"].as_str())
                .map(str::to_owned);
            if node_url.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let node_url = node_url.expect("the simulated Mac never registered");
        let mut tokens = Vec::new();
        for t in ["acme", "globex"] {
            let (s, v) = call(M::POST, "/fleet/tenants", "admin", json!({"id": t})).await;
            assert_eq!(s, 201, "{v}");
            tokens.push(v["token"].as_str().unwrap().to_owned());
        }
        let (acme, globex) = (tokens[0].as_str(), tokens[1].as_str());

        // Create.
        let (s, vol) = call(
            M::POST,
            "/fleet/volumes",
            acme,
            json!({"name": "data", "size_gib": 10}),
        )
        .await;
        assert_eq!(s, 201, "{vol}");
        assert_eq!(
            (&vol["node"], &vol["status"], &vol["size_gib"]),
            (&json!("sim-mac"), &json!("available"), &json!(10))
        );
        assert!(vol.get("path").is_none(), "tenants do not see host paths");
        let id = vol["id"].as_str().unwrap().to_owned();
        let (s, _) = call(
            M::POST,
            "/fleet/volumes",
            acme,
            json!({"name": "data", "size_gib": 1}),
        )
        .await;
        assert_eq!(s, 409, "one name per tenant");
        let (s, _) = call(
            M::POST,
            "/fleet/volumes",
            acme,
            json!({"name": "big", "size_gib": 4096}),
        )
        .await;
        assert_eq!(s, 400, "at most 2048 GiB");
        // Another tenant sees nothing of it.
        let (_, v) = call(M::GET, "/fleet/volumes", globex, Value::Null).await;
        assert_eq!(v["items"].as_array().unwrap().len(), 0, "{v}");
        let (s, _) = call(M::GET, &format!("/fleet/volumes/{id}"), globex, Value::Null).await;
        assert_eq!(s, 404);
        let (s, _) = call(
            M::DELETE,
            &format!("/fleet/volumes/{id}"),
            globex,
            Value::Null,
        )
        .await;
        assert_eq!(s, 404);

        // Launch with it: the VM lands on the volume's Mac with the volume as a virtio disk.
        let linux = |name: &str| json!({"name": name, "backend": "vz", "apple": {"guest_os": "linux"}, "volumes": ["data"]});
        let (s, v) = call(M::POST, "/fleet/vms", acme, linux("db")).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["node"], "sim-mac");
        let vm_id = v["vm"]["id"].as_str().unwrap().to_owned();
        let disk = &v["vm"]["request"]["apple"]["extra_disks"][0];
        assert_eq!(disk["block_device_id"], json!(id), "{v}");
        assert!(v["vm"]["request"].get("volumes").is_none(), "not forwarded");
        let (_, v) = call(M::GET, "/fleet/volumes/data", acme, Value::Null).await;
        assert_eq!(
            (&v["status"], &v["attached_to"]),
            (&json!("attached"), &json!(vm_id))
        );

        // While attached: no delete, no resize, no second VM.
        let (s, v) = call(M::DELETE, "/fleet/volumes/data", acme, Value::Null).await;
        assert_eq!(s, 409, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("delete the VM first"),
            "{v}"
        );
        let (s, _) = call(
            M::POST,
            "/fleet/volumes/data/resize",
            acme,
            json!({"size_gib": 20}),
        )
        .await;
        assert_eq!(s, 409);
        let (s, _) = call(M::POST, "/fleet/vms", acme, linux("db2")).await;
        assert_eq!(s, 409);
        // A macOS guest, an unknown volume and a different Mac are refused before anything is created.
        let (s, _) = call(
            M::POST,
            "/fleet/vms",
            acme,
            json!({"name": "m", "backend": "vz", "apple": {"guest_os": "macos"}, "volumes": ["data"]}),
        )
        .await;
        assert_eq!(s, 400);
        let (s, _) = call(
            M::POST,
            "/fleet/vms",
            acme,
            json!({"name": "x", "backend": "vz", "volumes": ["nope"]}),
        )
        .await;
        assert_eq!(s, 404);
        // Naming the file directly would skip the rules.
        let (_, all) = call(M::GET, "/fleet/volumes", "admin", Value::Null).await;
        let path = all["items"][0]["path"].as_str().unwrap().to_owned();
        let (s, _) = call(
            M::POST,
            "/fleet/vms",
            acme,
            json!({"name": "sneaky", "backend": "vz", "apple": {"extra_disks": [{"path": path}]}}),
        )
        .await;
        assert_eq!(s, 400);
        // So would any other host path: a tenant names no extra disks at all.
        let (s, _) = call(
            M::POST,
            "/fleet/vms",
            acme,
            json!({"name": "sneaky", "backend": "vz", "apple": {"extra_disks": [{"path": "/etc/master.passwd"}]}}),
        )
        .await;
        assert_eq!(s, 400);

        // Deleting the VM detaches it; the data stays.
        let (s, _) = call(
            M::DELETE,
            &format!("/fleet/vms/sim-mac/{vm_id}"),
            acme,
            Value::Null,
        )
        .await;
        assert_eq!(s, 204);
        let (_, v) = call(M::GET, "/fleet/volumes/data", acme, Value::Null).await;
        assert_eq!(
            (&v["status"], &v["attached_to"]),
            (&json!("available"), &Value::Null)
        );

        // Grow only, then delete; the file leaves the Mac.
        let (s, v) = call(
            M::POST,
            "/fleet/volumes/data/resize",
            acme,
            json!({"size_gib": 20}),
        )
        .await;
        assert_eq!((s, &v["size_gib"]), (200, &json!(20)), "{v}");
        let (s, _) = call(
            M::POST,
            "/fleet/volumes/data/resize",
            acme,
            json!({"size_gib": 5}),
        )
        .await;
        assert_eq!(s, 400);
        let (_, me) = call(M::GET, "/fleet/tenants/me", acme, Value::Null).await;
        assert_eq!(me["in_use"]["volume_gib"], 20, "{me}");
        let (s, _) = call(M::DELETE, "/fleet/volumes/data", acme, Value::Null).await;
        assert_eq!(s, 204);
        let (s, _) = call(M::GET, "/fleet/volumes/data", acme, Value::Null).await;
        assert_eq!(s, 404);
        let on_mac: Value = reqwest::get(format!("{node_url}/v1/volumes"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(on_mac["items"].as_array().unwrap().len(), 0, "{on_mac}");

        // Every change was recorded, and the tenant was told.
        let (_, ev) = call(M::GET, "/fleet/events", acme, Value::Null).await;
        assert!(
            ev["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["path"] == "/fleet/volumes" && e["target"] == "data"),
            "{ev}"
        );
        let (_, n) = call(M::GET, "/fleet/notifications", acme, Value::Null).await;
        assert!(n.to_string().contains("volume.deleted"), "{n}");
    }
}
