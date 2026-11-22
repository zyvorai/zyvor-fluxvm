// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl kube kairon`: put Kairon (Kubernetes declares, Kairon orchestrates, FluxVM executes) on the cluster FluxVM made, and
//! check that it works. Kubernetes (the k3s VM) is the control plane; VMs run on FluxVM hosts that are Kairon nodes.
//!
//! Everything it installs is pinned and verified: the Helm chart by its registry digest (`helm pull` must report the pinned
//! manifest digest) and `kaironctl` by the release's `SHA256SUMS`. All outside calls go through [`exec::Runner`].

mod certs;
mod exec;
mod health;
mod kaironctl;
mod model;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use clap::{Args, Subcommand};
use exec::{Cmd, Out, Runner};
use health::{Check, Level};
use model::{InstallOpts, NAMESPACE, NodeLabels, Pin, RELEASE, Target};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct KaironArgs {
    #[command(subcommand)]
    command: KaironCommand,
}

#[derive(Args, Clone)]
struct Common {
    /// Name of the FluxVM-created cluster; its kubeconfig is `~/.kube/fluxvm-NAME.yaml`.
    #[arg(long, default_value = crate::kube_host::DEFAULT_NAME)]
    name: String,
    /// Use this kubeconfig instead. The install goes into whatever cluster it points at: use only a cluster FluxVM made for this.
    #[arg(long)]
    kubeconfig: Option<PathBuf>,
    /// kubeconfig context to use.
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args, Clone)]
struct InstallArgs {
    #[command(flatten)]
    common: Common,
    /// The production profile: admission webhook, mTLS live migration, network default-deny. Makes the webhook and migration
    /// certificates itself and stores them as Secrets (never printed).
    #[arg(long)]
    production: bool,
    /// Label this node as a Kairon node (kairon.zyvor.dev/capable=true): a machine that runs the FluxVM daemon. Repeatable.
    /// A Mac that runs `kairon-node` registers and labels itself.
    #[arg(long, value_name = "NODE")]
    node: Vec<String>,
    /// Also label the node kairon.zyvor.dev/backend.BACKEND=true (for example `vz`).
    #[arg(long, requires = "node")]
    backend: Option<String>,
    /// Also label the node kairon.zyvor.dev/chip=CHIP.
    #[arg(long, requires = "node")]
    chip: Option<String>,
    /// Also label the node kairon.zyvor.dev/memory-gib=N.
    #[arg(long, requires = "node")]
    memory_gib: Option<u64>,
    /// Turn on the web dashboard (kairon-ui).
    #[arg(long)]
    ui: bool,
    /// Where kairon-node caches downloaded images (the self-test needs one); must exist on each node, owned by UID 65532.
    #[arg(long)]
    image_cache_dir: Option<String>,
    /// Certificate lifetime in days for the production profile (Kairon's script uses 30; an expired webhook certificate
    /// rejects every Machine write, so the default is a year).
    #[arg(long, default_value_t = 365)]
    cert_days: i64,
    /// Make new certificates even if the Secrets exist (upgrade).
    #[arg(long)]
    rotate_certs: bool,
    /// Another chart version than the pinned one; needs --chart-digest.
    #[arg(long)]
    chart_version: Option<String>,
    /// Registry digest of that chart (sha256:...).
    #[arg(long)]
    chart_digest: Option<String>,
    /// Seconds to wait for the health checks to pass.
    #[arg(long, default_value_t = 300)]
    wait: u64,
    /// Print what would run and change nothing.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Clone)]
struct UninstallArgs {
    #[command(flatten)]
    common: Common,
    /// Also delete the CRDs and the Secrets fluxctl made. Deletes every Machine; needs --yes.
    #[arg(long)]
    purge: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Args, Clone)]
struct SelftestArgs {
    #[command(flatten)]
    common: Common,
    /// quick (about a minute), standard or full.
    #[arg(long, default_value = "quick", value_parser = ["quick", "standard", "full"])]
    profile: String,
}

#[derive(Subcommand)]
enum KaironCommand {
    /// Install the pinned Kairon release (evaluation profile, or `--production`), label the nodes you name, and check it works.
    Install(InstallArgs),
    /// Plain-English health checks with a fix per failure: CRDs, controller, dashboard, node agent, webhook, nodes. Exits 1 on a failure.
    Status(Common),
    /// Apply the pinned release's CRDs first (Helm only does that on the first install), then upgrade the release.
    Upgrade(InstallArgs),
    /// Remove the Helm release. CRDs, Secrets and Machines stay unless `--purge --yes`.
    Uninstall(UninstallArgs),
    /// Run `kaironctl selftest` (boots a tiny CirrOS VM) against the registered FluxVM hosts, or say what is missing.
    Selftest(SelftestArgs),
}

pub async fn run(a: KaironArgs) -> Result<i32> {
    let r = exec::RealRunner;
    match a.command {
        KaironCommand::Install(i) => install(&r, &i, false),
        KaironCommand::Upgrade(i) => install(&r, &i, true),
        KaironCommand::Status(c) => status(&r, &c),
        KaironCommand::Uninstall(u) => uninstall(&r, &u),
        KaironCommand::Selftest(s) => selftest(&r, &s),
    }
}

fn target(c: &Common) -> Result<Target> {
    let kubeconfig = match &c.kubeconfig {
        Some(p) => p.clone(),
        None => crate::docker_host::home()?
            .join(".kube")
            .join(format!("fluxvm-{}.yaml", c.name)),
    };
    if !kubeconfig.is_file() {
        bail!(
            "no kubeconfig at {}. Create the cluster first with `fluxctl kube up` (or `fluxctl kube apple create`), \
             or pass --kubeconfig for another cluster FluxVM made",
            kubeconfig.display()
        );
    }
    Ok(Target {
        kubeconfig,
        context: c.context.clone(),
    })
}

struct Ctx<'a> {
    r: &'a dyn Runner,
    t: Target,
}

impl Ctx<'_> {
    fn kubectl(&self, args: &[&str]) -> Result<Out> {
        self.r.run(&Cmd::new("kubectl", self.t.kubectl(args)))
    }
    fn kubectl_in(&self, args: &[&str], stdin: String) -> Result<Out> {
        self.r
            .run(&Cmd::new("kubectl", self.t.kubectl(args)).stdin(stdin))
    }
    fn helm(&self, args: &[&str]) -> Result<Out> {
        self.r.run(&Cmd::new("helm", self.t.helm(args)))
    }
    fn need(&self, what: &str) -> Result<Out> {
        let o = self.kubectl(&["get", "nodes", "-o", "json"])?;
        if !o.ok() {
            bail!(
                "cannot reach the cluster for {what}: {}\nIs it running? `fluxctl kube status`",
                o.why()
            );
        }
        Ok(o)
    }
}

fn preflight(c: &Ctx, helm: bool) -> Result<()> {
    if !c.r.has("kubectl") {
        bail!("kubectl is not on PATH. On a Mac: `brew install kubectl`");
    }
    if helm && !c.r.has("helm") {
        bail!(
            "helm is not on PATH. On a Mac: `brew install helm`; on Linux: https://helm.sh/docs/intro/install/"
        );
    }
    c.need("Kairon")?;
    let server = c.kubectl(&[
        "config",
        "view",
        "--minify",
        "-o",
        "jsonpath={.clusters[0].cluster.server}",
    ])?;
    println!(
        "cluster: {} (kubeconfig {})",
        server.stdout.trim(),
        c.t.kubeconfig.display()
    );
    Ok(())
}

struct Chart {
    dir: PathBuf,
    tgz: PathBuf,
    values: Option<PathBuf>,
}

impl Drop for Chart {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn private_dir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join(format!("fluxctl-kairon-{}-{n}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&d)
        .with_context(|| format!("creating {}", d.display()))?;
    Ok(d)
}

/// Pulls the chart and refuses unless the registry served the pinned digest. Nothing in the cluster has changed by then.
fn fetch_chart(c: &Ctx, pin: &Pin, production: bool) -> Result<Chart> {
    let dir = private_dir()?;
    let mut chart = Chart {
        tgz: dir.join(format!("kairon-{}.tgz", pin.version)),
        dir,
        values: None,
    };
    let o =
        c.r.run(&Cmd::new("helm", model::helm_pull_args(pin, &chart.dir)))?;
    if !o.ok() {
        bail!(
            "helm could not pull {} {}: {}",
            model::CHART_REF,
            pin.version,
            o.why()
        );
    }
    model::verify_digest(&format!("{}\n{}", o.stdout, o.stderr), pin)?;
    println!(
        "chart {} {} verified: {}",
        model::CHART_REF,
        pin.version,
        pin.digest
    );
    if production {
        let o = c.r.run(&Cmd::new(
            "tar",
            vec![
                "-xzOf".into(),
                chart.tgz.display().to_string(),
                "kairon/values-production.yaml".into(),
            ],
        ))?;
        if !o.ok() || o.stdout.is_empty() {
            bail!("the chart has no values-production.yaml: {}", o.why());
        }
        let p = chart.dir.join("values-production.yaml");
        std::fs::write(&p, o.stdout)?;
        chart.values = Some(p);
    }
    Ok(chart)
}

fn secret_field(json: &str, key: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let b = v.pointer(&format!("/data/{key}"))?.as_str()?;
    String::from_utf8(B64.decode(b).ok()?).ok()
}

fn secret_not_after(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.pointer("/metadata/annotations")?
        .get(model::NOT_AFTER_ANNOTATION)?
        .as_str()
        .map(str::to_string)
}

/// Makes the production Secrets if they are missing (or `rotate`), and returns the base64 CA bundle for the webhook. Existing
/// Secrets are reused so an upgrade does not break running migrations. Private keys only ever go to `kubectl` on stdin.
fn ensure_secrets(c: &Ctx, days: i64, rotate: bool) -> Result<String> {
    let o = c.kubectl_in(&["apply", "-f", "-"], model::namespace_yaml())?;
    if !o.ok() {
        bail!("could not create namespace {NAMESPACE}: {}", o.why());
    }
    let have = c.kubectl(&[
        "-n",
        NAMESPACE,
        "get",
        "secret",
        model::WEBHOOK_SECRET,
        "-o",
        "json",
    ])?;
    let existing_ca = have
        .ok()
        .then(|| secret_field(&have.stdout, "ca.crt"))
        .flatten();
    let bundle = match (existing_ca, rotate) {
        (Some(ca), false) => {
            println!(
                "webhook certificate: reusing Secret {}",
                model::WEBHOOK_SECRET
            );
            model::ca_bundle(&ca)
        }
        _ => {
            let w = certs::webhook(model::WEBHOOK_SERVICE, NAMESPACE, days)?;
            let y = model::webhook_secret_yaml(&w.cert_pem, &w.key_pem, &w.ca_pem, &w.not_after);
            let o = c.kubectl_in(&["apply", "-f", "-"], y)?;
            if !o.ok() {
                bail!(
                    "could not store Secret {}: {}",
                    model::WEBHOOK_SECRET,
                    o.why()
                );
            }
            println!(
                "webhook certificate: made Secret {} (valid until {})",
                model::WEBHOOK_SECRET,
                w.not_after
            );
            model::ca_bundle(&w.ca_pem)
        }
    };
    let mig = c.kubectl(&[
        "-n",
        NAMESPACE,
        "get",
        "secret",
        model::MIGRATION_SECRET,
        "-o",
        "name",
    ])?;
    if mig.ok() && !rotate {
        println!("migration mTLS: reusing Secret {}", model::MIGRATION_SECRET);
    } else {
        let m = certs::migration(model::MIGRATION_SERVER_NAME, days.clamp(1, 90))?;
        let y = model::migration_secret_yaml(&m.cert_pem, &m.key_pem, &m.ca_pem, &m.not_after);
        let o = c.kubectl_in(&["apply", "-f", "-"], y)?;
        if !o.ok() {
            bail!(
                "could not store Secret {}: {}",
                model::MIGRATION_SECRET,
                o.why()
            );
        }
        println!(
            "migration mTLS: made Secret {} (valid until {})",
            model::MIGRATION_SECRET,
            m.not_after
        );
    }
    Ok(bundle)
}

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn collect(c: &Ctx) -> Vec<Check> {
    let get = |args: &[&str]| -> Option<String> {
        c.kubectl(args).ok().filter(Out::ok).map(|o| o.stdout)
    };
    let crd = get(&["get", "crd", "-o", "json"]).unwrap_or_default();
    let dep = get(&["-n", NAMESPACE, "get", "deploy", "-o", "json"]).unwrap_or_default();
    let ds = get(&["-n", NAMESPACE, "get", "ds", "-o", "json"]).unwrap_or_default();
    let vwc = get(&[
        "get",
        "validatingwebhookconfiguration",
        model::WEBHOOK_SERVICE,
        "-o",
        "json",
    ]);
    let ep = get(&[
        "-n",
        NAMESPACE,
        "get",
        "endpoints",
        model::WEBHOOK_SERVICE,
        "-o",
        "json",
    ]);
    let sec = get(&[
        "-n",
        NAMESPACE,
        "get",
        "secret",
        model::WEBHOOK_SECRET,
        "-o",
        "json",
    ]);
    let nodes = get(&["get", "nodes", "-o", "json"]).unwrap_or_default();
    vec![
        health::crds(&crd),
        health::deployment("controller", &dep, "kairon-controller", false),
        health::deployment("dashboard", &dep, "kairon-ui", true),
        health::daemonset(&ds),
        health::webhook(
            vwc.as_deref(),
            ep.as_deref(),
            sec.as_deref().and_then(secret_not_after).as_deref(),
            &now_utc(),
        ),
        health::nodes(&health::kairon_nodes(&nodes)),
    ]
}

fn wait_healthy(c: &Ctx, secs: u64) -> Vec<Check> {
    let start = std::time::Instant::now();
    loop {
        let checks = collect(c);
        if !health::failed(&checks) || start.elapsed().as_secs() >= secs {
            return checks;
        }
        eprintln!(
            "waiting for Kairon to come up ({}s left)…",
            secs.saturating_sub(start.elapsed().as_secs())
        );
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

fn label_nodes(c: &Ctx, a: &InstallArgs) -> Result<()> {
    if a.node.is_empty() {
        return Ok(());
    }
    for (what, v) in [("--backend", &a.backend), ("--chip", &a.chip)] {
        if let Some(v) = v {
            model::check_label_value(what, v)?;
        }
    }
    let l = NodeLabels {
        backend: a.backend.clone(),
        chip: a.chip.clone(),
        memory_gib: a.memory_gib,
    };
    for n in &a.node {
        model::check_label_value("--node", n)?;
        let o =
            c.r.run(&Cmd::new("kubectl", model::label_args(&c.t, n, &l)))?;
        if !o.ok() {
            bail!(
                "could not label node {n}: {}\nList the nodes with `kubectl get nodes`",
                o.why()
            );
        }
        println!("labelled node {n}: {}", model::label_pairs(&l).join(" "));
    }
    Ok(())
}

fn install(r: &dyn Runner, a: &InstallArgs, upgrade: bool) -> Result<i32> {
    let pin = Pin::from_flags(a.chart_version.as_deref(), a.chart_digest.as_deref())?;
    let opts = InstallOpts {
        production: a.production,
        ui: a.ui,
        image_cache_dir: a.image_cache_dir.clone(),
        ca_bundle: None,
    };
    if a.dry_run {
        let t = Target {
            kubeconfig: PathBuf::from("<kubeconfig>"),
            context: a.common.context.clone(),
        };
        println!(
            "dry run: nothing is contacted or changed. Kairon {} ({} profile) would be installed as follows.",
            pin.version,
            if a.production {
                "production"
            } else {
                "evaluation"
            }
        );
        println!(
            "  1. helm {}",
            model::helm_pull_args(&pin, Path::new("<tmp>")).join(" ")
        );
        println!("     refuse unless it reports Digest: {}", pin.digest);
        if upgrade {
            println!("  2. helm show crds <chart> | kubectl apply --server-side -f -");
        }
        if a.production {
            println!(
                "  3. make Secrets {} and {} (certificates generated in process; keys are not printed)",
                model::WEBHOOK_SECRET,
                model::MIGRATION_SECRET
            );
        }
        for n in &a.node {
            println!(
                "  4. kubectl {}",
                model::label_args(
                    &t,
                    n,
                    &NodeLabels {
                        backend: a.backend.clone(),
                        chip: a.chip.clone(),
                        memory_gib: a.memory_gib
                    }
                )
                .join(" ")
            );
        }
        let mut o2 = opts.clone();
        o2.ca_bundle = Some("<webhook CA bundle>".into());
        println!(
            "  5. helm {}",
            model::helm_install_args(
                &t,
                Path::new("<verified chart>"),
                &o2,
                Some(Path::new("<values-production.yaml from the chart>"))
            )?
            .join(" ")
        );
        println!("  6. health checks, as in `fluxctl kube kairon status`");
        return Ok(0);
    }
    let c = Ctx {
        r,
        t: target(&a.common)?,
    };
    preflight(&c, true)?;
    let released = c.helm(&["status", RELEASE, "-n", NAMESPACE])?.ok();
    if released && !upgrade {
        bail!(
            "Kairon is already installed here. `fluxctl kube kairon upgrade` changes it; `fluxctl kube kairon status` checks it"
        );
    }
    if !released && upgrade {
        bail!("Kairon is not installed here yet. Run `fluxctl kube kairon install`");
    }
    let chart = fetch_chart(&c, &pin, a.production)?;
    if upgrade {
        let crds = c.r.run(&Cmd::new(
            "helm",
            vec![
                "show".into(),
                "crds".into(),
                chart.tgz.display().to_string(),
            ],
        ))?;
        if !crds.ok() || crds.stdout.is_empty() {
            bail!("could not read the CRDs from the chart: {}", crds.why());
        }
        let o = c.kubectl_in(&["apply", "--server-side", "-f", "-"], crds.stdout)?;
        if !o.ok() {
            bail!("could not apply the CRDs: {}", o.why());
        }
        println!(
            "CRDs of {} applied (server-side) before the upgrade",
            pin.version
        );
    }
    let mut opts = opts;
    if a.production {
        opts.ca_bundle = Some(ensure_secrets(&c, a.cert_days, a.rotate_certs)?);
    }
    label_nodes(&c, a)?;
    let args = model::helm_install_args(&c.t, &chart.tgz, &opts, chart.values.as_deref())?;
    let o = c.r.run(&Cmd::new("helm", args))?;
    if !o.ok() {
        bail!(
            "helm {} failed: {}",
            if upgrade { "upgrade" } else { "install" },
            o.why()
        );
    }
    println!(
        "Kairon {} {} ({} profile)",
        pin.version,
        if upgrade { "upgraded" } else { "installed" },
        if a.production {
            "production"
        } else {
            "evaluation"
        }
    );
    let checks = wait_healthy(&c, a.wait);
    print!("{}", health::render(&checks));
    if a.node.is_empty()
        && checks
            .iter()
            .any(|x| x.name == "Kairon nodes" && x.level == Level::Fail)
    {
        println!(
            "\nNext: name a FluxVM host with `--node NAME` (or let a Mac's kairon-node register itself), see docs/kairon-on-kube.md."
        );
    }
    Ok(if health::failed(&checks) { 1 } else { 0 })
}

fn status(r: &dyn Runner, a: &Common) -> Result<i32> {
    let c = Ctx { r, t: target(a)? };
    preflight(&c, false)?;
    let checks = collect(&c);
    print!("{}", health::render(&checks));
    Ok(if health::failed(&checks) { 1 } else { 0 })
}

fn uninstall(r: &dyn Runner, a: &UninstallArgs) -> Result<i32> {
    if a.purge && !a.yes {
        bail!(
            "--purge deletes the Kairon CRDs, every Machine and the Secrets fluxctl made; add --yes if you mean it"
        );
    }
    let c = Ctx {
        r,
        t: target(&a.common)?,
    };
    preflight(&c, true)?;
    let o = c.helm(&["uninstall", RELEASE, "-n", NAMESPACE])?;
    if !o.ok() {
        bail!("helm uninstall failed: {}", o.why());
    }
    println!("Helm release {RELEASE} removed.");
    if !a.purge {
        println!(
            "Kept: the CRDs (Helm never deletes them), every Machine, the Secrets, and the node labels. `--purge --yes` removes the CRDs and Secrets; `kubectl label node NAME kairon.zyvor.dev/capable-` unlabels a node."
        );
        return Ok(0);
    }
    let crds = c.kubectl(&["get", "crd", "-o", "name"])?;
    let names: Vec<&str> = crds
        .stdout
        .lines()
        .filter(|l| l.ends_with(".kairon.zyvor.dev"))
        .collect();
    for n in &names {
        let o = c.kubectl(&["delete", n, "--ignore-not-found"])?;
        if !o.ok() {
            bail!("could not delete {n}: {}", o.why());
        }
    }
    for s in [model::WEBHOOK_SECRET, model::MIGRATION_SECRET] {
        c.kubectl(&["-n", NAMESPACE, "delete", "secret", s, "--ignore-not-found"])?;
    }
    println!("Purged {} CRDs and the Secrets.", names.len());
    Ok(0)
}

fn selftest(r: &dyn Runner, a: &SelftestArgs) -> Result<i32> {
    let c = Ctx {
        r,
        t: target(&a.common)?,
    };
    preflight(&c, false)?;
    let nodes = c.kubectl(&["get", "nodes", "-o", "json"])?;
    let plan = health::plan_selftest(&health::kairon_nodes(&nodes.stdout));
    println!("{}", health::selftest_explanation(&plan));
    if !matches!(plan, health::SelftestPlan::Run { .. }) {
        return Ok(2);
    }
    let dir = crate::docker_host::home()?
        .join(".cache")
        .join("fluxvm")
        .join("kairon")
        .join(model::CHART_VERSION);
    let exe = kaironctl::ensure(
        r,
        &dir,
        model::CHART_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )?;
    println!(
        "kaironctl {} verified against the release's SHA256SUMS",
        model::CHART_VERSION
    );
    let mut args = vec![
        "selftest".to_string(),
        "--profile".into(),
        a.profile.clone(),
    ];
    args.extend(c.t.kubectl(&[]));
    let o = r.run(&Cmd::new(&exe.display().to_string(), args).stream())?;
    Ok(o.code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exec::fake::Fake;

    fn ctx(f: &Fake) -> Ctx<'_> {
        Ctx {
            r: f,
            t: Target {
                kubeconfig: PathBuf::from("/k.yaml"),
                context: None,
            },
        }
    }

    fn args(production: bool) -> InstallArgs {
        InstallArgs {
            common: Common {
                name: "x".into(),
                kubeconfig: None,
                context: None,
            },
            production,
            node: vec![],
            backend: None,
            chip: None,
            memory_gib: None,
            ui: false,
            image_cache_dir: None,
            cert_days: 365,
            rotate_certs: false,
            chart_version: None,
            chart_digest: None,
            wait: 0,
            dry_run: false,
        }
    }

    #[test]
    fn secret_readers() {
        let j = r#"{"metadata":{"annotations":{"fluxvm.zyvor.dev/not-after":"2027-01-01T00:00:00Z"}},"data":{"ca.crt":"Q0E="}}"#;
        assert_eq!(secret_field(j, "ca.crt").as_deref(), Some("CA"));
        assert_eq!(secret_field(j, "tls.key"), None);
        assert_eq!(secret_not_after(j).as_deref(), Some("2027-01-01T00:00:00Z"));
        assert_eq!(secret_field("nope", "ca.crt"), None);
    }

    #[test]
    fn digest_mismatch_stops_before_any_cluster_change() {
        let f = Fake::default();
        f.rule("pull", 0, "Pulled: x\nDigest: sha256:0000000000000000000000000000000000000000000000000000000000000000\n");
        let c = ctx(&f);
        let e = fetch_chart(&c, &Pin::pinned(), false)
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("not the pinned"));
        // exactly one call: the pull
        assert_eq!(f.shown().len(), 1);
        assert!(
            f.shown()[0]
                .starts_with("helm pull oci://ghcr.io/zyvorai/charts/kairon --version 0.10.6")
        );
    }

    #[test]
    fn pulled_chart_is_removed_on_drop() {
        let f = Fake::default();
        f.rule("pull", 0, &format!("Digest: {}\n", model::CHART_DIGEST));
        let c = ctx(&f);
        let ch = fetch_chart(&c, &Pin::pinned(), false).unwrap();
        let d = ch.dir.clone();
        assert!(d.is_dir());
        drop(ch);
        assert!(!d.exists());
    }

    #[test]
    fn production_secrets_go_on_stdin_and_never_into_arguments() {
        let f = Fake::default();
        f.rule("get secret kairon-webhook-tls", 1, "");
        f.rule("get secret kairon-migration-tls", 1, "");
        let b = ensure_secrets(&ctx(&f), 365, false).unwrap();
        assert!(!b.is_empty());
        let calls = f.calls.borrow();
        let applies: Vec<&Cmd> = calls
            .iter()
            .filter(|c| c.args.contains(&"apply".to_string()))
            .collect();
        assert_eq!(applies.len(), 3); // namespace, webhook Secret, migration Secret
        let hook = applies[1].stdin.as_ref().unwrap();
        assert!(
            hook.contains("kind: Secret")
                && hook.contains("name: kairon-webhook-tls")
                && hook.contains("tls.key:")
        );
        // no private key material, PEM or base64 of it, in any argument list
        for c in calls.iter() {
            for a in &c.args {
                assert!(
                    !a.contains("PRIVATE KEY") && !a.contains("LS0tLS1CRUdJTiBQUklWQVRF"),
                    "{a}"
                );
            }
        }
    }

    #[test]
    fn existing_secrets_are_reused() {
        let f = Fake::default();
        f.rule(
            "get secret kairon-webhook-tls",
            0,
            r#"{"data":{"ca.crt":"Q0E="}}"#,
        );
        f.rule(
            "get secret kairon-migration-tls",
            0,
            "secret/kairon-migration-tls",
        );
        let b = ensure_secrets(&ctx(&f), 365, false).unwrap();
        assert_eq!(b, "Q0E=");
        // only the namespace apply
        assert_eq!(
            f.calls
                .borrow()
                .iter()
                .filter(|c| c.args.contains(&"apply".to_string()))
                .count(),
            1
        );
    }

    #[test]
    fn rotate_replaces_existing_secrets() {
        let f = Fake::default();
        f.rule(
            "get secret kairon-webhook-tls",
            0,
            r#"{"data":{"ca.crt":"Q0E="}}"#,
        );
        f.rule(
            "get secret kairon-migration-tls",
            0,
            "secret/kairon-migration-tls",
        );
        let b = ensure_secrets(&ctx(&f), 365, true).unwrap();
        assert_ne!(b, "Q0E=");
        assert_eq!(
            f.calls
                .borrow()
                .iter()
                .filter(|c| c.args.contains(&"apply".to_string()))
                .count(),
            3
        );
    }

    #[test]
    fn labels_are_applied_per_node() {
        let f = Fake::default();
        let mut a = args(false);
        a.node = vec!["studio-1".into(), "kvm-1".into()];
        a.backend = Some("vz".into());
        label_nodes(&ctx(&f), &a).unwrap();
        let s = f.shown();
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            "kubectl --kubeconfig /k.yaml label node studio-1 kairon.zyvor.dev/capable=true kairon.zyvor.dev/backend.vz=true --overwrite"
        );
        a.chip = Some("bad chip".into());
        assert!(label_nodes(&ctx(&f), &a).is_err());
    }

    #[test]
    fn collect_reads_all_the_things_and_flags_an_empty_cluster() {
        let f = Fake::default();
        let c = ctx(&f);
        let checks = collect(&c);
        assert_eq!(checks.len(), 6);
        assert!(health::failed(&checks));
        // the webhook is skipped, not failed, when its configuration does not exist
        f.rule("get validatingwebhookconfiguration", 1, "");
        assert_eq!(collect(&c)[4].level, Level::Skip);
        let shown = f.shown().join("\n");
        assert!(shown.contains("kubectl --kubeconfig /k.yaml get crd -o json"));
        assert!(shown.contains("-n kairon-system get deploy -o json"));
    }

    #[test]
    fn selftest_without_fluxvm_hosts_explains_and_runs_nothing_else() {
        let f = Fake::default();
        f.rule("get nodes", 0, r#"{"items":[{"metadata":{"name":"fluxvm-kube","labels":{"kairon.zyvor.dev/capable":"true","node-role.kubernetes.io/control-plane":"true"}},"status":{"conditions":[{"type":"Ready","status":"True"}]}}]}"#);
        let c = ctx(&f);
        let nodes = c.kubectl(&["get", "nodes", "-o", "json"]).unwrap();
        let plan = health::plan_selftest(&health::kairon_nodes(&nodes.stdout));
        assert!(matches!(
            plan,
            health::SelftestPlan::OnlyControlPlane { .. }
        ));
    }
}
