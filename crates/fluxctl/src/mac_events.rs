// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Lifecycle events for `fluxctl mac` instances and `fluxctl kube apple` nodes, in the shape of the daemon's event log
//! (`{ts, event, vm_id?, fields}`), so they travel the same stream and the same signed webhooks as the daemon's `vm.*` events.
//!
//! The daemon already logs raw VM events (`vm.start`, `vm.stop`, ...) but knows nothing of what a VM *is* to fluxctl: which
//! image an instance came from, which compose project it belongs to, which k3s cluster and role a VM serves. `mac watch` polls
//! the daemon's VM list (and the local k3s journals), diffs two snapshots and emits one event per change. With `--record-to
//! DIR` the events are appended to `DIR/events.jsonl`, the file the daemon's `/v1/events`, `/v1/events/stream` and
//! `[[event_sinks]]` read, so a webhook with `events = ["mac.", "kube.apple."]` receives them. `mac events` reads them back.
//!
//! Events carry names, ids, states, images and `kairon.zyvor.dev/*` labels only. Nothing is copied from a VM's environment,
//! command, kubeconfig or the cluster token; the fields are chosen here, not forwarded wholesale.

use crate::kube_apple::model::{STACK_PREFIX, State, parse_service_label};
use crate::mac_inspect::{IMAGE_LABEL, INSTANCE_LABEL, PROJECT_LABEL, status};
use crate::stack::{L_SERVICE, L_STACK};
use anyhow::Result;
use chrono::{DateTime, Utc};
use fluxvm_scheduler::events::VmEvent;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

/// Apple's licence limit: a Mac runs at most two macOS VMs (running or paused ones hold a slot).
pub const MAX_SLOTS: usize = 2;
/// Labels with this prefix are the Kairon node/placement convention (see docs/kairon-mac-scheduling.md); they are passed on.
pub const KAIRON_PREFIX: &str = "kairon.zyvor.dev/";
/// The prefixes of every event name this module emits.
pub const MAC_PREFIX: &str = "mac.";
pub const KUBE_PREFIX: &str = "kube.apple.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacInstance {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub project: Option<String>,
    pub kairon: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KubeNode {
    pub cluster: String,
    pub node: String,
    pub role: String,
    pub id: String,
    pub status: String,
}

/// Everything one poll sees.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// By VM id.
    pub macs: BTreeMap<String, MacInstance>,
    /// By `(cluster, node)`.
    pub kube: BTreeMap<(String, String), KubeNode>,
    /// Cluster name to journal phase (`creating`, `bootstrapping`, `ready`).
    pub phases: BTreeMap<String, String>,
}

impl Snapshot {
    /// Slots held: running and paused macOS instances.
    pub fn slots_used(&self) -> usize {
        self.macs
            .values()
            .filter(|m| matches!(m.status.as_str(), "running" | "paused"))
            .count()
    }
}

fn label<'a>(vm: &'a Value, k: &str) -> Option<&'a str> {
    vm["labels"][k].as_str()
}

/// Builds a snapshot from the daemon's VM records and the local k3s journals.
pub fn snapshot(vms: &[Value], states: &[State]) -> Snapshot {
    let mut s = Snapshot::default();
    for st in states {
        let phase = serde_json::to_value(st.phase)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        s.phases.insert(st.name.clone(), phase);
    }
    for vm in vms {
        let Some(id) = vm["id"].as_str() else {
            continue;
        };
        if label(vm, INSTANCE_LABEL) == Some("instance") {
            let kairon = vm["labels"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| k.starts_with(KAIRON_PREFIX))
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect();
            s.macs.insert(
                id.to_owned(),
                MacInstance {
                    id: id.to_owned(),
                    name: vm["name"].as_str().unwrap_or("-").to_owned(),
                    image: label(vm, IMAGE_LABEL).unwrap_or("-").to_owned(),
                    status: status(vm).to_owned(),
                    project: label(vm, PROJECT_LABEL).map(str::to_owned),
                    kairon,
                },
            );
        }
        let cluster = label(vm, L_STACK).and_then(|l| l.strip_prefix(STACK_PREFIX));
        if let (Some(cluster), Some((role, node))) =
            (cluster, label(vm, L_SERVICE).and_then(parse_service_label))
        {
            s.kube.insert(
                (cluster.to_owned(), node.clone()),
                KubeNode {
                    cluster: cluster.to_owned(),
                    node,
                    role: role.as_str().to_owned(),
                    id: id.to_owned(),
                    status: status(vm).to_owned(),
                },
            );
        }
    }
    s
}

fn event(now: DateTime<Utc>, name: &str, id: Option<&str>, fields: Vec<(&str, String)>) -> VmEvent {
    VmEvent {
        ts: now,
        event: name.to_owned(),
        vm_id: id.and_then(|i| Uuid::parse_str(i).ok()),
        fields: fields
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    }
}

/// The event name for an instance moving from `from` to `to`.
fn transition(from: &str, to: &str) -> &'static str {
    match (from, to) {
        ("paused", "running") => "resumed",
        (_, "running") => "started",
        (_, "paused") => "paused",
        (_, "stopped") => "stopped",
        (_, "failed") => "failed",
        _ => "state",
    }
}

fn mac_event(
    now: DateTime<Utc>,
    name: &str,
    m: &MacInstance,
    node: Option<&str>,
    from: Option<&str>,
) -> VmEvent {
    let mut ev = event(
        now,
        name,
        Some(&m.id),
        vec![
            ("name", m.name.clone()),
            ("image", m.image.clone()),
            ("status", m.status.clone()),
            ("project", m.project.clone().unwrap_or_default()),
            ("node", node.unwrap_or_default().to_owned()),
            ("from", from.unwrap_or_default().to_owned()),
        ],
    );
    for (k, v) in &m.kairon {
        ev.fields.insert(k.clone(), v.clone());
    }
    ev
}

fn kube_event(
    now: DateTime<Utc>,
    name: &str,
    n: &KubeNode,
    host: Option<&str>,
    from: Option<&str>,
) -> VmEvent {
    event(
        now,
        name,
        Some(&n.id),
        vec![
            ("cluster", n.cluster.clone()),
            ("node", n.node.clone()),
            ("role", n.role.clone()),
            ("status", n.status.clone()),
            ("host", host.unwrap_or_default().to_owned()),
            ("from", from.unwrap_or_default().to_owned()),
        ],
    )
}

/// The events that turn `prev` into `cur`, in a stable order. `node` names this Mac in the fleet (`--node`).
pub fn diff(
    prev: &Snapshot,
    cur: &Snapshot,
    now: DateTime<Utc>,
    node: Option<&str>,
) -> Vec<VmEvent> {
    let mut out = vec![];
    for (id, m) in &cur.macs {
        match prev.macs.get(id) {
            None => out.push(mac_event(now, "mac.instance.created", m, node, None)),
            Some(p) if p.status != m.status => {
                let name = format!("mac.instance.{}", transition(&p.status, &m.status));
                out.push(mac_event(now, &name, m, node, Some(&p.status)));
            }
            Some(_) => {}
        }
    }
    for (id, p) in &prev.macs {
        if !cur.macs.contains_key(id) {
            out.push(mac_event(now, "mac.instance.removed", p, node, None));
        }
    }
    let (was, is) = (prev.slots_used(), cur.slots_used());
    let slots = |name: &str| {
        event(
            now,
            name,
            None,
            vec![
                ("used", is.to_string()),
                ("max", MAX_SLOTS.to_string()),
                ("node", node.unwrap_or_default().to_owned()),
            ],
        )
    };
    if was < MAX_SLOTS && is >= MAX_SLOTS {
        out.push(slots("mac.slots.full"));
    } else if was >= MAX_SLOTS && is < MAX_SLOTS {
        out.push(slots("mac.slots.free"));
    }

    for (k, n) in &cur.kube {
        match prev.kube.get(k) {
            None => out.push(kube_event(now, "kube.apple.node.created", n, node, None)),
            Some(p) if p.status != n.status => {
                let name = format!("kube.apple.node.{}", transition(&p.status, &n.status));
                out.push(kube_event(now, &name, n, node, Some(&p.status)));
            }
            Some(_) => {}
        }
    }
    for (k, p) in &prev.kube {
        if !cur.kube.contains_key(k) {
            out.push(kube_event(now, "kube.apple.node.removed", p, node, None));
        }
    }
    for (c, phase) in &cur.phases {
        let cluster = |from: Option<&String>| {
            vec![
                ("cluster", c.clone()),
                ("phase", phase.clone()),
                ("from", from.cloned().unwrap_or_default()),
            ]
        };
        match prev.phases.get(c) {
            None => out.push(event(
                now,
                "kube.apple.cluster.created",
                None,
                cluster(None),
            )),
            Some(old) if old != phase => out.push(event(
                now,
                "kube.apple.cluster.phase",
                None,
                cluster(Some(old)),
            )),
            Some(_) => {}
        }
    }
    for c in prev.phases.keys().filter(|c| !cur.phases.contains_key(*c)) {
        out.push(event(
            now,
            "kube.apple.cluster.removed",
            None,
            vec![("cluster", c.clone())],
        ));
    }
    out
}

/// True for the events this module produces.
pub fn ours(name: &str) -> bool {
    name.starts_with(MAC_PREFIX) || name.starts_with(KUBE_PREFIX)
}

/// One line per event, like `fluxctl events`.
pub fn render_line(ev: &VmEvent) -> String {
    format!(
        "{}  {}  {}  {}",
        ev.ts.to_rfc3339(),
        ev.event,
        ev.vm_id.map_or("-".into(), |i| i.to_string()),
        ev.fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Appends events to the log in `state_dir`, the file the daemon streams and the webhook sinks tail.
pub fn record(state_dir: &Path, events: &[VmEvent]) -> Result<()> {
    let log = fluxvm_scheduler::events::EventLog::new(state_dir);
    for e in events {
        log.append(e)?;
    }
    Ok(())
}

/// Where a poll gets its snapshot; a fake in tests.
#[allow(async_fn_in_trait)]
pub trait Source {
    async fn poll(&self) -> Result<Snapshot>;
}

pub struct DaemonSource<'a> {
    pub r: &'a crate::remote::Remote,
    /// The directory of the k3s journals (`kube apple`), when clusters should be watched too.
    pub kube_root: Option<std::path::PathBuf>,
}

impl Source for DaemonSource<'_> {
    async fn poll(&self) -> Result<Snapshot> {
        let vms = self.r.list_vms(None).await?;
        let states = match &self.kube_root {
            Some(root) => crate::kube_apple::model::Store::new(root.clone())
                .list()
                .unwrap_or_default(),
            None => vec![],
        };
        Ok(snapshot(&vms, &states))
    }
}

#[derive(Debug, Clone)]
pub struct WatchOpts {
    pub interval: Duration,
    /// Stop after this many polls (`None` = until interrupted).
    pub polls: Option<u32>,
    pub node: Option<String>,
    pub record_to: Option<std::path::PathBuf>,
}

/// Polls, diffs and emits; returns how many events it emitted. The first poll is the baseline and produces no events. A
/// failed poll is reported on stderr and the previous baseline is kept, so a daemon restart does not turn every VM into a
/// "removed" followed by a "created".
pub async fn watch<S: Source>(
    src: &S,
    o: &WatchOpts,
    mut emit: impl FnMut(&VmEvent),
    mut sleep: impl AsyncFnMut(Duration),
) -> Result<u32> {
    let mut prev: Option<Snapshot> = None;
    let mut polls = 0u32;
    let mut count = 0u32;
    loop {
        match src.poll().await {
            Ok(cur) => {
                if let Some(p) = &prev {
                    let events = diff(p, &cur, Utc::now(), o.node.as_deref());
                    if let Some(dir) = &o.record_to
                        && let Err(e) = record(dir, &events)
                    {
                        eprintln!("could not record events: {e:#}");
                    }
                    for e in &events {
                        emit(e);
                        count += 1;
                    }
                }
                prev = Some(cur);
            }
            Err(e) => eprintln!("poll failed (keeping the last state): {e:#}"),
        }
        polls += 1;
        if o.polls.is_some_and(|max| polls >= max) {
            return Ok(count);
        }
        sleep(o.interval).await;
    }
}

fn print_event(ev: &VmEvent, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(ev)?);
    } else {
        println!("{}", render_line(ev));
    }
    Ok(())
}

/// `mac watch`.
pub async fn run_watch(
    r: &crate::remote::Remote,
    o: WatchOpts,
    json: bool,
    kube: bool,
) -> Result<()> {
    let src = DaemonSource {
        r,
        kube_root: if kube {
            Some(crate::kube_apple::model::default_root()?)
        } else {
            None
        },
    };
    if let Some(d) = &o.record_to {
        eprintln!("recording to {}", d.join("events.jsonl").display());
    }
    watch(
        &src,
        &o,
        |e| {
            let _ = print_event(e, json);
        },
        async |d| tokio::time::sleep(d).await,
    )
    .await
    .map(drop)
}

/// One `mac events --filter KEY=VALUE`. All filters must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventFilter {
    /// `type=mac` or `type=kube`: which family of events.
    Type(String),
    /// `event=started`: the whole name (`mac.instance.started`), its last part (`started`), or a leading part (`mac.instance`).
    Event(String),
    /// `name=ci`: part of the `name` field (instances), or of the `node` field (k3s node events).
    Name(String),
    /// `image=`, `project=`, `status=`, `cluster=`: part of that field of the event.
    Field(String, String),
    /// `label=KEY` or `label=KEY=VALUE`: events carry only the `kairon.zyvor.dev/*` labels, as fields of the same name.
    Label(String, Option<String>),
}

/// The keys `--filter` accepts, for messages.
pub const EVENT_FILTER_KEYS: &str = "type, event, name, image, project, status, cluster, label";

/// `type=mac`, `event=started`, `name=ci`, `label=kairon.zyvor.dev/pool=ios`, ...
pub fn parse_event_filter(s: &str) -> Result<EventFilter> {
    let Some((k, v)) = s.split_once('=') else {
        anyhow::bail!(
            "filter {s:?} must be KEY=VALUE ({EVENT_FILTER_KEYS}); see `fluxctl explain MAC_FILTER_INVALID`"
        );
    };
    anyhow::ensure!(
        !v.is_empty(),
        "filter {s:?} has an empty value; see `fluxctl explain MAC_FILTER_INVALID`"
    );
    Ok(match k {
        "type" => {
            anyhow::ensure!(
                matches!(v, "mac" | "kube"),
                "filter {s:?}: type is mac (instances) or kube (k3s nodes and clusters); see `fluxctl explain MAC_FILTER_INVALID`"
            );
            EventFilter::Type(v.into())
        }
        "event" => EventFilter::Event(v.into()),
        "name" => EventFilter::Name(v.into()),
        "image" | "project" | "status" | "cluster" => EventFilter::Field(k.into(), v.into()),
        "label" => match v.split_once('=') {
            Some((lk, lv)) => EventFilter::Label(lk.into(), Some(lv.into())),
            None => EventFilter::Label(v.into(), None),
        },
        _ => anyhow::bail!(
            "unknown filter {k:?}: use one of {EVENT_FILTER_KEYS}; see `fluxctl explain MAC_FILTER_INVALID`"
        ),
    })
}

pub fn parse_event_filters(f: &[String]) -> Result<Vec<EventFilter>> {
    f.iter().map(|s| parse_event_filter(s)).collect()
}

fn field_has(ev: &VmEvent, k: &str, want: &str) -> bool {
    ev.fields.get(k).is_some_and(|x| x.contains(want))
}

/// True when the event passes every filter.
pub fn event_matches(ev: &VmEvent, filters: &[EventFilter]) -> bool {
    filters.iter().all(|f| match f {
        EventFilter::Type(t) => {
            if t == "mac" {
                ev.event.starts_with(MAC_PREFIX)
            } else {
                ev.event.starts_with(KUBE_PREFIX)
            }
        }
        EventFilter::Event(e) => {
            ev.event == *e
                || ev.event.ends_with(&format!(".{e}"))
                || ev.event.starts_with(&format!("{e}."))
        }
        EventFilter::Name(n) => field_has(ev, "name", n) || field_has(ev, "node", n),
        EventFilter::Field(k, v) => field_has(ev, k, v),
        EventFilter::Label(k, v) => match (ev.fields.get(k), v) {
            (Some(x), Some(want)) => x == want,
            (Some(_), None) => true,
            (None, _) => false,
        },
    })
}

/// What `mac events` shows: `limit` newest of the events at or after `since` that pass `filters`, oldest first.
pub fn select_events(
    mut all: Vec<VmEvent>,
    since: Option<DateTime<Utc>>,
    filters: &[EventFilter],
    limit: usize,
) -> Vec<VmEvent> {
    all.retain(|e| since.is_none_or(|s| e.ts >= s) && event_matches(e, filters));
    all.sort_by_key(|e| e.ts);
    let skip = all.len().saturating_sub(limit);
    all.split_off(skip)
}

/// `mac events`: the recorded `mac.*` and `kube.apple.*` events from the daemon, then, with `follow`, new ones.
pub async fn run_events(
    r: &crate::remote::Remote,
    limit: usize,
    follow: bool,
    json: bool,
    since: Option<DateTime<Utc>>,
    filters: &[EventFilter],
) -> Result<()> {
    let mut all: Vec<VmEvent> = vec![];
    // The daemon keeps the newest `limit`; with filters here, ask for more so they have something to choose from.
    let ask = if filters.is_empty() { limit } else { 10_000 };
    for prefix in [MAC_PREFIX, KUBE_PREFIX] {
        let mut path = format!(
            "/v1/events?event={}&limit={ask}",
            crate::remote::encode_query(prefix)
        );
        if let Some(s) = since {
            // The daemon's `since` is exclusive; the filter below is inclusive, so nothing at the boundary is lost.
            path.push_str(&format!(
                "&since={}",
                crate::remote::encode_query(
                    &(s - chrono::Duration::milliseconds(1))
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                )
            ));
        }
        let v = r.call(reqwest::Method::GET, &path, None).await?;
        all.extend(serde_json::from_value::<Vec<VmEvent>>(v["items"].clone())?);
    }
    for ev in select_events(all, since, filters, limit) {
        print_event(&ev, json)?;
    }
    if follow {
        r.follow_events("", |ev| {
            if ours(&ev.event) && event_matches(&ev, filters) {
                print_event(&ev, json)?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    const A: &str = "11111111-1111-1111-1111-111111111111";
    const B: &str = "22222222-2222-2222-2222-222222222222";
    const C: &str = "33333333-3333-3333-3333-333333333333";

    fn inst(id: &str, name: &str, st: &str) -> Value {
        json!({"id": id, "name": name, "status": st, "labels": {
            "fluxvm.mac": "instance", "fluxvm.mac.image": "xcode:15",
            "kairon.zyvor.dev/chip": "m4", "zone": "lab", "fluxvm.mac.env": "TOKEN=hunter2"}})
    }

    fn knode(id: &str, cluster: &str, svc: &str, st: &str) -> Value {
        json!({"id": id, "name": format!("k3s-{svc}"), "status": st,
               "labels": {"fluxvm.stack": format!("apple-k3s-{cluster}"), "fluxvm.service": svc}})
    }

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn names(e: &[VmEvent]) -> Vec<&str> {
        e.iter().map(|e| e.event.as_str()).collect()
    }

    #[test]
    fn snapshot_separates_instances_from_other_vms_and_reads_kube_nodes() {
        let vms = vec![
            inst(A, "build", "running"),
            json!({"id": B, "name": "plain", "status": "running", "labels": {}}),
            knode(C, "dev", "server.cp1", "running"),
        ];
        let s = snapshot(&vms, &[]);
        assert_eq!(s.macs.len(), 1);
        assert_eq!(s.macs[A].image, "xcode:15");
        let n = &s.kube[&("dev".to_owned(), "cp1".to_owned())];
        assert_eq!((n.role.as_str(), n.id.as_str()), ("server", C));
        assert_eq!(s.slots_used(), 1);
    }

    #[test]
    fn a_new_changed_and_removed_instance_each_make_one_event() {
        let before = snapshot(
            &[inst(A, "build", "running"), inst(B, "old", "stopped")],
            &[],
        );
        let after = snapshot(
            &[inst(A, "build", "paused"), inst(C, "new", "creating")],
            &[],
        );
        let ev = diff(&before, &after, at(), Some("studio-1"));
        assert_eq!(
            names(&ev),
            [
                "mac.instance.paused",
                "mac.instance.created",
                "mac.instance.removed"
            ]
        );
        let paused = &ev[0];
        assert_eq!(paused.vm_id, Some(A.parse().unwrap()));
        assert_eq!(paused.fields["from"], "running");
        assert_eq!(paused.fields["image"], "xcode:15");
        assert_eq!(paused.fields["node"], "studio-1");
        assert_eq!(paused.fields["kairon.zyvor.dev/chip"], "m4");
    }

    #[test]
    fn resume_start_stop_and_failure_are_told_apart() {
        for (from, to, want) in [
            ("paused", "running", "mac.instance.resumed"),
            ("stopped", "running", "mac.instance.started"),
            ("creating", "running", "mac.instance.started"),
            ("running", "stopped", "mac.instance.stopped"),
            ("running", "failed", "mac.instance.failed"),
            ("failed", "creating", "mac.instance.state"),
        ] {
            let ev = diff(
                &snapshot(&[inst(A, "x", from)], &[]),
                &snapshot(&[inst(A, "x", to)], &[]),
                at(),
                None,
            );
            assert_eq!(names(&ev)[0], want, "{from} -> {to}");
        }
    }

    #[test]
    fn nothing_changed_makes_no_events() {
        let s = snapshot(&[inst(A, "x", "running")], &[]);
        assert!(diff(&s, &s.clone(), at(), None).is_empty());
    }

    #[test]
    fn filling_and_freeing_the_second_slot_is_announced() {
        let one = snapshot(&[inst(A, "a", "running"), inst(B, "b", "stopped")], &[]);
        let two = snapshot(&[inst(A, "a", "running"), inst(B, "b", "running")], &[]);
        let up = diff(&one, &two, at(), None);
        let full = up.iter().find(|e| e.event == "mac.slots.full").unwrap();
        assert_eq!(
            (full.fields["used"].as_str(), full.fields["max"].as_str()),
            ("2", "2")
        );
        // A paused instance still holds its slot: pausing frees nothing.
        let paused = snapshot(&[inst(A, "a", "running"), inst(B, "b", "paused")], &[]);
        assert!(!names(&diff(&two, &paused, at(), None)).contains(&"mac.slots.free"));
        assert!(names(&diff(&two, &one, at(), None)).contains(&"mac.slots.free"));
    }

    #[test]
    fn kube_node_and_cluster_events_carry_ids_and_labels_only() {
        let before = snapshot(&[knode(A, "dev", "server.cp1", "running")], &[]);
        let after = snapshot(
            &[
                knode(A, "dev", "server.cp1", "stopped"),
                knode(B, "dev", "agent.w1", "running"),
            ],
            &[],
        );
        let ev = diff(&before, &after, at(), None);
        assert_eq!(
            names(&ev),
            ["kube.apple.node.stopped", "kube.apple.node.created"]
        );
        assert_eq!(ev[1].fields["role"], "agent");
        assert_eq!(ev[1].fields["cluster"], "dev");
        assert_eq!(ev[0].vm_id, Some(A.parse().unwrap()));
        let gone = diff(&after, &Snapshot::default(), at(), None);
        assert_eq!(
            names(&gone),
            ["kube.apple.node.removed", "kube.apple.node.removed"]
        );
    }

    #[test]
    fn cluster_phase_changes_and_removal_are_events() {
        let mut a = Snapshot::default();
        a.phases.insert("dev".into(), "bootstrapping".into());
        let mut b = Snapshot::default();
        b.phases.insert("dev".into(), "ready".into());
        let ev = diff(&a, &b, at(), None);
        assert_eq!(names(&ev), ["kube.apple.cluster.phase"]);
        assert_eq!(ev[0].fields["from"], "bootstrapping");
        assert_eq!(
            names(&diff(&Snapshot::default(), &b, at(), None)),
            ["kube.apple.cluster.created"]
        );
        assert_eq!(
            names(&diff(&b, &Snapshot::default(), at(), None)),
            ["kube.apple.cluster.removed"]
        );
    }

    #[test]
    fn no_event_carries_a_secret_looking_label_or_env() {
        let after = snapshot(&[inst(A, "x", "running")], &[]);
        let text = serde_json::to_string(&diff(&Snapshot::default(), &after, at(), None)).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("TOKEN"),
            "{text}"
        );
        // Only Kairon labels are forwarded, not arbitrary user labels.
        assert!(!text.contains("zone") && text.contains("kairon.zyvor.dev/chip"));
    }

    #[test]
    fn events_are_recorded_where_the_daemon_reads_them() {
        let dir = std::env::temp_dir().join(format!("fluxvm-mac-events-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ev = diff(
            &Snapshot::default(),
            &snapshot(&[inst(A, "x", "running")], &[]),
            at(),
            None,
        );
        record(&dir, &ev).unwrap();
        let got = fluxvm_scheduler::events::EventLog::new(&dir)
            .list(&Default::default())
            .unwrap();
        assert_eq!(got, ev);
        assert!(got.iter().all(|e| ours(&e.event)));
        std::fs::remove_dir_all(&dir).ok();
    }

    struct Seq(RefCell<Vec<Result<Snapshot>>>);
    impl Source for Seq {
        async fn poll(&self) -> Result<Snapshot> {
            self.0.borrow_mut().remove(0)
        }
    }

    #[tokio::test]
    async fn watch_baselines_first_and_survives_a_failed_poll() {
        let s1 = snapshot(&[inst(A, "x", "running")], &[]);
        let s3 = snapshot(&[inst(A, "x", "stopped")], &[]);
        let src = Seq(RefCell::new(vec![
            Ok(s1),
            Err(anyhow::anyhow!("daemon restarting")),
            Ok(s3),
        ]));
        let o = WatchOpts {
            interval: Duration::from_millis(1),
            polls: Some(3),
            node: None,
            record_to: None,
        };
        let mut seen = vec![];
        let n = watch(&src, &o, |e| seen.push(e.event.clone()), async |_| {})
            .await
            .unwrap();
        // The baseline made nothing, the failed poll made nothing (not a "removed"), the last made one "stopped".
        assert_eq!(n, 1);
        assert_eq!(seen, ["mac.instance.stopped"]);
    }

    #[test]
    fn a_line_shows_time_name_id_and_fields() {
        let ev = event(at(), "mac.slots.full", None, vec![("used", "2".into())]);
        let l = render_line(&ev);
        assert!(l.contains("mac.slots.full  -  used=2"), "{l}");
    }

    fn ev_at(secs: i64, name: &str, fields: Vec<(&str, &str)>) -> VmEvent {
        VmEvent {
            ts: DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap(),
            event: name.into(),
            vm_id: None,
            fields: fields
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        }
    }

    fn sample() -> Vec<VmEvent> {
        vec![
            ev_at(
                0,
                "mac.instance.created",
                vec![("name", "ci-1"), ("image", "xcode:15")],
            ),
            ev_at(
                10,
                "mac.instance.started",
                vec![
                    ("name", "ci-1"),
                    ("image", "xcode:15"),
                    ("status", "running"),
                    ("kairon.zyvor.dev/chip", "m4"),
                ],
            ),
            ev_at(
                20,
                "mac.instance.started",
                vec![("name", "web"), ("project", "shop")],
            ),
            ev_at(
                30,
                "kube.apple.node.ready",
                vec![("cluster", "dev"), ("node", "server-1")],
            ),
            ev_at(40, "mac.slots.full", vec![("used", "2")]),
        ]
    }

    fn pick(filters: &[&str], since: Option<i64>) -> Vec<String> {
        let f = parse_event_filters(&filters.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap();
        let since = since.map(|s| DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap());
        select_events(sample(), since, &f, 100)
            .iter()
            .map(|e| format!("{}@{}", e.event, e.ts.timestamp() - 1_800_000_000))
            .collect()
    }

    #[test]
    fn event_filters_by_type_event_name_and_label() {
        assert_eq!(pick(&["type=kube"], None), ["kube.apple.node.ready@30"]);
        assert_eq!(pick(&["type=mac", "event=started"], None).len(), 2);
        assert_eq!(pick(&["event=mac.instance"], None).len(), 3);
        assert_eq!(pick(&["event=mac.slots.full"], None), ["mac.slots.full@40"]);
        assert_eq!(pick(&["name=web"], None), ["mac.instance.started@20"]);
        assert_eq!(pick(&["name=server"], None), ["kube.apple.node.ready@30"]);
        assert_eq!(pick(&["project=shop"], None), ["mac.instance.started@20"]);
        assert_eq!(
            pick(&["label=kairon.zyvor.dev/chip"], None),
            ["mac.instance.started@10"]
        );
        assert_eq!(pick(&["label=kairon.zyvor.dev/chip=m1"], None).len(), 0);
        assert_eq!(
            pick(&["name=ci-1", "event=created"], None),
            ["mac.instance.created@0"]
        );
    }

    #[test]
    fn since_is_inclusive_and_limit_keeps_the_newest() {
        assert_eq!(pick(&[], Some(20)).len(), 3);
        assert_eq!(pick(&["type=mac"], Some(21)), ["mac.slots.full@40"]);
        let all = select_events(sample(), None, &[], 2);
        assert_eq!(names(&all), ["kube.apple.node.ready", "mac.slots.full"]);
        let mut shuffled = sample();
        shuffled.reverse();
        assert_eq!(
            select_events(shuffled, None, &[], 100)[0].event,
            "mac.instance.created"
        );
    }

    #[test]
    fn bad_event_filters_say_what_to_do() {
        for bad in ["nokey", "type=docker", "colour=red", "name="] {
            let e = parse_event_filter(bad).unwrap_err().to_string();
            assert!(e.contains("MAC_FILTER_INVALID"), "{bad}: {e}");
        }
        assert!(
            parse_event_filter("colour=red")
                .unwrap_err()
                .to_string()
                .contains("type, event, name")
        );
    }
}
