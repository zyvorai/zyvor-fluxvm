// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl gateway expose | unexpose | list | publish | explain | check | inspect | demo`: built-in Gateway API tooling for Envoy Gateway on a FluxVM
//! k3s cluster, ported from Zyvor Setura 0.1.0 (see docs/gateway.md).
//!
//! * `expose SERVICE` is the one-command path (installs Envoy Gateway if you agree, makes a Gateway and HTTPRoute, forwards a free
//!   local port and prints the URL); `unexpose SERVICE` and `list` only touch objects labelled `managed-by=fluxctl-gateway`.
//! * `publish GATEWAY --listener NAME` finds the one Envoy Service behind a Gateway listener and runs `kubectl port-forward`
//!   on `127.0.0.1` (the process becomes kubectl, so Ctrl-C goes to it). `--dry-run` prints the command.
//! * `explain URL` makes one direct HTTP/1.1 GET and reports dns, tcp, tls, http and backend, each as ok or failed.
//! * `check FILE` runs a JSON array of those probes.
//! * `inspect ROUTE` shows an HTTPRoute's conditions per parent and its backends' ready endpoint counts.
//! * `demo apply | teardown | print` manages the demo fixture.
//!
//! Exit codes: 0 everything passed, 1 a probe observed a failure, 2 usage error or an error running kubectl.
//! Which cluster: `--cluster NAME` uses `~/.kube/fluxvm-NAME.yaml` (written by `fluxctl kube up`), `--kube-context` selects a
//! context of the kubeconfig kubectl would use anyway; with neither, kubectl's current context. (`--context` is fluxctl's
//! own daemon context and is not used here.)

mod demo;
mod expose;
pub(crate) mod kube;
mod probe;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use kube::{Cluster, KubeRun, Selection, SystemKubectl, shell_join};
use probe::{ProbeOpts, probe};
use serde::Deserialize;
use std::io::Write;

/// Which cluster kubectl talks to.
#[derive(Args, Debug, Clone, Default)]
pub struct Target {
    /// A kubectl context to use (default: kubectl's current context).
    #[arg(long = "kube-context", value_name = "CONTEXT")]
    pub kube_context: Option<String>,
    /// A FluxVM cluster by name: uses the kubeconfig `~/.kube/fluxvm-NAME.yaml` that `fluxctl kube up` wrote.
    #[arg(long, value_name = "NAME")]
    pub cluster: Option<String>,
}

impl Target {
    fn selection(&self) -> Selection {
        Selection {
            kube_context: self.kube_context.clone(),
            cluster: self.cluster.clone(),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum GatewayCommand {
    /// The one-command path: make a Kubernetes Service reachable from this machine. Installs Envoy Gateway if the cluster has
    /// none (asks first; `--yes` skips the question), creates a Gateway and an HTTPRoute for the Service, waits for them, picks
    /// a free local port and forwards it until Ctrl-C. Prints the URL (default host SERVICE.localhost).
    Expose {
        service: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// The Service port to route to (default: the Service's only port).
        #[arg(long, value_name = "SERVICE_PORT")]
        port: Option<i64>,
        /// The host name to route (default: SERVICE.localhost).
        #[arg(long, value_name = "NAME")]
        host: Option<String>,
        /// The local port on 127.0.0.1 (default: the first free one from 8080).
        #[arg(long, value_name = "N")]
        local_port: Option<u16>,
        /// Do not ask before installing Envoy Gateway.
        #[arg(long)]
        yes: bool,
        /// Print the objects and commands and run nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Remove what `expose` made for a Service (its Gateway, HTTPRoute and EnvoyProxy). Only objects labelled
    /// `app.kubernetes.io/managed-by=fluxctl-gateway` are ever deleted.
    Unexpose {
        service: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        #[command(flatten)]
        target: Target,
    },
    /// List what `expose` made, in every namespace: host, and whether the route is accepted and the gateway ready.
    List {
        #[command(flatten)]
        target: Target,
    },
    /// Forward one Gateway listener to 127.0.0.1 with kubectl port-forward, so you can open it from this machine. Finds the
    /// single Envoy Service Envoy Gateway made for the listener's port. Runs until Ctrl-C, or until the proxy pod exits.
    Publish {
        gateway: String,
        /// The listener name (an HTTP or HTTPS listener of the Gateway).
        #[arg(long)]
        listener: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// Where Envoy Gateway runs its proxies.
        #[arg(long, default_value = "envoy-gateway-system")]
        proxy_namespace: String,
        /// The local port on 127.0.0.1 (default: the first free one from 8080).
        #[arg(long)]
        port: Option<i64>,
        /// Look the Service up and print the kubectl command, but do not run it.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Show an HTTPRoute's status per parent (is each condition about the current generation?) and, for each backend Service,
    /// whether the port exists and how many endpoints are ready. JSON.
    Inspect {
        route: String,
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        #[command(flatten)]
        target: Target,
    },
    /// One direct HTTP/1.1 GET from this machine, step by step (dns, tcp, tls, http, backend). Exit 0 when it passed, 1 when a
    /// step failed. Proxy environment variables are ignored.
    Explain {
        url: String,
        /// Connect to this numeric IP while the request keeps the URL's hostname in Host and in the TLS server name.
        #[arg(long)]
        address: Option<String>,
        /// The expected X-Setura-Backend response header (which application answered).
        #[arg(long)]
        backend: Option<String>,
        /// A PEM CA bundle to trust for this request, in addition to the system roots.
        #[arg(long)]
        ca: Option<String>,
        /// The expected HTTP status.
        #[arg(long, default_value_t = 200)]
        status: i64,
        /// Seconds for each connect, read and write (up to 60).
        #[arg(long, default_value_t = 4.0)]
        timeout: f64,
        #[arg(long)]
        json: bool,
    },
    /// Run a JSON file of probes (an array of {url, address, status, backend, ca, timeout}); prints the reports as JSON.
    /// Exit 0 when all passed, 1 when any failed. `gateway demo print --checks` shows an example.
    Check { file: String },
    /// The built-in demo: a Gateway, an app and a route to try the commands on.
    Demo {
        #[command(subcommand)]
        command: DemoCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum DemoCommand {
    /// Apply the demo fixture (namespace fluxvm-gateway-demo and GatewayClass fluxvm-demo-envoy) and wait until it is ready.
    /// Needs Envoy Gateway on the cluster (`fluxctl kube apple create --addons envoy-gateway`).
    Apply {
        /// Print the kubectl commands and run none.
        #[arg(long)]
        dry_run: bool,
        /// Use kubectl's current context without naming a cluster.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Delete exactly what `demo apply` created.
    Teardown {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Print the demo manifest, or with --checks the example probes for `gateway check`.
    Print {
        #[arg(long)]
        checks: bool,
    },
}

/// Runs the command and returns the process exit code.
pub fn run(cmd: GatewayCommand) -> i32 {
    let mut out = std::io::stdout();
    let code = match execute(cmd, &SystemKubectl, &mut out) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fluxctl gateway: {e:#}");
            2
        }
    };
    let _ = out.flush();
    code
}

fn cluster_for<'a>(t: &Target, runner: &'a dyn KubeRun) -> Result<Cluster<'a>> {
    let sel = t.selection();
    let home = if sel.cluster.is_some() {
        Some(crate::docker_host::home()?)
    } else {
        None
    };
    Ok(Cluster::new(sel.prefix(home.as_deref())?, runner))
}

/// [`run`] with a replaceable kubectl and output, for the tests.
pub fn execute(cmd: GatewayCommand, runner: &dyn KubeRun, out: &mut dyn Write) -> Result<i32> {
    execute_with(cmd, runner, &expose::SystemEnv, out)
}

/// [`execute`] with a replaceable terminal and port-forward process too.
pub fn execute_with(
    cmd: GatewayCommand,
    runner: &dyn KubeRun,
    env: &dyn expose::ExposeEnv,
    out: &mut dyn Write,
) -> Result<i32> {
    match cmd {
        GatewayCommand::Expose {
            service,
            namespace,
            port,
            host,
            local_port,
            yes,
            dry_run,
            target,
        } => {
            let c = cluster_for(&target, runner)?;
            let opts = expose::ExposeOpts {
                service,
                namespace,
                port,
                host,
                local_port,
                yes,
                dry_run,
            };
            expose::expose(&opts, &target.selection(), &c, env, out, &mut std::io::stderr())
        }
        GatewayCommand::Unexpose {
            service,
            namespace,
            target,
        } => {
            let c = cluster_for(&target, runner)?;
            expose::unexpose(&service, &namespace, &target.selection(), &c, out)
        }
        GatewayCommand::List { target } => {
            let c = cluster_for(&target, runner)?;
            expose::list(&target.selection(), &c, out)
        }
        GatewayCommand::Publish {
            gateway,
            listener,
            namespace,
            proxy_namespace,
            port,
            dry_run,
            target,
        } => {
            let c = cluster_for(&target, runner)?;
            // No --port: the first free local port (a real bind), instead of failing on a busy 8080.
            let local = match port {
                Some(p) => p,
                None => i64::from(expose::choose_port(None)?),
            };
            if let Some(p) = port
                && !dry_run
                && (!(1..=65535).contains(&p) || !expose::port_is_free(p as u16))
            {
                bail!(
                    "local port {p} is already in use (or not a valid port). Leave out --port to let fluxctl pick a free one, or choose another with --port N"
                );
            }
            let command = c.publication(&namespace, &gateway, &listener, &proxy_namespace, local)?;
            if dry_run {
                writeln!(out, "{}", shell_join(&command))?;
                return Ok(0);
            }
            writeln!(
                out,
                "Forwarding http://127.0.0.1:{local}/ to {gateway}/{listener} until Ctrl-C."
            )?;
            out.flush()?;
            let mut find = || {
                c.publication(&namespace, &gateway, &listener, &proxy_namespace, local)
            };
            expose::forward_loop(command, &mut find, env, &mut std::io::stderr())
        }
        GatewayCommand::Inspect {
            route,
            namespace,
            target,
        } => {
            let c = cluster_for(&target, runner)?;
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&c.inspect(&namespace, &route)?)?
            )?;
            Ok(0)
        }
        GatewayCommand::Explain {
            url,
            address,
            backend,
            ca,
            status,
            timeout,
            json,
        } => {
            let report = probe(&ProbeOpts {
                url,
                address,
                ca,
                status,
                backend,
                timeout,
            })
            .map_err(|e| anyhow!(e))?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
            } else {
                write!(out, "{}", probe::render_text(&report))?;
            }
            Ok(i32::from(!report.ok))
        }
        GatewayCommand::Check { file } => {
            let reports = check(&file)?;
            writeln!(out, "{}", serde_json::to_string_pretty(&reports)?)?;
            Ok(i32::from(reports.iter().any(|r| !r.ok)))
        }
        GatewayCommand::Demo { command } => match command {
            DemoCommand::Print { checks } => {
                write!(
                    out,
                    "{}",
                    if checks { demo::CHECKS } else { demo::MANIFEST }
                )?;
                Ok(0)
            }
            DemoCommand::Apply {
                dry_run,
                yes,
                target,
            } => demo_run(demo::Action::Apply, &target, dry_run, yes, runner, out),
            DemoCommand::Teardown {
                dry_run,
                yes,
                target,
            } => demo_run(demo::Action::Teardown, &target, dry_run, yes, runner, out),
        },
    }
}

fn demo_run(
    action: demo::Action,
    target: &Target,
    dry_run: bool,
    yes: bool,
    runner: &dyn KubeRun,
    out: &mut dyn Write,
) -> Result<i32> {
    let c = cluster_for(target, runner)?;
    demo::run(
        action,
        &target.selection(),
        &c.cmd,
        dry_run,
        yes,
        runner,
        out,
    )?;
    Ok(0)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    url: String,
    address: Option<String>,
    status: Option<i64>,
    backend: Option<String>,
    ca: Option<String>,
    timeout: Option<f64>,
}

/// Reads and runs a check file. Any invalid case (or a probe argument that is invalid) is an error before anything is printed.
fn check(path: &str) -> Result<Vec<probe::Report>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let cases: Vec<Case> = serde_json::from_str(&text).map_err(|e| {
        anyhow!("invalid check file {path}: {e}; see `fluxctl gateway demo print --checks`")
    })?;
    if cases.is_empty() {
        bail!("check file must be a nonempty JSON array");
    }
    let mut reports = vec![];
    for c in cases {
        let opts = ProbeOpts {
            url: c.url,
            address: c.address,
            ca: c.ca,
            status: c.status.unwrap_or(200),
            backend: c.backend,
            timeout: c.timeout.unwrap_or(4.0),
        };
        reports.push(probe(&opts).map_err(|e| anyhow!(e))?);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::kube::fake::FakeKubectl;
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::io::Read;
    use std::net::TcpListener;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        c: GatewayCommand,
    }

    /// Answers `n` connections with 200 and `X-Setura-Backend: orders-v1`.
    fn serve(n: usize) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut s, _)) = l.accept() else { return };
                s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .ok();
                let mut buf = Vec::new();
                let mut b = [0u8; 512];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => buf.extend_from_slice(&b[..k]),
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nX-Setura-Backend: orders-v1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        port
    }

    fn parse(args: &[&str]) -> Result<GatewayCommand, clap::Error> {
        let mut v = vec!["gateway"];
        v.extend_from_slice(args);
        T::try_parse_from(v).map(|t| t.c)
    }

    fn run_ok(cmd: GatewayCommand, f: &FakeKubectl) -> (i32, String) {
        let mut out = Vec::<u8>::new();
        let code = execute(cmd, f, &mut out).unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_command_definitions_are_consistent() {
        T::command().debug_assert();
    }

    #[test]
    fn arguments_parse_with_setura_defaults() {
        match parse(&["publish", "dev", "--listener", "web", "--dry-run"]).unwrap() {
            GatewayCommand::Publish {
                namespace,
                proxy_namespace,
                port,
                dry_run,
                ..
            } => {
                assert_eq!(
                    (namespace.as_str(), proxy_namespace.as_str(), port, dry_run),
                    ("default", "envoy-gateway-system", None, true)
                );
            }
            _ => panic!(),
        }
        match parse(&["explain", "http://x/", "--status", "404", "--json"]).unwrap() {
            GatewayCommand::Explain {
                status,
                timeout,
                json,
                ..
            } => assert_eq!((status, timeout, json), (404, 4.0, true)),
            _ => panic!(),
        }
        assert!(
            parse(&["publish", "dev"]).is_err(),
            "--listener is required"
        );
        assert!(parse(&["demo", "apply", "--dry-run", "--cluster", "dev"]).is_ok());
    }

    #[test]
    fn publish_dry_run_prints_the_command_and_nothing_is_forwarded() {
        let f = FakeKubectl::default()
            .json(serde_json::json!({"spec": {"listeners": [{"name": "web", "protocol": "HTTP", "port": 8080}]}}))
            .json(serde_json::json!({"items": [{"metadata": {"name": "envoy-demo-dev-1a2b"}, "spec": {"ports": [{"port": 8080}]}}]}));
        let cmd = parse(&[
            "publish",
            "dev",
            "-n",
            "demo",
            "--listener",
            "web",
            "--port",
            "9080",
            "--dry-run",
            "--kube-context",
            "lab",
        ])
        .unwrap();
        let (code, out) = run_ok(cmd, &f);
        assert_eq!(code, 0);
        assert_eq!(
            out.trim(),
            "kubectl --context lab -n envoy-gateway-system port-forward --address 127.0.0.1 service/envoy-demo-dev-1a2b 9080:8080"
        );
        // only reads: two gets, no port-forward
        assert!(f.argvs().iter().all(|a| a.contains(" get ")));
    }

    #[test]
    fn publish_without_a_port_picks_a_free_one_and_a_taken_port_is_refused() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = taken.local_addr().unwrap().port();
        let gw = serde_json::json!({"spec": {"listeners": [{"name": "web", "protocol": "HTTP", "port": 8080}]}});
        let svc = serde_json::json!({"items": [{"metadata": {"name": "envoy-x"}, "spec": {"ports": [{"port": 8080}]}}]});
        let f = FakeKubectl::default().json(gw.clone()).json(svc.clone());
        let (code, out) = run_ok(parse(&["publish", "dev", "--listener", "web", "--dry-run"]).unwrap(), &f);
        assert_eq!(code, 0);
        let local: u16 = out.trim().rsplit(' ').next().unwrap().split(':').next().unwrap().parse().unwrap();
        assert!(expose::port_is_free(local), "{out}");
        let f = FakeKubectl::default().json(gw).json(svc);
        let cmd = parse(&["publish", "dev", "--listener", "web", "--port", &busy.to_string()]).unwrap();
        let e = execute(cmd, &f, &mut Vec::<u8>::new()).unwrap_err().to_string();
        assert!(e.contains("already in use") && e.contains("--port"), "{e}");
        assert!(f.calls.borrow().is_empty(), "refused before any kubectl call");
    }

    #[test]
    fn expose_unexpose_and_list_parse() {
        match parse(&["expose", "web", "-n", "shop", "--port", "8000", "--host", "w.localhost", "--local-port", "9001", "--yes", "--dry-run", "--cluster", "dev"]).unwrap() {
            GatewayCommand::Expose { service, namespace, port, host, local_port, yes, dry_run, target } => {
                assert_eq!((service.as_str(), namespace.as_str(), port, local_port, yes, dry_run), ("web", "shop", Some(8000), Some(9001), true, true));
                assert_eq!(host.as_deref(), Some("w.localhost"));
                assert_eq!(target.cluster.as_deref(), Some("dev"));
            }
            _ => panic!(),
        }
        assert!(parse(&["unexpose", "web"]).is_ok() && parse(&["list"]).is_ok());
        assert!(parse(&["expose"]).is_err(), "SERVICE is required");
    }

    #[test]
    fn inspect_prints_json() {
        let route = serde_json::json!({"metadata": {"generation": 1}, "spec": {"rules": []}, "status": {"parents": []}});
        let f = FakeKubectl::default().json(route);
        let (code, out) = run_ok(parse(&["inspect", "orders", "-n", "demo"]).unwrap(), &f);
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["route"], "orders");
        assert_eq!(v["generation"], 1);
    }

    #[test]
    fn explain_exit_codes_and_output() {
        let port = serve(2);
        let url = format!("http://orders.mactra.test:{port}/checkout?q=1");
        let f = FakeKubectl::default();
        let (code, text) = run_ok(
            parse(&[
                "explain",
                &url,
                "--address",
                "127.0.0.1",
                "--backend",
                "orders-v1",
            ])
            .unwrap(),
            &f,
        );
        assert_eq!(code, 0, "{text}");
        assert!(text.starts_with("dns      overridden"));
        let (code, json) = run_ok(
            parse(&[
                "explain",
                &url,
                "--address",
                "127.0.0.1",
                "--status",
                "204",
                "--json",
            ])
            .unwrap(),
            &f,
        );
        assert_eq!(code, 1);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["steps"]["http"]["status"], 200);
        assert_eq!(v["steps"]["backend"]["state"], "unused");
        // usage error: exit 2 path is an Err from execute
        let mut out = Vec::<u8>::new();
        assert!(
            execute(
                parse(&["explain", "file:///etc/passwd"]).unwrap(),
                &f,
                &mut out
            )
            .is_err()
        );
    }

    #[test]
    fn check_exit_codes() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("cases.json");
        let f = FakeKubectl::default();
        for (status, expected) in [(200, 0), (418, 1)] {
            let port = serve(1);
            std::fs::write(&path, format!(r#"[{{"url": "http://orders.mactra.test:{port}/checkout?q=1", "address": "127.0.0.1", "status": {status}}}]"#)).unwrap();
            let (code, out) = run_ok(parse(&["check", path.to_str().unwrap()]).unwrap(), &f);
            assert_eq!(code, expected);
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v[0]["ok"], expected == 0);
        }
    }

    #[test]
    fn invalid_check_files_are_usage_errors() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("cases.json");
        for bad in [
            "[]",
            "{}",
            "not json",
            r#"[{"address": "127.0.0.1"}]"#,
            r#"[{"url": "http://h/", "bogus": 1}]"#,
            r#"[{"url": "http://h/", "status": "200"}]"#,
            r#"[{"url": "ftp://h/"}]"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(check(path.to_str().unwrap()).is_err(), "{bad}");
        }
        assert!(check("/nonexistent/cases.json").is_err());
    }

    #[test]
    fn demo_print_and_dry_run() {
        let f = FakeKubectl::default();
        let (_, manifest) = run_ok(parse(&["demo", "print"]).unwrap(), &f);
        assert!(manifest.contains("kind: HTTPRoute"));
        let (_, checks) = run_ok(parse(&["demo", "print", "--checks"]).unwrap(), &f);
        assert!(checks.contains("orders-v1"));
        let (code, out) = run_ok(
            parse(&["demo", "teardown", "--dry-run", "--kube-context", "lab"]).unwrap(),
            &f,
        );
        assert_eq!(code, 0);
        assert!(out.contains("kubectl --context lab delete -f -"));
        assert!(
            f.calls.borrow().is_empty(),
            "dry runs and print never call kubectl"
        );
    }
}
