// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The container description shared by the daemon, the launcher and the unit renderer.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub memory_mib: Option<u64>,
    /// Percent of one CPU: 100 is one core, 250 two and a half.
    pub cpu_percent: Option<u32>,
    pub pids: Option<u64>,
    /// 1..=10000, systemd's `IOWeight=`; needs the io controller.
    pub io_weight: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Net {
    /// A private network namespace with only loopback: no way out. The safe default.
    #[default]
    None,
    /// The host's network namespace. Refused for untrusted code; no egress policy applies.
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Seccomp {
    #[default]
    Default,
    Unconfined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hardening {
    /// `ProtectSystem=strict` for the container: the image's root is mounted read-only (`/tmp`, `/run` stay tmpfs).
    pub read_only_root: bool,
    pub no_new_privileges: bool,
    /// Capabilities kept in the bounding set, `CAP_*` names. Default: the Docker default set.
    pub capabilities: Vec<String>,
    pub seccomp: Seccomp,
}

/// The capabilities Docker keeps by default.
pub const DEFAULT_CAPS: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FSETID",
    "CAP_FOWNER",
    "CAP_MKNOD",
    "CAP_NET_RAW",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETFCAP",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_SYS_CHROOT",
    "CAP_KILL",
    "CAP_AUDIT_WRITE",
];

impl Default for Hardening {
    fn default() -> Self {
        Self {
            read_only_root: true,
            no_new_privileges: true,
            capabilities: DEFAULT_CAPS.iter().map(|s| s.to_string()).collect(),
            seccomp: Seccomp::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerSpec {
    /// `fx-<id8>`; also the unit name `fx-<id8>.service` and the hostname.
    pub name: String,
    /// What it was made from, for the unit's description.
    pub image: String,
    pub rootfs: PathBuf,
    pub argv: Vec<String>,
    /// `KEY=value`.
    pub env: Vec<String>,
    pub workdir: String,
    /// `uid[:gid]`, numeric. Empty: root (rootless: the invoking user, mapped to 0).
    pub user: String,
    pub limits: Limits,
    pub hardening: Hardening,
    pub net: Net,
    pub restart: Restart,
    pub restart_sec: u32,
    /// Wall-clock lifetime; becomes `RuntimeMaxSec=`.
    pub ttl_seconds: Option<u64>,
    /// Units this one starts after / needs (stacks).
    pub after: Vec<String>,
    pub requires: Vec<String>,
    /// Where the launcher writes `pid` and `exit`.
    pub state_dir: PathBuf,
    /// Console log (`StandardOutput=append:`).
    pub log_path: PathBuf,
    /// Without systemd: cgroup directory the launcher creates and fills with the limits.
    pub cgroup_dir: Option<PathBuf>,
    /// The launcher binary (absolute) used in `ExecStart=`.
    pub launcher: PathBuf,
}

impl Default for ContainerSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            image: String::new(),
            rootfs: PathBuf::new(),
            argv: Vec::new(),
            env: Vec::new(),
            workdir: "/".into(),
            user: String::new(),
            limits: Limits::default(),
            hardening: Hardening::default(),
            net: Net::None,
            restart: Restart::No,
            restart_sec: 1,
            ttl_seconds: None,
            after: Vec::new(),
            requires: Vec::new(),
            state_dir: PathBuf::new(),
            log_path: PathBuf::new(),
            cgroup_dir: None,
            launcher: PathBuf::from(crate::LAUNCHER),
        }
    }
}

impl ContainerSpec {
    pub fn unit_name(&self) -> String {
        format!("{}.service", self.name)
    }

    /// Reject specs the runtime cannot honour, before anything is created.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(format!(
                "container name {:?} must be alphanumeric and dashes",
                self.name
            ));
        }
        if self.argv.is_empty() {
            return Err("the container has no command: the image has no Entrypoint or Cmd and none was given".into());
        }
        if self.argv.iter().any(|a| a.contains('\0')) || self.env.iter().any(|e| e.contains('\0')) {
            return Err("argv and env must not contain NUL bytes".into());
        }
        for e in &self.env {
            if !e.contains('=') || e.starts_with('=') {
                return Err(format!("environment entry {e:?} is not KEY=value"));
            }
        }
        if !self.workdir.starts_with('/') {
            return Err(format!("workdir {:?} must be absolute", self.workdir));
        }
        if !self.user.is_empty() {
            let ok = self.user.split(':').count() <= 2
                && self
                    .user
                    .split(':')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
            if !ok {
                return Err(format!(
                    "user {:?} must be numeric uid[:gid]; names need the image's /etc/passwd, which the launcher does not read",
                    self.user
                ));
            }
        }
        if self.limits.memory_mib == Some(0)
            || self.limits.pids == Some(0)
            || self.limits.cpu_percent == Some(0)
        {
            return Err("limits must be greater than zero".into());
        }
        if let Some(w) = self.limits.io_weight
            && !(1..=10_000).contains(&w)
        {
            return Err(format!("io_weight {w} is outside 1..=10000"));
        }
        for c in &self.hardening.capabilities {
            if !c.starts_with("CAP_") || !c.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_') {
                return Err(format!("capability {c:?} is not a CAP_* name"));
            }
        }
        for u in self.after.iter().chain(&self.requires) {
            if !u.ends_with(".service") || u.contains(char::is_whitespace) {
                return Err(format!("dependency {u:?} must be a .service unit name"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> ContainerSpec {
        ContainerSpec {
            name: "fx-1a2b3c4d".into(),
            argv: vec!["/bin/true".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_minimal_spec_is_valid_and_defaults_are_safe() {
        let s = ok();
        s.validate().unwrap();
        assert!(s.hardening.read_only_root && s.hardening.no_new_privileges);
        assert_eq!(s.net, Net::None);
        assert_eq!(s.hardening.seccomp, Seccomp::Default);
        assert_eq!(s.unit_name(), "fx-1a2b3c4d.service");
    }

    #[test]
    fn refusals() {
        let mut s = ok();
        s.argv.clear();
        assert!(s.validate().unwrap_err().contains("no command"));
        let mut s = ok();
        s.user = "nobody".into();
        assert!(s.validate().unwrap_err().contains("numeric"));
        let mut s = ok();
        s.env = vec!["NOEQUALS".into()];
        assert!(s.validate().is_err());
        let mut s = ok();
        s.limits.io_weight = Some(0);
        assert!(s.validate().is_err());
        let mut s = ok();
        s.limits.memory_mib = Some(0);
        assert!(s.validate().is_err());
        let mut s = ok();
        s.hardening.capabilities = vec!["sys_admin".into()];
        assert!(s.validate().is_err());
        let mut s = ok();
        s.requires = vec!["db".into()];
        assert!(s.validate().is_err());
        let mut s = ok();
        s.name = "a b".into();
        assert!(s.validate().is_err());
        let mut s = ok();
        s.workdir = "rel".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn json_round_trip_keeps_defaults_for_missing_fields() {
        let s: ContainerSpec = serde_json::from_str(r#"{"name":"fx-a","argv":["x"]}"#).unwrap();
        assert!(s.hardening.read_only_root);
        let back: ContainerSpec =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}
