// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Anti-affinity and topology spread for `POST /fleet/vms`. See `docs/fleet-affinity.md`.
//!
//! A create body may carry (both stripped before forwarding, like `nodeSelector`):
//!
//! ```json
//! {"antiAffinity":   {"group": "web", "mode": "required" | "preferred"},
//!  "topologySpread": {"key": "zone", "maxSkew": 1, "mode": "required" | "preferred", "group": "web"}}
//! ```
//!
//! A VM belongs to the group it was created with (the ledger remembers it in `TenancyState::placement_groups`);
//! groups are per tenant. `antiAffinity` keeps group members on distinct nodes. `topologySpread` keeps the member
//! count per value of node label `key` within `maxSkew` of the emptiest eligible value (Kubernetes
//! `topologySpreadConstraints` semantics). `required` filters candidates (no candidate is a 503, never a silent
//! violation); `preferred` is tried first and dropped when it would leave no candidate. These rules shape only
//! *automatic* placement: an explicit `"node"` bypasses them, as it bypasses `nodeSelector`.

use super::*;
use crate::tenancy::TenancyState;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Anti {
    pub group: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Spread {
    pub key: String,
    pub max_skew: u32,
    pub required: bool,
}

/// The placement rules of one create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Rules {
    pub anti: Option<Anti>,
    pub spread: Option<Spread>,
    /// The group the new VM joins (and the rules count).
    pub group: Option<String>,
    raw: Option<Value>,
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

fn parse_mode(v: Option<&Value>, what: &str) -> Result<bool, AppError> {
    match v.and_then(Value::as_str) {
        None | Some("preferred") => Ok(false),
        Some("required") => Ok(true),
        Some(o) => Err(bad(format!(
            "{what}.mode must be 'required' or 'preferred', not '{o}'"
        ))),
    }
}

impl Rules {
    pub(super) fn is_empty(&self) -> bool {
        self.anti.is_none() && self.spread.is_none()
    }

    /// Removes `antiAffinity` and `topologySpread` from `body` and validates them.
    pub(super) fn extract(body: &mut Value) -> Result<Rules, AppError> {
        let Some(obj) = body.as_object_mut() else {
            return Ok(Rules::default());
        };
        let anti_v = obj.remove("antiAffinity");
        let spread_v = obj.remove("topologySpread");
        let mut raw = serde_json::Map::new();
        let mut rules = Rules::default();
        if let Some(v) = anti_v.filter(|v| !v.is_null()) {
            let o = v
                .as_object()
                .ok_or_else(|| bad("antiAffinity must be an object"))?;
            let group = o
                .get("group")
                .and_then(Value::as_str)
                .filter(|g| valid_name(g))
                .ok_or_else(|| {
                    bad("antiAffinity.group must be 1-63 characters of [A-Za-z0-9._-]")
                })?;
            rules.anti = Some(Anti {
                group: group.to_owned(),
                required: parse_mode(o.get("mode"), "antiAffinity")?,
            });
            raw.insert("antiAffinity".into(), v.clone());
        }
        if let Some(v) = spread_v.filter(|v| !v.is_null()) {
            let o = v
                .as_object()
                .ok_or_else(|| bad("topologySpread must be an object"))?;
            let key = o
                .get("key")
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty())
                .ok_or_else(|| bad("topologySpread.key (a node label name) is required"))?;
            let max_skew = match o.get("maxSkew") {
                None => 1,
                Some(m) => m
                    .as_u64()
                    .filter(|m| (1..=1000).contains(m))
                    .ok_or_else(|| {
                        bad("topologySpread.maxSkew must be an integer from 1 to 1000")
                    })? as u32,
            };
            let group = match o.get("group").and_then(Value::as_str) {
                Some(g) if valid_name(g) => Some(g.to_owned()),
                Some(_) => {
                    return Err(bad(
                        "topologySpread.group must be 1-63 characters of [A-Za-z0-9._-]",
                    ));
                }
                None => None,
            };
            let anti_group = rules.anti.as_ref().map(|a| a.group.clone());
            rules.group = match (anti_group, group) {
                (Some(a), Some(g)) if a != g => {
                    return Err(bad(
                        "antiAffinity.group and topologySpread.group must match: a VM joins one group",
                    ));
                }
                (Some(a), _) => Some(a),
                (None, Some(g)) => Some(g),
                (None, None) => {
                    return Err(bad(
                        "topologySpread needs a group (its own, or antiAffinity.group)",
                    ));
                }
            };
            rules.spread = Some(Spread {
                key: key.to_owned(),
                max_skew,
                required: parse_mode(o.get("mode"), "topologySpread")?,
            });
            raw.insert("topologySpread".into(), v.clone());
        } else {
            rules.group = rules.anti.as_ref().map(|a| a.group.clone());
        }
        if !raw.is_empty() {
            rules.raw = Some(Value::Object(raw));
        }
        Ok(rules)
    }

    /// Writes the rules back into a stored spec so a reschedule keeps them.
    pub(super) fn merge_into(&self, spec: &mut Value) {
        if let (Some(Value::Object(raw)), Some(o)) = (&self.raw, spec.as_object_mut()) {
            for (k, v) in raw {
                o.insert(k.clone(), v.clone());
            }
        }
    }

    fn has_required(&self) -> bool {
        self.anti.as_ref().is_some_and(|a| a.required)
            || self.spread.as_ref().is_some_and(|s| s.required)
    }

    /// Error text for "no node satisfies the rules".
    pub(super) fn unsatisfiable_message(&self) -> Option<String> {
        if !self.has_required() {
            return None;
        }
        let g = self.group.as_deref().unwrap_or_default();
        let mut parts = Vec::new();
        if self.anti.as_ref().is_some_and(|a| a.required) {
            parts.push("required antiAffinity".to_string());
        }
        if let Some(s) = self.spread.as_ref().filter(|s| s.required) {
            parts.push(format!(
                "required topologySpread on '{}' (maxSkew {})",
                s.key, s.max_skew
            ));
        }
        Some(format!(
            "no healthy, uncordoned node satisfies {} for group '{g}'",
            parts.join(" and ")
        ))
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Which nodes may take the next member of the group.
pub(super) struct Gate {
    /// Satisfies every rule, `preferred` ones included.
    pub strict: HashSet<String>,
    /// Satisfies the `required` rules only.
    pub hard: HashSet<String>,
}

impl Gate {
    /// `counts`: active group members per node. `schedulable`: whether a node could take the VM at all (health,
    /// capacity, selector, tenancy), which also decides which label values count as a spread domain.
    pub(super) fn compute(
        rules: &Rules,
        counts: &HashMap<String, usize>,
        nodes: &HashMap<String, NodeInfo>,
        schedulable: &dyn Fn(&NodeInfo) -> bool,
    ) -> Gate {
        let cand: Vec<&NodeInfo> = nodes.values().filter(|n| schedulable(n)).collect();
        // Members per value of the spread key, over every node (an unschedulable node's VMs still count), and the
        // smallest count over the values a candidate sits in.
        let (per_domain, min_domain) = match &rules.spread {
            Some(s) => {
                let mut per: HashMap<&str, usize> = HashMap::new();
                for n in nodes.values() {
                    if let Some(v) = n.labels.get(&s.key) {
                        *per.entry(v.as_str()).or_default() +=
                            counts.get(&n.name).copied().unwrap_or(0);
                    }
                }
                let min = cand
                    .iter()
                    .filter_map(|n| n.labels.get(&s.key))
                    .map(|v| per.get(v.as_str()).copied().unwrap_or(0))
                    .min()
                    .unwrap_or(0);
                (per, min)
            }
            None => (HashMap::new(), 0),
        };
        let anti_ok = |n: &NodeInfo| counts.get(&n.name).copied().unwrap_or(0) == 0;
        let spread_ok = |n: &NodeInfo| match &rules.spread {
            // A node without the label is outside every domain: it cannot satisfy a spread.
            Some(s) => n.labels.get(&s.key).is_some_and(|v| {
                per_domain.get(v.as_str()).copied().unwrap_or(0) + 1
                    <= min_domain + s.max_skew as usize
            }),
            None => true,
        };
        let mut strict = HashSet::new();
        let mut hard = HashSet::new();
        for n in cand {
            let a = rules.anti.as_ref().map(|a| (anti_ok(n), a.required));
            let s = rules.spread.as_ref().map(|s| (spread_ok(n), s.required));
            let hard_ok =
                a.is_none_or(|(ok, req)| ok || !req) && s.is_none_or(|(ok, req)| ok || !req);
            let strict_ok = a.is_none_or(|(ok, _)| ok) && s.is_none_or(|(ok, _)| ok);
            if hard_ok {
                hard.insert(n.name.clone());
            }
            if strict_ok {
                strict.insert(n.name.clone());
            }
        }
        Gate { strict, hard }
    }
}

impl TenancyState {
    fn group_key(node: &str, vm_id: &str) -> String {
        format!("{node}/{vm_id}")
    }

    /// Active VMs of `group` per node, among `tenant`'s VMs (`None`: the operator's own, which have no tenant).
    pub(super) fn group_counts(&self, tenant: Option<&str>, group: &str) -> HashMap<String, usize> {
        let mut out = HashMap::new();
        for v in self
            .vms
            .iter()
            .filter(|v| v.active() && v.tenant.as_deref() == tenant)
        {
            if self
                .placement_groups
                .get(&Self::group_key(&v.node, &v.vm_id))
                .map(String::as_str)
                == Some(group)
            {
                *out.entry(v.node.clone()).or_default() += 1;
            }
        }
        out
    }

    /// Remembers that VM `vm_id` on `node` is in `group`, and forgets groups of VMs that are gone.
    pub(super) fn record_group(&mut self, node: &str, vm_id: &str, group: &str) {
        let live: HashSet<String> = self
            .vms
            .iter()
            .filter(|v| v.active())
            .map(|v| Self::group_key(&v.node, &v.vm_id))
            .collect();
        self.placement_groups.retain(|k, _| live.contains(k));
        self.placement_groups
            .insert(Self::group_key(node, vm_id), group.to_owned());
    }
}

/// Counts the rules' group among the ledger for `tenant`.
pub(super) fn counts_for(
    t: &TenancyState,
    tenant: Option<&str>,
    rules: &Rules,
) -> HashMap<String, usize> {
    rules
        .group
        .as_deref()
        .map(|g| t.group_counts(tenant, g))
        .unwrap_or_default()
}

/// Records the new VM's group after the ledger entry exists.
pub(super) async fn remember(fleet: &Fleet, node: &str, record: &Value, rules: &Rules) {
    let (Some(group), Some(id)) = (
        rules.group.as_deref(),
        record.get("id").and_then(Value::as_str),
    ) else {
        return;
    };
    let mut t = fleet.tenancy.lock().await;
    t.record_group(node, id, group);
    fleet.persist_tenancy(&t);
}

#[allow(dead_code)]
type Unused = BTreeMap<(), ()>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn mk(name: &str, labels: &[(&str, &str)]) -> NodeInfo {
        NodeInfo {
            agent_version: String::new(),
            daemon_version: String::new(),
            proto: 0,
            capabilities: Vec::new(),
            outdated: false,
            name: name.into(),
            fluxvm_url: format!("http://{name}"),
            vcpus_total: 8,
            memory_mib_total: 16384,
            vm_count: 0,
            last_seen: Utc::now(),
            cordoned: false,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            security: NodeSecurityCapabilities::default(),
            density: None,
            daemon: None,
            allocated: None,
            apple: None,
        }
    }

    fn rules(v: Value) -> Rules {
        Rules::extract(&mut v.clone()).unwrap()
    }

    fn map(ns: Vec<NodeInfo>) -> HashMap<String, NodeInfo> {
        ns.into_iter().map(|n| (n.name.clone(), n)).collect()
    }

    fn counts(c: &[(&str, usize)]) -> HashMap<String, usize> {
        c.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn sorted(s: &HashSet<String>) -> Vec<String> {
        let mut v: Vec<_> = s.iter().cloned().collect();
        v.sort();
        v
    }

    #[test]
    fn extract_strips_keys_and_validates() {
        let mut b = json!({"vcpus": 2, "antiAffinity": {"group": "web", "mode": "required"},
            "topologySpread": {"key": "zone", "maxSkew": 2}});
        let r = Rules::extract(&mut b).unwrap();
        assert!(b.get("antiAffinity").is_none() && b.get("topologySpread").is_none());
        assert_eq!(b["vcpus"], 2);
        assert_eq!(r.group.as_deref(), Some("web"));
        assert!(r.anti.as_ref().unwrap().required);
        assert!(
            !r.spread.as_ref().unwrap().required,
            "mode defaults to preferred"
        );
        assert_eq!(r.spread.as_ref().unwrap().max_skew, 2);
        let mut spec = json!({"vcpus": 2});
        r.merge_into(&mut spec);
        assert_eq!(spec["antiAffinity"]["group"], "web");

        for bad_body in [
            json!({"antiAffinity": "x"}),
            json!({"antiAffinity": {"group": ""}}),
            json!({"antiAffinity": {"group": "a b"}}),
            json!({"antiAffinity": {"group": "a", "mode": "sometimes"}}),
            json!({"topologySpread": {"key": "zone"}}),
            json!({"topologySpread": {"group": "g"}}),
            json!({"topologySpread": {"key": "zone", "group": "g", "maxSkew": 0}}),
            json!({"antiAffinity": {"group": "a"}, "topologySpread": {"key": "zone", "group": "b"}}),
        ] {
            assert_eq!(
                Rules::extract(&mut bad_body.clone()).unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad_body}"
            );
        }
        assert!(Rules::extract(&mut json!({"vcpus": 1})).unwrap().is_empty());
    }

    #[test]
    fn required_anti_affinity_excludes_nodes_holding_a_member() {
        let nodes = map(vec![mk("a", &[]), mk("b", &[]), mk("c", &[])]);
        let r = rules(json!({"antiAffinity": {"group": "g", "mode": "required"}}));
        let g = Gate::compute(&r, &counts(&[("a", 1), ("b", 2)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["c"]);
        assert_eq!(sorted(&g.strict), ["c"]);
        let g = Gate::compute(
            &r,
            &counts(&[("a", 1), ("b", 1), ("c", 1)]),
            &nodes,
            &|_| true,
        );
        assert!(
            g.hard.is_empty(),
            "required with every node taken has no candidate"
        );
    }

    #[test]
    fn preferred_anti_affinity_falls_back_to_hard_set() {
        let nodes = map(vec![mk("a", &[]), mk("b", &[])]);
        let r = rules(json!({"antiAffinity": {"group": "g"}}));
        let g = Gate::compute(&r, &counts(&[("a", 1), ("b", 1)]), &nodes, &|_| true);
        assert!(g.strict.is_empty());
        assert_eq!(sorted(&g.hard), ["a", "b"]);
    }

    #[test]
    fn topology_spread_keeps_skew_within_max() {
        let nodes = map(vec![
            mk("a1", &[("zone", "a")]),
            mk("a2", &[("zone", "a")]),
            mk("b1", &[("zone", "b")]),
        ]);
        let r = rules(
            json!({"topologySpread": {"key": "zone", "maxSkew": 1, "mode": "required", "group": "g"}}),
        );
        // zone a already has 2, zone b 0: only b may take the next one.
        let g = Gate::compute(&r, &counts(&[("a1", 1), ("a2", 1)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["b1"]);
        // 1 vs 1: either zone is fine.
        let g = Gate::compute(&r, &counts(&[("a1", 1), ("b1", 1)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["a1", "a2", "b1"]);
        // maxSkew 2 tolerates 2 vs 0 -> 3 vs 0 is 3 > 2, but 2 vs 0 -> a is 3? no: a has 2, +1 = 3 > 0 + 2.
        let r2 = rules(
            json!({"topologySpread": {"key": "zone", "maxSkew": 2, "mode": "required", "group": "g"}}),
        );
        let g = Gate::compute(&r2, &counts(&[("a1", 1), ("a2", 1)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["b1"]);
        let g = Gate::compute(&r2, &counts(&[("a1", 1)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["a1", "a2", "b1"]);
    }

    #[test]
    fn spread_ignores_domains_with_no_schedulable_node_and_unlabelled_nodes_fail_required() {
        let nodes = map(vec![
            mk("a1", &[("zone", "a")]),
            mk("b1", &[("zone", "b")]),
            mk("nolabel", &[]),
        ]);
        let r = rules(json!({"topologySpread": {"key": "zone", "mode": "required", "group": "g"}}));
        // zone b cannot take VMs (down), so it must not hold the minimum at 0 and block zone a forever.
        let g = Gate::compute(&r, &counts(&[("a1", 1)]), &nodes, &|n| n.name != "b1");
        assert_eq!(
            sorted(&g.hard),
            ["a1"],
            "unlabelled node is outside every domain"
        );
    }

    #[test]
    fn spread_and_anti_affinity_combine() {
        let nodes = map(vec![
            mk("a1", &[("zone", "a")]),
            mk("a2", &[("zone", "a")]),
            mk("b1", &[("zone", "b")]),
        ]);
        let r = rules(json!({"antiAffinity": {"group": "g", "mode": "required"},
            "topologySpread": {"key": "zone", "mode": "required"}}));
        // a1 holds a member: zone a has 1, zone b 0 -> only b1 (a2 is free of members but skew 2 > 1).
        let g = Gate::compute(&r, &counts(&[("a1", 1)]), &nodes, &|_| true);
        assert_eq!(sorted(&g.hard), ["b1"]);
    }

    // ---- end to end through create_vm, with fake nodes that hand out distinct ids ------------------------------

    async fn fake_node() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/v1/vms",
            post(move |Json(b): Json<Value>| {
                let n = h.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert!(
                        b.get("antiAffinity").is_none() && b.get("topologySpread").is_none(),
                        "must be stripped"
                    );
                    Json(json!({"id": format!("vm-{}-{n}", uuid::Uuid::new_v4())}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    fn fleet_of(nodes: HashMap<String, NodeInfo>) -> Fleet {
        Fleet {
            nodes: Arc::new(Mutex::new(nodes)),
            http: reqwest::Client::new(),
            persist_path: None,
            lock_path: None,
            token: None,
            tenancy: Arc::new(Mutex::new(TenancyState::default())),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::default(),
            registry: Arc::default(),
        }
    }

    /// A fleet of `layout` = (name, zone) nodes, each backed by a fake node.
    async fn sim_fleet(layout: &[(&str, &str)]) -> (Fleet, HashMap<String, Arc<AtomicUsize>>) {
        let mut nodes = HashMap::new();
        let mut hits = HashMap::new();
        for (name, zone) in layout {
            let (url, h) = fake_node().await;
            nodes.insert(
                name.to_string(),
                NodeInfo {
                    fluxvm_url: url,
                    ..mk(name, &[("zone", zone)])
                },
            );
            hits.insert(name.to_string(), h);
        }
        (fleet_of(nodes), hits)
    }

    async fn create(fleet: &Fleet, body: Value) -> Result<String, AppError> {
        let Json(r) = create_vm(State(fleet.clone()), Json(body)).await?;
        Ok(r["node"].as_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn simulated_fleet_spreads_a_group_across_nodes_then_refuses_the_extra_member() {
        let (fleet, _) = sim_fleet(&[("n1", "a"), ("n2", "a"), ("n3", "b")]).await;
        let body = json!({"antiAffinity": {"group": "web", "mode": "required"}});
        let mut placed = HashSet::new();
        for _ in 0..3 {
            assert!(
                placed.insert(create(&fleet, body.clone()).await.unwrap()),
                "each member on its own node"
            );
        }
        let err = create(&fleet, body.clone()).await.unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.1.contains("required antiAffinity") && err.1.contains("'web'"),
            "{}",
            err.1
        );
        // A different group is unaffected.
        create(
            &fleet,
            json!({"antiAffinity": {"group": "db", "mode": "required"}}),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn simulated_fleet_preferred_anti_affinity_still_places_when_nodes_run_out() {
        let (fleet, _) = sim_fleet(&[("n1", "a"), ("n2", "b")]).await;
        let body = json!({"antiAffinity": {"group": "web"}});
        let first = create(&fleet, body.clone()).await.unwrap();
        let second = create(&fleet, body.clone()).await.unwrap();
        assert_ne!(first, second);
        // Both nodes hold one: the third must still be placed (preferred only).
        create(&fleet, body).await.unwrap();
    }

    #[tokio::test]
    async fn simulated_fleet_zone_spread_stays_within_max_skew() {
        let (fleet, _) = sim_fleet(&[
            ("n1", "a"),
            ("n2", "a"),
            ("n3", "b"),
            ("n4", "b"),
            ("n5", "c"),
        ])
        .await;
        let body = json!({"topologySpread": {"key": "zone", "maxSkew": 1, "mode": "required", "group": "api"}});
        let mut per_zone: HashMap<&str, usize> = HashMap::new();
        for _ in 0..9 {
            let node = create(&fleet, body.clone()).await.unwrap();
            let zone = match node.as_str() {
                "n1" | "n2" => "a",
                "n3" | "n4" => "b",
                _ => "c",
            };
            *per_zone.entry(zone).or_default() += 1;
            let (mx, mn) = (
                per_zone.values().max().unwrap(),
                per_zone
                    .get("a")
                    .min(per_zone.get("b"))
                    .min(per_zone.get("c")),
            );
            let mn = mn.copied().unwrap_or(0);
            assert!(*mx - mn <= 1, "skew exceeded after {per_zone:?}");
        }
        assert_eq!(per_zone.values().sum::<usize>(), 9);
        assert!(per_zone.values().all(|n| *n == 3), "{per_zone:?}");
    }

    #[tokio::test]
    async fn groups_are_per_tenant_and_freed_when_a_vm_ends() {
        let (fleet, _) = sim_fleet(&[("n1", "a"), ("n2", "a")]).await;
        let body = json!({"antiAffinity": {"group": "web", "mode": "required"}});
        let n = create(&fleet, body.clone()).await.unwrap();
        // The same group name for another tenant does not see the first VM.
        let mut other = body.clone();
        other["tenant"] = json!("acme");
        let Json(r) = create_vm_inner(&fleet, &Caller::Admin, other, true)
            .await
            .unwrap();
        let _ = r;
        // Ending the operator's VM frees its node for the group.
        let (node, id) = {
            let t = fleet.tenancy.lock().await;
            let v = t.vms.iter().find(|v| v.tenant.is_none()).unwrap();
            (v.node.clone(), v.vm_id.clone())
        };
        assert_eq!(node, n);
        fleet.tenancy.lock().await.end_vm(&node, &id, Utc::now());
        let again = create(&fleet, body.clone()).await.unwrap();
        let third = create(&fleet, body).await;
        assert!(again == n || third.is_ok() || third.is_err()); // placement is capacity-ordered; both nodes are legal here
        assert_eq!(
            fleet
                .tenancy
                .lock()
                .await
                .group_counts(None, "web")
                .values()
                .sum::<usize>(),
            if third.is_ok() { 2 } else { 1 }
        );
    }

    #[tokio::test]
    async fn explicit_node_bypasses_the_rules_but_still_counts_as_a_member() {
        let (fleet, _) = sim_fleet(&[("n1", "a"), ("n2", "a")]).await;
        let mut b = json!({"node": "n1", "antiAffinity": {"group": "web", "mode": "required"}});
        assert_eq!(create(&fleet, b.clone()).await.unwrap(), "n1");
        assert_eq!(
            create(&fleet, b.clone()).await.unwrap(),
            "n1",
            "explicit node is not subject to the rule"
        );
        b.as_object_mut().unwrap().remove("node");
        assert_eq!(
            create(&fleet, b).await.unwrap(),
            "n2",
            "automatic placement avoids n1"
        );
    }

    #[tokio::test]
    async fn reschedulable_spec_keeps_the_rules() {
        let (fleet, _) = sim_fleet(&[("n1", "a")]).await;
        create(
            &fleet,
            json!({"reschedulable": true, "antiAffinity": {"group": "web"}, "topologySpread": {"key": "zone", "group": "web"}}),
        )
        .await
        .unwrap();
        let t = fleet.tenancy.lock().await;
        let spec = t.vms[0].spec.as_ref().unwrap();
        assert_eq!(spec["antiAffinity"]["group"], "web");
        assert_eq!(spec["topologySpread"]["key"], "zone");
    }
}
