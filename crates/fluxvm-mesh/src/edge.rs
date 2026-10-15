// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Public-edge reconciler for personal-cloud ingress.

use crate::acme::{self, AcmeClient};
use crate::ddns;
use crate::http3;
use crate::ingress::{ChallengeStore, serve_http01, serve_https};
use crate::public::{AcmeChallenge, PublicConfigStore};
use crate::public_relay::{self, RelayClientConfig};
use crate::router;
use crate::stun::probe_many;
use crate::telemetry::{EdgeTelemetry, TelemetryStore};
use crate::trace::{self, EdgeSpan};
use crate::wan::{WanState, linux_policy_plan};
use crate::{EndpointKind, MeshError, Result, StateStore};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct EdgeConfig {
    pub state_path: PathBuf,
    pub public_config_path: PathBuf,
    pub share_secret: PathBuf,
    pub acme_account_key: PathBuf,
    pub acme_marker: PathBuf,
    pub cert_path: PathBuf,
    pub cert_key_path: PathBuf,
    pub challenge_dir: PathBuf,
    pub https_listen: SocketAddr,
    /// UDP listener for standards-compliant HTTP/3. None disables HTTP/3.
    pub http3_listen: Option<SocketAddr>,
    pub http01_listen: SocketAddr,
    pub interval: Duration,
    pub stun_bind: SocketAddr,
    pub stun_servers: Vec<SocketAddr>,
    pub dry_run: bool,
    pub telemetry_path: PathBuf,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            state_path: crate::default_state_path(),
            public_config_path: crate::public::default_public_config_path(),
            share_secret: crate::share::default_share_secret_path(),
            acme_account_key: acme::default_account_key_path(),
            acme_marker: acme::default_issued_marker_path(),
            cert_path: acme::default_certificate_path(),
            cert_key_path: acme::default_certificate_key_path(),
            challenge_dir: crate::ingress::default_challenge_dir(),
            https_listen: "0.0.0.0:443".parse().unwrap(),
            http3_listen: Some("[::]:443".parse().unwrap()),
            http01_listen: "0.0.0.0:80".parse().unwrap(),
            interval: Duration::from_secs(30),
            stun_bind: "0.0.0.0:0".parse().unwrap(),
            stun_servers: Vec::new(),
            dry_run: false,
            telemetry_path: crate::telemetry::default_telemetry_path(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct EdgeReport {
    pub acme_renewed: bool,
    pub ddns_v4: bool,
    pub ddns_v6: bool,
    pub router_mappings: usize,
    pub active_wan: Option<String>,
    pub wan_commands: usize,
    pub public_ipv4: bool,
    pub public_ipv6: bool,
    pub ingress_routes: usize,
}

pub async fn reconcile_once(cfg: &EdgeConfig, wan_state: &mut WanState) -> Result<EdgeReport> {
    let public = PublicConfigStore::new(&cfg.public_config_path).load()?;
    let mesh = StateStore::new(&cfg.state_path).load()?;
    let mut report = EdgeReport {
        ingress_routes: public.ingress.len(),
        ..EdgeReport::default()
    };

    // Converge the active uplink before probing public reachability.
    if !public.wan.is_empty() {
        wan_state.probe(&public.wan, Duration::from_secs(2));
        if let Some(active) = wan_state.select(&public.wan) {
            report.active_wan = Some(active.name.clone());
            let plan = linux_policy_plan(&public.wan, &active.name)?;
            report.wan_commands = plan.len();
            if cfg!(target_os = "linux") && !cfg.dry_run {
                for argv in plan {
                    run_argv(&argv)?;
                }
            }
        }
    }

    let (v4, v6) = discover_public_addresses(cfg, &mesh).await?;
    report.public_ipv4 = v4.is_some();
    report.public_ipv6 = v6.is_some();

    // Publish the current edge addresses before attempting HTTP-01.
    if let Some(ddns_cfg) = &public.ddns {
        if !cfg.dry_run {
            let synced = ddns::sync(ddns_cfg, v4, v6).await?;
            report.ddns_v4 = synced.updated_v4;
            report.ddns_v6 = synced.updated_v6;
        } else {
            report.ddns_v4 = v4.is_some();
            report.ddns_v6 = v6.is_some();
        }
    }

    // Establish/refresh home-router mappings before the ACME server reaches
    // the HTTP-01 listener. Operators normally configure mappings for 80/443.
    if let Some(router_cfg) = &public.router {
        if cfg.dry_run {
            report.router_mappings = router_cfg.mappings.len();
        } else {
            report.router_mappings = router::reconcile(router_cfg).await?.len();
        }
    }

    if let Some(acme_cfg) = &public.acme {
        // Public ACME certificates are commonly 90 days. Renew at the
        // operator-requested margin instead of ignoring renew_before_days.
        let usable_days = 90u64.saturating_sub(u64::from(acme_cfg.renew_before_days.clamp(1, 89)));
        let max_age = Duration::from_secs(usable_days * 86_400);
        if (!cfg.cert_path.exists() || acme::renewal_due(&cfg.acme_marker, max_age)) && !cfg.dry_run
        {
            let mut client =
                AcmeClient::connect(&acme_cfg.directory_url, &cfg.acme_account_key).await?;
            match acme_cfg.challenge {
                AcmeChallenge::Http01 => {
                    let store = ChallengeStore::new(&cfg.challenge_dir);
                    client
                        .obtain_http01(
                            &acme_cfg.email,
                            &acme_cfg.domains,
                            &store,
                            &cfg.cert_path,
                            &cfg.cert_key_path,
                        )
                        .await?;
                }
                AcmeChallenge::Dns01 => {
                    let dns = acme_cfg.dns01.as_ref().ok_or_else(|| {
                        MeshError::InvalidState("DNS-01 ACME missing provider config".into())
                    })?;
                    client
                        .obtain_dns01(
                            &acme_cfg.email,
                            &acme_cfg.domains,
                            dns,
                            &cfg.cert_path,
                            &cfg.cert_key_path,
                        )
                        .await?;
                }
            }
            acme::mark_issued(&cfg.acme_marker)?;
            report.acme_renewed = true;
        }
    }

    Ok(report)
}

pub async fn run(cfg: EdgeConfig) -> Result<()> {
    let challenge_store = ChallengeStore::new(cfg.challenge_dir.clone());
    let http_listen = cfg.http01_listen;
    tokio::spawn(async move {
        let _ = serve_http01(http_listen, challenge_store).await;
    });

    // The HTTPS loop reloads cert/key for each new connection, so renewals from
    // the reconciler become active without a daemon restart.
    let https = cfg.https_listen;
    let public = cfg.public_config_path.clone();
    let share = cfg.share_secret.clone();
    let cert = cfg.cert_path.clone();
    let key = cfg.cert_key_path.clone();
    tokio::spawn(async move {
        let _ = serve_https(https, public, share, cert, key).await;
    });

    if let Some(http3_listen) = cfg.http3_listen {
        let public = cfg.public_config_path.clone();
        let share = cfg.share_secret.clone();
        let cert = cfg.cert_path.clone();
        let key = cfg.cert_key_path.clone();
        tokio::spawn(async move {
            if let Err(e) = http3::serve_http3(http3_listen, public, share, cert, key).await {
                eprintln!("fluxmesh-edge: HTTP/3 disabled after error: {e}");
            }
        });
    }

    if let Ok(public_cfg) = PublicConfigStore::new(&cfg.public_config_path).load()
        && let Some(relay) = public_cfg.public_relay
    {
        let client = RelayClientConfig {
            relay_addr: relay.relay_addr,
            quic_relay_addr: relay.quic_relay_addr,
            transport: relay.transport,
            server_name: relay.server_name,
            ca_file: relay.ca_file,
            site_id: relay.site_id,
            secret_file: relay.secret_file,
            local_ingress: relay.local_ingress,
            heartbeat_file: crate::mesh_dir().join("public-relay-heartbeat"),
        };
        tokio::spawn(async move {
            let _ = public_relay::run_client(client).await;
        });
    }

    let telemetry = TelemetryStore::new(&cfg.telemetry_path);
    let mut stats = telemetry
        .load()
        .unwrap_or_else(|_| EdgeTelemetry::default());
    let mut wan_state = WanState::default();
    loop {
        let started = std::time::Instant::now();
        stats.reconcile_total = stats.reconcile_total.saturating_add(1);
        let span_start = now_ns();
        let mut trace_ok = true;
        let mut trace_error = None::<String>;
        match reconcile_once(&cfg, &mut wan_state).await {
            Ok(r) => {
                if r.acme_renewed {
                    stats.acme_renewals = stats.acme_renewals.saturating_add(1);
                }
                stats.public_ipv4 = r.public_ipv4;
                stats.public_ipv6 = r.public_ipv6;
                stats.active_wan = r.active_wan.clone();
                stats.ingress_routes = r.ingress_routes;
                eprintln!(
                    "fluxmesh-edge: acme={} ddns4={} ddns6={} mappings={} wan={} commands={}",
                    r.acme_renewed,
                    r.ddns_v4,
                    r.ddns_v6,
                    r.router_mappings,
                    r.active_wan.as_deref().unwrap_or("none"),
                    r.wan_commands
                );
            }
            Err(e) => {
                trace_ok = false;
                trace_error = Some(e.to_string());
                stats.reconcile_failures = stats.reconcile_failures.saturating_add(1);
                eprintln!("fluxmesh-edge: reconcile failed: {e}");
            }
        }
        stats.updated_unix_ms = now_ms();
        stats.last_reconcile_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        stats.relay_connected = public_relay::heartbeat_fresh(
            crate::mesh_dir().join("public-relay-heartbeat").as_path(),
            Duration::from_secs(45),
        );
        let h3 = http3::counters();
        stats.http3_connections = h3.connections;
        stats.http3_requests = h3.requests;
        stats.http3_failures = h3.failures;
        let _ = telemetry.save(&stats);
        if let Ok(public_cfg) = PublicConfigStore::new(&cfg.public_config_path).load()
            && let Some(otlp) = public_cfg.otlp
        {
            let mut attributes = std::collections::BTreeMap::new();
            attributes.insert("relay.connected".into(), stats.relay_connected.to_string());
            attributes.insert("http3.requests".into(), stats.http3_requests.to_string());
            if let Some(error) = trace_error {
                attributes.insert("error.message".into(), error);
            }
            let span = EdgeSpan {
                name: "fluxmesh.edge.reconcile".into(),
                start_unix_nano: span_start,
                end_unix_nano: now_ns(),
                ok: trace_ok,
                attributes,
            };
            if let Err(e) = trace::export(&otlp, span).await {
                eprintln!("fluxmesh-edge: OTLP export failed: {e}");
            }
        }
        tokio::time::sleep(cfg.interval).await;
    }
}

async fn discover_public_addresses(
    cfg: &EdgeConfig,
    mesh: &crate::MeshState,
) -> Result<(Option<IpAddr>, Option<IpAddr>)> {
    let mut v4 = None;
    if !cfg.stun_servers.is_empty() {
        let servers = cfg.stun_servers.clone();
        let bind = cfg.stun_bind;
        let report =
            tokio::task::spawn_blocking(move || probe_many(&servers, bind, Duration::from_secs(2)))
                .await
                .map_err(|e| MeshError::InvalidState(format!("STUN task: {e}")))??;
        v4 = report
            .public_endpoint()
            .map(|s| s.ip())
            .filter(IpAddr::is_ipv4);
    }
    let local = mesh.node(&mesh.local_node)?;
    let v6 = local
        .endpoints
        .iter()
        .find(|e| e.kind == EndpointKind::PublicIpv6)
        .map(|e| e.addr.ip())
        .filter(IpAddr::is_ipv6)
        .or_else(|| {
            std::env::var("FLUXMESH_PUBLIC_IPV6")
                .ok()
                .and_then(|v| v.parse().ok())
        });
    Ok((v4, v6))
}
fn run_argv(argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        return Ok(());
    }
    let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
    let harmless_missing_rule =
        argv.get(1).is_some_and(|v| v == "rule") && argv.get(2).is_some_and(|v| v == "del");
    if !status.success() && !harmless_missing_rule {
        return Err(MeshError::InvalidState(format!(
            "command {:?} failed with {status}",
            argv
        )));
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_edge_ports_are_public_defaults() {
        let c = EdgeConfig::default();
        assert_eq!(c.https_listen.port(), 443);
        assert_eq!(c.http3_listen.unwrap().port(), 443);
        assert_eq!(c.http01_listen.port(), 80);
    }
}
