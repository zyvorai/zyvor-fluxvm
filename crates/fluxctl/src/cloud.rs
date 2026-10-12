// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl cloud`: one-command FluxMesh personal-cloud UX.

use anyhow::Result;
use clap::{Args, Subcommand};
use fluxvm_mesh::acme::AcmeClient;
use fluxvm_mesh::cloud::{
    CloudPaths, accept_join_token, issue_join_token, issue_open_join_token, issue_share_token,
    public_store, status, up,
};
use fluxvm_mesh::ddns;
use fluxvm_mesh::diagnose::{self, DiagnoseConfig, Severity};
use fluxvm_mesh::enroll_tls;
use fluxvm_mesh::http3::serve_http3;
use fluxvm_mesh::ingress::{ChallengeStore, serve_http01, serve_https};
use fluxvm_mesh::public::{
    AcmeChallenge, AcmeConfig, DdnsConfig, DdnsProvider, Dns01Config, Dns01Provider, IngressRoute,
    OtlpConfig, PortMapping, PublicRelayConfig, RouterConfig, RouterProtocol, WanConfig,
};
use fluxvm_mesh::public_relay::{
    RelayClientConfig, RelayTransport, run_client as run_public_relay,
};
use fluxvm_mesh::reconcile::{rotate_wireguard_key, wireguard_public_key};
use fluxvm_mesh::router;
use fluxvm_mesh::telemetry::{
    TelemetryStore, default_telemetry_path, evaluate_slo, render_prometheus,
};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct CloudArgs {
    #[command(subcommand)]
    command: CloudCommand,
}

#[derive(Subcommand)]
enum CloudCommand {
    /// Bootstrap this host as a personal-cloud node. Idempotent if already initialized.
    Up {
        node: String,
        hostname: String,
        #[arg(long)]
        pool: Option<String>,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Show the local overlay, dual-stack address and cloud inventory.
    Status {
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Create a short-lived one-time invitation. By default the joining host
    /// creates its own WireGuard key; --public-key can bind the invite to a
    /// pre-provisioned key instead.
    Invite {
        node: String,
        hostname: String,
        #[arg(long)]
        public_key: Option<String>,
        #[arg(long, default_value_t = 900)]
        ttl_seconds: u64,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Consume an invitation locally. This is useful for air-gapped/manual
    /// enrollment where the token was bound to a known public key.
    Accept {
        token: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Serve the verified-TLS zero-touch enrollment endpoint. A joining host
    /// presents only its one-time invitation and self-generated public key.
    ServeEnrollment {
        #[arg(long, default_value = "0.0.0.0:7443")]
        listen: SocketAddr,
        #[arg(long)]
        cert: PathBuf,
        /// The private key comes from the shared `--tls-key` path option
        /// (`FLUXMESH_TLS_KEY`, default `<mesh dir>/tls/key.pem`).
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Join an existing personal cloud over verified TLS. Generates a local
    /// WireGuard key if needed and atomically installs the returned mesh state.
    Join {
        controller: SocketAddr,
        #[arg(long)]
        server_name: String,
        #[arg(long)]
        ca: PathBuf,
        token: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Publish or update an HTTPS ingress hostname to a local/mesh backend.
    Expose {
        name: String,
        hostname: String,
        backend: SocketAddr,
        #[arg(long)]
        share_required: bool,
        #[arg(long)]
        strip_share_prefix: bool,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Remove a public ingress route.
    Unexpose {
        name: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Create a short-lived share URL token for an ingress route.
    Share {
        route: String,
        #[arg(long, default_value_t = 3600)]
        ttl_seconds: u64,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Run the TLS reverse ingress; optionally also expose the ACME HTTP-01 listener.
    ServeIngress {
        #[arg(long, default_value = "0.0.0.0:443")]
        listen: SocketAddr,
        #[arg(long, default_value = "[::]:443")]
        http3_listen: SocketAddr,
        #[arg(long)]
        http01_listen: Option<SocketAddr>,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Obtain/renew an ACME certificate using native HTTP-01.
    Acme {
        #[arg(long, default_value = "https://acme-v02.api.letsencrypt.org/directory")]
        directory: String,
        #[arg(long)]
        email: String,
        #[arg(long = "domain", required = true)]
        domains: Vec<String>,
        #[arg(long, default_value = "0.0.0.0:80")]
        http_listen: SocketAddr,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Obtain/renew an ACME certificate with DNS-01 via Cloudflare. Supports wildcard names and CGNAT.
    AcmeDns01Cloudflare {
        #[arg(long, default_value = "https://acme-v02.api.letsencrypt.org/directory")]
        directory: String,
        #[arg(long)]
        email: String,
        #[arg(long = "domain", required = true)]
        domains: Vec<String>,
        #[arg(long)]
        zone_id: String,
        #[arg(long, default_value = "CLOUDFLARE_API_TOKEN")]
        token_env: String,
        #[arg(long, default_value_t = 20)]
        propagation_seconds: u32,
        /// Recursive resolver used to verify the TXT value before notifying ACME.
        #[arg(long)]
        validation_resolver: Option<SocketAddr>,
        /// Require DNSSEC Authenticated Data (AD=1) from --validation-resolver.
        #[arg(long)]
        require_dnssec_ad: bool,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Configure the outbound CGNAT-safe public relay. `fluxmesh-edge run`
    /// starts the client automatically on its next restart.
    RelayConfigure {
        relay_addr: SocketAddr,
        #[arg(long)]
        quic_relay_addr: Option<SocketAddr>,
        #[arg(long, default_value = "auto", value_parser = ["auto", "quic", "tls-tcp"])]
        transport: String,
        #[arg(long)]
        server_name: String,
        #[arg(long)]
        ca: PathBuf,
        #[arg(long)]
        site_id: String,
        #[arg(long)]
        secret: PathBuf,
        #[arg(long, default_value = "127.0.0.1:443")]
        local_ingress: SocketAddr,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Run the configured outbound public-relay connector in the foreground.
    RelayConnect {
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Configure OTLP/HTTP JSON export for edge reconciliation and ingress spans.
    Otlp {
        endpoint: String,
        #[arg(long, default_value = "fluxmesh-edge")]
        service_name: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Print Prometheus edge metrics and evaluate the default personal-cloud SLO.
    Telemetry {
        #[arg(long)]
        telemetry: Option<PathBuf>,
    },
    /// Configure Cloudflare DDNS. The API token is read from --token-env.
    DdnsCloudflare {
        hostname: String,
        #[arg(long)]
        zone_id: String,
        #[arg(long)]
        record_id_v4: Option<String>,
        #[arg(long)]
        record_id_v6: Option<String>,
        #[arg(long, default_value = "CLOUDFLARE_API_TOKEN")]
        token_env: String,
        #[arg(long, default_value_t = 120)]
        ttl: u32,
        #[arg(long)]
        proxied: bool,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Configure a provider-neutral DDNS webhook.
    DdnsWebhook {
        hostname: String,
        url: String,
        #[arg(long)]
        token_env: Option<String>,
        #[arg(long, default_value_t = 120)]
        ttl: u32,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Push the current public address to the configured DDNS provider.
    DdnsSync {
        #[arg(long)]
        ipv4: Option<IpAddr>,
        #[arg(long)]
        ipv6: Option<IpAddr>,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Add/update one public router mapping. Protocol: auto, pcp, nat-pmp, upnp.
    RouterMap {
        gateway: IpAddr,
        name: String,
        transport: String,
        internal_port: u16,
        external_port: u16,
        #[arg(long, default_value = "auto")]
        protocol: String,
        #[arg(long, default_value_t = 3600)]
        lifetime_seconds: u32,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Reconcile configured PCP/NAT-PMP/UPnP port mappings.
    RouterSync {
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Add/update a WAN uplink used by health-based failover.
    WanAdd {
        name: String,
        interface: String,
        #[arg(long)]
        gateway: Option<IpAddr>,
        #[arg(long, default_value_t = 100)]
        metric: u32,
        #[arg(long)]
        preferred: bool,
        #[arg(long = "probe")]
        probe_targets: Vec<SocketAddr>,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Remove a WAN uplink.
    WanRemove {
        name: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Run end-to-end cloud diagnostics.
    Diagnose {
        #[arg(long)]
        ingress: Option<SocketAddr>,
        #[command(flatten)]
        paths: PathArgs,
    },
}

#[derive(Args, Clone, Default)]
struct PathArgs {
    #[arg(long, env = "FLUXMESH_STATE")]
    state: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_CATALOG")]
    catalog: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_KEY")]
    key: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ENROLL_SECRET")]
    enroll_secret: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ENROLL_NONCES")]
    used_nonces: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_KEYDIST")]
    keydist: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_PUBLIC_CONFIG")]
    public_config: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_SHARE_SECRET")]
    share_secret: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ACME_ACCOUNT_KEY")]
    acme_account_key: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ACME_MARKER")]
    acme_marker: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_TLS_CERT")]
    tls_cert: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_TLS_KEY")]
    tls_key: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_CHALLENGE_DIR")]
    challenge_dir: Option<PathBuf>,
}

impl PathArgs {
    fn resolve(self) -> CloudPaths {
        let mut p = CloudPaths::default();
        if let Some(v) = self.state {
            p.state = v;
        }
        if let Some(v) = self.catalog {
            p.catalog = v;
        }
        if let Some(v) = self.key {
            p.key = v;
        }
        if let Some(v) = self.enroll_secret {
            p.enroll_secret = v;
        }
        if let Some(v) = self.used_nonces {
            p.used_nonces = v;
        }
        if let Some(v) = self.keydist {
            p.keydist = v;
        }
        if let Some(v) = self.public_config {
            p.public_config = v;
        }
        if let Some(v) = self.share_secret {
            p.share_secret = v;
        }
        if let Some(v) = self.acme_account_key {
            p.acme_account_key = v;
        }
        if let Some(v) = self.acme_marker {
            p.acme_marker = v;
        }
        if let Some(v) = self.tls_cert {
            p.tls_cert = v;
        }
        if let Some(v) = self.tls_key {
            p.tls_key = v;
        }
        if let Some(v) = self.challenge_dir {
            p.challenge_dir = v;
        }
        p
    }
}

pub async fn run(args: CloudArgs) -> Result<()> {
    match args.command {
        CloudCommand::Up {
            node,
            hostname,
            pool,
            paths,
        } => {
            let status = up(&paths.resolve(), &node, &hostname, pool.as_deref())?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays
            );
        }
        CloudCommand::Status { paths } => {
            let status = status(&paths.resolve())?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={} pending_keys={} public_routes={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays,
                status.pending_key_rotations,
                status.public_routes
            );
        }
        CloudCommand::Invite {
            node,
            hostname,
            public_key,
            ttl_seconds,
            paths,
        } => {
            let paths = paths.resolve();
            let ttl = Duration::from_secs(ttl_seconds);
            let token = match public_key {
                Some(public_key) => issue_join_token(&paths, &node, &hostname, &public_key, ttl)?,
                None => issue_open_join_token(&paths, &node, &hostname, ttl)?,
            };
            println!("{token}");
        }
        CloudCommand::Accept { token, paths } => {
            println!(
                "enrolled ipv4={}",
                accept_join_token(&paths.resolve(), &token)?
            );
        }
        CloudCommand::ServeEnrollment {
            listen,
            cert,
            paths,
        } => {
            let paths = paths.resolve();
            enroll_tls::serve(
                listen,
                &cert,
                &paths.tls_key,
                paths.state,
                paths.enroll_secret,
                paths.used_nonces,
            )
            .await?;
        }
        CloudCommand::Join {
            controller,
            server_name,
            ca,
            token,
            paths,
        } => {
            let paths = paths.resolve();
            let public_key = if paths.key.exists() {
                wireguard_public_key(&paths.key)?
            } else {
                rotate_wireguard_key(&paths.key)?
            };
            enroll_tls::join(
                controller,
                &server_name,
                &ca,
                &token,
                &public_key,
                &paths.state,
            )
            .await?;
            let joined = status(&paths)?;
            println!(
                "joined cloud={} ipv4={} ipv6={} public_key={}",
                joined.local_node, joined.ipv4, joined.ipv6, public_key
            );
        }
        CloudCommand::Expose {
            name,
            hostname,
            backend,
            share_required,
            strip_share_prefix,
            paths,
        } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            cfg.upsert_ingress(IngressRoute {
                name: name.clone(),
                hostname: hostname.clone(),
                backend,
                share_required,
                strip_share_prefix,
            })?;
            store.save(&cfg)?;
            println!(
                "exposed name={name} https://{hostname} -> {backend} share_required={share_required}"
            );
        }
        CloudCommand::Unexpose { name, paths } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            if !cfg.remove_ingress(&name) {
                anyhow::bail!("ingress route {name} not found");
            }
            store.save(&cfg)?;
            println!("removed {name}");
        }
        CloudCommand::Share {
            route,
            ttl_seconds,
            paths,
        } => {
            let paths = paths.resolve();
            let cfg = public_store(&paths).load()?;
            let ingress = cfg
                .ingress
                .iter()
                .find(|v| v.name == route)
                .ok_or_else(|| anyhow::anyhow!("ingress route {route} not found"))?;
            let token = issue_share_token(&paths, &route, Duration::from_secs(ttl_seconds))?;
            println!("https://{}/s/{}/", ingress.hostname, token);
        }
        CloudCommand::ServeIngress {
            listen,
            http3_listen,
            http01_listen,
            paths,
        } => {
            let paths = paths.resolve();
            if let Some(http) = http01_listen {
                let store = ChallengeStore::new(paths.challenge_dir.clone());
                tokio::spawn(async move {
                    let _ = serve_http01(http, store).await;
                });
            }
            let h3_public = paths.public_config.clone();
            let h3_share = paths.share_secret.clone();
            let h3_cert = paths.tls_cert.clone();
            let h3_key = paths.tls_key.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    serve_http3(http3_listen, h3_public, h3_share, h3_cert, h3_key).await
                {
                    eprintln!("fluxctl cloud serve-ingress: HTTP/3 stopped: {e}");
                }
            });
            serve_https(
                listen,
                paths.public_config,
                paths.share_secret,
                paths.tls_cert,
                paths.tls_key,
            )
            .await?;
        }
        CloudCommand::Acme {
            directory,
            email,
            domains,
            http_listen,
            paths,
        } => {
            let paths = paths.resolve();
            let store = ChallengeStore::new(paths.challenge_dir.clone());
            let http_store = store.clone();
            tokio::spawn(async move {
                let _ = serve_http01(http_listen, http_store).await;
            });
            let mut client = AcmeClient::connect(&directory, &paths.acme_account_key).await?;
            client
                .obtain_http01(&email, &domains, &store, &paths.tls_cert, &paths.tls_key)
                .await?;
            fluxvm_mesh::acme::mark_issued(&paths.acme_marker)?;
            let public = public_store(&paths);
            let mut cfg = public.load()?;
            cfg.acme = Some(AcmeConfig {
                directory_url: directory,
                email,
                domains,
                renew_before_days: 30,
                challenge: AcmeChallenge::Http01,
                dns01: None,
            });
            public.save(&cfg)?;
            println!(
                "certificate={} key={}",
                paths.tls_cert.display(),
                paths.tls_key.display()
            );
        }
        CloudCommand::AcmeDns01Cloudflare {
            directory,
            email,
            domains,
            zone_id,
            token_env,
            propagation_seconds,
            validation_resolver,
            require_dnssec_ad,
            paths,
        } => {
            let paths = paths.resolve();
            let dns = Dns01Config {
                provider: Dns01Provider::Cloudflare {
                    zone_id,
                    token_env,
                    api_base: "https://api.cloudflare.com/client/v4".into(),
                },
                propagation_seconds,
                validation_resolver,
                require_dnssec_ad,
            };
            let mut client = AcmeClient::connect(&directory, &paths.acme_account_key).await?;
            client
                .obtain_dns01(&email, &domains, &dns, &paths.tls_cert, &paths.tls_key)
                .await?;
            fluxvm_mesh::acme::mark_issued(&paths.acme_marker)?;
            let public = public_store(&paths);
            let mut cfg = public.load()?;
            cfg.acme = Some(AcmeConfig {
                directory_url: directory,
                email,
                domains,
                renew_before_days: 30,
                challenge: AcmeChallenge::Dns01,
                dns01: Some(dns),
            });
            public.save(&cfg)?;
            println!(
                "DNS-01 certificate={} key={}",
                paths.tls_cert.display(),
                paths.tls_key.display()
            );
        }
        CloudCommand::RelayConfigure {
            relay_addr,
            quic_relay_addr,
            transport,
            server_name,
            ca,
            site_id,
            secret,
            local_ingress,
            paths,
        } => {
            let transport = match transport.as_str() {
                "auto" => RelayTransport::Auto,
                "quic" => RelayTransport::Quic,
                "tls-tcp" => RelayTransport::TlsTcp,
                _ => unreachable!(),
            };
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            cfg.public_relay = Some(PublicRelayConfig {
                relay_addr,
                quic_relay_addr,
                transport,
                server_name,
                ca_file: ca,
                site_id,
                secret_file: secret,
                local_ingress,
            });
            store.save(&cfg)?;
            println!(
                "public relay configured transport={transport:?}; restart/start fluxmesh-edge to activate it"
            );
        }
        CloudCommand::RelayConnect { paths } => {
            let paths = paths.resolve();
            let cfg = public_store(&paths).load()?;
            let relay = cfg
                .public_relay
                .ok_or_else(|| anyhow::anyhow!("public relay is not configured"))?;
            run_public_relay(RelayClientConfig {
                relay_addr: relay.relay_addr,
                quic_relay_addr: relay.quic_relay_addr,
                transport: relay.transport,
                server_name: relay.server_name,
                ca_file: relay.ca_file,
                site_id: relay.site_id,
                secret_file: relay.secret_file,
                local_ingress: relay.local_ingress,
                heartbeat_file: fluxvm_mesh::mesh_dir().join("public-relay-heartbeat"),
            })
            .await?;
        }
        CloudCommand::Otlp {
            endpoint,
            service_name,
            paths,
        } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            cfg.otlp = Some(OtlpConfig {
                endpoint,
                service_name,
                headers: Default::default(),
            });
            store.save(&cfg)?;
            println!("OTLP edge tracing configured");
        }
        CloudCommand::Telemetry { telemetry } => {
            let path = telemetry.unwrap_or_else(default_telemetry_path);
            let v = TelemetryStore::new(path).load()?;
            print!("{}", render_prometheus(&v));
            let slo = evaluate_slo(&v, 120_000, 0.20);
            eprintln!(
                "slo={}{}",
                if slo.healthy { "PASS" } else { "FAIL" },
                if slo.reasons.is_empty() {
                    String::new()
                } else {
                    format!(" {}", slo.reasons.join("; "))
                }
            );
        }
        CloudCommand::DdnsCloudflare {
            hostname,
            zone_id,
            record_id_v4,
            record_id_v6,
            token_env,
            ttl,
            proxied,
            paths,
        } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            cfg.ddns = Some(DdnsConfig {
                hostname,
                provider: DdnsProvider::Cloudflare {
                    zone_id,
                    record_id_v4,
                    record_id_v6,
                    token_env,
                    api_base: "https://api.cloudflare.com/client/v4".into(),
                },
                ttl,
                proxied,
            });
            store.save(&cfg)?;
            println!("DDNS configured");
        }
        CloudCommand::DdnsWebhook {
            hostname,
            url,
            token_env,
            ttl,
            paths,
        } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            cfg.ddns = Some(DdnsConfig {
                hostname,
                provider: DdnsProvider::Webhook { url, token_env },
                ttl,
                proxied: false,
            });
            store.save(&cfg)?;
            println!("DDNS webhook configured");
        }
        CloudCommand::DdnsSync { ipv4, ipv6, paths } => {
            let paths = paths.resolve();
            let cfg = public_store(&paths).load()?;
            let ddns_cfg = cfg
                .ddns
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("DDNS is not configured"))?;
            let report = ddns::sync(ddns_cfg, ipv4, ipv6).await?;
            println!(
                "hostname={} ipv4={} ipv6={}",
                report.hostname, report.updated_v4, report.updated_v6
            );
        }
        CloudCommand::RouterMap {
            gateway,
            name,
            transport,
            internal_port,
            external_port,
            protocol,
            lifetime_seconds,
            paths,
        } => {
            let protocol = match protocol.as_str() {
                "auto" => RouterProtocol::Auto,
                "pcp" => RouterProtocol::Pcp,
                "nat-pmp" => RouterProtocol::NatPmp,
                "upnp" => RouterProtocol::Upnp,
                _ => anyhow::bail!("protocol must be auto|pcp|nat-pmp|upnp"),
            };
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            let router = cfg.router.get_or_insert(RouterConfig {
                gateway,
                protocol,
                mappings: Vec::new(),
            });
            router.gateway = gateway;
            router.protocol = protocol;
            let mapping = PortMapping {
                name: name.clone(),
                protocol: transport,
                internal_port,
                external_port,
                lifetime_seconds,
            };
            if let Some(existing) = router.mappings.iter_mut().find(|v| v.name == name) {
                *existing = mapping;
            } else {
                router.mappings.push(mapping);
            }
            store.save(&cfg)?;
            println!("router mapping {name} configured");
        }
        CloudCommand::RouterSync { paths } => {
            let paths = paths.resolve();
            let cfg = public_store(&paths).load()?;
            let router_cfg = cfg
                .router
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("router automation is not configured"))?;
            for mapping in router::reconcile(router_cfg).await? {
                println!(
                    "{} {:?} external_port={} lifetime={}",
                    mapping.name, mapping.protocol, mapping.external_port, mapping.lifetime_seconds
                );
            }
        }
        CloudCommand::WanAdd {
            name,
            interface,
            gateway,
            metric,
            preferred,
            probe_targets,
            paths,
        } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            let wan = WanConfig {
                name: name.clone(),
                interface,
                gateway,
                metric,
                preferred,
                probe_targets,
            };
            if let Some(existing) = cfg.wan.iter_mut().find(|v| v.name == name) {
                *existing = wan;
            } else {
                cfg.wan.push(wan);
            }
            store.save(&cfg)?;
            println!("WAN {name} configured");
        }
        CloudCommand::WanRemove { name, paths } => {
            let paths = paths.resolve();
            let store = public_store(&paths);
            let mut cfg = store.load()?;
            let before = cfg.wan.len();
            cfg.wan.retain(|v| v.name != name);
            if before == cfg.wan.len() {
                anyhow::bail!("WAN {name} not found");
            }
            store.save(&cfg)?;
            println!("WAN {name} removed");
        }
        CloudCommand::Diagnose { ingress, paths } => {
            let paths = paths.resolve();
            let report = diagnose::run(&DiagnoseConfig {
                state: paths.state,
                public_config: paths.public_config,
                wireguard_key: paths.key,
                cert: paths.tls_cert,
                cert_key: paths.tls_key,
                share_secret: paths.share_secret,
                ingress,
            });
            for item in &report.items {
                let level = match item.severity {
                    Severity::Pass => "PASS",
                    Severity::Warn => "WARN",
                    Severity::Fail => "FAIL",
                };
                println!("{level:<4} {:<20} {}", item.name, item.detail);
            }
            if report.failed() {
                anyhow::bail!("cloud diagnostics found hard failures");
            }
        }
    }
    Ok(())
}
