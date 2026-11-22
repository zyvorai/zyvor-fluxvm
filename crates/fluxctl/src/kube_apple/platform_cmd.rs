// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl kube apple platform ...` and the small hooks the other `kube apple` commands call: the command line for the
//! platform layer (apply, gitops, metallb, cert-manager, metrics-server, netpol, pins), `upgrade --channel`,
//! `snapshot restore --rejoin-servers` and `doctor --cis`. The work itself is in [`super::platform`], [`super::suc`],
//! [`super::gitops`], [`super::recipes`], [`super::netpol`] and [`super::restore_all`].

use super::doctor::{Finding, Level};
use super::gitops::{self, GitopsReq, Tool};
use super::host::Host;
use super::model::*;
use super::netpol::{self, NetpolOpts};
use super::pins::{self, Pin};
use super::platform::{self, HostRemote, PlatformArgs, Remote};
use super::recipes::{self, PinSrc};
use super::restore_all::{self, RestoreAllReq};
use super::suc::{self, ChannelReq};
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct ManifestArgs {
    /// Install this local file instead of downloading the pinned manifest (air-gapped use, or another version). It must match
    /// the pinned SHA-256 or the one given with --sha256; nothing is sent otherwise.
    #[arg(long, value_name = "FILE")]
    manifest_file: Option<PathBuf>,
    /// The SHA-256 the --manifest-file must have, when it is not the pinned version.
    #[arg(long, value_name = "HEX", requires = "manifest_file")]
    sha256: Option<String>,
}

impl ManifestArgs {
    fn src(&self, pin: &Pin) -> Result<PinSrc> {
        match &self.manifest_file {
            Some(p) => recipes::manifest_file(p, self.sha256.as_deref(), pin),
            None => Ok(PinSrc::Download),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum PlatformCommand {
    /// Write registries, hardening, embedded-registry and Cilium settings to the nodes of an existing cluster (the same flags
    /// `create` takes), then restart them one at a time with --restart. Bundled add-ons (--addons) are chosen at `create`.
    Apply {
        name: String,
        #[command(flatten)]
        platform: PlatformArgs,
        /// Restart k3s on each node afterwards (servers first, one at a time). Without it nothing is restarted.
        #[arg(long)]
        restart: bool,
        /// Only print what would be written.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install Flux or Argo CD from a pinned manifest and point it at a Git repository (deploy key from a file, never an argument).
    Gitops {
        name: String,
        #[arg(value_parser = ["flux", "argo"])]
        tool: String,
        /// git@host:org/repo.git, ssh://git@host/org/repo.git, or https://host/org/repo.git
        #[arg(long)]
        repo: String,
        #[arg(long, default_value = "main")]
        branch: String,
        /// The directory of the repository to apply.
        #[arg(long, default_value = ".")]
        path: String,
        /// Name of the Flux GitRepository/Kustomization or the Argo CD Application.
        #[arg(long, default_value = "fleet")]
        app_name: String,
        /// A private key file (mode 600, no passphrase) for an ssh repository.
        #[arg(long, value_name = "FILE")]
        deploy_key_file: Option<PathBuf>,
        /// The Git server's host keys (`ssh-keyscan` lines you have verified). Required by Flux for ssh repositories.
        #[arg(long, value_name = "FILE")]
        known_hosts_file: Option<PathBuf>,
        /// Argo CD: sync automatically with prune and self-heal.
        #[arg(long)]
        auto_sync: bool,
        /// Argo CD: the namespace the Application deploys into.
        #[arg(long, default_value = "default")]
        dest_namespace: String,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
    /// MetalLB (pinned) with one L2 address pool. Create the cluster with `--addons metallb` so ServiceLB is off.
    Metallb {
        name: String,
        /// A CIDR (10.89.0.192/28) or a range (10.89.0.200-10.89.0.220). Repeatable.
        #[arg(long = "address", value_name = "ADDR")]
        addresses: Vec<String>,
        #[arg(long, default_value = "default")]
        pool: String,
        /// Only hand out addresses to Services that ask for this pool by name.
        #[arg(long)]
        no_auto_assign: bool,
        /// Print the pool manifest and exit.
        #[arg(long)]
        print_pool: bool,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
    /// cert-manager (pinned) and wait for its webhook.
    CertManager {
        name: String,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
    /// Envoy Gateway (pinned): the Gateway API CRDs and the controller. Then use `fluxctl gateway ...` from this Mac.
    EnvoyGateway {
        name: String,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
    /// A stock metrics-server (pinned), for a cluster created with metrics-server disabled (`--addons minimal`).
    MetricsServer {
        name: String,
        /// Skip verification of the kubelets' certificates, which k3s signs with its own CA. A security trade-off.
        #[arg(long)]
        kubelet_insecure_tls: bool,
        #[command(flatten)]
        manifest: ManifestArgs,
    },
    /// A baseline NetworkPolicy set for a namespace: deny all, allow DNS and same-namespace traffic, plus what you name. Prints
    /// the YAML; --apply sends it to the cluster.
    Netpol {
        name: String,
        #[arg(long, short = 'n')]
        namespace: String,
        #[arg(long)]
        no_dns: bool,
        #[arg(long)]
        no_same_namespace: bool,
        /// A namespace that may send traffic in. Repeatable.
        #[arg(long = "allow-from", value_name = "NAMESPACE")]
        allow_from: Vec<String>,
        /// A CIDR the pods may send traffic to. Repeatable.
        #[arg(long = "allow-egress-cidr", value_name = "CIDR")]
        allow_egress_cidr: Vec<String>,
        #[arg(long)]
        apply: bool,
    },
    /// The manifests this tool installs: version, URL and the SHA-256 it insists on.
    Pins,
}

impl PlatformCommand {
    /// Commands that only print and need neither a cluster nor ssh keys.
    pub fn is_offline(&self) -> bool {
        matches!(self, PlatformCommand::Pins | PlatformCommand::Netpol { apply: false, .. } | PlatformCommand::Metallb { print_pool: true, .. })
            || matches!(self, PlatformCommand::Apply { dry_run: true, platform, .. } if platform.addons.is_none())
    }
}

/// [`PlatformCommand::is_offline`] commands, without a host.
pub fn run_offline(cmd: PlatformCommand) -> Result<()> {
    match cmd {
        PlatformCommand::Pins => json(
            &pins::ALL
                .iter()
                .map(|p| serde_json::json!({"name": p.key, "version": p.version, "url": p.url, "sha256": p.sha256}))
                .collect::<Vec<_>>(),
        ),
        PlatformCommand::Netpol { namespace, no_dns, no_same_namespace, allow_from, allow_egress_cidr, .. } => {
            print!("{}", netpol::baseline(&NetpolOpts { namespace, no_dns, no_same_namespace, allow_from, allow_egress_cidr })?);
            Ok(())
        }
        PlatformCommand::Metallb { addresses, pool, no_auto_assign, .. } => {
            print!("{}", recipes::metallb_pool_yaml(&pool, &addresses, !no_auto_assign)?);
            Ok(())
        }
        PlatformCommand::Apply { platform, .. } => {
            let plan = plan_from_args(&platform)?;
            json(&dry_run_json(&plan))
        }
        _ => bail!("this command needs a cluster"),
    }
}

fn dry_run_json(plan: &platform::PlatformPlan) -> serde_json::Value {
    serde_json::json!({
        "dropin": plan.dropin, "all_nodes": plan.all_files.iter().map(|f| &f.path).collect::<Vec<_>>(),
        "servers": plan.server_files.iter().map(|f| &f.path).collect::<Vec<_>>(), "install": plan.install, "notes": plan.notes,
    })
}

fn json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn local(st: &State) -> Result<()> {
    if st.external_url.is_some() {
        bail!("this cluster is owned by another Mac; run platform commands there");
    }
    Ok(())
}

/// Builds the plan for `create`/`apply` from the flags, reading `--registries-file`.
pub fn plan_from_args(a: &PlatformArgs) -> Result<platform::PlatformPlan> {
    let reg = a.registries_file.as_deref().map(platform::read_registries).transpose()?;
    platform::plan(a, reg.as_deref())
}

pub async fn run<H: Host>(h: &H, store: &Store, cmd: PlatformCommand) -> Result<()> {
    match cmd {
        PlatformCommand::Pins => run_offline(PlatformCommand::Pins),
        PlatformCommand::Netpol { name, namespace, no_dns, no_same_namespace, allow_from, allow_egress_cidr, apply } => {
            let o = NetpolOpts { namespace, no_dns, no_same_namespace, allow_from, allow_egress_cidr };
            if !apply {
                return run_offline(PlatformCommand::Netpol { name, namespace: o.namespace, no_dns: o.no_dns, no_same_namespace: o.no_same_namespace, allow_from: o.allow_from, allow_egress_cidr: o.allow_egress_cidr, apply });
            }
            let st = store.require(&name)?;
            local(&st)?;
            let r = HostRemote { h, st: &st };
            println!("{}", netpol::apply(&r, &o).await?);
            Ok(())
        }
        PlatformCommand::Metallb { name, addresses, pool, no_auto_assign, print_pool, manifest } => {
            if print_pool {
                return run_offline(PlatformCommand::Metallb { name, addresses, pool, no_auto_assign, print_pool, manifest });
            }
            let st = store.require(&name)?;
            local(&st)?;
            let src = manifest.src(&pins::METALLB)?;
            let _lock = store.lock(&name)?;
            let r = HostRemote { h, st: &st };
            json(&recipes::metallb(&r, recipes::MetalLbReq { name: pool, addresses, auto_assign: !no_auto_assign, src, disabled: st.flags.disable.clone() }).await?)
        }
        PlatformCommand::CertManager { name, manifest } => {
            let st = store.require(&name)?;
            local(&st)?;
            let src = manifest.src(&pins::CERT_MANAGER)?;
            let _lock = store.lock(&name)?;
            json(&recipes::cert_manager(&HostRemote { h, st: &st }, src).await?)
        }
        PlatformCommand::EnvoyGateway { name, manifest } => {
            let st = store.require(&name)?;
            local(&st)?;
            let src = manifest.src(&pins::ENVOY_GATEWAY)?;
            let _lock = store.lock(&name)?;
            json(&recipes::envoy_gateway(&HostRemote { h, st: &st }, src).await?)
        }
        PlatformCommand::MetricsServer { name, kubelet_insecure_tls, manifest } => {
            let st = store.require(&name)?;
            local(&st)?;
            let src = manifest.src(&pins::METRICS_SERVER)?;
            let _lock = store.lock(&name)?;
            json(&recipes::metrics_server(&HostRemote { h, st: &st }, src, kubelet_insecure_tls, &st.flags.disable).await?)
        }
        PlatformCommand::Gitops { name, tool, repo, branch, path, app_name, deploy_key_file, known_hosts_file, auto_sync, dest_namespace, manifest } => {
            let st = store.require(&name)?;
            local(&st)?;
            let tool = if tool == "flux" { Tool::Flux } else { Tool::Argo };
            let pin = if tool == Tool::Flux { &pins::FLUX } else { &pins::ARGOCD };
            let req = GitopsReq {
                tool,
                repo,
                branch,
                path,
                name: app_name,
                key: deploy_key_file.as_deref().map(gitops::read_key_file).transpose()?,
                known_hosts: known_hosts_file.as_deref().map(gitops::read_known_hosts).transpose()?,
                auto_sync,
                dest_namespace,
                src: manifest.src(pin)?,
            };
            let _lock = store.lock(&name)?;
            json(&gitops::bootstrap(&HostRemote { h, st: &st }, req).await?)
        }
        PlatformCommand::Apply { name, platform: args, restart, dry_run } => {
            if args.addons.is_some() {
                bail!("--addons is chosen when the cluster is created (every server's unit carries it); use `create --addons`");
            }
            if args.is_empty() {
                bail!("nothing to apply: give --registries-file, --embedded-registry, --harden cis or --cilium");
            }
            let st = store.require(&name)?;
            local(&st)?;
            let plan = plan_from_args(&args)?;
            if dry_run {
                return json(&dry_run_json(&plan));
            }
            let _lock = store.lock(&name)?;
            json(&platform::apply(&HostRemote { h, st: &st }, &plan, restart).await?)
        }
    }
}

// ---------------------------------------------------------------- hooks for the other commands

/// `create` with platform flags: after the cluster is up, applies the plan to every node and restarts them, then installs
/// the pinned add-ons the plan names (Envoy Gateway).
pub async fn after_create<H: Host>(h: &H, store: &Store, name: &str, plan: &platform::PlatformPlan) -> Result<platform::ApplyReport> {
    let st = store.require(name)?;
    let r = HostRemote { h, st: &st };
    let retry = |e: anyhow::Error| {
        anyhow::anyhow!("{name} was created, but the platform settings were not all applied: {e:#}; re-run `fluxctl kube apple platform apply {name} --restart` with the same flags")
    };
    let mut rep = if plan.dropin.is_some() || !plan.all_files.is_empty() || !plan.server_files.is_empty() {
        platform::apply(&r, plan, true).await.map_err(retry)?
    } else {
        platform::ApplyReport { notes: plan.notes.clone(), ..Default::default() }
    };
    for add in &plan.install {
        // Only "envoy-gateway" exists; the plan builder never produces another name.
        let done = recipes::envoy_gateway(&r, PinSrc::Download).await.map_err(|e| {
            anyhow::anyhow!("{name} was created, but Envoy Gateway was not installed: {e:#}; run `fluxctl kube apple platform envoy-gateway {name}`")
        })?;
        rep.installed.push(format!("{add} {}", done.version));
        rep.notes.extend(done.notes);
    }
    Ok(rep)
}

pub async fn upgrade_channel<H: Host>(h: &H, store: &Store, name: &str, channel: String, allow_downgrade: bool, no_backup: bool, drain: bool) -> Result<()> {
    let st = store.require(name)?;
    let _lock = store.lock(name)?;
    json(&suc::upgrade_channel(&HostRemote { h, st: &st }, &st, &ChannelReq { channel, allow_downgrade, no_backup, drain }).await?)
}

pub async fn restore_rejoin<H: Host>(h: &H, store: &Store, name: &str, snapshot: String, from: Option<String>, yes: bool) -> Result<()> {
    let st = store.require(name)?;
    local(&st)?;
    let _lock = store.lock(name)?;
    json(&restore_all::restore_all(&HostRemote { h, st: &st }, &st, &RestoreAllReq { snapshot, from, yes }).await?)
}

/// The CIS audit of every server, as doctor findings (`doctor --cis`). Only servers whose VM answers are asked.
pub async fn cis_findings<R: Remote>(r: &R, cluster: &str) -> Vec<Finding> {
    let mut out = vec![];
    let fix = format!("fluxctl kube apple platform apply {cluster} --harden cis --restart");
    for node in r.servers() {
        match r.run(&node, &crate::kube_cis::gather_script(), None).await {
            Ok(text) => {
                for c in crate::kube_cis::to_checks(&crate::kube_cis::parse_facts(&text), &fix) {
                    out.push(Finding {
                        name: c.name,
                        level: match c.level {
                            crate::kube_doctor::Level::Ok => Level::Ok,
                            crate::kube_doctor::Level::Warn => Level::Warn,
                            crate::kube_doctor::Level::Fail => Level::Fail,
                        },
                        detail: format!("{node}: {}", c.detail),
                        fix: c.fix,
                    });
                }
            }
            Err(e) => out.push(Finding {
                name: "CIS config",
                level: Level::Warn,
                detail: format!("{node}: could not read its settings: {e:#}"),
                fix: format!("fluxctl kube apple start {cluster}"),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    #[tokio::test]
    async fn cis_findings_cover_each_server_and_name_the_node() {
        let r = FakeRemote::cluster(2, 1);
        r.answer("@@config", "@@sysctl\n@@files\n");
        let f = cis_findings(&r, "dev").await;
        assert!(!f.is_empty());
        assert!(f.iter().any(|x| x.detail.starts_with("c-s1: ")) && f.iter().any(|x| x.detail.starts_with("c-s2: ")));
        assert!(!f.iter().any(|x| x.detail.starts_with("c-a1")));
        assert!(f.iter().filter(|x| x.level == Level::Fail).all(|x| x.fix == "fluxctl kube apple platform apply dev --harden cis --restart"));
    }

    #[tokio::test]
    async fn cis_findings_warn_when_a_server_cannot_be_asked() {
        let r = FakeRemote::cluster(1, 0);
        r.fail_on("@@config", "connection refused");
        let f = cis_findings(&r, "dev").await;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].level, Level::Warn);
    }
}
