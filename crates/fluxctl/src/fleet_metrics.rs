// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet top` and `fluxctl fleet metrics <vm>`: usage from central's metrics collector
//! (`GET /fleet/metrics/vms`, `GET /fleet/metrics/vms/{node}/{id}`).

use super::{Out, Table, Tone, c, ct, esc, get, resolve_vm};
use anyhow::Result;
use serde_json::Value;

fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

fn pct(x: Option<f64>) -> String {
    x.map_or_else(|| "–".into(), |x| format!("{x:.1}%"))
}

/// MiB as `512 MiB` / `1.9 GiB`.
fn mib(x: Option<f64>) -> String {
    match x {
        None => "–".into(),
        Some(x) if x < 1024.0 => format!("{x:.0} MiB"),
        Some(x) => format!("{:.1} GiB", x / 1024.0),
    }
}

/// Bytes per second in decimal units, as disk and network tools show them.
pub(super) fn rate(x: Option<f64>) -> String {
    let Some(x) = x else {
        return "–".into();
    };
    let (v, unit) = if x >= 1e9 {
        (x / 1e9, "GB/s")
    } else if x >= 1e6 {
        (x / 1e6, "MB/s")
    } else if x >= 1e3 {
        (x / 1e3, "kB/s")
    } else {
        (x, "B/s")
    };
    if unit == "B/s" {
        format!("{v:.0} {unit}")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// `items` ordered for `top`: busiest first by CPU or memory (VMs without a sample last), then by name.
pub(super) fn order(items: &mut [Value], sort: &str) {
    let key = if sort == "mem" {
        "memory_used_mib"
    } else {
        "cpu_percent"
    };
    items.sort_by(|a, b| {
        let (x, y) = (f(a, key), f(b, key));
        y.partial_cmp(&x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| match (x, y) {
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
}

pub(super) async fn top(
    central: &str,
    token: Option<&str>,
    out: &Out,
    sort: &str,
    limit: Option<usize>,
) -> Result<()> {
    let mut v = get(central, token, "/fleet/metrics/vms").await?;
    let mut items: Vec<Value> = v["items"].as_array().cloned().unwrap_or_default();
    order(&mut items, sort);
    if let Some(n) = limit {
        items.truncate(n);
    }
    if !out.human {
        v["items"] = Value::Array(items);
        return out.json(&v);
    }
    if items.is_empty() {
        println!("No usage samples yet.");
        let every = v["interval_seconds"].as_u64().unwrap_or(15);
        println!(
            "{}",
            out.dim(format!(
                "Central samples every Mac every {every} seconds; VMs show up here within a minute of starting."
            ))
        );
        return Ok(());
    }
    let mut t = Table::new(vec![
        "NAME",
        "MAC",
        "CPU",
        "MEMORY",
        "DISK R/W",
        "NET RX/TX",
    ]);
    for vm in &items {
        let cpu = f(vm, "cpu_percent");
        let tone = match cpu {
            Some(x) if x >= 90.0 => Tone::Bad,
            Some(x) if x >= 70.0 => Tone::Warn,
            None => Tone::Dim,
            _ => Tone::Plain,
        };
        let size = vm["memory_mib"].as_f64();
        let mem = match (f(vm, "memory_used_mib"), size) {
            (Some(u), Some(s)) if s > 0.0 => format!("{} / {}", mib(Some(u)), mib(Some(s))),
            (u, _) => mib(u),
        };
        let net = if f(vm, "net_rx_bps").is_none() && f(vm, "net_tx_bps").is_none() {
            "–".to_owned()
        } else {
            format!(
                "{} / {}",
                rate(f(vm, "net_rx_bps")),
                rate(f(vm, "net_tx_bps"))
            )
        };
        t.row(vec![
            c(vm["name"].as_str().unwrap_or("")),
            ct(vm["node"].as_str().unwrap_or(""), Tone::Dim),
            ct(pct(cpu), tone),
            c(mem),
            c(format!(
                "{} / {}",
                rate(f(vm, "disk_read_bps")),
                rate(f(vm, "disk_write_bps"))
            )),
            c(net),
        ]);
    }
    t.print(out);
    println!(
        "{}",
        out.dim(format!(
            "CPU is the share of each VM's vCPUs; sampled every {} s. – means not measured yet (or not on this kind of VM).",
            v["interval_seconds"].as_u64().unwrap_or(15)
        ))
    );
    Ok(())
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// A text sparkline of at most `width` cells: values averaged into equal chunks, scaled between 0 and the larger of
/// the maximum and `floor` (so an idle VM's CPU line stays low instead of filling the scale), a space where nothing
/// was measured.
pub(super) fn sparkline(values: &[Option<f64>], width: usize, floor: f64) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let chunk = values.len().div_ceil(width);
    let cells: Vec<Option<f64>> = values
        .chunks(chunk)
        .map(|c| {
            let got: Vec<f64> = c.iter().flatten().copied().collect();
            (!got.is_empty()).then(|| got.iter().sum::<f64>() / got.len() as f64)
        })
        .collect();
    let max = cells.iter().flatten().fold(floor, |m, x| m.max(*x));
    cells
        .iter()
        .map(|x| match x {
            None => ' ',
            Some(_) if max <= 0.0 => BARS[0],
            Some(x) => BARS[((x / max) * 7.0).round().clamp(0.0, 7.0) as usize],
        })
        .collect()
}

pub(super) async fn show(
    central: &str,
    token: Option<&str>,
    out: &Out,
    vm: &str,
    range: &str,
) -> Result<()> {
    let (node, id, name) = resolve_vm(central, token, vm).await?;
    let v = get(
        central,
        token,
        &format!(
            "/fleet/metrics/vms/{}/{}?range={}",
            esc(&node),
            esc(&id),
            esc(range)
        ),
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    let points: Vec<Value> = v["points"].as_array().cloned().unwrap_or_default();
    println!(
        "{} {} {}",
        out.bold(&name),
        out.dim(format!("on {node} · last {range} · one point a minute")),
        v["source"]
            .as_str()
            .map(|s| out.dim(format!("· source {s}")))
            .unwrap_or_default()
    );
    if points.is_empty() {
        println!(
            "{}",
            v["note"]
                .as_str()
                .unwrap_or("No samples yet; graphs appear within a minute.")
        );
        return Ok(());
    }
    let latest = &v["latest"];
    let col = |k: &str| -> Vec<Option<f64>> { points.iter().map(|p| f(p, k)).collect() };
    let stats = |vals: &[Option<f64>]| {
        let got: Vec<f64> = vals.iter().flatten().copied().collect();
        if got.is_empty() {
            None
        } else {
            Some((
                got.iter().sum::<f64>() / got.len() as f64,
                got.iter().fold(f64::MIN, |m, x| m.max(*x)),
            ))
        }
    };
    let vcpus = v["vcpus"].as_f64().unwrap_or(0.0);
    let size = v["memory_mib"].as_f64().unwrap_or(0.0);
    type Row = (&'static str, &'static str, f64, fn(Option<f64>) -> String);
    let rows: [Row; 6] = [
        ("CPU", "cpu_percent", 100.0, pct),
        ("Memory", "memory_used_mib", size, mib),
        ("Disk read", "disk_read_bps", 1e6, rate),
        ("Disk write", "disk_write_bps", 1e6, rate),
        ("Net in", "net_rx_bps", 1e5, rate),
        ("Net out", "net_tx_bps", 1e5, rate),
    ];
    let width = 48;
    for (label, key, floor, fmt) in rows {
        let vals = col(key);
        let Some((avg, max)) = stats(vals.as_slice()) else {
            println!(
                "{}  {}",
                out.dim(format!("{label:<10}")),
                out.dim("not measured")
            );
            continue;
        };
        println!(
            "{}  {}  {:>10}  {}",
            out.dim(format!("{label:<10}")),
            out.cyan(format!("{:<width$}", sparkline(&vals, width, floor))),
            fmt(f(latest, key)),
            out.dim(format!("avg {}, max {}", fmt(Some(avg)), fmt(Some(max))))
        );
    }
    if vcpus > 0.0 {
        println!(
            "{}",
            out.dim(format!(
                "CPU is the share of its {vcpus:.0} vCPUs; memory of {}.",
                mib(Some(size))
            ))
        );
    }
    if let Some(note) = v["note"].as_str() {
        println!("{}", out.dim(note));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `top` and `metrics` against a real central, a simulated Mac and its collector.
    #[tokio::test]
    async fn top_and_metrics_against_a_simulated_mac() {
        use crate::fleet_client::call;
        use reqwest::Method;
        let dir = tempfile::tempdir().unwrap();
        let app = fluxvm_agent::central::router(fluxvm_agent::central::CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("op".into()),
            reconcile_interval: None,
            ha: None,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let central = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let sim = fluxvm_agent::sim::SimNode::default().router();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node_url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, sim).await.unwrap() });
        let tok = Some("op");
        call(
            &central,
            tok,
            Method::POST,
            "/fleet/register",
            Some(
                json!({"name": "mac-1", "fluxvm_url": node_url, "vcpus_total": 12,
                "memory_mib_total": 32768, "vm_count": 0}),
            ),
        )
        .await
        .unwrap();
        call(
            &central,
            tok,
            Method::POST,
            "/fleet/vms",
            Some(json!({"name": "busy", "image": "/vms/x.raw", "vcpus": 4, "memory_mib": 4096})),
        )
        .await
        .unwrap();
        let quiet = Out {
            human: false,
            wide: false,
            color: false,
        };
        // Before any sample: an empty list, not an error.
        top(&central, tok, &quiet, "cpu", None).await.unwrap();
        for _ in 0..2 {
            call(&central, tok, Method::POST, "/fleet/metrics/collect", None)
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        let v = get(&central, tok, "/fleet/metrics/vms").await.unwrap();
        let row = &v["items"][0];
        assert_eq!(row["name"], "busy");
        assert!(row["cpu_percent"].is_number(), "{v}");
        top(&central, tok, &quiet, "mem", Some(5)).await.unwrap();
        show(&central, tok, &quiet, "busy", "6h").await.unwrap();
        let human = Out {
            human: true,
            wide: false,
            color: false,
        };
        top(&central, tok, &human, "cpu", None).await.unwrap();
        show(&central, tok, &human, "busy", "1h").await.unwrap();
    }

    #[test]
    fn units_read_like_people_write_them() {
        assert_eq!(rate(Some(512.0)), "512 B/s");
        assert_eq!(rate(Some(2_500_000.0)), "2.5 MB/s");
        assert_eq!(rate(None), "–");
        assert_eq!(mib(Some(512.0)), "512 MiB");
        assert_eq!(mib(Some(2048.0)), "2.0 GiB");
        assert_eq!(pct(Some(37.25)), "37.2%");
    }

    #[test]
    fn top_orders_by_cpu_or_memory_with_unsampled_vms_last() {
        let mut items = vec![
            json!({"name": "a", "cpu_percent": 10.0, "memory_used_mib": 900.0}),
            json!({"name": "b", "cpu_percent": null, "memory_used_mib": null}),
            json!({"name": "c", "cpu_percent": 80.0, "memory_used_mib": 100.0}),
        ];
        order(&mut items, "cpu");
        let names: Vec<&str> = items.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["c", "a", "b"]);
        order(&mut items, "mem");
        let names: Vec<&str> = items.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["a", "c", "b"]);
    }

    #[test]
    fn sparklines_scale_and_leave_gaps() {
        let line = sparkline(&[Some(0.0), None, Some(50.0), Some(100.0)], 10, 100.0);
        assert_eq!(line, "▁ ▅█");
        // More points than cells are averaged down.
        let many: Vec<Option<f64>> = (0..120).map(|i| Some(i as f64)).collect();
        assert_eq!(sparkline(&many, 40, 0.0).chars().count(), 40);
        // An idle VM stays at the bottom of a 0–100% scale.
        assert_eq!(sparkline(&[Some(1.0), Some(2.0)], 10, 100.0), "▁▁");
        assert_eq!(sparkline(&[], 10, 1.0), "");
    }
}
