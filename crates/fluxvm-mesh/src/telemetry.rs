// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Durable public-edge telemetry and SLO evaluation.

use crate::{MeshError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EdgeTelemetry {
    pub updated_unix_ms: u64,
    pub reconcile_total: u64,
    pub reconcile_failures: u64,
    pub acme_renewals: u64,
    pub relay_connected: bool,
    pub relay_active_site: Option<String>,
    pub public_ipv4: bool,
    pub public_ipv6: bool,
    pub active_wan: Option<String>,
    pub ingress_routes: usize,
    pub last_reconcile_ms: u64,
    #[serde(default)]
    pub http3_connections: u64,
    #[serde(default)]
    pub http3_requests: u64,
    #[serde(default)]
    pub http3_failures: u64,
}
impl Default for EdgeTelemetry {
    fn default() -> Self {
        Self {
            updated_unix_ms: now_ms(),
            reconcile_total: 0,
            reconcile_failures: 0,
            acme_renewals: 0,
            relay_connected: false,
            relay_active_site: None,
            public_ipv4: false,
            public_ipv6: false,
            active_wan: None,
            ingress_routes: 0,
            last_reconcile_ms: 0,
            http3_connections: 0,
            http3_requests: 0,
            http3_failures: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TelemetryStore {
    path: PathBuf,
}
impl TelemetryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn load(&self) -> Result<EdgeTelemetry> {
        if !self.path.exists() {
            return Ok(EdgeTelemetry::default());
        }
        let b = fs::read(&self.path)?;
        serde_json::from_slice(&b)
            .map_err(|e| MeshError::InvalidState(format!("parse telemetry: {e}")))
    }
    pub fn save(&self, v: &EdgeTelemetry) -> Result<()> {
        if let Some(p) = self.path.parent() {
            fs::create_dir_all(p)?
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(v)
                .map_err(|e| MeshError::InvalidState(format!("serialize telemetry: {e}")))?,
        )?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn render_prometheus(v: &EdgeTelemetry) -> String {
    format!(
        concat!(
            "# HELP fluxmesh_edge_reconcile_total Edge reconciliation attempts.\n# TYPE fluxmesh_edge_reconcile_total counter\nfluxmesh_edge_reconcile_total {}\n",
            "# HELP fluxmesh_edge_reconcile_failures_total Failed edge reconciliation attempts.\n# TYPE fluxmesh_edge_reconcile_failures_total counter\nfluxmesh_edge_reconcile_failures_total {}\n",
            "# TYPE fluxmesh_edge_acme_renewals_total counter\nfluxmesh_edge_acme_renewals_total {}\n",
            "# TYPE fluxmesh_edge_relay_connected gauge\nfluxmesh_edge_relay_connected {}\n",
            "# TYPE fluxmesh_edge_public_ipv4 gauge\nfluxmesh_edge_public_ipv4 {}\n",
            "# TYPE fluxmesh_edge_public_ipv6 gauge\nfluxmesh_edge_public_ipv6 {}\n",
            "# TYPE fluxmesh_edge_ingress_routes gauge\nfluxmesh_edge_ingress_routes {}\n",
            "# TYPE fluxmesh_edge_last_reconcile_ms gauge\nfluxmesh_edge_last_reconcile_ms {}\n",
            "# TYPE fluxmesh_edge_http3_connections_total counter\nfluxmesh_edge_http3_connections_total {}\n",
            "# TYPE fluxmesh_edge_http3_requests_total counter\nfluxmesh_edge_http3_requests_total {}\n",
            "# TYPE fluxmesh_edge_http3_failures_total counter\nfluxmesh_edge_http3_failures_total {}\n"
        ),
        v.reconcile_total,
        v.reconcile_failures,
        v.acme_renewals,
        u8::from(v.relay_connected),
        u8::from(v.public_ipv4),
        u8::from(v.public_ipv6),
        v.ingress_routes,
        v.last_reconcile_ms,
        v.http3_connections,
        v.http3_requests,
        v.http3_failures
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SloReport {
    pub healthy: bool,
    pub reasons: Vec<String>,
}
pub fn evaluate_slo(v: &EdgeTelemetry, max_stale_ms: u64, max_failure_ratio: f64) -> SloReport {
    let mut r = Vec::new();
    let age = now_ms().saturating_sub(v.updated_unix_ms);
    if age > max_stale_ms {
        r.push(format!("telemetry stale for {age}ms"));
    }
    if v.reconcile_total >= 10 {
        let ratio = v.reconcile_failures as f64 / v.reconcile_total as f64;
        if ratio > max_failure_ratio {
            r.push(format!(
                "reconcile failure ratio {:.3} exceeds {:.3}",
                ratio, max_failure_ratio
            ));
        }
    }
    SloReport {
        healthy: r.is_empty(),
        reasons: r,
    }
}
pub fn default_telemetry_path() -> PathBuf {
    crate::mesh_dir().join("edge-telemetry.json")
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prometheus_has_counters() {
        let s = render_prometheus(&EdgeTelemetry::default());
        assert!(s.contains("fluxmesh_edge_reconcile_total"));
    }
    #[test]
    fn high_failure_ratio_fails() {
        let mut v = EdgeTelemetry::default();
        v.reconcile_total = 20;
        v.reconcile_failures = 10;
        assert!(!evaluate_slo(&v, 60_000, 0.1).healthy);
    }
}
