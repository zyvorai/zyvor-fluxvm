// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Leader election and state mirroring for one `fluxvm-agent central` (see [`crate::ha`] for the design).

use super::*;
use crate::ha::HaConfig;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

/// The HA state one central keeps.
pub(super) struct HaRuntime {
    pub(super) cfg: HaConfig,
    leader: std::sync::Mutex<Option<String>>,
    is_leader: AtomicBool,
    /// Serving also stops once this deadline (ms since `epoch`) passes without a renewal, so a leader whose elector
    /// is stuck (hung store call, starved task) stops before its lock can lapse and pass to another central.
    epoch: Instant,
    lease_until_ms: AtomicU64,
    stopped: AtomicBool,
    tx: mpsc::UnboundedSender<(&'static str, String)>,
}

impl HaRuntime {
    pub(super) fn new(
        cfg: HaConfig,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<(&'static str, String)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                cfg,
                leader: std::sync::Mutex::new(None),
                is_leader: AtomicBool::new(false),
                epoch: Instant::now(),
                lease_until_ms: AtomicU64::new(0),
                stopped: AtomicBool::new(false),
                tx,
            }),
            rx,
        )
    }

    pub(super) fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
            && (self.epoch.elapsed().as_millis() as u64)
                < self.lease_until_ms.load(Ordering::SeqCst)
    }

    /// After a successful renewal that started at `started`, the lock holds until at least `started + ttl`; serve
    /// until `margin` before that.
    fn extend_lease(&self, started: Instant, margin: std::time::Duration) {
        let until =
            started.saturating_duration_since(self.epoch) + self.cfg.ttl.saturating_sub(margin);
        self.lease_until_ms
            .store(until.as_millis() as u64, Ordering::SeqCst);
    }

    pub(super) fn leader(&self) -> Option<String> {
        self.leader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Queues a fenced write of state `name` (the latest queued value of a name wins).
    pub(super) fn mirror(&self, name: &'static str, json: String) {
        if self.is_leader() {
            let _ = self.tx.send((name, json));
        }
    }

    fn step_down(&self, why: &str) {
        if self.is_leader.swap(false, Ordering::SeqCst) {
            tracing::warn!(reason = why, "stepping down as fleet leader");
        }
    }
}

/// Stops one central's HA loop without releasing its lock, as a crash would. Tests use it to force a failover.
#[derive(Clone)]
pub struct HaHandle(pub(super) Arc<HaRuntime>);

impl HaHandle {
    pub fn kill(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
        self.0.is_leader.store(false, Ordering::SeqCst);
    }

    pub fn is_leader(&self) -> bool {
        self.0.is_leader()
    }

    /// Clean shutdown: stops serving and electing, then releases the lock (if held) so a standby takes over at once
    /// instead of after the TTL.
    pub async fn release(&self) -> anyhow::Result<()> {
        self.kill();
        self.0.cfg.store.release(&self.0.cfg.advertise_url).await
    }
}

/// Loads the mirrored state into this central (on promotion, before it serves anything).
async fn load_state(fleet: &Fleet, ha: &HaRuntime) -> anyhow::Result<()> {
    let store = &ha.cfg.store;
    if let Some(raw) = store.get("tenancy").await? {
        let state: TenancyState =
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("mirrored tenancy: {e}"))?;
        let mut t = fleet.tenancy.lock().await;
        *t = state;
        if let (Some(path), Some(lock)) = (&fleet.tenancy_path, &fleet.lock_path) {
            persist_json(path, lock, &*t);
        }
    }
    if let Some(raw) = store.get("nodes").await? {
        let nodes: HashMap<String, NodeInfo> =
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("mirrored nodes: {e}"))?;
        let mut n = fleet.nodes.lock().await;
        // The mirror is authoritative for which nodes exist: a node only in this central's own (older) registry was
        // deregistered by a later leader. Standbys take no heartbeats, so nothing newer is lost here.
        n.retain(|name, _| nodes.contains_key(name));
        // Keep any node that heartbeated here more recently than the mirror knows.
        for (name, info) in nodes {
            match n.get(&name) {
                Some(local) if local.last_seen >= info.last_seen => {
                    let cordoned = info.cordoned;
                    if let Some(l) = n.get_mut(&name) {
                        l.cordoned = cordoned;
                    }
                }
                _ => {
                    n.insert(name, info);
                }
            }
        }
        if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
            persist_nodes(path, lock, &n);
        }
    }
    Ok(())
}

pub(super) fn spawn(
    fleet: Fleet,
    ha: Arc<HaRuntime>,
    mut rx: mpsc::UnboundedReceiver<(&'static str, String)>,
) {
    // The writer: drains everything queued, keeps the latest value per state, writes each fenced.
    let writer_ha = ha.clone();
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut latest: HashMap<&'static str, String> = HashMap::new();
            latest.insert(first.0, first.1);
            while let Ok((k, v)) = rx.try_recv() {
                latest.insert(k, v);
            }
            for (name, json) in latest {
                if writer_ha.stopped.load(Ordering::SeqCst) {
                    return;
                }
                match writer_ha
                    .cfg
                    .store
                    .put_fenced(&writer_ha.cfg.advertise_url, name, &json)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => writer_ha.step_down("lost the leader lock while writing state"),
                    Err(e) => {
                        tracing::warn!(error = %e, state = name, "mirroring fleet state failed")
                    }
                }
            }
        }
    });

    // The elector: renew or take the lock every third of the TTL.
    tokio::spawn(async move {
        let me = ha.cfg.advertise_url.clone();
        let ttl = ha.cfg.ttl;
        let every = (ttl / 3).max(std::time::Duration::from_millis(50));
        loop {
            if ha.stopped.load(Ordering::SeqCst) {
                return;
            }
            let started = Instant::now();
            let res = match tokio::time::timeout(every, ha.cfg.store.acquire(&me, ttl)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("timed out after {every:?}")),
            };
            match res {
                Ok(holder) => {
                    let leading = holder == me;
                    *ha.leader.lock().unwrap_or_else(|e| e.into_inner()) = Some(holder);
                    if leading {
                        let was_serving = ha.is_leader();
                        ha.extend_lease(started, every);
                        if !was_serving {
                            // Newly leading (or our lease lapsed meanwhile): load what the store holds first.
                            ha.is_leader.store(false, Ordering::SeqCst);
                            match load_state(&fleet, &ha).await {
                                Ok(()) => {
                                    ha.is_leader.store(true, Ordering::SeqCst);
                                    tracing::info!(%me, "became fleet leader");
                                    // Seed the mirror with our tenancy, otherwise only written on a change.
                                    let t = serde_json::to_string(&*fleet.tenancy.lock().await);
                                    if let Ok(t) = t {
                                        ha.mirror("tenancy", t);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "holding the lock but could not load state; not serving")
                                }
                            }
                        }
                    } else {
                        ha.step_down("another central holds the lock");
                    }
                    if ha.is_leader() {
                        let snapshot = serde_json::to_string(&*fleet.nodes.lock().await);
                        if let Ok(s) = snapshot {
                            ha.mirror("nodes", s);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "fleet leader lock unreachable");
                    // Without the store there is no fencing: the lease deadline has already stopped serving before
                    // the lock could pass to another; this records it.
                    if !ha.is_leader() {
                        ha.step_down("leader lock store unreachable");
                    }
                }
            }
            tokio::time::sleep(every).await;
        }
    });
}

/// `GET /fleet/ha`: this central's view (no auth: it names URLs only, and load balancers poll it).
pub(super) async fn ha_status(State(fleet): State<Fleet>) -> Json<Value> {
    match &fleet.ha {
        Some(ha) => Json(json!({
            "enabled": true,
            "me": ha.cfg.advertise_url,
            "leader": ha.leader(),
            "is_leader": ha.is_leader(),
        })),
        None => Json(json!({"enabled": false, "is_leader": true})),
    }
}

/// `GET /fleet/ha/leader`: `200` on the leader, `503` elsewhere, for a load balancer's health check.
pub(super) async fn ha_leader_check(State(fleet): State<Fleet>) -> StatusCode {
    match &fleet.ha {
        Some(ha) if !ha.is_leader() => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    }
}

/// The answer a standby gives to everything else.
pub(super) fn not_leader(ha: &HaRuntime) -> Response {
    let leader = ha.leader();
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": "this central is a standby; send fleet requests to the leader",
            "code": "UNAVAILABLE",
            "leader": leader,
        })),
    )
        .into_response();
    if let Some(l) = leader
        && let Ok(v) = header::HeaderValue::from_str(&l)
    {
        resp.headers_mut().insert("x-fluxvm-leader", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::HaStore;
    use axum::body::Body;
    use tower::ServiceExt;

    async fn call(app: &Router, method: &str, uri: &str, token: &str, body: Value) -> (u16, Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn wait_for(cond: impl Fn() -> bool) {
        for _ in 0..300 {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        panic!("condition not reached");
    }

    fn central(store: &HaStore, url: &str, dir: &std::path::Path) -> (Router, HaHandle) {
        let (app, handle) = router_with_handle(CentralConfig {
            state_dir: dir.to_path_buf(),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: Some(HaConfig {
                store: store.clone(),
                advertise_url: url.into(),
                // Renewals run every ttl/3; 300 ms let one stalled renewal on a loaded CI runner cost the leader its lease.
                ttl: std::time::Duration::from_millis(1500),
            }),
        });
        (app, handle.expect("HA handle"))
    }

    #[tokio::test]
    async fn a_standby_takes_over_with_the_leaders_state() {
        let store = HaStore::memory("test");
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, ha) = central(&store, "http://central-a:7799", da.path());
        wait_for(|| ha.is_leader()).await;
        let (b, hb) = central(&store, "http://central-b:7799", db.path());
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!hb.is_leader());

        // The standby refuses fleet calls and names the leader; health and HA status still answer.
        let (s, v) = call(&b, "GET", "/fleet/nodes", "admin", json!({})).await;
        assert_eq!(s, 503, "{v}");
        assert_eq!(v["leader"], "http://central-a:7799");
        let (s, _) = call(&b, "GET", "/fleet/ha/leader", "", json!({})).await;
        assert_eq!(s, 503);
        let (s, _) = call(&a, "GET", "/fleet/ha/leader", "", json!({})).await;
        assert_eq!(s, 200);

        // State written on the leader is mirrored.
        let (s, v) = call(&a, "POST", "/fleet/tenants", "admin", json!({"id": "acme"})).await;
        assert_eq!(s, 201, "{v}");
        let token = v["token"].as_str().unwrap().to_owned();
        let (s, _) = call(
            &a,
            "POST",
            "/fleet/register",
            "admin",
            json!({"name": "mac-1", "fluxvm_url": "http://mac-1:7788", "vcpus_total": 10,
                   "memory_mib_total": 32768, "vm_count": 0}),
        )
        .await;
        assert_eq!(s, 200);
        // The mirror runs on the renewal tick (ttl/3 = 500 ms).
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // The leader dies without releasing the lock; the standby takes over after the TTL.
        ha.kill();
        wait_for(|| hb.is_leader()).await;
        let (s, v) = call(&b, "GET", "/fleet/tenants/me", &token, json!({})).await;
        assert_eq!(s, 200, "the tenant survived the failover: {v}");
        assert_eq!(v["id"], "acme");
        let (s, v) = call(&b, "GET", "/fleet/nodes/mac-1", "admin", json!({})).await;
        assert_eq!(s, 200, "the node survived the failover: {v}");
        // And the old leader, were it still running, could no longer write.
        assert!(
            !store
                .put_fenced("http://central-a:7799", "tenancy", "{}")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn the_console_is_served_without_a_token_with_a_strict_csp() {
        let d = tempfile::tempdir().unwrap();
        let app = router(CentralConfig {
            state_dir: d.path().to_path_buf(),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: None,
        });
        for path in ["/console/", "/console/assets/app.js", "/console/hosts"] {
            let req = axum::http::Request::builder()
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
            let csp = resp.headers()["content-security-policy"].to_str().unwrap();
            assert!(csp.contains("default-src 'none'") && csp.contains("script-src 'self'"));
        }
        // `/console` redirects to the trailing-slash form (asset URLs are relative) and keeps the query.
        let req = axum::http::Request::builder()
            .uri("/console?payment_method=added")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 308);
        assert_eq!(resp.headers()["location"], "/console/?payment_method=added");
        // A stale asset URL is a real 404, never the HTML shell served as JavaScript.
        let req = axum::http::Request::builder()
            .uri("/console/assets/gone.js")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 404);
        // Only reads are exempt: a POST under /console is not served without a token.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/console/x")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), 401);
        // The API itself still needs the token.
        let (s, _) = call(&app, "GET", "/fleet/nodes", "wrong", json!({})).await;
        assert_eq!(s, 401);
    }

    #[tokio::test]
    async fn a_clean_shutdown_hands_over_at_once() {
        let store = HaStore::memory("test");
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // A long TTL: without the release, the standby would wait a minute.
        let mk = |url: &str, dir: &std::path::Path| {
            let (_, h) = router_with_handle(CentralConfig {
                state_dir: dir.to_path_buf(),
                token: Some("admin".into()),
                reconcile_interval: None,
                ha: Some(HaConfig {
                    store: store.clone(),
                    advertise_url: url.into(),
                    ttl: std::time::Duration::from_secs(60),
                }),
            });
            h.unwrap()
        };
        let a = mk("http://central-a:7799", da.path());
        wait_for(|| a.is_leader()).await;
        let b = mk("http://central-b:7799", db.path());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!b.is_leader());
        a.release().await.unwrap();
        assert!(!a.is_leader());
        // b's next renewal (every TTL/3 = 20 s) would be slow for a test; ask the store directly instead.
        assert_eq!(
            store
                .acquire("http://central-b:7799", std::time::Duration::from_secs(60))
                .await
                .unwrap(),
            "http://central-b:7799"
        );
    }

    #[tokio::test]
    async fn without_ha_everything_is_the_leader() {
        let d = tempfile::tempdir().unwrap();
        let (app, handle) = router_with_handle(CentralConfig {
            state_dir: d.path().to_path_buf(),
            token: None,
            reconcile_interval: None,
            ha: None,
        });
        assert!(handle.is_none());
        let (s, v) = call(&app, "GET", "/fleet/ha", "", json!({})).await;
        assert_eq!(s, 200);
        assert_eq!(v["enabled"], false);
        let (s, _) = call(&app, "GET", "/fleet/ha/leader", "", json!({})).await;
        assert_eq!(s, 200);
    }
}
