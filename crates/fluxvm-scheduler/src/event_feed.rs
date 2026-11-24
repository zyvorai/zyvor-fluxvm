// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! One chronological feed of everything that happened, merged from the logs that already exist (nothing new is recorded):
//!
//! | source   | where it comes from                                                                          |
//! |----------|----------------------------------------------------------------------------------------------|
//! | `vm`     | VM lifecycle events in `<state_dir>/events.jsonl` (`vm.start`, `quota.deny`, ...)             |
//! | `mac`    | `mac.*` events (`fluxctl mac watch`) in the same file                                         |
//! | `kube`   | `kube.*` events (Apple k3s clusters and nodes) in the same file                               |
//! | `docker` | container events: the lifecycle events of containers, in the words Docker uses                |
//! | `backup` | backup and schedule runs: backup events, `fluxctl mac backup` archives, `fluxctl mac schedule` last runs |
//! | `k8s`    | real Kubernetes API events of the running clusters (read live by the daemon, never stored here) |
//! | `audit`  | the audit log, `<state_dir>/audit.jsonl` (who changed what, and whether it worked)            |
//!
//! `GET /v1/events?source=...`, `GET /v1/events/stream?source=...` and `fluxctl events` all read through here, so the
//! terminal and Studio show the same lines. [`FeedEvent`] is a superset of [`VmEvent`] (`ts`, `event`, `vm_id`,
//! `fields` keep their meaning), so a reader of the old shape keeps working.

use crate::audit;
use crate::events::{EventFilter, EventLog, VmEvent};
use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use fluxvm_core::{mac_backup, mac_schedule};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Every source, in the order `fluxctl events --help` lists them, with what each one is.
pub const SOURCES: &[(&str, &str)] = &[
    ("vm", "VM starts, stops, crashes and refusals"),
    ("mac", "Mac VM instances (fluxctl mac)"),
    ("kube", "Kubernetes clusters and nodes"),
    ("docker", "container events, as Docker words them"),
    ("backup", "backup and schedule runs"),
    ("k8s", "events from the Kubernetes API of a running cluster"),
    ("audit", "who changed what, and whether it worked"),
];

/// Newest events shown when no limit is given.
pub const DEFAULT_LIMIT: usize = 100;
/// The most one request may ask for.
pub const MAX_LIMIT: usize = 5000;

/// What went wrong with a request, as a sentence that says what to do and names the code `fluxctl explain` knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n  more: fluxctl explain {}", self.message, self.code)
    }
}

impl std::error::Error for FeedError {}

/// One line of the feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeedEvent {
    pub ts: DateTime<Utc>,
    /// One of [`SOURCES`].
    pub source: String,
    /// The machine name: `vm.start`, `mac.run`, `container.die`, `mac.schedule.run`.
    pub event: String,
    /// The same thing in plain English.
    pub summary: String,
    /// `info`, `warn` or `error`.
    pub level: String,
    /// The VM, cluster, schedule or backup it is about, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

/// The query string of `GET /v1/events` that selects the feed (every field optional).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeedQuery {
    /// Comma list of [`SOURCES`], or `all` (the default).
    #[serde(default)]
    pub source: Option<String>,
    /// RFC 3339, or how far back: `30m`, `2h`, `7d`.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    /// Prefix of the event name.
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub vm: Option<Uuid>,
    /// `info`, `warn` or `error`: only that level and worse.
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only the Kubernetes events of this cluster (the `k8s` source).
    #[serde(default)]
    pub cluster: Option<String>,
}

/// A [`FeedQuery`] that has been checked.
#[derive(Debug, Clone)]
pub struct Feed {
    pub sources: BTreeSet<&'static str>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub event: Option<String>,
    pub vm: Option<Uuid>,
    pub min_level: u8,
    pub limit: usize,
    pub cluster: Option<String>,
}

fn level_rank(l: &str) -> u8 {
    match l {
        "error" => 2,
        "warn" => 1,
        _ => 0,
    }
}

/// `30m`, `2h`, `7d`, `1w` (how far back) or an RFC 3339 time, as a point in time.
pub fn parse_when(text: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, FeedError> {
    let t = text.trim();
    let bad = || FeedError {
        code: "EVENTS_BAD_TIME",
        message: format!(
            "`{t}` is not a time I can read. Use how far back to look (30m, 2h, 7d) or a full timestamp such as 2026-11-24T09:00:00Z."
        ),
    };
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Ok(d.with_timezone(&Utc));
    }
    let (num, unit) = t.split_at(t.len().saturating_sub(1));
    let n: i64 = num.parse().map_err(|_| bad())?;
    if n < 0 {
        return Err(bad());
    }
    let d = match unit {
        "s" => Duration::try_seconds(n),
        "m" => Duration::try_minutes(n),
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        "w" => Duration::try_weeks(n),
        _ => return Err(bad()),
    };
    d.and_then(|d| now.checked_sub_signed(d)).ok_or_else(bad)
}

impl Feed {
    pub fn parse(q: &FeedQuery, now: DateTime<Utc>) -> Result<Feed, FeedError> {
        let mut sources = BTreeSet::new();
        let asked = q.source.as_deref().map(str::trim).unwrap_or("");
        if asked.is_empty() || asked.split(',').any(|s| s.trim() == "all") {
            sources.extend(SOURCES.iter().map(|(s, _)| *s));
        } else {
            for s in asked.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match SOURCES.iter().find(|(n, _)| *n == s.to_ascii_lowercase()) {
                    Some((n, _)) => {
                        sources.insert(*n);
                    }
                    None => {
                        let names: Vec<&str> = SOURCES.iter().map(|(n, _)| *n).collect();
                        return Err(FeedError {
                            code: "EVENTS_BAD_SOURCE",
                            message: format!(
                                "`{s}` is not an event source. Pick from: {}, or leave --source out to see them all.",
                                names.join(", ")
                            ),
                        });
                    }
                }
            }
        }
        let time = |v: &Option<String>| -> Result<Option<DateTime<Utc>>, FeedError> {
            v.as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(|s| parse_when(s, now))
                .transpose()
        };
        let min_level = match q.level.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            None => 0,
            Some(l @ ("info" | "warn" | "error")) => level_rank(l),
            Some(l) => {
                return Err(FeedError {
                    code: "EVENTS_BAD_LEVEL",
                    message: format!("`{l}` is not a level. Use info, warn or error."),
                });
            }
        };
        Ok(Feed {
            sources,
            since: time(&q.since)?,
            until: time(&q.until)?,
            event: q.event.clone().filter(|e| !e.is_empty()),
            vm: q.vm,
            min_level,
            limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            cluster: q.cluster.clone().filter(|c| !c.trim().is_empty()),
        })
    }

    /// Does this line pass every filter?
    pub fn keep(&self, e: &FeedEvent) -> bool {
        self.sources.contains(e.source.as_str())
            && self.since.is_none_or(|s| e.ts > s)
            && self.until.is_none_or(|u| e.ts <= u)
            && self.vm.is_none_or(|v| e.vm_id == Some(v))
            && self.event.as_deref().is_none_or(|p| e.event.starts_with(p))
            && level_rank(&e.level) >= self.min_level
            && (e.source != "k8s"
                || self
                    .cluster
                    .as_deref()
                    .is_none_or(|c| e.target.as_deref() == Some(c)))
    }
}

/// Where the feed reads from. `container` names the containers of a daemon (id to name and image); a plain CLI has none.
pub struct Inputs<'a> {
    pub state_dir: &'a Path,
    pub container: &'a (dyn Fn(Uuid) -> Option<(String, String)> + Sync),
    pub schedules: Option<PathBuf>,
    pub backups: Option<PathBuf>,
}

impl<'a> Inputs<'a> {
    /// Reads the state directory and the Mac schedule and backup folders at their default places.
    pub fn local(state_dir: &'a Path) -> Self {
        fn none(_: Uuid) -> Option<(String, String)> {
            None
        }
        Inputs {
            state_dir,
            container: &none,
            schedules: mac_schedule::default_path(),
            backups: mac_backup::default_root(),
        }
    }
}

fn unix(secs: u64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs as i64, 0)
        .single()
        .unwrap_or_default()
}

/// Which source an event of the daemon's log belongs to.
pub fn classify(event: &str) -> &'static str {
    if event.starts_with("mac.backup")
        || event.starts_with("mac.schedule")
        || event.starts_with("backup.")
        || event.starts_with("vm.backup")
    {
        "backup"
    } else if event.starts_with("mac.") {
        "mac"
    } else if event.starts_with("kube.") {
        "kube"
    } else {
        "vm"
    }
}

fn level_of(event: &str, fields: &BTreeMap<String, String>) -> &'static str {
    match fields.get("result").map(String::as_str) {
        Some("failed") => return "error",
        Some("denied") => return "warn",
        _ => {}
    }
    let e = event.to_ascii_lowercase();
    if ["fail", "crash", "oom", "error", "damaged"]
        .iter()
        .any(|w| e.contains(w))
    {
        "error"
    } else if ["deny", "denied", "exited", "stalled"]
        .iter()
        .any(|w| e.contains(w))
    {
        "warn"
    } else {
        "info"
    }
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_string()
}

/// A few `key=value` pairs for a line that has no wording of its own. Values are scrubbed like the audit log's.
fn details(fields: &BTreeMap<String, String>, skip: &[&str]) -> String {
    let parts: Vec<String> = fields
        .iter()
        .filter(|(k, v)| {
            !skip.contains(&k.as_str())
                && !audit::is_secret_name(k)
                && !v.is_empty()
                && v.len() <= 80
        })
        .take(3)
        .map(|(k, v)| format!("{k} {}", audit::redact(v)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn words(event: &str) -> String {
    event.replace(['.', '-', '_'], " ")
}

/// The container event that a lifecycle event stands for, in Docker's words.
fn docker_word(event: &str) -> Option<(&'static str, &'static str)> {
    Some(match event {
        "vm.create" => ("container.create", "was created"),
        "vm.start" => ("container.start", "started"),
        "vm.stop" => ("container.die", "stopped"),
        "vm.pause" => ("container.pause", "was paused"),
        "vm.resume" => ("container.unpause", "was resumed"),
        "vm.restart" => ("container.restart", "restarted"),
        "vm.delete" => ("container.destroy", "was removed"),
        _ => return None,
    })
}

/// One line of the daemon's event log as a feed line.
pub fn from_vm_event(
    ev: &VmEvent,
    container: &dyn Fn(Uuid) -> Option<(String, String)>,
) -> FeedEvent {
    let cname = ev.vm_id.and_then(container);
    let name = ev
        .fields
        .get("name")
        .cloned()
        .or_else(|| cname.as_ref().map(|c| c.0.clone()))
        .or_else(|| ev.vm_id.map(short));
    let fields: BTreeMap<String, String> = ev
        .fields
        .iter()
        .filter(|(k, _)| !audit::is_secret_name(k))
        .map(|(k, v)| (k.clone(), audit::redact(v)))
        .collect();
    if let (Some((cname, image)), Some((event, what))) = (&cname, docker_word(&ev.event)) {
        return FeedEvent {
            ts: ev.ts,
            source: "docker".into(),
            event: event.into(),
            summary: format!("Container {cname} ({image}) {what}"),
            level: "info".into(),
            target: Some(cname.clone()),
            vm_id: ev.vm_id,
            fields,
        };
    }
    let source = classify(&ev.event);
    let who = name.clone().unwrap_or_default();
    let summary = match (source, ev.event.as_str()) {
        ("vm", "vm.start") => format!("VM {who} started"),
        ("vm", "vm.stop") => format!("VM {who} stopped"),
        ("vm", "vm.create") => format!("VM {who} was created"),
        ("vm", "vm.delete") => format!("VM {who} was removed"),
        ("vm", "vm.pause") => format!("VM {who} was paused"),
        ("vm", "vm.resume") => format!("VM {who} was resumed"),
        ("vm", "vm.crash-loop") => {
            format!("VM {who} keeps crashing and was stopped from restarting")
        }
        ("vm", "quota.deny") => format!(
            "A request was refused by a quota{}",
            fields
                .get("reason")
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        ),
        _ => {
            let skip = ["name"];
            let base = words(&ev.event);
            let base = base
                .split_once(' ')
                .map_or(base.clone(), |(_, r)| r.to_string());
            let area = match source {
                "mac" => "Mac",
                "kube" => "Kubernetes",
                "backup" => "Backup",
                _ => "VM",
            };
            let subject = if who.is_empty() {
                String::new()
            } else {
                format!(" {who}")
            };
            format!("{area}{subject}: {base}{}", details(&fields, &skip))
        }
    };
    FeedEvent {
        ts: ev.ts,
        source: source.into(),
        event: ev.event.clone(),
        summary,
        level: level_of(&ev.event, &fields).into(),
        target: name,
        vm_id: ev.vm_id,
        fields,
    }
}

/// One audit record as a feed line.
pub fn from_audit(r: &audit::Record) -> FeedEvent {
    let result = match r.result.as_str() {
        "ok" => "worked",
        "denied" => "was refused",
        "failed" => "failed",
        other => other,
    };
    let who = if r.actor.is_empty() {
        "Someone"
    } else {
        r.actor.as_str()
    };
    let on = if r.target.is_empty() {
        String::new()
    } else {
        format!(" on {}", r.target)
    };
    let mut fields = r.detail.clone();
    fields.insert("actor".into(), r.actor.clone());
    fields.insert("result".into(), r.result.clone());
    fields.insert("via".into(), r.source.clone());
    FeedEvent {
        ts: r.ts,
        source: "audit".into(),
        event: format!("audit.{}", r.action),
        summary: format!("{who} ran {}{on}: {result}", r.action),
        level: match r.result.as_str() {
            "failed" => "error",
            "denied" => "warn",
            _ => "info",
        }
        .into(),
        target: (!r.target.is_empty()).then(|| r.target.clone()),
        vm_id: None,
        fields,
    }
}

/// Backup archives and the last run of each Mac schedule: files that exist already, read as events.
pub fn runs(inp: &Inputs<'_>, now: DateTime<Utc>) -> Vec<FeedEvent> {
    let mut out = Vec::new();
    if let Some(root) = inp.backups.as_deref().filter(|r| r.is_dir()) {
        for e in mac_backup::list(root, now.timestamp().max(0) as u64) {
            let mut fields = BTreeMap::new();
            fields.insert("id".to_string(), e.id.clone());
            match (&e.manifest, e.state) {
                (Some(m), mac_backup::State::Ok) => {
                    fields.insert("bytes".into(), m.archive_bytes.to_string());
                    out.push(FeedEvent {
                        ts: unix(m.created_unix),
                        source: "backup".into(),
                        event: if m.automatic {
                            "mac.backup.safety-net"
                        } else {
                            "mac.backup.created"
                        }
                        .into(),
                        summary: format!(
                            "Backed up {} {} ({}){}",
                            m.kind.word(),
                            m.name,
                            mac_backup::human_bytes(m.archive_bytes),
                            if m.automatic {
                                ", a safety copy taken before a restore"
                            } else {
                                ""
                            }
                        ),
                        level: "info".into(),
                        target: Some(m.name.clone()),
                        vm_id: None,
                        fields,
                    });
                }
                (_, mac_backup::State::Failed | mac_backup::State::Damaged) => {
                    let when = ["error", "part", "json"]
                        .iter()
                        .filter_map(|x| std::fs::metadata(root.join(format!("{}.{x}", e.id))).ok())
                        .filter_map(|m| m.modified().ok())
                        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| unix(d.as_secs()))
                        .max()
                        .unwrap_or(now);
                    if let Some(msg) = &e.error {
                        fields.insert("error".into(), audit::redact(msg.trim()));
                    }
                    out.push(FeedEvent {
                        ts: when,
                        source: "backup".into(),
                        event: "mac.backup.failed".into(),
                        summary: format!(
                            "Backup {} did not finish. Run `fluxctl mac backup ls` to see it and `fluxctl mac backup rm {}` to clear it.",
                            e.id, e.id
                        ),
                        level: "error".into(),
                        target: Some(e.id.clone()),
                        vm_id: None,
                        fields,
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(path) = inp.schedules.as_deref().filter(|p| p.is_file()) {
        for s in mac_schedule::load(path)
            .map(|f| f.schedules)
            .unwrap_or_default()
        {
            let Some(last) = s.last_run_unix else {
                continue;
            };
            let status = s.last_status.clone().unwrap_or_else(|| "ran".into());
            let mut fields = BTreeMap::new();
            fields.insert("status".to_string(), status.clone());
            if let Some(d) = s.last_detail.as_deref().filter(|d| !d.is_empty()) {
                fields.insert("detail".into(), audit::redact(d));
            }
            out.push(FeedEvent {
                ts: unix(last),
                source: "backup".into(),
                event: "mac.schedule.run".into(),
                summary: format!(
                    "Schedule {} {}",
                    s.name,
                    if status == "ok" {
                        "ran and finished".to_string()
                    } else {
                        status.clone()
                    }
                ),
                level: if status == "failed" { "error" } else { "info" }.into(),
                target: Some(s.name.clone()),
                vm_id: None,
                fields,
            });
        }
    }
    out
}

pub fn merge(mut all: Vec<FeedEvent>, feed: &Feed) -> Vec<FeedEvent> {
    all.retain(|e| feed.keep(e));
    all.sort_by(|a, b| a.ts.cmp(&b.ts));
    if all.len() > feed.limit {
        all.drain(..all.len() - feed.limit);
    }
    all
}

/// One Kubernetes API event, as the Kubernetes page shows it (`kube_workloads::simplify_event`), as a line of the feed.
/// `None` when it carries no usable time.
pub fn from_k8s_event(cluster: &str, e: &serde_json::Value) -> Option<FeedEvent> {
    let text = |k: &str| e[k].as_str().filter(|s| !s.is_empty());
    let ts = DateTime::parse_from_rfc3339(text("last").or_else(|| text("first"))?)
        .ok()?
        .with_timezone(&Utc);
    let reason = text("reason").unwrap_or("Event");
    let object = text("object").unwrap_or("");
    let count = e["count"].as_u64().unwrap_or(1);
    let warning = text("type") == Some("Warning");
    let mut summary = format!("{cluster}: {reason}");
    if !object.is_empty() {
        summary.push_str(&format!(" on {object}"));
    }
    if let Some(m) = text("message") {
        let m: String = m.chars().take(240).collect();
        summary.push_str(&format!(": {m}"));
    }
    if count > 1 {
        summary.push_str(&format!(" (x{count})"));
    }
    let mut fields = BTreeMap::new();
    for (k, v) in [
        ("namespace", text("namespace")),
        ("object", Some(object).filter(|o| !o.is_empty())),
        ("reason", Some(reason)),
        ("component", text("component")),
    ] {
        if let Some(v) = v {
            fields.insert(k.to_string(), v.to_string());
        }
    }
    fields.insert("count".into(), count.to_string());
    Some(FeedEvent {
        ts,
        source: "k8s".into(),
        event: format!("k8s.{reason}"),
        summary,
        level: if warning { "warn" } else { "info" }.into(),
        target: Some(cluster.to_string()),
        vm_id: None,
        fields,
    })
}

/// The newest `feed.limit` matching events from every wanted source, oldest first.
pub fn collect(inp: &Inputs<'_>, feed: &Feed, now: DateTime<Utc>) -> Result<Vec<FeedEvent>> {
    collect_with(inp, feed, now, Vec::new())
}

/// [`collect`] plus lines the caller gathered itself (the daemon adds the live `k8s` events), merged and limited together.
pub fn collect_with(
    inp: &Inputs<'_>,
    feed: &Feed,
    now: DateTime<Utc>,
    extra: Vec<FeedEvent>,
) -> Result<Vec<FeedEvent>> {
    let mut all = extra;
    let wants = |s: &str| feed.sources.contains(s);
    if ["vm", "mac", "kube", "docker", "backup"]
        .iter()
        .any(|s| wants(s))
    {
        let log = EventLog::new(inp.state_dir);
        let filter = EventFilter {
            since: feed.since,
            ..Default::default()
        };
        all.extend(
            log.list(&filter)?
                .iter()
                .map(|e| from_vm_event(e, inp.container)),
        );
    }
    if wants("audit") {
        all.extend(
            audit::AuditLog::new(inp.state_dir)
                .all()?
                .iter()
                .map(from_audit),
        );
    }
    if wants("backup") {
        all.extend(runs(inp, now));
    }
    Ok(merge(all, feed))
}

/// Where a follower has got to in each file.
#[derive(Debug, Clone)]
pub struct Cursor {
    events: u64,
    audit: u64,
    seen_runs: BTreeSet<(String, String, i64)>,
    polls: u32,
}

impl Cursor {
    /// Starts at the end of every file: only what happens from now on. The runs already on disk count as seen.
    pub fn at_end(inp: &Inputs<'_>, now: DateTime<Utc>) -> Cursor {
        Cursor {
            events: EventLog::new(inp.state_dir).end_offset(),
            audit: EventLog::at(inp.state_dir.join(audit::FILE)).end_offset(),
            seen_runs: runs(inp, now).iter().map(run_key).collect(),
            polls: 0,
        }
    }
}

fn run_key(e: &FeedEvent) -> (String, String, i64) {
    (
        e.event.clone(),
        e.target.clone().unwrap_or_default(),
        e.ts.timestamp(),
    )
}

/// What was added since the last call, oldest first. Backup and schedule runs are looked at every tenth call, as they
/// are files that change rarely.
pub fn poll(
    inp: &Inputs<'_>,
    feed: &Feed,
    cur: &mut Cursor,
    now: DateTime<Utc>,
) -> Result<Vec<FeedEvent>> {
    let mut fresh = Vec::new();
    let (evs, next) =
        EventLog::new(inp.state_dir).read_from(cur.events, &EventFilter::default())?;
    cur.events = next;
    fresh.extend(evs.iter().map(|e| from_vm_event(e, inp.container)));
    let (recs, next) = EventLog::at(inp.state_dir.join(audit::FILE))
        .read_from(cur.audit, &EventFilter::default())?;
    cur.audit = next;
    fresh.extend(
        recs.iter()
            .filter_map(audit::Record::from_event)
            .map(|r| from_audit(&r)),
    );
    cur.polls += 1;
    if cur.polls % 10 == 0 {
        for r in runs(inp, now) {
            if cur.seen_runs.insert(run_key(&r)) {
                fresh.push(r);
            }
        }
    }
    fresh.retain(|e| feed.keep(e));
    fresh.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(fresh)
}

/// The sources as `[{name, about}]`, for Studio and `--help`.
pub fn source_list() -> Vec<serde_json::Value> {
    SOURCES
        .iter()
        .map(|(n, a)| serde_json::json!({"name": n, "about": a}))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap()
    }

    fn none(_: Uuid) -> Option<(String, String)> {
        None
    }

    fn inputs(dir: &Path) -> Inputs<'_> {
        Inputs {
            state_dir: dir,
            container: &none,
            schedules: None,
            backups: None,
        }
    }

    fn put(dir: &Path, file: &str, ts: &str, event: &str, fields: &[(&str, &str)]) {
        let f: BTreeMap<_, _> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let line = serde_json::json!({"ts": ts, "event": event, "fields": f});
        use std::io::Write;
        let mut h = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))
            .unwrap();
        writeln!(h, "{line}").unwrap();
    }

    #[test]
    fn time_words_and_stamps() {
        assert_eq!(parse_when("2h", now()).unwrap(), now() - Duration::hours(2));
        assert_eq!(parse_when("7d", now()).unwrap(), now() - Duration::days(7));
        assert_eq!(
            parse_when("2026-11-24T09:00:00Z", now()).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 24, 9, 0, 0).unwrap()
        );
        for bad in ["", "soon", "5x", "-3h", "h"] {
            assert_eq!(
                parse_when(bad, now()).unwrap_err().code,
                "EVENTS_BAD_TIME",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_bad_source_says_what_to_pick_and_what_explains_it() {
        let q = FeedQuery {
            source: Some("vm,nope".into()),
            ..Default::default()
        };
        let e = Feed::parse(&q, now()).unwrap_err();
        assert_eq!(e.code, "EVENTS_BAD_SOURCE");
        assert!(
            e.to_string()
                .contains("Pick from: vm, mac, kube, docker, backup, k8s, audit")
        );
        assert!(e.to_string().ends_with("fluxctl explain EVENTS_BAD_SOURCE"));
        let q = FeedQuery {
            level: Some("loud".into()),
            ..Default::default()
        };
        assert_eq!(Feed::parse(&q, now()).unwrap_err().code, "EVENTS_BAD_LEVEL");
    }

    #[test]
    fn all_sources_by_default_and_a_list_narrows() {
        assert_eq!(
            Feed::parse(&FeedQuery::default(), now())
                .unwrap()
                .sources
                .len(),
            7
        );
        let q = FeedQuery {
            source: Some("Mac, audit".into()),
            ..Default::default()
        };
        let f = Feed::parse(&q, now()).unwrap();
        assert_eq!(f.sources.into_iter().collect::<Vec<_>>(), ["audit", "mac"]);
    }

    #[test]
    fn events_are_sorted_into_sources() {
        for (name, src) in [
            ("vm.start", "vm"),
            ("quota.deny", "vm"),
            ("mac.run", "mac"),
            ("mac.backup.created", "backup"),
            ("mac.schedule.run", "backup"),
            ("kube.apple.node.ready", "kube"),
            ("backup.verify", "backup"),
            ("vm.backup.delete", "backup"),
        ] {
            assert_eq!(classify(name), src, "{name}");
        }
    }

    #[test]
    fn containers_read_like_docker_and_vms_like_vms() {
        let id = Uuid::new_v4();
        let ev = VmEvent {
            ts: now(),
            event: "vm.stop".into(),
            vm_id: Some(id),
            fields: BTreeMap::new(),
        };
        let c = |i: Uuid| (i == id).then(|| ("web".to_string(), "nginx:1".to_string()));
        let f = from_vm_event(&ev, &c);
        assert_eq!(
            (f.source.as_str(), f.event.as_str()),
            ("docker", "container.die")
        );
        assert_eq!(f.summary, "Container web (nginx:1) stopped");
        let f = from_vm_event(&ev, &none);
        assert_eq!(f.source, "vm");
        assert_eq!(f.summary, format!("VM {} stopped", short(id)));
    }

    #[test]
    fn failures_are_errors_and_refusals_warnings_and_secrets_stay_out() {
        let mut fields = BTreeMap::new();
        fields.insert("result".to_string(), "failed".to_string());
        fields.insert("api_token".to_string(), "hunter2".to_string());
        fields.insert("note".to_string(), "see https://u:pw@host/x".to_string());
        let ev = VmEvent {
            ts: now(),
            event: "backup.verify".into(),
            vm_id: None,
            fields,
        };
        let f = from_vm_event(&ev, &none);
        assert_eq!(f.level, "error");
        assert!(!f.fields.contains_key("api_token"));
        assert!(!f.fields["note"].contains("pw@"));
        assert!(!f.summary.contains("hunter2") && !f.summary.contains("pw@"));
        assert_eq!(level_of("quota.deny", &BTreeMap::new()), "warn");
        assert_eq!(level_of("vm.start", &BTreeMap::new()), "info");
    }

    #[test]
    fn the_feed_merges_logs_by_time_and_filters() {
        let d = tempfile::tempdir().unwrap();
        put(
            d.path(),
            "events.jsonl",
            "2026-11-24T10:00:00Z",
            "vm.start",
            &[],
        );
        put(
            d.path(),
            "events.jsonl",
            "2026-11-24T10:30:00Z",
            "mac.run",
            &[("name", "ci")],
        );
        put(
            d.path(),
            "audit.jsonl",
            "2026-11-24T10:15:00Z",
            "audit.mac.run",
            &[
                ("actor", "sam"),
                ("target", "ci"),
                ("result", "failed"),
                ("source", "cli"),
            ],
        );
        let all = Feed::parse(&FeedQuery::default(), now()).unwrap();
        let got = collect(&inputs(d.path()), &all, now()).unwrap();
        assert_eq!(
            got.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
            ["vm", "audit", "mac"]
        );
        assert_eq!(got[1].summary, "sam ran mac.run on ci: failed");
        assert_eq!(got[1].level, "error");

        let q = |f: fn(&mut FeedQuery)| {
            let mut q = FeedQuery::default();
            f(&mut q);
            collect(&inputs(d.path()), &Feed::parse(&q, now()).unwrap(), now()).unwrap()
        };
        assert_eq!(q(|q| q.source = Some("audit".into())).len(), 1);
        assert_eq!(q(|q| q.level = Some("error".into())).len(), 1);
        assert_eq!(q(|q| q.event = Some("mac.".into())).len(), 1);
        assert_eq!(
            q(|q| q.since = Some("2026-11-24T10:20:00Z".into())).len(),
            1
        );
        assert_eq!(q(|q| q.limit = Some(2)).len(), 2);
        assert_eq!(
            q(|q| q.limit = Some(2))[1].event,
            "mac.run",
            "the newest are kept"
        );
        assert!(
            collect(&inputs(&d.path().join("nothing")), &all, now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn backup_and_schedule_runs_come_from_the_files_they_already_have() {
        let d = tempfile::tempdir().unwrap();
        let sched = d.path().join("mac-schedules.json");
        std::fs::write(
            &sched,
            serde_json::json!({"schedules": [{
                "name": "nightly", "cron": "0 2 * * *", "image": "macos:1",
                "last_run_unix": 1_795_000_000u64, "last_status": "failed", "last_detail": "no slot free"
            }, {"name": "never", "cron": "0 2 * * *", "image": "macos:1"}]})
            .to_string(),
        )
        .unwrap();
        let mut inp = inputs(d.path());
        inp.schedules = Some(sched);
        let r = runs(&inp, now());
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].event.as_str(), r[0].level.as_str()),
            ("mac.schedule.run", "error")
        );
        assert_eq!(r[0].summary, "Schedule nightly failed");
        assert_eq!(r[0].fields["detail"], "no slot free");
    }

    #[test]
    fn following_sees_only_what_is_new() {
        let d = tempfile::tempdir().unwrap();
        put(
            d.path(),
            "events.jsonl",
            "2026-11-24T10:00:00Z",
            "vm.start",
            &[],
        );
        let inp = inputs(d.path());
        let feed = Feed::parse(&FeedQuery::default(), now()).unwrap();
        let mut cur = Cursor::at_end(&inp, now());
        assert!(poll(&inp, &feed, &mut cur, now()).unwrap().is_empty());
        put(
            d.path(),
            "audit.jsonl",
            "2026-11-24T11:00:00Z",
            "audit.vm.delete",
            &[("target", "x"), ("result", "ok")],
        );
        put(
            d.path(),
            "events.jsonl",
            "2026-11-24T11:00:01Z",
            "vm.stop",
            &[],
        );
        let got = poll(&inp, &feed, &mut cur, now()).unwrap();
        assert_eq!(
            got.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
            ["audit", "vm"]
        );
        assert!(poll(&inp, &feed, &mut cur, now()).unwrap().is_empty());
        let only_mac = Feed::parse(
            &FeedQuery {
                source: Some("mac".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        put(
            d.path(),
            "events.jsonl",
            "2026-11-24T11:00:02Z",
            "vm.start",
            &[],
        );
        assert!(poll(&inp, &only_mac, &mut cur, now()).unwrap().is_empty());
    }

    fn k8s(o: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({"namespace": "default", "type": "Warning", "reason": "BackOff",
            "message": "Back-off restarting failed container", "object": "Pod/web-1", "count": 3,
            "first": "2026-11-24T11:00:00Z", "last": "2026-11-24T11:30:00Z", "component": "kubelet"});
        for (k, v) in o.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    #[test]
    fn a_kubernetes_event_becomes_a_line_with_the_cluster_and_the_object() {
        let e = from_k8s_event("lab", &k8s(serde_json::json!({}))).unwrap();
        assert_eq!(
            (e.source.as_str(), e.event.as_str(), e.level.as_str()),
            ("k8s", "k8s.BackOff", "warn")
        );
        assert_eq!(e.target.as_deref(), Some("lab"));
        assert_eq!(e.ts, Utc.with_ymd_and_hms(2026, 11, 24, 11, 30, 0).unwrap());
        assert!(
            e.summary.starts_with("lab: BackOff on Pod/web-1: Back-off")
                && e.summary.ends_with("(x3)")
        );
        assert_eq!(e.fields["namespace"], "default");
        let n = from_k8s_event(
            "lab",
            &k8s(serde_json::json!({"type": "Normal", "count": 1, "last": null})),
        )
        .unwrap();
        assert_eq!(n.level, "info");
        assert_eq!(
            n.ts,
            Utc.with_ymd_and_hms(2026, 11, 24, 11, 0, 0).unwrap(),
            "falls back to the first time"
        );
        assert!(!n.summary.contains("(x"));
        assert!(
            from_k8s_event("lab", &serde_json::json!({"reason": "X"})).is_none(),
            "no time, no line"
        );
    }

    #[test]
    fn k8s_lines_are_merged_by_time_filtered_by_cluster_and_never_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        put(
            dir.path(),
            "events.jsonl",
            "2026-11-24T11:15:00Z",
            "vm.start",
            &[],
        );
        let lab = from_k8s_event("lab", &k8s(serde_json::json!({}))).unwrap();
        let dev = from_k8s_event(
            "dev",
            &k8s(serde_json::json!({"last": "2026-11-24T11:45:00Z"})),
        )
        .unwrap();
        let q = |source: &str, cluster: Option<&str>| {
            Feed::parse(
                &FeedQuery {
                    source: Some(source.into()),
                    cluster: cluster.map(Into::into),
                    ..Default::default()
                },
                now(),
            )
            .unwrap()
        };
        let all = collect_with(
            &inputs(dir.path()),
            &q("all", None),
            now(),
            vec![lab.clone(), dev.clone()],
        )
        .unwrap();
        assert_eq!(
            all.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
            ["vm", "k8s", "k8s"]
        );
        let only = collect_with(
            &inputs(dir.path()),
            &q("k8s", Some("dev")),
            now(),
            vec![lab.clone(), dev],
        )
        .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].target.as_deref(), Some("dev"));
        // without the extra lines (a plain collect, a local read) the k8s source is simply empty
        assert!(
            collect(&inputs(dir.path()), &q("k8s", None), now())
                .unwrap()
                .is_empty()
        );
        // a feed that does not ask for k8s drops them
        assert!(
            collect_with(&inputs(dir.path()), &q("vm", None), now(), vec![lab])
                .unwrap()
                .iter()
                .all(|e| e.source == "vm")
        );
        assert!(source_list().iter().any(|s| s["name"] == "k8s"));
    }
}
