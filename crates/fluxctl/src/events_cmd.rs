// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl events`: one chronological feed of what happened, merged from the logs FluxVM already keeps (VM lifecycle, Mac
//! instances, Kubernetes, containers, backup and schedule runs, and the audit log). The merging lives in
//! `fluxvm_scheduler::event_feed`, so this, `GET /v1/events?source=...` and the Studio Events page show the same lines.
//!
//! Against a daemon (`--server`, a context) it asks `GET /v1/events` and follows `GET /v1/events/stream`; without one it
//! reads the state directory directly. Local reads show containers as VMs (only a daemon knows which VMs are containers).

use crate::mac_errors::coded;
use crate::remote::{self, Remote};
use anyhow::Result;
use chrono::Utc;
use fluxvm_scheduler::event_feed::{self, Cursor, Feed, FeedQuery, Inputs};
use reqwest::Method;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

#[derive(clap::Args, Debug, Clone, Default)]
#[command(
    after_help = "Sources: vm, mac, kube, docker, backup, audit (comma list; default: all).\n\
Examples:\n  fluxctl events --since 1h\n  fluxctl events --follow --source mac,kube\n  fluxctl events --level error --json"
)]
pub struct EventsArgs {
    /// Only events for this VM (id, name, or id prefix).
    #[arg(long, value_parser = crate::output::parse_vm_ref)]
    pub vm: Option<Uuid>,
    /// Event-name prefix, e.g. `vm.` or `mac.run`.
    #[arg(long)]
    pub event: Option<String>,
    /// Only events after this: how far back (`30m`, `2h`, `7d`) or an RFC 3339 timestamp.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    /// Only events up to this (same forms as --since).
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,
    /// Which sources to merge: any of vm, mac, kube, docker, backup, audit (comma separated). Default: all of them.
    #[arg(long, value_name = "LIST")]
    pub source: Option<String>,
    /// Only this level and worse: info, warn or error.
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,
    /// Newest N events (default 100).
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
    /// Keep printing new events as they happen (Ctrl-C to stop).
    #[arg(long, short = 'f', default_value_t = false)]
    pub follow: bool,
    /// One JSON object per line (with --follow), or a JSON list (without).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl EventsArgs {
    fn feed_query(&self) -> FeedQuery {
        FeedQuery {
            source: Some(self.source.clone().unwrap_or_else(|| "all".into())),
            since: self.since.clone(),
            until: self.until.clone(),
            event: self.event.clone(),
            vm: self.vm,
            level: self.level.clone(),
            limit: Some(self.limit),
        }
    }

    /// The query string for the daemon: only what is set. `source` is always sent, as it is what asks for the merged feed.
    pub fn query_string(&self, with_limit: bool) -> String {
        let q = self.feed_query();
        let mut p = vec![format!(
            "source={}",
            remote::encode_query(q.source.as_deref().unwrap_or("all"))
        )];
        for (k, v) in [
            ("since", &q.since),
            ("until", &q.until),
            ("event", &q.event),
            ("level", &q.level),
        ] {
            if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                p.push(format!("{k}={}", remote::encode_query(v)));
            }
        }
        if let Some(vm) = q.vm {
            p.push(format!("vm={vm}"));
        }
        if with_limit {
            p.push(format!("limit={}", self.limit));
        }
        p.join("&")
    }
}

pub enum Where<'a> {
    Remote(&'a Remote),
    Local(&'a Path),
}

/// What to say when nothing matches, and what to try.
pub fn empty_words(a: &EventsArgs) -> String {
    let mut narrowed = Vec::new();
    for (name, v) in [
        ("--source", &a.source),
        ("--since", &a.since),
        ("--level", &a.level),
        ("--event", &a.event),
    ] {
        if v.as_deref().is_some_and(|v| !v.is_empty()) {
            narrowed.push(name);
        }
    }
    if a.vm.is_some() {
        narrowed.push("--vm");
    }
    if narrowed.is_empty() {
        "No events recorded yet. They appear as VMs start and stop and as changes are made; `fluxctl events --follow` waits for the next one.".into()
    } else {
        format!(
            "Nothing matches {}. Loosen it (for example --since 7d, or leave --source out) and try again.",
            narrowed.join(" and ")
        )
    }
}

/// One line for the terminal: `2026-11-24 10:00:00Z  vm      WARN   VM web stopped`.
pub fn format_line(ev: &Value) -> String {
    let ts = ev["ts"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| {
            t.with_timezone(&Utc)
                .format("%Y-%m-%d %H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_else(|| ev["ts"].as_str().unwrap_or("-").to_string());
    let source = ev["source"].as_str().unwrap_or("vm");
    let level = match ev["level"].as_str() {
        Some("error") => "ERROR",
        Some("warn") => "WARN",
        _ => "",
    };
    let summary = match ev["summary"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        // A daemon from before the merged feed: the raw event and its fields.
        None => {
            let fields = ev["fields"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{} {fields}", ev["event"].as_str().unwrap_or("-"))
                .trim()
                .to_string()
        }
    };
    format!("{ts}  {source:<7} {level:<5}  {summary}")
}

fn print_one(json: bool, ev: &Value) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(ev)?);
    } else {
        println!("{}", format_line(ev));
    }
    Ok(())
}

fn print_backlog(a: &EventsArgs, json: bool, items: &[Value]) -> Result<()> {
    if json && !a.follow {
        println!("{}", serde_json::to_string_pretty(items)?);
    } else if items.is_empty() {
        if !json {
            println!("{}", empty_words(a));
        }
    } else {
        for ev in items {
            print_one(json, ev)?;
        }
    }
    Ok(())
}

fn lost(what: &str) -> anyhow::Error {
    coded(
        format!(
            "The live feed stopped ({what}). Run the same command again to keep watching; `--since 5m` shows what you missed."
        ),
        "EVENTS_STREAM_LOST",
    )
}

pub async fn run(a: EventsArgs, at: Where<'_>, format: crate::output::OutputFormat) -> Result<()> {
    let json = a.json || format == crate::output::OutputFormat::Json;
    match at {
        Where::Remote(r) => {
            let v = r
                .call(
                    Method::GET,
                    &format!("/v1/events?{}", a.query_string(true)),
                    None,
                )
                .await?;
            let items = v["items"].as_array().cloned().unwrap_or_default();
            print_backlog(&a, json, &items)?;
            if a.follow {
                r.follow_sse(
                    &format!("/v1/events/stream?{}", a.query_string(false)),
                    |kind, data| {
                        if kind == "error" {
                            let msg = serde_json::from_str::<Value>(data)
                                .ok()
                                .and_then(|v| v["error"].as_str().map(str::to_owned))
                                .unwrap_or_else(|| data.to_string());
                            return Err(anyhow::anyhow!("{msg}"));
                        }
                        match serde_json::from_str::<Value>(data) {
                            Ok(ev) => print_one(json, &ev),
                            Err(_) => Ok(()),
                        }
                    },
                )
                .await
                .map_err(|e| lost(&format!("{e:#}")))?;
                return Err(lost("the daemon closed the connection"));
            }
            Ok(())
        }
        Where::Local(state_dir) => {
            let feed =
                Feed::parse(&a.feed_query(), Utc::now()).map_err(|e| anyhow::anyhow!("{e}"))?;
            let inp = Inputs::local(state_dir);
            let mut cursor = a.follow.then(|| Cursor::at_end(&inp, Utc::now()));
            let items: Vec<Value> = event_feed::collect(&inp, &feed, Utc::now())?
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?;
            print_backlog(&a, json, &items)?;
            let Some(cur) = cursor.as_mut() else {
                return Ok(());
            };
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let fresh = event_feed::poll(&inp, &feed, cur, Utc::now()).map_err(|e| {
                    coded(
                        format!("The event logs could not be read ({e:#}). Check that the state directory is readable, then run this again."),
                        "EVENTS_UNREADABLE",
                    )
                })?;
                for ev in fresh {
                    print_one(json, &serde_json::to_value(&ev)?)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_daemon_query_names_the_source_and_only_what_is_set() {
        let a = EventsArgs {
            limit: 50,
            ..Default::default()
        };
        assert_eq!(a.query_string(true), "source=all&limit=50");
        assert_eq!(a.query_string(false), "source=all");
        let a = EventsArgs {
            source: Some("mac,kube".into()),
            since: Some("2h".into()),
            level: Some("warn".into()),
            event: Some("mac.run x".into()),
            limit: 5,
            ..Default::default()
        };
        let q = a.query_string(true);
        assert!(
            q.starts_with("source=mac%2Ckube") || q.starts_with("source=mac,kube"),
            "{q}"
        );
        assert!(q.contains("since=2h") && q.contains("level=warn") && q.contains("limit=5"));
        assert!(!q.contains("until") && !q.contains("vm="));
        assert!(!q.contains("mac.run x"), "spaces are encoded: {q}");
    }

    #[test]
    fn a_line_has_time_source_level_and_words() {
        let ev = json!({"ts": "2026-11-24T10:00:00Z", "source": "vm", "level": "warn", "summary": "VM web stopped", "event": "vm.stop"});
        assert_eq!(
            format_line(&ev),
            "2026-11-24 10:00:00Z  vm      WARN   VM web stopped"
        );
        let ev = json!({"ts": "2026-11-24T10:00:00Z", "source": "audit", "level": "info", "summary": "sam ran mac.run on ci: worked"});
        assert_eq!(
            format_line(&ev),
            format!(
                "2026-11-24 10:00:00Z  {:<7} {:<5}  sam ran mac.run on ci: worked",
                "audit", ""
            )
        );
    }

    #[test]
    fn an_older_daemon_reply_still_prints() {
        let ev =
            json!({"ts": "2026-11-24T10:00:00Z", "event": "vm.start", "fields": {"name": "web"}});
        assert_eq!(
            format_line(&ev),
            format!(
                "2026-11-24 10:00:00Z  {:<7} {:<5}  vm.start name=web",
                "vm", ""
            )
        );
    }

    #[test]
    fn nothing_found_says_what_to_try() {
        assert!(empty_words(&EventsArgs::default()).contains("--follow"));
        let a = EventsArgs {
            source: Some("audit".into()),
            since: Some("1h".into()),
            ..Default::default()
        };
        let w = empty_words(&a);
        assert!(
            w.contains("--source and --since") && w.contains("--since 7d"),
            "{w}"
        );
    }

    #[test]
    fn a_lost_feed_points_at_the_explanation() {
        let e = lost("connection reset").to_string();
        assert!(
            e.contains("connection reset") && e.ends_with("fluxctl explain EVENTS_STREAM_LOST")
        );
    }

    #[tokio::test]
    async fn a_local_read_with_a_wrong_source_fails_in_plain_words() {
        let d = tempfile::tempdir().unwrap();
        let a = EventsArgs {
            source: Some("nope".into()),
            limit: 10,
            ..Default::default()
        };
        let e = run(
            a,
            Where::Local(d.path()),
            crate::output::OutputFormat::Table,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("not an event source") && e.contains("fluxctl explain EVENTS_BAD_SOURCE"),
            "{e}"
        );
    }

    #[test]
    fn every_events_code_is_explained_and_found_from_the_message_that_produces_it() {
        let src = Feed::parse(
            &FeedQuery {
                source: Some("nope".into()),
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap_err();
        let time = event_feed::parse_when("soon", Utc::now()).unwrap_err();
        let level = Feed::parse(
            &FeedQuery {
                level: Some("loud".into()),
                ..Default::default()
            },
            Utc::now(),
        )
        .unwrap_err();
        let msgs = [
            (src.to_string(), "EVENTS_BAD_SOURCE"),
            (time.to_string(), "EVENTS_BAD_TIME"),
            (level.to_string(), "EVENTS_BAD_LEVEL"),
            (lost("x").to_string(), "EVENTS_STREAM_LOST"),
            (
                "`audit` shows who changed what, so it needs an admin token. Use an admin token"
                    .to_string(),
                "EVENTS_NOT_ALLOWED",
            ),
            (
                "The event logs could not be read (denied). Check".to_string(),
                "EVENTS_UNREADABLE",
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (m, code) in &msgs {
            assert_eq!(
                crate::mac_errors::hint_for(m).map(|c| c.code),
                Some(*code),
                "{m}"
            );
            assert_eq!(crate::mac_errors::find(code).len(), 1, "{code}");
            seen.insert(*code);
        }
        for c in crate::mac_errors::CODES
            .iter()
            .filter(|c| c.code.starts_with("EVENTS_"))
        {
            assert!(seen.contains(c.code), "{} is never produced", c.code);
        }
    }
}
