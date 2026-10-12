// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Prices and statements: turns the metered usage of [`crate::tenancy`] into money.
//!
//! The operator sets a [`PriceList`]: an hourly price per leased Mac by size class, and optional hourly prices for
//! VMs, vCPUs, memory and macOS guests on shared hosts. A lease's hourly price is fixed when it starts (stored on the
//! lease), so changing prices never re-prices a running lease. VM charges use the current list.
//!
//! Amounts are computed in micro-units of the currency (1e-6) with integer arithmetic and rounded to cents only for
//! display, so a month of statements adds up exactly.

use crate::tenancy::{Lease, LeaseState, TenancyState};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// One size class of Mac and its hourly lease price. A Mac falls in the largest class it meets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacClass {
    pub name: String,
    #[serde(default)]
    pub min_vcpus: u32,
    #[serde(default)]
    pub min_memory_gib: u64,
    pub per_hour: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceList {
    /// ISO 4217 code, shown with every amount.
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Leased Macs, by size; the largest class a Mac meets applies.
    #[serde(default)]
    pub mac_classes: Vec<MacClass>,
    /// A leased Mac that meets no class.
    #[serde(default)]
    pub mac_per_hour: f64,
    /// VMs on shared hosts (not on a Mac the tenant leases, unless `charge_vms_on_leased_macs`).
    #[serde(default)]
    pub vm_per_hour: f64,
    #[serde(default)]
    pub vcpu_per_hour: f64,
    #[serde(default)]
    pub memory_gib_per_hour: f64,
    /// On top of the above, for each macOS guest on a shared host.
    #[serde(default)]
    pub macos_vm_per_hour: f64,
    /// Also charge VM prices for VMs on a Mac the tenant leases (normally the lease covers them).
    #[serde(default)]
    pub charge_vms_on_leased_macs: bool,
}

fn default_currency() -> String {
    "USD".into()
}

impl Default for PriceList {
    fn default() -> Self {
        PriceList {
            currency: default_currency(),
            mac_classes: Vec::new(),
            mac_per_hour: 0.0,
            vm_per_hour: 0.0,
            vcpu_per_hour: 0.0,
            memory_gib_per_hour: 0.0,
            macos_vm_per_hour: 0.0,
            charge_vms_on_leased_macs: false,
        }
    }
}

/// A price in micro-units.
fn micros(price: f64) -> i64 {
    if price.is_finite() && price > 0.0 {
        (price * 1_000_000.0).round() as i64
    } else {
        0
    }
}

/// `rate` (micro-units per hour) for `secs` seconds.
fn cost(rate: i64, secs: i64) -> i64 {
    ((rate as i128 * secs.max(0) as i128) / 3600) as i64
}

impl PriceList {
    /// Checks a list before it is stored.
    pub fn validate(&self) -> Result<(), String> {
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err("currency must be a three-letter ISO code such as USD".into());
        }
        let prices = [
            self.mac_per_hour,
            self.vm_per_hour,
            self.vcpu_per_hour,
            self.memory_gib_per_hour,
            self.macos_vm_per_hour,
        ];
        if prices
            .iter()
            .chain(self.mac_classes.iter().map(|c| &c.per_hour))
            .any(|p| !p.is_finite() || *p < 0.0 || *p > 1_000_000.0)
        {
            return Err("prices must be between 0 and 1,000,000 per hour".into());
        }
        if self
            .mac_classes
            .iter()
            .any(|c| c.name.trim().is_empty() || c.name.chars().count() > 64)
        {
            return Err("every Mac class needs a name of at most 64 characters".into());
        }
        Ok(())
    }

    /// The class and hourly price (micro-units) of a Mac of this size.
    pub fn mac_rate(&self, vcpus: u32, memory_mib: u64) -> (Option<String>, i64) {
        self.mac_classes
            .iter()
            .filter(|c| vcpus >= c.min_vcpus && memory_mib >= c.min_memory_gib.saturating_mul(1024))
            .max_by_key(|c| (c.min_vcpus, c.min_memory_gib))
            .map(|c| (Some(c.name.clone()), micros(c.per_hour)))
            .unwrap_or((None, micros(self.mac_per_hour)))
    }

    /// The hourly price (micro-units) of one VM on a shared host.
    pub fn vm_rate(&self, vcpus: u32, memory_mib: u64, macos: bool) -> i64 {
        micros(self.vm_per_hour)
            + micros(self.vcpu_per_hour) * i64::from(vcpus)
            + (micros(self.memory_gib_per_hour) as i128 * memory_mib as i128 / 1024) as i64
            + if macos {
                micros(self.macos_vm_per_hour)
            } else {
                0
            }
    }

    pub fn is_free(&self) -> bool {
        self.mac_classes.iter().all(|c| c.per_hour == 0.0)
            && [
                self.mac_per_hour,
                self.vm_per_hour,
                self.vcpu_per_hour,
                self.memory_gib_per_hour,
                self.macos_vm_per_hour,
            ]
            .iter()
            .all(|p| *p == 0.0)
    }
}

/// One line of a statement.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    pub description: String,
    /// Hours (or GiB-hours, vCPU-hours).
    pub quantity: f64,
    pub unit: &'static str,
    /// Per unit, in currency units.
    pub unit_price: f64,
    pub amount_micros: i64,
    /// `amount_micros` rounded to cents.
    pub amount: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Budget {
    pub monthly: f64,
    /// Spend so far this month over the budget.
    pub used: f64,
    /// `ok`, `near` (80% or more) or `over`.
    pub state: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Statement {
    pub tenant: String,
    pub currency: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub lines: Vec<Line>,
    pub total_micros: i64,
    pub total: f64,
    /// For the current month: the total if spending continues at the same rate to the month's end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecast: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
}

fn round_cents(m: i64) -> f64 {
    (m as f64 / 10_000.0).round() / 100.0
}

fn line(description: String, secs: i64, units: f64, unit: &'static str, rate: i64) -> Line {
    let amount = cost(rate, secs);
    Line {
        description,
        quantity: (secs as f64 / 3600.0 * units * 100.0).round() / 100.0,
        unit,
        unit_price: rate as f64 / 1_000_000.0 / if units > 0.0 { units } else { 1.0 },
        amount_micros: amount,
        amount: round_cents(amount),
    }
}

fn overlap(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> i64 {
    (end.min(to) - start.max(from)).num_seconds().max(0)
}

/// The first instant of the month `t` falls in, and of the next.
pub fn month_of(t: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc
        .with_ymd_and_hms(t.year(), t.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(t);
    let (y, m) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    let end = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single().unwrap_or(t);
    (start, end)
}

/// Parses `2026-10` into that month's bounds.
pub fn parse_month(s: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (y, m) = s.split_once('-')?;
    let (y, m): (i32, u32) = (y.parse().ok()?, m.parse().ok()?);
    let t = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single()?;
    Some(month_of(t))
}

fn lease_rate(prices: &PriceList, l: &Lease) -> i64 {
    l.rate_micros.unwrap_or_else(|| micros(prices.mac_per_hour))
}

/// `tenant`'s statement for `[from, to)`: a line per lease, then VM time on shared hosts.
pub fn statement(
    state: &TenancyState,
    tenant: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Statement {
    let prices = &state.prices;
    let mut lines = Vec::new();
    for l in state.leases.iter().filter(|l| l.tenant == tenant) {
        let secs = overlap(l.started_at, l.billable_end(now), from, to);
        if secs == 0 {
            continue;
        }
        let class = l
            .price_class
            .as_deref()
            .map(|c| format!(", {c}"))
            .unwrap_or_default();
        let released = if l.state == LeaseState::Released {
            " (released)"
        } else {
            ""
        };
        lines.push(line(
            format!("Mac {} lease{class}{released}", l.node),
            secs,
            1.0,
            "hours",
            lease_rate(prices, l),
        ));
    }
    // VM time, on shared hosts only unless the price list says otherwise: the part of a VM's life that falls in one
    // of this tenant's leases of its node is covered by the lease; the rest (before the lease, after it) is not.
    let leased_secs = |node: &str, s: DateTime<Utc>, e: DateTime<Utc>| {
        let mut spans: Vec<(DateTime<Utc>, DateTime<Utc>)> = state
            .leases
            .iter()
            .filter(|l| l.tenant == tenant && l.node == node)
            .map(|l| (l.started_at.max(s), l.billable_end(now).min(e)))
            .filter(|(a, b)| a < b)
            .collect();
        spans.sort();
        let (mut total, mut reach) = (0i64, s);
        for (a, b) in spans {
            let a = a.max(reach);
            if b > a {
                total += (b - a).num_seconds();
                reach = b;
            }
        }
        total
    };
    let (mut vm, mut vcpu, mut mem, mut mac) = (0i64, 0i64, 0i64, 0i64);
    let mut mem_cost = 0i64;
    for v in state
        .vms
        .iter()
        .filter(|v| v.tenant.as_deref() == Some(tenant))
    {
        let end = v.ended_at.unwrap_or(now);
        let (s, e) = (v.created_at.max(from), end.min(to));
        let mut secs = overlap(v.created_at, end, from, to);
        if secs > 0 && !prices.charge_vms_on_leased_macs {
            secs -= leased_secs(&v.node, s, e);
        }
        if secs <= 0 {
            continue;
        }
        vm += secs;
        vcpu += secs * i64::from(v.vcpus);
        mem += (secs as i128 * v.memory_mib as i128 / 1024) as i64;
        mem_cost += cost(
            (micros(prices.memory_gib_per_hour) as i128 * v.memory_mib as i128 / 1024) as i64,
            secs,
        );
        if v.macos_guest {
            mac += secs;
        }
    }
    let mut push = |desc: &str, secs: i64, unit: &'static str, rate: f64| {
        if secs > 0 && rate > 0.0 {
            lines.push(line(desc.into(), secs, 1.0, unit, micros(rate)));
        }
    };
    push("VMs on shared hosts", vm, "VM-hours", prices.vm_per_hour);
    push(
        "vCPUs on shared hosts",
        vcpu,
        "vCPU-hours",
        prices.vcpu_per_hour,
    );
    push(
        "macOS guests on shared hosts",
        mac,
        "VM-hours",
        prices.macos_vm_per_hour,
    );
    if mem > 0 && prices.memory_gib_per_hour > 0.0 {
        lines.push(Line {
            description: "Memory on shared hosts".into(),
            quantity: (mem as f64 / 3600.0 * 100.0).round() / 100.0,
            unit: "GiB-hours",
            unit_price: prices.memory_gib_per_hour,
            amount_micros: mem_cost,
            amount: round_cents(mem_cost),
        });
    }
    let total: i64 = lines.iter().map(|l| l.amount_micros).sum();
    let (month_start, month_end) = month_of(now);
    let current = from == month_start && to >= now && now < month_end;
    let forecast = current.then(|| {
        let elapsed = (now - month_start).num_seconds().max(1);
        let whole = (month_end - month_start).num_seconds();
        round_cents((total as i128 * whole as i128 / elapsed as i128) as i64)
    });
    let budget = state
        .tenants
        .get(tenant)
        .and_then(|t| t.monthly_budget)
        .filter(|b| *b > 0.0)
        .map(|monthly| {
            let used = round_cents(total) / monthly;
            Budget {
                monthly,
                used: (used * 1000.0).round() / 1000.0,
                state: if used >= 1.0 {
                    "over"
                } else if used >= 0.8 {
                    "near"
                } else {
                    "ok"
                },
            }
        });
    Statement {
        tenant: tenant.into(),
        currency: prices.currency.clone(),
        from,
        to,
        lines,
        total_micros: total,
        total: round_cents(total),
        forecast,
        budget,
    }
}

/// A quoted CSV field. Text a spreadsheet would read as a formula (`=`, `+`, `-`, `@`, tab, CR) gets a leading `'`.
fn csv_field(s: &str) -> String {
    let guard = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        "'"
    } else {
        ""
    };
    format!("\"{guard}{}\"", s.replace('"', "\"\""))
}

/// The header line of [`Statement::to_csv`].
pub const CSV_HEADER: &str =
    "tenant,from,to,description,quantity,unit,unit_price,amount,currency\n";

impl Statement {
    pub fn to_csv(&self) -> String {
        let mut out = String::from(CSV_HEADER);
        let q = csv_field;
        for l in &self.lines {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{:.2},{}\n",
                q(&self.tenant),
                self.from.to_rfc3339(),
                self.to.to_rfc3339(),
                q(&l.description),
                l.quantity,
                l.unit,
                l.unit_price,
                l.amount,
                self.currency
            ));
        }
        out.push_str(&format!(
            "{},{},{},\"Total\",,,,{:.2},{}\n",
            q(&self.tenant),
            self.from.to_rfc3339(),
            self.to.to_rfc3339(),
            self.total,
            self.currency
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{Quota, VmEntry};
    use chrono::Duration;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn prices() -> PriceList {
        PriceList {
            mac_classes: vec![
                MacClass {
                    name: "Mac mini".into(),
                    min_vcpus: 8,
                    min_memory_gib: 16,
                    per_hour: 0.80,
                },
                MacClass {
                    name: "Mac Studio".into(),
                    min_vcpus: 20,
                    min_memory_gib: 64,
                    per_hour: 1.80,
                },
            ],
            mac_per_hour: 0.50,
            vcpu_per_hour: 0.02,
            memory_gib_per_hour: 0.005,
            ..Default::default()
        }
    }

    #[test]
    fn classes_pick_the_largest_met() {
        let p = prices();
        assert_eq!(
            p.mac_rate(24, 192 * 1024),
            (Some("Mac Studio".into()), 1_800_000)
        );
        assert_eq!(
            p.mac_rate(12, 32 * 1024),
            (Some("Mac mini".into()), 800_000)
        );
        assert_eq!(p.mac_rate(4, 8 * 1024), (None, 500_000));
        assert_eq!(p.vm_rate(4, 8192, false), 4 * 20_000 + 8 * 5_000);
        assert!(PriceList::default().is_free());
        assert!(p.validate().is_ok());
        assert!(
            PriceList {
                currency: "usd".into(),
                ..p.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            PriceList {
                vcpu_per_hour: -1.0,
                ..p
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn statement_prices_leases_at_their_start_and_vms_on_shared_hosts() {
        let mut s = TenancyState::default();
        s.prices = prices();
        s.create_tenant("acme", None, Quota::default(), t0())
            .unwrap();
        // A Studio leased for 30 hours at the price of the day.
        let l = s.create_lease("acme", "studio-1", None, t0()).unwrap();
        s.price_lease(&l.id, 24, 192 * 1024);
        // Prices go up afterwards: the running lease keeps its price.
        s.prices.mac_classes[1].per_hour = 9.0;
        // A VM on the leased Mac (free) and one on a shared host (4 vCPUs, 8 GiB, 10 hours).
        for (node, id) in [("studio-1", "a"), ("shared-1", "b")] {
            s.vms.push(VmEntry {
                node: node.into(),
                vm_id: id.into(),
                name: id.into(),
                tenant: Some("acme".into()),
                vcpus: 4,
                memory_mib: 8192,
                macos_guest: false,
                created_at: t0(),
                ended_at: Some(t0() + Duration::hours(10)),
                spec: None,
            });
        }
        let now = t0() + Duration::hours(30);
        let (from, to) = month_of(now);
        let st = statement(&s, "acme", from, to, now);
        assert_eq!(st.lines[0].amount, 54.0, "{st:?}"); // 30 h × 1.80
        assert!(st.lines[0].description.contains("Mac Studio"));
        let vcpu = st.lines.iter().find(|l| l.unit == "vCPU-hours").unwrap();
        assert_eq!((vcpu.quantity, vcpu.amount), (40.0, 0.8));
        let mem = st.lines.iter().find(|l| l.unit == "GiB-hours").unwrap();
        assert_eq!((mem.quantity, mem.amount), (80.0, 0.4));
        assert_eq!(st.total, 55.2);
        assert!(st.forecast.unwrap() > st.total);
        assert!(st.to_csv().contains("\"Total\",,,,55.20,USD"));

        // Budgets.
        s.tenants.get_mut("acme").unwrap().monthly_budget = Some(60.0);
        assert_eq!(
            statement(&s, "acme", from, to, now).budget.unwrap().state,
            "near"
        );
        s.tenants.get_mut("acme").unwrap().monthly_budget = Some(50.0);
        assert_eq!(
            statement(&s, "acme", from, to, now).budget.unwrap().state,
            "over"
        );
    }

    #[test]
    fn a_vm_is_free_only_while_its_mac_is_leased() {
        let mut s = TenancyState::default();
        s.prices = prices();
        s.create_tenant("acme", None, Quota::default(), t0())
            .unwrap();
        // The VM runs 48 hours on mac-1; the tenant leases mac-1 for the 24 hours in the middle.
        s.vms.push(VmEntry {
            node: "mac-1".into(),
            vm_id: "a".into(),
            name: "a".into(),
            tenant: Some("acme".into()),
            vcpus: 1,
            memory_mib: 0,
            macos_guest: false,
            created_at: t0(),
            ended_at: Some(t0() + Duration::hours(48)),
            spec: None,
        });
        let l = s
            .create_lease("acme", "mac-1", None, t0() + Duration::hours(12))
            .unwrap();
        s.release_lease(&l.id, None, false, t0() + Duration::hours(36))
            .unwrap();
        let now = t0() + Duration::hours(50);
        let (from, to) = month_of(now);
        let st = statement(&s, "acme", from, to, now);
        let vcpu = st.lines.iter().find(|l| l.unit == "vCPU-hours").unwrap();
        assert_eq!(vcpu.quantity, 24.0, "{st:?}");
        assert!(st.forecast.is_some());
    }

    #[test]
    fn csv_fields_never_start_a_formula() {
        assert_eq!(csv_field("=cmd()"), "\"'=cmd()\"");
        assert_eq!(csv_field("a \"b\""), "\"a \"\"b\"\"\"");
        assert_eq!(csv_field("acme"), "\"acme\"");
    }

    #[test]
    fn months() {
        let (a, b) = parse_month("2026-12").unwrap();
        assert_eq!(a, Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap());
        assert_eq!(b, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
        assert!(parse_month("2026-13").is_none());
        assert!(parse_month("soon").is_none());
    }
}
