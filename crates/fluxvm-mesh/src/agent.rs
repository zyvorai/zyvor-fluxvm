// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! FluxMesh runtime reconciler.
//!
//! The agent re-reads desired state every cycle. Linux reconciles a dedicated
//! WireGuard interface; macOS writes the signed NetworkExtension handoff plan.
//! Service and Network Fabric intent is fanned into the local FluxVM daemon.

use crate::catalog::{CatalogStore, default_catalog_path};
use crate::fabric::{default_ledger_path, reconcile_fluxvm_fabrics};
use crate::http::HttpEndpoint;
use crate::ipv6::augment_wireguard_plan;
use crate::keydist::{
    KeyDistributionStore, RotationStep, coordinated_local_rotation, default_keydist_path,
};
use crate::macos::{MacTunnelPlan, default_plan_path};
use crate::reconcile::reconcile_linux;
use crate::stun::{StunReport, probe_many};
use crate::{Endpoint, EndpointKind, MeshState, Result, StateStore, default_state_path};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub state_path: PathBuf,
    pub catalog_path: PathBuf,
    pub ledger_path: PathBuf,
    pub api_url: Option<String>,
    pub api_token: Option<String>,
    pub interface: String,
    pub listen_port: u16,
    pub private_key_file: PathBuf,
    pub interval: Duration,
    pub key_max_age: Option<Duration>,
    pub keydist_path: PathBuf,
    pub stun_bind: SocketAddr,
    pub stun_servers: Vec<SocketAddr>,
    pub stun_timeout: Duration,
    pub macos_plan_path: PathBuf,
    /// Per-peer local UDP endpoint owned by `fluxmesh-relay client-tls`.
    /// These override direct endpoints in the WireGuard/macOS plan.
    pub relay_bindings: BTreeMap<String, SocketAddr>,
    pub dry_run: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            state_path: default_state_path(),
            catalog_path: default_catalog_path(),
            ledger_path: default_ledger_path(),
            api_url: std::env::var("FLUXVM_URL").ok(),
            api_token: std::env::var("FLUXVM_TOKEN").ok(),
            interface: "fluxmesh0".into(),
            listen_port: 51820,
            private_key_file: crate::mesh_dir().join("wireguard.key"),
            interval: Duration::from_secs(15),
            // Rotation is opt-in until the deployment has a key-distribution
            // control plane. Unilateral default rotation would strand peers.
            key_max_age: None,
            keydist_path: default_keydist_path(),
            stun_bind: "0.0.0.0:0".parse().expect("valid STUN bind"),
            stun_servers: Vec::new(),
            stun_timeout: Duration::from_secs(2),
            macos_plan_path: default_plan_path(),
            relay_bindings: BTreeMap::new(),
            dry_run: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AgentReport {
    pub key_rotated: bool,
    pub stun: Option<StunReport>,
    pub stun_error: Option<String>,
    pub direct_endpoint_persisted: bool,
    pub wireguard_commands: usize,
    pub fabric_services: usize,
    pub fabric_cnps: usize,
    pub macos_plan_written: bool,
}

pub fn reconcile_once(cfg: &AgentConfig) -> Result<AgentReport> {
    let store = StateStore::new(&cfg.state_path);
    let mut mesh = store.load()?;
    let mut report = AgentReport::default();

    if cfg!(target_os = "linux") {
        if let Some(max_age) = cfg.key_max_age {
            let key_store = KeyDistributionStore::new(&cfg.keydist_path);
            match coordinated_local_rotation(
                &store,
                &key_store,
                &cfg.private_key_file,
                max_age,
                cfg.dry_run,
            )? {
                RotationStep::Activated { .. } => {
                    report.key_rotated = true;
                    if !cfg.dry_run {
                        mesh = store.load()?;
                    }
                }
                RotationStep::NotDue
                | RotationStep::Proposed { .. }
                | RotationStep::AwaitingAcks { .. } => {}
            }
        }

        match run_stun_observation(cfg, &mut mesh) {
            Ok(stun) => report.stun = stun,
            Err(e) => report.stun_error = Some(e.to_string()),
        }
        if report.stun.is_some() && !cfg.dry_run {
            store.save(&mesh)?;
        }
        report.direct_endpoint_persisted = report
            .stun
            .as_ref()
            .and_then(StunReport::public_endpoint)
            .is_some_and(|addr| {
                cfg.stun_bind.port() == cfg.listen_port
                    && addr.port() != 0
                    && mesh.node(&mesh.local_node).is_ok_and(|node| {
                        node.endpoints.iter().any(|endpoint| {
                            endpoint.kind == EndpointKind::PublicUdp && endpoint.addr == addr
                        })
                    })
            });

        let mut plan = mesh.wireguard_plan(
            &cfg.interface,
            cfg.listen_port,
            cfg.private_key_file.clone(),
        )?;
        augment_wireguard_plan(&mesh, &mut plan)?;
        for peer in &mut plan.peers {
            if let Some(endpoint) = cfg.relay_bindings.get(&peer.node_id) {
                peer.endpoint = Some(*endpoint);
                peer.persistent_keepalive = Some(25);
            }
        }
        let wg = reconcile_linux(&plan, cfg.dry_run)?;
        report.wireguard_commands = if cfg.dry_run {
            wg.commands.len()
        } else {
            wg.applied
        };
    } else if cfg!(target_os = "macos") {
        let catalog = CatalogStore::new(&cfg.catalog_path).load(&mesh)?;
        let mut plan = MacTunnelPlan::from_mesh(&mesh, cfg.listen_port, catalog.dns_suffix)?;
        for peer in &mut plan.peers {
            if let Some(endpoint) = cfg.relay_bindings.get(&peer.node_id) {
                peer.endpoint = Some(endpoint.to_string());
                peer.persistent_keepalive = 25;
            }
        }
        if !cfg.dry_run {
            plan.write_atomic(&cfg.macos_plan_path)?;
            report.macos_plan_written = true;
        }
    }

    if let Some(url) = &cfg.api_url {
        let catalog = CatalogStore::new(&cfg.catalog_path).load(&mesh)?;
        if !cfg.dry_run {
            let api = HttpEndpoint::parse(url)?;
            let fabrics = reconcile_fluxvm_fabrics(
                &api,
                cfg.api_token.as_deref(),
                &mesh,
                &catalog,
                &cfg.ledger_path,
            )?;
            report.fabric_services = fabrics.services_upserted + fabrics.services_deleted;
            report.fabric_cnps = fabrics.cnps_upserted + fabrics.cnps_deleted;
        } else {
            report.fabric_services = catalog.services.services.len();
            report.fabric_cnps = 1;
        }
    }

    Ok(report)
}

pub fn run_forever(cfg: AgentConfig) -> ! {
    loop {
        match reconcile_once(&cfg) {
            Ok(report) => eprintln!(
                "fluxmesh-agent: reconcile ok wg={} services={} cnp={} stun={} stun_error={} key_rotated={} mac_plan={}",
                report.wireguard_commands,
                report.fabric_services,
                report.fabric_cnps,
                report.stun.is_some(),
                report.stun_error.as_deref().unwrap_or("none"),
                report.key_rotated,
                report.macos_plan_written,
            ),
            Err(e) => eprintln!("fluxmesh-agent: reconcile failed: {e}"),
        }
        std::thread::sleep(cfg.interval);
    }
}

fn run_stun_observation(cfg: &AgentConfig, mesh: &mut MeshState) -> Result<Option<StunReport>> {
    if cfg.stun_servers.is_empty() {
        return Ok(None);
    }
    let report = probe_many(&cfg.stun_servers, cfg.stun_bind, cfg.stun_timeout)?;
    // An ephemeral STUN source port is useful for NAT classification but not a
    // valid WireGuard endpoint. Publish only when the probe used the exact WG
    // listen port and the NAT mapping is endpoint-independent.
    if cfg.stun_bind.port() == cfg.listen_port
        && report.nat_type == crate::NatType::EndpointIndependent
    {
        if let Some(mapped) = report.public_endpoint() {
            let local = mesh.local_node.clone();
            mesh.node_mut(&local)?
                .endpoints
                .retain(|endpoint| endpoint.kind != EndpointKind::PublicUdp);
            mesh.add_endpoint(
                &local,
                Endpoint {
                    kind: EndpointKind::PublicUdp,
                    addr: mapped,
                    priority: 30,
                },
            )?;
        }
    }
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_stun_ephemeral_until_explicitly_bound_to_wg_port() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.stun_bind.port(), 0);
        assert_eq!(cfg.listen_port, 51820);
    }
}
