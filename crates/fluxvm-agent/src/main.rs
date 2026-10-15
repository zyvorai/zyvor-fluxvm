// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

#[derive(Parser)]
#[command(
    name = "fluxvm-agent",
    about = "Distributed node-agent + fleet registry for multi-host Zyvor FluxVM"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// A clap subcommand enum: one variant per subcommand, built once at start-up.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Command {
    /// Run the central fleet registry + create/list/delete proxy.
    Central {
        #[arg(long, env = "LISTEN", default_value = "0.0.0.0:7799")]
        listen: String,
        /// Directory for `fleet-nodes.json` persistence.
        #[arg(
            long,
            env = "FLUXVM_AGENT_STATE_DIR",
            default_value = "/var/lib/fluxvm-agent"
        )]
        state_dir: PathBuf,
        /// Operator bearer token for `/fleet/*`. Omit it and central makes one on first start, keeps it in
        /// `<state-dir>/operator-token` (mode 0600) and prints it once; `--token` always wins and changes it.
        #[arg(long, env = "FLUXVM_AGENT_TOKEN")]
        token: Option<String>,
        /// Leave `/fleet/*` open with no token at all (lab only; the old behaviour of omitting `--token`).
        #[arg(long, env = "FLUXVM_AGENT_NO_AUTH", conflicts_with = "token")]
        no_auth: bool,
        /// Optional TLS certificate (PEM). Requires `--tls-key`.
        #[arg(long, env = "FLUXVM_AGENT_TLS_CERT")]
        tls_cert: Option<PathBuf>,
        /// Optional TLS private key (PEM).
        #[arg(long, env = "FLUXVM_AGENT_TLS_KEY")]
        tls_key: Option<PathBuf>,
        /// Seconds between ledger reconciles and lease-expiry checks against every node (0 turns the background
        /// pass off; `POST /fleet/reconcile` still runs one).
        #[arg(long, env = "FLUXVM_AGENT_RECONCILE_SECS", default_value = "60")]
        reconcile_secs: u64,
        /// Run highly available: several centrals share this Redis, one leads (docs/fleet-ha.md).
        #[arg(long, env = "FLUXVM_AGENT_HA_REDIS")]
        ha_redis: Option<String>,
        /// Name of the HA group (several fleets can share one Redis).
        #[arg(long, env = "FLUXVM_AGENT_HA_CLUSTER", default_value = "default")]
        ha_cluster: String,
        /// This central's URL as clients reach it; its identity in the leader election. Required with --ha-redis.
        #[arg(long, env = "FLUXVM_AGENT_ADVERTISE_URL")]
        advertise_url: Option<String>,
        /// Leader lock TTL: how long a dead leader blocks a takeover.
        #[arg(long, env = "FLUXVM_AGENT_HA_TTL_SECS", default_value = "10")]
        ha_ttl_secs: u64,
        /// A token that may only scrape `GET /metrics` (Prometheus), so the scraper does not need the fleet token.
        #[arg(long, env = "FLUXVM_AGENT_METRICS_TOKEN", hide_env_values = true)]
        metrics_token: Option<String>,
        /// Seconds between the metrics collector's samples of every node (graphs and `GET /metrics`).
        #[arg(long, env = "FLUXVM_AGENT_METRICS_INTERVAL_SECS", default_value = "15")]
        metrics_interval_secs: u64,
        /// Console single sign-on (OpenID Connect). See docs/mac-cloud-tenancy.md.
        #[command(flatten)]
        sso: fluxvm_agent::central::SsoArgs,
        // ---- signup ----
        /// File with the Stripe secret key (`sk_live_…`, or a restricted `rk_…` key); turns on payments
        /// (docs/mac-cloud-billing.md). The `STRIPE_SECRET_KEY` environment variable can hold the key instead.
        #[arg(long, env = "STRIPE_SECRET_KEY_FILE")]
        stripe_secret_key_file: Option<PathBuf>,
        /// File with the signing secret (`whsec_…`) of the Stripe webhook endpoint `POST /stripe/webhook`. The
        /// `STRIPE_WEBHOOK_SECRET` environment variable can hold it instead.
        #[arg(long, env = "STRIPE_WEBHOOK_SECRET_FILE")]
        stripe_webhook_secret_file: Option<PathBuf>,
        /// Stripe API base URL (a local mock for testing). Stripe Checkout sends people back to `--public-url`
        /// (shared with single sign-on).
        #[arg(long, env = "STRIPE_API_BASE", default_value = fluxvm_agent::stripe::DEFAULT_API_BASE)]
        stripe_api_base: String,
        /// Where self-service sign-up confirmation mail goes: an `https://` URL that central POSTs JSON to
        /// (`{type, to, subject, text, link}`) and that sends the mail, or `log` to write the mail to this log (labs).
        /// Needs `--public-url` (the link's base). Without it sign-up cannot be opened while email confirmation is
        /// required (the policy default). See docs/mac-cloud-billing.md.
        #[arg(long, env = "FLUXVM_SIGNUP_MAIL_WEBHOOK")]
        signup_mail_webhook: Option<String>,
        /// File with a secret that signs each mail webhook POST (`X-FluxVM-Signature: sha256=…`, HMAC-SHA256 of the body).
        #[arg(long, env = "FLUXVM_SIGNUP_MAIL_SECRET_FILE")]
        signup_mail_secret_file: Option<PathBuf>,
    },
    /// Run ephemeral GitHub Actions runners on fleet VMs: one VM per queued job, deleted when the job is done.
    /// See docs/github-actions-runners.md.
    GhaRunners {
        /// JSON config (GitHub owner/repos, fleet URL, runner pools).
        #[arg(long, env = "FLUXVM_GHA_CONFIG")]
        config: PathBuf,
    },
    /// Run a simulated Mac: an in-memory node API plus a heartbeat reporting Virtualization.framework capabilities.
    /// For trying the fleet (leases, tenants, overlays, the console) on any machine. Nothing boots.
    /// See docs/fleet-quickstart.md.
    SimNode {
        #[arg(long)]
        name: String,
        /// Central URL; with HA, a comma-separated list.
        #[arg(long, env = "CENTRAL_URL", default_value = "http://127.0.0.1:7799")]
        central: String,
        /// Where the simulated node API listens; port 0 picks a free one.
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: std::net::SocketAddr,
        #[arg(long, env = "FLUXVM_AGENT_TOKEN")]
        token: Option<String>,
        #[arg(long, default_value = "12")]
        vcpus: u32,
        #[arg(long, default_value = "32")]
        memory_gib: u64,
        /// Report nested virtualization (an M3 or later Mac).
        #[arg(long)]
        nested: bool,
        #[arg(long = "label", value_parser = parse_label)]
        labels: Vec<(String, String)>,
        #[arg(long, default_value = "10")]
        interval_secs: u64,
        /// Write the overlay.json this Mac would get here (default: none, no overlay sync).
        #[arg(long)]
        overlay_config: Option<PathBuf>,
    },
    /// Run this node's heartbeat client, reporting to a central registry.
    Node {
        /// This node's name, as it'll appear in `POST /fleet/vms {"node": "..."}`.
        #[arg(long, env = "NODE_NAME")]
        name: String,
        /// Base URL of the `fluxvm-agent central` instance; with HA, a comma-separated list of every central (the
        /// heartbeat goes to whichever one leads).
        #[arg(long, env = "CENTRAL_URL")]
        central: String,
        /// Base URL THIS agent itself uses to reach its local `fluxvm
        /// serve` — almost always a loopback address.
        #[arg(long, env = "FLUXVM_URL", default_value = "http://127.0.0.1:7788")]
        fluxvm_url: String,
        /// Base URL the CENTRAL registry should use to reach this same
        /// `fluxctl serve` — must be this host's real, externally
        /// routable address when central runs elsewhere.
        #[arg(long, env = "ADVERTISE_URL")]
        advertise_url: Option<String>,
        #[arg(long, default_value = "10")]
        interval_secs: u64,
        /// Bearer token matching `fluxvm-agent central --token`.
        #[arg(long, env = "FLUXVM_AGENT_TOKEN")]
        token: Option<String>,
        /// Label this node reports on every heartbeat, as `key=value`.
        /// Repeatable. Lets `fluxctl fleet create --node-selector
        /// key=value` (or a plain `POST /fleet/vms` `"nodeSelector"`
        /// object) constrain automatic placement to nodes carrying it —
        /// e.g. `--label zone=us-east --label gpu=true`.
        #[arg(long = "label", value_parser = parse_label)]
        labels: Vec<(String, String)>,
        /// Do not write this Mac's overlay.json (private networks spanning Macs) from the fleet's allocation.
        #[arg(long)]
        no_overlay_sync: bool,
    },
}

/// Resolves on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = ctrl_c.await;
            return;
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}

/// Parses one `--label key=value` argument. Splits on the FIRST `=` only,
/// so a value containing `=` (unusual, but not invalid) still round-trips.
fn parse_label(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("expected key=value, got '{s}'")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Command::Central {
            listen,
            state_dir,
            token,
            no_auth,
            tls_cert,
            tls_key,
            reconcile_secs,
            ha_redis,
            ha_cluster,
            advertise_url,
            ha_ttl_secs,
            metrics_token,
            metrics_interval_secs,
            sso,
            stripe_secret_key_file,
            stripe_webhook_secret_file,
            stripe_api_base,
            signup_mail_webhook,
            signup_mail_secret_file,
        } => {
            // `--public-url` is declared once (in the SSO flags) and used by Stripe Checkout too.
            let public_url = sso
                .public_url
                .as_deref()
                .map(|u| u.trim().trim_end_matches('/').to_owned())
                .filter(|u| !u.is_empty());
            fluxvm_agent::central::configure_metrics(fluxvm_agent::central::MetricsSettings {
                interval: Some(Duration::from_secs(metrics_interval_secs.max(1))),
                token: metrics_token,
            });
            let sso = sso.into_config()?;
            if sso.is_some() && no_auth {
                bail!(
                    "--oidc-issuer cannot be combined with --no-auth: every caller would be the operator"
                );
            }
            // The token was given on the command line (or the environment), as opposed to made here: standbys in an
            // HA group must be started with the same explicit one.
            let token_given = token.is_some();
            let token = match token {
                Some(t) => Some(t),
                None if no_auth => {
                    tracing::warn!("--no-auth: /fleet/* is open to anyone who can reach this port");
                    None
                }
                None => {
                    let (t, created) = fluxvm_agent::tenancy::default_operator_token(&state_dir)
                        .with_context(|| {
                            format!("creating the operator token in {}", state_dir.display())
                        })?;
                    let file = state_dir.join(fluxvm_agent::tenancy::OPERATOR_TOKEN_FILE);
                    if created {
                        // Printed with println! (not tracing): central's default log filter shows errors only.
                        println!("No --token given: generated the operator token (shown once).");
                        println!("  token: {t}");
                        println!("  stored in {} (mode 0600)", file.display());
                        println!("  change it with --token, or delete that file to get a new one");
                    } else {
                        println!("Using the operator token stored in {}", file.display());
                    }
                    Some(t)
                }
            };
            // ---- signup ----
            let secret = |file: Option<PathBuf>, var: &str| -> Result<Option<String>> {
                match file {
                    Some(f) => fluxvm_agent::stripe::read_secret_file(&f)
                        .map(Some)
                        .map_err(anyhow::Error::msg),
                    None => Ok(std::env::var(var).ok().filter(|v| !v.trim().is_empty())),
                }
            };
            if let Some(secret_key) = secret(stripe_secret_key_file, "STRIPE_SECRET_KEY")? {
                let webhook_secret = secret(stripe_webhook_secret_file, "STRIPE_WEBHOOK_SECRET")?;
                if webhook_secret.is_none() {
                    tracing::warn!(
                        "Stripe is on but no webhook secret is set: invoice status and saved payment methods are not updated"
                    );
                }
                if let Some(u) = &public_url
                    && !(u.starts_with("https://") || u.starts_with("http://"))
                {
                    bail!("--public-url must be an http(s) URL");
                }
                let cfg = fluxvm_agent::stripe::StripeConfig {
                    secret_key,
                    webhook_secret,
                    api_base: stripe_api_base,
                    public_url: public_url.clone(),
                };
                tracing::info!(mode = cfg.mode(), "Stripe payments on");
                fluxvm_agent::central::configure_stripe(cfg);
            }
            if let Some(url) = signup_mail_webhook
                .map(|u| u.trim().to_owned())
                .filter(|u| !u.is_empty())
            {
                if url != "log" && !(url.starts_with("https://") || url.starts_with("http://")) {
                    bail!("--signup-mail-webhook must be an http(s) URL, or `log`");
                }
                if url.starts_with("http://")
                    && !(url.starts_with("http://127.") || url.starts_with("http://localhost"))
                {
                    tracing::warn!(
                        "the sign-up mail webhook is plain http: confirmation links cross the network unencrypted"
                    );
                }
                let Some(base) = public_url.clone() else {
                    bail!(
                        "--signup-mail-webhook needs --public-url: it is the base of the confirmation link"
                    );
                };
                let secret = match signup_mail_secret_file {
                    Some(f) => Some(
                        fluxvm_agent::stripe::read_secret_file(&f).map_err(anyhow::Error::msg)?,
                    ),
                    None => None,
                };
                tracing::info!("sign-up confirmation mail on");
                fluxvm_agent::central::configure_mail(fluxvm_agent::central::MailConfig {
                    url,
                    secret,
                    public_url: base,
                });
            }
            let ha = match ha_redis {
                Some(url) => {
                    let Some(advertise_url) = advertise_url else {
                        bail!("--ha-redis needs --advertise-url (this central's own URL)");
                    };
                    if !token_given {
                        bail!(
                            "--ha-redis needs --token: standbys and the leader must share one fleet token"
                        );
                    }
                    Some(fluxvm_agent::ha::HaConfig {
                        store: fluxvm_agent::ha::HaStore::redis(&url, &ha_cluster).await?,
                        advertise_url,
                        ttl: Duration::from_secs(ha_ttl_secs.max(2)),
                    })
                }
                None => None,
            };
            let (app, ha_handle) = fluxvm_agent::central::router_with_sso(
                fluxvm_agent::central::CentralConfig {
                    state_dir,
                    token,
                    reconcile_interval: (reconcile_secs > 0)
                        .then(|| Duration::from_secs(reconcile_secs)),
                    ha,
                },
                sso,
            );
            // On Ctrl-C or SIGTERM (launchd, systemd), hand the leader lock over before exiting.
            if let Some(h) = ha_handle {
                tokio::spawn(async move {
                    shutdown_signal().await;
                    match h.release().await {
                        Ok(()) => tracing::info!("released the fleet leader lock"),
                        Err(e) => {
                            tracing::warn!(error = %e, "could not release the fleet leader lock")
                        }
                    }
                    std::process::exit(0);
                });
            }
            let addr: SocketAddr = listen
                .parse()
                .with_context(|| format!("parsing listen address {listen}"))?;
            match (tls_cert, tls_key) {
                (Some(cert), Some(key)) => {
                    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
                        .await
                        .context("loading TLS cert/key")?;
                    tracing::info!(listen = %listen, tls = true, "fleet registry listening");
                    axum_server::bind_rustls(addr, config)
                        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                        .context("serving TLS")?;
                }
                (None, None) => {
                    let listener = tokio::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("binding {listen}"))?;
                    tracing::info!(listen = %listen, tls = false, "fleet registry listening");
                    axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .await
                    .context("serving")?;
                }
                _ => bail!("both --tls-cert and --tls-key are required together"),
            }
        }
        Command::GhaRunners { config } => {
            let raw =
                std::fs::read(&config).with_context(|| format!("reading {}", config.display()))?;
            let cfg: fluxvm_agent::gha::GhaConfig = serde_json::from_slice(&raw)
                .with_context(|| format!("parsing {}", config.display()))?;
            fluxvm_agent::gha::run(cfg).await?;
        }
        Command::SimNode {
            name,
            central,
            listen,
            token,
            vcpus,
            memory_gib,
            nested,
            labels,
            interval_secs,
            overlay_config,
        } => {
            fluxvm_agent::sim::run(fluxvm_agent::sim::SimConfig {
                name,
                central_url: central,
                token,
                listen,
                host: fluxvm_agent::node::SimHost {
                    vcpus,
                    memory_mib: memory_gib.saturating_mul(1024),
                    nested_virtualization: nested,
                    vmnet: true,
                },
                labels: labels.into_iter().collect(),
                interval: Duration::from_secs(interval_secs.max(1)),
                overlay_config,
            })
            .await?;
        }
        Command::Node {
            name,
            central,
            fluxvm_url,
            advertise_url,
            interval_secs,
            token,
            labels,
            no_overlay_sync,
        } => {
            let advertise_url = advertise_url.unwrap_or_else(|| fluxvm_url.clone());
            fluxvm_agent::node::run(fluxvm_agent::node::NodeConfig {
                name,
                central_url: central,
                fluxvm_url,
                advertise_url,
                interval: Duration::from_secs(interval_secs.max(1)),
                token,
                labels: labels.into_iter().collect(),
                overlay_config: (!no_overlay_sync).then(fluxvm_apple::vznet::overlay_config_path),
                sim: None,
            })
            .await;
        }
    }
    Ok(())
}
