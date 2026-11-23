// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl audit ls` and `fluxctl audit export`: read the audit log (what was changed, by whom, when, and how it
//! ended). Reads `<state_dir>/audit.jsonl` directly, or `GET /v1/audit` when `--server` (or a context) points at a
//! daemon. See `fluxvm_scheduler::audit` for what is recorded and how secrets are kept out.
//!
//! The commands that change something (`mac run`, `mac rm`, `mac exec`, `kube ...`, `mac-fleet ...`, backups, image
//! pulls) record themselves when they finish (`record_cli`); the daemon records every change made through its API.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use clap::{Args, Subcommand};
use fluxvm_scheduler::audit::{self, AuditLog, Entry, Outcome, Page, Query, Record};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum AuditCommand {
    /// List what was changed, newest first: who did what to which thing, when, and whether it worked.
    ///
    /// Examples: `fluxctl audit ls --since 24h`, `fluxctl audit ls --action mac. --target web`,
    /// `fluxctl audit ls --result denied --json`. REST: `GET /v1/audit`.
    Ls {
        #[command(flatten)]
        filter: Filters,
        /// Records to show (default 100, at most 1000).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Records to skip, for the next page (the output says what to pass).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Print the page as JSON (`items`, `total`, `next_offset`, `notice`).
        #[arg(long)]
        json: bool,
    },
    /// Write every matching record, oldest first, to a file or stdout, to keep or to hand to someone.
    Export {
        #[command(flatten)]
        filter: Filters,
        /// Write to this file (mode 0600) instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// `jsonl` (default, one record per line) or `csv`.
        #[arg(long, default_value = "jsonl")]
        format: String,
    },
}

#[derive(Args, Debug, Default, Clone)]
pub struct Filters {
    /// Only records after this: `30m`, `24h`, `7d`, a date (`2026-11-01`) or an RFC 3339 time.
    #[arg(long)]
    pub since: Option<String>,
    /// Only records up to this time (same forms as --since).
    #[arg(long)]
    pub until: Option<String>,
    /// Action prefix: `mac.` for every Mac change, `mac.run` for one.
    #[arg(long)]
    pub action: Option<String>,
    /// Part of the target's name (not case sensitive).
    #[arg(long)]
    pub target: Option<String>,
    /// Who did it.
    #[arg(long)]
    pub actor: Option<String>,
    /// `ok`, `failed` or `denied`.
    #[arg(long)]
    pub result: Option<String>,
}

/// `30m`, `24h`, `7d`, `2026-11-01` or an RFC 3339 time, relative to `now`.
pub fn parse_when(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Some(unit) = s
        .chars()
        .last()
        .filter(|c| matches!(c, 'm' | 'h' | 'd' | 'w'))
        && let Ok(n) = s[..s.len() - 1].parse::<i64>()
        && n >= 0
    {
        let d = match unit {
            'm' => Duration::minutes(n),
            'h' => Duration::hours(n),
            'd' => Duration::days(n),
            _ => Duration::weeks(n),
        };
        return Ok(now - d);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    }
    bail!(
        "`{s}` is not a time I understand. Use 30m, 24h, 7d, a date like 2026-11-01, or an RFC 3339 time like 2026-11-01T09:00:00Z"
    )
}

impl Filters {
    pub fn to_query(&self, now: DateTime<Utc>) -> Result<Query> {
        if let Some(r) = &self.result
            && !matches!(r.to_ascii_lowercase().as_str(), "ok" | "failed" | "denied")
        {
            bail!("--result takes ok, failed or denied, not `{r}`");
        }
        Ok(Query {
            since: self
                .since
                .as_deref()
                .map(|s| parse_when(s, now))
                .transpose()?,
            until: self
                .until
                .as_deref()
                .map(|s| parse_when(s, now))
                .transpose()?,
            action: self.action.clone(),
            target: self.target.clone(),
            actor: self.actor.clone(),
            result: self.result.clone(),
            limit: None,
            offset: None,
        })
    }
}

/// Where the records are read from.
pub enum Source<'a> {
    Local(&'a Path),
    Remote(&'a crate::remote::Remote),
}

impl Source<'_> {
    async fn page(&self, q: &Query) -> Result<Page> {
        match self {
            Source::Local(dir) => AuditLog::new(dir).query(q).with_context(|| {
                format!(
                    "could not read the audit log in {}. Point --state-dir at the daemon's state directory",
                    dir.display()
                )
            }),
            Source::Remote(r) => {
                let v = r
                    .call(reqwest::Method::GET, &format!("/v1/audit?{}", query_string(q)), None)
                    .await
                    .context("could not read the audit log from the daemon. Reading it needs an admin token (--server-token), and a daemon that has the audit log (this release or newer)")?;
                serde_json::from_value(v).context("the daemon's audit reply was not in the expected shape")
            }
        }
    }
}

fn query_string(q: &Query) -> String {
    let enc = crate::remote::encode_query;
    let mut p: Vec<String> = Vec::new();
    if let Some(t) = q.since {
        p.push(format!("since={}", enc(&t.to_rfc3339())));
    }
    if let Some(t) = q.until {
        p.push(format!("until={}", enc(&t.to_rfc3339())));
    }
    for (k, v) in [
        ("action", &q.action),
        ("target", &q.target),
        ("actor", &q.actor),
        ("result", &q.result),
    ] {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            p.push(format!("{k}={}", enc(v)));
        }
    }
    if let Some(l) = q.limit {
        p.push(format!("limit={l}"));
    }
    if let Some(o) = q.offset {
        p.push(format!("offset={o}"));
    }
    p.join("&")
}

pub async fn run(cmd: AuditCommand, src: Source<'_>) -> Result<()> {
    let now = Utc::now();
    match cmd {
        AuditCommand::Ls {
            filter,
            limit,
            offset,
            json,
        } => {
            let mut q = filter.to_query(now)?;
            q.limit = Some(limit);
            q.offset = Some(offset);
            let page = src.page(&q).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&page)?);
            } else {
                print!("{}", render(&page));
            }
            if let Some(n) = &page.notice {
                eprintln!("{n}");
            }
            Ok(())
        }
        AuditCommand::Export {
            filter,
            out,
            format,
        } => {
            let csv = match format.as_str() {
                "jsonl" | "json" => false,
                "csv" => true,
                other => bail!("--format takes jsonl or csv, not `{other}`"),
            };
            let q = filter.to_query(now)?;
            let (records, notice) = collect_all(&src, q).await?;
            let body = if csv {
                to_csv(&records)
            } else {
                to_jsonl(&records)?
            };
            match out {
                Some(path) => {
                    write_private(&path, body.as_bytes()).with_context(|| {
                        format!(
                            "could not write {}. Choose a folder you can write to",
                            path.display()
                        )
                    })?;
                    eprintln!(
                        "Wrote {} audit records to {}.",
                        records.len(),
                        path.display()
                    );
                }
                None => std::io::stdout().write_all(body.as_bytes())?,
            }
            if let Some(n) = notice {
                eprintln!("{n}");
            }
            Ok(())
        }
    }
}

/// Every matching record, oldest first, by paging.
async fn collect_all(src: &Source<'_>, mut q: Query) -> Result<(Vec<Record>, Option<String>)> {
    q.limit = Some(audit::MAX_LIMIT);
    let mut offset = 0;
    let mut all = Vec::new();
    let mut notice = None;
    loop {
        q.offset = Some(offset);
        let page = src.page(&q).await?;
        notice = page.notice.clone().or(notice);
        all.extend(page.items);
        match page.next_offset {
            Some(n) if n > offset => offset = n,
            _ => break,
        }
    }
    all.reverse();
    Ok((all, notice))
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(data)?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, data)?;
    Ok(())
}

pub fn to_jsonl(records: &[Record]) -> Result<String> {
    let mut s = String::new();
    for r in records {
        s.push_str(&serde_json::to_string(r)?);
        s.push('\n');
    }
    Ok(s)
}

fn csv_cell(v: &str) -> String {
    // Quote when needed, and defuse spreadsheet formulas.
    let v = if v.starts_with(['=', '+', '-', '@']) {
        format!("'{v}")
    } else {
        v.to_string()
    };
    if v.contains([',', '"', '\n']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

pub fn to_csv(records: &[Record]) -> String {
    let mut s = String::from("time,action,actor,target,result,source,detail\n");
    for r in records {
        let detail = r
            .detail
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let cells = [
            r.ts.to_rfc3339(),
            r.action.clone(),
            r.actor.clone(),
            r.target.clone(),
            r.result.clone(),
            r.source.clone(),
            detail,
        ];
        s.push_str(
            &cells
                .iter()
                .map(|c| csv_cell(c))
                .collect::<Vec<_>>()
                .join(","),
        );
        s.push('\n');
    }
    s
}

pub fn render(page: &Page) -> String {
    if page.items.is_empty() {
        return if page.total == 0 {
            "No audit records match. Changes made with fluxctl (and through the daemon's API) are recorded as they happen.\n".into()
        } else {
            format!(
                "No records on this page: there are {} in all, so use a smaller --offset.\n",
                page.total
            )
        };
    }
    let rows: Vec<[String; 6]> = page
        .items
        .iter()
        .map(|r| {
            [
                r.ts.format("%Y-%m-%d %H:%M:%S").to_string(),
                r.action.clone(),
                r.target.clone(),
                r.actor.clone(),
                r.result.clone(),
                r.detail.get("error").cloned().unwrap_or_default(),
            ]
        })
        .collect();
    let head = ["TIME (UTC)", "ACTION", "TARGET", "WHO", "RESULT", "NOTE"];
    let mut w = head.map(str::len);
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count().min(40));
        }
    }
    let mut out = String::new();
    let line = |cells: &[String; 6]| {
        let mut l = String::new();
        for (i, c) in cells.iter().enumerate() {
            let c: String = c.chars().take(40).collect();
            l.push_str(&format!("{c:<width$}  ", width = w[i]));
        }
        l.trim_end().to_string() + "\n"
    };
    out.push_str(&line(&head.map(String::from)));
    for r in &rows {
        out.push_str(&line(r));
    }
    out.push_str(&format!(
        "\n{} of {} records.",
        page.items.len(),
        page.total
    ));
    if let Some(n) = page.next_offset {
        out.push_str(&format!(" More: add --offset {n}."));
    }
    out.push('\n');
    out
}

/// Called when a changing `fluxctl` command ends: writes one record to `<state_dir>/audit.jsonl`. Never fails the
/// command; a log that cannot be written is only a warning.
pub fn record_cli(state_dir: &Path, action: &audit::CliAction, result: &Result<()>) {
    let (outcome, error) = match result {
        Ok(()) => (Outcome::Ok, None),
        Err(e) => {
            let msg = format!("{e:#}");
            let denied = {
                let l = msg.to_lowercase();
                l.contains("denied")
                    || l.contains("forbidden")
                    || l.contains("not allowed")
                    || l.contains("policy")
            };
            (
                if denied {
                    Outcome::Denied
                } else {
                    Outcome::Failed
                },
                Some(msg),
            )
        }
    };
    let entry: Entry = audit::cli_entry(action, outcome, error.as_deref());
    if let Err(e) = AuditLog::new(state_dir).append(&entry) {
        eprintln!(
            "warning: could not write the audit record ({e:#}). Check that {} is writable.",
            state_dir.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-23T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn parses_times() {
        assert_eq!(
            parse_when("24h", now()).unwrap(),
            now() - Duration::hours(24)
        );
        assert_eq!(parse_when("7d", now()).unwrap(), now() - Duration::days(7));
        assert_eq!(
            parse_when("2026-11-01", now()).unwrap().to_rfc3339(),
            "2026-11-01T00:00:00+00:00"
        );
        assert_eq!(
            parse_when("2026-11-01T09:00:00Z", now())
                .unwrap()
                .to_rfc3339(),
            "2026-11-01T09:00:00+00:00"
        );
        let e = parse_when("yesterday", now()).unwrap_err().to_string();
        assert!(e.contains("Use 30m, 24h, 7d"), "{e}");
        assert!(
            Filters {
                result: Some("bad".into()),
                ..Default::default()
            }
            .to_query(now())
            .is_err()
        );
    }

    #[test]
    fn ls_and_export_round_trip_through_the_files() {
        let d = std::env::temp_dir().join(format!("fluxctl-audit-{}", uuid::Uuid::new_v4()));
        let log = AuditLog::new(&d);
        let a = audit::describe_cli(
            &["fluxctl", "mac", "run", "macos-15", "--name", "web"].map(String::from),
        )
        .unwrap();
        record_cli(&d, &a, &Ok(()));
        let b = audit::describe_cli(&["fluxctl", "mac", "rm", "web"].map(String::from)).unwrap();
        record_cli(
            &d,
            &b,
            &Err(anyhow::anyhow!("the image policy denied it, token=hunter2")),
        );
        let page = log.query(&Query::default()).unwrap();
        assert_eq!(page.items[0].action, "mac.rm");
        assert_eq!(page.items[0].result, "denied");
        let raw = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
        assert!(!raw.contains("hunter2"), "{raw}");
        let text = render(&page);
        assert!(
            text.contains("mac.run") && text.contains("2 of 2 records."),
            "{text}"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (all, _) = rt
            .block_on(collect_all(&Source::Local(&d), Query::default()))
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.action.as_str()).collect::<Vec<_>>(),
            ["mac.run", "mac.rm"],
            "oldest first"
        );
        let csv = to_csv(&all);
        assert!(
            csv.starts_with("time,action,actor") && csv.contains("mac.rm"),
            "{csv}"
        );
        assert_eq!(to_jsonl(&all).unwrap().lines().count(), 2);
        assert_eq!(csv_cell("=1+1"), "'=1+1");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn query_string_encodes_filters() {
        let q = Query {
            action: Some("mac.".into()),
            target: Some("a b".into()),
            limit: Some(5),
            ..Default::default()
        };
        let s = query_string(&q);
        assert!(
            s.contains("action=mac.") && s.contains("limit=5") && !s.contains("a b"),
            "{s}"
        );
    }
}
