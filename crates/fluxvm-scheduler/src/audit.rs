// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The audit log: an append-only record of what was changed, by whom, when, and whether it worked.
//!
//! It is the same machinery as the VM event log ([`crate::events`]): every record is a [`VmEvent`] named `audit.<action>`
//! in a JSON-lines file, written by the daemon and by every `fluxctl` process, so the daemon's `/v1/audit` and
//! `fluxctl audit ls` see all of them. It lives in its own file (`<state_dir>/audit.jsonl`) so that the busy VM event
//! log rotating never takes the audit trail with it.
//!
//! What goes in: changes only (a read is not an event): `fluxctl` verbs that change something (`mac run`, `mac rm`,
//! `mac exec`, `kube ...`, `mac-fleet ...`, backups, image pulls and policy decisions) and every `POST`, `PUT`, `PATCH`
//! and `DELETE` the daemon serves. The record names the action, the target, the actor, the source (`cli` or `api`) and
//! the result (`ok`, `failed` or `denied`).
//!
//! What stays out: secrets. Every value passes through [`redact`] before it is written (tokens, passwords, keys, URL
//! credentials, `--token VALUE` flags), a field whose *name* looks like a secret is never stored at all, and a command
//! run with `exec` is recorded as its program name only.
//!
//! Size: the live file rotates at [`Limits::segment_bytes`] into `audit.jsonl.1` .. `audit.jsonl.N`; when the oldest
//! segment is dropped, `audit.trim.json` remembers how many records went and [`AuditLog::trim_notice`] says so in plain
//! words, so a missing record is never a surprise.

use crate::events::{EventFilter, EventLog, VmEvent};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// File name of the live segment under the state directory.
pub const FILE: &str = "audit.jsonl";
/// File name of the note left when old records are dropped.
pub const TRIM_FILE: &str = "audit.trim.json";
/// Every audit event name starts with this.
pub const PREFIX: &str = "audit.";
/// Longest value kept in one field; longer ones are cut.
const MAX_VALUE: usize = 240;
const REDACTED: &str = "[redacted]";

/// How big the log may get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The live file rotates once it passes this size.
    pub segment_bytes: u64,
    /// Rotated segments kept besides the live one. The most the log holds is `segment_bytes * (keep_rotated + 1)`.
    pub keep_rotated: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            segment_bytes: 8 * 1024 * 1024,
            keep_rotated: 3,
        }
    }
}

impl Limits {
    /// The defaults, with `FLUXVM_AUDIT_SEGMENT_MB` (1 or more) overriding the segment size.
    pub fn from_env() -> Self {
        let mut l = Limits::default();
        if let Some(mb) = std::env::var("FLUXVM_AUDIT_SEGMENT_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|mb| *mb >= 1)
        {
            l.segment_bytes = mb * 1024 * 1024;
        }
        l
    }

    pub fn max_bytes(&self) -> u64 {
        self.segment_bytes * (self.keep_rotated as u64 + 1)
    }
}

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Failed,
    /// Refused: not allowed, a policy said no, or the credentials were wrong.
    Denied,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Denied => "denied",
        }
    }

    /// From an HTTP status: 2xx/3xx ok, 401/403/428 denied, anything else failed.
    pub fn from_status(status: u16) -> Self {
        match status {
            200..=399 => Outcome::Ok,
            401 | 403 | 428 => Outcome::Denied,
            _ => Outcome::Failed,
        }
    }
}

/// One thing to record. Build it, hand it to [`record`] (or [`AuditLog::append`]).
#[derive(Debug, Clone)]
pub struct Entry {
    pub action: String,
    pub target: String,
    pub actor: String,
    /// `cli` or `api`.
    pub source: String,
    pub outcome: Outcome,
    pub detail: Vec<(String, String)>,
}

impl Entry {
    pub fn new(action: &str) -> Self {
        Entry {
            action: action.to_string(),
            target: String::new(),
            actor: local_actor(),
            source: "cli".into(),
            outcome: Outcome::Ok,
            detail: Vec::new(),
        }
    }

    pub fn target(mut self, t: &str) -> Self {
        self.target = t.to_string();
        self
    }

    pub fn actor(mut self, a: &str) -> Self {
        self.actor = a.to_string();
        self
    }

    pub fn source(mut self, s: &str) -> Self {
        self.source = s.to_string();
        self
    }

    pub fn outcome(mut self, o: Outcome) -> Self {
        self.outcome = o;
        self
    }

    pub fn detail(mut self, k: &str, v: &str) -> Self {
        self.detail.push((k.to_string(), v.to_string()));
        self
    }

    /// The stored form: redacted, trimmed, in the shape of the daemon's event log.
    pub fn to_event(&self) -> VmEvent {
        let mut fields = BTreeMap::new();
        let put = |fields: &mut BTreeMap<String, String>, k: &str, v: &str| {
            let v = clean(v);
            if !v.is_empty() {
                fields.insert(k.to_string(), v);
            }
        };
        put(&mut fields, "actor", &self.actor);
        put(&mut fields, "target", &self.target);
        put(&mut fields, "source", &self.source);
        put(&mut fields, "result", self.outcome.as_str());
        for (k, v) in &self.detail {
            // Never copy a secret under any name: the name decides before the value is even looked at.
            if matches!(k.as_str(), "actor" | "target" | "source" | "result") {
                continue;
            }
            if is_secret_name(k) {
                fields.insert(clean_key(k), REDACTED.to_string());
            } else {
                put(&mut fields, &clean_key(k), v);
            }
        }
        let action: String = self
            .action
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .collect();
        VmEvent {
            ts: Utc::now(),
            event: format!("{PREFIX}{action}"),
            vm_id: None,
            fields,
        }
    }
}

fn clean_key(k: &str) -> String {
    k.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(40)
        .collect()
}

/// Redacts, flattens control characters and cuts to [`MAX_VALUE`].
fn clean(v: &str) -> String {
    let red = redact(v);
    let flat: String = red
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() > MAX_VALUE {
        let cut: String = flat.chars().take(MAX_VALUE).collect();
        format!("{cut}...")
    } else {
        flat.to_string()
    }
}

const SECRET_NAME_WORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "private",
    "credential",
    "cookie",
    "authorization",
    "bearer",
    "passphrase",
    "kubeconfig",
];

/// A field or flag name that holds a secret.
pub fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_NAME_WORDS.iter().any(|w| n.contains(w))
        || n == "key"
        || n.ends_with("_key")
        || n.ends_with("-key")
}

/// Hides secrets in free text: `password=x`, `Bearer x`, `--token x`, `https://user:pass@host`, and well-known token
/// shapes (GitHub, GitLab, Slack, AWS access keys, JWTs, `sk-` API keys).
pub fn redact(s: &str) -> String {
    let first = crate::diagnostics::redact_text(s);
    let mut out: Vec<String> = Vec::new();
    let mut hide_next = false;
    for word in first.split(' ') {
        if hide_next && !word.is_empty() {
            hide_next = false;
            out.push(REDACTED.to_string());
            continue;
        }
        let bare = word.trim_matches(|c| c == '"' || c == '\'');
        if bare.starts_with('-')
            && !bare.contains('=')
            && is_secret_name(bare.trim_start_matches('-'))
        {
            hide_next = true;
            out.push(word.to_string());
            continue;
        }
        out.push(redact_word(word));
    }
    out.join(" ")
}

fn redact_word(word: &str) -> String {
    let w = hide_url_password(word);
    let bare =
        w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')));
    if looks_like_token(bare) {
        return w.replace(bare, REDACTED);
    }
    w
}

fn hide_url_password(word: &str) -> String {
    let Some(i) = word.find("://") else {
        return word.to_string();
    };
    let rest = &word[i + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at) = authority.rfind('@') else {
        return word.to_string();
    };
    let userinfo = &authority[..at];
    let shown = match userinfo.split_once(':') {
        Some((user, _)) => format!("{user}:{REDACTED}"),
        None => REDACTED.to_string(),
    };
    format!(
        "{}://{}{}{}",
        &word[..i],
        shown,
        &authority[at..],
        &rest[authority_end..]
    )
}

fn looks_like_token(w: &str) -> bool {
    let long = |min: usize| w.len() >= min;
    (w.starts_with("ghp_")
        || w.starts_with("gho_")
        || w.starts_with("ghu_")
        || w.starts_with("ghs_")
        || w.starts_with("ghr_"))
        && long(20)
        || w.starts_with("github_pat_") && long(30)
        || w.starts_with("glpat-") && long(20)
        || w.starts_with("xox")
            && w.len() > 4
            && w.as_bytes()[3].is_ascii_alphabetic()
            && w[4..].starts_with('-')
            && long(20)
        || w.starts_with("sk-") && long(24)
        || w.starts_with("AKIA")
            && w.len() == 20
            && w.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        || w.starts_with("eyJ") && w.matches('.').count() == 2 && long(30)
}

/// A stored audit record, as served by the API and printed by the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub ts: DateTime<Utc>,
    pub action: String,
    pub actor: String,
    pub target: String,
    pub result: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, String>,
}

impl Record {
    pub fn from_event(e: &VmEvent) -> Option<Record> {
        let action = e.event.strip_prefix(PREFIX)?.to_string();
        let mut detail = e.fields.clone();
        let mut take = |k: &str| detail.remove(k).unwrap_or_default();
        let actor = take("actor");
        let target = take("target");
        let result = take("result");
        let source = take("source");
        Some(Record {
            ts: e.ts,
            action,
            actor,
            target,
            result,
            source,
            detail,
        })
    }
}

/// What to show. Everything is optional; filters combine with AND.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Query {
    /// Only records after this time.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Prefix of the action: `mac.` matches `mac.run`, `mac.rm`; `mac.run` matches itself.
    #[serde(default)]
    pub action: Option<String>,
    /// Part of the target's name (case does not matter).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    /// `ok`, `failed` or `denied`.
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Records to skip, newest first (for paging).
    #[serde(default)]
    pub offset: Option<usize>,
}

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 1000;

impl Query {
    pub fn matches(&self, r: &Record) -> bool {
        if self.since.is_some_and(|s| r.ts <= s) || self.until.is_some_and(|u| r.ts > u) {
            return false;
        }
        if let Some(a) = self.action.as_deref().filter(|a| !a.is_empty()) {
            if !r.action.starts_with(a) {
                return false;
            }
        }
        if let Some(t) = self.target.as_deref().filter(|t| !t.is_empty()) {
            if !r.target.to_lowercase().contains(&t.to_lowercase()) {
                return false;
            }
        }
        if let Some(a) = self.actor.as_deref().filter(|a| !a.is_empty()) {
            if r.actor != a {
                return false;
            }
        }
        if let Some(x) = self.result.as_deref().filter(|x| !x.is_empty()) {
            if !r.result.eq_ignore_ascii_case(x) {
                return false;
            }
        }
        true
    }
}

/// One page of results, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub items: Vec<Record>,
    /// Records that matched in all (not just this page).
    pub total: usize,
    #[serde(default)]
    pub offset: usize,
    /// Pass this as `offset` for the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Plain-English note that older records were dropped, when they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trim {
    /// Records dropped so far, over the life of the log.
    pub dropped: u64,
    pub last_trim: DateTime<Utc>,
    pub max_bytes: u64,
}

pub struct AuditLog {
    dir: PathBuf,
    limits: Limits,
    lock: Mutex<()>,
}

static GLOBAL: OnceLock<AuditLog> = OnceLock::new();

/// Install the process-wide log (first caller wins).
pub fn init(state_dir: &Path) {
    let _ = GLOBAL.set(AuditLog::with_limits(state_dir, Limits::from_env()));
}

pub fn global() -> Option<&'static AuditLog> {
    GLOBAL.get()
}

/// Record into the global log, if initialised. Never fails the caller.
pub fn record(entry: Entry) {
    if let Some(log) = global() {
        if let Err(e) = log.append(&entry) {
            tracing::warn!(error = %e, "failed to append an audit record");
        }
    }
}

/// The person at the keyboard: `$USER`, else `unknown`.
pub fn local_actor() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

impl AuditLog {
    pub fn new(state_dir: &Path) -> Self {
        Self::with_limits(state_dir, Limits::from_env())
    }

    pub fn with_limits(state_dir: &Path, limits: Limits) -> Self {
        AuditLog {
            dir: state_dir.to_path_buf(),
            limits,
            lock: Mutex::new(()),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Segment 0 is the live file, 1 the newest rotated one.
    fn segment(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(FILE)
        } else {
            self.dir.join(format!("{FILE}.{n}"))
        }
    }

    fn live(&self) -> EventLog {
        EventLog::at(self.segment(0))
    }

    pub fn append(&self, entry: &Entry) -> Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        if std::fs::metadata(self.segment(0)).is_ok_and(|m| m.len() > self.limits.segment_bytes) {
            self.rotate();
        }
        let live = self.live();
        let existed = live.path().exists();
        live.append_unrotated(&entry.to_event())?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(live.path(), std::fs::Permissions::from_mode(0o600));
        }
        let _ = existed;
        Ok(())
    }

    /// Shifts every segment up by one; the oldest falls off the end and is counted in the trim note.
    fn rotate(&self) {
        let oldest = self.segment(self.limits.keep_rotated);
        let mut dropped = 0u64;
        if self.limits.keep_rotated == 0 {
            dropped = count_lines(&self.segment(0));
            let _ = std::fs::remove_file(self.segment(0));
        } else {
            if oldest.exists() {
                dropped = count_lines(&oldest);
                let _ = std::fs::remove_file(&oldest);
            }
            for n in (1..self.limits.keep_rotated).rev() {
                let from = self.segment(n);
                if from.exists() {
                    let _ = std::fs::rename(&from, self.segment(n + 1));
                }
            }
            let _ = std::fs::rename(self.segment(0), self.segment(1));
        }
        if dropped > 0 {
            let prev = self.trim().map(|t| t.dropped).unwrap_or(0);
            let t = Trim {
                dropped: prev + dropped,
                last_trim: Utc::now(),
                max_bytes: self.limits.max_bytes(),
            };
            if let Ok(json) = serde_json::to_vec(&t) {
                let _ = std::fs::write(self.dir.join(TRIM_FILE), json);
            }
        }
    }

    pub fn trim(&self) -> Option<Trim> {
        serde_json::from_slice(&std::fs::read(self.dir.join(TRIM_FILE)).ok()?).ok()
    }

    /// Plain-English account of dropped records, if any were.
    pub fn trim_notice(&self) -> Option<String> {
        let t = self.trim()?;
        let oldest = self.all().ok()?.first().map(|r| r.ts);
        Some(trim_message(&t, oldest))
    }

    /// Every record, oldest first.
    pub fn all(&self) -> Result<Vec<Record>> {
        let mut out = Vec::new();
        for n in (0..=self.limits.keep_rotated).rev() {
            let log = EventLog::at(self.segment(n));
            for e in log.list(&EventFilter::default())? {
                if let Some(r) = Record::from_event(&e) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }

    /// One page of matching records, newest first.
    pub fn query(&self, q: &Query) -> Result<Page> {
        let mut hits: Vec<Record> = self.all()?.into_iter().filter(|r| q.matches(r)).collect();
        hits.reverse();
        let total = hits.len();
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = q.offset.unwrap_or(0).min(total);
        let items: Vec<Record> = hits.into_iter().skip(offset).take(limit).collect();
        let next = offset + items.len();
        Ok(Page {
            items,
            total,
            offset,
            next_offset: (next < total).then_some(next),
            notice: self.trim_notice(),
        })
    }
}

fn count_lines(p: &Path) -> u64 {
    std::fs::read(p)
        .map(|b| b.iter().filter(|c| **c == b'\n').count() as u64)
        .unwrap_or(0)
}

/// The sentence shown wherever records are listed after some were dropped.
pub fn trim_message(t: &Trim, oldest: Option<DateTime<Utc>>) -> String {
    let mb = (t.max_bytes / (1024 * 1024)).max(1);
    let from = oldest
        .map(|o| format!(" The log now starts on {}.", o.format("%Y-%m-%d")))
        .unwrap_or_default();
    format!(
        "The audit log is capped at {mb} MB, so {} of the oldest records were removed (last on {}).{from} To keep a longer history, run `fluxctl audit export --out audit.jsonl` regularly, or raise the cap with FLUXVM_AUDIT_SEGMENT_MB.",
        t.dropped,
        t.last_trim.format("%Y-%m-%d"),
    )
}

// ---- what is worth recording ---------------------------------------------------------------------------------------

/// Paths whose changes are not worth a record: they fire constantly or carry keystrokes.
fn http_skipped(path: &str) -> bool {
    path.starts_with("/v1/audit")
        || path.starts_with("/v1/events")
        || path == "/fleet/register"
        || path.ends_with("/input")
        || path.ends_with("/resize")
        || path.ends_with("/heartbeat")
        || path.contains("/terminal")
}

fn singular(word: &str) -> String {
    if word.chars().any(|c| c.is_ascii_digit()) {
        return word.to_string();
    }
    if ["xes", "ses", "ches"].iter().any(|s| word.ends_with(s)) {
        return word[..word.len() - 2].to_string();
    }
    word.strip_suffix('s').unwrap_or(word).to_string()
}

/// What an API change is called and what it touched, from the method and path alone (never the body, which may hold
/// secrets): `POST /v1/vms/{id}/start` is `vm.start` on `{id}`; `DELETE /v1/pools/web` is `pool.delete` on `web`.
/// `None` for reads and for paths not worth recording.
pub fn describe_http(method: &str, path: &str) -> Option<(String, String)> {
    let verb = match method.to_ascii_uppercase().as_str() {
        "POST" => "create",
        "PUT" | "PATCH" => "update",
        "DELETE" => "delete",
        _ => return None,
    };
    if http_skipped(path) {
        return None;
    }
    let rest = path
        .strip_prefix("/v1/")
        .unwrap_or(path.trim_start_matches('/'));
    let mut segs = rest.split('/').filter(|s| !s.is_empty());
    let resource = singular(segs.next()?);
    let target = segs.next().unwrap_or("").to_string();
    let tail: Vec<&str> = segs.collect();
    let action = match (tail.is_empty(), verb) {
        (true, v) => format!("{resource}.{v}"),
        (false, "delete") => format!("{resource}.{}-delete", tail.join("-")),
        (false, _) => format!("{resource}.{}", tail.join("-")),
    };
    Some((action, target))
}

/// A `fluxctl` command that changes something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliAction {
    pub action: String,
    pub target: String,
    /// The names of the flags given (never their values).
    pub flags: Vec<String>,
    /// For `exec`: the program only.
    pub program: Option<String>,
}

/// Global flags that take a value, so the value is not mistaken for the verb.
const GLOBAL_VALUE_FLAGS: &[&str] = &[
    "--config",
    "--listen",
    "--state-dir",
    "-o",
    "--output-format",
    "--server",
    "--server-token",
    "--totp",
    "--context",
];

/// Top-level groups whose sub-verbs are audited unless they only read.
const GROUPS: &[&str] = &[
    "mac",
    "mac-fleet",
    "kube",
    "fleet",
    "image",
    "sandbox",
    "docker",
    "cluster",
    "gateway",
    "branch",
    "oci",
    "disk",
    "pool",
    "vm-image",
    "macos-pool",
    "agent",
    "service",
    "node-agent",
    "migrate",
    "vz",
    "vznet",
    "recipe",
];

/// Top-level verbs that change something.
const VERBS: &[&str] = &[
    "create",
    "start",
    "stop",
    "pause",
    "resume",
    "hibernate",
    "delete",
    "restart",
    "label",
    "rename-vm",
    "clone-vm",
    "fork-vm",
    "terminate",
    "freeze",
    "thaw",
    "resources",
    "cpuset",
    "balloon",
    "hotplug",
    "snapshot",
    "snapshot-delete",
    "backup",
    "backup-delete",
    "restore-backup",
    "start-from-snapshot",
    "up",
    "down",
    "run",
    "poweroff",
    "reboot",
    "kill",
    "enable",
    "disable",
    "exec",
    "copy-to",
    "copy-from",
    "build-image",
    "import-image",
    "import-raw",
    "pull-raw",
    "pull-tar",
    "import-tar",
    "import-fs",
    "remove",
    "clean",
    "clone",
    "rename",
    "fix",
    "import-compose",
    "edit",
    "bind",
    "signin",
];

/// Second words that only read or check.
const READ_ONLY: &[&str] = &[
    "ps",
    "ls",
    "list",
    "images",
    "inspect",
    "logs",
    "top",
    "status",
    "show",
    "get",
    "doctor",
    "version",
    "info",
    "events",
    "watch",
    "stats",
    "history",
    "diff",
    "help",
    "search",
    "check",
    "explain",
    "view",
    "report",
    "plan",
    "verify",
    "backups",
    "which",
    "ready",
    "wait-ready",
    "health",
    "cat",
    "tree",
    "df",
    "du",
    "stat",
    "port",
    "ports",
    "top-level",
    "completion",
    "completions",
    "cheatsheet",
    "export",
    "save",
    "preflight",
    "audit",
    "lint",
];

/// Decides whether `args` (`argv`, program name first) is a change worth recording and, if so, what to call it.
pub fn describe_cli(args: &[String]) -> Option<CliAction> {
    let mut positional: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut program = None;
    let mut it = args.iter().skip(1).peekable();
    let mut after_dashes = false;
    while let Some(a) = it.next() {
        if after_dashes {
            if program.is_none() {
                program = Some(a.rsplit('/').next().unwrap_or(a).to_string());
            }
            continue;
        }
        if a == "--" {
            after_dashes = true;
        } else if let Some(flag) = a.strip_prefix("--") {
            let name = flag.split('=').next().unwrap_or(flag).to_string();
            if name == "dry-run" || name == "help" {
                return None;
            }
            if !a.contains('=') && GLOBAL_VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
            flags.push(format!("--{name}"));
        } else if a.starts_with('-') && a.len() > 1 {
            if a == "-h" {
                return None;
            }
            if GLOBAL_VALUE_FLAGS.contains(&a.as_str()) {
                it.next();
            }
        } else {
            positional.push(a.clone());
        }
    }
    let first = positional.first()?.as_str();
    let (action, consumed) = if GROUPS.contains(&first) {
        let second = positional.get(1)?.as_str();
        if READ_ONLY.contains(&second) {
            return None;
        }
        (format!("{first}.{second}"), 2)
    } else if VERBS.contains(&first) {
        (first.to_string(), 1)
    } else {
        return None;
    };
    // A third word that is itself a verb (`kube backup run-due`, `mac-fleet ssh ...`) names the action; the target is
    // the first plain argument after the verbs.
    let target = positional.get(consumed).cloned().unwrap_or_default();
    let is_exec = action == "exec" || action.ends_with(".exec");
    Some(CliAction {
        action,
        target,
        flags,
        program: if is_exec { program } else { None },
    })
}

/// Everything the CLI records for a finished command.
pub fn cli_entry(a: &CliAction, outcome: Outcome, error: Option<&str>) -> Entry {
    let mut e = Entry::new(&a.action)
        .target(&a.target)
        .source("cli")
        .outcome(outcome);
    if !a.flags.is_empty() {
        e = e.detail("flags", &a.flags.join(" "));
    }
    if let Some(p) = &a.program {
        e = e.detail("program", p);
    }
    if let Some(msg) = error {
        e = e.detail("error", msg.lines().next().unwrap_or(""));
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxvm-audit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(action: &str, target: &str, outcome: Outcome) -> Entry {
        Entry::new(action)
            .target(target)
            .actor("sam")
            .outcome(outcome)
    }

    #[test]
    fn redacts_secrets_in_every_shape() {
        for (input, secret) in [
            ("login --token abc123def456", "abc123def456"),
            ("login --password=hunter2", "hunter2"),
            ("curl -H 'Authorization: Bearer abc.def.ghi'", "abc.def.ghi"),
            (
                "clone https://bob:s3cretpass@git.example.com/x.git",
                "s3cretpass",
            ),
            (
                "push ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            ),
            ("key AKIAIOSFODNN7EXAMPLE here", "AKIAIOSFODNN7EXAMPLE"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijk",
                "eyJhbGciOiJIUzI1NiJ9",
            ),
            (
                "api sk-abcdefghijklmnopqrstuvwxyz",
                "sk-abcdefghijklmnopqrstuvwxyz",
            ),
            ("DB_PASSWORD=swordfish run", "swordfish"),
            ("--api-key topsecretvalue", "topsecretvalue"),
        ] {
            let out = redact(input);
            assert!(
                !out.contains(secret),
                "{input:?} leaked {secret:?}: {out:?}"
            );
            assert!(out.contains(REDACTED), "{input:?} -> {out:?}");
        }
        // ordinary text is untouched
        assert_eq!(
            redact("mac run macos-15 --name web"),
            "mac run macos-15 --name web"
        );
        assert_eq!(
            redact("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn secret_named_fields_are_never_stored() {
        let e = Entry::new("kube.secrets")
            .detail("token", "visible-if-bug")
            .detail("api_key", "visible-if-bug")
            .detail("name", "web")
            .to_event();
        let all = serde_json::to_string(&e).unwrap();
        assert!(!all.contains("visible-if-bug"), "{all}");
        assert_eq!(e.fields.get("token").map(String::as_str), Some(REDACTED));
        assert_eq!(e.fields.get("name").map(String::as_str), Some("web"));
        // a caller cannot overwrite the fixed fields through detail
        let e = Entry::new("x")
            .actor("sam")
            .detail("actor", "root")
            .to_event();
        assert_eq!(e.fields["actor"], "sam");
    }

    #[test]
    fn long_and_multiline_values_are_cut() {
        let e = Entry::new("x")
            .detail("note", &format!("a\nb{}", "z".repeat(1000)))
            .to_event();
        let v = &e.fields["note"];
        assert!(!v.contains('\n'));
        assert!(v.chars().count() <= MAX_VALUE + 3);
    }

    #[test]
    fn filters_combine_and_page_newest_first() {
        let d = dir();
        let log = AuditLog::with_limits(&d, Limits::default());
        log.append(&rec("mac.run", "web", Outcome::Ok)).unwrap();
        log.append(&rec("mac.rm", "web", Outcome::Ok)).unwrap();
        log.append(&rec("mac.run", "db", Outcome::Failed)).unwrap();
        log.append(&rec("kube.backup", "default", Outcome::Ok))
            .unwrap();
        log.append(&rec("image.pull", "evil:latest", Outcome::Denied))
            .unwrap();

        let all = log.query(&Query::default()).unwrap();
        assert_eq!(all.total, 5);
        assert_eq!(all.items[0].action, "image.pull", "newest first");

        let mac = log
            .query(&Query {
                action: Some("mac.".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            mac.items
                .iter()
                .map(|r| r.action.as_str())
                .collect::<Vec<_>>(),
            ["mac.run", "mac.rm", "mac.run"]
        );
        let web = log
            .query(&Query {
                action: Some("mac.".into()),
                target: Some("WEB".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(web.total, 2);
        let failed = log
            .query(&Query {
                result: Some("failed".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(failed.items.len(), 1);
        assert_eq!(failed.items[0].target, "db");
        assert_eq!(
            log.query(&Query {
                actor: Some("nobody".into()),
                ..Default::default()
            })
            .unwrap()
            .total,
            0
        );

        let future = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(
            log.query(&Query {
                since: Some(future),
                ..Default::default()
            })
            .unwrap()
            .total,
            0
        );
        assert_eq!(
            log.query(&Query {
                until: Some(future),
                ..Default::default()
            })
            .unwrap()
            .total,
            5
        );

        let p1 = log
            .query(&Query {
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((p1.items.len(), p1.next_offset), (2, Some(2)));
        let p3 = log
            .query(&Query {
                limit: Some(2),
                offset: p1.next_offset.and_then(|o| Some(o + 2)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((p3.items.len(), p3.next_offset), (1, None));
        assert!(all.notice.is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rotation_caps_the_size_and_says_so_in_plain_words() {
        let d = dir();
        let limits = Limits {
            segment_bytes: 600,
            keep_rotated: 2,
        };
        let log = AuditLog::with_limits(&d, limits);
        for i in 0..60 {
            log.append(&rec("mac.run", &format!("vm-{i}"), Outcome::Ok))
                .unwrap();
        }
        let mut total_bytes = 0;
        for n in 0..=3 {
            if let Ok(m) = std::fs::metadata(log.segment(n)) {
                total_bytes += m.len();
            }
        }
        assert!(
            !log.segment(3).exists(),
            "never more than keep_rotated segments"
        );
        // each segment may run one record past the limit before it rotates
        assert!(total_bytes < limits.max_bytes() + 3 * 400, "{total_bytes}");

        let page = log
            .query(&Query {
                limit: Some(1000),
                ..Default::default()
            })
            .unwrap();
        assert!(page.total < 60 && page.total > 0, "{}", page.total);
        // what is left is the newest, in order
        assert_eq!(page.items[0].target, "vm-59");
        let notice = page.notice.expect("trimmed, so a notice");
        assert!(notice.contains("oldest records were removed"), "{notice}");
        assert!(notice.contains("fluxctl audit export"), "{notice}");
        let dropped = log.trim().unwrap().dropped as usize;
        assert_eq!(
            dropped + page.total,
            60,
            "every record is either kept or counted as dropped"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(log.segment(0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn describes_api_changes_without_the_body() {
        assert_eq!(describe_http("GET", "/v1/vms"), None);
        assert_eq!(
            describe_http("POST", "/v1/vms"),
            Some(("vm.create".into(), "".into()))
        );
        assert_eq!(
            describe_http("POST", "/v1/vms/abc/start"),
            Some(("vm.start".into(), "abc".into()))
        );
        assert_eq!(
            describe_http("DELETE", "/v1/pools/web"),
            Some(("pool.delete".into(), "web".into()))
        );
        assert_eq!(
            describe_http("POST", "/v1/sandboxes"),
            Some(("sandbox.create".into(), "".into()))
        );
        assert_eq!(
            describe_http("POST", "/v1/k3s/default/backups/b1/restore"),
            Some(("k3s.backups-b1-restore".into(), "default".into()))
        );
        assert_eq!(describe_http("POST", "/v1/audit"), None);
        assert_eq!(describe_http("POST", "/v1/vms/abc/input"), None);
    }

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn describes_cli_changes_and_skips_reads() {
        let run = describe_cli(&args("fluxctl mac run macos-15 --name web --detach")).unwrap();
        assert_eq!(
            (run.action.as_str(), run.target.as_str()),
            ("mac.run", "macos-15")
        );
        assert_eq!(run.flags, ["--name", "--detach"]);
        assert_eq!(describe_cli(&args("fluxctl mac ps")), None);
        assert_eq!(
            describe_cli(&args("fluxctl mac rm web")).unwrap().target,
            "web"
        );
        // global flags with values do not become the verb
        let r = describe_cli(&args(
            "fluxctl --server http://h:1 --server-token s3cr3t mac rm web",
        ))
        .unwrap();
        assert_eq!((r.action.as_str(), r.target.as_str()), ("mac.rm", "web"));
        assert!(!format!("{r:?}").contains("s3cr3t"));
        // exec records the program only, never its arguments
        let x = describe_cli(&args(
            "fluxctl mac exec web -- /usr/bin/curl -H Authorization:xyz",
        ))
        .unwrap();
        assert_eq!(x.program.as_deref(), Some("curl"));
        assert!(!format!("{x:?}").contains("xyz"));
        assert_eq!(describe_cli(&args("fluxctl mac run x --dry-run")), None);
        assert_eq!(describe_cli(&args("fluxctl list")), None);
        assert_eq!(
            describe_cli(&args("fluxctl delete vm1")).unwrap().action,
            "delete"
        );
        assert_eq!(describe_cli(&args("fluxctl audit ls")), None);
    }

    #[test]
    fn outcome_from_status() {
        assert_eq!(Outcome::from_status(204), Outcome::Ok);
        assert_eq!(Outcome::from_status(403), Outcome::Denied);
        assert_eq!(Outcome::from_status(500), Outcome::Failed);
    }
}
