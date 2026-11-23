// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A boot-time and density benchmark report format, and the comparison of two reports, for
//! `fluxctl bench-report check|compare`.
//!
//! The repo's bench scripts (`scripts/bench-*.sh`) print single averages. A single average hides tail latency, and
//! two numbers are only comparable when the same clock was used on the same kind of guest, so this format keeps the
//! raw samples together with the method that produced them (where the clock starts and stops, page-cache state,
//! guest image, backend, density per vCPU). `compare` refuses to call two reports comparable when the method
//! differs, computes nearest-rank p50/p95/p99, and flags a regression only when the change exceeds a threshold.
//! It reads files; it never runs a benchmark or a VM. Methodology notes and sources: `docs/RESEARCH.md`.
//!
//! Hostile input: sample counts, series counts and string lengths are capped; non-finite and negative samples
//! are errors, not skipped.

use crate::config_lint::{Finding, Severity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "fluxvm-bench/1";
pub const MAX_DOC_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SERIES: usize = 64;
pub const MAX_SAMPLES: usize = 200_000;
const MAX_STR: usize = 128;
/// A percentile needs this many samples to mean anything: p99 of fewer than 100 samples is just the maximum.
const MIN_FOR_P99: usize = 100;
const MIN_SAMPLES: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BenchReport {
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub host: HostInfo,
    #[serde(default)]
    pub method: Method,
    #[serde(default)]
    pub series: BTreeMap<String, Series>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HostInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cpu_model: String,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Method {
    /// Event that starts the clock, e.g. `api_create`, `vmm_spawn`.
    #[serde(default)]
    pub clock_start: String,
    /// Event that stops it, e.g. `guest_ready`, `ssh_login`, `vsock_ping`.
    #[serde(default)]
    pub clock_stop: String,
    /// `warm` or `cold`: whether the image and snapshot files were in the page cache.
    #[serde(default)]
    pub page_cache: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub guest_image: String,
    /// VMs per host vCPU while measuring (density), if the run was loaded.
    #[serde(default)]
    pub density_per_vcpu: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Series {
    #[serde(default)]
    pub unit: String,
    /// Default true: smaller is better (latency). Set false for throughput or VMs per host.
    #[serde(default = "yes")]
    pub lower_is_better: bool,
    #[serde(default)]
    pub samples: Vec<f64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

/// Nearest-rank percentile of sorted data (`p` in 0..=100).
fn rank(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let idx = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[idx.clamp(1, n) - 1]
}

pub fn stats(samples: &[f64]) -> Option<Stats> {
    if samples.is_empty() || samples.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    Some(Stats {
        n: s.len(),
        min: s[0],
        p50: rank(&s, 50.0),
        p95: rank(&s, 95.0),
        p99: rank(&s, 99.0),
        max: s[s.len() - 1],
        mean: s.iter().sum::<f64>() / s.len() as f64,
    })
}

fn fi(sev: Severity, key: &str, message: String, fix: Option<&str>) -> Finding {
    Finding {
        severity: sev,
        key: key.to_string(),
        message,
        fix: fix.map(str::to_string),
    }
}

fn clip(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_STR)
        .collect()
}

/// Parses a report. Errors are one-line strings for the CLI.
pub fn parse(raw: &str) -> Result<BenchReport, String> {
    // serde accepts a JSON array for a struct (fields by position); a report is an object.
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("not a benchmark report: {e}"))?;
    if !v.is_object() {
        return Err("not a benchmark report: the file must hold one JSON object".into());
    }
    serde_json::from_value(v).map_err(|e| format!("not a benchmark report: {e}"))
}

/// Format and methodology checks of one report.
pub fn validate(r: &BenchReport) -> Vec<Finding> {
    let mut out = Vec::new();
    if r.schema != SCHEMA {
        out.push(fi(
            Severity::Error,
            "schema",
            format!("schema is `{}`, expected `{SCHEMA}`", clip(&r.schema)),
            Some("set \"schema\": \"fluxvm-bench/1\""),
        ));
    }
    if r.series.is_empty() {
        out.push(fi(
            Severity::Error,
            "series",
            "the report has no series".into(),
            None,
        ));
    }
    if r.series.len() > MAX_SERIES {
        out.push(fi(
            Severity::Error,
            "series",
            format!("{} series; the limit is {MAX_SERIES}", r.series.len()),
            None,
        ));
    }
    for (what, v) in [
        ("method.clock_start", &r.method.clock_start),
        ("method.clock_stop", &r.method.clock_stop),
        ("method.backend", &r.method.backend),
        ("method.guest_image", &r.method.guest_image),
    ] {
        if v.trim().is_empty() {
            out.push(fi(
                Severity::Warning,
                what,
                "not given, so this report cannot be compared safely with another one".into(),
                Some("record where the clock starts and stops, the backend and the guest image"),
            ));
        }
    }
    if !matches!(r.method.page_cache.as_str(), "warm" | "cold") {
        out.push(fi(
            Severity::Warning,
            "method.page_cache",
            format!("`{}`; use `warm` or `cold`", clip(&r.method.page_cache)),
            None,
        ));
    }
    if let Some(d) = r.method.density_per_vcpu
        && (!d.is_finite() || d <= 0.0)
    {
        out.push(fi(
            Severity::Error,
            "method.density_per_vcpu",
            "must be a positive number".into(),
            None,
        ));
    }
    for (name, s) in r.series.iter().take(MAX_SERIES) {
        let key = format!("series.{}", clip(name));
        if s.unit.trim().is_empty() {
            out.push(fi(Severity::Warning, &key, "no unit".into(), None));
        }
        if s.samples.len() > MAX_SAMPLES {
            out.push(fi(
                Severity::Error,
                &key,
                format!("{} samples; the limit is {MAX_SAMPLES}", s.samples.len()),
                None,
            ));
            continue;
        }
        if s.samples.iter().any(|x| !x.is_finite() || *x < 0.0) {
            out.push(fi(
                Severity::Error,
                &key,
                "samples must be finite and not negative".into(),
                None,
            ));
            continue;
        }
        if s.samples.len() < MIN_SAMPLES {
            out.push(fi(
                Severity::Warning,
                &key,
                format!(
                    "{} sample(s); under {MIN_SAMPLES} the percentiles are noise",
                    s.samples.len()
                ),
                None,
            ));
        } else if s.samples.len() < MIN_FOR_P99 {
            out.push(fi(
                Severity::Info,
                &key,
                format!(
                    "{} samples; p99 is the maximum until there are {MIN_FOR_P99}",
                    s.samples.len()
                ),
                None,
            ));
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct SeriesDelta {
    pub series: String,
    pub unit: String,
    pub base: Stats,
    pub new: Stats,
    /// Signed change of p50 and p99 in percent of the base (positive = value went up).
    pub p50_change_pct: f64,
    pub p99_change_pct: f64,
    pub verdict: DeltaVerdict,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeltaVerdict {
    Regression,
    Improvement,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    pub comparable: bool,
    pub threshold_pct: f64,
    pub deltas: Vec<SeriesDelta>,
    pub findings: Vec<Finding>,
}

impl Comparison {
    pub fn regressions(&self) -> usize {
        self.deltas
            .iter()
            .filter(|d| d.verdict == DeltaVerdict::Regression)
            .count()
    }
}

fn pct(base: f64, new: f64) -> f64 {
    if base == 0.0 {
        if new == 0.0 { 0.0 } else { f64::INFINITY }
    } else {
        (new - base) / base * 100.0
    }
}

/// Compares `new` with `base`. A series regresses when its p50 or p99 gets worse by more than `threshold_pct`
/// (a p99 from fewer than 100 samples is not used). Differences in the method make the reports not comparable.
pub fn compare(base: &BenchReport, new: &BenchReport, threshold_pct: f64) -> Comparison {
    let mut findings = Vec::new();
    for (side, r) in [("base", base), ("new", new)] {
        for mut f in validate(r) {
            if f.severity == Severity::Error {
                f.key = format!("{side}.{}", f.key);
                findings.push(f);
            }
        }
    }
    let mut comparable = findings.is_empty();
    let (b, n) = (&base.method, &new.method);
    for (key, x, y) in [
        ("method.clock_start", &b.clock_start, &n.clock_start),
        ("method.clock_stop", &b.clock_stop, &n.clock_stop),
        ("method.backend", &b.backend, &n.backend),
        ("method.guest_image", &b.guest_image, &n.guest_image),
        ("method.page_cache", &b.page_cache, &n.page_cache),
    ] {
        if x != y {
            comparable = false;
            findings.push(fi(
                Severity::Warning,
                key,
                format!(
                    "differs: `{}` vs `{}`; the numbers measure different things",
                    clip(x),
                    clip(y)
                ),
                None,
            ));
        }
    }
    if b.density_per_vcpu != n.density_per_vcpu {
        comparable = false;
        findings.push(fi(
            Severity::Warning,
            "method.density_per_vcpu",
            format!(
                "differs: {:?} vs {:?}; tail latency changes sharply with load",
                b.density_per_vcpu, n.density_per_vcpu
            ),
            None,
        ));
    }
    if (&base.host.cpu_model, base.host.vcpus) != (&new.host.cpu_model, new.host.vcpus) {
        findings.push(fi(
            Severity::Info,
            "host",
            "the two runs are on different CPUs or vCPU counts; read the change as a host difference as much as a software one".into(),
            None,
        ));
    }
    let mut deltas = Vec::new();
    for (name, bs) in base.series.iter().take(MAX_SERIES) {
        let key = format!("series.{}", clip(name));
        if bs.samples.len() > MAX_SAMPLES
            || new
                .series
                .get(name)
                .is_some_and(|n| n.samples.len() > MAX_SAMPLES)
        {
            continue; // already reported as an error by validate()
        }
        let Some(ns) = new.series.get(name) else {
            findings.push(fi(
                Severity::Warning,
                &key,
                "missing from the new report".into(),
                None,
            ));
            continue;
        };
        if bs.unit != ns.unit {
            comparable = false;
            findings.push(fi(
                Severity::Error,
                &key,
                format!("unit differs: `{}` vs `{}`", clip(&bs.unit), clip(&ns.unit)),
                None,
            ));
            continue;
        }
        if bs.lower_is_better != ns.lower_is_better {
            comparable = false;
            findings.push(fi(
                Severity::Error,
                &key,
                "lower_is_better differs".into(),
                None,
            ));
            continue;
        }
        let (Some(sb), Some(sn)) = (stats(&bs.samples), stats(&ns.samples)) else {
            findings.push(fi(
                Severity::Warning,
                &key,
                "no usable samples on one side".into(),
                None,
            ));
            continue;
        };
        let p50 = pct(sb.p50, sn.p50);
        let p99 = pct(sb.p99, sn.p99);
        let use_p99 = sb.n >= MIN_FOR_P99 && sn.n >= MIN_FOR_P99;
        // "worse" is a rise when lower is better, a fall otherwise.
        let sign = if bs.lower_is_better { 1.0 } else { -1.0 };
        let worst = (sign * p50).max(if use_p99 {
            sign * p99
        } else {
            f64::NEG_INFINITY
        });
        let best = (sign * p50).min(if use_p99 { sign * p99 } else { f64::INFINITY });
        let verdict = if worst > threshold_pct {
            DeltaVerdict::Regression
        } else if best < -threshold_pct && worst <= 0.0 {
            DeltaVerdict::Improvement
        } else {
            DeltaVerdict::Unchanged
        };
        if !use_p99 {
            findings.push(fi(
                Severity::Info,
                &key,
                format!("fewer than {MIN_FOR_P99} samples on a side; only p50 decides the verdict"),
                None,
            ));
        }
        deltas.push(SeriesDelta {
            series: clip(name),
            unit: clip(&bs.unit),
            base: sb,
            new: sn,
            p50_change_pct: p50,
            p99_change_pct: p99,
            verdict,
        });
    }
    for name in new.series.keys().take(MAX_SERIES) {
        if !base.series.contains_key(name) {
            findings.push(fi(
                Severity::Info,
                &format!("series.{}", clip(name)),
                "new in this report; nothing to compare".into(),
                None,
            ));
        }
    }
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.key.cmp(&b.key)));
    Comparison {
        comparable,
        threshold_pct,
        deltas,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(samples: Vec<f64>) -> BenchReport {
        let mut series = BTreeMap::new();
        series.insert(
            "boot_ms".to_string(),
            Series {
                unit: "ms".into(),
                lower_is_better: true,
                samples,
            },
        );
        BenchReport {
            schema: SCHEMA.into(),
            name: "t".into(),
            host: HostInfo {
                cpu_model: "x".into(),
                vcpus: 8,
                ..Default::default()
            },
            method: Method {
                clock_start: "api_create".into(),
                clock_stop: "guest_ready".into(),
                page_cache: "warm".into(),
                backend: "firecracker".into(),
                guest_image: "ubuntu-24.04".into(),
                density_per_vcpu: None,
            },
            series,
        }
    }
    fn ramp(n: usize, base: f64) -> Vec<f64> {
        (0..n).map(|i| base + i as f64 * 0.1).collect()
    }

    #[test]
    fn nearest_rank_percentiles() {
        let s = stats(&(1..=100).map(f64::from).collect::<Vec<_>>()).unwrap();
        assert_eq!(
            (s.p50, s.p95, s.p99, s.max, s.n),
            (50.0, 95.0, 99.0, 100.0, 100)
        );
        let one = stats(&[7.0]).unwrap();
        assert_eq!((one.p50, one.p99), (7.0, 7.0));
        assert!(stats(&[]).is_none());
        assert!(stats(&[1.0, f64::NAN]).is_none());
    }

    #[test]
    fn validate_flags_bad_reports() {
        let mut r = report(vec![1.0; 200]);
        assert!(validate(&r).iter().all(|f| f.severity != Severity::Error));
        r.schema = "x".into();
        r.series.get_mut("boot_ms").unwrap().samples[3] = -1.0;
        let f = validate(&r);
        assert_eq!(
            f.iter().filter(|f| f.severity == Severity::Error).count(),
            2
        );
        let mut r = report(vec![1.0; 3]);
        r.method.clock_stop.clear();
        let f = validate(&r);
        assert!(f.iter().any(|f| f.key == "method.clock_stop"));
        assert!(f.iter().any(|f| f.message.contains("noise")));
    }

    #[test]
    fn oversize_series_is_an_error_not_a_hang() {
        let r = report(vec![1.0; MAX_SAMPLES + 1]);
        assert!(validate(&r).iter().any(|f| f.severity == Severity::Error));
        let c = compare(&r, &r, 10.0);
        assert!(!c.comparable && c.deltas.is_empty());
    }

    #[test]
    fn detects_regression_improvement_and_noise() {
        let base = report(ramp(200, 100.0));
        let slow = report(ramp(200, 130.0));
        let c = compare(&base, &slow, 10.0);
        assert!(c.comparable);
        assert_eq!(c.regressions(), 1);
        assert_eq!(c.deltas[0].verdict, DeltaVerdict::Regression);
        let fast = report(ramp(200, 70.0));
        assert_eq!(
            compare(&base, &fast, 10.0).deltas[0].verdict,
            DeltaVerdict::Improvement
        );
        let same = report(ramp(200, 101.0));
        assert_eq!(
            compare(&base, &same, 10.0).deltas[0].verdict,
            DeltaVerdict::Unchanged
        );
    }

    #[test]
    fn tail_only_regression_is_caught_with_enough_samples() {
        let base = report(vec![100.0; 200]);
        let mut s = vec![100.0; 200];
        for v in s.iter_mut().take(10) {
            *v = 500.0;
        }
        let c = compare(&base, &report(s), 10.0);
        assert_eq!(c.deltas[0].verdict, DeltaVerdict::Regression);
        assert!(c.deltas[0].p99_change_pct > 100.0);
        // with 20 samples the tail is not used
        let b = report(vec![100.0; 20]);
        let mut s = vec![100.0; 20];
        s[0] = 900.0;
        assert_eq!(
            compare(&b, &report(s), 10.0).deltas[0].verdict,
            DeltaVerdict::Unchanged
        );
    }

    #[test]
    fn higher_is_better_series_invert() {
        let mut a = report(vec![100.0; 50]);
        let mut b = report(vec![80.0; 50]);
        for r in [&mut a, &mut b] {
            r.series.get_mut("boot_ms").unwrap().lower_is_better = false;
        }
        assert_eq!(
            compare(&a, &b, 10.0).deltas[0].verdict,
            DeltaVerdict::Regression
        );
    }

    #[test]
    fn different_method_is_not_comparable() {
        let a = report(vec![1.0; 50]);
        let mut b = a.clone();
        b.method.clock_stop = "ssh_login".into();
        let c = compare(&a, &b, 10.0);
        assert!(!c.comparable);
        assert!(c.findings.iter().any(|f| f.key == "method.clock_stop"));
        let mut b = a.clone();
        b.series.get_mut("boot_ms").unwrap().unit = "s".into();
        assert!(compare(&a, &b, 10.0).deltas.is_empty());
    }

    #[test]
    fn zero_baseline_does_not_divide_by_zero() {
        let c = compare(&report(vec![0.0; 50]), &report(vec![5.0; 50]), 10.0);
        assert_eq!(c.deltas[0].verdict, DeltaVerdict::Regression);
        assert!(c.deltas[0].p50_change_pct.is_infinite());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("[]").is_err());
        assert!(parse("{\"series\":{\"a\":{\"samples\":[\"x\"]}}}").is_err());
        assert!(parse("{}").is_ok());
    }
}
