// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Outbound policy for the QEMU process of a VM (user-mode networking). The types live in
//! core so a create request can carry one; `fluxvm-network::qemu_egress` programs the kernel.

use serde::{Deserialize, Serialize};

/// How the QEMU process's outbound IP traffic is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    /// Only loopback leaves the cgroup. The pre-policy behaviour and the default.
    #[default]
    Loopback,
    /// Loopback plus the rules.
    Allowlist,
    /// Allow everything, but record what an allow-list would have dropped.
    Audit,
    /// Allow everything.
    Open,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressProto {
    #[default]
    Any,
    Tcp,
    Udp,
}

/// One allowed destination. `port: None` allows every port (and, with `proto: any`, every protocol).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressRule {
    /// IPv4 CIDR or bare address (`10.0.0.0/8`, `192.168.1.5`).
    pub cidr: String,
    #[serde(default)]
    pub proto: EgressProto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Free text shown in UIs ("Veyron API", "Ubuntu mirror").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The whole policy for one VM (or the node default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QemuEgressPolicy {
    pub mode: EgressMode,
    pub rules: Vec<EgressRule>,
}

impl std::str::FromStr for EgressMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "loopback" => Ok(EgressMode::Loopback),
            "allowlist" => Ok(EgressMode::Allowlist),
            "audit" => Ok(EgressMode::Audit),
            "open" => Ok(EgressMode::Open),
            other => Err(format!(
                "unknown egress mode {other:?}; use loopback, allowlist, audit or open"
            )),
        }
    }
}
