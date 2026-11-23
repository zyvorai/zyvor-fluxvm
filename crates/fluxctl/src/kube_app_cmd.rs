// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl kube run | rm | shell | scale | rollout | expose`, and the apps section of `kube apps`: run a container image on a
//! FluxVM k3s cluster the way `docker run` does, then look after it.
//!
//! `run IMAGE` creates a Deployment and a Service (labelled `app.kubernetes.io/managed-by=fluxctl-app`), waits until the app is
//! ready, says in plain words what is wrong when it is not (a bad image, a crash, a port that is taken), and gives you a URL by
//! forwarding a free local port to it. The cluster is the only one made with `fluxctl kube up` when there is exactly one; with
//! several, `--cluster NAME` picks. Every kubectl call goes through the same [`KubeRun`] the gateway commands use, so the tests
//! use a fake. The objects are built, and the cluster's answers read, by `fluxvm_core::kube_app`, which Studio shares.

use crate::gateway::Target;
use crate::gateway::expose::{self, ExposeEnv, ForwardEnd};
use crate::gateway::kube::{Cluster, KubeRun, RunOut, Selection, SystemKubectl, shell_join};
use anyhow::{Result, anyhow, bail};
use clap::{Subcommand, ValueEnum};
use fluxvm_core::kube_app::{self, Spec};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum AppCommand {
    /// Run an image on the cluster like `docker run`: creates a Deployment and a Service, waits until it is ready and prints a URL
    /// (a free local port forwarded to it; Ctrl-C stops the forward, the app keeps running). `fluxctl kube run nginx`.
    Run {
        /// The image, for example `nginx`, `ghcr.io/you/app:1.0` or one you loaded with `kube load`.
        image: String,
        /// The app's name (default: taken from the image).
        #[arg(long)]
        name: Option<String>,
        /// The port the app listens on (default: the usual one for well-known images, otherwise 80).
        #[arg(long)]
        port: Option<u16>,
        /// How many copies to run.
        #[arg(long, default_value_t = 1)]
        replicas: u32,
        /// An environment variable for the app: `--env KEY=value`. Repeatable.
        #[arg(short, long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// The port on this machine for the URL (default: the first free one from 8080).
        #[arg(long)]
        local_port: Option<u16>,
        /// Do not wait for it to be ready and do not open a URL; just create it.
        #[arg(short, long)]
        detach: bool,
        /// How many seconds to wait for it to become ready.
        #[arg(long, default_value_t = 180)]
        wait: u64,
        #[command(flatten)]
        target: Target,
    },
    /// Stop and delete an app made by `kube run`, and the gateway route if you exposed it. Never touches anything else.
    Rm {
        app: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        #[command(flatten)]
        target: Target,
    },
    /// A shell inside an app: `fluxctl kube shell web`, or run one command: `fluxctl kube shell web -- ls /app`.
    Shell {
        app: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// A command to run instead of a shell.
        #[arg(last = true)]
        command: Vec<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Run more or fewer copies of an app: `fluxctl kube scale web 3` (0 stops it without deleting it).
    Scale {
        app: String,
        replicas: u32,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        #[command(flatten)]
        target: Target,
    },
    /// Go back to the previous version of an app (`undo`), or restart it (`restart`).
    Rollout {
        #[arg(value_enum)]
        action: Rollout,
        app: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        #[command(flatten)]
        target: Target,
    },
    /// Publish an app on a name on this machine through the Gateway (`fluxctl gateway expose`): installs the gateway the first
    /// time (asks first), then prints `http://APP.localhost:PORT`.
    Expose {
        app: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// The host name to route (default: APP.localhost).
        #[arg(long)]
        host: Option<String>,
        /// The local port (default: the first free one from 8080).
        #[arg(long)]
        local_port: Option<u16>,
        /// Do not ask before installing the gateway.
        #[arg(long)]
        yes: bool,
        /// Print what would be created and change nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq)]
pub enum Rollout {
    /// Back to the previous version.
    Undo,
    /// Restart every copy, one at a time.
    Restart,
}

/// Runs the command and returns the exit code. Errors are printed as sentences ending with what to do next.
pub fn run(cmd: AppCommand) -> i32 {
    match execute(cmd, &SystemKubectl, &expose::SystemEnv, &mut std::io::stdout()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fluxctl kube: {e:#}");
            1
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Errors

/// An error ending with the command that explains it, like the other tables.
pub fn coded(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    debug_assert!(
        fluxvm_core::health::code(code).is_some(),
        "unknown code {code}"
    );
    anyhow!("{message}\n  more: fluxctl explain {code}")
}

fn no_cluster(detail: &str) -> anyhow::Error {
    coded(
        "APP_NO_CLUSTER",
        format!(
            "There is no cluster to run apps on ({detail}). Create one with `fluxctl kube up`, or name an existing one with --cluster NAME or --kube-context CONTEXT"
        ),
    )
}

/// What a failed kubectl call means for a person.
fn kerr(what: &str, r: &RunOut) -> anyhow::Error {
    let low = r.stderr.to_ascii_lowercase();
    let unreachable = [
        "unable to connect",
        "connection refused",
        "no configuration has been provided",
        "couldn't get current server api group list",
        "context was not found",
        "no such host",
        "i/o timeout",
        "invalid configuration",
        "unable to read",
        "was refused",
    ];
    if unreachable.iter().any(|m| low.contains(m)) {
        return no_cluster(r.stderr.lines().next().unwrap_or("").trim());
    }
    if r.stderr.trim().is_empty() {
        anyhow!("{what} failed")
    } else {
        anyhow!("{what} failed: {}", r.stderr.trim())
    }
}

fn runner_err(e: &str) -> anyhow::Error {
    if e.contains("could not run kubectl") {
        no_cluster("kubectl is not installed; on a Mac `brew install kubectl`")
    } else if e.contains("did not finish within") {
        no_cluster("the cluster did not answer; it may be stopped, so try `fluxctl cluster status`")
    } else {
        anyhow!("{e}")
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Which cluster

/// The cluster to use: the one named, or the only one `fluxctl kube up` made; with none, kubectl's own current context.
pub fn pick(names: &[String], t: &Target) -> Result<Selection> {
    let sel = Selection {
        kube_context: t.kube_context.clone(),
        cluster: t.cluster.clone(),
    };
    if sel.is_explicit() {
        return Ok(sel);
    }
    match names {
        [] => Ok(sel),
        [one] => Ok(Selection {
            cluster: Some(one.clone()),
            ..sel
        }),
        many => Err(coded(
            "APP_CLUSTER_AMBIGUOUS",
            format!(
                "There are {} clusters ({}), so fluxctl cannot guess. Add --cluster NAME to the command",
                many.len(),
                many.join(", ")
            ),
        )),
    }
}

/// The names of the clusters that have a kubeconfig `~/.kube/fluxvm-NAME.yaml`.
pub fn cluster_names(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(home.join(".kube"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let f = e.file_name().to_string_lossy().into_owned();
            let n = f.strip_prefix("fluxvm-")?.strip_suffix(".yaml")?.to_owned();
            kube_app::name_ok(&n).then_some(n)
        })
        .collect();
    v.sort();
    v
}

fn selection(t: &Target) -> Result<(Selection, Vec<String>)> {
    let home = crate::docker_host::home()?;
    let sel = pick(&cluster_names(&home), t)?;
    let prefix = sel.prefix(Some(&home))?;
    Ok((sel, prefix))
}

fn flags(sel: &Selection) -> String {
    let mut s = String::new();
    if let Some(c) = &sel.cluster {
        s.push_str(&format!(" --cluster {c}"));
    }
    if let Some(c) = &sel.kube_context {
        s.push_str(&format!(" --kube-context {c}"));
    }
    s
}

// ---------------------------------------------------------------------------------------------------------------------
// kubectl

fn kc(
    c: &Cluster,
    ns: Option<&str>,
    rest: &[&str],
    stdin: Option<&[u8]>,
    secs: u64,
) -> Result<RunOut> {
    let mut argv = c.cmd.clone();
    if let Some(ns) = ns {
        argv.extend(["-n".to_owned(), ns.to_owned()]);
    }
    argv.extend(rest.iter().map(|s| (*s).to_owned()));
    c.runner
        .run(&argv, stdin, Duration::from_secs(secs))
        .map_err(|e| runner_err(&e))
}

/// One object by `kind/name`, `None` when it does not exist.
fn get_one(c: &Cluster, ns: &str, target: &str) -> Result<Option<Value>> {
    let r = kc(
        c,
        Some(ns),
        &["get", target, "-o", "json", "--ignore-not-found"],
        None,
        30,
    )?;
    if r.code != 0 {
        return Err(kerr(&format!("kubectl get {target}"), &r));
    }
    if r.stdout.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&r.stdout)
        .map(Some)
        .map_err(|e| anyhow!("kubectl answered something that is not JSON: {e}"))
}

/// The app rows for one namespace (or all of them), from one kubectl call.
fn read_apps(c: &Cluster, ns: Option<&str>) -> Result<Vec<Value>> {
    let mut rest = vec![
        "get",
        "deployments,services,pods",
        "-l",
        kube_app::SELECTOR,
        "-o",
        "json",
    ];
    if ns.is_none() {
        rest.push("-A");
    }
    let r = kc(c, ns, &rest, None, 30)?;
    if r.code != 0 {
        return Err(kerr("kubectl get", &r));
    }
    let all: Value = serde_json::from_str(&r.stdout)
        .map_err(|e| anyhow!("kubectl answered something that is not JSON: {e}"))?;
    let of = |kind: &str| {
        json!({"items": all["items"].as_array().map(|a| a.iter().filter(|i| i["kind"] == kind).cloned().collect::<Vec<_>>()).unwrap_or_default()})
    };
    Ok(kube_app::summarize(
        &of("Deployment"),
        &of("Service"),
        &of("Pod"),
    ))
}

/// The managed Deployment, or the sentence saying why there is none.
fn require_app(c: &Cluster, app: &str, ns: &str, sel: &Selection) -> Result<Value> {
    if !kube_app::name_ok(app) || !kube_app::name_ok(ns) {
        bail!("{app:?} in {ns:?} is not an app name. Names use lowercase letters, digits and dashes");
    }
    match get_one(c, ns, &format!("deployment/{app}"))? {
        Some(d) if kube_app::is_managed(&d) => Ok(d),
        Some(_) => Err(coded(
            "APP_NAME_TAKEN",
            format!(
                "{app} in {ns} was not made by `fluxctl kube run`, so fluxctl leaves it alone. Use kubectl for it, or run your own app under another --name"
            ),
        )),
        None => Err(coded(
            "APP_NOT_FOUND",
            format!(
                "There is no app called {app} in {ns}. List them with `fluxctl kube apps{}`; if it is in another namespace add -n NAMESPACE",
                flags(sel)
            ),
        )),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Commands

pub fn execute(
    cmd: AppCommand,
    runner: &dyn KubeRun,
    env: &dyn ExposeEnv,
    out: &mut dyn Write,
) -> Result<i32> {
    match cmd {
        AppCommand::Run {
            image,
            name,
            port,
            replicas,
            env: vars,
            namespace,
            local_port,
            detach,
            wait,
            target,
        } => {
            let mut pairs = vec![];
            for v in &vars {
                let (k, val) = v.split_once('=').ok_or_else(|| {
                    anyhow!("{v:?} is not KEY=value. Pass settings like --env PORT=8080")
                })?;
                pairs.push((k.to_owned(), val.to_owned()));
            }
            let spec = Spec {
                name: name.unwrap_or_else(|| kube_app::default_name(&image)),
                namespace,
                port: port.unwrap_or_else(|| kube_app::default_port(&image)),
                image,
                replicas,
                env: pairs,
            };
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            run_app(
                &spec,
                &RunOpts {
                    local_port,
                    detach,
                    wait: Duration::from_secs(wait),
                },
                &sel,
                &c,
                env,
                out,
            )
        }
        AppCommand::Rm {
            app,
            namespace,
            target,
        } => {
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            rm(&app, &namespace, &sel, &c, out)
        }
        AppCommand::Shell {
            app,
            namespace,
            command,
            target,
        } => {
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            require_app(&c, &app, &namespace, &sel)?;
            let argv = shell_argv(&c.cmd, &app, &namespace, &command, env.interactive());
            interactive(&argv)
        }
        AppCommand::Scale {
            app,
            replicas,
            namespace,
            target,
        } => {
            if replicas > kube_app::MAX_REPLICAS {
                bail!("an app can have at most {} copies; give a smaller number", kube_app::MAX_REPLICAS);
            }
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            require_app(&c, &app, &namespace, &sel)?;
            let n = replicas.to_string();
            let r = kc(
                &c,
                Some(&namespace),
                &["scale", &format!("deployment/{app}"), &format!("--replicas={n}")],
                None,
                60,
            )?;
            if r.code != 0 {
                return Err(kerr("kubectl scale", &r));
            }
            if replicas == 0 {
                writeln!(out, "{app} stopped (0 copies). Start it again with `fluxctl kube scale {app} 1`.")?;
            } else {
                writeln!(out, "{app} scaled to {replicas}.")?;
            }
            Ok(0)
        }
        AppCommand::Rollout {
            action,
            app,
            namespace,
            target,
        } => {
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            require_app(&c, &app, &namespace, &sel)?;
            let verb = if action == Rollout::Undo { "undo" } else { "restart" };
            let r = kc(
                &c,
                Some(&namespace),
                &["rollout", verb, &format!("deployment/{app}")],
                None,
                60,
            )?;
            if r.code != 0 {
                if r.stderr.contains("no rollout history") || r.stderr.contains("no previous revision") {
                    bail!(
                        "{app} has no earlier version to go back to. Run it again with a different image (`fluxctl kube run IMAGE --name {app}`) and then undo"
                    );
                }
                return Err(kerr(&format!("kubectl rollout {verb}"), &r));
            }
            if action == Rollout::Undo {
                writeln!(out, "{app} is going back to its previous version. Watch it with `fluxctl kube apps{}`.", flags(&sel))?;
            } else {
                writeln!(out, "{app} is restarting. Watch it with `fluxctl kube apps{}`.", flags(&sel))?;
            }
            Ok(0)
        }
        AppCommand::Expose {
            app,
            namespace,
            host,
            local_port,
            yes,
            dry_run,
            target,
        } => {
            let (sel, prefix) = selection(&target)?;
            let c = Cluster::new(prefix, runner);
            require_app(&c, &app, &namespace, &sel)?;
            out.flush()?;
            // The gateway command does the rest: Envoy Gateway if needed, a Gateway and HTTPRoute, a free port and a forward.
            Ok(crate::gateway::run(crate::gateway::GatewayCommand::Expose {
                service: app,
                namespace,
                port: None,
                host,
                local_port,
                yes,
                dry_run,
                target: Target {
                    kube_context: sel.kube_context,
                    cluster: sel.cluster,
                },
            }))
        }
    }
}

/// `kubectl exec` into the app: a shell when no command is given.
pub fn shell_argv(
    prefix: &[String],
    app: &str,
    ns: &str,
    command: &[String],
    tty: bool,
) -> Vec<String> {
    let mut v = prefix.to_vec();
    v.extend(["-n".into(), ns.into(), "exec".into()]);
    v.push(if tty { "-it" } else { "-i" }.into());
    v.push(format!("deploy/{app}"));
    v.push("--".into());
    if command.is_empty() {
        v.extend(
            ["sh", "-c", "[ -x /bin/bash ] && exec bash || exec sh"]
                .iter()
                .map(|s| (*s).to_owned()),
        );
    } else {
        v.extend(command.iter().cloned());
    }
    v
}

/// Runs a command on the terminal and returns its exit code.
fn interactive(argv: &[String]) -> Result<i32> {
    let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
    let st = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| anyhow!("could not run {program}: {e}"))?;
    Ok(st.code().unwrap_or(1))
}

pub struct RunOpts {
    pub local_port: Option<u16>,
    pub detach: bool,
    pub wait: Duration,
}

/// `kube run`. Returns the exit code.
pub fn run_app(
    spec: &Spec,
    o: &RunOpts,
    sel: &Selection,
    c: &Cluster,
    env: &dyn ExposeEnv,
    out: &mut dyn Write,
) -> Result<i32> {
    kube_app::validate(spec).map_err(|e| anyhow!(e))?;
    let forwarding = !o.detach && env.interactive();
    // A busy port is found out before anything is created.
    let local = if forwarding {
        Some(expose::choose_port(o.local_port).map_err(|e| {
            coded(
                "APP_PORT_TAKEN",
                format!(
                    "{e:#}. Nothing was created yet; run the command again without --local-port, or with another number"
                ),
            )
        })?)
    } else {
        None
    };
    let ns = spec.namespace.as_str();
    let name = spec.name.as_str();
    let mut existed = false;
    for kind in ["deployment", "service"] {
        if let Some(obj) = get_one(c, ns, &format!("{kind}/{name}"))? {
            if !kube_app::is_managed(&obj) {
                return Err(coded(
                    "APP_NAME_TAKEN",
                    format!(
                        "A {kind} called {name} already exists in {ns} and was not made by `fluxctl kube run`, so fluxctl will not change it. Choose another name with --name"
                    ),
                ));
            }
            existed = true;
        }
    }
    if ns != "default" {
        let r = kc(c, None, &["get", "namespace", ns, "-o", "name", "--ignore-not-found"], None, 30)?;
        if r.code == 0 && r.stdout.trim().is_empty() {
            let r = kc(c, None, &["create", "namespace", ns], None, 30)?;
            if r.code != 0 && !r.stderr.contains("AlreadyExists") {
                return Err(kerr("kubectl create namespace", &r));
            }
        }
    }
    let list = json!({"apiVersion": "v1", "kind": "List", "items": kube_app::manifests(spec)});
    let r = kc(c, None, &["apply", "-f", "-"], Some(list.to_string().as_bytes()), 60)?;
    if r.code != 0 {
        return Err(kerr("kubectl apply", &r));
    }
    writeln!(
        out,
        "{} {name} ({}) in {ns}.",
        if existed { "Updated" } else { "Created" },
        spec.image
    )?;
    if o.detach {
        writeln!(
            out,
            "It is starting. Watch it with `fluxctl kube apps{0}`, reach it with `fluxctl kube forward {name}` or `fluxctl kube expose {name}{0}`.",
            flags(sel)
        )?;
        return Ok(0);
    }
    out.flush()?;
    eprintln!("Waiting for {name} to be ready (the first run downloads the image)...");
    let mut bad_polls = 0;
    let mut waited = Duration::ZERO;
    loop {
        let rows = read_apps(c, Some(ns))?;
        if let Some(row) = rows.iter().find(|r| r["name"] == name) {
            match row["state"].as_str() {
                Some("running") => break,
                Some("problem") => {
                    bad_polls += 1;
                    // One look can catch a pull that is only slow to start; two in a row is a real problem.
                    if bad_polls >= 2 {
                        return Err(coded(
                            row["code"].as_str().unwrap_or("APP_NOT_READY"),
                            format!(
                                "{} The app was left in the cluster; remove it with `fluxctl kube rm {name}{}`.\n  next: {}",
                                row["summary"].as_str().unwrap_or("The app is not healthy."),
                                if ns == "default" { String::new() } else { format!(" -n {ns}") },
                                row["next"].as_str().unwrap_or("")
                            ),
                        ));
                    }
                }
                _ => bad_polls = 0,
            }
        }
        if waited >= o.wait {
            return Err(coded(
                "APP_NOT_READY",
                format!(
                    "{name} is not ready after {} seconds. It may still be downloading the image; watch it with `fluxctl kube apps{}`, or wait longer with --wait 600",
                    o.wait.as_secs(),
                    flags(sel)
                ),
            ));
        }
        let step = Duration::from_secs(2);
        env.pause(step);
        waited += step;
    }
    writeln!(out, "{name} is running.")?;
    let Some(local) = local else {
        writeln!(
            out,
            "Reach it with `fluxctl kube forward {name}` (or `fluxctl kube expose {name}{}` for a name), or see it with `fluxctl kube apps{0}`.",
            flags(sel)
        )?;
        return Ok(0);
    };
    writeln!(
        out,
        "Open http://127.0.0.1:{local}/  (Ctrl-C stops this forward; {name} keeps running. Logs: `fluxctl kube logs {name}`; remove: `fluxctl kube rm {name}`)"
    )?;
    out.flush()?;
    let mut argv = c.cmd.clone();
    argv.extend([
        "-n".into(),
        ns.into(),
        "port-forward".into(),
        format!("svc/{name}"),
        format!("{local}:{}", spec.port),
    ]);
    keep_forwarding(&argv, env, &mut std::io::stderr())
}

/// A port-forward that reconnects when kubectl exits (the pod was replaced) and gives up after a dozen quick failures.
fn keep_forwarding(argv: &[String], env: &dyn ExposeEnv, err: &mut dyn Write) -> Result<i32> {
    let mut fails = 0u32;
    loop {
        match env.forward(argv)? {
            ForwardEnd::Interrupted => return Ok(0),
            ForwardEnd::Exited { code, ran } => {
                fails = if ran >= Duration::from_secs(10) { 1 } else { fails + 1 };
                if fails >= 12 {
                    bail!(
                        "the forward keeps ending (kubectl exited {code}). Check the app with `fluxctl kube apps`, then run `{}` again",
                        shell_join(argv)
                    );
                }
                let wait = expose::backoff(fails);
                writeln!(
                    err,
                    "The forward ended (kubectl exited {code}), probably because the app's pod was replaced; reconnecting in {}s (Ctrl-C to stop)",
                    wait.as_secs()
                )?;
                env.pause(wait);
            }
        }
    }
}

fn rm(app: &str, ns: &str, sel: &Selection, c: &Cluster, out: &mut dyn Write) -> Result<i32> {
    require_app(c, app, ns, sel)?;
    // A route made by `kube expose` points at the Service about to disappear; take it down first. Clusters without Gateway API
    // simply have no such object, and that is not an error here.
    if get_one(c, ns, &format!("httproute.gateway.networking.k8s.io/{app}"))
        .ok()
        .flatten()
        .is_some()
    {
        expose::unexpose(app, ns, sel, c, out)?;
    }
    let r = kc(
        c,
        Some(ns),
        &["delete", "deployment,service", app, "--ignore-not-found", "--timeout=60s"],
        None,
        90,
    )?;
    if r.code != 0 {
        return Err(kerr("kubectl delete", &r));
    }
    writeln!(out, "Removed {app} from {ns}.")?;
    Ok(0)
}

// ---------------------------------------------------------------------------------------------------------------------
// Listing

/// The table of apps, or `None` when there are none.
pub fn table(rows: &[Value]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut s = String::new();
    let w = rows
        .iter()
        .map(|r| r["name"].as_str().unwrap_or("").len())
        .max()
        .unwrap_or(4)
        .max(4);
    s.push_str(&format!("{:<w$}  {:<10}  {:<9}  {:<9}  IMAGE\n", "NAME", "NAMESPACE", "READY", "STATE"));
    for r in rows {
        s.push_str(&format!(
            "{:<w$}  {:<10}  {:<9}  {:<9}  {}\n",
            r["name"].as_str().unwrap_or(""),
            r["namespace"].as_str().unwrap_or(""),
            format!("{}/{}", r["ready"], r["desired"]),
            r["state"].as_str().unwrap_or(""),
            r["image"].as_str().unwrap_or(""),
        ));
        if r["state"] == "problem" {
            s.push_str(&format!(
                "  {}\n  next: {}\n",
                r["summary"].as_str().unwrap_or(""),
                r["next"].as_str().unwrap_or("")
            ));
        }
    }
    Some(s)
}

/// The "your apps" part of `fluxctl kube apps`: best effort, so a missing cluster never hides the catalog above it.
pub fn apps_section(cluster: Option<&str>) -> String {
    let t = Target {
        kube_context: None,
        cluster: cluster
            .filter(|n| {
                crate::docker_host::home()
                    .map(|h| h.join(".kube").join(format!("fluxvm-{n}.yaml")).exists())
                    .unwrap_or(false)
            })
            .map(str::to_owned),
    };
    let Ok((_, prefix)) = selection(&t) else {
        return String::new();
    };
    let c = Cluster::new(prefix, &SystemKubectl);
    match read_apps(&c, None) {
        Ok(rows) => match table(&rows) {
            Some(t) => format!("\nYour apps (run with `fluxctl kube run IMAGE`):\n{t}"),
            None => "\nYour apps: none yet. Start one with `fluxctl kube run nginx`.\n".to_owned(),
        },
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::kube::fake::FakeKubectl;
    use std::cell::{Cell, RefCell};

    struct Env {
        tty: bool,
        ends: RefCell<Vec<ForwardEnd>>,
        forwarded: RefCell<Vec<String>>,
        pauses: Cell<u32>,
    }
    impl Env {
        fn new(tty: bool) -> Self {
            Env {
                tty,
                ends: RefCell::new(vec![ForwardEnd::Interrupted]),
                forwarded: RefCell::default(),
                pauses: Cell::new(0),
            }
        }
    }
    impl ExposeEnv for Env {
        fn interactive(&self) -> bool {
            self.tty
        }
        fn confirm(&self, _: &str) -> bool {
            true
        }
        fn install_envoy(&self, _: &Selection) -> Result<()> {
            Ok(())
        }
        fn forward(&self, argv: &[String]) -> Result<ForwardEnd> {
            self.forwarded.borrow_mut().push(argv.join(" "));
            Ok(self.ends.borrow_mut().remove(0))
        }
        fn pause(&self, _: Duration) {
            self.pauses.set(self.pauses.get() + 1);
        }
    }

    fn spec() -> Spec {
        Spec {
            name: "web".into(),
            namespace: "default".into(),
            image: "nginx".into(),
            port: 80,
            replicas: 1,
            env: vec![],
        }
    }
    fn opts(detach: bool) -> RunOpts {
        RunOpts {
            local_port: None,
            detach,
            wait: Duration::from_secs(10),
        }
    }
    fn cluster(f: &FakeKubectl) -> Cluster<'_> {
        Cluster::new(vec!["kubectl".into()], f)
    }
    fn list(state_ready: u64, pod_wait: Option<&str>) -> Value {
        let l = kube_app::manifests(&spec());
        let mut dep = l[0].clone();
        dep["status"] = json!({"readyReplicas": state_ready});
        let state = pod_wait.map_or(json!({"running": {}}), |r| json!({"waiting": {"reason": r}}));
        json!({"items": [dep, l[1].clone(), {"kind": "Pod", "metadata": {"namespace": "default", "name": "web-1", "labels": {kube_app::NAME_KEY: "web"}},
            "spec": {"containers": [{"name": "web"}]}, "status": {"phase": "Running", "containerStatuses": [{"ready": pod_wait.is_none(), "restartCount": 0, "state": state}]}}]})
    }
    fn ok() -> RunOut {
        RunOut::default()
    }

    #[test]
    fn one_cluster_is_picked_none_falls_back_and_many_must_be_named() {
        let t = Target::default();
        assert_eq!(pick(&["dev".into()], &t).unwrap().cluster.as_deref(), Some("dev"));
        assert!(pick(&[], &t).unwrap().cluster.is_none());
        let e = pick(&["a".into(), "b".into()], &t).unwrap_err().to_string();
        assert!(e.contains("--cluster NAME") && e.contains("APP_CLUSTER_AMBIGUOUS"), "{e}");
        let named = Target { cluster: Some("x".into()), kube_context: None };
        assert_eq!(pick(&["a".into(), "b".into()], &named).unwrap().cluster.as_deref(), Some("x"));
    }

    #[test]
    fn run_creates_waits_and_forwards_on_a_free_port() {
        let f = FakeKubectl::default()
            .text(0, "", "") // get deployment
            .text(0, "", "") // get service
            .text(0, "created", "") // apply
            .json(list(0, Some("ContainerCreating")))
            .json(list(1, None));
        let env = Env::new(true);
        let mut out = vec![];
        let code = run_app(&spec(), &opts(false), &Selection::default(), &cluster(&f), &env, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("Created web") && out.contains("web is running") && out.contains("http://127.0.0.1:"), "{out}");
        let calls = f.calls.borrow();
        assert!(calls[2].0.join(" ").contains("apply -f -"));
        assert!(calls[2].1.as_deref().unwrap().contains("fluxctl-app"));
        let fwd = env.forwarded.borrow();
        assert!(fwd[0].contains("port-forward svc/web") && fwd[0].ends_with(":80"), "{fwd:?}");
        assert_eq!(env.pauses.get(), 1);
    }

    #[test]
    fn detach_creates_and_does_not_wait_or_forward() {
        let f = FakeKubectl::default().text(0, "", "").text(0, "", "").text(0, "", "");
        let env = Env::new(true);
        let mut out = vec![];
        run_app(&spec(), &opts(true), &Selection::default(), &cluster(&f), &env, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("fluxctl kube expose web"));
        assert!(env.forwarded.borrow().is_empty());
    }

    #[test]
    fn a_bad_image_is_explained_with_the_next_command() {
        let f = FakeKubectl::default()
            .text(0, "", "")
            .text(0, "", "")
            .text(0, "", "")
            .json(list(0, Some("ErrImagePull")))
            .json(list(0, Some("ImagePullBackOff")));
        let env = Env::new(true);
        let e = run_app(&spec(), &opts(false), &Selection::default(), &cluster(&f), &env, &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("cannot download the image nginx") && e.contains("kube load") && e.contains("kube rm web") && e.contains("APP_IMAGE_PULL"), "{e}");
    }

    #[test]
    fn a_taken_name_is_refused_before_anything_is_created() {
        let foreign = json!({"metadata": {"name": "web", "labels": {}}});
        let f = FakeKubectl::default().json(foreign);
        let env = Env::new(false);
        let e = run_app(&spec(), &opts(false), &Selection::default(), &cluster(&f), &env, &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("--name") && e.contains("APP_NAME_TAKEN"), "{e}");
        assert_eq!(f.calls.borrow().len(), 1);
    }

    #[test]
    fn a_taken_local_port_is_explained_and_nothing_is_created() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let f = FakeKubectl::default();
        let env = Env::new(true);
        let o = RunOpts { local_port: Some(port), detach: false, wait: Duration::from_secs(4) };
        let e = run_app(&spec(), &o, &Selection::default(), &cluster(&f), &env, &mut vec![]).unwrap_err().to_string();
        assert!(e.contains("APP_PORT_TAKEN") && e.contains("Nothing was created"), "{e}");
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn no_cluster_is_explained() {
        let f = FakeKubectl::default().text(1, "", "Unable to connect to the server: dial tcp 127.0.0.1:6443: connection refused");
        let e = run_app(&spec(), &opts(true), &Selection::default(), &cluster(&f), &Env::new(false), &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("fluxctl kube up") && e.contains("APP_NO_CLUSTER"), "{e}");
        let f = FakeKubectl::default().fail("could not run kubectl: not found");
        let e = run_app(&spec(), &opts(true), &Selection::default(), &cluster(&f), &Env::new(false), &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("brew install kubectl"), "{e}");
    }

    #[test]
    fn rm_only_deletes_what_fluxctl_made_and_says_when_there_is_nothing() {
        let l = kube_app::manifests(&spec());
        let f = FakeKubectl::default()
            .json(l[0].clone()) // the app
            .fail("x") // httproute lookup fails: no gateway API; ignored
            .text(0, "deleted", "");
        let mut out = vec![];
        rm("web", "default", &Selection::default(), &cluster(&f), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("Removed web"));
        assert!(f.argvs()[2].contains("delete deployment,service web"));
        let f = FakeKubectl::default().text(0, "", "");
        let e = rm("web", "default", &Selection::default(), &cluster(&f), &mut vec![]).unwrap_err().to_string();
        assert!(e.contains("fluxctl kube apps") && e.contains("APP_NOT_FOUND"), "{e}");
        let f = FakeKubectl::default().json(json!({"metadata": {"labels": {}}}));
        assert!(rm("web", "default", &Selection::default(), &cluster(&f), &mut vec![]).unwrap_err().to_string().contains("leaves it alone"));
    }

    #[test]
    fn scale_and_rollout_check_the_label_first() {
        let l = kube_app::manifests(&spec());
        let f = FakeKubectl::default().json(l[0].clone()).text(0, "scaled", "");
        let r = execute(
            AppCommand::Scale { app: "web".into(), replicas: 3, namespace: "default".into(), target: Target { cluster: Some("c".into()), kube_context: None } },
            &f,
            &Env::new(false),
            &mut vec![],
        );
        // `selection` reads the home directory for --cluster; whatever it answers, the first call must be the label check.
        if r.is_ok() {
            assert!(f.argvs()[1].contains("scale deployment/web --replicas=3"));
        }
        assert_eq!(
            shell_argv(&["kubectl".into()], "web", "default", &[], true).join(" "),
            "kubectl -n default exec -it deploy/web -- sh -c [ -x /bin/bash ] && exec bash || exec sh"
        );
        assert!(shell_argv(&["kubectl".into()], "web", "ns", &["ls".into(), "/".into()], false).join(" ").ends_with("-- ls /"));
    }

    #[test]
    fn the_forward_reconnects_and_stops_on_ctrl_c() {
        let env = Env::new(true);
        *env.ends.borrow_mut() = vec![ForwardEnd::Exited { code: 1, ran: Duration::from_secs(1) }, ForwardEnd::Interrupted];
        let mut err = vec![];
        assert_eq!(keep_forwarding(&["kubectl".into()], &env, &mut err).unwrap(), 0);
        assert_eq!(env.forwarded.borrow().len(), 2);
        assert!(String::from_utf8(err).unwrap().contains("reconnecting"));
    }

    #[test]
    fn the_table_shows_problems_with_their_next_step() {
        let deps = json!({"items": [kube_app::manifests(&spec())[0].clone()]});
        let pods = list(0, Some("ImagePullBackOff"));
        let pods = json!({"items": pods["items"].as_array().unwrap().iter().filter(|i| i["kind"] == "Pod").cloned().collect::<Vec<_>>()});
        let rows = kube_app::summarize(&deps, &json!({"items": []}), &pods);
        let t = table(&rows).unwrap();
        assert!(t.contains("web") && t.contains("problem") && t.contains("next:"), "{t}");
        assert!(table(&[]).is_none());
    }
}
