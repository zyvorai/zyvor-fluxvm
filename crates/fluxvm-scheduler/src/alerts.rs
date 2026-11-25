// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What needs attention now, in one short list. Nothing new is recorded: an alert is built from data that already exists.
//!
//! * warn and error lines of the merged event feed ([`crate::event_feed`]) from the last day (or `--since`),
//! * problems that are true right now even when the line is older: a backup that did not finish, a schedule whose last
//!   run failed ([`event_feed::runs`]),
//! * Mac instances whose healthcheck says `unhealthy` and the findings of the cluster health model, which the daemon
//!   hands in as [`Alert`]s of their own.
//!
//! Alerts are deduplicated by a key (`event:target`), so a crash loop is one alert with a count, ordered errors first and
//! newest first. Each carries a plain-English next step and the `fluxctl explain` code where one exists.
//!
//! "Snooze 24h" hides one alert key for a while. It is stored in `<state_dir>/alert-snoozes.json` so that the CLI,
//! Studio and every browser agree, and it simply expires: a problem that is still there comes back. Writers take a file
//! lock, and a list that cannot be read is reported (see [`notes`]) and never silently overwritten.

use crate::event_feed::{self, Feed, FeedEvent, FeedQuery, Inputs};
use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// How far back warnings are looked at when nothing is asked.
pub const DEFAULT_SINCE: &str = "24h";
/// Snooze length when none is given, and the longest one allowed.
pub const DEFAULT_SNOOZE_HOURS: u32 = 24;
pub const MAX_SNOOZE_HOURS: u32 = 24 * 30;

/// One thing that needs attention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alert {
    /// Stable id used to snooze: `event:target` for events, `health:machine/code` and `mac:unhealthy:NAME` otherwise.
    pub key: String,
    /// `error` or `warn`.
    pub level: String,
    /// The feed source (`vm`, `mac`, `backup`, ...) or `health`.
    pub source: String,
    /// What happened, in plain English.
    pub summary: String,
    /// What to do next, in plain English, with the command when there is one.
    pub next_step: String,
    /// A code `fluxctl explain` knows, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// When it last happened (for a problem that is true now: when it was last seen).
    pub last_seen: DateTime<Utc>,
    pub first_seen: DateTime<Utc>,
    /// How many lines were folded into this alert.
    pub count: u32,
    /// Set when the alert is snoozed: until when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<DateTime<Utc>>,
}

pub fn rank(level: &str) -> u8 {
    match level {
        "error" => 2,
        "warn" => 1,
        _ => 0,
    }
}

/// The reply of `GET /v1/alerts` and `fluxctl alerts --json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub since: String,
    pub items: Vec<Alert>,
    /// Alerts that count (not snoozed).
    pub errors: usize,
    pub warnings: usize,
    pub snoozed: usize,
    /// Things that could not be read, each saying what to do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl Report {
    /// Newest-first list; snoozed alerts are dropped unless `include_snoozed`.
    pub fn new(since: &str, all: Vec<Alert>, include_snoozed: bool, notes: Vec<String>) -> Report {
        let snoozed = all.iter().filter(|a| a.snoozed_until.is_some()).count();
        let live = |a: &&Alert| a.snoozed_until.is_none();
        let errors = all
            .iter()
            .filter(live)
            .filter(|a| a.level == "error")
            .count();
        let warnings = all
            .iter()
            .filter(live)
            .filter(|a| a.level == "warn")
            .count();
        let items = all
            .into_iter()
            .filter(|a| include_snoozed || a.snoozed_until.is_none())
            .collect();
        Report {
            since: since.to_string(),
            items,
            errors,
            warnings,
            snoozed,
            notes,
        }
    }
}

// ---- building -----------------------------------------------------------------------------------------------------------

fn key_of(e: &FeedEvent) -> String {
    format!("{}:{}", e.event, e.target.as_deref().unwrap_or("-"))
}

/// The explain code for a failed backup, picked from what the error says.
fn backup_code(fields: &BTreeMap<String, String>) -> &'static str {
    let err = fields
        .get("error")
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if err.contains("space") || err.contains("no room") {
        "MAC_BACKUP_NO_ROOM"
    } else if err.contains("checksum") {
        "MAC_BACKUP_CHECKSUM"
    } else {
        "MAC_BACKUP_IO"
    }
}

/// The explain code of an event, when the codes `fluxctl explain` knows cover it.
pub fn explain_of(e: &FeedEvent) -> Option<&'static str> {
    match e.event.as_str() {
        "mac.backup.failed" => Some(backup_code(&e.fields)),
        _ => None,
    }
}

/// What to do about an event, as a sentence with the command that helps.
pub fn next_step(e: &FeedEvent) -> String {
    let target = e.target.as_deref().unwrap_or("");
    let ev = e.event.as_str();
    match ev {
        "mac.backup.failed" => format!(
            "Run `fluxctl mac backup ls` to see why, `fluxctl mac backup rm {target}` to clear it, then back up again with `fluxctl mac backup create NAME`."
        ),
        "mac.schedule.run" => format!(
            "Run `fluxctl mac schedule ls` to see why `{target}` failed. Fix the command or image, or remove it with `fluxctl mac schedule rm {target}`."
        ),
        "quota.deny" => "A limit refused a request. See your limits with `fluxctl quota`, free some capacity or ask for a higher limit.".into(),
        _ if ev.starts_with("mac.") => format!(
            "Run `fluxctl mac ps -a` to see the instance, then `fluxctl events --since 24h --event {ev}` for the lines around it."
        ),
        _ if ev.starts_with("kube.") => "Run `fluxctl cluster status` to see what is wrong and how to fix it.".into(),
        _ if e.source == "docker" => format!(
            "Run `fluxctl events --source docker --since 24h` to see the container's last lines, then `fluxctl logs {}` if it keeps stopping.",
            e.vm_id.map(|v| v.to_string()).unwrap_or_else(|| "NAME".into())
        ),
        _ => format!(
            "Run `fluxctl events --since 24h --level warn --event {ev}` to see the lines around it, and `fluxctl cluster status` for the health of the machine."
        ),
    }
}

/// What the alert says happened. A failed backup's feed line also says what to run; here that is the next step.
fn summary_of(e: &FeedEvent) -> String {
    match (e.event.as_str(), e.target.as_deref()) {
        ("mac.backup.failed", Some(id)) => format!("Backup {id} did not finish."),
        _ => e.summary.clone(),
    }
}

fn from_events(events: &[FeedEvent], current: &[FeedEvent], since: DateTime<Utc>) -> Vec<Alert> {
    // A run that has since finished fine clears the older failure of the same schedule.
    let healed: BTreeMap<String, DateTime<Utc>> = current
        .iter()
        .filter(|e| rank(&e.level) == 0)
        .map(|e| (key_of(e), e.ts))
        .collect();
    let mut by_key: BTreeMap<String, Alert> = BTreeMap::new();
    let recent: Vec<&FeedEvent> = events.iter().filter(|e| e.ts >= since).collect();
    // The merged feed already holds the backup and schedule runs of the window: count each line once.
    let seen: BTreeSet<(String, i64)> = recent
        .iter()
        .map(|e| (key_of(e), e.ts.timestamp()))
        .collect();
    let older = current
        .iter()
        .filter(|e| !seen.contains(&(key_of(e), e.ts.timestamp())));
    for e in recent.iter().copied().chain(older) {
        if rank(&e.level) == 0 {
            continue;
        }
        let key = key_of(e);
        if healed.get(&key).is_some_and(|ok| *ok >= e.ts) {
            continue;
        }
        match by_key.get_mut(&key) {
            Some(a) => {
                a.count += 1;
                a.first_seen = a.first_seen.min(e.ts);
                if e.ts >= a.last_seen {
                    a.last_seen = e.ts;
                    a.level = e.level.clone();
                    a.summary = summary_of(e);
                    a.explain = explain_of(e).map(str::to_string);
                    a.next_step = next_step(e);
                }
            }
            None => {
                by_key.insert(
                    key.clone(),
                    Alert {
                        key,
                        level: e.level.clone(),
                        source: e.source.clone(),
                        summary: summary_of(e),
                        next_step: next_step(e),
                        explain: explain_of(e).map(str::to_string),
                        target: e.target.clone(),
                        last_seen: e.ts,
                        first_seen: e.ts,
                        count: 1,
                        snoozed_until: None,
                    },
                );
            }
        }
    }
    by_key.into_values().collect()
}

/// Mac instances whose healthcheck failed, one alert per instance. `vms` is `(name, health label value)`.
pub fn unhealthy_macs(vms: &[(String, String)], now: DateTime<Utc>) -> Vec<Alert> {
    vms.iter()
        .filter(|(_, h)| h == "unhealthy")
        .map(|(name, _)| Alert {
            key: format!("mac:unhealthy:{name}"),
            level: "warn".into(),
            source: "mac".into(),
            summary: format!("Mac instance {name} is unhealthy: its healthcheck keeps failing."),
            next_step: format!(
                "Run `fluxctl mac logs {name}` to see why, then `fluxctl mac restart {name}` if it needs a fresh start."
            ),
            explain: Some("MAC_UNHEALTHY".into()),
            target: Some(name.clone()),
            last_seen: now,
            first_seen: now,
            count: 1,
            snoozed_until: None,
        })
        .collect()
}

/// Order for a person: errors first, then warnings, newest first inside each.
pub fn sort(items: &mut [Alert]) {
    items.sort_by(|a, b| {
        rank(&b.level)
            .cmp(&rank(&a.level))
            .then(b.last_seen.cmp(&a.last_seen))
            .then(a.key.cmp(&b.key))
    });
}

/// Is `since` a time the feed can read? The words and the explain code of the refusal.
pub fn check_since(since: &str, now: DateTime<Utc>) -> Result<(), String> {
    event_feed::parse_when(since, now)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Things that could not be read and so may be missing from the list, each saying what to do.
pub fn notes(inp: &Inputs<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if let Err(e) = Snoozes::read(inp.state_dir) {
        out.push(format!("Snoozed alerts are shown again: {e}"));
    }
    if let Some(p) = inp.schedules.as_deref().filter(|p| p.is_file())
        && let Err(e) = fluxvm_core::mac_schedule::load(p)
    {
        out.push(format!("Schedule runs are not checked: {e}."));
    }
    out
}

/// Every alert the state directory can tell, plus `extra` ones the caller found itself (health, unhealthy Macs), with
/// snoozes applied and the list ordered. Audit lines are not alerts (they say who did what, not what is wrong).
pub fn gather(
    inp: &Inputs<'_>,
    since: &str,
    extra: Vec<Alert>,
    now: DateTime<Utc>,
) -> Result<Vec<Alert>> {
    let q = FeedQuery {
        source: Some("vm,mac,kube,docker,backup".into()),
        since: Some(since.to_string()),
        level: Some("warn".into()),
        limit: Some(event_feed::MAX_LIMIT),
        ..Default::default()
    };
    let feed = Feed::parse(&q, now).map_err(|e| anyhow::anyhow!("{e}"))?;
    let from = feed.since.unwrap_or(now - Duration::hours(24));
    let events = event_feed::collect(inp, &feed, now)?;
    let current: Vec<FeedEvent> = event_feed::runs(inp, now);
    let mut all = from_events(&events, &current, from);
    for a in extra {
        match all.iter_mut().find(|x| x.key == a.key) {
            Some(x) => *x = a,
            None => all.push(a),
        }
    }
    // An unreadable list hides nothing; `notes` says why.
    let snoozes = Snoozes::read(inp.state_dir).unwrap_or_default();
    for a in &mut all {
        a.snoozed_until = snoozes.until(&a.key, now);
    }
    sort(&mut all);
    Ok(all)
}

// ---- snooze -------------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Snoozes {
    /// alert key -> unix seconds the snooze ends.
    #[serde(default)]
    pub snoozed: BTreeMap<String, i64>,
}

fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("alert-snoozes.json")
}

/// Why a snooze did not happen, in words: the request was wrong, or the list could not be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnoozeError {
    Request(String),
    Store(String),
}

impl std::fmt::Display for SnoozeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnoozeError::Request(m) | SnoozeError::Store(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SnoozeError {}

/// A key is whatever `GET /v1/alerts` handed out; anything else is refused in words.
pub fn check_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 300 || key.chars().any(|c| c.is_control()) {
        return Err("That is not an alert key. Copy the key shown by `fluxctl alerts` (the KEY column, or `--json`).".into());
    }
    Ok(())
}

impl Snoozes {
    /// The stored list; an empty one when nothing was ever snoozed. A file that cannot be read says what to do.
    pub fn read(state_dir: &Path) -> Result<Snoozes, String> {
        let path = file(state_dir);
        let why = match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(s) => return Ok(s),
                Err(e) => format!("is damaged ({e}). Delete it, then snooze again"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Snoozes::default()),
            Err(e) => format!(
                "cannot be read ({e}). Run this as the user that runs the daemon, or fix the file's permissions"
            ),
        };
        Err(format!("{} {why}.", path.display()))
    }

    pub fn until(&self, key: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let t = *self.snoozed.get(key)?;
        (t > now.timestamp())
            .then(|| Utc.timestamp_opt(t, 0).single())
            .flatten()
    }

    fn save(&mut self, state_dir: &Path, now: DateTime<Utc>) -> std::io::Result<()> {
        self.snoozed.retain(|_, t| *t > now.timestamp());
        let tmp = file(state_dir).with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, file(state_dir))
    }
}

fn not_saved(state_dir: &Path, e: std::io::Error) -> SnoozeError {
    SnoozeError::Store(format!(
        "The snooze list in {} could not be saved ({e}). Run this as the user that runs the daemon, or check that the folder is writable and the disk is not full.",
        state_dir.display()
    ))
}

/// Load, change and save under an exclusive lock, so the daemon and the CLI cannot lose each other's snoozes. `change`
/// says whether anything needs saving.
fn edit<T>(
    state_dir: &Path,
    now: DateTime<Utc>,
    change: impl FnOnce(&mut Snoozes) -> (T, bool),
) -> Result<T, SnoozeError> {
    std::fs::create_dir_all(state_dir).map_err(|e| not_saved(state_dir, e))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file(state_dir).with_extension("lock"))
        .map_err(|e| not_saved(state_dir, e))?;
    lock.lock().map_err(|e| not_saved(state_dir, e))?;
    let mut s = Snoozes::read(state_dir).map_err(SnoozeError::Store)?;
    let (out, dirty) = change(&mut s);
    if dirty {
        s.save(state_dir, now)
            .map_err(|e| not_saved(state_dir, e))?;
    }
    Ok(out)
}

/// Hide `key` for `hours` (1 to 720). Returns when it ends.
pub fn snooze(
    state_dir: &Path,
    key: &str,
    hours: u32,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, SnoozeError> {
    check_key(key).map_err(SnoozeError::Request)?;
    if hours == 0 || hours > MAX_SNOOZE_HOURS {
        return Err(SnoozeError::Request(format!(
            "Snooze for between 1 and {MAX_SNOOZE_HOURS} hours (for example `--hours 24`)."
        )));
    }
    // Whole seconds, which is what the file keeps.
    let until = now.timestamp() + i64::from(hours) * 3600;
    edit(state_dir, now, |s| {
        s.snoozed.insert(key.to_string(), until);
        ((), true)
    })?;
    Ok(Utc.timestamp_opt(until, 0).single().unwrap_or(now))
}

/// Bring a snoozed alert back. Returns whether it was snoozed.
pub fn unsnooze(state_dir: &Path, key: &str, now: DateTime<Utc>) -> Result<bool, SnoozeError> {
    check_key(key).map_err(SnoozeError::Request)?;
    if !file(state_dir).exists() {
        return Ok(false);
    }
    edit(state_dir, now, |s| {
        let was = s.until(key, now).is_some();
        let removed = s.snoozed.remove(key).is_some();
        (was, removed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 24, 12, 0, 0).unwrap()
    }

    fn ev(event: &str, level: &str, target: &str, ago_min: i64) -> FeedEvent {
        FeedEvent {
            ts: now() - Duration::minutes(ago_min),
            source: event_feed::classify(event).into(),
            event: event.into(),
            summary: format!("{event} {target}"),
            level: level.into(),
            target: Some(target.into()),
            vm_id: None,
            fields: Default::default(),
        }
    }

    fn none(_: Uuid) -> Option<(String, String)> {
        None
    }

    #[test]
    fn repeats_fold_into_one_alert_with_a_count_and_the_newest_wording() {
        let evs = vec![
            ev("vm.crash-loop", "error", "web", 50),
            ev("vm.crash-loop", "error", "web", 10),
            ev("vm.crash-loop", "error", "db", 5),
            ev("vm.start", "info", "web", 1),
        ];
        let mut a = from_events(&evs, &[], now() - Duration::hours(24));
        sort(&mut a);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].key, "vm.crash-loop:db");
        let web = &a[1];
        assert_eq!(
            (web.count, web.last_seen),
            (2, now() - Duration::minutes(10))
        );
        assert_eq!(web.first_seen, now() - Duration::minutes(50));
    }

    #[test]
    fn errors_sort_before_warnings_then_newest_first() {
        let evs = vec![
            ev("quota.deny", "warn", "a", 1),
            ev("vm.oom", "error", "b", 30),
            ev("vm.oom", "error", "c", 20),
        ];
        let mut a = from_events(&evs, &[], now() - Duration::hours(24));
        sort(&mut a);
        let keys: Vec<_> = a.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["vm.oom:c", "vm.oom:b", "quota.deny:a"]);
    }

    #[test]
    fn old_warnings_drop_out_but_a_current_problem_stays_whatever_its_age() {
        let old = ev("vm.crash-loop", "error", "web", 60 * 30);
        let broken_backup = ev("mac.backup.failed", "error", "bk-1", 60 * 100);
        let a = from_events(
            &[old],
            std::slice::from_ref(&broken_backup),
            now() - Duration::hours(24),
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key, "mac.backup.failed:bk-1");
        assert!(a[0].next_step.contains("fluxctl mac backup rm bk-1"));
        assert_eq!(a[0].explain.as_deref(), Some("MAC_BACKUP_IO"));
    }

    #[test]
    fn a_later_good_schedule_run_clears_the_earlier_failure() {
        let failed = ev("mac.schedule.run", "error", "nightly", 120);
        let fine = ev("mac.schedule.run", "info", "nightly", 10);
        let a = from_events(
            std::slice::from_ref(&failed),
            &[fine],
            now() - Duration::hours(24),
        );
        assert!(a.is_empty(), "{a:?}");
        // but a failure newer than the good run is kept
        let fine_old = ev("mac.schedule.run", "info", "nightly", 300);
        let a = from_events(&[failed], &[fine_old], now() - Duration::hours(24));
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn a_full_disk_picks_the_no_room_code() {
        let mut e = ev("mac.backup.failed", "error", "bk", 1);
        e.fields
            .insert("error".into(), "No space left on device".into());
        assert_eq!(explain_of(&e), Some("MAC_BACKUP_NO_ROOM"));
        e.fields.insert("error".into(), "checksum mismatch".into());
        assert_eq!(explain_of(&e), Some("MAC_BACKUP_CHECKSUM"));
    }

    #[test]
    fn unhealthy_macs_become_warnings_with_the_unhealthy_code() {
        let a = unhealthy_macs(
            &[
                ("web".into(), "unhealthy".into()),
                ("db".into(), "healthy".into()),
            ],
            now(),
        );
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key, "mac:unhealthy:web");
        assert_eq!(a[0].explain.as_deref(), Some("MAC_UNHEALTHY"));
        assert_eq!(a[0].level, "warn");
    }

    #[test]
    fn every_alert_says_what_to_do_next() {
        for e in [
            "vm.oom",
            "mac.run.failed",
            "kube.node.down",
            "container.die",
            "weird.thing",
            "quota.deny",
        ] {
            let s = next_step(&ev(e, "warn", "x", 1));
            assert!(s.contains("fluxctl"), "{e}: {s}");
        }
    }

    #[test]
    fn snooze_hides_until_it_expires_and_unsnooze_brings_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let until = snooze(dir.path(), "vm.oom:web", 24, now()).unwrap();
        assert_eq!(until, now() + Duration::hours(24));
        let s = Snoozes::read(dir.path()).unwrap();
        assert_eq!(s.until("vm.oom:web", now()), Some(until));
        assert_eq!(s.until("vm.oom:web", now() + Duration::hours(25)), None);
        assert_eq!(s.until("other", now()), None);
        assert!(unsnooze(dir.path(), "vm.oom:web", now()).unwrap());
        assert!(!unsnooze(dir.path(), "vm.oom:web", now()).unwrap());
        let s = Snoozes::read(dir.path()).unwrap();
        assert_eq!(s.until("vm.oom:web", now()), None);
    }

    #[test]
    fn bad_snoozes_say_what_to_use() {
        let dir = tempfile::tempdir().unwrap();
        let words = |k, h| snooze(dir.path(), k, h, now()).unwrap_err().to_string();
        assert!(words("", 24).contains("fluxctl alerts"));
        assert!(words("k", 0).contains("between 1 and"));
        assert!(snooze(dir.path(), "k", 99999, now()).is_err());
        assert!(snooze(dir.path(), "a\nb", 1, now()).is_err());
    }

    #[test]
    fn a_damaged_snooze_list_is_reported_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alert-snoozes.json"), "{not json").unwrap();
        let inp = Inputs {
            state_dir: dir.path(),
            container: &none,
            schedules: None,
            backups: None,
        };
        let n = notes(&inp);
        assert!(
            n[0].contains("damaged") && n[0].contains("Delete it"),
            "{n:?}"
        );
        let e = snooze(dir.path(), "k", 1, now()).unwrap_err();
        assert!(matches!(e, SnoozeError::Store(_)), "{e}");
        assert!(unsnooze(dir.path(), "k", now()).is_err());
        let kept = std::fs::read_to_string(dir.path().join("alert-snoozes.json")).unwrap();
        assert_eq!(kept, "{not json");
    }

    #[test]
    fn unsnooze_with_nothing_stored_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("never-made");
        assert!(!unsnooze(&state, "k", now()).unwrap());
        assert!(!state.exists());
    }

    #[test]
    fn snoozes_written_at_the_same_time_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|s| {
            for i in 0..16 {
                let path = dir.path();
                s.spawn(move || snooze(path, &format!("k{i}"), 1, now()).unwrap());
            }
        });
        assert_eq!(Snoozes::read(dir.path()).unwrap().snoozed.len(), 16);
    }

    #[test]
    fn a_failed_backup_and_schedule_run_count_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(backups.join("bk-1.error"), "No space left on device").unwrap();
        let sched = dir.path().join("mac-schedules.json");
        let at = Utc::now();
        std::fs::write(
            &sched,
            serde_json::json!({"schedules": [{"name": "nightly", "cron": "0 2 * * *", "image": "macos-15",
                "last_run_unix": at.timestamp() - 60, "last_status": "failed"}]})
            .to_string(),
        )
        .unwrap();
        let inp = Inputs {
            state_dir: dir.path(),
            container: &none,
            schedules: Some(sched.clone()),
            backups: Some(backups),
        };
        assert!(notes(&inp).is_empty(), "{:?}", notes(&inp));
        let at = Utc::now();
        let all = gather(&inp, "24h", vec![], at).unwrap();
        let got: Vec<_> = all.iter().map(|a| (a.key.as_str(), a.count)).collect();
        assert_eq!(all.len(), 2, "{got:?}");
        assert!(all.iter().all(|a| a.count == 1), "{got:?}");
        let bk = all
            .iter()
            .find(|a| a.key == "mac.backup.failed:bk-1")
            .unwrap();
        assert_eq!(bk.summary, "Backup bk-1 did not finish.");
        assert_eq!(bk.explain.as_deref(), Some("MAC_BACKUP_NO_ROOM"));
        // a schedule file that cannot be read is said, not skipped in silence
        std::fs::write(&sched, "oops").unwrap();
        let n = notes(&inp);
        assert!(n[0].contains("Schedule runs are not checked"), "{n:?}");
    }

    #[test]
    fn a_bad_since_names_the_explain_code() {
        assert!(check_since("7d", now()).is_ok());
        let e = check_since("yesterday", now()).unwrap_err();
        assert!(e.contains("fluxctl explain EVENTS_BAD_TIME"), "{e}");
    }

    #[test]
    fn gather_reads_the_state_dir_and_applies_snoozes() {
        let dir = tempfile::tempdir().unwrap();
        let ts = (now() - Duration::hours(2)).to_rfc3339();
        let line = |event: &str, result: &str| {
            serde_json::json!({"ts": ts, "event": event, "fields": {"result": result, "name": "web"}}).to_string()
        };
        std::fs::write(
            dir.path().join("events.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                line("vm.crash", "failed"),
                line("vm.start", "ok"),
                line("mac.run", "failed")
            ),
        )
        .unwrap();
        let inp = Inputs {
            state_dir: dir.path(),
            container: &none,
            schedules: None,
            backups: None,
        };
        let all = gather(&inp, "24h", vec![], now()).unwrap();
        assert!(all.iter().all(|a| rank(&a.level) >= 1), "{all:?}");
        assert!(all.len() >= 2, "{all:?}");
        let key = all[0].key.clone();
        snooze(dir.path(), &key, 24, now()).unwrap();
        let all2 = gather(&inp, "24h", vec![], now()).unwrap();
        let hidden = all2.iter().find(|a| a.key == key).unwrap();
        assert!(hidden.snoozed_until.is_some());
        let r = Report::new("24h", all2, false, vec![]);
        assert_eq!(r.snoozed, 1);
        assert!(r.items.iter().all(|a| a.key != key));
        assert_eq!(r.errors + r.warnings, r.items.len());
        // a narrower window drops them
        assert!(gather(&inp, "30m", vec![], now()).unwrap().is_empty());
    }
}
