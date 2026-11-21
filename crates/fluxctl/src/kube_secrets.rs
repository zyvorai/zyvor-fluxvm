// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Secrets-encryption status and key rotation, as thin wrappers over `k3s secrets-encrypt` run on a server through a
//! [`GuestOps`]. The status parser is pure. Rotation is refused without an explicit `yes`, because a wrong rotation
//! can corrupt a cluster; in an HA cluster it must be run on one server only, then the others restarted.

use crate::kube_addons::GuestOps;
use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionStatus {
    pub enabled: bool,
    pub stage: String,
    /// "All hashes match" or the mismatch text
    pub hashes: String,
}

impl EncryptionStatus {
    pub fn hashes_match(&self) -> bool {
        self.hashes.contains("All hashes match")
    }
    /// After `rotate-keys` has finished re-encrypting.
    pub fn rotation_finished(&self) -> bool {
        self.stage == "reencrypt_finished"
    }
}

/// Parses `k3s secrets-encrypt status`.
pub fn parse_status(s: &str) -> Option<EncryptionStatus> {
    let field = |key: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
    };
    let enabled = field("Encryption Status:")?;
    Some(EncryptionStatus {
        enabled: enabled == "Enabled",
        stage: field("Current Rotation Stage:").unwrap_or_default(),
        hashes: field("Server Encryption Hashes:").unwrap_or_default(),
    })
}

pub fn status(g: &dyn GuestOps) -> Result<EncryptionStatus> {
    let out = g.run("sudo k3s secrets-encrypt status")?;
    parse_status(&out).ok_or_else(|| {
        anyhow::anyhow!("could not read `k3s secrets-encrypt status` output:\n{out}")
    })
}

/// Runs `rotate-keys` (k3s 1.30+) after checking that encryption is enabled and hashes match, then returns the status after.
pub fn rotate_keys(g: &dyn GuestOps, yes: bool) -> Result<EncryptionStatus> {
    if !yes {
        bail!(
            "key rotation re-encrypts every Secret and can corrupt a cluster if done wrongly; take a checkpoint or etcd snapshot, \
             then repeat with --yes. In an HA cluster run it on ONE server only, then restart k3s on the others."
        );
    }
    let before = status(g)?;
    if !before.enabled {
        bail!(
            "secrets encryption is not enabled; enable it first (config `secrets-encryption: true`)"
        );
    }
    if !before.hashes_match() {
        bail!(
            "encryption hashes do not match between servers ({}); fix that before rotating",
            before.hashes
        );
    }
    g.run("sudo k3s secrets-encrypt rotate-keys")?;
    status(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_addons::FakeGuest;

    const OK: &str = "Encryption Status: Enabled\nCurrent Rotation Stage: reencrypt_finished\nServer Encryption Hashes: All hashes match\n\nActive  Key Type  Name\n------  --------  ----\n *      AES-CBC   aescbckey\n";

    #[test]
    fn parses_status() {
        let s = parse_status(OK).unwrap();
        assert!(s.enabled && s.hashes_match() && s.rotation_finished());
        let d =
            parse_status("Encryption Status: Disabled\nCurrent Rotation Stage: start\n").unwrap();
        assert!(!d.enabled && !d.hashes_match());
        assert!(parse_status("garbage").is_none());
    }

    #[test]
    fn rotation_needs_yes_and_a_healthy_start() {
        let g = FakeGuest {
            answers: vec![("status".into(), OK.into())],
            ..Default::default()
        };
        assert!(rotate_keys(&g, false).is_err());
        assert!(g.calls().is_empty(), "nothing runs without --yes");
        let s = rotate_keys(&g, true).unwrap();
        assert!(s.rotation_finished());
        assert_eq!(
            g.calls(),
            vec![
                "run sudo k3s secrets-encrypt status",
                "run sudo k3s secrets-encrypt rotate-keys",
                "run sudo k3s secrets-encrypt status"
            ]
        );
    }

    #[test]
    fn rotation_refused_when_disabled_or_mismatched() {
        let off = FakeGuest {
            answers: vec![("status".into(), "Encryption Status: Disabled\n".into())],
            ..Default::default()
        };
        assert!(rotate_keys(&off, true).is_err());
        let skew = FakeGuest {
            answers: vec![("status".into(), "Encryption Status: Enabled\nServer Encryption Hashes: hash does not match between s1 and s2\n".into())],
            ..Default::default()
        };
        assert!(rotate_keys(&skew, true).is_err());
        assert_eq!(skew.calls().len(), 1, "rotate-keys never ran");
    }
}
