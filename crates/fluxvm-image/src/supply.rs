// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The image policy and the one-line answer about an image: signed? vulnerable? allowed to run?
//!
//! Everything is local files under `<state_dir>/oci`:
//!
//! - `policy.toml`: the allow/deny file (this module). No file means no policy and nothing is blocked.
//! - `verify/<digest>.json`: the last result of `fluxctl image verify` (written by fluxctl, which owns the trust store).
//! - `advisories.json`: see [`crate::advisory`].
//! - `policy-overrides.log`: one JSON line for every run that went ahead against the policy.
//!
//! The daemon honours the policy when it starts an OCI sandbox ([`enforce`]); `fluxctl image policy check` and Studio use
//! the same [`decide`], so what they predict is what a run does.

use crate::advisory::{self, ScanReport, Severity};
use crate::oci_registry::BlobStore;
use crate::sbom::{self, Subject};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const POLICY_FILE: &str = "policy.toml";
pub const OVERRIDE_LOG: &str = "policy-overrides.log";

pub const CODE_DENIED: &str = "IMAGE_POLICY_DENIED";
pub const CODE_UNSIGNED: &str = "IMAGE_POLICY_UNSIGNED";
pub const CODE_VULNERABLE: &str = "IMAGE_POLICY_VULNERABLE";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    block_unsigned: bool,
    /// `critical`, `high`, `medium`, `low` or `none`.
    #[serde(default)]
    block_severity: Option<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default = "default_age")]
    max_advisory_age_days: u64,
}

fn default_age() -> u64 {
    30
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Refuse an image whose signature `fluxctl image verify` has not found trusted.
    pub block_unsigned: bool,
    /// Refuse an image with a finding at this severity or worse.
    pub block_severity: Option<Severity>,
    /// Reference patterns (`*` matches anything) that skip the two rules above. `deny` still wins.
    pub allow: Vec<String>,
    /// Reference patterns that are never run.
    pub deny: Vec<String>,
    /// Advisory data older than this earns a warning.
    pub max_advisory_age_days: u64,
}

pub const STARTER: &str = "\
# FluxVM image policy: what a container sandbox may start from.
# Checked every time a sandbox starts. Remove this file to turn the policy off.
# `fluxctl image policy check IMAGE` shows what would happen; `fluxctl image policy` shows this file as understood.

# Refuse images that `fluxctl image verify` has not found signed by a key you trust.
block_unsigned = true

# Refuse images with an advisory at this severity or worse: \"critical\", \"high\", \"medium\", \"low\" or \"none\".
# Needs advisory data: `fluxctl image scan IMAGE --update --source URL_OR_FILE`.
block_severity = \"critical\"

# Reference patterns (* matches anything). allow skips the two rules above; deny always refuses.
allow = []
deny = []

# Advisory data older than this many days is reported as stale.
max_advisory_age_days = 30
";

impl Policy {
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawPolicy = toml::from_str(text).map_err(|e| {
            anyhow::anyhow!("the image policy file does not parse: {e}")
        })?;
        let block_severity = match raw.block_severity.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) if s.eq_ignore_ascii_case("none") => None,
            Some(s) => Some(Severity::parse(s).filter(|v| *v != Severity::Unknown).with_context(|| {
                format!("block_severity {s:?} is not one of critical, high, medium, low, none")
            })?),
        };
        Ok(Self {
            block_unsigned: raw.block_unsigned,
            block_severity,
            allow: raw.allow,
            deny: raw.deny,
            max_advisory_age_days: raw.max_advisory_age_days,
        })
    }
}

pub fn policy_path(state_dir: &Path) -> PathBuf {
    state_dir.join("oci").join(POLICY_FILE)
}

pub fn load_policy(state_dir: &Path) -> Result<Option<Policy>> {
    let p = policy_path(state_dir);
    match std::fs::read_to_string(&p) {
        Ok(t) => Policy::parse(&t)
            .map(Some)
            .with_context(|| format!("reading {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// Write the starter policy. Refuses to overwrite.
pub fn init_policy(state_dir: &Path) -> Result<PathBuf> {
    let p = policy_path(state_dir);
    if p.exists() {
        bail!("{} already exists; edit it instead", p.display());
    }
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, STARTER)?;
    Ok(p)
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part) && rest.len() >= part.len();
        } else {
            let Some(at) = rest.find(part) else { return false };
            rest = &rest[at + part.len()..];
        }
    }
    true
}

// ---- verify records ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyRecord {
    pub reference: String,
    pub digest: String,
    /// `trusted`, `unsigned`, `untrusted-signer` or `invalid`.
    pub verdict: String,
    #[serde(default)]
    pub signer: Option<String>,
    /// The layer and config blobs were re-hashed and match the manifest (false when they were not all local).
    #[serde(default)]
    pub blobs_ok: bool,
    pub checked_unix: u64,
}

fn hex_of(digest: &str) -> Option<&str> {
    let h = digest.strip_prefix("sha256:")?;
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

fn verify_path(state_dir: &Path, digest: &str) -> Option<PathBuf> {
    hex_of(digest).map(|h| state_dir.join("oci").join("verify").join(format!("{h}.json")))
}

pub fn save_verify(state_dir: &Path, rec: &VerifyRecord) -> Result<()> {
    let p = verify_path(state_dir, &rec.digest).context("not a sha256 digest")?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, serde_json::to_vec_pretty(rec)?)?;
    Ok(())
}

pub fn load_verify(state_dir: &Path, digest: &str) -> Option<VerifyRecord> {
    let p = verify_path(state_dir, digest)?;
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

// ---- decisions ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// One of the `IMAGE_POLICY_*` codes; `fluxctl explain CODE` says what to do.
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub allowed: bool,
    pub violations: Vec<Violation>,
    pub warnings: Vec<String>,
    /// The run went ahead although the policy refused it.
    #[serde(default)]
    pub overridden: bool,
}

fn matches_any(patterns: &[String], reference: &str, digest: &str) -> bool {
    patterns.iter().any(|p| glob(p, reference) || glob(p, digest))
}

/// Judge one image. Pure: the caller supplies what is known about it.
pub fn decide(
    policy: &Policy,
    subject: &Subject,
    verify: Option<&VerifyRecord>,
    scan: Option<&ScanReport>,
) -> Decision {
    let mut violations = Vec::new();
    let mut warnings = Vec::new();
    let (r, d) = (subject.reference.as_str(), subject.manifest_digest.as_str());
    if matches_any(&policy.deny, r, d) {
        violations.push(Violation {
            code: CODE_DENIED.into(),
            message: format!("{r} is on the policy's deny list"),
        });
    } else if !matches_any(&policy.allow, r, d) {
        if policy.block_unsigned {
            let trusted = verify.is_some_and(|v| v.verdict == "trusted" && v.digest == d);
            if !trusted {
                let why = match verify.map(|v| v.verdict.as_str()) {
                    None => "it has not been verified: run `fluxctl image verify` on it".to_string(),
                    Some("unsigned") => "it has no signature".to_string(),
                    Some("untrusted-signer") => "it is signed, but not by a key you trust".to_string(),
                    Some("invalid") => "its signature does not match it".to_string(),
                    Some(other) => format!("its last verification said {other:?}"),
                };
                violations.push(Violation {
                    code: CODE_UNSIGNED.into(),
                    message: format!("{r} is not trusted: {why}"),
                });
            }
        }
        if let Some(min) = policy.block_severity {
            match scan {
                Some(s) if s.advisory.present => {
                    let hits: Vec<_> = s.findings.iter().filter(|f| f.severity >= min).collect();
                    if !hits.is_empty() {
                        let ids: Vec<&str> = hits.iter().take(3).map(|f| f.id.as_str()).collect();
                        violations.push(Violation {
                            code: CODE_VULNERABLE.into(),
                            message: format!(
                                "{r} has {} finding(s) at {} or worse ({}{}); {}",
                                hits.len(),
                                min.as_str(),
                                ids.join(", "),
                                if hits.len() > 3 { ", ..." } else { "" },
                                s.advisory.describe()
                            ),
                        });
                    } else if s.advisory.age_days.is_some_and(|a| a > policy.max_advisory_age_days) {
                        warnings.push(format!("{}; the policy asks for at most {} days", s.advisory.describe(), policy.max_advisory_age_days));
                    }
                }
                _ => warnings.push(
                    "the policy blocks vulnerable images but there is no advisory data on this machine, so the image was not checked: load it with `fluxctl image scan IMAGE --update --source URL_OR_FILE`"
                        .into(),
                ),
            }
        }
    }
    Decision { allowed: violations.is_empty(), violations, warnings, overridden: false }
}

// ---- assessment (what Studio and `image policy check` show) ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment {
    pub reference: String,
    pub manifest_digest: String,
    /// `trusted`, `unsigned`, `untrusted-signer`, `invalid` or `unchecked` (never verified).
    pub signature: String,
    #[serde(default)]
    pub signer: Option<String>,
    #[serde(default)]
    pub signature_checked_unix: u64,
    #[serde(default)]
    pub packages: Option<usize>,
    #[serde(default)]
    pub scan: Option<ScanReport>,
    /// Present when a policy file exists.
    #[serde(default)]
    pub decision: Option<Decision>,
    /// `blocked`, `risk`, `unsigned`, `unscanned`, `ok` or `error`.
    pub badge: String,
    /// One plain sentence.
    pub summary: String,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn assess(state_dir: &Path, subject: &Subject, now: u64) -> Assessment {
    let verify = load_verify(state_dir, &subject.manifest_digest);
    let signature = verify.as_ref().map_or("unchecked".to_string(), |v| v.verdict.clone());
    let mut a = Assessment {
        reference: subject.reference.clone(),
        manifest_digest: subject.manifest_digest.clone(),
        signature,
        signer: verify.as_ref().and_then(|v| v.signer.clone()),
        signature_checked_unix: verify.as_ref().map_or(0, |v| v.checked_unix),
        packages: None,
        scan: None,
        decision: None,
        badge: "error".into(),
        summary: String::new(),
        error: None,
    };
    let sbom = match sbom::load_or_collect(state_dir, subject, now) {
        Ok(s) => s,
        Err(e) => {
            a.error = Some(format!("{e:#}"));
            a.summary = "Could not read this image's layers.".into();
            return a;
        }
    };
    a.packages = Some(sbom.packages.len());
    let file = match advisory::load(state_dir) {
        Ok(f) => f,
        Err(e) => {
            a.error = Some(format!("{e:#}"));
            None
        }
    };
    let scan = advisory::scan(&sbom, file.as_ref(), now);
    match load_policy(state_dir) {
        Ok(Some(p)) => a.decision = Some(decide(&p, subject, verify.as_ref(), Some(&scan))),
        Ok(None) => {}
        Err(e) => a.error = Some(format!("{e:#}")),
    }
    let worst = scan.worst();
    let trusted = a.signature == "trusted";
    let (badge, summary) = if a.decision.as_ref().is_some_and(|d| !d.allowed) {
        let d = a.decision.as_ref().unwrap();
        ("blocked", format!("The policy blocks this image: {}.", d.violations.first().map_or("", |v| v.message.as_str())))
    } else if worst.is_some_and(|w| w >= Severity::High) {
        ("risk", format!("{} known problem(s), the worst {}; {}.", scan.findings.len(), worst.unwrap().as_str(), scan.advisory.describe()))
    } else if !trusted {
        (
            "unsigned",
            match a.signature.as_str() {
                "unchecked" => "Its signature has not been checked yet.".to_string(),
                "unsigned" => "It has no signature.".to_string(),
                "untrusted-signer" => "It is signed by a key you do not trust.".to_string(),
                _ => "Its signature does not match.".to_string(),
            },
        )
    } else if !scan.advisory.present {
        ("unscanned", "Signed by a trusted key, but there is no advisory data to scan it against.".to_string())
    } else {
        ("ok", format!("Signed by a trusted key; no high or critical findings; {}.", scan.advisory.describe()))
    };
    a.badge = badge.into();
    a.summary = summary;
    a.scan = Some(scan);
    a
}

// ---- enforcement ----

/// Called when a sandbox starts. `Ok(None)`: no policy. `Ok(Some(d))`: the decision (allowed, or overridden).
/// `Err`: the policy refused and no override was given; the message names the fix and the explain code.
pub fn enforce(
    state_dir: &Path,
    subject: &Subject,
    override_policy: bool,
    actor: &str,
    now: u64,
) -> Result<Option<Decision>> {
    let Some(policy) = load_policy(state_dir)? else { return Ok(None) };
    let verify = load_verify(state_dir, &subject.manifest_digest);
    let scan = if policy.block_severity.is_some() && !policy_skips(&policy, subject) {
        let sbom = sbom::load_or_collect(state_dir, subject, now)?;
        Some(advisory::scan(&sbom, advisory::load(state_dir)?.as_ref(), now))
    } else {
        None
    };
    let mut d = decide(&policy, subject, verify.as_ref(), scan.as_ref());
    for w in &d.warnings {
        tracing::warn!(image = %subject.reference, "image policy: {w}");
    }
    if d.allowed {
        return Ok(Some(d));
    }
    if override_policy {
        d.overridden = true;
        audit_override(state_dir, subject, &d, actor, now);
        return Ok(Some(d));
    }
    let first = &d.violations[0];
    Err(anyhow::Error::new(PolicyBlocked(format!(
        "the image policy blocks {}: {}\n  to run it anyway (this is logged): set policy_override / pass `--policy-override`\n  more: fluxctl explain {}",
        subject.reference,
        d.violations.iter().map(|v| v.message.as_str()).collect::<Vec<_>>().join("; "),
        first.code
    ))))
}

/// The policy refused an image. The API answers it with 403.
#[derive(Debug)]
pub struct PolicyBlocked(pub String);

impl std::fmt::Display for PolicyBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyBlocked {}

fn policy_skips(p: &Policy, s: &Subject) -> bool {
    matches_any(&p.allow, &s.reference, &s.manifest_digest)
}

/// The loud record: a WARN on the audit target and a line in `policy-overrides.log`.
pub fn audit_override(state_dir: &Path, subject: &Subject, d: &Decision, actor: &str, now: u64) {
    let reasons: Vec<String> = d.violations.iter().map(|v| format!("{}: {}", v.code, v.message)).collect();
    tracing::warn!(
        target: "fluxvm_audit",
        actor = %actor,
        image = %subject.reference,
        digest = %subject.manifest_digest,
        "POLICY OVERRIDE: {} was started against the image policy ({})",
        subject.reference,
        reasons.join("; ")
    );
    let line = serde_json::json!({
        "unix": now,
        "event": "POLICY OVERRIDE",
        "actor": actor,
        "reference": subject.reference,
        "digest": subject.manifest_digest,
        "violations": d.violations,
    });
    let p = state_dir.join("oci").join(OVERRIDE_LOG);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// Re-hash the manifest's blobs: every layer and the config. Returns the digests that are missing or do not match.
pub fn check_blobs(store: &BlobStore, subject: &Subject, config_digest: Option<&str>) -> (Vec<String>, Vec<String>) {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let (mut missing, mut bad) = (Vec::new(), Vec::new());
    let mut all: Vec<&str> = subject.layers.iter().map(|l| l.digest.as_str()).collect();
    if let Some(c) = config_digest {
        all.push(c);
    }
    for d in all {
        let Ok(p) = store.path(d) else {
            bad.push(d.to_string());
            continue;
        };
        let Ok(mut f) = std::fs::File::open(&p) else {
            missing.push(d.to_string());
            continue;
        };
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let ok = loop {
            match f.read(&mut buf) {
                Ok(0) => break true,
                Ok(n) => h.update(&buf[..n]),
                Err(_) => break false,
            }
        };
        if !ok || format!("sha256:{}", hex::encode(h.finalize())) != d {
            bad.push(d.to_string());
        }
    }
    (missing, bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisory::{Advisory, AdvisoryFile};
    use sha2::Digest;

    fn subject() -> Subject {
        Subject {
            reference: "ghcr.io/acme/app:1".into(),
            manifest_digest: format!("sha256:{}", "ab".repeat(32)),
            architecture: "arm64".into(),
            layers: vec![],
        }
    }

    fn verify(v: &str) -> VerifyRecord {
        VerifyRecord { reference: "r".into(), digest: subject().manifest_digest, verdict: v.into(), signer: Some("me".into()), blobs_ok: true, checked_unix: 1 }
    }

    fn scan_with(sev: Option<Severity>, age_days: u64) -> ScanReport {
        let f = AdvisoryFile {
            format: advisory::FORMAT.into(),
            updated_unix: 1_000_000,
            source: "t".into(),
            advisories: sev
                .map(|s| Advisory { id: "CVE-1".into(), package: "p".into(), ecosystem: "*".into(), severity: s, introduced: None, fixed: None, summary: "x".into() })
                .into_iter()
                .collect(),
        };
        let sbom = sbom::Sbom {
            reference: "r".into(),
            manifest_digest: "d".into(),
            architecture: "arm64".into(),
            distro: None,
            packages: vec![sbom::Package { name: "p".into(), version: "1".into(), ecosystem: "deb".into(), arch: None, purl: String::new() }],
            notes: vec![],
            generated_unix: 0,
        };
        advisory::scan(&sbom, Some(&f), 1_000_000 + age_days * 86_400)
    }

    #[test]
    fn globs() {
        assert!(glob("ghcr.io/acme/*", "ghcr.io/acme/app:1"));
        assert!(glob("*:latest", "x/y:latest"));
        assert!(!glob("*:latest", "x/y:1"));
        assert!(glob("a*c*e", "abcde"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
        assert!(glob("*", ""));
        assert!(!glob("ab*ab", "ab"));
    }

    #[test]
    fn the_policy_file_parses_and_rejects_typos() {
        let p = Policy::parse(STARTER).unwrap();
        assert!(p.block_unsigned);
        assert_eq!(p.block_severity, Some(Severity::Critical));
        assert_eq!(p.max_advisory_age_days, 30);
        assert!(Policy::parse("block_unsigend = true").unwrap_err().to_string().contains("does not parse"));
        assert!(Policy::parse("block_severity = \"bad\"").unwrap_err().to_string().contains("not one of"));
        assert_eq!(Policy::parse("block_severity = \"none\"").unwrap().block_severity, None);
    }

    #[test]
    fn unsigned_and_critical_are_blocked() {
        let p = Policy::parse(STARTER).unwrap();
        let d = decide(&p, &subject(), None, Some(&scan_with(None, 1)));
        assert!(!d.allowed);
        assert_eq!(d.violations[0].code, CODE_UNSIGNED);
        assert!(d.violations[0].message.contains("has not been verified"));
        let d = decide(&p, &subject(), Some(&verify("trusted")), Some(&scan_with(Some(Severity::Critical), 3)));
        assert_eq!(d.violations.len(), 1);
        assert_eq!(d.violations[0].code, CODE_VULNERABLE);
        assert!(d.violations[0].message.contains("3 days old"));
        let d = decide(&p, &subject(), Some(&verify("trusted")), Some(&scan_with(Some(Severity::High), 3)));
        assert!(d.allowed, "high is below the critical threshold");
        let d = decide(&p, &subject(), Some(&verify("untrusted-signer")), None);
        assert!(d.violations[0].message.contains("not by a key you trust"));
    }

    #[test]
    fn allow_skips_the_rules_but_deny_wins() {
        let mut p = Policy::parse(STARTER).unwrap();
        p.allow = vec!["ghcr.io/acme/*".into()];
        assert!(decide(&p, &subject(), None, None).allowed);
        p.deny = vec!["*:1".into()];
        let d = decide(&p, &subject(), None, None);
        assert!(!d.allowed);
        assert_eq!(d.violations[0].code, CODE_DENIED);
    }

    #[test]
    fn a_verification_of_another_digest_does_not_count() {
        let p = Policy::parse("block_unsigned = true").unwrap();
        let mut v = verify("trusted");
        v.digest = format!("sha256:{}", "cd".repeat(32));
        assert!(!decide(&p, &subject(), Some(&v), None).allowed);
    }

    #[test]
    fn no_advisory_data_warns_instead_of_claiming_clean() {
        let p = Policy::parse("block_severity = \"critical\"").unwrap();
        let sbom_scan = advisory::scan(
            &sbom::Sbom { reference: "r".into(), manifest_digest: "d".into(), architecture: "a".into(), distro: None, packages: vec![], notes: vec![], generated_unix: 0 },
            None,
            0,
        );
        let d = decide(&p, &subject(), None, Some(&sbom_scan));
        assert!(d.allowed);
        assert!(d.warnings[0].contains("not checked"));
    }

    #[test]
    fn stale_data_warns() {
        let p = Policy::parse("block_severity = \"critical\"\nmax_advisory_age_days = 7").unwrap();
        let d = decide(&p, &subject(), None, Some(&scan_with(None, 20)));
        assert!(d.allowed);
        assert!(d.warnings[0].contains("20 days old"));
    }

    #[test]
    fn enforce_without_a_policy_is_a_no_op_then_blocks_then_overrides_loudly() {
        let d = tempfile::tempdir().unwrap();
        assert!(enforce(d.path(), &subject(), false, "t", 0).unwrap().is_none());
        init_policy(d.path()).unwrap();
        assert!(init_policy(d.path()).is_err(), "never overwrites");
        // Only block_unsigned can fire here (no layers, no advisory data).
        let e = enforce(d.path(), &subject(), false, "t", 0).unwrap_err().to_string();
        assert!(e.contains("IMAGE_POLICY_UNSIGNED"), "{e}");
        assert!(e.contains("--policy-override"));
        let ok = enforce(d.path(), &subject(), true, "tok-a", 77).unwrap().unwrap();
        assert!(ok.overridden && !ok.allowed);
        let log = std::fs::read_to_string(d.path().join("oci").join(OVERRIDE_LOG)).unwrap();
        assert!(log.contains("POLICY OVERRIDE") && log.contains("tok-a") && log.contains("IMAGE_POLICY_UNSIGNED"));
        // A trusted verification lets it through with no override.
        save_verify(d.path(), &verify("trusted")).unwrap();
        let ok = enforce(d.path(), &subject(), false, "t", 0).unwrap().unwrap();
        assert!(ok.allowed && !ok.overridden);
    }

    #[test]
    fn blob_check_finds_missing_and_corrupt() {
        let d = tempfile::tempdir().unwrap();
        let store = BlobStore::for_state_dir(d.path());
        let good = store.put(&format!("sha256:{}", hex::encode(sha2::Sha256::digest(b"abc"))), b"abc").unwrap();
        let good_digest = format!("sha256:{}", good.file_name().unwrap().to_string_lossy());
        let bad_digest = format!("sha256:{}", "11".repeat(32));
        std::fs::write(store.path(&bad_digest).unwrap(), b"not that").unwrap();
        let lost = format!("sha256:{}", "22".repeat(32));
        let mut s = subject();
        let layer = |d: &str| crate::oci_registry::Layer { digest: d.into(), size: 0, compression: crate::oci_registry::Compression::None, diff_id: String::new() };
        s.layers = vec![layer(&good_digest), layer(&bad_digest), layer(&lost)];
        let (missing, bad) = check_blobs(&store, &s, None);
        assert_eq!(missing, vec![lost]);
        assert_eq!(bad, vec![bad_digest]);
    }
}
