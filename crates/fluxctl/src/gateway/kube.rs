// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Reads Gateway API evidence and builds the `kubectl port-forward` command, through the kubectl on this host and the
//! kubeconfig or context you select. Ported from Zyvor Setura 0.1.0.
//!
//! Every kubectl call goes through [`KubeRun`], so the tests use a fake and never start a process. The real runner is
//! [`SystemKubectl`]: no shell, a 20 second deadline, `--request-timeout=10s` on every read.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// What one kubectl call produced.
#[derive(Debug, Clone, Default)]
pub struct RunOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs one program with arguments (and optional stdin) and returns what it printed. `Err` means it could not be run or
/// passed its deadline.
pub trait KubeRun {
    fn run(
        &self,
        argv: &[String],
        stdin: Option<&[u8]>,
        deadline: Duration,
    ) -> Result<RunOut, String>;
}

/// The kubectl on `PATH`, run without a shell.
pub struct SystemKubectl;

impl KubeRun for SystemKubectl {
    fn run(
        &self,
        argv: &[String],
        stdin: Option<&[u8]>,
        deadline: Duration,
    ) -> Result<RunOut, String> {
        let (program, args) = argv.split_first().ok_or("empty command")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        if let Some(data) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
            });
        }
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(deadline) {
            Ok(Ok(out)) => Ok(RunOut {
                code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            Ok(Err(e)) => Err(format!("{program}: {e}")),
            Err(_) => {
                // SAFETY: plain kill(2) on the child this function started.
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                Err(format!(
                    "{program} did not finish within {} seconds",
                    deadline.as_secs()
                ))
            }
        }
    }
}

/// Why a read failed: a bad argument (nothing was run) or kubectl itself failing.
#[derive(Debug)]
pub enum GetErr {
    Invalid(String),
    Failed(String),
}

impl std::fmt::Display for GetErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GetErr::Invalid(s) | GetErr::Failed(s) => f.write_str(s),
        }
    }
}

/// A Kubernetes DNS label: lowercase letters, digits and `-`, at most 63 characters, alphanumeric at both ends.
pub fn label(value: &str) -> Result<&str, GetErr> {
    let b = value.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-';
    if ok {
        Ok(value)
    } else {
        Err(GetErr::Invalid(format!(
            "expected Kubernetes DNS label, got {value:?}"
        )))
    }
}

/// Quotes an argument for display in a shell command line, only where it needs it.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The cluster to talk to: kubectl plus the flags that select a kubeconfig and context.
pub struct Cluster<'a> {
    pub cmd: Vec<String>,
    pub runner: &'a dyn KubeRun,
}

/// Which kubeconfig and context kubectl should use. Both empty means kubectl's own default (`KUBECONFIG`, current context).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    pub kube_context: Option<String>,
    /// A FluxVM cluster by name: `~/.kube/fluxvm-NAME.yaml`, the file `fluxctl kube up` writes.
    pub cluster: Option<String>,
}

impl Selection {
    pub fn is_explicit(&self) -> bool {
        self.kube_context.is_some() || self.cluster.is_some()
    }

    /// The kubectl prefix for this selection. `home` is the home directory (for `--cluster`).
    pub fn prefix(&self, home: Option<&std::path::Path>) -> Result<Vec<String>> {
        let mut v = vec!["kubectl".to_owned()];
        if let Some(name) = &self.cluster {
            label(name).map_err(|e| anyhow!("--cluster: {e}"))?;
            let home = home.ok_or_else(|| anyhow!("--cluster needs a home directory"))?;
            let path = home.join(".kube").join(format!("fluxvm-{name}.yaml"));
            v.extend(["--kubeconfig".to_owned(), path.display().to_string()]);
        }
        if let Some(c) = &self.kube_context {
            if c.is_empty() || c.starts_with('-') || c.bytes().any(|b| b < 33 || b == 127) {
                bail!("--kube-context {c:?} is not a context name");
            }
            v.extend(["--context".to_owned(), c.clone()]);
        }
        Ok(v)
    }
}

impl<'a> Cluster<'a> {
    pub fn new(cmd: Vec<String>, runner: &'a dyn KubeRun) -> Self {
        Cluster { cmd, runner }
    }

    fn args(&self, rest: &[&str]) -> Vec<String> {
        self.cmd
            .iter()
            .cloned()
            .chain(rest.iter().map(|s| (*s).to_owned()))
            .collect()
    }

    /// `kubectl -n NS get RESOURCE OPTIONS -o json`, parsed.
    pub fn get(&self, namespace: &str, resource: &str, options: &[&str]) -> Result<Value, GetErr> {
        let mut rest = vec!["-n", label(namespace)?, "get", resource];
        rest.extend_from_slice(options);
        rest.extend(["-o", "json", "--request-timeout=10s"]);
        let out = self
            .runner
            .run(&self.args(&rest), None, Duration::from_secs(20))
            .map_err(GetErr::Failed)?;
        if out.code != 0 {
            let msg = out.stderr.trim();
            return Err(GetErr::Failed(if msg.is_empty() {
                format!("kubectl get {resource} failed")
            } else {
                msg.to_owned()
            }));
        }
        serde_json::from_str(&out.stdout)
            .map_err(|e| GetErr::Failed(format!("kubectl get {resource}: not JSON: {e}")))
    }

    /// The `kubectl port-forward` command that publishes one listener of a Gateway on `127.0.0.1`. Finds the single Envoy
    /// Service Envoy Gateway generated for the Gateway that exposes the listener's port.
    pub fn publication(
        &self,
        namespace: &str,
        gateway: &str,
        listener: &str,
        proxy_namespace: &str,
        local_port: i64,
    ) -> Result<Vec<String>> {
        if !(1..=65535).contains(&local_port) {
            bail!("local port must be between 1 and 65535");
        }
        let obj = self
            .get(
                namespace,
                &format!(
                    "gateway.gateway.networking.k8s.io/{}",
                    label(gateway).map_err(|e| anyhow!("{e}"))?
                ),
                &[],
            )
            .map_err(|e| anyhow!("{e}"))?;
        let target = obj["spec"]["listeners"]
            .as_array()
            .and_then(|l| l.iter().find(|x| x["name"].as_str() == Some(listener)))
            .filter(|t| matches!(t["protocol"].as_str(), Some("HTTP" | "HTTPS")))
            .ok_or_else(|| anyhow!("choose an existing HTTP or HTTPS listener"))?;
        let port = target["port"]
            .as_i64()
            .ok_or_else(|| anyhow!("the listener has no port"))?;
        let selector = format!(
            "gateway.envoyproxy.io/owning-gateway-name={gateway},gateway.envoyproxy.io/owning-gateway-namespace={namespace}"
        );
        let services = self
            .get(proxy_namespace, "services", &["-l", &selector])
            .map_err(|e| anyhow!("{e}"))?;
        let matches: Vec<&str> = services["items"]
            .as_array()
            .map(|items| items.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|s| {
                s["spec"]["ports"].as_array().is_some_and(|ps| {
                    ps.iter().any(|p| {
                        p["port"].as_i64() == Some(port)
                            && p["protocol"].as_str().unwrap_or("TCP") == "TCP"
                    })
                })
            })
            .filter_map(|s| s["metadata"]["name"].as_str())
            .collect();
        if matches.len() != 1 {
            bail!(
                "expected one Envoy Service for this listener, found {}",
                matches.len()
            );
        }
        let svc = label(matches[0]).map_err(|e| anyhow!("{e}"))?;
        Ok(self.args(&[
            "-n",
            proxy_namespace,
            "port-forward",
            "--address",
            "127.0.0.1",
            &format!("service/{svc}"),
            &format!("{local_port}:{port}"),
        ]))
    }

    /// An HTTPRoute's conditions per parent, whether each is current, and its Services' ready endpoint counts.
    pub fn inspect(&self, namespace: &str, route: &str) -> Result<Value> {
        let obj = self
            .get(
                namespace,
                &format!(
                    "httproute.gateway.networking.k8s.io/{}",
                    label(route).map_err(|e| anyhow!("{e}"))?
                ),
                &[],
            )
            .map_err(|e| anyhow!("{e}"))?;
        let generation = obj["metadata"]["generation"].clone();
        let mut parents = vec![];
        for entry in obj["status"]["parents"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let conditions: Vec<Value> = entry["conditions"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    let current =
                        c.get("observedGeneration") == Some(&generation) && !generation.is_null();
                    c["current"] = json!(current);
                    c
                })
                .collect();
            parents.push(json!({"parent": entry["parentRef"], "controller": entry["controllerName"], "conditions": conditions}));
        }
        let mut backends = vec![];
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for rule in obj["spec"]["rules"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            for r in rule["backendRefs"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                let name = r["name"]
                    .as_str()
                    .ok_or_else(|| anyhow!("a backendRef has no name"))?;
                let ns = r["namespace"].as_str().unwrap_or(namespace);
                let kind = r["kind"].as_str().unwrap_or("Service");
                let group = r["group"].as_str().unwrap_or("");
                let key = format!("{ns}\u{0}{name}\u{0}{}\u{0}{kind}\u{0}{group}", r["port"]);
                if !seen.insert(key) {
                    continue;
                }
                let mut row = json!({"reference": r, "namespace": ns});
                if (kind, group) != ("Service", "") {
                    row["observation"] = json!("unsupported backend kind");
                    backends.push(row);
                    continue;
                }
                match self.service_evidence(ns, name, r.get("port").cloned().unwrap_or(Value::Null))
                {
                    Ok((found, counts)) => {
                        row["service_port_found"] = json!(found);
                        row["endpoint_records_by_ready_condition"] = counts;
                    }
                    Err(GetErr::Invalid(m)) => bail!("{m}"),
                    Err(GetErr::Failed(m)) => row["observation_error"] = json!(m),
                }
                backends.push(row);
            }
        }
        Ok(
            json!({"namespace": namespace, "route": route, "generation": generation, "parents": parents, "backends": backends}),
        )
    }

    fn service_evidence(&self, ns: &str, name: &str, port: Value) -> Result<(bool, Value), GetErr> {
        label(name)?;
        let svc = self.get(ns, &format!("service/{name}"), &[])?;
        let slices = self.get(
            ns,
            "endpointslices.discovery.k8s.io",
            &["-l", &format!("kubernetes.io/service-name={name}")],
        )?;
        let found = svc["spec"]["ports"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p["port"] == port));
        let (mut t, mut f, mut u) = (0u64, 0u64, 0u64);
        for s in slices["items"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            for e in s["endpoints"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
            {
                match e["conditions"]["ready"].as_bool() {
                    Some(true) => t += 1,
                    Some(false) => f += 1,
                    None => u += 1,
                }
            }
        }
        Ok((found, json!({"true": t, "false": f, "unknown": u})))
    }
}

// ---- health facts -----------------------------------------------------------------------------------------------------

const ENVOY_CONTROLLER: &str = "gateway.envoyproxy.io/gatewayclass-controller";

fn items_of(v: &Value) -> &[Value] {
    v["items"].as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

fn condition(conditions: &Value, kind: &str) -> Option<fluxvm_core::health::GwCondition> {
    conditions
        .as_array()?
        .iter()
        .find(|c| c["type"].as_str() == Some(kind))
        .map(|c| fluxvm_core::health::GwCondition {
            status: c["status"].as_str().unwrap_or("Unknown").to_owned(),
            observed_generation: c["observedGeneration"].as_i64(),
            message: c["message"].as_str().filter(|m| !m.is_empty()).map(str::to_owned),
        })
}

impl Cluster<'_> {
    /// What the health model needs to know about this cluster's Gateways, HTTPRoutes and their backends. Read-only.
    /// `None` when the cluster cannot be read or has no Gateway API, which is nothing to report. `cluster` is the FluxVM
    /// cluster name used in the fix commands.
    pub fn facts(&self, cluster: &str) -> Option<fluxvm_core::health::GatewayFacts> {
        use fluxvm_core::health::{BackendFact, GatewayFact, GatewayFacts, RouteFact, RouteParentFact};
        let list = |resource: &str, all_namespaces: bool| -> Result<Value, GetErr> {
            let mut rest = vec!["get", resource];
            if all_namespaces {
                rest.push("-A");
            }
            rest.extend(["-o", "json", "--request-timeout=10s"]);
            let out = self
                .runner
                .run(&self.args(&rest), None, Duration::from_secs(20))
                .map_err(GetErr::Failed)?;
            if out.code != 0 {
                return Err(GetErr::Failed(out.stderr.trim().to_owned()));
            }
            serde_json::from_str(&out.stdout).map_err(|e| GetErr::Failed(e.to_string()))
        };
        let gateways = list("gateways.gateway.networking.k8s.io", true).ok()?;
        let classes = list("gatewayclasses.gateway.networking.k8s.io", false).unwrap_or(Value::Null);
        let envoy_classes: BTreeSet<&str> = items_of(&classes)
            .iter()
            .filter(|c| c["spec"]["controllerName"].as_str() == Some(ENVOY_CONTROLLER))
            .filter_map(|c| c["metadata"]["name"].as_str())
            .collect();
        let mut out = GatewayFacts { cluster: cluster.to_owned(), ..Default::default() };
        for g in items_of(&gateways) {
            out.gateways.push(GatewayFact {
                namespace: g["metadata"]["namespace"].as_str().unwrap_or("default").to_owned(),
                name: g["metadata"]["name"].as_str().unwrap_or("").to_owned(),
                envoy: g["spec"]["gatewayClassName"].as_str().is_some_and(|c| envoy_classes.contains(c)),
                generation: g["metadata"]["generation"].as_i64(),
                programmed: condition(&g["status"]["conditions"], "Programmed"),
            });
        }
        let routes = list("httproutes.gateway.networking.k8s.io", true).unwrap_or(Value::Null);
        let mut checked: std::collections::BTreeMap<(String, String), Option<u64>> = Default::default();
        for r in items_of(&routes).iter().take(200) {
            let ns = r["metadata"]["namespace"].as_str().unwrap_or("default");
            let envoy = r["spec"]["parentRefs"].as_array().is_some_and(|p| {
                p.iter().any(|p| {
                    let pns = p["namespace"].as_str().unwrap_or(ns);
                    out.gateways.iter().any(|g| g.envoy && g.namespace == pns && Some(g.name.as_str()) == p["name"].as_str())
                })
            });
            let mut route = RouteFact {
                namespace: ns.to_owned(),
                name: r["metadata"]["name"].as_str().unwrap_or("").to_owned(),
                envoy,
                generation: r["metadata"]["generation"].as_i64(),
                ..Default::default()
            };
            for p in r["status"]["parents"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                route.parents.push(RouteParentFact {
                    gateway: p["parentRef"]["name"].as_str().unwrap_or("?").to_owned(),
                    accepted: condition(&p["conditions"], "Accepted"),
                    resolved_refs: condition(&p["conditions"], "ResolvedRefs"),
                });
            }
            for rule in r["spec"]["rules"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                for b in rule["backendRefs"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                    let (kind, group) = (b["kind"].as_str().unwrap_or("Service"), b["group"].as_str().unwrap_or(""));
                    let (Some(name), true) = (b["name"].as_str(), (kind, group) == ("Service", "")) else { continue };
                    let bns = b["namespace"].as_str().unwrap_or(ns).to_owned();
                    if route.backends.iter().any(|x| x.namespace == bns && x.service == name) {
                        continue;
                    }
                    // At most 50 Services are looked up, so a huge cluster cannot make `cluster status` slow.
                    let ready = match checked.get(&(bns.clone(), name.to_owned())) {
                        Some(v) => *v,
                        None if checked.len() >= 50 => None,
                        None => {
                            let v = self.service_evidence(&bns, name, Value::Null).ok().and_then(|(_, c)| {
                                let ready = c["true"].as_u64()?;
                                // Only a definite zero: endpoints with unknown readiness are not "not ready".
                                (c["unknown"].as_u64() == Some(0) || ready > 0).then_some(ready)
                            });
                            checked.insert((bns.clone(), name.to_owned()), v);
                            v
                        }
                    };
                    route.backends.push(BackendFact { namespace: bns, service: name.to_owned(), ready_endpoints: ready });
                }
            }
            out.routes.push(route);
        }
        out.controller_present = match self.get("envoy-gateway-system", "deployment/envoy-gateway", &[]) {
            Ok(d) => Some(d["status"]["availableReplicas"].as_u64().unwrap_or(0) > 0),
            Err(GetErr::Failed(m)) if m.contains("NotFound") || m.contains("not found") => Some(false),
            Err(_) => None,
        };
        Some(out)
    }
}

/// Replaces this process with the command, so signals (Ctrl-C) go straight to kubectl. Only returns on failure.
pub fn exec_forward(command: &[String]) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let Some((program, args)) = command.split_first() else {
        return anyhow!("empty command");
    };
    let err = Command::new(program).args(args).exec();
    anyhow!("could not run {program}: {err}")
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Answers each call from a queue and records every argv (and stdin). No process is started.
    #[derive(Default)]
    pub struct FakeKubectl {
        pub answers: RefCell<VecDeque<Result<RunOut, String>>>,
        pub calls: RefCell<Vec<(Vec<String>, Option<String>)>>,
    }

    impl FakeKubectl {
        pub fn json(self, v: Value) -> Self {
            self.answers.borrow_mut().push_back(Ok(RunOut {
                code: 0,
                stdout: v.to_string(),
                stderr: String::new(),
            }));
            self
        }
        pub fn text(self, code: i32, out: &str, err: &str) -> Self {
            self.answers.borrow_mut().push_back(Ok(RunOut {
                code,
                stdout: out.into(),
                stderr: err.into(),
            }));
            self
        }
        pub fn fail(self, msg: &str) -> Self {
            self.answers.borrow_mut().push_back(Err(msg.into()));
            self
        }
        pub fn argvs(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(a, _)| a.join(" "))
                .collect()
        }
    }

    impl KubeRun for FakeKubectl {
        fn run(
            &self,
            argv: &[String],
            stdin: Option<&[u8]>,
            _: Duration,
        ) -> Result<RunOut, String> {
            self.calls.borrow_mut().push((
                argv.to_vec(),
                stdin.map(|b| String::from_utf8_lossy(b).into_owned()),
            ));
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected kubectl call".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeKubectl;
    use super::*;

    fn gateway() -> Value {
        json!({"spec": {"listeners": [{"name": "web", "protocol": "HTTP", "port": 8080}]}})
    }
    fn svc() -> Value {
        json!({"metadata": {"name": "generated-envoy"}, "spec": {"ports": [{"port": 8080}]}})
    }
    fn cluster<'a>(f: &'a FakeKubectl, context: Option<&str>) -> Cluster<'a> {
        let sel = Selection {
            kube_context: context.map(str::to_owned),
            cluster: None,
        };
        Cluster::new(sel.prefix(None).unwrap(), f)
    }

    #[test]
    fn service_discovery_builds_a_loopback_port_forward() {
        let f = FakeKubectl::default()
            .json(gateway())
            .json(json!({"items": [svc()]}));
        let cmd = cluster(&f, Some("mactra"))
            .publication("demo", "dev", "web", "envoy", 9080)
            .unwrap();
        assert_eq!(
            cmd,
            [
                "kubectl",
                "--context",
                "mactra",
                "-n",
                "envoy",
                "port-forward",
                "--address",
                "127.0.0.1",
                "service/generated-envoy",
                "9080:8080"
            ]
        );
        let calls = f.argvs();
        assert!(
            calls[1].contains("owning-gateway-name=dev")
                && calls[1].contains("owning-gateway-namespace=demo")
        );
        assert!(calls[1].contains("-n envoy get services"));
        assert!(
            calls[0].contains("get gateway.gateway.networking.k8s.io/dev")
                && calls[0].ends_with("--request-timeout=10s")
        );
    }

    #[test]
    fn a_missing_or_ambiguous_service_is_refused() {
        for items in [vec![], vec![svc(), svc()]] {
            let f = FakeKubectl::default()
                .json(gateway())
                .json(json!({"items": items}));
            let e = cluster(&f, None)
                .publication("demo", "dev", "web", "envoy", 8080)
                .unwrap_err();
            assert!(e.to_string().contains("expected one Envoy Service"), "{e}");
        }
    }

    #[test]
    fn only_a_tcp_service_on_the_listener_port_matches() {
        let udp = json!({"metadata": {"name": "dns"}, "spec": {"ports": [{"port": 8080, "protocol": "UDP"}]}});
        let other = json!({"metadata": {"name": "other"}, "spec": {"ports": [{"port": 9090}]}});
        let f = FakeKubectl::default()
            .json(gateway())
            .json(json!({"items": [udp, other, svc()]}));
        let cmd = cluster(&f, None)
            .publication("demo", "dev", "web", "envoy", 8080)
            .unwrap();
        assert!(cmd.contains(&"service/generated-envoy".to_owned()));
        assert_eq!(cmd[0], "kubectl");
    }

    #[test]
    fn unknown_or_non_http_listener_and_bad_inputs_are_refused() {
        let f = FakeKubectl::default().json(gateway());
        assert!(
            cluster(&f, None)
                .publication("demo", "dev", "nope", "envoy", 8080)
                .is_err()
        );
        let tcp =
            json!({"spec": {"listeners": [{"name": "web", "protocol": "TCP", "port": 8080}]}});
        let f = FakeKubectl::default().json(tcp);
        assert!(
            cluster(&f, None)
                .publication("demo", "dev", "web", "envoy", 8080)
                .is_err()
        );
        // nothing is run for an invalid name or port
        let f = FakeKubectl::default();
        assert!(
            cluster(&f, None)
                .publication("Demo", "dev", "web", "envoy", 8080)
                .is_err()
        );
        assert!(
            cluster(&f, None)
                .publication("demo", "dev", "web", "envoy", 0)
                .is_err()
        );
        assert!(
            cluster(&f, None)
                .publication("demo", "dev", "web", "envoy", 70000)
                .is_err()
        );
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn kubectl_failure_is_reported_with_its_message() {
        let f = FakeKubectl::default().text(
            1,
            "",
            "Error from server (NotFound): gateways \"dev\" not found\n",
        );
        let e = cluster(&f, None)
            .publication("demo", "dev", "web", "envoy", 8080)
            .unwrap_err();
        assert!(e.to_string().contains("NotFound"));
        let f = FakeKubectl::default().fail("kubectl did not finish within 20 seconds");
        assert!(cluster(&f, None).inspect("demo", "orders").is_err());
    }

    #[test]
    fn inspect_marks_stale_conditions_and_counts_unknown_endpoints() {
        let route = json!({"metadata": {"generation": 2},
            "spec": {"rules": [{"backendRefs": [{"name": "orders", "port": 80}]}]},
            "status": {"parents": [{"parentRef": {"name": "dev"}, "controllerName": "envoy",
                "conditions": [{"type": "Accepted", "status": "True", "observedGeneration": 1},
                               {"type": "ResolvedRefs", "status": "True", "observedGeneration": 2}]}]}});
        let f = FakeKubectl::default()
            .json(route)
            .json(json!({"spec": {"ports": [{"port": 80}]}}))
            .json(json!({"items": [{"endpoints": [{}, {"conditions": {"ready": false}}, {"conditions": {"ready": true}}]}]}));
        let row = cluster(&f, None).inspect("demo", "orders").unwrap();
        assert_eq!(row["parents"][0]["conditions"][0]["current"], json!(false));
        assert_eq!(row["parents"][0]["conditions"][1]["current"], json!(true));
        assert_eq!(
            row["backends"][0]["endpoint_records_by_ready_condition"],
            json!({"true": 1, "false": 1, "unknown": 1})
        );
        assert_eq!(row["backends"][0]["service_port_found"], json!(true));
        assert_eq!(row["generation"], json!(2));
        assert!(f.argvs()[2].contains("kubernetes.io/service-name=orders"));
    }

    #[test]
    fn inspect_dedupes_backends_and_flags_other_kinds_and_read_errors() {
        let route = json!({"metadata": {"generation": 1}, "spec": {"rules": [
            {"backendRefs": [{"name": "a", "port": 80}, {"name": "a", "port": 80}, {"name": "b", "port": 80, "kind": "ServiceImport", "group": "multicluster.x-k8s.io"}]},
            {"backendRefs": [{"name": "c", "port": 80, "namespace": "other"}]}]}});
        let f = FakeKubectl::default()
            .json(route)
            .json(json!({"spec": {"ports": [{"port": 8080}]}}))
            .json(json!({"items": []}))
            .text(1, "", "forbidden");
        let row = cluster(&f, None).inspect("demo", "r").unwrap();
        let b = row["backends"].as_array().unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b[0]["service_port_found"], json!(false));
        assert_eq!(b[1]["observation"], json!("unsupported backend kind"));
        assert_eq!(b[2]["namespace"], json!("other"));
        assert_eq!(b[2]["observation_error"], json!("forbidden"));
    }

    #[test]
    fn selection_prefix_and_labels() {
        let home = std::path::Path::new("/home/u");
        let s = Selection {
            kube_context: Some("lab".into()),
            cluster: Some("dev".into()),
        };
        assert_eq!(
            s.prefix(Some(home)).unwrap(),
            [
                "kubectl",
                "--kubeconfig",
                "/home/u/.kube/fluxvm-dev.yaml",
                "--context",
                "lab"
            ]
        );
        assert_eq!(Selection::default().prefix(None).unwrap(), ["kubectl"]);
        assert!(
            Selection {
                cluster: Some("../x".into()),
                ..Default::default()
            }
            .prefix(Some(home))
            .is_err()
        );
        assert!(
            Selection {
                kube_context: Some("--all".into()),
                ..Default::default()
            }
            .prefix(None)
            .is_err()
        );
        assert!(label("a-b1").is_ok());
        for bad in ["", "-a", "a-", "A", "a_b", &"a".repeat(64)] {
            assert!(label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shell_join_quotes_only_when_needed() {
        let v: Vec<String> = [
            "kubectl",
            "--context",
            "my ctx",
            "-n",
            "envoy",
            "8080:80",
            "it's",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            shell_join(&v),
            "kubectl --context 'my ctx' -n envoy 8080:80 'it'\\''s'"
        );
    }

    #[test]
    fn facts_read_gateways_routes_backends_and_the_controller() {
        let gateways = json!({"items": [{"metadata": {"namespace": "web", "name": "edge", "generation": 2},
            "spec": {"gatewayClassName": "eg"},
            "status": {"conditions": [{"type": "Programmed", "status": "False", "observedGeneration": 2, "message": "no address"}]}}]});
        let classes = json!({"items": [{"metadata": {"name": "eg"}, "spec": {"controllerName": "gateway.envoyproxy.io/gatewayclass-controller"}}]});
        let routes = json!({"items": [{"metadata": {"namespace": "web", "name": "shop", "generation": 3},
            "spec": {"parentRefs": [{"name": "edge"}], "rules": [{"backendRefs": [{"name": "shop", "port": 80}, {"name": "shop", "port": 80}]}]},
            "status": {"parents": [{"parentRef": {"name": "edge"}, "conditions": [
                {"type": "Accepted", "status": "True", "observedGeneration": 3},
                {"type": "ResolvedRefs", "status": "False", "observedGeneration": 2}]}]}}]});
        let f = FakeKubectl::default()
            .json(gateways)
            .json(classes)
            .json(routes)
            .json(json!({"spec": {"ports": [{"port": 80}]}}))
            .json(json!({"items": [{"endpoints": [{"conditions": {"ready": false}}]}]}))
            .text(1, "", "Error from server (NotFound): deployments.apps \"envoy-gateway\" not found");
        let facts = cluster(&f, None).facts("dev").unwrap();
        assert_eq!(facts.cluster, "dev");
        assert_eq!(facts.controller_present, Some(false));
        assert!(facts.gateways[0].envoy);
        assert_eq!(facts.gateways[0].programmed.as_ref().unwrap().observed_generation, Some(2));
        assert!(facts.routes[0].envoy);
        assert_eq!(facts.routes[0].backends.len(), 1);
        assert_eq!(facts.routes[0].backends[0].ready_endpoints, Some(0));
        assert_eq!(facts.routes[0].parents[0].resolved_refs.as_ref().unwrap().observed_generation, Some(2));
        // reads only
        assert!(f.argvs().iter().all(|a| a.contains(" get ")), "{:?}", f.argvs());
    }

    #[test]
    fn facts_are_none_when_the_cluster_has_no_gateway_api_or_cannot_be_read() {
        let f = FakeKubectl::default().text(1, "", "error: the server doesn't have a resource type \"gateways\"");
        assert!(cluster(&f, None).facts("dev").is_none());
        let f = FakeKubectl::default().fail("kubectl did not finish within 20 seconds");
        assert!(cluster(&f, None).facts("dev").is_none());
    }
}
