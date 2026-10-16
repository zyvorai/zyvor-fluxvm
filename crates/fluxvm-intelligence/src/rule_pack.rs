// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Signed "virtual patch" rule packs for the VMM Guard.
//!
//! A pack is a time-limited mitigation for a known-vulnerable host component:
//! it blocks specific files or devices by kernel object identity until the
//! real fix is deployed. It is not an exploit-signature engine. The rule
//! vocabulary is exactly what the guard's LSM hooks can express:
//!
//! - `deny-exec`: executing this file is blocked;
//! - `deny-open`: opening this file (read or write, including loading it as a
//!   library) is blocked;
//! - `deny-device`: opening this character/block device is blocked.
//!
//! Packs are TOML with a detached Ed25519 signature (`<file>.sig`) and a
//! mandatory `valid_until`. Already-open handles are not affected.

use crate::exec_policy;
use anyhow::{Context, Result, bail};
use fluxvm_core::signing::ParsedSigner;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const SCHEMA_VERSION: u32 = 1;
/// Bits of a file rule value in the `guard_patch_files` map.
pub const BIT_DENY_EXEC: u8 = 1;
pub const BIT_DENY_OPEN: u8 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RuleAction {
    DenyExec,
    DenyOpen,
    DenyDevice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub action: RuleAction,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RulePack {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub cve: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// RFC 3339 timestamp after which the pack is refused.
    pub valid_until: String,
    pub rules: Vec<Rule>,
}

/// What `status` reports about an applied pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackInfo {
    pub id: String,
    pub cve: Vec<String>,
    pub valid_until: i64,
    pub signer: String,
}

impl RulePack {
    pub fn parse(text: &str) -> Result<Self> {
        let p: RulePack = toml::from_str(text).context("parsing rule pack")?;
        if p.version != SCHEMA_VERSION {
            bail!(
                "unsupported rule pack version {} (expected {SCHEMA_VERSION})",
                p.version
            );
        }
        if p.id.is_empty()
            || !p
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            bail!("rule pack id must be non-empty [A-Za-z0-9._-]");
        }
        if p.rules.is_empty() {
            bail!("rule pack {} has no rules", p.id);
        }
        if let Some(bad) = p.rules.iter().find(|r| !r.path.is_absolute()) {
            bail!("rule path {} is not absolute", bad.path.display());
        }
        p.valid_until_unix()?;
        Ok(p)
    }

    pub fn render(&self) -> Result<String> {
        toml::to_string_pretty(self).context("rendering rule pack")
    }

    pub fn valid_until_unix(&self) -> Result<i64> {
        Ok(chrono::DateTime::parse_from_rfc3339(&self.valid_until)
            .with_context(|| format!("valid_until {:?} is not RFC 3339", self.valid_until))?
            .timestamp())
    }

    pub fn is_expired(&self, now: i64) -> Result<bool> {
        Ok(self.valid_until_unix()? <= now)
    }
}

/// Packs merged into the per-path/per-device rules the guard loads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchSet {
    pub packs: Vec<PackInfo>,
    /// Path -> OR of `BIT_*`.
    pub files: BTreeMap<PathBuf, u8>,
    pub devices: Vec<PathBuf>,
}

impl PatchSet {
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }
}

/// Merges verified, unexpired packs into a [`PatchSet`]. Duplicate pack ids
/// are rejected so one pack cannot silently shadow another.
pub fn merge(packs: &[(RulePack, String)], now: i64) -> Result<PatchSet> {
    let mut set = PatchSet::default();
    for (pack, signer) in packs {
        if set.packs.iter().any(|p| p.id == pack.id) {
            bail!("rule pack id {} given twice", pack.id);
        }
        if pack.is_expired(now)? {
            bail!("rule pack {} expired at {}", pack.id, pack.valid_until);
        }
        for rule in &pack.rules {
            match rule.action {
                RuleAction::DenyExec => {
                    *set.files.entry(rule.path.clone()).or_default() |= BIT_DENY_EXEC
                }
                RuleAction::DenyOpen => {
                    *set.files.entry(rule.path.clone()).or_default() |= BIT_DENY_OPEN
                }
                RuleAction::DenyDevice => {
                    if !set.devices.contains(&rule.path) {
                        set.devices.push(rule.path.clone());
                    }
                }
            }
        }
        set.packs.push(PackInfo {
            id: pack.id.clone(),
            cve: pack.cve.clone(),
            valid_until: pack.valid_until_unix()?,
            signer: signer.clone(),
        });
    }
    Ok(set)
}

/// Signs `file` after checking it parses as a rule pack.
pub fn sign_file(file: &Path, private_key_b64: &str) -> Result<PathBuf> {
    RulePack::parse(&std::fs::read_to_string(file)?)?;
    exec_policy::sign_detached(file, private_key_b64)
}

/// Verifies and parses a pack. Expiry is checked by [`merge`] at apply time.
pub fn load_verified(file: &Path, trusted: &[ParsedSigner]) -> Result<(RulePack, String)> {
    let (text, signer) = exec_policy::read_verified(file, trusted)?;
    Ok((RulePack::parse(&text)?, signer))
}

pub fn load_unsigned(file: &Path) -> Result<(RulePack, String)> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    Ok((RulePack::parse(&text)?, "unsigned".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::signing;

    fn pack(id: &str, valid_until: &str, rules: &[(RuleAction, &str)]) -> RulePack {
        RulePack {
            version: 1,
            id: id.into(),
            cve: vec!["CVE-2026-0001".into()],
            description: None,
            valid_until: valid_until.into(),
            rules: rules
                .iter()
                .map(|(a, p)| Rule {
                    action: *a,
                    path: (*p).into(),
                })
                .collect(),
        }
    }

    const FUTURE: &str = "2099-01-01T00:00:00Z";

    #[test]
    fn parse_rejects_bad_packs() {
        let ok = pack("P-1", FUTURE, &[(RuleAction::DenyExec, "/usr/bin/x")]);
        assert!(RulePack::parse(&ok.render().unwrap()).is_ok());
        let mut bad = ok.clone();
        bad.version = 2;
        assert!(RulePack::parse(&bad.render().unwrap()).is_err());
        let mut bad = ok.clone();
        bad.id = "has space".into();
        assert!(RulePack::parse(&bad.render().unwrap()).is_err());
        let mut bad = ok.clone();
        bad.rules.clear();
        assert!(RulePack::parse(&bad.render().unwrap()).is_err());
        let bad = pack("P-2", FUTURE, &[(RuleAction::DenyOpen, "relative/x")]);
        assert!(RulePack::parse(&bad.render().unwrap()).is_err());
        let bad = pack("P-3", "next tuesday", &[(RuleAction::DenyOpen, "/x")]);
        assert!(RulePack::parse(&bad.render().unwrap()).is_err());
    }

    #[test]
    fn merge_combines_bits_and_rejects_expired_or_duplicate() {
        let a = pack(
            "A",
            FUTURE,
            &[
                (RuleAction::DenyExec, "/usr/lib/v"),
                (RuleAction::DenyDevice, "/dev/vhost-vdpa"),
            ],
        );
        let b = pack("B", FUTURE, &[(RuleAction::DenyOpen, "/usr/lib/v")]);
        let set = merge(&[(a.clone(), "ci".into()), (b, "ci".into())], 1_000).unwrap();
        assert_eq!(
            set.files[Path::new("/usr/lib/v")],
            BIT_DENY_EXEC | BIT_DENY_OPEN
        );
        assert_eq!(set.devices, vec![PathBuf::from("/dev/vhost-vdpa")]);
        assert_eq!(set.packs.len(), 2);

        assert!(merge(&[(a.clone(), "ci".into()), (a.clone(), "ci".into())], 1_000).is_err());
        // 2099 pack is expired relative to a "now" past it
        assert!(merge(&[(a, "ci".into())], 5_000_000_000).is_err());
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let p = pack("E", "2030-01-01T00:00:00Z", &[(RuleAction::DenyOpen, "/x")]);
        let t = p.valid_until_unix().unwrap();
        assert!(!p.is_expired(t - 1).unwrap());
        assert!(p.is_expired(t).unwrap());
    }

    #[test]
    fn signed_pack_round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pack.toml");
        let p = pack("S", FUTURE, &[(RuleAction::DenyExec, "/usr/bin/x")]);
        std::fs::write(&f, p.render().unwrap()).unwrap();
        let (private, public) = signing::generate_keypair();
        sign_file(&f, &private).unwrap();
        let trusted = exec_policy::trusted_from_keys(&[public]).unwrap();
        let (loaded, signer) = load_verified(&f, &trusted).unwrap();
        assert_eq!(loaded, p);
        assert_eq!(signer, "key0");

        // extend validity after signing
        let text = std::fs::read_to_string(&f)
            .unwrap()
            .replace("2099-01-01", "2199-01-01");
        std::fs::write(&f, text).unwrap();
        assert!(load_verified(&f, &trusted).is_err());
    }

    #[test]
    fn a_pack_is_not_a_valid_exec_policy_and_vice_versa() {
        // different schemas: neither signed file type parses as the other
        let p = pack("X", FUTURE, &[(RuleAction::DenyExec, "/x")]);
        assert!(exec_policy::ExecPolicy::parse(&p.render().unwrap()).is_err());
        let e = "version = 1\npaths = [\"/bin/ls\"]\n";
        assert!(RulePack::parse(e).is_err());
    }
}
