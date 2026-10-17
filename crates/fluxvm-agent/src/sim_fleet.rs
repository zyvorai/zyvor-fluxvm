// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxvm-agent sim-fleet`: N simulated nodes heartbeating one central, to measure it and to check that no
//! registration is lost.
//!
//! Each simulated node is one lightweight task that POSTs the same body a real `fluxvm-agent node` sends to
//! `/fleet/register`, at the same interval, with no daemon behind it. At the end the run asks central which
//! nodes it knows and checks that every simulated node is there exactly once and healthy. What is measured is
//! central's side only: latency of the heartbeat requests, how many failed, and how long it took to settle.
//!
//! Limits, so the numbers are not over-read: everything runs over loopback or one LAN hop from one process
//! (or a few); connections are pooled, so it does not reproduce the TLS-handshake load of 10,000 distinct
//! clients; the advertised node URL points nowhere, so central's own polling of nodes fails fast instead of
//! costing a real round trip.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How the simulated nodes authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SimMode {
    /// Every node beats with the fleet token (what nodes did before enrollment).
    Legacy,
    /// Every node enrolls with the join token first and beats with its own credential.
    Enrolled,
    /// Odd-numbered nodes enroll, even-numbered ones use the fleet token.
    Mixed,
}

impl std::str::FromStr for SimMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "legacy" => Ok(SimMode::Legacy),
            "enrolled" => Ok(SimMode::Enrolled),
            "mixed" => Ok(SimMode::Mixed),
            _ => Err(format!("'{s}' is not legacy, enrolled or mixed")),
        }
    }
}

impl SimMode {
    fn enrolls(self, index: usize) -> bool {
        match self {
            SimMode::Legacy => false,
            SimMode::Enrolled => true,
            SimMode::Mixed => index % 2 == 1,
        }
    }
}

pub struct SimFleetConfig {
    pub central_url: String,
    pub token: Option<String>,
    pub mode: SimMode,
    /// A join token (`fvjt_...`) for the nodes that enroll; they need `max_uses` of at least their number.
    pub join_token: Option<String>,
    pub nodes: usize,
    /// Node `i` is named `{prefix}-{i:05}`.
    pub prefix: String,
    pub interval: Duration,
    /// How long to keep heartbeating.
    pub duration: Duration,
    /// Spread each node's first beat over the interval by a stable per-node offset. Off = every node beats at
    /// once on start (the thundering herd a mass reboot causes).
    pub start_jitter: bool,
    /// Per-request timeout.
    pub request_timeout: Duration,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SimFleetReport {
    pub mode: SimMode,
    /// Simulated nodes that obtained a credential.
    pub enrolled: u64,
    pub nodes: usize,
    pub interval_secs: f64,
    pub duration_secs: f64,
    pub start_jitter: bool,
    pub beats_sent: u64,
    pub beats_ok: u64,
    /// Answers other than 2xx, by class.
    pub http_429: u64,
    pub http_4xx_other: u64,
    pub http_5xx: u64,
    /// Requests that got no answer (refused, reset, timed out).
    pub transport_errors: u64,
    pub beats_per_sec: f64,
    pub latency_ms: Latency,
    pub check: CheckReport,
}

#[derive(Debug, Serialize, PartialEq, Default)]
pub struct Latency {
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

/// What central reported after the run.
#[derive(Debug, Serialize, PartialEq, Default)]
pub struct CheckReport {
    pub registered: usize,
    /// Simulated nodes central does not list.
    pub missing: usize,
    /// Names with this run's prefix that this run did not create.
    pub unexpected: usize,
    pub duplicates: usize,
    pub unhealthy: usize,
    pub ok: bool,
}

impl SimFleetReport {
    /// A run passes when every beat was accepted and every node is registered once and healthy.
    pub fn passed(&self) -> bool {
        self.beats_sent > 0 && self.beats_ok == self.beats_sent && self.check.ok
    }
}

pub fn node_name(prefix: &str, i: usize) -> String {
    format!("{prefix}-{i:05}")
}

/// A stable 0.0..1.0 from the node name, so each node keeps its own beat phase and a fleet spreads evenly.
pub fn phase_fraction(name: &str) -> f64 {
    let d = Sha256::digest(name.as_bytes());
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
    f64::from(n) / (f64::from(u32::MAX) + 1.0)
}

/// The percentile (0..=100) of an ascending-sorted slice, nearest-rank. 0 for an empty slice.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn heartbeat_body(name: &str, i: usize) -> Value {
    // The fields of a real beat (node.rs `beat`). The URL goes nowhere: central's polling of it fails at once.
    json!({
        "name": name,
        "fluxvm_url": "http://127.0.0.1:1",
        "vcpus_total": 12,
        "memory_mib_total": 32768,
        "vm_count": i % 5,
        "labels": {"sim": "fleet", "zone": format!("z{}", i % 8)},
        "security": fluxvm_core::security::NodeSecurityCapabilities::standard_only(),
        "density": Value::Null,
        "apple": Value::Null,
        "daemon": {"reachable": true},
        "allocated": {"vcpus": (i % 5) * 2, "memory_mib": (i % 5) * 2048},
    })
}

#[derive(Default)]
struct Tally {
    sent: u64,
    ok: u64,
    r429: u64,
    other4xx: u64,
    r5xx: u64,
    transport: u64,
    enrolled: u64,
    latencies_ms: Vec<u64>,
}

impl Tally {
    fn merge(&mut self, o: Tally) {
        self.sent += o.sent;
        self.ok += o.ok;
        self.r429 += o.r429;
        self.other4xx += o.other4xx;
        self.r5xx += o.r5xx;
        self.transport += o.transport;
        self.enrolled += o.enrolled;
        self.latencies_ms.extend(o.latencies_ms);
    }
}

/// What one simulated node needs; grouped so the task function stays readable.
struct NodeJob {
    http: reqwest::Client,
    url: String,
    token: Option<String>,
    enroll_with: Option<String>,
    name: String,
    index: usize,
    interval: Duration,
    start_jitter: bool,
    deadline: Instant,
}

async fn node_task(job: NodeJob) -> Tally {
    let NodeJob {
        http,
        url,
        mut token,
        enroll_with,
        name,
        index: i,
        interval,
        start_jitter,
        deadline,
    } = job;
    let mut t = Tally::default();
    if start_jitter {
        tokio::time::sleep(interval.mul_f64(phase_fraction(&name))).await;
    }
    if let Some(join) = enroll_with {
        // A stable id per name, so a rerun finds the same enrollment instead of a conflict.
        let node_id = format!("{:x}", Sha256::digest(name.as_bytes()))[..32].to_owned();
        // Like a real node: a 429 is waited out (Retry-After) and tried again.
        let mut tries = 0;
        loop {
            tries += 1;
            match http
                .post(format!("{url}/fleet/enroll"))
                .bearer_auth(&join)
                .json(&json!({"name": name, "node_id": node_id}))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    t.enrolled = 1;
                    token = resp
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|v| v["credential"].as_str().map(str::to_owned));
                    break;
                }
                Ok(resp) if resp.status().as_u16() == 429 && tries < 30 => {
                    t.r429 += 1;
                    let wait = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(1);
                    tokio::time::sleep(Duration::from_secs(wait.clamp(1, 10))).await;
                }
                Ok(_) => {
                    t.other4xx += 1;
                    return t;
                }
                Err(_) => {
                    t.transport += 1;
                    return t;
                }
            }
        }
    }
    let body = heartbeat_body(&name, i);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if Instant::now() >= deadline {
            break;
        }
        let mut req = http.post(format!("{url}/fleet/register")).json(&body);
        if let Some(tok) = &token {
            req = req.bearer_auth(tok);
        }
        let start = Instant::now();
        t.sent += 1;
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                // Drain the body so the connection can be reused and the timing includes it.
                let _ = resp.bytes().await;
                t.latencies_ms.push(start.elapsed().as_millis() as u64);
                if status.is_success() {
                    t.ok += 1;
                } else if status.as_u16() == 429 {
                    t.r429 += 1;
                } else if status.is_server_error() {
                    t.r5xx += 1;
                } else {
                    t.other4xx += 1;
                }
            }
            Err(_) => t.transport += 1,
        }
    }
    t
}

/// Asks central which nodes it has and compares with the expected names.
pub async fn check_registry(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    prefix: &str,
    nodes: usize,
) -> Result<CheckReport> {
    let mut req = http.get(format!("{url}/fleet/nodes"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("GET /fleet/nodes")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "GET /fleet/nodes answered {}",
        resp.status()
    );
    let v: Value = resp.json().await.context("parsing /fleet/nodes")?;
    let items = v["items"].as_array().cloned().unwrap_or_default();

    let expected: BTreeSet<String> = (0..nodes).map(|i| node_name(prefix, i)).collect();
    let mut seen = BTreeSet::new();
    let mut duplicates = 0usize;
    let mut unexpected = 0usize;
    let mut unhealthy = 0usize;
    let marker = format!("{prefix}-");
    for item in &items {
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        if !name.starts_with(&marker) {
            continue;
        }
        if !expected.contains(name) {
            unexpected += 1;
            continue;
        }
        if !seen.insert(name.to_owned()) {
            duplicates += 1;
        }
        if item["healthy"] == json!(false) {
            unhealthy += 1;
        }
    }
    let missing = expected.difference(&seen).count();
    Ok(CheckReport {
        registered: seen.len(),
        missing,
        unexpected,
        duplicates,
        unhealthy,
        ok: missing == 0 && unexpected == 0 && duplicates == 0 && unhealthy == 0,
    })
}

/// Raises the open-file limit so thousands of in-flight connections do not die with EMFILE. Best effort.
fn raise_nofile(wanted: u64) -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain libc calls on a local struct.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return None;
        }
        let target = (wanted as libc::rlim_t).min(lim.rlim_max);
        if target > lim.rlim_cur {
            let new = libc::rlimit {
                rlim_cur: target,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                #[allow(clippy::unnecessary_cast)] // rlim_t is not u64 everywhere
                return Some(target as u64);
            }
        }
        #[allow(clippy::unnecessary_cast)]
        Some(lim.rlim_cur as u64)
    }
}

pub async fn run(cfg: SimFleetConfig) -> Result<SimFleetReport> {
    anyhow::ensure!(cfg.nodes > 0, "--nodes must be at least 1");
    let url = cfg.central_url.trim_end_matches('/').to_owned();
    if let Some(limit) = raise_nofile(cfg.nodes as u64 * 2 + 1024)
        && limit < 1024
    {
        tracing::warn!(
            limit,
            "open-file limit is low; raise it (ulimit -n) for many nodes"
        );
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(cfg.request_timeout)
        .pool_max_idle_per_host(cfg.nodes.min(4096))
        .build()
        .context("building the HTTP client")?;

    let started = Instant::now();
    let deadline = started + cfg.duration;
    let mut handles = Vec::with_capacity(cfg.nodes);
    for i in 0..cfg.nodes {
        handles.push(tokio::spawn(node_task(NodeJob {
            http: http.clone(),
            url: url.clone(),
            token: cfg.token.clone(),
            enroll_with: cfg
                .mode
                .enrolls(i)
                .then(|| cfg.join_token.clone())
                .flatten(),
            name: node_name(&cfg.prefix, i),
            index: i,
            interval: cfg.interval,
            start_jitter: cfg.start_jitter,
            deadline,
        })));
    }
    let mut total = Tally::default();
    for h in handles {
        total.merge(h.await.context("a simulated node task panicked")?);
    }
    let elapsed = started.elapsed().as_secs_f64();

    let check = check_registry(&http, &url, cfg.token.as_deref(), &cfg.prefix, cfg.nodes).await?;
    total.latencies_ms.sort_unstable();
    let l = &total.latencies_ms;
    Ok(SimFleetReport {
        mode: cfg.mode,
        enrolled: total.enrolled,
        nodes: cfg.nodes,
        interval_secs: cfg.interval.as_secs_f64(),
        duration_secs: elapsed,
        start_jitter: cfg.start_jitter,
        beats_sent: total.sent,
        beats_ok: total.ok,
        http_429: total.r429,
        http_4xx_other: total.other4xx,
        http_5xx: total.r5xx,
        transport_errors: total.transport,
        beats_per_sec: if elapsed > 0.0 {
            total.sent as f64 / elapsed
        } else {
            0.0
        },
        latency_ms: Latency {
            p50: percentile(l, 50.0),
            p90: percentile(l, 90.0),
            p99: percentile(l, 99.0),
            max: l.last().copied().unwrap_or(0),
        },
        check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Mock {
        names: Arc<Mutex<Vec<String>>>,
        /// Every Nth register answers 500 when non-zero.
        fail_every: usize,
        count: Arc<Mutex<usize>>,
    }

    async fn register(State(m): State<Mock>, Json(b): Json<Value>) -> StatusCode {
        let n = {
            let mut c = m.count.lock().unwrap();
            *c += 1;
            *c
        };
        if m.fail_every != 0 && n % m.fail_every == 0 {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        if let Some(name) = b["name"].as_str() {
            let mut names = m.names.lock().unwrap();
            if !names.iter().any(|x| x == name) {
                names.push(name.to_owned());
            }
        }
        StatusCode::OK
    }

    async fn nodes(State(m): State<Mock>) -> Json<Value> {
        let items: Vec<Value> = m
            .names
            .lock()
            .unwrap()
            .iter()
            .map(|n| json!({"name": n, "healthy": true}))
            .collect();
        Json(json!({"items": items}))
    }

    async fn serve(m: Mock) -> String {
        let app = Router::new()
            .route("/fleet/register", post(register))
            .route("/fleet/nodes", get(nodes))
            .with_state(m);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn cfg(url: String, nodes: usize) -> SimFleetConfig {
        SimFleetConfig {
            central_url: url,
            token: None,
            mode: SimMode::Legacy,
            join_token: None,
            nodes,
            prefix: "t".into(),
            interval: Duration::from_millis(100),
            duration: Duration::from_millis(450),
            start_jitter: true,
            request_timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn names_are_stable_zero_padded() {
        assert_eq!(node_name("sf", 7), "sf-00007");
        assert_eq!(node_name("sf", 12345), "sf-12345");
    }

    #[test]
    fn phase_is_stable_in_range_and_spread() {
        assert_eq!(phase_fraction("sf-00001"), phase_fraction("sf-00001"));
        let fs: Vec<f64> = (0..1000)
            .map(|i| phase_fraction(&node_name("sf", i)))
            .collect();
        assert!(fs.iter().all(|f| (0.0..1.0).contains(f)));
        // Spread: every tenth of the interval gets some nodes (a clustered phase would herd).
        for tenth in 0..10 {
            let lo = f64::from(tenth) / 10.0;
            let n = fs.iter().filter(|f| **f >= lo && **f < lo + 0.1).count();
            assert!(n > 50, "tenth {tenth} has only {n} of 1000 nodes");
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), 50);
        assert_eq!(percentile(&v, 99.0), 99);
        assert_eq!(percentile(&v, 100.0), 100);
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[7], 1.0), 7);
    }

    #[tokio::test]
    async fn every_node_registers_once_and_the_run_passes() {
        let m = Mock::default();
        let url = serve(m.clone()).await;
        let r = run(cfg(url, 20)).await.unwrap();
        assert_eq!(r.check.registered, 20, "{r:?}");
        assert_eq!(r.check.missing, 0);
        assert_eq!(r.check.duplicates, 0);
        assert!(
            r.beats_sent >= 20 * 3,
            "expected several beats per node: {}",
            r.beats_sent
        );
        assert_eq!(r.beats_ok, r.beats_sent);
        assert!(r.passed(), "{r:?}");
        assert!(r.latency_ms.max >= r.latency_ms.p50);
    }

    #[tokio::test]
    async fn failed_beats_fail_the_run_and_are_counted() {
        let m = Mock {
            fail_every: 4,
            ..Mock::default()
        };
        let url = serve(m).await;
        let r = run(cfg(url, 8)).await.unwrap();
        assert!(r.http_5xx > 0, "{r:?}");
        assert!(r.beats_ok < r.beats_sent);
        assert!(!r.passed());
    }

    #[tokio::test]
    async fn a_missing_node_is_reported() {
        let m = Mock::default();
        let url = serve(m.clone()).await;
        // Register 3 of 5 by hand, then check for 5.
        let http = reqwest::Client::new();
        for i in 0..3 {
            http.post(format!("{url}/fleet/register"))
                .json(&json!({"name": node_name("t", i)}))
                .send()
                .await
                .unwrap();
        }
        let c = check_registry(&http, &url, None, "t", 5).await.unwrap();
        assert_eq!((c.registered, c.missing, c.ok), (3, 2, false));
    }

    #[tokio::test]
    async fn a_foreign_name_with_the_prefix_is_unexpected() {
        let m = Mock::default();
        let url = serve(m.clone()).await;
        let http = reqwest::Client::new();
        for n in ["t-00000", "t-99999", "other-00001"] {
            http.post(format!("{url}/fleet/register"))
                .json(&json!({"name": n}))
                .send()
                .await
                .unwrap();
        }
        let c = check_registry(&http, &url, None, "t", 1).await.unwrap();
        assert_eq!((c.registered, c.unexpected, c.missing), (1, 1, 0));
        assert!(!c.ok);
    }

    #[tokio::test]
    async fn an_unreachable_central_counts_transport_errors() {
        // Port 1 on loopback refuses at once.
        let mut c = cfg("http://127.0.0.1:1".into(), 3);
        c.duration = Duration::from_millis(250);
        let err = run(c).await.unwrap_err();
        // The final registry check cannot reach central either: the run reports that as an error, not a pass.
        assert!(format!("{err:#}").contains("GET /fleet/nodes"), "{err:#}");
    }
}
