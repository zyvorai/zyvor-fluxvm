// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Fleet metrics: a collector that samples every healthy node's `GET /v1/stats`, a bounded in-memory history per VM
//! and per node, the graphs' API and a Prometheus endpoint.
//!
//! * The collector polls every [`DEFAULT_INTERVAL`] (`--metrics-interval-secs`), at most `FLUXVM_POLL_CONCURRENCY` (default 64)
//!   nodes at a time, each with a [`POLL_TIMEOUT`] and a [`MAX_BODY`] cap. Only the HA leader collects.
//! * Nodes report cumulative counters; rates are taken between two consecutive samples of the same node, so the
//!   first sample after a node (or VM) appears has no CPU or I/O rate yet. A counter that goes backwards (a VM
//!   restarted, a runner replaced) yields no rate for that interval rather than a negative one.
//! * Samples are averaged into one point per minute and kept for 24 hours ([`KEEP_POINTS`]). History is memory
//!   only: it starts empty when central restarts, and Prometheus (`GET /metrics`) is the place for long-term storage.
//! * Memory is bounded: at most [`MAX_VM_SERIES`] VM series and [`MAX_NODE_SERIES`] node series (about 50 KiB per
//!   full series); a VM that disappears from its node's sample loses its series at once, and a node that is
//!   deregistered loses its series and its VMs'.
//!
//! Routes: `GET /fleet/metrics/summary`, `GET /fleet/metrics/vms`, `GET /fleet/metrics/vms/{node}/{id}`,
//! `GET /fleet/metrics/nodes/{name}`, `POST /fleet/metrics/collect` (operator) and `GET /metrics` (operator token or
//! `--metrics-token`).

use super::*;
use axum::extract::{MatchedPath, Query};
use fluxvm_scheduler::node_stats::{HostStats, NodeStats, VmStats};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// How often the collector samples every node.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);
/// Resolution of the kept history.
const STEP_SECS: i64 = 60;
/// 24 hours of one-minute points.
const KEEP_POINTS: usize = 24 * 60;
const KEEP_SECS: i64 = KEEP_POINTS as i64 * STEP_SECS;
pub(super) const MAX_VM_SERIES: usize = 1000;
pub(super) const MAX_NODE_SERIES: usize = 500;

/// The series caps, raised or lowered with `FLUXVM_METRICS_MAX_NODE_SERIES` / `FLUXVM_METRICS_MAX_VM_SERIES`. Each
/// series costs about 50 KiB of history, so 10,000 node series is about 500 MiB: raise it only with the memory to match.
/// Series past the cap are counted in `fluxvm_central_metrics_series_dropped_total`.
fn cap(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}
fn max_node_series() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| cap("FLUXVM_METRICS_MAX_NODE_SERIES", MAX_NODE_SERIES))
}
fn max_vm_series() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| cap("FLUXVM_METRICS_MAX_VM_SERIES", MAX_VM_SERIES))
}
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest `/v1/stats` body accepted from a node.
const MAX_BODY: usize = 4 << 20;
/// Distinct (method, route, status) request counters before new ones fold into `route="other"`.
const MAX_REQUEST_SERIES: usize = 2048;
const MAX_ID_LEN: usize = 128;

/// The six per-VM measures, in this order everywhere.
const VM_FIELDS: [&str; 6] = [
    "cpu_percent",
    "memory_used_mib",
    "disk_read_bps",
    "disk_write_bps",
    "net_rx_bps",
    "net_tx_bps",
];
/// The six per-node measures.
const NODE_FIELDS: [&str; 6] = [
    "cpu_percent",
    "memory_used_mib",
    "memory_total_mib",
    "disk_free_bytes",
    "vms",
    "vms_running",
];

type Vals = [Option<f64>; 6];

/// Collector settings `fluxvm-agent central` passes before building the router (`--metrics-token`,
/// `--metrics-interval-secs`). Routers built without it use the defaults and no separate scrape token.
#[derive(Debug, Clone, Default)]
pub struct MetricsSettings {
    pub interval: Option<Duration>,
    /// A token that may only read `GET /metrics`, for a Prometheus server that should not hold the fleet token.
    pub token: Option<String>,
}

static SETTINGS: std::sync::Mutex<Option<MetricsSettings>> = std::sync::Mutex::new(None);

/// Sets the collector settings for routers built after this call.
pub fn configure(s: MetricsSettings) {
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- the series ----------------------------------------------------------------------------------------------------

/// The samples of the minute being filled.
#[derive(Debug, Clone, Default)]
struct Bucket {
    minute: i64,
    sum: [f64; 6],
    n: [u32; 6],
}

impl Bucket {
    fn add(&mut self, v: &Vals) {
        for (i, x) in v.iter().enumerate() {
            if let Some(x) = x.filter(|x| x.is_finite()) {
                self.sum[i] += x;
                self.n[i] += 1;
            }
        }
    }

    fn mean(&self) -> [f32; 6] {
        let mut out = [f32::NAN; 6];
        for (i, o) in out.iter_mut().enumerate() {
            if self.n[i] > 0 {
                *o = (self.sum[i] / f64::from(self.n[i])) as f32;
            }
        }
        out
    }
}

/// One minute-resolution series: closed minutes (NaN = no value), the open minute and the latest raw values.
#[derive(Debug, Default)]
struct Series {
    points: VecDeque<(i64, [f32; 6])>,
    open: Option<Bucket>,
    latest: Option<(i64, Vals)>,
}

impl Series {
    fn record(&mut self, ts: i64, v: Vals) {
        let minute = ts.div_euclid(STEP_SECS) * STEP_SECS;
        match &mut self.open {
            Some(b) if b.minute == minute => b.add(&v),
            _ => {
                if let Some(b) = self.open.take() {
                    self.close(b.minute, b.mean());
                }
                let mut b = Bucket {
                    minute,
                    ..Default::default()
                };
                b.add(&v);
                self.open = Some(b);
            }
        }
        self.latest = Some((ts, v));
    }

    fn close(&mut self, minute: i64, vals: [f32; 6]) {
        // A clock that stepped backwards must not reorder the series.
        if self.points.back().is_some_and(|(t, _)| *t >= minute) {
            return;
        }
        self.points.push_back((minute, vals));
        while self.points.len() > KEEP_POINTS
            || self
                .points
                .front()
                .is_some_and(|(t, _)| *t <= minute - KEEP_SECS)
        {
            self.points.pop_front();
        }
    }

    /// Points from `since` on, oldest first, the open minute last.
    fn window(&self, since: i64) -> Vec<(i64, [f32; 6])> {
        let mut out: Vec<(i64, [f32; 6])> = self
            .points
            .iter()
            .filter(|(t, _)| *t >= since)
            .copied()
            .collect();
        if let Some(b) = &self.open
            && b.minute >= since
            && out.last().is_none_or(|(t, _)| *t < b.minute)
        {
            out.push((b.minute, b.mean()));
        }
        out
    }
}

/// Counters of the previous sample, for rates.
#[derive(Debug, Clone, Default)]
struct VmRaw {
    at: f64,
    cpu: Option<f64>,
    dr: Option<u64>,
    dw: Option<u64>,
    rx: Option<u64>,
    tx: Option<u64>,
}

#[derive(Debug)]
struct VmSeries {
    series: Series,
    raw: Option<VmRaw>,
    last: VmStats,
}

#[derive(Debug)]
struct NodeSeries {
    series: Series,
    raw: Option<(f64, Option<u64>, Option<u64>)>,
    last: HostStats,
}

#[derive(Default)]
struct Inner {
    nodes: HashMap<String, NodeSeries>,
    vms: HashMap<(String, String), VmSeries>,
}

/// Everything the metrics feature keeps (one per central).
pub(super) struct MetricsStore {
    pub(super) interval: Duration,
    token: Option<String>,
    inner: std::sync::Mutex<Inner>,
    polls_ok: AtomicU64,
    polls_failed: AtomicU64,
    series_dropped: AtomicU64,
    requests: std::sync::Mutex<BTreeMap<(String, String, u16), u64>>,
    last_collect: std::sync::Mutex<Option<DateTime<Utc>>>,
}

impl Default for MetricsStore {
    fn default() -> Self {
        Self::with(MetricsSettings::default())
    }
}

fn rate(prev: Option<u64>, cur: Option<u64>, dt: f64) -> Option<f64> {
    let (p, c) = (prev?, cur?);
    (c >= p && dt > 0.0).then(|| (c - p) as f64 / dt)
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

impl MetricsStore {
    fn with(s: MetricsSettings) -> Self {
        Self {
            interval: s
                .interval
                .unwrap_or(DEFAULT_INTERVAL)
                .max(Duration::from_secs(1)),
            token: s.token.filter(|t| !t.is_empty()),
            inner: Default::default(),
            polls_ok: AtomicU64::new(0),
            polls_failed: AtomicU64::new(0),
            series_dropped: AtomicU64::new(0),
            requests: Default::default(),
            last_collect: Default::default(),
        }
    }

    /// With the settings given to [`configure`], or the defaults.
    pub(super) fn configured() -> Self {
        Self::with(lock(&SETTINGS).clone().unwrap_or_default())
    }

    /// Takes one node's sample received at `at` (Unix seconds, fractional).
    fn ingest(&self, node: &str, s: &NodeStats, at: f64) {
        let ts = at.floor() as i64;
        let mut g = lock(&self.inner);
        let g = &mut *g;
        if g.nodes.contains_key(node) || g.nodes.len() < max_node_series() {
            let e = g
                .nodes
                .entry(node.to_owned())
                .or_insert_with(|| NodeSeries {
                    series: Series::default(),
                    raw: None,
                    last: HostStats::default(),
                });
            let h = &s.host;
            let cpu = e.raw.and_then(|(_, pb, pt)| {
                let db = h.cpu_busy_ticks?.checked_sub(pb?)?;
                let dt = h.cpu_total_ticks?.checked_sub(pt?)?;
                (dt > 0).then(|| (db as f64 / dt as f64 * 100.0).clamp(0.0, 100.0))
            });
            let used = match (h.memory_total_mib, h.memory_available_mib) {
                (Some(t), Some(a)) => Some(t.saturating_sub(a) as f64),
                _ => None,
            };
            let vals = [
                cpu,
                used,
                h.memory_total_mib.map(|x| x as f64),
                h.disk_free_bytes.map(|x| x as f64),
                Some(f64::from(h.vm_count)),
                Some(f64::from(h.vms_running)),
            ];
            e.series.record(ts, vals);
            e.raw = Some((at, h.cpu_busy_ticks, h.cpu_total_ticks));
            e.last = h.clone();
        } else {
            self.series_dropped.fetch_add(1, Ordering::Relaxed);
        }

        // A VM missing from its node's sample was deleted (or moved): its series goes now.
        let present: HashSet<&str> = s.vms.iter().map(|v| v.id.as_str()).collect();
        g.vms
            .retain(|(n, id), _| n != node || present.contains(id.as_str()));
        for v in &s.vms {
            if v.id.is_empty() || v.id.len() > MAX_ID_LEN {
                continue;
            }
            let key = (node.to_owned(), v.id.clone());
            if !g.vms.contains_key(&key) && g.vms.len() >= max_vm_series() {
                self.series_dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let e = g.vms.entry(key).or_insert_with(|| VmSeries {
                series: Series::default(),
                raw: None,
                last: VmStats::default(),
            });
            let (cpu, dr, dw, rx, tx) = match &e.raw {
                Some(p) => {
                    let dt = at - p.at;
                    let cpu = match (p.cpu, v.cpu_seconds_total) {
                        (Some(a), Some(b)) if b >= a && dt > 0.0 => Some(
                            ((b - a) / dt / f64::from(v.vcpus.max(1)) * 100.0).clamp(0.0, 100.0),
                        ),
                        _ => None,
                    };
                    (
                        cpu,
                        rate(p.dr, v.disk_read_bytes_total, dt),
                        rate(p.dw, v.disk_write_bytes_total, dt),
                        rate(p.rx, v.net_rx_bytes_total, dt),
                        rate(p.tx, v.net_tx_bytes_total, dt),
                    )
                }
                None => (None, None, None, None, None),
            };
            let mem = v.memory_used_bytes.map(|b| b as f64 / 1_048_576.0);
            e.series.record(ts, [cpu, mem, dr, dw, rx, tx]);
            e.raw = Some(VmRaw {
                at,
                cpu: v.cpu_seconds_total,
                dr: v.disk_read_bytes_total,
                dw: v.disk_write_bytes_total,
                rx: v.net_rx_bytes_total,
                tx: v.net_tx_bytes_total,
            });
            let mut last = v.clone();
            last.name = clip(&last.name, 256);
            last.status = clip(&last.status, 32);
            last.backend = clip(&last.backend, 32);
            last.source = clip(&last.source, 32);
            e.last = last;
        }
    }

    /// Drops the series of nodes that are no longer registered, and of their VMs.
    fn retain_nodes(&self, keep: &HashSet<String>) {
        let mut g = lock(&self.inner);
        g.nodes.retain(|n, _| keep.contains(n));
        g.vms.retain(|(n, _), _| keep.contains(n));
    }

    fn count_request(&self, method: &str, route: String, status: u16) {
        let method = match method {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" => method,
            _ => "OTHER",
        };
        let mut r = lock(&self.requests);
        let mut key = (method.to_owned(), route, status);
        if !r.contains_key(&key) && r.len() >= MAX_REQUEST_SERIES {
            key.1 = "other".into();
        }
        *r.entry(key).or_insert(0) += 1;
    }

    fn series_counts(&self) -> (usize, usize) {
        let g = lock(&self.inner);
        (g.nodes.len(), g.vms.len())
    }
}

// ---- collecting ----------------------------------------------------------------------------------------------------

/// Starts the collector (needs a Tokio runtime). It samples only while this central leads.
pub(super) fn spawn_collector(fleet: Fleet) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(fleet.metrics.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if !fleet.leading() {
                continue;
            }
            collect_once(&fleet).await;
        }
    });
}

async fn fetch_stats(http: &reqwest::Client, url: &str) -> Result<NodeStats, String> {
    let work = async {
        let mut resp = http
            .get(format!("{url}/v1/stats"))
            .timeout(POLL_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("node answered {}", resp.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err("stats response too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<NodeStats>(&body).map_err(|e| format!("bad stats: {e}"))
    };
    tokio::time::timeout(POLL_TIMEOUT + Duration::from_secs(1), work)
        .await
        .unwrap_or_else(|_| Err("timed out".into()))
}

/// Samples every healthy node once; returns (polled, failed).
pub(super) async fn collect_once(fleet: &Fleet) -> (usize, usize) {
    let (targets, registered): (Vec<(String, String)>, HashSet<String>) = {
        let nodes = fleet.nodes.lock().await;
        (
            nodes
                .values()
                .filter(|n| n.healthy())
                .map(|n| (n.name.clone(), n.fluxvm_url.clone()))
                .collect(),
            nodes.keys().cloned().collect(),
        )
    };
    fleet.metrics.retain_nodes(&registered);
    let results = super::registry::bounded(targets, |(name, url)| {
        let http = fleet.http.clone();
        async move { (name, fetch_stats(&http, &url).await) }
    })
    .await;
    let (mut polled, mut failed) = (0, 0);
    for (name, r) in results {
        polled += 1;
        match r {
            Ok(s) => {
                let at = Utc::now().timestamp_millis() as f64 / 1000.0;
                fleet.metrics.ingest(&name, &s, at);
                fleet.metrics.polls_ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                failed += 1;
                fleet.metrics.polls_failed.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(node = %name, error = %e, "metrics poll failed");
            }
        }
    }
    *lock(&fleet.metrics.last_collect) = Some(Utc::now());
    (polled, failed)
}

/// `POST /fleet/metrics/collect` (operator): sample every node now instead of waiting for the next tick.
pub(super) async fn collect_now(State(fleet): State<Fleet>) -> Json<Value> {
    let (polled, failed) = collect_once(&fleet).await;
    Json(json!({"polled": polled, "failed": failed}))
}

/// Counts every request by method, matched route and status for `fluxvm_central_http_requests_total`.
pub(super) async fn count_requests(
    State(fleet): State<Fleet>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().as_str().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".into());
    let resp = next.run(req).await;
    fleet
        .metrics
        .count_request(&method, route, resp.status().as_u16());
    resp
}

// ---- the graphs' API -----------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct RangeQuery {
    range: Option<String>,
}

/// `1h` (default), `6h` or `24h`, in seconds.
fn parse_range(r: Option<&str>) -> Result<(i64, &'static str), AppError> {
    match r.unwrap_or("1h") {
        "1h" => Ok((3600, "1h")),
        "6h" => Ok((6 * 3600, "6h")),
        "24h" | "1d" => Ok((24 * 3600, "24h")),
        other => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("range must be 1h, 6h or 24h, not '{}'", clip(other, 16)),
        )),
    }
}

fn round(x: f64, places: i32) -> f64 {
    let p = 10f64.powi(places);
    (x * p).round() / p
}

/// One point as JSON: `t` (Unix seconds, start of the minute) and each field, `null` where nothing was measured.
fn point_json(t: i64, vals: &[f32; 6], fields: &[&str; 6]) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("t".into(), json!(t));
    for (i, f) in fields.iter().enumerate() {
        let v = vals[i];
        let places = if f.ends_with("percent") || f.ends_with("mib") {
            1
        } else {
            0
        };
        o.insert(
            (*f).into(),
            if v.is_finite() {
                json!(round(f64::from(v), places))
            } else {
                Value::Null
            },
        );
    }
    Value::Object(o)
}

fn latest_json(latest: &Option<(i64, Vals)>, fields: &[&str; 6]) -> Value {
    match latest {
        Some((t, v)) => {
            let f: [f32; 6] = std::array::from_fn(|i| v[i].map_or(f32::NAN, |x| x as f32));
            point_json(*t, &f, fields)
        }
        None => Value::Null,
    }
}

/// What a VM's graphs cannot show, and why.
fn vm_note(s: &VmStats) -> Option<&'static str> {
    match s.source.as_str() {
        "vz-runner" => Some(
            "The Virtualization service that runs this guest could not be found, so CPU and memory are the runner process only and do not include the guest.",
        ),
        "vz-process" => Some(
            "Network traffic is not measured for VMs on Apple's Virtualization framework: it has no per-VM network counters.",
        ),
        "cgroup" if s.net_rx_bytes_total.is_none() => Some(
            "Network traffic is not measured: this VM has no network device in the host's namespace.",
        ),
        "none" | "" => Some(
            "Nothing is measured while the VM is stopped, or when its Mac cannot read its usage.",
        ),
        _ => None,
    }
}

fn check_id(what: &str, s: &str) -> Result<(), AppError> {
    if s.is_empty() || s.len() > MAX_ID_LEN || s.chars().any(char::is_control) {
        return Err(AppError(StatusCode::BAD_REQUEST, format!("bad {what}")));
    }
    Ok(())
}

/// `GET /fleet/metrics/vms/{node}/{id}?range=1h|6h|24h`: one VM's graphs. A tenant sees only its own VMs (anything
/// else is a 404, as everywhere).
pub(super) async fn vm_series(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Value>, AppError> {
    let (secs, label) = parse_range(q.range.as_deref())?;
    check_id("node name", &node)?;
    check_id("VM id", &id)?;
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no VM '{id}' on node '{node}'"),
        ));
    }
    let since = Utc::now().timestamp() - secs;
    let g = lock(&fleet.metrics.inner);
    let base = json!({
        "node": node,
        "id": id,
        "range": label,
        "step_seconds": STEP_SECS,
        "interval_seconds": fleet.metrics.interval.as_secs(),
        "fields": VM_FIELDS,
    });
    let Some(e) = g.vms.get(&(node.clone(), id.clone())) else {
        let mut v = base;
        v["points"] = json!([]);
        v["latest"] = Value::Null;
        v["note"] = json!(
            "No samples yet. Central samples every Mac every few seconds; graphs appear within a minute."
        );
        return Ok(Json(v));
    };
    let mut v = base;
    v["name"] = json!(e.last.name);
    v["status"] = json!(e.last.status);
    v["backend"] = json!(e.last.backend);
    v["vcpus"] = json!(e.last.vcpus);
    v["memory_mib"] = json!(e.last.memory_mib);
    v["source"] = json!(e.last.source);
    v["note"] = json!(vm_note(&e.last));
    v["latest"] = latest_json(&e.series.latest, &VM_FIELDS);
    v["points"] = Value::Array(
        e.series
            .window(since)
            .iter()
            .map(|(t, vals)| point_json(*t, vals, &VM_FIELDS))
            .collect(),
    );
    Ok(Json(v))
}

/// `GET /fleet/metrics/nodes/{name}?range=`: one Mac's graphs. Operators see every Mac; a tenant only the Macs it
/// leases now.
pub(super) async fn node_series(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Value>, AppError> {
    let (secs, label) = parse_range(q.range.as_deref())?;
    check_id("node name", &name)?;
    if let Caller::Tenant(t) = &caller
        && !fleet
            .tenancy
            .lock()
            .await
            .active_lease_for_node(&name)
            .is_some_and(|l| l.tenant == *t)
    {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("you do not lease a Mac named '{name}'"),
        ));
    }
    if !fleet.nodes.lock().await.contains_key(&name) {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no registered node named '{name}'"),
        ));
    }
    let since = Utc::now().timestamp() - secs;
    let g = lock(&fleet.metrics.inner);
    let mut v = json!({
        "node": name,
        "range": label,
        "step_seconds": STEP_SECS,
        "interval_seconds": fleet.metrics.interval.as_secs(),
        "fields": NODE_FIELDS,
        "points": [],
        "latest": null,
    });
    if let Some(e) = g.nodes.get(&name) {
        v["cpus"] = json!(e.last.cpus);
        v["memory_total_mib"] = json!(e.last.memory_total_mib);
        v["disk_total_bytes"] = json!(e.last.disk_total_bytes);
        v["latest"] = latest_json(&e.series.latest, &NODE_FIELDS);
        v["points"] = Value::Array(
            e.series
                .window(since)
                .iter()
                .map(|(t, vals)| point_json(*t, vals, &NODE_FIELDS))
                .collect(),
        );
    } else {
        v["note"] = json!("No samples yet. Graphs appear within a minute of the Mac reporting.");
    }
    Ok(Json(v))
}

/// The VM keys and node names a caller may see, from the ledger and leases (`None`: everything).
async fn visible(
    fleet: &Fleet,
    caller: &Caller,
) -> Option<(HashSet<(String, String)>, HashSet<String>)> {
    let Caller::Tenant(t) = caller else {
        return None;
    };
    let tenancy = fleet.tenancy.lock().await;
    let vms = tenancy
        .vms
        .iter()
        .filter(|v| v.active() && v.tenant.as_deref() == Some(t.as_str()))
        .map(|v| (v.node.clone(), v.vm_id.clone()))
        .collect();
    let nodes = tenancy
        .leases
        .iter()
        .filter(|l| l.state == crate::tenancy::LeaseState::Active && l.tenant == *t)
        .map(|l| l.node.clone())
        .collect();
    Some((vms, nodes))
}

fn vm_row(node: &str, id: &str, e: &VmSeries) -> Value {
    let mut row = latest_json(&e.series.latest, &VM_FIELDS);
    if row.is_null() {
        row = json!({});
    }
    row["node"] = json!(node);
    row["id"] = json!(id);
    row["name"] = json!(e.last.name);
    row["status"] = json!(e.last.status);
    row["vcpus"] = json!(e.last.vcpus);
    row["memory_mib"] = json!(e.last.memory_mib);
    row["source"] = json!(e.last.source);
    row
}

/// `GET /fleet/metrics/vms`: the latest sample of every VM the caller may see (what `fluxctl fleet top` shows).
pub(super) async fn vm_latest(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let scope = visible(&fleet, &caller).await;
    let g = lock(&fleet.metrics.inner);
    let mut items: Vec<Value> = g
        .vms
        .iter()
        .filter(|((n, id), _)| {
            scope
                .as_ref()
                .is_none_or(|(vms, _)| vms.contains(&(n.clone(), id.clone())))
        })
        .map(|((n, id), e)| vm_row(n, id, e))
        .collect();
    items.sort_by(|a, b| {
        a["name"]
            .as_str()
            .cmp(&b["name"].as_str())
            .then(a["node"].as_str().cmp(&b["node"].as_str()))
    });
    let at = *lock(&fleet.metrics.last_collect);
    Json(json!({
        "items": items,
        "interval_seconds": fleet.metrics.interval.as_secs(),
        "collected_at": at,
    }))
}

/// `GET /fleet/metrics/summary`: utilisation now and over the last hour. Operators get the whole fleet; a tenant its
/// leased Macs and its own VMs.
pub(super) async fn summary(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let scope = visible(&fleet, &caller).await;
    let g = lock(&fleet.metrics.inner);
    let node_ok = |n: &str| scope.as_ref().is_none_or(|(_, nodes)| nodes.contains(n));
    let since = Utc::now().timestamp() - 3600;
    let (mut cpus, mut busy_cpus, mut cpu_known) = (0f64, 0f64, 0f64);
    let (mut mem_used, mut mem_total, mut disk_free, mut nodes) = (0f64, 0f64, 0f64, 0usize);
    let mut hist: BTreeMap<i64, [f64; 4]> = BTreeMap::new();
    for (_, e) in g.nodes.iter().filter(|(n, _)| node_ok(n.as_str())) {
        nodes += 1;
        let c = f64::from(e.last.cpus.max(1));
        cpus += c;
        if let Some((_, v)) = &e.series.latest {
            if let Some(p) = v[0] {
                busy_cpus += p / 100.0 * c;
                cpu_known += c;
            }
            mem_used += v[1].unwrap_or(0.0);
            mem_total += v[2].unwrap_or(0.0);
            disk_free += v[3].unwrap_or(0.0);
        }
        for (t, vals) in e.series.window(since) {
            let h = hist.entry(t).or_default();
            if vals[0].is_finite() {
                h[0] += f64::from(vals[0]) / 100.0 * c;
                h[1] += c;
            }
            if vals[1].is_finite() && vals[2].is_finite() && vals[2] > 0.0 {
                h[2] += f64::from(vals[1]);
                h[3] += f64::from(vals[2]);
            }
        }
    }
    let (mut vms, mut running, mut vm_cpu, mut vm_mem) = (0usize, 0usize, 0f64, 0f64);
    for ((n, id), e) in g.vms.iter() {
        let ok = match &scope {
            None => true,
            Some((v, _)) => v.contains(&(n.clone(), id.clone())),
        };
        if !ok {
            continue;
        }
        vms += 1;
        if e.last.status == "running" {
            running += 1;
        }
        if let Some((_, v)) = &e.series.latest {
            vm_cpu += v[0].unwrap_or(0.0) / 100.0 * f64::from(e.last.vcpus.max(1));
            vm_mem += v[1].unwrap_or(0.0);
        }
    }
    let pct = |a: f64, b: f64| (b > 0.0).then(|| round(a / b * 100.0, 1));
    let points: Vec<Value> = hist
        .iter()
        .map(|(t, h)| {
            json!({
                "t": t,
                "cpu_percent": pct(h[0], h[1]),
                "memory_percent": pct(h[2], h[3]),
            })
        })
        .collect();
    let at = *lock(&fleet.metrics.last_collect);
    Json(json!({
        "scope": if scope.is_some() { "tenant" } else { "fleet" },
        "collected_at": at,
        "interval_seconds": fleet.metrics.interval.as_secs(),
        "nodes_reporting": nodes,
        "cpus": cpus,
        "cpu_percent": pct(busy_cpus, cpu_known),
        "memory_used_mib": round(mem_used, 0),
        "memory_total_mib": round(mem_total, 0),
        "memory_percent": pct(mem_used, mem_total),
        "disk_free_bytes": disk_free,
        "vms": vms,
        "vms_running": running,
        "vm_vcpus_busy": round(vm_cpu, 2),
        "vm_memory_used_mib": round(vm_mem, 0),
        "step_seconds": STEP_SECS,
        "points": points,
    }))
}

// ---- Prometheus ----------------------------------------------------------------------------------------------------

/// A label value in the text exposition format: backslash, double quote and newline escaped.
fn esc_label(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o
}

fn fmt_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.into()
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Builds one exposition: each family's HELP and TYPE once, then its samples.
#[derive(Default)]
struct Prom {
    out: String,
}

impl Prom {
    fn family(&mut self, name: &str, kind: &str, help: &str) {
        let _ = writeln!(self.out, "# HELP {name} {help}");
        let _ = writeln!(self.out, "# TYPE {name} {kind}");
    }

    fn sample(&mut self, name: &str, labels: &[(&str, &str)], v: f64) {
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (i, (k, val)) in labels.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                let _ = write!(self.out, "{k}=\"{}\"", esc_label(val));
            }
            self.out.push('}');
        }
        let _ = writeln!(self.out, " {}", fmt_value(v));
    }

    /// A family with one sample per row.
    fn rows<'a>(
        &mut self,
        name: &str,
        kind: &str,
        help: &str,
        rows: impl IntoIterator<Item = (Vec<(&'a str, &'a str)>, f64)>,
    ) {
        self.family(name, kind, help);
        for (labels, v) in rows {
            self.sample(name, &labels, v);
        }
    }

    fn one(&mut self, name: &str, kind: &str, help: &str, v: f64) {
        self.family(name, kind, help);
        self.sample(name, &[], v);
    }
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
}

/// `GET /metrics`: the fleet in the Prometheus text format. Needs the fleet token or `--metrics-token`; tenant tokens
/// are refused. Without either token (lab mode) it is open until tenants exist, like the rest of central.
pub(super) async fn prometheus(
    State(fleet): State<Fleet>,
    headers: axum::http::HeaderMap,
) -> Response {
    let presented = bearer(&headers);
    let is = |want: Option<&str>| matches!((want, presented), (Some(w), Some(p)) if constant_time_eq(p, w));
    let allowed = is(fleet.token.as_deref())
        || is(fleet.metrics.token.as_deref())
        || (fleet.token.is_none()
            && fleet.metrics.token.is_none()
            && !fleet.tenancy.lock().await.enabled());
    if !allowed {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": "GET /metrics needs the fleet token or the --metrics-token"})),
        )
            .into_response();
    }
    let body = render(&fleet).await;
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

struct TenantRow {
    id: String,
    disabled: bool,
    vms: u64,
    vcpus: u64,
    memory_mib: u64,
    leases: u64,
}

async fn render(fleet: &Fleet) -> String {
    // Lock order: tenancy, then nodes; the metrics store's own lock is never held across an await.
    let (tenants, ledger, leases_active, scrub, draining) = {
        let t = fleet.tenancy.lock().await;
        let mut rows: BTreeMap<String, TenantRow> = t
            .tenants
            .values()
            .map(|x| {
                (
                    x.id.clone(),
                    TenantRow {
                        id: x.id.clone(),
                        disabled: x.disabled,
                        vms: 0,
                        vcpus: 0,
                        memory_mib: 0,
                        leases: 0,
                    },
                )
            })
            .collect();
        let mut ledger: HashMap<(String, String), String> = HashMap::new();
        for v in t.vms.iter().filter(|v| v.active()) {
            if let Some(id) = &v.tenant {
                ledger.insert((v.node.clone(), v.vm_id.clone()), id.clone());
                if let Some(r) = rows.get_mut(id) {
                    r.vms += 1;
                    r.vcpus += u64::from(v.vcpus);
                    r.memory_mib += v.memory_mib;
                }
            }
        }
        let mut active = 0u64;
        for l in &t.leases {
            if l.state == crate::tenancy::LeaseState::Active {
                active += 1;
                if let Some(r) = rows.get_mut(&l.tenant) {
                    r.leases += 1;
                }
            }
        }
        (
            rows.into_values().collect::<Vec<_>>(),
            ledger,
            active,
            t.scrub_pending.len(),
            t.draining.len(),
        )
    };
    let nodes: Vec<NodeInfo> = {
        let mut v: Vec<NodeInfo> = fleet.nodes.lock().await.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };

    let mut p = Prom::default();
    // Fleet
    let healthy: Vec<&NodeInfo> = nodes.iter().filter(|n| n.healthy()).collect();
    p.rows(
        "fluxvm_fleet_nodes",
        "gauge",
        "Registered nodes (Macs) by state: healthy (reported in the last 30 seconds), unhealthy, cordoned.",
        [
            (vec![("state", "healthy")], healthy.len() as f64),
            (
                vec![("state", "unhealthy")],
                (nodes.len() - healthy.len()) as f64,
            ),
            (
                vec![("state", "cordoned")],
                nodes.iter().filter(|n| n.cordoned).count() as f64,
            ),
        ],
    );
    p.one(
        "fluxvm_fleet_vcpus",
        "gauge",
        "Logical CPUs of the healthy nodes.",
        healthy.iter().map(|n| f64::from(n.vcpus_total)).sum(),
    );
    p.one(
        "fluxvm_fleet_memory_bytes",
        "gauge",
        "Memory of the healthy nodes.",
        healthy
            .iter()
            .map(|n| n.memory_mib_total as f64 * 1_048_576.0)
            .sum(),
    );
    p.one(
        "fluxvm_fleet_vms",
        "gauge",
        "VMs on the healthy nodes, as their heartbeats report.",
        healthy.iter().map(|n| n.vm_count as f64).sum(),
    );
    p.one(
        "fluxvm_fleet_leases_active",
        "gauge",
        "Active Mac leases.",
        leases_active as f64,
    );
    p.one(
        "fluxvm_fleet_nodes_scrub_pending",
        "gauge",
        "Nodes that must be scrubbed before they can be leased again.",
        scrub as f64,
    );
    p.one(
        "fluxvm_fleet_nodes_draining",
        "gauge",
        "Nodes being drained.",
        draining as f64,
    );

    // Nodes: registry facts, then the collector's latest sample.
    let up: Vec<(Vec<(&str, &str)>, f64)> = nodes
        .iter()
        .map(|n| {
            (
                vec![("node", n.name.as_str())],
                f64::from(u8::from(n.healthy())),
            )
        })
        .collect();
    p.rows(
        "fluxvm_node_up",
        "gauge",
        "1 when the node reported in the last 30 seconds.",
        up,
    );
    p.rows(
        "fluxvm_node_cordoned",
        "gauge",
        "1 when the node is cordoned (no new VMs).",
        nodes.iter().map(|n| {
            (
                vec![("node", n.name.as_str())],
                f64::from(u8::from(n.cordoned)),
            )
        }),
    );
    p.rows(
        "fluxvm_node_vcpus",
        "gauge",
        "Logical CPUs the node reports.",
        nodes
            .iter()
            .map(|n| (vec![("node", n.name.as_str())], f64::from(n.vcpus_total))),
    );
    p.rows(
        "fluxvm_node_vms",
        "gauge",
        "VMs on the node, as its last heartbeat reports.",
        nodes
            .iter()
            .map(|n| (vec![("node", n.name.as_str())], n.vm_count as f64)),
    );
    {
        let g = lock(&fleet.metrics.inner);
        let mut names: Vec<&String> = g.nodes.keys().collect();
        names.sort();
        let with = |f: &dyn Fn(&NodeSeries) -> Option<f64>| {
            names
                .iter()
                .filter_map(|n| Some((vec![("node", n.as_str())], f(&g.nodes[*n])?)))
                .collect::<Vec<_>>()
        };
        let cpu =
            with(&|e: &NodeSeries| e.series.latest.and_then(|(_, v)| v[0]).map(|p| p / 100.0));
        p.rows(
            "fluxvm_node_cpu_utilization_ratio",
            "gauge",
            "Share of the node's CPU time that was busy between the collector's last two samples (0 to 1).",
            cpu,
        );
        let busy = with(&|e: &NodeSeries| e.last.cpu_busy_ticks.map(|x| x as f64));
        p.rows(
            "fluxvm_node_cpu_busy_ticks_total",
            "counter",
            "Busy CPU time over all cores since boot, in the node's clock ticks; rate() it against fluxvm_node_cpu_ticks_total.",
            busy,
        );
        let total = with(&|e: &NodeSeries| e.last.cpu_total_ticks.map(|x| x as f64));
        p.rows(
            "fluxvm_node_cpu_ticks_total",
            "counter",
            "All CPU time over all cores since boot, in the node's clock ticks.",
            total,
        );
        let mt = with(&|e: &NodeSeries| e.last.memory_total_mib.map(|x| x as f64 * 1_048_576.0));
        p.rows(
            "fluxvm_node_memory_bytes",
            "gauge",
            "Physical memory of the node.",
            mt,
        );
        let mu = with(&|e: &NodeSeries| {
            Some(
                e.last
                    .memory_total_mib?
                    .saturating_sub(e.last.memory_available_mib?) as f64
                    * 1_048_576.0,
            )
        });
        p.rows(
            "fluxvm_node_memory_used_bytes",
            "gauge",
            "Memory in use on the node (total minus available).",
            mu,
        );
        let df = with(&|e: &NodeSeries| e.last.disk_free_bytes.map(|x| x as f64));
        p.rows(
            "fluxvm_node_disk_free_bytes",
            "gauge",
            "Free space on the volume that holds the node's VM disks.",
            df,
        );
        let dt = with(&|e: &NodeSeries| e.last.disk_total_bytes.map(|x| x as f64));
        p.rows(
            "fluxvm_node_disk_bytes",
            "gauge",
            "Size of the volume that holds the node's VM disks.",
            dt,
        );
        let vr = with(&|e: &NodeSeries| Some(f64::from(e.last.vms_running)));
        p.rows(
            "fluxvm_node_vms_running",
            "gauge",
            "Running VMs on the node, from the collector's last sample.",
            vr,
        );

        // VMs
        let mut keys: Vec<&(String, String)> = g.vms.keys().collect();
        keys.sort();
        let rows: Vec<(&(String, String), &VmSeries, &str)> = keys
            .iter()
            .map(|k| {
                let e = &g.vms[*k];
                let t = ledger.get(*k).map(String::as_str).unwrap_or("");
                (*k, e, t)
            })
            .collect();
        let vm = |f: &dyn Fn(&VmSeries) -> Option<f64>| {
            rows.iter()
                .filter_map(|((n, id), e, t)| {
                    Some((
                        vec![
                            ("node", n.as_str()),
                            ("vm", id.as_str()),
                            ("name", e.last.name.as_str()),
                            ("tenant", *t),
                        ],
                        f(e)?,
                    ))
                })
                .collect::<Vec<_>>()
        };
        p.rows(
            "fluxvm_vm_vcpus",
            "gauge",
            "vCPUs the VM was given.",
            vm(&|e: &VmSeries| Some(f64::from(e.last.vcpus))),
        );
        p.rows(
            "fluxvm_vm_running",
            "gauge",
            "1 when the VM is running.",
            vm(&|e: &VmSeries| Some(f64::from(u8::from(e.last.status == "running")))),
        );
        p.rows(
            "fluxvm_vm_cpu_seconds_total",
            "counter",
            "CPU time the VM has used (for vz guests: the runner plus the Virtualization service that runs the guest).",
            vm(&|e: &VmSeries| e.last.cpu_seconds_total),
        );
        p.rows(
            "fluxvm_vm_cpu_utilization_ratio",
            "gauge",
            "Share of the VM's vCPUs that was busy between the collector's last two samples (0 to 1).",
            vm(&|e: &VmSeries| e.series.latest.and_then(|(_, v)| v[0]).map(|x| x / 100.0)),
        );
        p.rows(
            "fluxvm_vm_memory_used_bytes",
            "gauge",
            "Memory the VM uses on its host (resident size for vz guests, cgroup memory.current otherwise).",
            vm(&|e: &VmSeries| e.last.memory_used_bytes.map(|x| x as f64)),
        );
        p.rows(
            "fluxvm_vm_disk_read_bytes_total",
            "counter",
            "Bytes the VM has read from storage.",
            vm(&|e: &VmSeries| e.last.disk_read_bytes_total.map(|x| x as f64)),
        );
        p.rows(
            "fluxvm_vm_disk_written_bytes_total",
            "counter",
            "Bytes the VM has written to storage.",
            vm(&|e: &VmSeries| e.last.disk_write_bytes_total.map(|x| x as f64)),
        );
        p.rows(
            "fluxvm_vm_network_receive_bytes_total",
            "counter",
            "Bytes the VM has received (absent where the host cannot measure it, such as vz guests).",
            vm(&|e: &VmSeries| e.last.net_rx_bytes_total.map(|x| x as f64)),
        );
        p.rows(
            "fluxvm_vm_network_transmit_bytes_total",
            "counter",
            "Bytes the VM has sent.",
            vm(&|e: &VmSeries| e.last.net_tx_bytes_total.map(|x| x as f64)),
        );
    }

    // Tenants
    let trow = |f: &dyn Fn(&TenantRow) -> f64| {
        tenants
            .iter()
            .map(|t| (vec![("tenant", t.id.as_str())], f(t)))
            .collect::<Vec<_>>()
    };
    p.rows(
        "fluxvm_tenant_vms",
        "gauge",
        "VMs a tenant has running or stopped (the ledger's active entries).",
        trow(&|t: &TenantRow| t.vms as f64),
    );
    p.rows(
        "fluxvm_tenant_vcpus",
        "gauge",
        "vCPUs of a tenant's VMs.",
        trow(&|t: &TenantRow| t.vcpus as f64),
    );
    p.rows(
        "fluxvm_tenant_memory_bytes",
        "gauge",
        "Memory of a tenant's VMs.",
        trow(&|t: &TenantRow| t.memory_mib as f64 * 1_048_576.0),
    );
    p.rows(
        "fluxvm_tenant_leases_active",
        "gauge",
        "Macs a tenant leases now.",
        trow(&|t: &TenantRow| t.leases as f64),
    );
    p.rows(
        "fluxvm_tenant_disabled",
        "gauge",
        "1 when the tenant is disabled.",
        trow(&|t: &TenantRow| f64::from(u8::from(t.disabled))),
    );

    // Central itself
    p.one(
        "fluxvm_central_ha_enabled",
        "gauge",
        "1 when this central runs with leader election (--ha-redis).",
        f64::from(u8::from(fleet.ha.is_some())),
    );
    p.one(
        "fluxvm_central_ha_leader",
        "gauge",
        "1 when this central serves the fleet (always 1 without HA).",
        f64::from(u8::from(fleet.leading())),
    );
    let (ok, failed) = fleet.notify.delivery_counts();
    p.rows(
        "fluxvm_central_webhook_deliveries_total",
        "counter",
        "Webhook delivery attempts by result.",
        [
            (vec![("result", "ok")], ok as f64),
            (vec![("result", "failed")], failed as f64),
        ],
    );
    p.rows(
        "fluxvm_central_metrics_polls_total",
        "counter",
        "Node samples the metrics collector took, by result.",
        [
            (
                vec![("result", "ok")],
                fleet.metrics.polls_ok.load(Ordering::Relaxed) as f64,
            ),
            (
                vec![("result", "failed")],
                fleet.metrics.polls_failed.load(Ordering::Relaxed) as f64,
            ),
        ],
    );
    let (ns, vs) = fleet.metrics.series_counts();
    p.rows(
        "fluxvm_central_metrics_series",
        "gauge",
        "History series kept in memory.",
        [
            (vec![("kind", "node")], ns as f64),
            (vec![("kind", "vm")], vs as f64),
        ],
    );
    p.one(
        "fluxvm_central_metrics_series_dropped_total",
        "counter",
        "Samples not kept because the series cap was reached.",
        fleet.metrics.series_dropped.load(Ordering::Relaxed) as f64,
    );
    let reqs: Vec<((String, String, u16), u64)> = lock(&fleet.metrics.requests)
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let codes: Vec<String> = reqs.iter().map(|((_, _, s), _)| s.to_string()).collect();
    p.rows(
        "fluxvm_central_http_requests_total",
        "counter",
        "Requests central answered, by method, route pattern and status.",
        reqs.iter().zip(&codes).map(|(((m, r, _), v), code)| {
            (
                vec![
                    ("method", m.as_str()),
                    ("route", r.as_str()),
                    ("status", code.as_str()),
                ],
                *v as f64,
            )
        }),
    );
    p.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_scheduler::node_stats::{HostStats, NodeStats, VmStats};

    fn sample(vms: Vec<VmStats>, busy: u64, total: u64) -> NodeStats {
        NodeStats {
            ts: Utc::now(),
            host: HostStats {
                cpus: 8,
                cpu_busy_ticks: Some(busy),
                cpu_total_ticks: Some(total),
                memory_total_mib: Some(16384),
                memory_available_mib: Some(4096),
                disk_free_bytes: Some(100),
                vm_count: vms.len() as u32,
                vms_running: vms.len() as u32,
                ..Default::default()
            },
            vms,
        }
    }

    fn vm(id: &str, cpu: f64, disk: u64) -> VmStats {
        VmStats {
            id: id.into(),
            name: format!("vm-{id}"),
            status: "running".into(),
            vcpus: 2,
            cpu_seconds_total: Some(cpu),
            memory_used_bytes: Some(512 * 1_048_576),
            disk_read_bytes_total: Some(disk),
            disk_write_bytes_total: Some(disk / 2),
            source: "vz-process".into(),
            ..Default::default()
        }
    }

    #[test]
    fn rates_come_from_two_samples_and_resets_give_none() {
        let m = MetricsStore::default();
        let t0 = 1_800_000_000.0;
        m.ingest("mac", &sample(vec![vm("a", 10.0, 1000)], 100, 1000), t0);
        m.ingest(
            "mac",
            &sample(vec![vm("a", 25.0, 4000)], 400, 2000),
            t0 + 15.0,
        );
        let g = lock(&m.inner);
        let v = g.vms[&("mac".to_string(), "a".to_string())]
            .series
            .latest
            .unwrap()
            .1;
        // 15 CPU-seconds in 15 s on 2 vCPUs: 50%.
        assert_eq!(v[0], Some(50.0));
        assert_eq!(v[1], Some(512.0));
        assert_eq!(v[2], Some(200.0));
        let n = g.nodes["mac"].series.latest.unwrap().1;
        assert_eq!(n[0], Some(30.0));
        assert_eq!(n[1], Some(12288.0));
        drop(g);
        // The VM restarted: its counters went back. No rate, no negative numbers.
        m.ingest("mac", &sample(vec![vm("a", 1.0, 10)], 500, 3000), t0 + 30.0);
        let g = lock(&m.inner);
        let v = g.vms[&("mac".to_string(), "a".to_string())]
            .series
            .latest
            .unwrap()
            .1;
        assert_eq!(v[0], None);
        assert_eq!(v[2], None);
    }

    #[test]
    fn samples_average_into_minutes_and_history_is_bounded() {
        let mut s = Series::default();
        let base = 1_800_000_000 / 60 * 60;
        s.record(base, [Some(10.0), None, None, None, None, None]);
        s.record(base + 30, [Some(30.0), Some(1.0), None, None, None, None]);
        s.record(base + 60, [Some(50.0), None, None, None, None, None]);
        let w = s.window(0);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].0, base);
        assert_eq!(w[0].1[0], 20.0);
        assert_eq!(w[0].1[1], 1.0);
        assert!(w[0].1[2].is_nan());
        assert_eq!(w[1].1[0], 50.0, "the open minute is included");
        for i in 0..(KEEP_POINTS as i64 + 100) {
            s.record(base + 120 + i * 60, [Some(1.0); 6]);
        }
        assert!(s.points.len() <= KEEP_POINTS);
        let newest = s.points.back().unwrap().0;
        assert!(s.points.front().unwrap().0 > newest - KEEP_SECS);
        // A clock step backwards does not reorder anything.
        s.record(base, [Some(1.0); 6]);
        s.record(base + 60, [Some(1.0); 6]);
        let w = s.window(0);
        assert!(w.windows(2).all(|p| p[0].0 < p[1].0));
    }

    #[test]
    fn deleted_vms_and_nodes_lose_their_series() {
        let m = MetricsStore::default();
        m.ingest(
            "mac",
            &sample(vec![vm("a", 1.0, 1), vm("b", 1.0, 1)], 1, 2),
            1.0,
        );
        m.ingest("other", &sample(vec![vm("c", 1.0, 1)], 1, 2), 1.0);
        m.ingest("mac", &sample(vec![vm("b", 2.0, 2)], 2, 4), 16.0);
        assert_eq!(m.series_counts(), (2, 2));
        m.retain_nodes(&["mac".to_owned()].into_iter().collect());
        assert_eq!(m.series_counts(), (1, 1));
        assert!(
            lock(&m.inner)
                .vms
                .contains_key(&("mac".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn the_series_cap_holds() {
        let m = MetricsStore::default();
        let many: Vec<VmStats> = (0..MAX_VM_SERIES + 5)
            .map(|i| vm(&i.to_string(), 0.0, 0))
            .collect();
        m.ingest("mac", &sample(many, 1, 2), 1.0);
        assert_eq!(m.series_counts().1, MAX_VM_SERIES);
        assert_eq!(m.series_dropped.load(Ordering::Relaxed), 5);
        // Over-long ids from a node are ignored.
        m.ingest(
            "mac2",
            &sample(vec![vm(&"x".repeat(500), 0.0, 0)], 1, 2),
            1.0,
        );
        assert!(!lock(&m.inner).vms.keys().any(|(n, _)| n == "mac2"));
    }

    #[test]
    fn exposition_escapes_labels_and_formats_numbers() {
        assert_eq!(esc_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(fmt_value(3.0), "3");
        assert_eq!(fmt_value(0.25), "0.25");
        assert_eq!(fmt_value(f64::NAN), "NaN");
        let mut p = Prom::default();
        p.rows(
            "x_total",
            "counter",
            "Help.",
            [(vec![("name", "we\"ird")], 1.0)],
        );
        assert_eq!(
            p.out,
            "# HELP x_total Help.\n# TYPE x_total counter\nx_total{name=\"we\\\"ird\"} 1\n"
        );
    }

    #[test]
    fn ranges_are_checked() {
        assert_eq!(parse_range(None).unwrap().0, 3600);
        assert_eq!(parse_range(Some("6h")).unwrap().1, "6h");
        assert_eq!(parse_range(Some("24h")).unwrap().0, 86400);
        assert!(parse_range(Some("7d")).is_err());
    }

    #[test]
    fn request_counters_are_bounded() {
        let m = MetricsStore::default();
        for i in 0..(MAX_REQUEST_SERIES + 10) {
            m.count_request("GET", format!("/r{i}"), 200);
        }
        m.count_request("BREW", "/fleet/vms".into(), 418);
        let r = lock(&m.requests);
        // Past the cap only the folded `route="other"` keys are added (one per method and status).
        assert_eq!(r.len(), MAX_REQUEST_SERIES + 2);
        assert!(r.contains_key(&("GET".into(), "other".into(), 200)));
        assert!(r.keys().any(|(m, _, _)| m == "OTHER"));
    }

    #[test]
    fn tenants_may_read_metrics_but_not_collect_or_scrape() {
        use axum::http::Method;
        assert!(tenant_route_allowed(&Method::GET, "/fleet/metrics/summary"));
        assert!(tenant_route_allowed(&Method::GET, "/fleet/metrics/vms"));
        assert!(tenant_route_allowed(
            &Method::GET,
            "/fleet/metrics/vms/mac/abc"
        ));
        assert!(tenant_route_allowed(
            &Method::GET,
            "/fleet/metrics/nodes/mac"
        ));
        assert!(!tenant_route_allowed(
            &Method::POST,
            "/fleet/metrics/collect"
        ));
        assert!(!tenant_route_allowed(
            &Method::DELETE,
            "/fleet/metrics/vms/mac/abc"
        ));
    }
}
