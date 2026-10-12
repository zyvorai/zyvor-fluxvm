// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! End-to-end personal-cloud diagnostics with actionable pass/warn/fail items.

use crate::StateStore;
use crate::public::PublicConfigStore;
use crate::public_relay;
use crate::telemetry::{TelemetryStore, default_telemetry_path, evaluate_slo};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticItem {
    pub name: String,
    pub severity: Severity,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticReport {
    pub items: Vec<DiagnosticItem>,
}

impl DiagnosticReport {
    pub fn push(&mut self, name: &str, severity: Severity, detail: impl Into<String>) {
        self.items.push(DiagnosticItem {
            name: name.into(),
            severity,
            detail: detail.into(),
        });
    }
    pub fn failed(&self) -> bool {
        self.items.iter().any(|v| v.severity == Severity::Fail)
    }
}

#[derive(Clone, Debug)]
pub struct DiagnoseConfig {
    pub state: PathBuf,
    pub public_config: PathBuf,
    pub wireguard_key: PathBuf,
    pub cert: PathBuf,
    pub cert_key: PathBuf,
    pub share_secret: PathBuf,
    pub ingress: Option<SocketAddr>,
}

pub fn run(cfg: &DiagnoseConfig) -> DiagnosticReport {
    let mut report = DiagnosticReport::default();
    match StateStore::new(&cfg.state).load() {
        Ok(mesh) => report.push(
            "mesh-state",
            Severity::Pass,
            format!("local={} nodes={}", mesh.local_node, mesh.nodes.len()),
        ),
        Err(e) => report.push("mesh-state", Severity::Fail, e.to_string()),
    }
    match PublicConfigStore::new(&cfg.public_config).load() {
        Ok(public) => {
            report.push(
                "public-config",
                Severity::Pass,
                format!(
                    "ingress={} wan={} ddns={} router={} relay={}",
                    public.ingress.len(),
                    public.wan.len(),
                    public.ddns.is_some(),
                    public.router.is_some(),
                    public.public_relay.is_some()
                ),
            );
            if public.public_relay.is_some() {
                let hb = crate::mesh_dir().join("public-relay-heartbeat");
                let fresh = public_relay::heartbeat_fresh(&hb, Duration::from_secs(45));
                report.push(
                    "public-relay",
                    if fresh {
                        Severity::Pass
                    } else {
                        Severity::Warn
                    },
                    if fresh {
                        String::from("outbound relay tunnel heartbeat is fresh")
                    } else {
                        String::from("public relay is configured but heartbeat is stale/missing")
                    },
                );
            }
        }
        Err(e) => report.push("public-config", Severity::Fail, e.to_string()),
    }
    if let Ok(v) = TelemetryStore::new(default_telemetry_path()).load() {
        let slo = evaluate_slo(&v, 120_000, 0.20);
        report.push(
            "edge-slo",
            if slo.healthy {
                Severity::Pass
            } else {
                Severity::Warn
            },
            if slo.healthy {
                "edge SLO healthy".into()
            } else {
                slo.reasons.join("; ")
            },
        );
    }
    check_file(&mut report, "wireguard-key", &cfg.wireguard_key, true);
    check_file(&mut report, "tls-certificate", &cfg.cert, false);
    check_file(&mut report, "tls-private-key", &cfg.cert_key, true);
    check_file(&mut report, "share-secret", &cfg.share_secret, true);
    check_binary(&mut report, "wg");
    if cfg!(target_os = "linux") {
        check_binary(&mut report, "ip");
    }
    if let Some(addr) = cfg.ingress {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
            Ok(_) => report.push(
                "https-ingress",
                Severity::Pass,
                format!("TCP {addr} reachable"),
            ),
            Err(e) => report.push(
                "https-ingress",
                Severity::Warn,
                format!("TCP {addr} not reachable: {e}"),
            ),
        }
    }
    report
}

fn check_file(report: &mut DiagnosticReport, name: &str, path: &Path, private: bool) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if !meta.is_file() {
                report.push(
                    name,
                    Severity::Fail,
                    format!("{} is not a file", path.display()),
                );
                return;
            }
            #[cfg(unix)]
            if private {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    report.push(
                        name,
                        Severity::Warn,
                        format!("{} mode {:o} is broader than 0600", path.display(), mode),
                    );
                    return;
                }
            }
            report.push(name, Severity::Pass, path.display().to_string());
        }
        Err(e) => report.push(name, Severity::Warn, format!("{}: {e}", path.display())),
    }
}

fn check_binary(report: &mut DiagnosticReport, name: &str) {
    match Command::new(name).arg("--version").output() {
        Ok(output) if output.status.success() => report.push(
            &format!("binary-{name}"),
            Severity::Pass,
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or(name)
                .to_string(),
        ),
        Ok(output) => report.push(
            &format!("binary-{name}"),
            Severity::Warn,
            format!("exit {}", output.status),
        ),
        Err(e) => report.push(&format!("binary-{name}"), Severity::Warn, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_only_counts_hard_fail() {
        let mut report = DiagnosticReport::default();
        report.push("x", Severity::Warn, "w");
        assert!(!report.failed());
        report.push("y", Severity::Fail, "f");
        assert!(report.failed());
    }
}
