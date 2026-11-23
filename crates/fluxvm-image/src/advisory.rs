// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Matching an SBOM against a locally cached advisory file. Offline by default: `<state_dir>/oci/advisories.json` is
//! the only data source, `fluxctl image scan --update` replaces it from a source you name, and nothing is ever
//! invented. When there is no file the report says so (`advisory.present = false`) instead of answering "clean", and
//! when there is one it says how old it is.
//!
//! Two input shapes are accepted: this crate's own (`{"format":"fluxvm-advisories-v1", "updated_unix", "source",
//! "advisories":[...]}`) and OSV records (one object, an array, or `{"vulns":[...]}`), whose `affected[].ranges` events
//! become "introduced"/"fixed" pairs. OSV `versions` lists and `last_affected` are not used.

use crate::sbom::{Package, Sbom};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "fluxvm-advisories-v1";
/// Largest advisory file accepted from `--update`.
pub const MAX_BYTES: usize = 256 * 1024 * 1024;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Unknown,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "high" | "important" => Self::High,
            "medium" | "moderate" => Self::Medium,
            "low" | "negligible" => Self::Low,
            "unknown" | "" => Self::Unknown,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

fn star() -> String {
    "*".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    pub id: String,
    pub package: String,
    /// `deb`, `apk`, `pypi`, `npm` or `*`.
    #[serde(default = "star")]
    pub ecosystem: String,
    #[serde(default)]
    pub severity: Severity,
    /// Affected from this version (inclusive). Absent: from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced: Option<String>,
    /// Fixed in this version. Absent: no fix is known, every later version is affected too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisoryFile {
    #[serde(default)]
    pub format: String,
    /// When the data was current (the feed's own date, else when it was fetched).
    pub updated_unix: u64,
    /// Where it came from: a URL or a file path.
    #[serde(default)]
    pub source: String,
    pub advisories: Vec<Advisory>,
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("oci").join("advisories.json")
}

/// The cached file, or `None` when there is none yet.
pub fn load(state_dir: &Path) -> Result<Option<AdvisoryFile>> {
    let p = path(state_dir);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
    };
    let mtime = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    parse(&bytes, mtime)
        .map(Some)
        .with_context(|| format!("{} is not a usable advisory file", p.display()))
}

pub fn save(state_dir: &Path, file: &AdvisoryFile) -> Result<()> {
    let p = path(state_dir);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension(format!("part-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(file)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Parse either input shape. `fallback_unix` dates data that does not carry its own date (an OSV dump).
pub fn parse(bytes: &[u8], fallback_unix: u64) -> Result<AdvisoryFile> {
    let v: serde_json::Value = serde_json::from_slice(bytes).context("the file is not JSON")?;
    if v.get("advisories").is_some_and(|a| a.is_array()) {
        let mut f: AdvisoryFile =
            serde_json::from_value(v).context("not a fluxvm advisory file")?;
        if f.updated_unix == 0 {
            f.updated_unix = fallback_unix;
        }
        f.format = FORMAT.into();
        return Ok(f);
    }
    let records: Vec<serde_json::Value> = if let Some(a) = v.as_array() {
        a.clone()
    } else if let Some(a) = v.get("vulns").and_then(|a| a.as_array()) {
        a.clone()
    } else if v.get("id").is_some() {
        vec![v]
    } else {
        bail!(
            "expected a fluxvm advisory file or OSV records (an object with id, an array, or {{\"vulns\": [...]}})"
        );
    };
    let mut advisories = Vec::new();
    for r in &records {
        osv_record(r, &mut advisories);
    }
    Ok(AdvisoryFile {
        format: FORMAT.into(),
        updated_unix: fallback_unix,
        source: String::new(),
        advisories,
    })
}

fn osv_ecosystem(name: &str) -> String {
    let base = name.split(':').next().unwrap_or(name).to_ascii_lowercase();
    match base.as_str() {
        "debian" | "ubuntu" => "deb".into(),
        "alpine" => "apk".into(),
        "pypi" => "pypi".into(),
        "npm" => "npm".into(),
        other => other.into(),
    }
}

fn osv_record(r: &serde_json::Value, out: &mut Vec<Advisory>) {
    let Some(id) = r.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    let summary = r
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let severity = r
        .pointer("/database_specific/severity")
        .and_then(|v| v.as_str())
        .and_then(Severity::parse)
        .unwrap_or_default();
    for aff in r
        .get("affected")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
    {
        let Some(name) = aff.pointer("/package/name").and_then(|v| v.as_str()) else {
            continue;
        };
        let eco = osv_ecosystem(
            aff.pointer("/package/ecosystem")
                .and_then(|v| v.as_str())
                .unwrap_or("*"),
        );
        for range in aff
            .get("ranges")
            .and_then(|a| a.as_array())
            .into_iter()
            .flatten()
        {
            if !matches!(
                range.get("type").and_then(|v| v.as_str()),
                Some("ECOSYSTEM" | "SEMVER")
            ) {
                continue;
            }
            let mut introduced: Option<String> = None;
            let mut open = false;
            let emit = |intro: &Option<String>, fixed: Option<String>, out: &mut Vec<Advisory>| {
                out.push(Advisory {
                    id: id.into(),
                    package: name.into(),
                    ecosystem: eco.clone(),
                    severity,
                    introduced: intro.clone().filter(|v| v != "0"),
                    fixed,
                    summary: summary.clone(),
                });
            };
            for ev in range
                .get("events")
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(i) = ev.get("introduced").and_then(|v| v.as_str()) {
                    if open {
                        emit(&introduced, None, out);
                    }
                    introduced = Some(i.into());
                    open = true;
                } else if let Some(f) = ev.get("fixed").and_then(|v| v.as_str()) {
                    emit(&introduced, Some(f.into()), out);
                    open = false;
                }
            }
            if open {
                emit(&introduced, None, out);
            }
        }
    }
}

// ---- versions ----

fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}

fn deb_order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(c) if digit(c) => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(b'~') => -1,
        Some(c) => i32::from(c) + 256,
    }
}

fn deb_part(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !digit(a[i])) || (j < b.len() && !digit(b[j])) {
            let (ac, bc) = (deb_order(a.get(i).copied()), deb_order(b.get(j).copied()));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let mut first = Ordering::Equal;
        while i < a.len() && digit(a[i]) && j < b.len() && digit(b[j]) {
            if first == Ordering::Equal {
                first = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if i < a.len() && digit(a[i]) {
            return Ordering::Greater;
        }
        if j < b.len() && digit(b[j]) {
            return Ordering::Less;
        }
        if first != Ordering::Equal {
            return first;
        }
    }
    Ordering::Equal
}

/// dpkg's version order (`epoch:upstream-revision`).
pub fn cmp_deb(a: &str, b: &str) -> Ordering {
    let split = |v: &str| -> (u64, String, String) {
        let (epoch, rest) = match v.split_once(':') {
            Some((e, r)) if e.bytes().all(digit) && !e.is_empty() => (e.parse().unwrap_or(0), r),
            _ => (0, v),
        };
        match rest.rsplit_once('-') {
            Some((u, r)) => (epoch, u.into(), r.into()),
            None => (epoch, rest.into(), String::new()),
        }
    };
    let (ea, ua, ra) = split(a);
    let (eb, ub, rb) = split(b);
    ea.cmp(&eb)
        .then_with(|| deb_part(&ua, &ub))
        .then_with(|| deb_part(&ra, &rb))
}

/// Dotted-number order with pre-release tags below the release (`1.0rc1 < 1.0`), for pypi and npm. An approximation of
/// PEP 440 and semver, not either of them.
pub fn cmp_generic(a: &str, b: &str) -> Ordering {
    fn tokens(v: &str) -> Vec<String> {
        let v = v.trim_start_matches('v');
        let v = v.split('+').next().unwrap_or(v);
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut num = false;
        for c in v.chars() {
            if c.is_ascii_alphanumeric() {
                if !cur.is_empty() && c.is_ascii_digit() != num {
                    out.push(std::mem::take(&mut cur));
                }
                num = c.is_ascii_digit();
                cur.push(c);
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
    let (ta, tb) = (tokens(a), tokens(b));
    for i in 0..ta.len().max(tb.len()) {
        match (ta.get(i), tb.get(i)) {
            (Some(x), Some(y)) => {
                let (nx, ny) = (x.parse::<u64>(), y.parse::<u64>());
                let o = match (nx, ny) {
                    (Ok(p), Ok(q)) => p.cmp(&q),
                    (Ok(_), Err(_)) => Ordering::Greater,
                    (Err(_), Ok(_)) => Ordering::Less,
                    _ => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), None) => {
                return if x.parse::<u64>().is_ok() {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (None, Some(y)) => {
                return if y.parse::<u64>().is_ok() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (None, None) => {}
        }
    }
    Ordering::Equal
}

pub fn cmp_version(ecosystem: &str, a: &str, b: &str) -> Ordering {
    match ecosystem {
        "deb" | "apk" => cmp_deb(a, b),
        _ => cmp_generic(a, b),
    }
}

fn same_name(eco: &str, a: &str, b: &str) -> bool {
    if eco == "pypi" {
        let n = |s: &str| s.to_ascii_lowercase().replace(['_', '.'], "-");
        n(a) == n(b)
    } else {
        a == b
    }
}

pub fn matches(adv: &Advisory, pkg: &Package) -> bool {
    (adv.ecosystem == "*" || adv.ecosystem.eq_ignore_ascii_case(&pkg.ecosystem))
        && same_name(&pkg.ecosystem, &adv.package, &pkg.name)
        && adv
            .introduced
            .as_deref()
            .is_none_or(|i| cmp_version(&pkg.ecosystem, &pkg.version, i) != Ordering::Less)
        && adv
            .fixed
            .as_deref()
            .is_none_or(|f| cmp_version(&pkg.ecosystem, &pkg.version, f) == Ordering::Less)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub package: String,
    pub version: String,
    pub ecosystem: String,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataStatus {
    /// False when there is no advisory file at all: nothing was checked.
    pub present: bool,
    #[serde(default)]
    pub updated_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_days: Option<u64>,
    #[serde(default)]
    pub advisories: usize,
    #[serde(default)]
    pub source: String,
}

impl DataStatus {
    /// "advisory data is 12 days old", or the plain statement that there is none.
    pub fn describe(&self) -> String {
        match (self.present, self.age_days) {
            (false, _) => {
                "there is no advisory data on this machine, so nothing was checked".into()
            }
            (true, Some(0)) => format!(
                "advisory data is from today ({} advisories)",
                self.advisories
            ),
            (true, Some(1)) => format!(
                "advisory data is 1 day old ({} advisories)",
                self.advisories
            ),
            (true, Some(n)) => format!(
                "advisory data is {n} days old ({} advisories)",
                self.advisories
            ),
            (true, None) => "advisory data has no date".into(),
        }
    }
}

pub fn status(file: Option<&AdvisoryFile>, now: u64) -> DataStatus {
    match file {
        None => DataStatus {
            present: false,
            updated_unix: 0,
            age_days: None,
            advisories: 0,
            source: String::new(),
        },
        Some(f) => DataStatus {
            present: true,
            updated_unix: f.updated_unix,
            age_days: (f.updated_unix > 0).then(|| now.saturating_sub(f.updated_unix) / 86_400),
            advisories: f.advisories.len(),
            source: f.source.clone(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanReport {
    pub reference: String,
    pub manifest_digest: String,
    pub packages: usize,
    pub findings: Vec<Finding>,
    /// Findings per severity name.
    pub counts: std::collections::BTreeMap<String, usize>,
    pub advisory: DataStatus,
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ScanReport {
    /// The worst severity found, if any.
    pub fn worst(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }
}

pub fn scan(sbom: &Sbom, file: Option<&AdvisoryFile>, now: u64) -> ScanReport {
    let mut findings: Vec<Finding> = Vec::new();
    if let Some(f) = file {
        for pkg in &sbom.packages {
            for a in f.advisories.iter().filter(|a| matches(a, pkg)) {
                findings.push(Finding {
                    id: a.id.clone(),
                    package: pkg.name.clone(),
                    version: pkg.version.clone(),
                    ecosystem: pkg.ecosystem.clone(),
                    severity: a.severity,
                    fixed: a.fixed.clone(),
                    summary: a.summary.clone(),
                });
            }
        }
    }
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.package.cmp(&b.package))
            .then_with(|| a.id.cmp(&b.id))
    });
    findings.dedup_by(|a, b| a.id == b.id && a.package == b.package && a.version == b.version);
    let mut counts = std::collections::BTreeMap::new();
    for f in &findings {
        *counts.entry(f.severity.as_str().to_string()).or_insert(0) += 1;
    }
    let mut notes = sbom.notes.clone();
    if file.is_some() {
        notes.push("matching is by package name and version; Debian security data keyed by source package name can be missed".into());
    }
    ScanReport {
        reference: sbom.reference.clone(),
        manifest_digest: sbom.manifest_digest.clone(),
        packages: sbom.packages.len(),
        findings,
        counts,
        advisory: status(file, now),
        notes,
    }
}

/// Download an advisory source (an https URL, never a default: the caller names it).
pub async fn fetch(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} answered {}", resp.status());
    }
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_BYTES)
    {
        bail!("{url} is larger than {} MiB", MAX_BYTES >> 20);
    }
    let bytes = resp
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    if bytes.len() > MAX_BYTES {
        bail!("{url} is larger than {} MiB", MAX_BYTES >> 20);
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(eco: &str, name: &str, version: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            ecosystem: eco.into(),
            arch: None,
            purl: String::new(),
        }
    }

    fn adv(
        eco: &str,
        name: &str,
        intro: Option<&str>,
        fixed: Option<&str>,
        sev: Severity,
    ) -> Advisory {
        Advisory {
            id: "CVE-TEST-1".into(),
            package: name.into(),
            ecosystem: eco.into(),
            severity: sev,
            introduced: intro.map(Into::into),
            fixed: fixed.map(Into::into),
            summary: "s".into(),
        }
    }

    #[test]
    fn deb_versions_order_like_dpkg() {
        assert_eq!(cmp_deb("1.0-1", "1.0-2"), Ordering::Less);
        assert_eq!(cmp_deb("1:0.9", "2.0"), Ordering::Greater);
        assert_eq!(cmp_deb("1.0~rc1", "1.0"), Ordering::Less);
        assert_eq!(cmp_deb("3.0.11-1", "3.0.2-1"), Ordering::Greater);
        assert_eq!(cmp_deb("1.2.4-r2", "1.2.4-r10"), Ordering::Less);
        assert_eq!(cmp_deb("2.36-9", "2.36-9"), Ordering::Equal);
    }

    #[test]
    fn generic_versions_put_prereleases_first() {
        assert_eq!(cmp_generic("1.0.0-rc1", "1.0.0"), Ordering::Less);
        assert_eq!(cmp_generic("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(cmp_generic("2.31.0", "2.31.0"), Ordering::Equal);
        assert_eq!(cmp_generic("v1.2", "1.2.0"), Ordering::Less);
    }

    #[test]
    fn ranges_decide_who_is_affected() {
        let a = adv(
            "deb",
            "openssl",
            Some("3.0.0"),
            Some("3.0.12-1"),
            Severity::Critical,
        );
        assert!(matches(&a, &pkg("deb", "openssl", "3.0.11-1")));
        assert!(
            !matches(&a, &pkg("deb", "openssl", "3.0.12-1")),
            "the fixed version is not affected"
        );
        assert!(
            !matches(&a, &pkg("deb", "openssl", "2.9")),
            "before it was introduced"
        );
        assert!(
            !matches(&a, &pkg("apk", "openssl", "3.0.11-1")),
            "other ecosystem"
        );
        let unfixed = adv("*", "x", None, None, Severity::Low);
        assert!(matches(&unfixed, &pkg("npm", "x", "99.0.0")));
        let py = adv("pypi", "My_Pkg", None, Some("2.0"), Severity::High);
        assert!(matches(&py, &pkg("pypi", "my-pkg", "1.9")));
    }

    #[test]
    fn scan_without_data_says_nothing_was_checked() {
        let sbom = Sbom {
            reference: "a:1".into(),
            manifest_digest: "sha256:x".into(),
            architecture: "arm64".into(),
            distro: None,
            packages: vec![pkg("deb", "openssl", "1")],
            notes: vec![],
            generated_unix: 0,
        };
        let r = scan(&sbom, None, 100);
        assert!(!r.advisory.present);
        assert!(r.findings.is_empty());
        assert!(r.advisory.describe().contains("nothing was checked"));
    }

    #[test]
    fn scan_sorts_worst_first_and_reports_age() {
        let sbom = Sbom {
            reference: "a:1".into(),
            manifest_digest: "sha256:x".into(),
            architecture: "arm64".into(),
            distro: None,
            packages: vec![pkg("deb", "a", "1.0"), pkg("deb", "b", "1.0")],
            notes: vec![],
            generated_unix: 0,
        };
        let f = AdvisoryFile {
            format: FORMAT.into(),
            updated_unix: 1_000_000,
            source: "t".into(),
            advisories: vec![
                adv("deb", "a", None, Some("2.0"), Severity::Low),
                adv("deb", "b", None, Some("2.0"), Severity::Critical),
            ],
        };
        let r = scan(&sbom, Some(&f), 1_000_000 + 12 * 86_400 + 5);
        assert_eq!(r.findings[0].package, "b");
        assert_eq!(r.worst(), Some(Severity::Critical));
        assert_eq!(r.counts["low"], 1);
        assert_eq!(
            r.advisory.describe(),
            "advisory data is 12 days old (2 advisories)"
        );
    }

    #[test]
    fn osv_records_become_advisories() {
        let osv = serde_json::json!({
            "id": "GHSA-xxxx",
            "summary": "bad",
            "database_specific": {"severity": "CRITICAL"},
            "affected": [{
                "package": {"name": "left-pad", "ecosystem": "npm"},
                "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "1.3.1"}]}]
            }, {
                "package": {"name": "openssl", "ecosystem": "Debian:12"},
                "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}]}]
            }]
        });
        let f = parse(osv.to_string().as_bytes(), 42).unwrap();
        assert_eq!(f.updated_unix, 42);
        assert_eq!(f.advisories.len(), 2);
        assert_eq!(f.advisories[0].severity, Severity::Critical);
        assert_eq!(f.advisories[0].fixed.as_deref(), Some("1.3.1"));
        assert_eq!(f.advisories[0].introduced, None);
        assert_eq!(f.advisories[1].ecosystem, "deb");
        assert_eq!(f.advisories[1].fixed, None);
        assert!(parse(b"{\"x\":1}", 0).is_err());
        assert!(parse(b"nope", 0).is_err());
    }

    #[test]
    fn save_and_load_round_trip() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_none());
        let f = AdvisoryFile {
            format: FORMAT.into(),
            updated_unix: 7,
            source: "s".into(),
            advisories: vec![adv("*", "x", None, None, Severity::High)],
        };
        save(d.path(), &f).unwrap();
        assert_eq!(load(d.path()).unwrap().unwrap(), f);
    }
}
