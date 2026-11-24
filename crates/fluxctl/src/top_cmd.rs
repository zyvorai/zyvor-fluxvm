// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl top [--once] [--interval 2] [--sort cpu|mem|disk|net|name] [--all] [--json]`: a live table of what each VM is
//! using right now, like `orb top` and `docker stats`. The daemon's `GET /v1/stats` gives cumulative counters; this
//! command takes two samples and shows the difference as rates (CPU as a percentage of one core, disk and network as
//! bytes per second). A VM that has no usable counters (for example a hibernated one) shows `-`.

use crate::cli_errors::coded;
use crate::remote::Remote;
use anyhow::Result;
use fluxvm_scheduler::node_stats::{NodeStats, VmStats};
use reqwest::Method;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

#[derive(clap::Args, Debug, Clone)]
pub struct TopArgs {
    /// Seconds between samples.
    #[arg(long, default_value_t = 2.0)]
    pub interval: f64,
    /// Print one table and exit (it still waits one interval to measure rates).
    #[arg(long)]
    pub once: bool,
    /// Order by `cpu` (default), `mem`, `disk`, `net` or `name`.
    #[arg(long, default_value = "cpu")]
    pub sort: String,
    /// Include VMs that are not running.
    #[arg(short, long)]
    pub all: bool,
    /// One JSON document per sample instead of a table (implies one sample unless `--count` says otherwise).
    #[arg(long)]
    pub json: bool,
    /// Stop after this many samples.
    #[arg(long)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Cpu,
    Mem,
    Disk,
    Net,
    Name,
}

pub fn parse_sort(s: &str) -> Result<Sort, String> {
    Ok(match s {
        "cpu" => Sort::Cpu,
        "mem" | "memory" => Sort::Mem,
        "disk" | "io" => Sort::Disk,
        "net" | "network" => Sort::Net,
        "name" => Sort::Name,
        other => {
            return Err(format!(
                "cannot sort by {other:?}: use cpu, mem, disk, net or name"
            ));
        }
    })
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub status: String,
    pub vcpus: u32,
    /// Percent of one core (so 200 means two busy cores); `None` when a counter is missing.
    pub cpu_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_mib: u64,
    pub disk_bytes_per_sec: Option<f64>,
    pub net_bytes_per_sec: Option<f64>,
}

fn rate(prev: Option<u64>, cur: Option<u64>, secs: f64) -> Option<f64> {
    // A counter that went backwards (VM restarted between samples) has no meaningful rate.
    match (prev, cur) {
        (Some(p), Some(c)) if c >= p && secs > 0.0 => Some((c - p) as f64 / secs),
        _ => None,
    }
}

fn sum2(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        _ => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// Rates between two samples. VMs missing from the earlier sample show no rates. Pure.
pub fn rows(prev: &NodeStats, cur: &NodeStats, all: bool, sort: Sort) -> Vec<Row> {
    let secs = (cur.ts - prev.ts).num_milliseconds() as f64 / 1000.0;
    let before: HashMap<&str, &VmStats> = prev.vms.iter().map(|v| (v.id.as_str(), v)).collect();
    let mut out: Vec<Row> = cur
        .vms
        .iter()
        .filter(|v| all || v.status == "running")
        .map(|v| {
            let p = before.get(v.id.as_str());
            let cpu = match (p.and_then(|p| p.cpu_seconds_total), v.cpu_seconds_total) {
                (Some(a), Some(b)) if b >= a && secs > 0.0 => Some((b - a) / secs * 100.0),
                _ => None,
            };
            Row {
                id: v.id.clone(),
                name: v.name.clone(),
                status: v.status.clone(),
                vcpus: v.vcpus,
                cpu_percent: cpu,
                memory_used_bytes: v.memory_used_bytes,
                memory_mib: v.memory_mib,
                disk_bytes_per_sec: p.and_then(|p| {
                    rate(
                        sum2(p.disk_read_bytes_total, p.disk_write_bytes_total),
                        sum2(v.disk_read_bytes_total, v.disk_write_bytes_total),
                        secs,
                    )
                }),
                net_bytes_per_sec: p.and_then(|p| {
                    rate(
                        sum2(p.net_rx_bytes_total, p.net_tx_bytes_total),
                        sum2(v.net_rx_bytes_total, v.net_tx_bytes_total),
                        secs,
                    )
                }),
            }
        })
        .collect();
    let key = |o: Option<f64>| o.unwrap_or(-1.0);
    match sort {
        Sort::Name => out.sort_by(|a, b| a.name.cmp(&b.name)),
        Sort::Cpu => out.sort_by(|a, b| {
            key(b.cpu_percent)
                .total_cmp(&key(a.cpu_percent))
                .then(a.name.cmp(&b.name))
        }),
        Sort::Mem => out.sort_by(|a, b| {
            b.memory_used_bytes
                .cmp(&a.memory_used_bytes)
                .then(a.name.cmp(&b.name))
        }),
        Sort::Disk => out.sort_by(|a, b| {
            key(b.disk_bytes_per_sec)
                .total_cmp(&key(a.disk_bytes_per_sec))
                .then(a.name.cmp(&b.name))
        }),
        Sort::Net => out.sort_by(|a, b| {
            key(b.net_bytes_per_sec)
                .total_cmp(&key(a.net_bytes_per_sec))
                .then(a.name.cmp(&b.name))
        }),
    }
    out
}

fn rate_text(r: Option<f64>) -> String {
    r.map_or("-".into(), |v| {
        format!("{}/s", crate::df_cmd::bytes(v as u64))
    })
}

/// The table, plain text. Pure.
pub fn render(host: &NodeStats, rows: &[Row]) -> String {
    let mut out = format!(
        "{} VMs, {} running; host memory {} MiB free of {} MiB\n",
        host.host.vm_count,
        host.host.vms_running,
        host.host
            .memory_available_mib
            .map_or("?".into(), |v| v.to_string()),
        host.host
            .memory_total_mib
            .map_or("?".into(), |v| v.to_string()),
    );
    if rows.is_empty() {
        out.push_str("No running VM. Use --all to include stopped ones.\n");
        return out;
    }
    out.push_str(&format!(
        "{:<22} {:<8} {:>5} {:>8} {:>20} {:>11} {:>11}\n",
        "NAME", "STATUS", "VCPU", "CPU%", "MEMORY", "DISK I/O", "NET"
    ));
    for r in rows {
        let mem = match r.memory_used_bytes {
            Some(u) => format!("{} / {} MiB", u >> 20, r.memory_mib),
            None => format!("- / {} MiB", r.memory_mib),
        };
        out.push_str(&format!(
            "{:<22} {:<8} {:>5} {:>8} {:>20} {:>11} {:>11}\n",
            r.name.chars().take(22).collect::<String>(),
            r.status,
            r.vcpus,
            r.cpu_percent.map_or("-".into(), |c| format!("{c:.1}")),
            mem,
            rate_text(r.disk_bytes_per_sec),
            rate_text(r.net_bytes_per_sec)
        ));
    }
    out
}

async fn sample(r: &Remote) -> Result<NodeStats> {
    let v = r.call(Method::GET, "/v1/stats", None).await.map_err(|e| {
        coded(
            format!("could not read stats from the daemon: {e:#}"),
            "DAEMON_REQUIRED",
        )
    })?;
    serde_json::from_value(v).map_err(|e| {
        coded(
            format!("the daemon's stats answer was not understood: {e}"),
            "DAEMON_REQUIRED",
        )
    })
}

pub async fn run(r: &Remote, a: &TopArgs) -> Result<()> {
    let sort = parse_sort(&a.sort).map_err(|m| coded(m, "TOP_BAD_SORT"))?;
    if !(a.interval.is_finite() && (0.05..=3600.0).contains(&a.interval)) {
        return Err(coded(
            "--interval must be between 0.05 and 3600 seconds".to_string(),
            "TOP_BAD_SORT",
        ));
    }
    let limit = a.count.or((a.once || a.json).then_some(1));
    let mut prev = sample(r).await?;
    let mut n = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs_f64(a.interval)).await;
        let cur = sample(r).await?;
        let table = rows(&prev, &cur, a.all, sort);
        if a.json {
            println!(
                "{}",
                serde_json::to_string(
                    &serde_json::json!({"ts": cur.ts, "host": cur.host, "vms": table})
                )?
            );
        } else {
            if limit.is_none() {
                // Redraw in place on a terminal; a pipe just gets successive tables.
                use std::io::IsTerminal;
                if std::io::stdout().is_terminal() {
                    print!("\x1b[2J\x1b[H");
                }
            }
            print!("{}", render(&cur, &table));
        }
        n += 1;
        if limit.is_some_and(|l| n >= l) {
            return Ok(());
        }
        prev = cur;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fake, spawn};
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn ns(ts: &str, vms: Vec<Value>) -> Value {
        json!({"ts": ts, "host": {"cpus": 8, "memory_total_mib": 16384, "memory_available_mib": 8000, "vm_count": vms.len(), "vms_running": vms.len()}, "vms": vms})
    }

    fn v(id: &str, name: &str, status: &str, cpu: f64, disk: u64, net: u64) -> Value {
        json!({"id": id, "name": name, "status": status, "vcpus": 2, "memory_mib": 2048, "cpu_seconds_total": cpu,
               "memory_used_bytes": 512u64 << 20, "disk_read_bytes_total": disk, "disk_write_bytes_total": 0,
               "net_rx_bytes_total": net, "net_tx_bytes_total": net})
    }

    fn parsed(v: Value) -> NodeStats {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn rates_come_from_the_difference_between_two_samples() {
        let a = parsed(ns(
            "2026-11-24T10:00:00Z",
            vec![
                v("1", "busy", "running", 10.0, 0, 0),
                v("2", "idle", "running", 5.0, 0, 0),
            ],
        ));
        let b = parsed(ns(
            "2026-11-24T10:00:02Z",
            vec![
                v("1", "busy", "running", 13.0, 4096, 1000),
                v("2", "idle", "running", 5.2, 0, 0),
            ],
        ));
        let r = rows(&a, &b, false, Sort::Cpu);
        assert_eq!(r[0].name, "busy");
        assert!((r[0].cpu_percent.unwrap() - 150.0).abs() < 1e-6);
        assert_eq!(r[0].disk_bytes_per_sec, Some(2048.0));
        assert_eq!(r[0].net_bytes_per_sec, Some(1000.0));
        assert!((r[1].cpu_percent.unwrap() - 10.0).abs() < 1e-6);
        let t = render(&b, &r);
        assert!(
            t.contains("busy") && t.contains("150.0") && t.contains("2.0 KiB/s"),
            "{t}"
        );
    }

    #[test]
    fn new_vms_and_restarted_counters_show_dashes_not_nonsense() {
        let a = parsed(ns(
            "2026-11-24T10:00:00Z",
            vec![v("1", "web", "running", 100.0, 9000, 0)],
        ));
        let b = parsed(ns(
            "2026-11-24T10:00:01Z",
            vec![
                v("1", "web", "running", 1.0, 10, 0),
                v("3", "new", "running", 1.0, 0, 0),
            ],
        ));
        let r = rows(&a, &b, false, Sort::Name);
        let web = r.iter().find(|r| r.name == "web").unwrap();
        assert_eq!((web.cpu_percent, web.disk_bytes_per_sec), (None, None));
        let new = r.iter().find(|r| r.name == "new").unwrap();
        assert!(new.cpu_percent.is_none());
        assert!(render(&b, &r).contains(" -"));
    }

    #[test]
    fn stopped_vms_need_all_and_sorting_works() {
        let a = parsed(ns("2026-11-24T10:00:00Z", vec![]));
        let mut b = parsed(ns(
            "2026-11-24T10:00:01Z",
            vec![
                v("1", "z", "running", 0.0, 0, 0),
                v("2", "a", "stopped", 0.0, 0, 0),
            ],
        ));
        b.vms[0].memory_used_bytes = Some(1 << 30);
        assert_eq!(rows(&a, &b, false, Sort::Name).len(), 1);
        let all = rows(&a, &b, true, Sort::Name);
        assert_eq!(all[0].name, "a");
        assert_eq!(rows(&a, &b, true, Sort::Mem)[0].name, "z");
        assert!(render(&b, &rows(&a, &b, false, Sort::Cpu)[..0]).contains("No running VM"));
    }

    #[test]
    fn sort_names() {
        assert_eq!(parse_sort("memory"), Ok(Sort::Mem));
        assert!(
            parse_sort("heat")
                .unwrap_err()
                .contains("cpu, mem, disk, net or name")
        );
    }

    #[tokio::test]
    async fn once_samples_twice_and_prints_json() {
        let f = Arc::new(Fake::default());
        {
            let mut s = f.stats.lock().unwrap();
            s.push_back(ns(
                "2026-11-24T10:00:00Z",
                vec![v("1", "web", "running", 1.0, 0, 0)],
            ));
            s.push_back(ns(
                "2026-11-24T10:00:01Z",
                vec![v("1", "web", "running", 1.5, 0, 0)],
            ));
        }
        let r = spawn(f.clone()).await;
        let a = TopArgs {
            interval: 0.05,
            once: true,
            sort: "cpu".into(),
            all: false,
            json: true,
            count: None,
        };
        run(&r, &a).await.unwrap();
        assert_eq!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| *c == "GET stats")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn bad_arguments_and_a_dead_daemon_are_coded() {
        let r = Remote::new("http://127.0.0.1:1", None);
        let a = |sort: &str, interval: f64| TopArgs {
            interval,
            once: true,
            sort: sort.into(),
            all: false,
            json: false,
            count: None,
        };
        for bad in [a("heat", 1.0), a("cpu", 0.0), a("cpu", f64::NAN)] {
            let e = run(&r, &bad).await.unwrap_err();
            assert!(
                format!("{e:#}").contains("fluxctl explain TOP_BAD_SORT"),
                "{e:#}"
            );
        }
        let e = run(&r, &a("cpu", 1.0)).await.unwrap_err();
        assert!(format!("{e:#}").contains("fluxctl explain DAEMON_REQUIRED"));
    }
}
