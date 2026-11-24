// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The `k8s` source of the merged event feed: the Kubernetes API events of the running clusters (the data behind
//! `GET /v1/kube/{cluster}/events`), read live and turned into feed lines.
//!
//! The feed must never be slowed or broken by a cluster, and a poller (Studio's Live switch asks every few seconds) must not
//! hammer an API server. So every cluster is read at most once per [`FRESH`] (or once per [`RETRY`] after a failure), with one
//! request of at most [`PER_CLUSTER`] events and a [`DEADLINE`], at most [`MAX_CLUSTERS`] clusters per request. A cluster that
//! cannot be read becomes a one-line note that says what to do next; the rest of the feed is untouched.

use crate::kube;
use fluxvm_core::model::VmStatus;
use fluxvm_scheduler::VmManager;
use fluxvm_scheduler::event_feed::{FeedEvent, from_k8s_event};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a successful read is reused.
pub(crate) const FRESH: Duration = Duration::from_secs(15);
/// How long a failed read is remembered before the cluster is tried again.
pub(crate) const RETRY: Duration = Duration::from_secs(60);
/// The longest one cluster may take.
const DEADLINE: Duration = Duration::from_secs(6);
/// Events asked of each API server.
const PER_CLUSTER: usize = 200;
/// Clusters read per request.
const MAX_CLUSTERS: usize = 8;

type Reading = Result<Vec<FeedEvent>, String>;

#[derive(Default)]
pub(crate) struct Cache(Mutex<HashMap<String, (Instant, Reading)>>);

impl Cache {
    fn get(&self, cluster: &str, now: Instant) -> Option<Reading> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (at, r) = map.get(cluster)?;
        let ttl = if r.is_ok() { FRESH } else { RETRY };
        (now.saturating_duration_since(*at) < ttl).then(|| r.clone())
    }

    fn put(&self, cluster: &str, now: Instant, r: Reading) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, (at, r)| {
            now.saturating_duration_since(*at) < if r.is_ok() { FRESH } else { RETRY }
        });
        map.insert(cluster.to_string(), (now, r));
    }
}

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Cache::default)
}

/// The note for a cluster that could not be read: what happened, what to do, and the code to look up.
pub(crate) fn unreachable_note(cluster: &str, why: &str) -> String {
    let why: String = why.chars().take(160).collect();
    format!(
        "Kubernetes events of `{cluster}` are left out: the cluster could not be reached ({why}). Check it with `fluxctl kube status --name {cluster}` (start it with `fluxctl kube up --name {cluster}`), then run this again, or leave k8s out of --source. more: fluxctl explain K8S_EVENTS_UNREACHABLE"
    )
}

async fn read_cluster(m: &Arc<VmManager>, name: &str) -> Reading {
    let target = kube::resolve(m, None, name).await.map_err(|e| e.message)?;
    let path = format!("/api/v1/events?limit={PER_CLUSTER}");
    let list = tokio::time::timeout(DEADLINE, kube::get_json(&target, &path))
        .await
        .map_err(|_| format!("no answer within {} seconds", DEADLINE.as_secs()))?
        .map_err(|e| e.message)?;
    let (events, _) = crate::kube_workloads::simplify_events(&list, PER_CLUSTER);
    Ok(events
        .iter()
        .filter_map(|e| from_k8s_event(name, e))
        .collect())
}

/// What the `k8s` source adds to one request.
#[derive(Default)]
pub(crate) struct K8s {
    pub events: Vec<FeedEvent>,
    pub notes: Vec<String>,
}

/// The Kubernetes events of `only` (or of every running cluster), and a note per cluster that could not be read.
/// `asked` is true when the caller named the source: then an empty host or an unknown cluster is worth a note too, while
/// under `all` a host without clusters stays quiet.
pub(crate) async fn gather(m: &Arc<VmManager>, only: Option<&str>, asked: bool) -> K8s {
    let all = m.list().await;
    let groups = kube::group(&all);
    let mut names: Vec<String> = groups.keys().cloned().collect();
    let mut out = K8s::default();
    if let Some(c) = only {
        if names.iter().any(|n| n == c) {
            names = vec![c.to_string()];
        } else {
            out.notes.push(unreachable_note(
                c,
                "no cluster with that name on this host",
            ));
            return out;
        }
    } else if names.is_empty() {
        if asked {
            out.notes.push(
                "There are no Kubernetes clusters on this host, so there are no k8s events. Create one with `fluxctl kube up --name NAME`, then run this again. more: fluxctl explain K8S_EVENTS_UNREACHABLE".into(),
            );
        }
        return out;
    }
    // Under `all`, a cluster whose server is not running is skipped quietly: it is simply not there to ask.
    if only.is_none() && !asked {
        names.retain(|n| {
            groups.get(n).is_some_and(|v| {
                v.iter()
                    .any(|vm| vm.name == *n && vm.status == VmStatus::Running)
            })
        });
    }
    let skipped = names.len().saturating_sub(MAX_CLUSTERS);
    names.truncate(MAX_CLUSTERS);
    let reads = futures::future::join_all(names.iter().map(|n| async move {
        if let Some(r) = cache().get(n, Instant::now()) {
            return r;
        }
        let r = read_cluster(m, n).await;
        cache().put(n, Instant::now(), r.clone());
        r
    }))
    .await;
    for (n, r) in names.iter().zip(reads) {
        match r {
            Ok(ev) => out.events.extend(ev),
            Err(why) => out.notes.push(unreachable_note(n, &why)),
        }
    }
    if skipped > 0 {
        out.notes.push(format!(
            "{skipped} more clusters were not read (at most {MAX_CLUSTERS} per request). Name one with --cluster NAME."
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> FeedEvent {
        FeedEvent {
            ts: chrono::Utc::now(),
            source: "k8s".into(),
            event: "k8s.X".into(),
            summary: s.into(),
            level: "info".into(),
            target: Some("lab".into()),
            vm_id: None,
            fields: Default::default(),
        }
    }

    #[test]
    fn a_read_is_reused_until_it_is_stale_and_a_failure_waits_longer() {
        let c = Cache::default();
        let t0 = Instant::now();
        c.put("lab", t0, Ok(vec![line("a")]));
        c.put("bad", t0, Err("down".into()));
        let soon = t0 + Duration::from_secs(5);
        assert_eq!(c.get("lab", soon).unwrap().unwrap().len(), 1);
        assert!(c.get("bad", soon).unwrap().is_err());
        let later = t0 + FRESH + Duration::from_secs(1);
        assert!(c.get("lab", later).is_none(), "good reads expire");
        assert!(
            c.get("bad", later).is_some(),
            "failures are not retried yet"
        );
        assert!(c.get("bad", t0 + RETRY + Duration::from_secs(1)).is_none());
        assert!(c.get("other", soon).is_none());
    }

    #[test]
    fn stale_entries_are_dropped_when_a_new_one_is_stored() {
        let c = Cache::default();
        let t0 = Instant::now();
        c.put("old", t0, Ok(vec![]));
        c.put("new", t0 + RETRY + Duration::from_secs(1), Ok(vec![]));
        assert_eq!(c.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_note_says_what_to_do_and_names_the_code() {
        let n = unreachable_note("lab", "the server VM lab is stopped");
        assert!(n.contains("`lab`") && n.contains("is stopped"));
        assert!(
            n.contains("fluxctl kube up --name lab")
                && n.ends_with("fluxctl explain K8S_EVENTS_UNREACHABLE")
        );
        assert!(!n.contains('\n'), "one line");
        let long = unreachable_note("lab", &"x".repeat(1000));
        assert!(long.len() < 500);
    }
}
