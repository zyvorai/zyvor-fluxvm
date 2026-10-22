//! How central keeps the node registry under a heartbeat load: heartbeats change memory and, only when something
//! durable changed, mark the registry dirty; one background task writes `fleet-nodes.json` at most every few
//! seconds. Heartbeats are also admitted through a concurrency limit and a token bucket, and a refused one gets a
//! `429` with a jittered `Retry-After` so a fleet does not retry in lockstep.
//!
//! Before this, every heartbeat rewrote the whole registry file under the registry lock (see docs/fleet-scale.md).

use super::{Fleet, persist_nodes};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Largest heartbeat body central reads. A real one is about 1 KiB; this leaves room for labels and capabilities.
pub(super) const REGISTER_BODY_LIMIT: usize = 256 * 1024;

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Concurrency limit plus token bucket for `POST /fleet/register`.
pub(super) struct Admission {
    inflight: Semaphore,
    bucket: StdMutex<Bucket>,
    /// Heartbeats per second admitted in the long run (0 = unlimited).
    rate: f64,
    burst: f64,
    pub(super) rejected: AtomicU64,
}

impl Admission {
    pub(super) fn new(rate: f64, burst: f64, inflight: usize) -> Self {
        Admission {
            inflight: Semaphore::new(inflight.max(1)),
            bucket: StdMutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
            }),
            rate,
            burst,
            rejected: AtomicU64::new(0),
        }
    }

    /// `FLUXVM_REGISTER_RATE` (default 5000/s), `FLUXVM_REGISTER_BURST` (default 10000), `FLUXVM_REGISTER_INFLIGHT`
    /// (default 512).
    pub(super) fn from_env() -> Self {
        Self::new(
            env_f64("FLUXVM_REGISTER_RATE", 5000.0),
            env_f64("FLUXVM_REGISTER_BURST", 10000.0),
            env_f64("FLUXVM_REGISTER_INFLIGHT", 512.0) as usize,
        )
    }

    /// A permit to hold while the heartbeat is handled, or the seconds to wait before retrying.
    pub(super) fn admit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, u64> {
        if self.rate > 0.0 {
            let mut b = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            b.tokens =
                (b.tokens + now.duration_since(b.last).as_secs_f64() * self.rate).min(self.burst);
            b.last = now;
            if b.tokens < 1.0 {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                return Err(retry_after((1.0 - b.tokens) / self.rate));
            }
            b.tokens -= 1.0;
        }
        match self.inflight.try_acquire() {
            Ok(p) => Ok(p),
            Err(_) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                Err(retry_after(0.0))
            }
        }
    }
}

/// Whole seconds to wait: at least `need`, at least 1, plus up to 3 more so refused nodes spread out.
fn retry_after(need: f64) -> u64 {
    let spread = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % 4)
        .unwrap_or(0);
    (need.ceil() as u64).max(1) + spread
}

/// The auth layer has already run (this is a `route_layer` on the register route), so only callers central would
/// have accepted use up admission capacity.
pub(super) async fn admission(State(fleet): State<Fleet>, req: Request, next: Next) -> Response {
    match fleet.registry.admission.admit() {
        Ok(_permit) => next.run(req).await,
        Err(secs) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            axum::Json(json!({"error": "central is busy; retry later", "code": "RATE_LIMITED", "retry_after_secs": secs})),
        )
            .into_response(),
    }
}

/// Runtime state for the registry: the dirty flag, the flush numbers and admission control.
pub(super) struct RegistryRt {
    /// Join tokens and node credentials (`fleet-enroll.json`).
    pub(super) enroll: super::enroll::Enroll,
    dirty: AtomicBool,
    pub(super) admission: Admission,
    flushes: AtomicU64,
    last_flush_ms: AtomicU64,
    last_flush_bytes: AtomicU64,
}

impl Default for RegistryRt {
    fn default() -> Self {
        RegistryRt::with_enroll(Default::default())
    }
}

impl RegistryRt {
    pub(super) fn with_enroll(enroll: super::enroll::Enroll) -> Self {
        RegistryRt {
            enroll,
            dirty: AtomicBool::new(false),
            admission: Admission::from_env(),
            flushes: AtomicU64::new(0),
            last_flush_ms: AtomicU64::new(0),
            last_flush_bytes: AtomicU64::new(0),
        }
    }
}

impl RegistryRt {
    /// Something a restart must not lose changed (a new node, an address, labels, capabilities, a cordon).
    pub(super) fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub(super) fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub(super) fn status(&self, nodes: usize) -> serde_json::Value {
        json!({
            "nodes": nodes,
            "dirty": self.is_dirty(),
            "flushes": self.flushes.load(Ordering::Relaxed),
            "last_flush_ms": self.last_flush_ms.load(Ordering::Relaxed),
            "last_flush_bytes": self.last_flush_bytes.load(Ordering::Relaxed),
            "heartbeats_refused": self.admission.rejected.load(Ordering::Relaxed),
        })
    }
}

fn flush_interval() -> Duration {
    Duration::from_millis((env_f64("FLUXVM_REGISTRY_FLUSH_SECS", 5.0).max(0.05) * 1000.0) as u64)
}

/// Writes the registry now if it is dirty (or `force`). A restart can lose at most one flush interval of durable
/// changes, and a node simply registers again on its next beat.
pub(super) async fn flush(fleet: &Fleet, force: bool) {
    let rt = &fleet.registry;
    if !force && !rt.dirty.swap(false, Ordering::AcqRel) {
        return;
    }
    rt.dirty.store(false, Ordering::Release);
    let (Some(path), Some(lock)) = (fleet.persist_path.clone(), fleet.lock_path.clone()) else {
        return;
    };
    // Clone under the lock (microseconds per thousand nodes), serialize and write outside it.
    let snapshot = fleet.nodes.lock().await.clone();
    let started = Instant::now();
    let bytes = tokio::task::spawn_blocking(move || {
        persist_nodes(&path, &lock, &snapshot);
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    })
    .await
    .unwrap_or(0);
    rt.flushes.fetch_add(1, Ordering::Relaxed);
    rt.last_flush_ms
        .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    rt.last_flush_bytes.store(bytes, Ordering::Relaxed);
}

/// The background writer. Also writes every minute even when nothing durable changed, so `last_seen` in the file
/// is never older than that.
pub(super) fn spawn_flusher(fleet: Fleet) {
    tokio::spawn(async move {
        let every = flush_interval();
        let mut last_forced = Instant::now();
        loop {
            tokio::time::sleep(every).await;
            let force = last_forced.elapsed() >= Duration::from_secs(60);
            if force {
                last_forced = Instant::now();
            }
            flush(&fleet, force).await;
        }
    });
}

/// How many nodes central talks to at once when it polls or fans out (`FLUXVM_POLL_CONCURRENCY`, default 64).
pub(super) fn poll_concurrency() -> usize {
    (env_f64("FLUXVM_POLL_CONCURRENCY", 64.0) as usize).clamp(1, 1024)
}

/// Runs `f` on every item with at most [`poll_concurrency`] in flight and returns the results in completion order
/// (put what identifies the item in the result). A task that panics is dropped from the results.
pub(super) async fn bounded<T, R, F, Fut>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    let sem = std::sync::Arc::new(Semaphore::new(poll_concurrency()));
    let mut set = tokio::task::JoinSet::new();
    for item in items {
        let Ok(permit) = sem.clone().acquire_owned().await else {
            break;
        };
        let fut = f(item);
        set.spawn(async move {
            let r = fut.await;
            drop(permit);
            r
        });
    }
    let mut out = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        if let Ok(r) = joined {
            out.push(r);
        }
    }
    out
}

/// Filters and paging for `GET /fleet/nodes`.
#[derive(Default)]
pub(super) struct NodeQuery {
    pub(super) limit: Option<usize>,
    pub(super) cursor: Option<String>,
    healthy: Option<bool>,
    cordoned: Option<bool>,
    outdated: Option<bool>,
    version: Option<String>,
    labels: Vec<(String, String)>,
    pub(super) summary: bool,
}

impl NodeQuery {
    pub(super) fn parse(raw: &str) -> Result<Self, String> {
        let mut q = NodeQuery::default();
        if raw.is_empty() {
            return Ok(q);
        }
        let url = reqwest::Url::parse(&format!("http://x/?{raw}"))
            .map_err(|e| format!("bad query: {e}"))?;
        let flag = |k: &str, v: &str| -> Result<bool, String> {
            match v {
                "true" | "1" => Ok(true),
                "false" | "0" => Ok(false),
                _ => Err(format!("{k} must be true or false")),
            }
        };
        for (k, v) in url.query_pairs() {
            match &*k {
                "limit" => {
                    let n: usize = v.parse().map_err(|_| "limit must be a number".to_owned())?;
                    if !(1..=5000).contains(&n) {
                        return Err("limit must be between 1 and 5000".into());
                    }
                    q.limit = Some(n);
                }
                "cursor" => q.cursor = Some(v.into_owned()),
                "healthy" => q.healthy = Some(flag("healthy", &v)?),
                "cordoned" => q.cordoned = Some(flag("cordoned", &v)?),
                "outdated" => q.outdated = Some(flag("outdated", &v)?),
                "summary" => q.summary = flag("summary", &v)?,
                "version" => q.version = Some(v.into_owned()),
                "label" => {
                    let (lk, lv) = v.split_once('=').ok_or("label must be key=value")?;
                    q.labels.push((lk.to_owned(), lv.to_owned()));
                }
                other => return Err(format!("unknown query parameter '{other}'")),
            }
        }
        Ok(q)
    }

    pub(super) fn matches(&self, n: &super::NodeInfo) -> bool {
        self.healthy.is_none_or(|h| n.healthy() == h)
            && self.cordoned.is_none_or(|c| n.cordoned == c)
            && self.outdated.is_none_or(|o| n.outdated == o)
            && self.version.as_ref().is_none_or(|v| &n.agent_version == v)
            && self.labels.iter().all(|(k, v)| n.labels.get(k) == Some(v))
    }
}

/// Counts for `GET /fleet/nodes?summary=true`: a fleet-sized answer in a few hundred bytes.
pub(super) fn summarize<'a>(nodes: impl Iterator<Item = &'a super::NodeInfo>) -> serde_json::Value {
    let (mut total, mut healthy, mut cordoned, mut outdated, mut unreachable) = (0, 0, 0, 0, 0);
    let mut versions: std::collections::BTreeMap<String, usize> = Default::default();
    for n in nodes {
        total += 1;
        healthy += n.healthy() as usize;
        cordoned += n.cordoned as usize;
        outdated += n.outdated as usize;
        unreachable += (!n.daemon_reachable()) as usize;
        *versions
            .entry(if n.agent_version.is_empty() {
                "unknown".into()
            } else {
                n.agent_version.clone()
            })
            .or_default() += 1;
    }
    json!({"summary": {"total": total, "healthy": healthy, "unhealthy": total - healthy, "cordoned": cordoned,
                       "outdated": outdated, "daemon_unreachable": unreachable, "by_agent_version": versions}})
}

/// Advice to lengthen the heartbeat interval once the fleet is big: none up to 2,000 nodes (a node keeps its own
/// setting), then 11 s and up, never more than 15 s (the stale window is 30 s). Nodes that predate the field ignore
/// it, and a node never shortens its own interval because of it.
pub(super) fn next_interval_secs(nodes: usize) -> Option<u64> {
    (nodes > 2000).then(|| 10 + (nodes / 2000).min(5) as u64)
}

/// `a` is older than `b` comparing dotted numbers (`0.4.9` < `0.4.12`); non-numeric parts count as 0.
pub(super) fn version_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (mut x, mut y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x < y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(version_lt("0.4.9", "0.4.12"));
        assert!(!version_lt("0.4.12", "0.4.9"));
        assert!(!version_lt("0.4.12", "0.4.12"));
        assert!(version_lt("0.4", "0.4.1"));
        assert!(version_lt("v0.4.9", "0.5.0"));
    }

    #[test]
    fn interval_grows_slowly_and_is_capped() {
        assert_eq!(next_interval_secs(100), None);
        assert_eq!(next_interval_secs(2000), None);
        assert_eq!(next_interval_secs(4000), Some(12));
        assert_eq!(next_interval_secs(10_000), Some(15));
        assert_eq!(next_interval_secs(1_000_000), Some(15));
    }

    #[tokio::test]
    async fn bounded_runs_everything_with_a_cap_on_concurrency() {
        use std::sync::atomic::AtomicUsize;
        let (now, peak) = (
            std::sync::Arc::new(AtomicUsize::new(0)),
            std::sync::Arc::new(AtomicUsize::new(0)),
        );
        let out = bounded((0..300).collect::<Vec<u32>>(), |i| {
            let (now, peak) = (now.clone(), peak.clone());
            async move {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                i
            }
        })
        .await;
        assert_eq!(out.len(), 300);
        assert!(peak.load(Ordering::SeqCst) <= poll_concurrency());
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "it should actually run in parallel"
        );
    }

    #[test]
    fn bucket_refuses_past_the_burst_and_says_when_to_retry() {
        let a = Admission::new(10.0, 3.0, 100);
        for _ in 0..3 {
            assert!(a.admit().is_ok());
        }
        let wait = a.admit().unwrap_err();
        assert!((1..=4).contains(&wait), "retry-after {wait}");
        assert_eq!(a.rejected.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn inflight_limit_refuses_when_full() {
        let a = Admission::new(0.0, 0.0, 2);
        let (p1, p2) = (a.admit().unwrap(), a.admit().unwrap());
        assert!(a.admit().is_err());
        drop(p1);
        assert!(a.admit().is_ok());
        drop(p2);
    }

    async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .uri(uri)
            .header("authorization", "Bearer adm")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(json!(null)),
        )
    }

    #[tokio::test]
    async fn the_node_list_pages_filters_and_summarizes() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let app = super::super::router(super::super::CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("adm".into()),
            reconcile_interval: None,
            ha: None,
        });
        for i in 0..25 {
            let body = json!({"name": format!("n{i:02}"), "fluxvm_url": "http://x", "vcpus_total": 2, "memory_mib_total": 2048,
                "vm_count": 0, "labels": {"zone": if i % 2 == 0 { "a" } else { "b" }},
                "agent_version": if i < 10 { "0.4.9" } else { "0.4.12" }});
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/fleet/register")
                .header("authorization", "Bearer adm")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 200);
        }
        // No query: everything, as before.
        let (s, v) = get(&app, "/fleet/nodes").await;
        assert_eq!(
            (
                s,
                v["items"].as_array().unwrap().len(),
                v["truncated"].clone()
            ),
            (200, 25, json!(false))
        );
        // Paging walks the whole list once, in name order.
        let (mut seen, mut cursor) = (Vec::new(), String::new());
        loop {
            let uri = if cursor.is_empty() {
                "/fleet/nodes?limit=10".to_owned()
            } else {
                format!("/fleet/nodes?limit=10&cursor={cursor}")
            };
            let (_, v) = get(&app, &uri).await;
            seen.extend(
                v["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n["name"].as_str().unwrap().to_owned()),
            );
            assert_eq!(v["total"], 25);
            match v["next_cursor"].as_str() {
                Some(c) => cursor = c.to_owned(),
                None => break,
            }
        }
        assert_eq!(seen.len(), 25);
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        // Filters.
        let (_, v) = get(&app, "/fleet/nodes?label=zone%3Da").await;
        assert_eq!(v["total"], 13);
        let (_, v) = get(&app, "/fleet/nodes?version=0.4.9&healthy=true").await;
        assert_eq!(v["total"], 10);
        let (_, v) = get(&app, "/fleet/nodes?cordoned=true").await;
        assert_eq!(v["total"], 0);
        // Summary.
        let (_, v) = get(&app, "/fleet/nodes?summary=true").await;
        assert_eq!(v["summary"]["total"], 25);
        assert_eq!(v["summary"]["by_agent_version"]["0.4.12"], 15);
        // Bad input is a 400, not an empty list.
        assert_eq!(get(&app, "/fleet/nodes?limit=0").await.0, 400);
        assert_eq!(get(&app, "/fleet/nodes?bogus=1").await.0, 400);
        assert_eq!(get(&app, "/fleet/nodes?label=nokv").await.0, 400);
    }
}
