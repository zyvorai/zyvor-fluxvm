// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl usage [--by vm|day|label:KEY] [--since 7d] [--label SELECTOR] [--json]`: what each VM used (CPU seconds, memory
//! GiB-hours, disk and network bytes) and, when rates are set (`fluxctl config set usage.cpu_hour 0.04`), what it cost.
//! The daemon does the counting (`GET /v1/usage`); rates in this machine's config file win over the daemon's, so the
//! numbers match what `config get usage.cpu_hour` shows here.

use crate::cli_errors::coded;
use crate::remote::{Remote, encode_query};
use anyhow::Result;
use fluxvm_core::config::UsageConfig;
use fluxvm_core::usage::{NO_RATES_NOTE, Report, Units};
use reqwest::Method;

#[derive(clap::Args, Debug, Clone)]
pub struct UsageArgs {
    /// Sum by `vm` (default), `day`, or `label:KEY` (for example `label:team`).
    #[arg(long, default_value = "vm")]
    pub by: String,
    /// Only this far back: `7d`, `24h`, `2w` or a date `2026-11-01` (whole UTC days). Default: all that is kept (90 days).
    #[arg(long)]
    pub since: Option<String>,
    /// Only VMs whose labels match, for example `env=dev,team` (same as `fluxctl ps --label`).
    #[arg(long)]
    pub label: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn query(a: &UsageArgs) -> String {
    let mut q = vec![format!("by={}", encode_query(&a.by))];
    if let Some(s) = &a.since {
        q.push(format!("since={}", encode_query(s)));
    }
    if let Some(l) = &a.label {
        q.push(format!("label={}", encode_query(l)));
    }
    format!("/v1/usage?{}", q.join("&"))
}

pub async fn run(r: &Remote, a: &UsageArgs, local: &UsageConfig) -> Result<()> {
    let v = r.call(Method::GET, &query(a), None).await.map_err(|e| {
        let m = format!("{e:#}");
        if m.contains("USAGE_BAD_QUERY") {
            e
        } else {
            coded(
                format!("could not read usage from the daemon: {m}"),
                "USAGE_UNAVAILABLE",
            )
        }
    })?;
    let mut report: Report = serde_json::from_value(v).map_err(|e| {
        coded(
            format!("the daemon's usage answer was not understood: {e}"),
            "USAGE_UNAVAILABLE",
        )
    })?;
    if local.any_set() {
        report.reprice(local);
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn hours(secs: f64) -> String {
    format!("{:.2}", secs / 3600.0)
}

fn line(name: &str, u: &Units, cost: Option<f64>, cur: &str, priced: bool) -> String {
    let mut s = format!(
        "{:<24} {:>9} {:>9} {:>11} {:>11} {:>11}",
        name.chars().take(24).collect::<String>(),
        hours(u.running_seconds),
        hours(u.cpu_seconds),
        format!("{:.2}", u.memory_gib_hours),
        bytes(u.disk_read_bytes + u.disk_write_bytes),
        bytes(u.net_rx_bytes + u.net_tx_bytes),
    );
    if priced {
        s.push_str(&format!(
            " {:>10}",
            cost.map_or("-".into(), |c| format!("{c:.2} {cur}"))
        ));
    }
    s
}

/// The table, plain text. Pure.
pub fn render(r: &Report) -> String {
    if r.rows.is_empty() {
        return "No usage recorded yet. FluxVM samples running VMs once a minute and keeps 90 days; start a VM and check back in a few minutes.\n".into();
    }
    let head = match r.by.as_str() {
        "vm" => "VM",
        "day" => "DAY",
        _ => "GROUP",
    };
    let mut out = format!(
        "{:<24} {:>9} {:>9} {:>11} {:>11} {:>11}{}\n",
        head,
        "RUN HRS",
        "CPU HRS",
        "MEM GiB-HRS",
        "DISK I/O",
        "NETWORK",
        if r.priced {
            format!(" {:>10}", "COST")
        } else {
            String::new()
        }
    );
    for row in &r.rows {
        out.push_str(&line(
            &row.name,
            &row.units,
            row.cost,
            &r.currency,
            r.priced,
        ));
        out.push('\n');
    }
    out.push_str(&line(
        "TOTAL",
        &r.total,
        r.total_cost,
        &r.currency,
        r.priced,
    ));
    out.push('\n');
    if !r.priced {
        out.push_str(&format!("\n{NO_RATES_NOTE}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::usage::Row;

    fn report(priced: bool) -> Report {
        let u = Units {
            running_seconds: 7200.0,
            cpu_seconds: 3600.0,
            memory_gib_hours: 4.0,
            net_rx_bytes: 3 << 20,
            ..Default::default()
        };
        Report {
            by: "vm".into(),
            since: None,
            priced,
            currency: "EUR".into(),
            total: u.clone(),
            total_cost: priced.then_some(1.5),
            rows: vec![Row {
                key: "a".into(),
                name: "web".into(),
                units: u.clone(),
                cost: priced.then_some(1.5),
            }],
            days: vec![],
        }
    }

    #[test]
    fn without_rates_it_says_so_and_shows_units() {
        let s = render(&report(false));
        assert!(s.contains("No rates set, showing units only"), "{s}");
        assert!(s.contains("fluxctl config set usage.cpu_hour"));
        assert!(s.contains("web") && s.contains("4.00") && s.contains("3.0 MiB"));
        assert!(!s.contains("COST"));
    }

    #[test]
    fn with_rates_it_shows_a_cost_column() {
        let s = render(&report(true));
        assert!(s.contains("COST") && s.contains("1.50 EUR"), "{s}");
        assert!(!s.contains("No rates set"));
    }

    #[test]
    fn empty_is_explained() {
        let mut r = report(false);
        r.rows.clear();
        assert!(render(&r).contains("No usage recorded yet"));
    }

    #[test]
    fn the_query_is_encoded() {
        let a = UsageArgs {
            by: "label:team".into(),
            since: Some("7d".into()),
            label: Some("env=dev,x".into()),
            json: false,
        };
        let q = query(&a);
        assert!(q.starts_with("/v1/usage?by=label"), "{q}");
        assert!(q.contains("&since=7d") && q.contains("&label=env"));
        assert!(!q.contains(' ') && !q.contains(','), "{q}");
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
    }
}
