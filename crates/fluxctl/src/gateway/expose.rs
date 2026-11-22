// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl gateway expose | unexpose | list`: one command from a Kubernetes Service to a URL on this machine.
//!
//! `expose SERVICE` makes sure Envoy Gateway is on the cluster (asking one yes/no question, or `--yes`), creates a GatewayClass
//! if the cluster has none for Envoy, then a Gateway, an EnvoyProxy (ClusterIP) and an HTTPRoute for the Service, waits for the
//! Gateway to be Programmed, picks a free local port with a real bind and runs `kubectl port-forward` until Ctrl-C, reconnecting
//! when the proxy pod is replaced. Everything it creates carries the label `app.kubernetes.io/managed-by=fluxctl-gateway`;
//! `unexpose` and `list` only ever look at, or delete, objects with that label.
//!
//! Every kubectl call goes through [`KubeRun`] and everything that touches the terminal or a long-running process goes through
//! [`ExposeEnv`], so the tests use fakes and loopback sockets.

use super::demo::selection_flags;
use super::kube::{Cluster, RunOut, Selection, label, shell_join};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "fluxctl-gateway";
const SERVICE_KEY: &str = "fluxvm.dev/service";
const HOST_KEY: &str = "fluxvm.dev/host";
const PORT_KEY: &str = "fluxvm.dev/service-port";
const PROXY_NS: &str = "envoy-gateway-system";
const EG_CONTROLLER: &str = "gateway.envoyproxy.io/gatewayclass-controller";
const OUR_CLASS: &str = "fluxvm-envoy";
const LISTENER: &str = "http";
const GATEWAY_KIND: &str = "gateway.gateway.networking.k8s.io";
const ROUTE_KIND: &str = "httproute.gateway.networking.k8s.io";
const PROXY_KIND: &str = "envoyproxy.gateway.envoyproxy.io";
const FIRST_PORT: u16 = 8080;
const PORT_SPAN: u16 = 200;
/// Consecutive short-lived forwards (or failed Service lookups) before `publish`/`expose` give up.
const MAX_FAILS: u32 = 12;
/// A forward that ran this long counts as having worked, so the backoff starts over.
const HEALTHY: Duration = Duration::from_secs(10);

/// How a `kubectl port-forward` ended.
#[derive(Debug, Clone, PartialEq)]
pub enum ForwardEnd {
    /// Ctrl-C or SIGTERM: the user stopped it.
    Interrupted,
    Exited { code: i32, ran: Duration },
}

/// What touches the terminal and long-running processes.
pub trait ExposeEnv {
    /// Is there a person to ask (stdin and stderr are terminals)?
    fn interactive(&self) -> bool;
    /// One yes/no question; the default is no.
    fn confirm(&self, question: &str) -> bool;
    /// Installs the pinned Envoy Gateway add-on on the selected cluster.
    fn install_envoy(&self, sel: &Selection) -> Result<()>;
    /// Runs the command (a port-forward) until it exits.
    fn forward(&self, argv: &[String]) -> Result<ForwardEnd>;
    fn pause(&self, d: Duration);
}

/// The real terminal and processes.
pub struct SystemEnv;

impl ExposeEnv for SystemEnv {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn confirm(&self, question: &str) -> bool {
        eprint!("{question} [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }

    fn install_envoy(&self, sel: &Selection) -> Result<()> {
        let Some(name) = &sel.cluster else {
            bail!(
                "fluxctl installs Envoy Gateway only on a FluxVM cluster it created. For this cluster run `helm install eg oci://docker.io/envoyproxy/gateway-helm --version v1.10.0 -n envoy-gateway-system --create-namespace` and then run this command again, or name a FluxVM cluster with --cluster NAME"
            );
        };
        let exe = std::env::current_exe()?;
        let st = std::process::Command::new(exe)
            .args(["kube", "apple", "platform", "envoy-gateway", name])
            .status()
            .map_err(|e| anyhow!("could not start the installer: {e}"))?;
        if !st.success() {
            bail!(
                "installing Envoy Gateway failed (see the output above). You can run the installer yourself: fluxctl kube apple platform envoy-gateway {name}"
            );
        }
        Ok(())
    }

    fn forward(&self, argv: &[String]) -> Result<ForwardEnd> {
        use std::os::unix::process::ExitStatusExt;
        let (program, args) = argv.split_first().ok_or_else(|| anyhow!("empty command"))?;
        let started = Instant::now();
        let st = std::process::Command::new(program)
            .args(args)
            .status()
            .map_err(|e| anyhow!("could not run {program}: {e}"))?;
        if matches!(st.signal(), Some(2 | 15)) {
            return Ok(ForwardEnd::Interrupted);
        }
        Ok(ForwardEnd::Exited {
            code: st.code().unwrap_or(-1),
            ran: started.elapsed(),
        })
    }

    fn pause(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Ports

/// Can this process bind `127.0.0.1:port` right now? A real bind, never an assumption.
pub fn port_is_free(port: u16) -> bool {
    port != 0 && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The first free local port from `start` up (at most [`PORT_SPAN`] candidates).
pub fn free_port(start: u16) -> Option<u16> {
    let end = start.saturating_add(PORT_SPAN);
    (start..=end).find(|p| port_is_free(*p))
}

/// The local port to use: the one asked for (it must be free) or the first free one from 8080.
pub fn choose_port(requested: Option<u16>) -> Result<u16> {
    match requested {
        Some(p) => {
            if port_is_free(p) {
                Ok(p)
            } else {
                bail!(
                    "port {p} is already in use on this machine. Leave out the port option to let fluxctl pick a free one, or choose another with --port or --local-port N"
                )
            }
        }
        None => free_port(FIRST_PORT).ok_or_else(|| {
            anyhow!(
                "no free port between {FIRST_PORT} and {}. Free one up, or choose one with --port or --local-port N",
                FIRST_PORT.saturating_add(PORT_SPAN)
            )
        }),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The forward loop

/// How long to wait before reconnect number `fails` (1, 2, 4, 8, then 15 seconds).
pub fn backoff(fails: u32) -> Duration {
    let secs = 1u64 << fails.saturating_sub(1).min(4);
    Duration::from_secs(secs.min(15))
}

/// Runs the port-forward until the user stops it. When kubectl exits (usually because the Envoy proxy pod was replaced) it
/// says so on `err`, waits with a backoff, finds the Service again with `find` and reconnects. Gives up after [`MAX_FAILS`]
/// short-lived attempts in a row.
pub fn forward_loop(
    first: Vec<String>,
    find: &mut dyn FnMut() -> Result<Vec<String>>,
    env: &dyn ExposeEnv,
    err: &mut dyn Write,
) -> Result<i32> {
    let mut cmd = first;
    let mut fails = 0u32;
    loop {
        match env.forward(&cmd)? {
            ForwardEnd::Interrupted => return Ok(0),
            ForwardEnd::Exited { code, ran } => {
                fails = if ran >= HEALTHY { 1 } else { fails + 1 };
                if fails >= MAX_FAILS {
                    bail!(
                        "the port-forward keeps ending (kubectl exited {code}). Check the proxy with `kubectl get pods -n {PROXY_NS}` and the cluster with `kubectl get nodes`, then run this command again"
                    );
                }
                let wait = backoff(fails);
                writeln!(
                    err,
                    "fluxctl gateway: the port-forward ended (kubectl exited {code}), probably because the Envoy proxy pod was replaced; finding the Service again and reconnecting in {}s (Ctrl-C to stop)",
                    wait.as_secs()
                )?;
                env.pause(wait);
                loop {
                    match find() {
                        Ok(c) => {
                            cmd = c;
                            break;
                        }
                        Err(e) => {
                            fails += 1;
                            if fails >= MAX_FAILS {
                                bail!(
                                    "could not find the Envoy Service again: {e:#}. Check the Gateway with `kubectl get gateway -A`, then run this command again"
                                );
                            }
                            let wait = backoff(fails);
                            writeln!(
                                err,
                                "fluxctl gateway: the Envoy Service is not there yet ({e:#}); trying again in {}s",
                                wait.as_secs()
                            )?;
                            env.pause(wait);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// kubectl helpers with plain-English failures

fn runner_err(e: &str) -> anyhow::Error {
    if e.contains("could not run kubectl") {
        anyhow!(
            "kubectl was not found on this machine. Install it (macOS: `brew install kubectl`; Linux: your package manager, or https://kubernetes.io/docs/tasks/tools/) and run this command again"
        )
    } else if e.contains("did not finish within") {
        anyhow!(
            "{e}. The cluster may be stopped or unreachable; check it with `kubectl get nodes`, or name the cluster with --cluster NAME or --kube-context CONTEXT"
        )
    } else {
        anyhow!("{e}")
    }
}

fn stderr_err(what: &str, stderr: &str) -> anyhow::Error {
    let low = stderr.to_ascii_lowercase();
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
    ];
    if unreachable.iter().any(|m| low.contains(m)) {
        let first = stderr.lines().next().unwrap_or("").trim();
        anyhow!(
            "cannot reach a Kubernetes cluster ({first}). Start one with `fluxctl kube apple create NAME`, or pick an existing one with --cluster NAME or --kube-context CONTEXT (`kubectl config get-contexts` lists them)"
        )
    } else if stderr.trim().is_empty() {
        anyhow!("{what} failed")
    } else {
        anyhow!("{what} failed: {}", stderr.trim())
    }
}

fn run(c: &Cluster, rest: &[&str], stdin: Option<&[u8]>, secs: u64) -> Result<RunOut> {
    let argv: Vec<String> = c
        .cmd
        .iter()
        .cloned()
        .chain(rest.iter().map(|s| (*s).to_owned()))
        .collect();
    c.runner
        .run(&argv, stdin, Duration::from_secs(secs))
        .map_err(|e| runner_err(&e))
}

/// `kubectl [-n NS] get RESOURCE [-A] [-l SELECTOR] -o json --ignore-not-found`; `None` when nothing is there (or the
/// resource type is not installed on the cluster).
fn read(
    c: &Cluster,
    ns: Option<&str>,
    all: bool,
    resource: &str,
    selector: Option<&str>,
) -> Result<Option<Value>> {
    let mut rest: Vec<&str> = vec![];
    if let Some(ns) = ns {
        rest.extend(["-n", ns]);
    }
    rest.extend(["get", resource]);
    if all {
        rest.push("-A");
    }
    if let Some(s) = selector {
        rest.extend(["-l", s]);
    }
    rest.extend(["-o", "json", "--ignore-not-found", "--request-timeout=10s"]);
    let out = run(c, &rest, None, 20)?;
    if out.code != 0 {
        if out.stderr.contains("doesn't have a resource type")
            || out.stderr.contains("no matches for kind")
        {
            return Ok(None);
        }
        return Err(stderr_err(&format!("kubectl get {resource}"), &out.stderr));
    }
    if out.stdout.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&out.stdout)
        .map(Some)
        .map_err(|e| anyhow!("kubectl get {resource}: not JSON: {e}"))
}

fn has_our_label(obj: &Value) -> bool {
    obj["metadata"]["labels"][MANAGED_BY_KEY].as_str() == Some(MANAGED_BY)
}

// ---------------------------------------------------------------------------------------------------------------------
// Names and manifests

/// The name of the Gateway, HTTPRoute and EnvoyProxy made for a Service.
pub fn object_name(service: &str) -> Result<String> {
    label(service).map_err(|e| anyhow!("service name: {e}"))?;
    let name = format!("fx-{service}");
    if name.len() > 63 {
        bail!("the Service name {service:?} is too long to expose (60 characters at most)");
    }
    Ok(name)
}

fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.split('.').all(|l| label(l).is_ok())
}

fn meta(name: &str, ns: Option<&str>, service: Option<(&str, &str, i64)>) -> Value {
    let mut labels = json!({ MANAGED_BY_KEY: MANAGED_BY });
    let mut m = json!({"name": name});
    if let Some(ns) = ns {
        m["namespace"] = json!(ns);
    }
    if let Some((svc, host, port)) = service {
        labels[SERVICE_KEY] = json!(svc);
        m["annotations"] = json!({ HOST_KEY: host, PORT_KEY: port.to_string() });
    }
    m["labels"] = labels;
    m
}

/// The GatewayClass fluxctl makes when the cluster has none for Envoy Gateway.
pub fn class_manifest() -> Value {
    json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "GatewayClass",
        "metadata": meta(OUR_CLASS, None, None),
        "spec": {"controllerName": EG_CONTROLLER}})
}

/// The EnvoyProxy (ClusterIP Service, as in the demo), the Gateway and the HTTPRoute for one Service.
pub fn manifests(
    ns: &str,
    name: &str,
    service: &str,
    port: i64,
    host: &str,
    class: &str,
) -> Vec<Value> {
    let m = |n: &str| meta(n, Some(ns), Some((service, host, port)));
    vec![
        json!({"apiVersion": "gateway.envoyproxy.io/v1alpha1", "kind": "EnvoyProxy", "metadata": m(name),
            "spec": {"provider": {"type": "Kubernetes", "kubernetes": {"envoyService": {"type": "ClusterIP"}}}}}),
        json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway", "metadata": m(name),
            "spec": {"gatewayClassName": class,
                "infrastructure": {"parametersRef": {"group": "gateway.envoyproxy.io", "kind": "EnvoyProxy", "name": name}},
                "listeners": [{"name": LISTENER, "protocol": "HTTP", "port": 80, "hostname": host,
                    "allowedRoutes": {"namespaces": {"from": "Same"}}}]}}),
        json!({"apiVersion": "gateway.networking.k8s.io/v1", "kind": "HTTPRoute", "metadata": m(name),
            "spec": {"parentRefs": [{"name": name}], "hostnames": [host],
                "rules": [{"backendRefs": [{"name": service, "port": port}]}]}}),
    ]
}

fn yaml_docs(items: &[Value]) -> String {
    items
        .iter()
        .map(|v| serde_yaml::to_string(v).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("---\n")
}

// ---------------------------------------------------------------------------------------------------------------------
// expose

#[derive(Debug, Clone)]
pub struct ExposeOpts {
    pub service: String,
    pub namespace: String,
    pub port: Option<i64>,
    pub host: Option<String>,
    pub local_port: Option<u16>,
    pub yes: bool,
    pub dry_run: bool,
}

impl ExposeOpts {
    fn host(&self) -> String {
        self.host
            .clone()
            .unwrap_or_else(|| format!("{}.localhost", self.service))
    }

    /// The command that repeats this one with `--yes`.
    fn rerun_with_yes(&self, sel: &Selection) -> String {
        let mut s = format!("fluxctl gateway expose {}", self.service);
        if self.namespace != "default" {
            s.push_str(&format!(" -n {}", self.namespace));
        }
        if let Some(p) = self.port {
            s.push_str(&format!(" --port {p}"));
        }
        if let Some(h) = &self.host {
            s.push_str(&format!(" --host {h}"));
        }
        s.push_str(&selection_flags(sel));
        s.push_str(" --yes");
        s
    }
}

fn ensure_envoy(
    o: &ExposeOpts,
    sel: &Selection,
    c: &Cluster,
    env: &dyn ExposeEnv,
    out: &mut dyn Write,
) -> Result<()> {
    let present =
        |c: &Cluster| read(c, Some(PROXY_NS), false, "deployment/envoy-gateway", None);
    if present(c)?.is_some() {
        return Ok(());
    }
    let how = if sel.cluster.is_some() {
        format!("Run it again with --yes to install it first: {}", o.rerun_with_yes(sel))
    } else {
        format!(
            "Install it with `helm install eg oci://docker.io/envoyproxy/gateway-helm --version v1.10.0 -n {PROXY_NS} --create-namespace`, or run this on a FluxVM cluster (`{}`) and it is installed for you",
            o.rerun_with_yes(&Selection { cluster: Some("NAME".into()), ..sel.clone() })
        )
    };
    if !o.yes {
        if !env.interactive() {
            bail!("Envoy Gateway is not installed on this cluster, and there is no terminal to ask you. {how}");
        }
        if !env.confirm(
            "Envoy Gateway (the Gateway API controller) is not installed on this cluster. Install it now?",
        ) {
            bail!("Envoy Gateway is not installed, so there is nothing to route the traffic. {how}");
        }
    }
    writeln!(out, "Installing Envoy Gateway ...")?;
    env.install_envoy(sel)?;
    if present(c)?.is_none() {
        bail!(
            "Envoy Gateway still is not there after the install. Check `kubectl -n {PROXY_NS} get pods` and run this command again"
        );
    }
    Ok(())
}

/// The first GatewayClass on the cluster that Envoy Gateway serves, if any.
fn find_class(c: &Cluster) -> Result<Option<String>> {
    let Some(list) = read(c, None, false, "gatewayclass.gateway.networking.k8s.io", None)? else {
        return Ok(None);
    };
    Ok(list["items"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .find(|i| i["spec"]["controllerName"].as_str() == Some(EG_CONTROLLER))
        .and_then(|i| i["metadata"]["name"].as_str().map(str::to_owned)))
}

fn service_port(svc: &Value, requested: Option<i64>, o: &ExposeOpts) -> Result<i64> {
    let ports: Vec<i64> = svc["spec"]["ports"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|p| p["port"].as_i64())
        .collect();
    let list = ports.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
    match (requested, ports.as_slice()) {
        (Some(p), _) if ports.contains(&p) => Ok(p),
        (Some(p), _) => bail!(
            "Service {} has no port {p} (it has: {list}). Pick one with --port",
            o.service
        ),
        (None, [only]) => Ok(*only),
        (None, []) => bail!("Service {} has no ports to expose", o.service),
        (None, _) => bail!(
            "Service {} has several ports ({list}). Say which one with --port N",
            o.service
        ),
    }
}

/// `fluxctl gateway expose`. Returns the exit code.
pub fn expose(
    o: &ExposeOpts,
    sel: &Selection,
    c: &Cluster,
    env: &dyn ExposeEnv,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32> {
    label(&o.namespace).map_err(|e| anyhow!("namespace: {e}"))?;
    let name = object_name(&o.service)?;
    let host = o.host();
    if !valid_host(&host) {
        bail!("--host {host:?} is not a host name (lowercase letters, digits, - and dots; for example web.localhost)");
    }
    let local = choose_port(o.local_port)?;
    let ns = o.namespace.as_str();

    if o.dry_run {
        let port = o.port.unwrap_or(80);
        writeln!(out, "# dry run: nothing is read or changed. The GatewayClass is made only if the cluster has none for Envoy Gateway; the Service port is read from the Service when --port is left out.")?;
        writeln!(out, "{}", yaml_docs(&[class_manifest()]).trim_end())?;
        let docs = manifests(ns, &name, &o.service, port, &host, OUR_CLASS);
        writeln!(out, "---\n{}", yaml_docs(&docs).trim_end())?;
        let mut apply = c.cmd.clone();
        apply.extend(["apply".into(), "-f".into(), "-".into()]);
        writeln!(out, "# then: {}", shell_join(&apply))?;
        let gw_ref = format!("gateway/{name}");
        let mut w = c.cmd.clone();
        w.extend(
            ["-n", ns, "wait", gw_ref.as_str(), "--for=condition=Programmed", "--timeout=120s"]
                .iter()
                .map(|s| (*s).to_owned()),
        );
        writeln!(out, "# then: {}", shell_join(&w))?;
        writeln!(out, "# then: kubectl -n {PROXY_NS} port-forward --address 127.0.0.1 service/<the Envoy Service> {local}:80")?;
        writeln!(out, "# URL: http://{host}:{local}/")?;
        return Ok(0);
    }

    ensure_envoy(o, sel, c, env, out)?;

    let flags = selection_flags(sel);
    let ns_flag = if ns == "default" { String::new() } else { format!(" -n {ns}") };
    let Some(svc) = read(c, Some(ns), false, &format!("service/{}", o.service), None)? else {
        bail!(
            "Service {} was not found in namespace {ns}. See what is there with `kubectl -n {ns} get services`, or pass -n NAMESPACE",
            o.service
        );
    };
    let port = service_port(&svc, o.port, o)?;

    let (class, make_class) = match find_class(c)? {
        Some(n) => (n, false),
        None => (OUR_CLASS.to_owned(), true),
    };

    for (kind, label_name) in [(ROUTE_KIND, "HTTPRoute"), (GATEWAY_KIND, "Gateway"), (PROXY_KIND, "EnvoyProxy")] {
        if let Some(existing) = read(c, Some(ns), false, &format!("{kind}/{name}"), None)?
            && !has_our_label(&existing)
        {
            bail!(
                "a {label_name} named {name} already exists in {ns} and fluxctl did not make it, so it will not be overwritten. Rename or delete it (`kubectl -n {ns} delete {kind} {name}`) and run this command again"
            );
        }
    }

    let mut items = vec![];
    if make_class {
        items.push(class_manifest());
    }
    items.extend(manifests(ns, &name, &o.service, port, &host, &class));
    let list = json!({"apiVersion": "v1", "kind": "List", "items": items});
    writeln!(out, "Creating the Gateway and HTTPRoute for {} ...", o.service)?;
    let applied = run(c, &["apply", "-f", "-"], Some(list.to_string().as_bytes()), 60)?;
    if applied.code != 0 {
        return Err(stderr_err("kubectl apply", &applied.stderr));
    }
    writeln!(out, "Waiting for the Gateway to be ready (this starts an Envoy proxy; up to two minutes) ...")?;
    let gw_ref = format!("gateway/{name}");
    let waited = run(
        c,
        &["-n", ns, "wait", gw_ref.as_str(), "--for=condition=Programmed", "--timeout=120s"],
        None,
        140,
    )?;
    if waited.code != 0 {
        bail!(
            "the Gateway did not become ready: {}. See why with `kubectl -n {ns} describe gateway {name}` and `kubectl -n {PROXY_NS} get pods`. Undo with `fluxctl gateway unexpose {}{ns_flag}{flags}`",
            waited.stderr.trim(),
            o.service
        );
    }

    let find = |local: u16| c.publication(ns, &name, LISTENER, PROXY_NS, i64::from(local));
    let first = find(local)?;
    writeln!(out, "{} is exposed.", o.service)?;
    writeln!(out, "  URL:   http://{host}:{local}/")?;
    writeln!(out, "  If your browser does not open {host}, use: curl --resolve {host}:{local}:127.0.0.1 http://{host}:{local}/")?;
    writeln!(out, "  (*.localhost names resolve to this machine in most browsers; fluxctl has not checked yours.)")?;
    writeln!(out, "Forwarding until you press Ctrl-C. Stop sharing it with: fluxctl gateway unexpose {}{ns_flag}{flags}", o.service)?;
    out.flush()?;
    forward_loop(first, &mut || find(local), env, err)
}

// ---------------------------------------------------------------------------------------------------------------------
// unexpose

/// `fluxctl gateway unexpose`: deletes the HTTPRoute, Gateway and EnvoyProxy that expose made for the Service, and only
/// those that carry fluxctl's label. The shared GatewayClass and Envoy Gateway itself stay.
pub fn unexpose(
    service: &str,
    ns: &str,
    sel: &Selection,
    c: &Cluster,
    out: &mut dyn Write,
) -> Result<i32> {
    label(ns).map_err(|e| anyhow!("namespace: {e}"))?;
    let name = object_name(service)?;
    let mut deleted = 0;
    let mut foreign = vec![];
    for (kind, what) in [(ROUTE_KIND, "HTTPRoute"), (GATEWAY_KIND, "Gateway"), (PROXY_KIND, "EnvoyProxy")] {
        let target = format!("{kind}/{name}");
        let Some(obj) = read(c, Some(ns), false, &target, None)? else {
            continue;
        };
        if !has_our_label(&obj) {
            foreign.push(what);
            continue;
        }
        let r = run(c, &["-n", ns, "delete", &target, "--ignore-not-found", "--timeout=60s"], None, 90)?;
        if r.code != 0 {
            return Err(stderr_err(&format!("kubectl delete {target}"), &r.stderr));
        }
        writeln!(out, "Deleted {what} {name} in {ns}.")?;
        deleted += 1;
    }
    for what in &foreign {
        writeln!(out, "Left the {what} {name} alone: fluxctl gateway did not make it.")?;
    }
    if deleted == 0 {
        writeln!(
            out,
            "Nothing made by fluxctl gateway is exposing {service} in {ns}. See what is exposed with: fluxctl gateway list{}",
            selection_flags(sel)
        )?;
    }
    Ok(0)
}

// ---------------------------------------------------------------------------------------------------------------------
// list

fn condition(obj: &Value, ty: &str) -> Vec<(bool, String)> {
    let mut found = vec![];
    let mut scan = |conds: &Value| {
        for c in conds.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if c["type"].as_str() == Some(ty) {
                found.push((
                    c["status"].as_str() == Some("True"),
                    c["reason"].as_str().unwrap_or("").to_owned(),
                ));
            }
        }
    };
    scan(&obj["status"]["conditions"]);
    for p in obj["status"]["parents"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        scan(&p["conditions"]);
    }
    found
}

fn words(conds: Vec<(bool, String)>, yes: &str, no: &str) -> String {
    if let Some((_, reason)) = conds.iter().find(|(ok, _)| !ok) {
        if reason.is_empty() {
            no.to_owned()
        } else {
            format!("{no} ({reason})")
        }
    } else if conds.is_empty() {
        "waiting for Envoy Gateway".to_owned()
    } else {
        yes.to_owned()
    }
}

#[derive(Default)]
struct Row {
    host: String,
    route: String,
    gateway: String,
}

/// `fluxctl gateway list`: what fluxctl exposed, in plain words.
pub fn list(sel: &Selection, c: &Cluster, out: &mut dyn Write) -> Result<i32> {
    let selector = format!("{MANAGED_BY_KEY}={MANAGED_BY}");
    let resources = format!("{GATEWAY_KIND},{ROUTE_KIND}");
    let items = read(c, None, true, &resources, Some(&selector))?;
    let mut rows: BTreeMap<(String, String), Row> = BTreeMap::new();
    for i in items
        .as_ref()
        .and_then(|v| v["items"].as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        if !has_our_label(i) {
            continue;
        }
        let ns = i["metadata"]["namespace"].as_str().unwrap_or("").to_owned();
        let Some(svc) = i["metadata"]["labels"][SERVICE_KEY].as_str() else {
            continue;
        };
        let row = rows.entry((ns, svc.to_owned())).or_default();
        if let Some(h) = i["metadata"]["annotations"][HOST_KEY].as_str() {
            row.host = h.to_owned();
        }
        match i["kind"].as_str() {
            Some("Gateway") => row.gateway = words(condition(i, "Programmed"), "ready", "not ready"),
            Some("HTTPRoute") => row.route = words(condition(i, "Accepted"), "accepted", "rejected"),
            _ => {}
        }
    }
    if rows.is_empty() {
        writeln!(
            out,
            "Nothing is exposed by fluxctl gateway. Expose a Service with: fluxctl gateway expose SERVICE -n NAMESPACE{}",
            selection_flags(sel)
        )?;
        return Ok(0);
    }
    writeln!(out, "{:<24} {:<16} {:<28} {:<26} {}", "SERVICE", "NAMESPACE", "HOST", "ROUTE", "GATEWAY")?;
    for ((ns, svc), r) in &rows {
        let dash = |s: &str| if s.is_empty() { "-".to_owned() } else { s.to_owned() };
        writeln!(
            out,
            "{:<24} {:<16} {:<28} {:<26} {}",
            svc, ns, dash(&r.host), dash(&r.route), dash(&r.gateway)
        )?;
    }
    writeln!(
        out,
        "Open one on this machine with: fluxctl gateway expose SERVICE -n NAMESPACE{}   (it reuses what exists and prints the URL)",
        selection_flags(sel)
    )?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::kube::fake::FakeKubectl;
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct FakeEnv {
        tty: bool,
        answer: bool,
        installs: RefCell<u32>,
        forwards: RefCell<VecDeque<ForwardEnd>>,
        pauses: RefCell<Vec<Duration>>,
        forwarded: RefCell<Vec<String>>,
    }

    impl FakeEnv {
        fn new(tty: bool, answer: bool) -> Self {
            FakeEnv {
                tty,
                answer,
                installs: RefCell::new(0),
                forwards: RefCell::new(VecDeque::new()),
                pauses: RefCell::new(vec![]),
                forwarded: RefCell::new(vec![]),
            }
        }
        fn then(self, e: ForwardEnd) -> Self {
            self.forwards.borrow_mut().push_back(e);
            self
        }
    }

    impl ExposeEnv for FakeEnv {
        fn interactive(&self) -> bool {
            self.tty
        }
        fn confirm(&self, _: &str) -> bool {
            self.answer
        }
        fn install_envoy(&self, _: &Selection) -> Result<()> {
            *self.installs.borrow_mut() += 1;
            Ok(())
        }
        fn forward(&self, argv: &[String]) -> Result<ForwardEnd> {
            self.forwarded.borrow_mut().push(argv.join(" "));
            Ok(self.forwards.borrow_mut().pop_front().unwrap_or(ForwardEnd::Interrupted))
        }
        fn pause(&self, d: Duration) {
            self.pauses.borrow_mut().push(d);
        }
    }

    fn opts() -> ExposeOpts {
        ExposeOpts {
            service: "web".into(),
            namespace: "shop".into(),
            port: None,
            host: None,
            local_port: None,
            yes: false,
            dry_run: false,
        }
    }

    fn sel() -> Selection {
        Selection { kube_context: Some("lab".into()), cluster: None }
    }

    fn cluster<'a>(f: &'a FakeKubectl) -> Cluster<'a> {
        Cluster::new(sel().prefix(None).unwrap(), f)
    }

    fn labelled() -> Value {
        json!({"metadata": {"labels": {MANAGED_BY_KEY: MANAGED_BY}}})
    }

    fn empty(f: FakeKubectl) -> FakeKubectl {
        f.text(0, "", "")
    }

    #[test]
    fn free_port_skips_a_port_that_is_really_taken() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = l.local_addr().unwrap().port();
        assert!(!port_is_free(taken));
        let got = free_port(taken).expect("a free port above");
        assert_ne!(got, taken);
        assert!(got > taken);
        TcpListener::bind(("127.0.0.1", got)).expect("the chosen port can be bound");
        let e = choose_port(Some(taken)).unwrap_err().to_string();
        assert!(e.contains("already in use") && e.contains("--local-port"), "{e}");
        drop(l);
        assert_eq!(choose_port(Some(taken)).unwrap(), taken);
        assert!(!port_is_free(0));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let s: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(s, [1, 2, 4, 8, 15, 15, 15, 15]);
    }

    #[test]
    fn dry_run_prints_the_manifests_and_runs_nothing() {
        let f = FakeKubectl::default();
        let env = FakeEnv::new(false, false);
        let mut o = opts();
        o.dry_run = true;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = expose(&o, &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        for want in ["kind: Gateway", "kind: HTTPRoute", "kind: EnvoyProxy", "kind: GatewayClass", "web.localhost", "app.kubernetes.io/managed-by: fluxctl-gateway", "name: fx-web"] {
            assert!(text.contains(want), "{want} in {text}");
        }
        assert!(f.calls.borrow().is_empty(), "dry run never calls kubectl");
        assert!(env.forwarded.borrow().is_empty() && *env.installs.borrow() == 0);
    }

    #[test]
    fn without_a_terminal_and_without_yes_a_missing_envoy_fails_with_the_command_to_run() {
        let f = empty(FakeKubectl::default());
        let env = FakeEnv::new(false, true);
        let mut o = opts();
        let s = Selection { cluster: Some("dev".into()), ..Default::default() };
        let c = Cluster::new(s.prefix(Some(std::path::Path::new("/h"))).unwrap(), &f);
        o.port = Some(80);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = expose(&o, &s, &c, &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("fluxctl gateway expose web -n shop --port 80 --cluster dev --yes"), "{e}");
        assert_eq!(*env.installs.borrow(), 0);
        assert_eq!(f.calls.borrow().len(), 1, "only the install check ran");
    }

    #[test]
    fn declining_the_question_installs_nothing() {
        let f = empty(FakeKubectl::default());
        let env = FakeEnv::new(true, false);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("not installed") && e.contains("helm install"), "{e}");
        assert_eq!(*env.installs.borrow(), 0);
    }

    #[test]
    fn a_missing_kubectl_and_an_unreachable_cluster_are_explained() {
        let f = FakeKubectl::default().fail("could not run kubectl: No such file or directory");
        let env = FakeEnv::new(false, false);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("kubectl was not found") && e.contains("brew install kubectl"), "{e}");
        let f = FakeKubectl::default().text(1, "", "The connection to the server localhost:8080 was refused - did you specify the right host or port? connection refused\n");
        let e = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("cannot reach a Kubernetes cluster") && e.contains("--cluster NAME"), "{e}");
    }

    #[test]
    fn a_missing_service_names_the_next_command() {
        let f = FakeKubectl::default().json(json!({"metadata": {"name": "envoy-gateway"}})).text(0, "", "");
        let env = FakeEnv::new(false, false);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("Service web was not found in namespace shop") && e.contains("kubectl -n shop get services"), "{e}");
    }

    fn happy_fake() -> FakeKubectl {
        FakeKubectl::default()
            .json(json!({"metadata": {"name": "envoy-gateway"}}))
            .json(json!({"spec": {"ports": [{"port": 8000}]}}))
            .json(json!({"items": []}))
            .text(0, "", "")
            .text(0, "", "")
            .text(0, "", "")
            .text(0, "applied", "")
            .text(0, "condition met", "")
            .json(json!({"spec": {"listeners": [{"name": "http", "protocol": "HTTP", "port": 80}]}}))
            .json(json!({"items": [{"metadata": {"name": "envoy-shop-fx-web-1"}, "spec": {"ports": [{"port": 80}]}}]}))
    }

    #[test]
    fn expose_creates_labelled_objects_and_forwards_on_a_free_port() {
        let f = happy_fake();
        let env = FakeEnv::new(false, false);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("URL:   http://web.localhost:") && text.contains("curl --resolve web.localhost:"), "{text}");
        let calls = f.calls.borrow();
        let apply = calls.iter().find(|(a, _)| a.contains(&"apply".to_owned())).expect("an apply");
        let body = apply.1.as_ref().unwrap();
        for want in ["fx-web", "fluxctl-gateway", "fluxvm-envoy", "\"port\":8000", "GatewayClass"] {
            assert!(body.contains(want), "{want} in {body}");
        }
        assert!(calls.iter().all(|(a, _)| !a.contains(&"delete".to_owned())));
        let fwd = env.forwarded.borrow();
        assert_eq!(fwd.len(), 1);
        assert!(fwd[0].contains("port-forward --address 127.0.0.1 service/envoy-shop-fx-web-1 "), "{}", fwd[0]);
        assert!(fwd[0].ends_with(":80"));
    }

    #[test]
    fn an_existing_envoy_class_is_reused_and_a_foreign_object_is_never_overwritten() {
        let f = FakeKubectl::default()
            .json(json!({"metadata": {}}))
            .json(json!({"spec": {"ports": [{"port": 8000}]}}))
            .json(json!({"items": [{"metadata": {"name": "eg"}, "spec": {"controllerName": EG_CONTROLLER}}]}))
            .text(0, "", "")
            .json(json!({"metadata": {"name": "fx-web", "labels": {}}}));
        let env = FakeEnv::new(false, false);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = expose(&opts(), &sel(), &cluster(&f), &env, &mut out, &mut err).unwrap_err().to_string();
        assert!(e.contains("fluxctl did not make it") && e.contains("fx-web"), "{e}");
        assert!(f.argvs().iter().all(|a| !a.contains(" apply ")));
    }

    #[test]
    fn unexpose_only_deletes_objects_with_our_label() {
        let f = FakeKubectl::default()
            .json(labelled())
            .text(0, "deleted", "")
            .json(json!({"metadata": {"labels": {"app": "mine"}}}))
            .text(0, "", "");
        let mut out = Vec::new();
        unexpose("web", "shop", &sel(), &cluster(&f), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Deleted HTTPRoute fx-web") && text.contains("Left the Gateway fx-web alone"), "{text}");
        let deletes: Vec<String> = f.argvs().into_iter().filter(|a| a.contains(" delete ")).collect();
        assert_eq!(deletes.len(), 1, "{deletes:?}");
        assert!(deletes[0].contains("httproute.gateway.networking.k8s.io/fx-web"));
    }

    #[test]
    fn unexpose_with_nothing_there_says_so_and_deletes_nothing() {
        let f = empty(empty(empty(FakeKubectl::default())));
        let mut out = Vec::new();
        assert_eq!(unexpose("web", "shop", &sel(), &cluster(&f), &mut out).unwrap(), 0);
        assert!(String::from_utf8(out).unwrap().contains("fluxctl gateway list"));
        assert!(f.argvs().iter().all(|a| !a.contains(" delete ")));
    }

    #[test]
    fn list_uses_the_label_selector_and_prints_plain_words() {
        let gw = json!({"kind": "Gateway", "metadata": {"namespace": "shop", "labels": {MANAGED_BY_KEY: MANAGED_BY, SERVICE_KEY: "web"}, "annotations": {HOST_KEY: "web.localhost"}},
            "status": {"conditions": [{"type": "Programmed", "status": "True"}]}});
        let rt = json!({"kind": "HTTPRoute", "metadata": {"namespace": "shop", "labels": {MANAGED_BY_KEY: MANAGED_BY, SERVICE_KEY: "web"}},
            "status": {"parents": [{"conditions": [{"type": "Accepted", "status": "False", "reason": "NoMatchingListenerHostname"}]}]}});
        let stranger = json!({"kind": "Gateway", "metadata": {"namespace": "x", "labels": {SERVICE_KEY: "other"}}});
        let f = FakeKubectl::default().json(json!({"items": [gw, rt, stranger]}));
        let mut out = Vec::new();
        list(&sel(), &cluster(&f), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(f.argvs()[0].contains("-l app.kubernetes.io/managed-by=fluxctl-gateway") && f.argvs()[0].contains(" -A"));
        assert!(text.contains("web.localhost") && text.contains("ready") && text.contains("rejected (NoMatchingListenerHostname)"), "{text}");
        assert!(!text.contains("other"));
    }

    #[test]
    fn the_forward_loop_finds_the_service_again_and_reconnects() {
        let env = FakeEnv::new(false, false)
            .then(ForwardEnd::Exited { code: 1, ran: Duration::from_secs(1) })
            .then(ForwardEnd::Exited { code: 1, ran: Duration::from_secs(1) })
            .then(ForwardEnd::Interrupted);
        let mut calls = 0;
        let mut find = || {
            calls += 1;
            if calls == 1 {
                Err(anyhow!("expected one Envoy Service for this listener, found 0"))
            } else {
                Ok(vec!["kubectl".to_owned(), format!("try-{calls}")])
            }
        };
        let mut err = Vec::new();
        let code = forward_loop(vec!["kubectl".into(), "first".into()], &mut find, &env, &mut err).unwrap();
        assert_eq!(code, 0);
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("reconnecting") && text.contains("not there yet"), "{text}");
        assert_eq!(env.forwarded.borrow().len(), 3);
        assert_eq!(env.forwarded.borrow()[0], "kubectl first");
        assert!(env.forwarded.borrow()[1].contains("try-2"));
        assert!(!env.pauses.borrow().is_empty());
    }

    #[test]
    fn the_forward_loop_gives_up_when_nothing_ever_works() {
        let mut env = FakeEnv::new(false, false);
        for _ in 0..40 {
            env = env.then(ForwardEnd::Exited { code: 1, ran: Duration::ZERO });
        }
        let mut find = || Ok(vec!["kubectl".to_owned()]);
        let mut err = Vec::new();
        let e = forward_loop(vec!["kubectl".into()], &mut find, &env, &mut err).unwrap_err().to_string();
        assert!(e.contains("keeps ending") && e.contains("kubectl get pods"), "{e}");
    }

    #[test]
    fn names_hosts_and_ports_are_validated() {
        assert_eq!(object_name("web").unwrap(), "fx-web");
        assert!(object_name("Web").is_err());
        assert!(object_name(&"a".repeat(61)).is_err());
        assert!(valid_host("web.localhost") && !valid_host("Web") && !valid_host("a..b") && !valid_host(""));
        let svc = json!({"spec": {"ports": [{"port": 80}, {"port": 81}]}});
        assert!(service_port(&svc, None, &opts()).unwrap_err().to_string().contains("--port"));
        assert_eq!(service_port(&svc, Some(81), &opts()).unwrap(), 81);
        assert!(service_port(&svc, Some(9), &opts()).is_err());
    }
}
