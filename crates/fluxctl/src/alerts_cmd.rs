// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl alerts`: what needs attention now, in one short list, each with the next step. Built from data that already
//! exists (`fluxvm_scheduler::alerts`): warn and error events of the last day, backups and schedule runs that failed, Mac
//! instances that are unhealthy and the problems of `fluxctl cluster status`. Exit code 1 when any alert is an error, 2
//! when the list could not be read (or a snooze not saved), so a cron job or a prompt can tell the two apart.
//!
//! A daemon named with `--server` or a context is asked (`GET /v1/alerts`) and its refusal is shown. On this machine the
//! local daemon is asked, as only it knows about unhealthy Mac instances and machine health; when it cannot give the list,
//! the state directory is read instead (events, backups, schedules) and a note says why and what to do. `snooze KEY` hides
//! one alert for 24 hours (`POST /v1/alerts/snooze`, or the state directory when no daemon is running).

use crate::remote::{self, Remote};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use fluxvm_scheduler::alerts::{self, Alert, Report};
use fluxvm_scheduler::event_feed::Inputs;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// How long a daemon gets to answer.
const PATIENCE: Duration = Duration::from_secs(20);

#[derive(clap::Args, Debug, Clone)]
#[command(
    after_help = "Exit code: 0 when nothing is an error, 1 when something is, 2 when the list could not be read. Snoozed alerts do not count.\n\
Examples:\n  fluxctl alerts\n  fluxctl alerts --since 7d --json\n  fluxctl alerts snooze mac.schedule.run:nightly --hours 24\n  fluxctl alerts --all"
)]
pub struct AlertsArgs {
    /// How far back warnings and errors count: `30m`, `24h`, `7d`, or a timestamp. Failed backups and schedules, unhealthy
    /// instances and health problems that are true now always show.
    #[arg(long, value_name = "WHEN", default_value = "24h")]
    pub since: String,
    /// Also list the alerts you snoozed.
    #[arg(long)]
    pub all: bool,
    /// The answer as JSON (for the list, the same document as `GET /v1/alerts`).
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<AlertsCommand>,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum AlertsCommand {
    /// Hide one alert for a while (default 24 hours). It comes back when the time is up if the problem is still there.
    Snooze {
        /// The key shown by `fluxctl alerts` (for example `mac.schedule.run:nightly`).
        key: String,
        /// How long, in hours (1 to 720).
        #[arg(long, default_value_t = alerts::DEFAULT_SNOOZE_HOURS)]
        hours: u32,
    },
    /// Bring a snoozed alert back now.
    Unsnooze {
        /// The key shown by `fluxctl alerts --all`.
        key: String,
    },
}

#[derive(Clone, Copy)]
pub enum Where<'a> {
    /// The daemon the user named (`--server`, a context): its answer, or its error.
    Server(&'a Remote),
    /// This machine: the daemon when one is given and answers, otherwise the state directory.
    ThisMachine(Option<&'a Remote>, &'a Path),
}

/// A daemon call that did not give an answer.
struct Failed {
    /// The HTTP status; `None` when nothing answered.
    status: Option<u16>,
    unreachable: bool,
    error: anyhow::Error,
}

/// The daemon did not answer at all (as opposed to answering with an error).
fn is_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>()
            .is_some_and(|r| r.is_connect() || r.is_timeout())
    })
}

/// The status in the error [`Remote::call`] makes: `METHOD PATH: 401 Unauthorized: message`.
fn status_of(method: &Method, path: &str, e: &anyhow::Error) -> Option<u16> {
    e.to_string()
        .strip_prefix(&format!("{method} {path}: "))?
        .get(..3)?
        .parse()
        .ok()
}

async fn ask(r: &Remote, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failed> {
    match tokio::time::timeout(PATIENCE, r.call(method.clone(), path, body)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(error)) => Err(Failed {
            status: status_of(&method, path, &error),
            unreachable: is_unreachable(&error),
            error,
        }),
        Err(_) => Err(Failed {
            status: None,
            unreachable: true,
            error: anyhow!("no answer in {} seconds", PATIENCE.as_secs()),
        }),
    }
}

/// A named daemon's refusal, with what to do next.
fn refused(r: &Remote, f: Failed) -> anyhow::Error {
    let base = r.base();
    let step = match f.status {
        _ if f.unreachable => format!(
            "Could not reach the daemon at {base}. Check it is running and that the address is right (`fluxctl context list` shows saved ones)"
        ),
        Some(401) => format!(
            "The daemon at {base} wants a token: pass --server-token TOKEN or set FLUXVM_TOKEN"
        ),
        Some(403) => format!(
            "The daemon at {base} refused this token. Reading alerts needs an admin or read-only token without a tenant; snoozing needs an admin token"
        ),
        Some(404) => format!(
            "The daemon at {base} has no alerts yet: upgrade it to this version, or use `fluxctl events --level warn`"
        ),
        _ => return f.error,
    };
    f.error.context(step)
}

/// Why the list comes from the logs only, and what to do about it.
fn why_logs_only(r: &Remote, f: &Failed) -> String {
    let base = r.base();
    let why = match f.status {
        _ if f.unreachable => format!(
            "No daemon answered at {base}. Start it (`fluxctl serve`, or your system service)"
        ),
        Some(401) => format!(
            "The daemon at {base} wants a token. Pass --server-token TOKEN or set FLUXVM_TOKEN"
        ),
        Some(404) => {
            format!("The daemon at {base} is older than this fluxctl. Restart it on this version")
        }
        _ => format!(
            "The daemon at {base} could not give the list ({:#}). See its log",
            f.error
        ),
    };
    format!(
        "{why}. Until then this list is from the logs only: unhealthy Mac instances and machine health are not checked (`fluxctl cluster status` shows the health)."
    )
}

fn local_report(state_dir: &Path, since: &str, all: bool, why: String) -> Result<Report> {
    let inp = Inputs::local(state_dir);
    let now = Utc::now();
    alerts::check_since(since, now).map_err(anyhow::Error::msg)?;
    let items = alerts::gather(&inp, since, Vec::new(), now).with_context(|| {
        format!(
            "Could not read the logs in {}. Run this as the user that runs the daemon (for example with sudo), or point --server at the daemon",
            state_dir.display()
        )
    })?;
    let mut notes = vec![why];
    notes.extend(alerts::notes(&inp));
    Ok(Report::new(since, items, all, notes))
}

/// The list, and whether it came from the logs only.
async fn report(at: Where<'_>, since: &str, all: bool) -> Result<(Report, bool)> {
    let path = format!(
        "/v1/alerts?since={}{}",
        remote::encode_query(since),
        if all { "&include_snoozed=1" } else { "" }
    );
    let (daemon, dir) = match at {
        Where::Server(r) => (r, None),
        Where::ThisMachine(Some(r), dir) => (r, Some(dir)),
        Where::ThisMachine(None, dir) => {
            let why = format!(
                "--state-dir was given, so no daemon was asked and this list is from the logs in {} only. Add --listen HOST:PORT to include unhealthy Mac instances and machine health.",
                dir.display()
            );
            return Ok((local_report(dir, since, all, why)?, true));
        }
    };
    match ask(daemon, Method::GET, &path, None).await {
        Ok(v) => Ok((
            serde_json::from_value(v)
                .context("the daemon's answer was not a list of alerts: upgrade it and fluxctl to the same version")?,
            false,
        )),
        Err(f) => match dir {
            // A bad `--since` (400) and a token that may not see the host (403) are answers, not outages.
            Some(dir) if !matches!(f.status, Some(400 | 403)) => {
                let why = why_logs_only(daemon, &f);
                Ok((local_report(dir, since, all, why)?, true))
            }
            _ => Err(refused(daemon, f)),
        },
    }
}

/// Snooze (`hours`) or unsnooze one key: through the daemon, or in the state directory when no daemon is running.
async fn change(at: Where<'_>, key: &str, hours: Option<u32>) -> Result<Value> {
    let local = |dir: &Path| -> Result<Value> {
        Ok(match hours {
            Some(h) => json!({"snoozed_until": alerts::snooze(dir, key, h, Utc::now())?}),
            None => json!({"was_snoozed": alerts::unsnooze(dir, key, Utc::now())?}),
        })
    };
    let (daemon, dir) = match at {
        Where::Server(r) => (r, None),
        Where::ThisMachine(Some(r), dir) => (r, Some(dir)),
        Where::ThisMachine(None, dir) => return local(dir),
    };
    let (path, body) = match hours {
        Some(h) => ("/v1/alerts/snooze", json!({"key": key, "hours": h})),
        None => ("/v1/alerts/unsnooze", json!({"key": key})),
    };
    match ask(daemon, Method::POST, path, Some(body)).await {
        Ok(v) => Ok(v),
        Err(f) => match dir {
            Some(dir) if f.unreachable => local(dir),
            _ => Err(refused(daemon, f)),
        },
    }
}

/// A key as it can be pasted into a shell.
fn quoted(key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:/,@=+-".contains(c));
    if plain {
        key.to_string()
    } else {
        format!("'{}'", key.replace('\'', "'\\''"))
    }
}

/// Log text must not move the cursor or colour the terminal.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `last 24h` for a length of time, `since X` for a point in time.
fn window(since: &str) -> String {
    let s = since.trim();
    let length = s.len() > 1
        && s.ends_with(['s', 'm', 'h', 'd', 'w'])
        && s[..s.len() - 1].chars().all(|c| c.is_ascii_digit());
    if length {
        format!("last {s}")
    } else {
        format!("since {s}")
    }
}

/// The list as text: a headline, then one block per alert, then the notes.
pub fn render(r: &Report, logs_only: bool) -> String {
    let mut out = String::new();
    let total = r.errors + r.warnings;
    if total == 0 {
        out.push_str(&format!(
            "Nothing {}needs attention ({}).",
            if logs_only { "in the logs " } else { "" },
            window(&r.since)
        ));
        if r.snoozed > 0 {
            out.push_str(&format!(
                " {} snoozed: `fluxctl alerts --all` lists them.",
                r.snoozed
            ));
        }
        out.push('\n');
    } else {
        let mut parts = Vec::new();
        if r.errors > 0 {
            parts.push(plural(r.errors, "error"));
        }
        if r.warnings > 0 {
            parts.push(plural(r.warnings, "warning"));
        }
        out.push_str(&format!(
            "{} {} attention ({}){}\n",
            parts.join(" and "),
            if total == 1 { "needs" } else { "need" },
            window(&r.since),
            if r.snoozed > 0 {
                format!(", {} snoozed", r.snoozed)
            } else {
                String::new()
            }
        ));
    }
    for a in &r.items {
        out.push('\n');
        out.push_str(&block(a));
    }
    if !r.notes.is_empty() {
        out.push('\n');
    }
    for n in &r.notes {
        out.push_str(&format!("note: {}\n", clean(n)));
    }
    out
}

fn block(a: &Alert) -> String {
    let when = a.last_seen.format("%Y-%m-%d %H:%M UTC");
    let times = if a.count > 1 {
        format!(" (x{}, last {when})", a.count)
    } else {
        format!(" ({when})")
    };
    let mut s = format!(
        "{:<5} {}{times}\n      next: {}\n",
        a.level.to_uppercase(),
        clean(&a.summary),
        clean(&a.next_step)
    );
    if let Some(c) = &a.explain {
        s.push_str(&format!("      more: fluxctl explain {c}\n"));
    }
    s.push_str(&format!("      key:  {}", quoted(&a.key)));
    if let Some(u) = a.snoozed_until {
        s.push_str(&format!(
            "   (snoozed until {})",
            u.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    s.push('\n');
    s
}

/// Is `key` on the list at all? `None` when that cannot be told.
async fn listed(at: Where<'_>, key: &str) -> Option<bool> {
    let (r, _) = report(at, "30d", true).await.ok()?;
    Some(r.items.iter().any(|a| a.key == key))
}

async fn act(a: &AlertsArgs, at: Where<'_>) -> Result<i32> {
    match &a.command {
        Some(AlertsCommand::Snooze { key, hours }) => {
            let v = change(at, key, Some(*hours)).await?;
            let until = v["snoozed_until"].as_str().unwrap_or_default().to_string();
            if a.json {
                println!("{}", json!({"key": key, "snoozed_until": until}));
            } else {
                let k = quoted(key);
                println!(
                    "Snoozed {k} until {until}. `fluxctl alerts unsnooze {k}` brings it back."
                );
            }
            if listed(at, key).await == Some(false) {
                eprintln!(
                    "note: no alert has that key right now. If you meant one on the list, copy its key from `fluxctl alerts`."
                );
            }
            Ok(0)
        }
        Some(AlertsCommand::Unsnooze { key }) => {
            let v = change(at, key, None).await?;
            let was = v["was_snoozed"].as_bool().unwrap_or(false);
            if a.json {
                println!("{}", json!({"key": key, "was_snoozed": was}));
            } else if was {
                println!("{} is back on the list.", quoted(key));
            } else {
                println!(
                    "{} was not snoozed. `fluxctl alerts --all` lists the snoozed ones.",
                    quoted(key)
                );
            }
            Ok(0)
        }
        None => {
            let (report, logs_only) = report(at, &a.since, a.all).await?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render(&report, logs_only));
            }
            Ok(i32::from(report.errors > 0))
        }
    }
}

/// Runs the command and returns the exit code: 0, 1 (something is an error) or 2 (it could not be done; said on stderr).
pub async fn run(a: AlertsArgs, at: Where<'_>) -> i32 {
    match act(&a, at).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:#}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn alert(level: &str, key: &str, explain: Option<&str>) -> Alert {
        let t = Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 0).unwrap();
        Alert {
            key: key.into(),
            level: level.into(),
            source: "backup".into(),
            summary: "Backup bk-1 did not finish.".into(),
            next_step: "Run `fluxctl mac backup ls`.".into(),
            explain: explain.map(str::to_string),
            target: None,
            last_seen: t,
            first_seen: t,
            count: 3,
            snoozed_until: None,
        }
    }

    #[test]
    fn the_text_has_a_headline_the_next_step_the_explain_code_and_the_key() {
        let mut r = Report::new(
            "24h",
            vec![
                alert("error", "k1", Some("MAC_BACKUP_IO")),
                alert("warn", "k2", None),
            ],
            false,
            vec![],
        );
        let t = render(&r, false);
        assert!(
            t.starts_with("1 error and 1 warning need attention (last 24h)"),
            "{t}"
        );
        assert!(t.contains("next: Run `fluxctl mac backup ls`."));
        assert!(t.contains("more: fluxctl explain MAC_BACKUP_IO"));
        assert!(t.contains("key:  k1") && t.contains("(x3, last 2026-11-24 09:30 UTC)"));
        assert!(!t.contains("note:"));
        r.notes.push("No daemon answered.".into());
        assert!(render(&r, true).ends_with("\nnote: No daemon answered.\n"));
    }

    #[test]
    fn one_alert_needs_and_a_timestamp_is_since() {
        let r = Report::new(
            "2026-11-20T00:00:00Z",
            vec![alert("error", "a b", None)],
            false,
            vec![],
        );
        let t = render(&r, false);
        assert!(
            t.starts_with("1 error needs attention (since 2026-11-20T00:00:00Z)"),
            "{t}"
        );
        assert!(t.contains("key:  'a b'"), "{t}");
    }

    #[test]
    fn log_text_cannot_drive_the_terminal() {
        let mut a = alert("error", "k", None);
        a.summary = "bad\u{1b}[2J\nname".into();
        assert!(!block(&a).contains('\u{1b}'));
        assert!(block(&a).contains("bad [2J name"));
    }

    #[test]
    fn nothing_to_report_says_so_and_mentions_snoozed_ones() {
        let mut s = alert("error", "k", None);
        s.snoozed_until = Some(Utc.with_ymd_and_hms(2026, 11, 25, 9, 0, 0).unwrap());
        let r = Report::new("24h", vec![s], false, vec![]);
        assert_eq!(r.errors, 0);
        let t = render(&r, false);
        assert!(
            t.starts_with("Nothing needs attention (last 24h). 1 snoozed"),
            "{t}"
        );
        let empty = Report::new("7d", vec![], false, vec![]);
        assert!(render(&empty, true).starts_with("Nothing in the logs needs attention (last 7d)."));
    }

    #[test]
    fn every_explain_code_an_alert_can_carry_is_one_explain_knows() {
        for code in [
            "MAC_BACKUP_IO",
            "MAC_BACKUP_NO_ROOM",
            "MAC_BACKUP_CHECKSUM",
            "MAC_UNHEALTHY",
            "EVENTS_BAD_TIME",
        ] {
            let (_, status) = crate::explain::render(&crate::explain::ExplainArgs {
                code: Some(code.into()),
                list: false,
                json: false,
            })
            .unwrap();
            assert_eq!(status, 0, "{code}");
        }
    }

    fn one_failed_run() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let ts = Utc::now().to_rfc3339();
        let line =
            json!({"ts": ts, "event": "mac.run", "fields": {"result": "failed", "name": "build"}});
        std::fs::write(dir.path().join("events.jsonl"), format!("{line}\n")).unwrap();
        dir
    }

    fn list_args() -> AlertsArgs {
        AlertsArgs {
            since: "24h".into(),
            all: false,
            json: true,
            command: None,
        }
    }

    fn snooze_args(key: &str) -> AlertsArgs {
        AlertsArgs {
            command: Some(AlertsCommand::Snooze {
                key: key.into(),
                hours: 24,
            }),
            ..list_args()
        }
    }

    #[tokio::test]
    async fn local_mode_reads_the_logs_exits_one_on_an_error_and_snoozing_clears_it() {
        let dir = one_failed_run();
        let at = Where::ThisMachine(None, dir.path());
        assert_eq!(run(list_args(), at).await, 1);
        let (r, logs_only) = report(at, "24h", false).await.unwrap();
        assert!(logs_only);
        assert_eq!(r.errors, 1);
        assert!(r.notes[0].contains("--listen"), "{:?}", r.notes);
        let key = r.items[0].key.clone();
        assert_eq!(run(snooze_args(&key), at).await, 0);
        assert_eq!(run(list_args(), at).await, 0);
        // a bad time or a bad snooze is "could not", not "something is wrong"
        let bad = AlertsArgs {
            since: "yesterday".into(),
            ..list_args()
        };
        assert_eq!(run(bad, at).await, 2);
        assert_eq!(run(snooze_args(""), at).await, 2);
    }

    /// A daemon that answers every request with `status` and `body`.
    async fn fake(status: &'static str, body: &'static str) -> Remote {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            }
        });
        Remote::new(&addr.to_string(), None)
    }

    /// An address nothing listens on.
    fn nobody() -> Remote {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Remote::new(&format!("127.0.0.1:{port}"), None)
    }

    #[tokio::test]
    async fn with_no_daemon_the_local_list_says_so_and_a_named_server_is_an_error() {
        let dir = one_failed_run();
        let r = nobody();
        let (got, logs_only) = report(Where::ThisMachine(Some(&r), dir.path()), "24h", false)
            .await
            .unwrap();
        assert!(logs_only && got.errors == 1);
        assert!(
            got.notes[0].contains("No daemon answered"),
            "{:?}",
            got.notes
        );
        let e = report(Where::Server(&r), "24h", false).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("Could not reach the daemon"),
            "{e:#}"
        );
        assert_eq!(run(list_args(), Where::Server(&r)).await, 2);
        // snoozing works on this machine without a daemon
        let at = Where::ThisMachine(Some(&r), dir.path());
        assert_eq!(run(snooze_args("mac.run:build"), at).await, 0);
        assert_eq!(run(list_args(), at).await, 0);
    }

    #[tokio::test]
    async fn a_daemon_that_wants_a_token_gives_the_logs_with_the_next_step_but_never_a_snooze() {
        let dir = one_failed_run();
        let r = fake(
            "401 Unauthorized",
            r#"{"error":"missing or invalid bearer token"}"#,
        )
        .await;
        let at = Where::ThisMachine(Some(&r), dir.path());
        let (got, logs_only) = report(at, "24h", false).await.unwrap();
        assert!(logs_only);
        assert!(got.notes[0].contains("FLUXVM_TOKEN"), "{:?}", got.notes);
        let e = change(at, "mac.run:build", Some(1)).await.unwrap_err();
        assert!(format!("{e:#}").contains("FLUXVM_TOKEN"), "{e:#}");
        assert!(!dir.path().join("alert-snoozes.json").exists());
    }

    #[tokio::test]
    async fn a_refused_tenant_token_is_not_worked_around_with_the_logs() {
        let dir = one_failed_run();
        let r = fake(
            "403 Forbidden",
            r#"{"error":"Alerts cover the whole host"}"#,
        )
        .await;
        let e = report(Where::ThisMachine(Some(&r), dir.path()), "24h", false)
            .await
            .unwrap_err();
        let text = format!("{e:#}");
        assert!(
            text.contains("whole host") && text.contains("admin"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn an_older_daemon_gives_the_logs_and_says_to_restart_it() {
        let dir = one_failed_run();
        let r = fake("404 Not Found", "").await;
        let (got, _) = report(Where::ThisMachine(Some(&r), dir.path()), "24h", false)
            .await
            .unwrap();
        assert!(
            got.notes[0].contains("older than this fluxctl"),
            "{:?}",
            got.notes
        );
    }
}
