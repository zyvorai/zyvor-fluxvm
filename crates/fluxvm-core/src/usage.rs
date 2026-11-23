// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Per-VM usage and cost: the ledger the daemon fills once a minute, the report built from it, and the rate math.
//!
//! The ledger keeps one bucket per VM per UTC day (CPU seconds, memory GiB-hours, disk and network bytes, running
//! seconds), so its size is bounded by `MAX_DAYS` times the VM count and never grows with uptime. It is fed by
//! [`Ledger::observe`] with the counters `fluxvm_scheduler::node_stats` already reads (cgroups, the Mac runner, TAP
//! devices); nothing here touches the machine, so it is all testable with plain numbers.
//!
//! Counters are cumulative, so usage is the difference between two samples; a counter that went backwards (the VM was
//! restarted) counts from zero. The first sample of a running VM only sets the baseline. Memory is the size the VM was
//! given (what a cloud bills), counted while it runs; CPU is what it actually used.
//!
//! Prices are optional. With no rate set the report shows units only and says how to set one.

use crate::config::UsageConfig;
use crate::model::LabelSelector;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Days of history kept.
pub const MAX_DAYS: i64 = 90;
/// Hard cap on buckets, whatever the VM count, so a host with thousands of short-lived VMs stays bounded.
pub const MAX_BUCKETS: usize = 20_000;
/// A gap longer than this between two samples (the daemon was down) is counted as this long, not as the whole gap.
pub const MAX_GAP_SECONDS: f64 = 300.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// What one VM (or a group of them) used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Units {
    pub running_seconds: f64,
    pub cpu_seconds: f64,
    pub memory_gib_hours: f64,
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
}

impl Units {
    pub fn add(&mut self, o: &Units) {
        self.running_seconds += o.running_seconds;
        self.cpu_seconds += o.cpu_seconds;
        self.memory_gib_hours += o.memory_gib_hours;
        self.disk_read_bytes += o.disk_read_bytes;
        self.disk_write_bytes += o.disk_write_bytes;
        self.net_rx_bytes += o.net_rx_bytes;
        self.net_tx_bytes += o.net_tx_bytes;
    }
}

/// What the sampler saw of one VM at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Obs {
    pub id: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub running: bool,
    pub memory_mib: u64,
    pub cpu_seconds_total: Option<f64>,
    pub disk_read_bytes_total: Option<u64>,
    pub disk_write_bytes_total: Option<u64>,
    pub net_rx_bytes_total: Option<u64>,
    pub net_tx_bytes_total: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Last {
    ts: DateTime<Utc>,
    cpu: Option<f64>,
    dr: Option<u64>,
    dw: Option<u64>,
    rx: Option<u64>,
    tx: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    /// UTC day, `YYYY-MM-DD`.
    pub day: String,
    pub vm_id: String,
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub units: Units,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    buckets: BTreeMap<String, Bucket>,
    #[serde(default)]
    last: BTreeMap<String, Last>,
}

fn delta_f(cur: Option<f64>, prev: Option<f64>) -> f64 {
    match (cur, prev) {
        (Some(c), Some(p)) if c >= p => c - p,
        // went backwards: the counter restarted
        (Some(c), Some(_)) => c.max(0.0),
        _ => 0.0,
    }
}

fn delta_u(cur: Option<u64>, prev: Option<u64>) -> u64 {
    match (cur, prev) {
        (Some(c), Some(p)) if c >= p => c - p,
        (Some(c), Some(_)) => c,
        _ => 0,
    }
}

impl Ledger {
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Adds what happened since the previous call for each VM seen running, then drops what is too old.
    pub fn observe(&mut self, now: DateTime<Utc>, obs: &[Obs]) {
        let day = now.format("%Y-%m-%d").to_string();
        let mut still: Vec<&str> = Vec::new();
        for o in obs.iter().filter(|o| o.running) {
            still.push(&o.id);
            let cur = Last {
                ts: now,
                cpu: o.cpu_seconds_total,
                dr: o.disk_read_bytes_total,
                dw: o.disk_write_bytes_total,
                rx: o.net_rx_bytes_total,
                tx: o.net_tx_bytes_total,
            };
            if let Some(p) = self.last.insert(o.id.clone(), cur.clone()) {
                let gap =
                    ((now - p.ts).num_milliseconds() as f64 / 1000.0).clamp(0.0, MAX_GAP_SECONDS);
                let u = Units {
                    running_seconds: gap,
                    cpu_seconds: delta_f(cur.cpu, p.cpu),
                    memory_gib_hours: o.memory_mib as f64 / 1024.0 * gap / 3600.0,
                    disk_read_bytes: delta_u(cur.dr, p.dr),
                    disk_write_bytes: delta_u(cur.dw, p.dw),
                    net_rx_bytes: delta_u(cur.rx, p.rx),
                    net_tx_bytes: delta_u(cur.tx, p.tx),
                };
                let b = self
                    .buckets
                    .entry(format!("{day}|{}", o.id))
                    .or_insert_with(|| Bucket {
                        day: day.clone(),
                        vm_id: o.id.clone(),
                        name: o.name.clone(),
                        labels: BTreeMap::new(),
                        units: Units::default(),
                    });
                b.name.clone_from(&o.name);
                b.labels.clone_from(&o.labels);
                b.units.add(&u);
            }
        }
        // A VM that stopped (or was deleted) gets a fresh baseline when it runs again.
        self.last.retain(|id, _| still.contains(&id.as_str()));
        self.prune(now.date_naive());
    }

    fn prune(&mut self, today: NaiveDate) {
        let oldest = (today - Duration::days(MAX_DAYS))
            .format("%Y-%m-%d")
            .to_string();
        self.buckets.retain(|_, b| b.day >= oldest);
        // Keys start with the day, so the first keys are the oldest.
        while self.buckets.len() > MAX_BUCKETS {
            let Some(k) = self.buckets.keys().next().cloned() else {
                break;
            };
            self.buckets.remove(&k);
        }
    }

    pub fn report(&self, q: &Query, rates: &UsageConfig) -> Report {
        let keep = |b: &&Bucket| {
            q.since_day.as_deref().is_none_or(|d| b.day.as_str() >= d)
                && q.selector.as_ref().is_none_or(|s| s.matches(&b.labels))
        };
        let group = |by: &GroupBy| {
            let mut m: BTreeMap<String, (String, Units)> = BTreeMap::new();
            for b in self.buckets.values().filter(keep) {
                let (key, name) = match by {
                    GroupBy::Vm => (b.vm_id.clone(), b.name.clone()),
                    GroupBy::Day => (b.day.clone(), b.day.clone()),
                    GroupBy::Label(k) => {
                        let v = b.labels.get(k).cloned().unwrap_or_else(|| "(none)".into());
                        (v.clone(), v)
                    }
                };
                let e = m.entry(key).or_insert((name.clone(), Units::default()));
                e.0 = name; // the latest name wins (buckets are in day order)
                e.1.add(&b.units);
            }
            let mut rows: Vec<Row> = m
                .into_iter()
                .map(|(key, (name, units))| Row {
                    cost: cost(rates, &units),
                    key,
                    name,
                    units,
                })
                .collect();
            if !matches!(by, GroupBy::Day) {
                rows.sort_by(|a, b| {
                    b.units
                        .cpu_seconds
                        .total_cmp(&a.units.cpu_seconds)
                        .then(a.name.cmp(&b.name))
                });
            }
            rows
        };
        let rows = group(&q.by);
        let days = if matches!(q.by, GroupBy::Day) {
            rows.clone()
        } else {
            group(&GroupBy::Day)
        };
        let mut total = Units::default();
        for r in &days {
            total.add(&r.units);
        }
        Report {
            by: q.by.to_string(),
            since: q.since_day.clone(),
            priced: rates.any_set(),
            currency: rates.currency_or_default(),
            total_cost: cost(rates, &total),
            total,
            rows,
            days,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupBy {
    Vm,
    Day,
    /// Group by the value of this label key.
    Label(String),
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupBy::Vm => write!(f, "vm"),
            GroupBy::Day => write!(f, "day"),
            GroupBy::Label(k) => write!(f, "label:{k}"),
        }
    }
}

impl GroupBy {
    /// `vm`, `day`, `label:KEY` or `label=KEY`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        match t {
            "vm" => Ok(Self::Vm),
            "day" => Ok(Self::Day),
            _ => match t.strip_prefix("label:").or_else(|| t.strip_prefix("label=")) {
                Some(k) if !k.trim().is_empty() => Ok(Self::Label(k.trim().to_owned())),
                _ if t == "label" => Err("`label` needs the label to group by, for example `--by label:team` (see the labels with `fluxctl ps --json`)".into()),
                _ => Err(format!("cannot group by {t:?}: use `vm`, `day` or `label:KEY` (for example `label:team`)")),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct Query {
    pub by: GroupBy,
    /// First UTC day to include (`YYYY-MM-DD`); `None` is everything kept.
    pub since_day: Option<String>,
    pub selector: Option<LabelSelector>,
}

/// `7d`, `24h`, `2w` or a date `2026-11-01` as the first day to include. Whole days: `24h` means today and yesterday.
pub fn parse_since(s: &str, now: DateTime<Utc>) -> Result<String, String> {
    let t = s.trim();
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Ok(d.format("%Y-%m-%d").to_string());
    }
    let bad = || {
        format!(
            "cannot read --since {t:?}: use a length like `7d`, `24h`, `2w` or a date like `2026-11-01`"
        )
    };
    if t.len() < 2 || !t.is_char_boundary(t.len() - 1) {
        return Err(bad());
    }
    let (n, unit) = t.split_at(t.len() - 1);
    let n: i64 = n.parse().map_err(|_| bad())?;
    if !(0..=36_500).contains(&n) {
        return Err(bad());
    }
    let d = match unit {
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => return Err(bad()),
    };
    Ok((now - d).format("%Y-%m-%d").to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// VM id, day or label value.
    pub key: String,
    /// VM name, day or label value: what to show.
    pub name: String,
    pub units: Units,
    /// `None` when no rate is set.
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub by: String,
    pub since: Option<String>,
    /// Whether any rate is set; when false every `cost` is `None` and the page says how to set one.
    pub priced: bool,
    pub currency: String,
    pub total: Units,
    #[serde(default)]
    pub total_cost: Option<f64>,
    pub rows: Vec<Row>,
    /// Per-day totals (the chart), always present.
    pub days: Vec<Row>,
}

impl Report {
    /// Fills the costs from `rates`, for a client whose own config has rates the daemon does not.
    pub fn reprice(&mut self, rates: &UsageConfig) {
        self.priced = rates.any_set();
        self.currency = rates.currency_or_default();
        self.total_cost = cost(rates, &self.total);
        for r in self.rows.iter_mut().chain(self.days.iter_mut()) {
            r.cost = cost(rates, &r.units);
        }
    }
}

/// The price of `u`, or `None` when no rate is set.
pub fn cost(r: &UsageConfig, u: &Units) -> Option<f64> {
    if !r.any_set() {
        return None;
    }
    let disk = (u.disk_read_bytes + u.disk_write_bytes) as f64 / GIB;
    let net = (u.net_rx_bytes + u.net_tx_bytes) as f64 / GIB;
    Some(
        u.cpu_seconds / 3600.0 * r.rate(r.cpu_hour)
            + u.memory_gib_hours * r.rate(r.memory_gib_hour)
            + disk * r.rate(r.disk_gib)
            + net * r.rate(r.network_gib),
    )
}

pub const NO_RATES_NOTE: &str = "No rates set, showing units only. To see costs: `fluxctl config set usage.cpu_hour 0.04` and `usage.memory_gib_hour 0.005` (also `usage.disk_gib`, `usage.network_gib`, `usage.currency`).";

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    fn vm(id: &str, label: Option<(&str, &str)>, cpu: f64, rx: u64) -> Obs {
        Obs {
            id: id.into(),
            name: format!("vm-{id}"),
            labels: label
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .into_iter()
                .collect(),
            running: true,
            memory_mib: 2048,
            cpu_seconds_total: Some(cpu),
            disk_read_bytes_total: Some(0),
            disk_write_bytes_total: Some(0),
            net_rx_bytes_total: Some(rx),
            net_tx_bytes_total: None,
        }
    }

    fn q(by: GroupBy) -> Query {
        Query {
            by,
            since_day: None,
            selector: None,
        }
    }

    fn rates() -> UsageConfig {
        UsageConfig {
            currency: "EUR".into(),
            cpu_hour: 0.5,
            memory_gib_hour: 0.1,
            disk_gib: 0.0,
            network_gib: 1.0,
        }
    }

    #[test]
    fn first_sample_is_only_a_baseline_then_deltas_accrue() {
        let mut l = Ledger::default();
        l.observe(t(0), &[vm("a", None, 100.0, 1000)]);
        assert!(l.is_empty());
        l.observe(t(60), &[vm("a", None, 130.0, 3000)]);
        let r = l.report(&q(GroupBy::Vm), &UsageConfig::default());
        let u = &r.rows[0].units;
        assert_eq!(u.running_seconds, 60.0);
        assert_eq!(u.cpu_seconds, 30.0);
        assert_eq!(u.net_rx_bytes, 2000);
        // 2 GiB for 60 s
        assert!((u.memory_gib_hours - 2.0 * 60.0 / 3600.0).abs() < 1e-9);
    }

    #[test]
    fn a_counter_that_goes_backwards_restarts_from_zero_and_a_stop_rebaselines() {
        let mut l = Ledger::default();
        l.observe(t(0), &[vm("a", None, 500.0, 0)]);
        l.observe(t(60), &[vm("a", None, 20.0, 0)]);
        assert_eq!(
            l.report(&q(GroupBy::Vm), &UsageConfig::default())
                .total
                .cpu_seconds,
            20.0
        );
        let mut stopped = vm("a", None, 20.0, 0);
        stopped.running = false;
        l.observe(t(120), &[stopped]);
        l.observe(t(180), &[vm("a", None, 9000.0, 0)]);
        // the sample after a stop is a new baseline, so the 9000 is not charged
        assert_eq!(
            l.report(&q(GroupBy::Vm), &UsageConfig::default())
                .total
                .cpu_seconds,
            20.0
        );
    }

    #[test]
    fn a_long_gap_is_capped() {
        let mut l = Ledger::default();
        l.observe(t(0), &[vm("a", None, 0.0, 0)]);
        l.observe(t(10_000), &[vm("a", None, 5.0, 0)]);
        assert_eq!(
            l.report(&q(GroupBy::Vm), &UsageConfig::default())
                .total
                .running_seconds,
            MAX_GAP_SECONDS
        );
    }

    #[test]
    fn groups_by_vm_label_and_day_and_filters_by_label_and_since() {
        let mut l = Ledger::default();
        let day2 = 86_400;
        for (s, a, b) in [
            (0, 0.0, 0.0),
            (60, 10.0, 40.0),
            (day2, 10.0, 40.0),
            (day2 + 60, 30.0, 40.0),
        ] {
            l.observe(
                t(s),
                &[
                    vm("a", Some(("team", "x")), a, 0),
                    vm("b", Some(("team", "y")), b, 0),
                ],
            );
        }
        let none = UsageConfig::default();
        let by_vm = l.report(&q(GroupBy::Vm), &none);
        assert_eq!(by_vm.rows.len(), 2);
        assert_eq!(by_vm.rows[0].name, "vm-b", "sorted by CPU, biggest first");
        let by_label = l.report(&q(GroupBy::Label("team".into())), &none);
        let names: Vec<_> = by_label
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.units.cpu_seconds))
            .collect();
        assert_eq!(names, vec![("y", 40.0), ("x", 30.0)]);
        let by_day = l.report(&q(GroupBy::Day), &none);
        assert_eq!(by_day.rows.len(), 2);
        assert!(by_day.rows[0].key < by_day.rows[1].key);
        // a label with no VM that has it
        let nolabel = l.report(&q(GroupBy::Label("env".into())), &none);
        assert_eq!(nolabel.rows[0].name, "(none)");
        let sel = Query {
            by: GroupBy::Vm,
            since_day: None,
            selector: Some(LabelSelector::parse("team=x").unwrap()),
        };
        assert_eq!(l.report(&sel, &none).rows.len(), 1);
        let since = Query {
            by: GroupBy::Vm,
            since_day: Some(by_day.rows[1].key.clone()),
            selector: None,
        };
        assert_eq!(l.report(&since, &none).total.cpu_seconds, 20.0);
    }

    #[test]
    fn rate_math() {
        let u = Units {
            cpu_seconds: 7200.0,
            memory_gib_hours: 4.0,
            net_rx_bytes: 1 << 30,
            net_tx_bytes: 1 << 30,
            disk_read_bytes: 5 << 30,
            ..Default::default()
        };
        // 2 h * 0.5 + 4 * 0.1 + 2 GiB * 1.0 (disk is free)
        assert!((cost(&rates(), &u).unwrap() - 3.4).abs() < 1e-9);
        assert_eq!(cost(&UsageConfig::default(), &u), None);
        // a negative or non-finite rate counts as not set
        let bad = UsageConfig {
            cpu_hour: -1.0,
            memory_gib_hour: f64::NAN,
            ..Default::default()
        };
        assert_eq!(cost(&bad, &u), None);
    }

    #[test]
    fn report_prices_and_reprices() {
        let mut l = Ledger::default();
        l.observe(t(0), &[vm("a", None, 0.0, 0)]);
        l.observe(t(60), &[vm("a", None, 3600.0, 0)]);
        let mut r = l.report(&q(GroupBy::Vm), &UsageConfig::default());
        assert!(!r.priced && r.total_cost.is_none() && r.rows[0].cost.is_none());
        r.reprice(&rates());
        assert!(r.priced && r.currency == "EUR");
        let want = 0.5 + 2.0 * 60.0 / 3600.0 * 0.1;
        assert!((r.total_cost.unwrap() - want).abs() < 1e-9);
        assert!((r.rows[0].cost.unwrap() - want).abs() < 1e-9);
    }

    #[test]
    fn storage_is_bounded_by_days_and_by_count() {
        let mut l = Ledger::default();
        for d in 0..(MAX_DAYS + 30) {
            l.observe(t(d * 86_400), &[vm("a", None, d as f64, 0)]);
            l.observe(t(d * 86_400 + 60), &[vm("a", None, d as f64 + 1.0, 0)]);
        }
        assert!(l.len() as i64 <= MAX_DAYS + 1, "kept {}", l.len());
        let mut big = Ledger::default();
        for i in 0..(MAX_BUCKETS + 50) {
            big.buckets.insert(
                format!("2026-01-01|{i:06}"),
                Bucket {
                    day: format!("{}", t(0).format("%Y-%m-%d")),
                    vm_id: i.to_string(),
                    name: String::new(),
                    labels: BTreeMap::new(),
                    units: Units::default(),
                },
            );
        }
        big.prune(t(0).date_naive());
        assert_eq!(big.len(), MAX_BUCKETS);
    }

    #[test]
    fn grouping_and_since_parse() {
        assert_eq!(GroupBy::parse("vm"), Ok(GroupBy::Vm));
        assert_eq!(
            GroupBy::parse("label:team"),
            Ok(GroupBy::Label("team".into()))
        );
        assert_eq!(
            GroupBy::parse("label=team"),
            Ok(GroupBy::Label("team".into()))
        );
        assert!(GroupBy::parse("label").unwrap_err().contains("label:team"));
        assert!(GroupBy::parse("host").is_err());
        let now = t(0); // 2027-01-15
        assert_eq!(parse_since("2026-11-01", now).unwrap(), "2026-11-01");
        assert_eq!(
            parse_since("7d", now).unwrap(),
            (now - Duration::days(7)).format("%Y-%m-%d").to_string()
        );
        assert_eq!(
            parse_since("2w", now).unwrap(),
            (now - Duration::weeks(2)).format("%Y-%m-%d").to_string()
        );
        for bad in ["", "d", "7", "7x", "-3d", "é", "99999d"] {
            assert!(parse_since(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_ledger_round_trips_through_json() {
        let mut l = Ledger::default();
        l.observe(t(0), &[vm("a", Some(("k", "v")), 1.0, 1)]);
        l.observe(t(60), &[vm("a", Some(("k", "v")), 2.0, 2)]);
        let back: Ledger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
    }
}
