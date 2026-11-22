// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet`: the inventory file (YAML, or JSON, which YAML reads) that `apply` converges the fleet to.
//! It holds no secret: the join token is a path to a file on the machine running `fluxctl`.

use crate::mac_fleet_remote::{
    K3sSpec, valid_label, valid_node_name, valid_sha256, valid_url, valid_version,
};
use crate::mac_fleet_ssh::{Target, valid_host};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    /// The release to install on every Mac. Default: the version of this `fluxctl`.
    pub version: Option<String>,
    /// SHA-256 the release package must have (in addition to the one published beside it).
    pub sha256: Option<String>,
    pub release_base: Option<String>,
    /// The fleet registry (`fluxvm-agent central`). Without it Macs are installed but not enrolled.
    pub central: Option<String>,
    /// `{host}` and `{name}` are replaced per Mac.
    pub advertise_url: Option<String>,
    /// A file on this machine holding a short-lived join token (`fvjt_...`).
    pub token_file: Option<PathBuf>,
    /// Default ssh user and key for every host.
    pub user: Option<String>,
    pub identity: Option<PathBuf>,
    pub known_hosts: Option<PathBuf>,
    pub k3s: Option<K3sInv>,
    pub hosts: Vec<HostEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K3sInv {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    pub wait_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    pub name: Option<String>,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
    /// Prepare a k3s Linux guest on this Mac.
    #[serde(default)]
    pub k3s: bool,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// `inventory` finds `inventory`, `inventory.yaml`, `inventory.yml`, `inventory.json`.
pub fn resolve_path(p: &Path) -> Result<PathBuf> {
    if p.is_file() {
        return Ok(p.to_owned());
    }
    for ext in ["yaml", "yml", "json"] {
        let c = PathBuf::from(format!("{}.{ext}", p.display()));
        if c.is_file() {
            return Ok(c);
        }
    }
    bail!(
        "inventory file {} not found (also tried .yaml, .yml, .json)",
        p.display()
    )
}

pub fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ => p.to_owned(),
    }
}

impl Inventory {
    pub fn load(p: &Path) -> Result<Self> {
        let path = resolve_path(p)?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let inv: Inventory = serde_yaml::from_str(text)
            .context("not a valid inventory (see docs/mac-fleet-quickstart.md)")?;
        inv.validate()?;
        Ok(inv)
    }

    fn validate(&self) -> Result<()> {
        if self.hosts.is_empty() {
            bail!("the inventory has no hosts");
        }
        if let Some(v) = &self.version
            && !valid_version(v)
        {
            bail!("version {v:?} is not a release number such as 0.4.21");
        }
        if let Some(s) = &self.sha256
            && !valid_sha256(s)
        {
            bail!("sha256 must be 64 hex digits");
        }
        for (k, u) in [
            ("release_base", &self.release_base),
            ("central", &self.central),
            ("advertise_url", &self.advertise_url),
        ] {
            if let Some(u) = u
                && !valid_url(&u.replace("{host}", "h").replace("{name}", "n"))
            {
                bail!("{k} {u:?} is not an http(s):// URL without shell characters");
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for h in self.targets()? {
            if !seen.insert(h.name.clone()) {
                bail!("two hosts are named {:?}: give one a `name`", h.name);
            }
        }
        Ok(())
    }

    pub fn targets(&self) -> Result<Vec<Target>> {
        self.hosts
            .iter()
            .map(|h| {
                if !valid_host(&h.host) {
                    bail!("bad host {:?}", h.host);
                }
                let mut t = Target::parse(&h.host)?;
                if let Some(n) = &h.name {
                    t.name = n.clone();
                }
                if !valid_node_name(&t.name) {
                    bail!("host {:?}: name {:?} must be lowercase letters, digits, dashes and dots", h.host, t.name);
                }
                t.user = h.user.clone().or_else(|| self.user.clone());
                if let Some(u) = &t.user
                    && Target::parse(&format!("{u}@x")).is_err()
                {
                    bail!("bad user {u:?}");
                }
                t.port = h.port;
                t.identity = h.identity.clone().or_else(|| self.identity.clone()).map(|p| expand_home(&p));
                t.k3s = h.k3s;
                for l in &h.labels {
                    if !valid_label(l) {
                        bail!("host {}: label {l:?} must be key=value without spaces or shell characters", t.name);
                    }
                }
                t.labels = h.labels.clone();
                Ok(t)
            })
            .collect()
    }

    pub fn k3s_spec(&self) -> K3sSpec {
        let d = K3sSpec::default();
        let k = self.k3s.clone().unwrap_or_default();
        K3sSpec {
            cpus: k.cpus.unwrap_or(d.cpus),
            memory_mib: k.memory_mib.unwrap_or(d.memory_mib),
            disk_gib: k.disk_gib.unwrap_or(d.disk_gib),
            wait_secs: k.wait_secs.unwrap_or(d.wait_secs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = "
version: 0.4.22
central: https://fleet.example:7790
token_file: ~/join.token
user: admin
hosts:
  - host: mini-1.local
    k3s: true
    labels: [zone=lab]
  - host: 10.0.0.7
    name: studio
    user: ops
    port: 2222
";

    #[test]
    fn yaml_and_json_inventories_give_the_same_targets() {
        let a = Inventory::parse(YAML).unwrap();
        let t = a.targets().unwrap();
        assert_eq!(t[0].name, "mini-1");
        assert_eq!(t[0].user.as_deref(), Some("admin"));
        assert!(t[0].k3s && t[0].labels == ["zone=lab"]);
        assert_eq!(
            (t[1].name.as_str(), t[1].user.as_deref(), t[1].port),
            ("studio", Some("ops"), Some(2222))
        );
        let j = Inventory::parse(
            r#"{"hosts":[{"host":"mini-1.local","k3s":true,"labels":["zone=lab"]}],"user":"admin"}"#,
        )
        .unwrap();
        assert_eq!(j.targets().unwrap()[0], t[0]);
    }

    #[test]
    fn mistakes_are_named() {
        let bad = |s: &str| Inventory::parse(s).unwrap_err().to_string();
        assert!(bad("hosts: []").contains("no hosts"));
        assert!(
            format!(
                "{:#}",
                Inventory::parse("hosts: [{host: a}]\nverison: 1").unwrap_err()
            )
            .contains("verison")
        );
        assert!(bad("version: latest\nhosts: [{host: a}]").contains("release number"));
        assert!(bad("hosts: [{host: a.x}, {host: a.y}]").contains("two hosts"));
        assert!(bad("hosts: [{host: '-oFoo'}]").contains("bad host"));
        assert!(bad("central: \"https://x';id\"\nhosts: [{host: a}]").contains("central"));
        assert!(bad("hosts: [{host: a, labels: ['a=b c']}]").contains("label"));
    }

    #[test]
    fn a_bare_name_finds_the_file_with_an_extension() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("fleet.yaml"), YAML).unwrap();
        assert!(Inventory::load(&d.path().join("fleet")).is_ok());
        assert!(resolve_path(&d.path().join("nope")).is_err());
    }
}
