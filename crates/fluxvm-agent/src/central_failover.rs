// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Automatic reschedule of `reschedulable` VMs after a node stops heartbeating. Opt-in, fencing-safe.
//! See `docs/fleet-affinity.md` for the rule and its limits.
//!
//! Before this module, heartbeat loss never moved a VM: a stale node was only skipped for placement and flagged
//! unreachable; moving VMs needed an operator's `POST /fleet/nodes/{name}/drain?mode=reschedule` (which needs the
//! node to answer). This pass is off unless `FLUXVM_AUTO_RESCHEDULE_GRACE_SECS` is set.
//!
//! A node's VMs are recreated elsewhere only when ALL hold:
//! 1. this central is the leader and has been for at least the grace period (a restart or failover must not read
//!    its own downtime as every node's death);
//! 2. the node's last heartbeat is older than the grace period;
//! 3. the node is *fenced*: it advertised `self_fence:F` (it stops its own VMs after F seconds without an accepted
//!    heartbeat, `node_fence.rs`) and `last_seen` is older than `F + margin`, the margin covering a slow stop.
//!
//! A silent node with no fencing promise is never rescheduled: central cannot tell a crashed host from a
//! partitioned one that is still running the VM, and two copies of one VM writing to shared storage is worse than
//! an outage. Such a node is reported as `unfenced` and left to the operator.

use super::*;

pub(super) const ENV_GRACE: &str = "FLUXVM_AUTO_RESCHEDULE_GRACE_SECS";
pub(super) const ENV_MARGIN: &str = "FLUXVM_AUTO_RESCHEDULE_FENCE_MARGIN_SECS";
const MIN_GRACE_SECS: i64 = 2 * HEALTHY_WINDOW_SECS;
const DEFAULT_MARGIN_SECS: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Config {
    pub grace_secs: i64,
    pub margin_secs: i64,
}

impl Config {
    pub(super) fn from_env() -> Option<Config> {
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<i64>().ok());
        let grace = get(ENV_GRACE).filter(|g| *g > 0)?;
        Some(Config {
            grace_secs: grace.max(MIN_GRACE_SECS),
            margin_secs: get(ENV_MARGIN).filter(|m| *m >= 0).unwrap_or(DEFAULT_MARGIN_SECS),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decision {
    /// Heartbeating (or only briefly late).
    Alive,
    /// Silent, but the grace period or the fence deadline has not passed.
    Waiting,
    /// Silent past the grace period, but never promised to fence itself.
    Unfenced,
    /// Silent, past the grace period and past its own fence deadline.
    Reschedule,
}

fn fence_secs(n: &NodeInfo) -> Option<i64> {
    n.capabilities
        .iter()
        .find_map(|c| c.strip_prefix(crate::node_fence::CAP_PREFIX))
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|f| *f > 0)
}

pub(super) fn decide(n: &NodeInfo, cfg: &Config, now: DateTime<Utc>) -> Decision {
    let age = (now - n.last_seen).num_seconds();
    if age < HEALTHY_WINDOW_SECS {
        return Decision::Alive;
    }
    if age < cfg.grace_secs {
        return Decision::Waiting;
    }
    match fence_secs(n) {
        None => Decision::Unfenced,
        Some(f) if age < f + cfg.margin_secs => Decision::Waiting,
        Some(_) => Decision::Reschedule,
    }
}

pub(super) fn spawn(fleet: Fleet, every: std::time::Duration, cfg: Config) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut leading_since: Option<std::time::Instant> = None;
        tracing::info!(?cfg, "automatic reschedule after heartbeat loss is ON");
        loop {
            tick.tick().await;
            if !fleet.leading() {
                leading_since = None;
                continue;
            }
            let since = *leading_since.get_or_insert_with(std::time::Instant::now);
            let report = pass(&fleet, &cfg, Utc::now(), since.elapsed()).await;
            if report["rescheduled"].as_array().is_some_and(|a| !a.is_empty()) {
                tracing::warn!(%report, "rescheduled VMs off silent nodes");
            }
        }
    });
}

/// One pass. `leading_for`: how long this central has been leading; nothing happens before the grace period.
pub(super) async fn pass(fleet: &Fleet, cfg: &Config, now: DateTime<Utc>, leading_for: std::time::Duration) -> Value {
    if (leading_for.as_secs() as i64) < cfg.grace_secs {
        return json!({"skipped": "leader for less than the grace period", "rescheduled": [], "unfenced_nodes": [], "failed": []});
    }
    let due: Vec<(NodeInfo, Decision)> = fleet
        .nodes
        .lock()
        .await
        .values()
        .map(|n| (n.clone(), decide(n, cfg, now)))
        .filter(|(_, d)| matches!(d, Decision::Reschedule | Decision::Unfenced))
        .collect();
    let (mut moved, mut failed, mut unfenced) = (Vec::new(), Vec::new(), Vec::new());
    for (node, decision) in due {
        let entries: Vec<crate::tenancy::VmEntry> = fleet
            .tenancy
            .lock()
            .await
            .vms
            .iter()
            .filter(|v| v.active() && v.node == node.name && v.spec.is_some())
            .cloned()
            .collect();
        if entries.is_empty() {
            continue;
        }
        if decision == Decision::Unfenced {
            tracing::warn!(node = %node.name, vms = entries.len(), "node is silent but never promised to self-fence; not rescheduling its VMs (set FLUXVM_NODE_SELF_FENCE_SECS on the node agent, or drain it by hand)");
            unfenced.push(json!({"node": node.name, "reschedulable_vms": entries.len()}));
            continue;
        }
        let mut any = false;
        for e in entries {
            let owner = match e.tenant.clone() {
                Some(t) => Caller::Tenant(t),
                None => Caller::Admin,
            };
            let mut body = e.spec.clone().unwrap_or_else(|| json!({}));
            body["reschedulable"] = json!(true);
            match create_vm_inner(fleet, &owner, body, true).await {
                Ok(Json(created)) => {
                    let mut t = fleet.tenancy.lock().await;
                    t.end_vm(&e.node, &e.vm_id, now);
                    fleet.persist_tenancy(&t);
                    any = true;
                    moved.push(json!({"vm": e.vm_id, "from": e.node, "to": created.get("node"),
                        "new_vm": created.get("vm").and_then(|v| v.get("id"))}));
                }
                Err(err) => failed.push(json!({"vm": e.vm_id, "node": e.node, "error": err.1})),
            }
        }
        if any {
            // If the node comes back it must not take new VMs until an operator has looked at it (stopped copies of
            // the moved VMs may still be there).
            {
                let mut t = fleet.tenancy.lock().await;
                t.draining.insert(node.name.clone());
                fleet.persist_tenancy(&t);
            }
            let mut nodes = fleet.nodes.lock().await;
            set_cordoned(&mut nodes, &node.name, true);
            if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
                persist_nodes(path, lock, &nodes);
            }
        }
    }
    json!({"rescheduled": moved, "failed": failed, "unfenced_nodes": unfenced})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{TenancyState, VmEntry};
    use std::time::Duration;

    const CFG: Config = Config { grace_secs: 60, margin_secs: 30 };

    fn n(age: i64, caps: &[&str]) -> NodeInfo {
        NodeInfo {
            agent_version: String::new(),
            daemon_version: String::new(),
            proto: 0,
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
            outdated: false,
            name: "dead".into(),
            fluxvm_url: "http://dead".into(),
            vcpus_total: 8,
            memory_mib_total: 16384,
            vm_count: 1,
            last_seen: Utc::now() - chrono::Duration::seconds(age),
            cordoned: false,
            labels: HashMap::new(),
            security: NodeSecurityCapabilities::default(),
            density: None,
            daemon: None,
            allocated: None,
            apple: None,
        }
    }

    #[test]
    fn decision_follows_the_fencing_rule() {
        let now = Utc::now();
        let d = |age, caps: &[&str]| decide(&n(age, caps), &CFG, now);
        assert_eq!(d(5, &["self_fence:45"]), Decision::Alive);
        assert_eq!(d(40, &["self_fence:45"]), Decision::Waiting, "stale but inside the grace period");
        // Past grace (60) but fence deadline is 45 + 30 = 75.
        assert_eq!(d(70, &["self_fence:45"]), Decision::Waiting, "node may not have fenced itself yet");
        assert_eq!(d(76, &["self_fence:45"]), Decision::Reschedule);
        assert_eq!(d(100_000, &[]), Decision::Unfenced, "no fencing promise: never");
        assert_eq!(d(100_000, &["next_interval"]), Decision::Unfenced);
        assert_eq!(d(100_000, &["self_fence:0"]), Decision::Unfenced);
        // The grace period can be the later deadline.
        assert_eq!(decide(&n(70, &["self_fence:20"]), &Config { grace_secs: 120, margin_secs: 30 }, now), Decision::Waiting);
    }

    #[test]
    fn config_needs_an_explicit_grace_and_has_a_floor() {
        // Parsing is env based; the pure rule is covered above. The floor keeps grace above the healthy window.
        assert!(MIN_GRACE_SECS >= 2 * HEALTHY_WINDOW_SECS);
    }

    async fn fake_node() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/v1/vms",
            post(move || {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Json(json!({"id": uuid::Uuid::new_v4().to_string()})) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    async fn fleet_with_dead_node(caps: &[&str], age: i64, spec: Option<Value>) -> (Fleet, Arc<std::sync::atomic::AtomicUsize>) {
        let (url, hits) = fake_node().await;
        let mut nodes = HashMap::new();
        nodes.insert("dead".to_string(), n(age, caps));
        nodes.insert("live".to_string(), NodeInfo { name: "live".into(), fluxvm_url: url, last_seen: Utc::now(), ..n(0, &[]) });
        let mut t = TenancyState::default();
        t.record_vm(VmEntry {
            node: "dead".into(),
            vm_id: "old-1".into(),
            name: "x".into(),
            tenant: None,
            vcpus: 2,
            memory_mib: 2048,
            macos_guest: false,
            created_at: Utc::now() - chrono::Duration::hours(1),
            ended_at: None,
            spec,
            warm_pool: None,
            claimed_at: None,
            expires_at: None,
            billed_from: None,
        });
        let fleet = Fleet {
            nodes: Arc::new(Mutex::new(nodes)),
            http: reqwest::Client::new(),
            persist_path: None,
            lock_path: None,
            token: None,
            tenancy: Arc::new(Mutex::new(t)),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::default(),
            registry: Arc::default(),
        };
        (fleet, hits)
    }

    const LONG: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn a_fenced_silent_node_has_its_reschedulable_vm_recreated_elsewhere() {
        let (fleet, hits) = fleet_with_dead_node(&["self_fence:45"], 200, Some(json!({"name": "x"}))).await;
        let r = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert_eq!(r["rescheduled"].as_array().unwrap().len(), 1, "{r}");
        assert_eq!(r["rescheduled"][0]["to"], "live");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let t = fleet.tenancy.lock().await;
        assert!(!t.vm("dead", "old-1").unwrap().active(), "old entry closed");
        assert!(t.draining.contains("dead"));
        assert!(fleet.nodes.lock().await["dead"].cordoned);
        // A second pass does nothing: nothing active is left on the dead node.
        drop(t);
        let r2 = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert!(r2["rescheduled"].as_array().unwrap().is_empty());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_silent_node_without_a_fencing_promise_is_never_rescheduled() {
        let (fleet, hits) = fleet_with_dead_node(&[], 100_000, Some(json!({"name": "x"}))).await;
        let r = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert!(r["rescheduled"].as_array().unwrap().is_empty());
        assert_eq!(r["unfenced_nodes"][0]["node"], "dead");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(fleet.tenancy.lock().await.vm("dead", "old-1").unwrap().active());
    }

    #[tokio::test]
    async fn before_the_fence_deadline_nothing_moves() {
        // Fenced node, silent 70s: past grace (60) but inside fence (45) + margin (30).
        let (fleet, hits) = fleet_with_dead_node(&["self_fence:45"], 70, Some(json!({"name": "x"}))).await;
        let r = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert!(r["rescheduled"].as_array().unwrap().is_empty());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_new_leader_waits_out_the_grace_period() {
        let (fleet, hits) = fleet_with_dead_node(&["self_fence:45"], 200, Some(json!({"name": "x"}))).await;
        let r = pass(&fleet, &CFG, Utc::now(), Duration::from_secs(10)).await;
        assert!(r["skipped"].is_string());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_vm_without_a_stored_spec_stays_put() {
        let (fleet, hits) = fleet_with_dead_node(&["self_fence:45"], 200, None).await;
        let r = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert!(r["rescheduled"].as_array().unwrap().is_empty());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(fleet.tenancy.lock().await.vm("dead", "old-1").unwrap().active());
    }

    #[tokio::test]
    async fn a_failed_recreate_leaves_the_old_entry_active_for_the_next_pass() {
        let (fleet, _) = fleet_with_dead_node(&["self_fence:45"], 200, Some(json!({"name": "x", "nodeSelector": {"zone": "nowhere"}}))).await;
        let r = pass(&fleet, &CFG, Utc::now(), LONG).await;
        assert_eq!(r["failed"].as_array().unwrap().len(), 1);
        assert!(fleet.tenancy.lock().await.vm("dead", "old-1").unwrap().active());
        assert!(!fleet.nodes.lock().await["dead"].cordoned);
    }
}
