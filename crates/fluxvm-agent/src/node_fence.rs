// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Node-side self-fencing (opt-in): `FLUXVM_NODE_SELF_FENCE_SECS=N`. A node that has had no successful heartbeat for
//! `N` seconds stops every VM on its own daemon, and advertises `self_fence:N` in its heartbeat. That promise is
//! what lets central's automatic reschedule (`central_failover.rs`) start a replacement without risking two copies
//! of one VM. See `docs/fleet-affinity.md`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ENV: &str = "FLUXVM_NODE_SELF_FENCE_SECS";
/// Below this a few slow heartbeats would stop a healthy node's VMs.
pub const MIN_SECS: u64 = 20;
pub const CAP_PREFIX: &str = "self_fence:";

/// The configured fence delay, or `None` when self-fencing is off (unset, empty or 0).
pub fn configured() -> Option<Duration> {
    let n: u64 = std::env::var(ENV).ok()?.trim().parse().ok()?;
    (n > 0).then(|| Duration::from_secs(n.max(MIN_SECS)))
}

pub fn capability(after: Duration) -> String {
    format!("{CAP_PREFIX}{}", after.as_secs())
}

/// Time since the last successful heartbeat, shared between the heartbeat loop and the watchdog task.
#[derive(Clone)]
pub struct Beat(Arc<Mutex<Instant>>);

impl Beat {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }
    pub fn ok(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
    fn since(&self) -> Duration {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).elapsed()
    }
}

impl Default for Beat {
    fn default() -> Self {
        Self::new()
    }
}

/// Polls once a second; fences once per outage, and re-arms on the next successful heartbeat.
pub fn spawn_watchdog(beat: Beat, after: Duration, http: reqwest::Client, fluxvm_url: String) {
    tokio::spawn(async move {
        let mut fenced = false;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let silent = beat.since();
            if silent < after {
                fenced = false;
            } else if !fenced {
                tracing::error!(silent_secs = silent.as_secs(), "no heartbeat accepted; self-fencing: stopping local VMs");
                let (stopped, failed) = fence(&http, &fluxvm_url).await;
                tracing::error!(stopped, failed, "self-fence finished");
                // Retry next second while any stop failed: the promise to central is "stopped", not "tried".
                fenced = failed == 0;
            }
        }
    });
}

/// Stops every active VM the local daemon lists. Returns (stopped, failed).
pub async fn fence(http: &reqwest::Client, fluxvm_url: &str) -> (usize, usize) {
    let list = match http.get(format!("{fluxvm_url}/v1/vms")).send().await {
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok(),
        _ => None,
    };
    let Some(items) = list.as_ref().and_then(|v| v.get("items")).and_then(|v| v.as_array()).cloned() else {
        return (0, 1);
    };
    let (mut stopped, mut failed) = (0, 0);
    for vm in items.iter().filter(|v| crate::node::is_active(v)) {
        let Some(id) = vm.get("id").and_then(|v| v.as_str()) else {
            failed += 1;
            continue;
        };
        match http.post(format!("{fluxvm_url}/v1/vms/{id}/stop")).send().await {
            Ok(r) if r.status().is_success() => stopped += 1,
            _ => failed += 1,
        }
    }
    (stopped, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::{get, post}};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn capability_roundtrip_and_floor() {
        assert_eq!(capability(Duration::from_secs(45)), "self_fence:45");
    }

    #[tokio::test]
    async fn fence_stops_only_active_vms() {
        let stops = Arc::new(AtomicUsize::new(0));
        let s = stops.clone();
        let app = Router::new()
            .route(
                "/v1/vms",
                get(|| async {
                    Json(json!({"items": [
                        {"id": "a", "status": "running"},
                        {"id": "b", "status": "stopped"},
                        {"id": "c", "status": "paused"},
                    ]}))
                }),
            )
            .route(
                "/v1/vms/{id}/stop",
                post(move || {
                    s.fetch_add(1, Ordering::SeqCst);
                    async { Json(json!({})) }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        let (stopped, failed) = fence(&reqwest::Client::new(), &format!("http://{addr}")).await;
        assert_eq!((stopped, failed), (2, 0));
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unreachable_daemon_counts_as_failed_so_the_watchdog_retries() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        assert_eq!(fence(&reqwest::Client::new(), &format!("http://{addr}")).await, (0, 1));
    }
}
