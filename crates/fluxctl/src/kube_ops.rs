// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Day-two operations on a cluster, through the kubectl on this machine (no daemon needed), the same way `kube run` works:
//!
//! * `kube secrets set|ls|rm` and `kube config ls|set|rm`: Secrets and ConfigMaps. A secret value is read from a hidden prompt,
//!   stdin or a file, never from the command line, travels to kubectl on stdin as a server-side-apply manifest, and is never
//!   printed, logged or put in an error. `ls` asks the cluster for names and key names only.
//! * `kube certs`: the certificates Ingresses and Gateways serve, with expiry warnings and the one command that fixes each
//!   (`fluxvm_core::kube_view`, which Studio shares).
//! * `kube events`, `kube top`: what happened and what each pod and node uses; `top` says plainly when the cluster has no
//!   metrics-server and how to install one.
//!
//! The cluster is picked like `kube run` does: the only one `fluxctl kube up` made, `--cluster NAME` or `--kube-context`.
//! Every kubectl call goes through [`KubeRun`], so the tests use a fake and start nothing.

use crate::gateway::Target;
use crate::gateway::kube::{Cluster, KubeRun, SystemKubectl};
use crate::kube_app_cmd::{coded, kc, kerr, selection};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use clap::Subcommand;
use fluxvm_core::kube_app::name_ok;
use fluxvm_core::kube_view;
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

/// Kubernetes refuses an object over 1 MiB; refuse before sending it.
const MAX_VALUE: usize = 1024 * 1024;
const GO_NL: &str = "{{\"\\n\"}}";
const GO_TAB: &str = "{{\"\\t\"}}";

#[derive(Subcommand, Debug)]
pub enum OpsCommand {
    /// Secrets: `set` (value from a hidden prompt, stdin or a file, never the command line), `ls` (names only) and `rm`.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// ConfigMaps: `ls` (or show one), `set` and `rm`.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// The certificates your Ingresses and Gateways serve: when each ends, what is wrong, and the one command that fixes it.
    Certs {
        /// Warn when a certificate ends within this many days.
        #[arg(long, default_value_t = kube_view::WARN_DAYS)]
        warn_days: i64,
        /// Exit with an error when any certificate is expired, expiring, missing or unreadable (for scripts and CI).
        #[arg(long)]
        check: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: Target,
    },
    /// What happened in the cluster, newest last: the quickest way to see why a pod is not starting.
    Events {
        /// One namespace (default: all of them).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
        /// Only warnings.
        #[arg(long)]
        warnings: bool,
        /// Only events about one object, for example `pod/web-1` or `deploy/web`.
        #[arg(long = "for", value_name = "OBJECT")]
        r#for: Option<String>,
        /// How many to show.
        #[arg(long, default_value_t = 40)]
        limit: usize,
        #[command(flatten)]
        target: Target,
    },
    /// CPU and memory per pod or per node (needs a metrics-server, which k3s runs by default).
    Top {
        /// `pods` (default) or `nodes`.
        #[arg(default_value = "pods", value_parser = ["pods", "nodes"])]
        what: String,
        /// One namespace (default: all of them).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
        /// Sort by `cpu` or `memory`.
        #[arg(long, value_parser = ["cpu", "memory"])]
        sort_by: Option<String>,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug)]
pub enum SecretsCommand {
    /// Set one key of a Secret, creating it if needed: `fluxctl kube secrets set db password`. The value is asked for at a hidden
    /// prompt, or taken from stdin (`printf %s "$PW" | fluxctl kube secrets set db password`) or `--from-file`; it is never accepted
    /// as an argument, printed or logged.
    Set {
        /// The Secret's name.
        secret: String,
        /// The key inside it, such as `password`.
        key: String,
        #[arg(long, short = 'n', default_value = "default")]
        namespace: String,
        /// Read the value from this file (its bytes exactly).
        #[arg(long, value_name = "FILE")]
        from_file: Option<PathBuf>,
        #[command(flatten)]
        target: Target,
    },
    /// The Secrets and the names of their keys. Never the values.
    Ls {
        /// One namespace (default: all of them).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Remove a whole Secret, or one key of it.
    Rm {
        secret: String,
        /// Only this key (default: the whole Secret).
        key: Option<String>,
        #[arg(long, short = 'n', default_value = "default")]
        namespace: String,
        /// Do it; without this only what would be removed is shown.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// The ConfigMaps (names and keys), or one ConfigMap's keys and values.
    Ls {
        /// Show this one in full.
        map: Option<String>,
        /// One namespace (default: all of them when listing, `default` when showing one).
        #[arg(long, short = 'n')]
        namespace: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Set one key of a ConfigMap, creating it if needed: `kube config set app LOG_LEVEL=debug`, or `KEY` with the value on stdin or
    /// from `--from-file`.
    Set {
        map: String,
        /// `KEY=VALUE`, or just `KEY`.
        pair: String,
        #[arg(long, short = 'n', default_value = "default")]
        namespace: String,
        #[arg(long, value_name = "FILE")]
        from_file: Option<PathBuf>,
        #[command(flatten)]
        target: Target,
    },
    /// Remove a whole ConfigMap, or one key of it.
    Rm {
        map: String,
        key: Option<String>,
        #[arg(long, short = 'n', default_value = "default")]
        namespace: String,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
}

// ---- inputs ----------------------------------------------------------------------------------------------------------------

/// A key of a Secret or ConfigMap: letters, digits, `-`, `_` and `.`, up to 253 characters, and not `.` or `..`.
pub fn key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 253
        && k != "."
        && k != ".."
        && k.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if !name_ok(name) {
        bail!(
            "{name:?} is not a valid {what} name: lowercase letters, digits and dashes, up to 63 characters"
        );
    }
    Ok(())
}

fn check_key(key: &str, secret: bool) -> Result<()> {
    if secret && key.contains('=') {
        return Err(coded(
            "KUBE_VALUE_MISSING",
            "The value is not taken on the command line (your shell history and `ps` would keep it): give just the key, and the value comes from a hidden prompt, stdin or --from-file",
        ));
    }
    if !key_ok(key) {
        bail!(
            "{key:?} is not a valid key: letters, digits, `-`, `_` and `.` only, up to 253 characters"
        );
    }
    Ok(())
}

/// What stdin or a prompt gave, without its line ending; and whether it is acceptable at all.
pub fn finish_value(mut raw: Vec<u8>, from_file: bool) -> Result<Vec<u8>> {
    if !from_file {
        if raw.ends_with(b"\n") {
            raw.pop();
        }
        if raw.ends_with(b"\r") {
            raw.pop();
        }
    }
    if raw.is_empty() {
        return Err(coded(
            "KUBE_VALUE_MISSING",
            "No value was given: type it at the prompt, pipe it in, or use --from-file",
        ));
    }
    if raw.len() > MAX_VALUE {
        bail!(
            "that value is {} bytes; Kubernetes keeps at most 1 MiB in one object",
            raw.len()
        );
    }
    Ok(raw)
}

/// Switches the terminal's echo off for the lifetime of the guard (so a typed secret is not shown), and back on after.
struct NoEcho(libc::termios);

impl NoEcho {
    fn enter() -> Option<Self> {
        // SAFETY: plain termios calls on fd 0 with a zeroed struct that tcgetattr fills.
        unsafe {
            if libc::isatty(0) == 0 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            t.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(0, libc::TCSAFLUSH, &t) != 0 {
                return None;
            }
            Some(Self(orig))
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `enter`.
        unsafe {
            libc::tcsetattr(0, libc::TCSAFLUSH, &self.0);
        }
    }
}

/// The value for a `set`: a file's bytes, a prompt when stdin is a terminal (hidden for a secret), else all of stdin.
fn read_value(from_file: Option<&Path>, label: &str, hidden: bool) -> Result<Vec<u8>> {
    if let Some(p) = from_file {
        let len = std::fs::metadata(p)
            .with_context(|| format!("reading {}", p.display()))?
            .len();
        if len > MAX_VALUE as u64 {
            bail!(
                "{} is {len} bytes; Kubernetes keeps at most 1 MiB in one object",
                p.display()
            );
        }
        let raw = std::fs::read(p).with_context(|| format!("reading {}", p.display()))?;
        return finish_value(raw, true);
    }
    let mut stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("{label}: ");
        let _ = std::io::stderr().flush();
        let guard = if hidden { NoEcho::enter() } else { None };
        let mut line = String::new();
        let read = stdin.read_line(&mut line);
        drop(guard);
        if hidden {
            eprintln!();
        }
        read.context("reading the value")?;
        return finish_value(line.into_bytes(), false);
    }
    let mut raw = Vec::new();
    stdin
        .by_ref()
        .take(MAX_VALUE as u64 + 1)
        .read_to_end(&mut raw)
        .context("reading the value from stdin")?;
    finish_value(raw, false)
}

// ---- pure builders and parsers -------------------------------------------------------------------------------------------

/// The manifest that sets one key of a Secret. Server-side apply with our own field manager only touches this key, so the Secret's
/// other keys (and its type) are left alone, and the Secret is created when missing.
pub fn secret_manifest(ns: &str, name: &str, key: &str, value: &[u8]) -> Value {
    json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name, "namespace": ns},
        "data": {key: base64::engine::general_purpose::STANDARD.encode(value)}})
}

/// Same for a ConfigMap; a value that is not text goes in `binaryData`.
pub fn config_manifest(ns: &str, name: &str, key: &str, value: &[u8]) -> Value {
    let (field, v) = match std::str::from_utf8(value) {
        Ok(t) => ("data", t.to_owned()),
        Err(_) => (
            "binaryData",
            base64::engine::general_purpose::STANDARD.encode(value),
        ),
    };
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": ns}, field: {key: v}})
}

const APPLY: [&str; 5] = [
    "apply",
    "--server-side",
    "--force-conflicts",
    "--field-manager=fluxctl",
    "-f",
];

/// A kubectl error with every trace of a value taken out (raw and base64), so a refusal that echoes the object cannot leak it.
pub fn scrub(text: &str, value: &[u8]) -> String {
    let mut out = text.to_owned();
    let b64 = base64::engine::general_purpose::STANDARD.encode(value);
    for needle in [b64, String::from_utf8_lossy(value).into_owned()] {
        if needle.len() >= 4 {
            out = out.replace(&needle, "<value hidden>");
        }
    }
    out
}

/// The go-template that prints one tab-separated line per Secret or ConfigMap: namespace, name, type, key names, created.
/// Values stay in the cluster.
fn list_template(kind: &str) -> String {
    let typ = if kind == "secrets" { "{{.type}}" } else { "-" };
    format!(
        "{{{{range .items}}}}{{{{.metadata.namespace}}}}{GO_TAB}{{{{.metadata.name}}}}{GO_TAB}{typ}{GO_TAB}{{{{range $k,$v := .data}}}}{{{{$k}}}},{{{{end}}}}{{{{range $k,$v := .binaryData}}}}{{{{$k}}}},{{{{end}}}}{GO_TAB}{{{{.metadata.creationTimestamp}}}}{GO_NL}{{{{end}}}}"
    )
}

fn tls_template() -> String {
    format!(
        "{{{{range .items}}}}{{{{.metadata.namespace}}}}{GO_TAB}{{{{.metadata.name}}}}{GO_TAB}{{{{index .data \"tls.crt\"}}}}{GO_NL}{{{{end}}}}"
    )
}

/// One row of the list template's output.
#[derive(Debug, PartialEq, Eq)]
pub struct ListRow {
    pub namespace: String,
    pub name: String,
    pub kind: String,
    pub keys: Vec<String>,
    pub created: String,
}

pub fn parse_list(text: &str, hide_helm: bool) -> Vec<ListRow> {
    let mut rows: Vec<ListRow> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 5 {
                return None;
            }
            let kind = if f[2] == "<no value>" { "Opaque" } else { f[2] };
            if hide_helm && kind == "helm.sh/release.v1" {
                return None;
            }
            let mut keys: Vec<String> = f[3]
                .split(',')
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .collect();
            keys.sort();
            Some(ListRow {
                namespace: f[0].into(),
                name: f[1].into(),
                kind: kind.into(),
                keys,
                created: f[4].into(),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    rows
}

fn age(created: &str, now: i64) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(created) else {
        return "-".into();
    };
    crate::kube_ps::ago(now - t.timestamp())
}

pub fn render_list(rows: &[ListRow], with_type: bool, now: i64) -> String {
    if rows.is_empty() {
        return "none\n".into();
    }
    let mut out = if with_type {
        format!(
            "{:<14} {:<32} {:<28} {:<9} KEYS\n",
            "NAMESPACE", "NAME", "TYPE", "AGE"
        )
    } else {
        format!("{:<14} {:<32} {:<9} KEYS\n", "NAMESPACE", "NAME", "AGE")
    };
    for r in rows {
        let keys = if r.keys.is_empty() {
            "-".to_owned()
        } else {
            r.keys.join(",")
        };
        let a = age(&r.created, now);
        if with_type {
            out.push_str(&format!(
                "{:<14} {:<32} {:<28} {:<9} {keys}\n",
                r.namespace, r.name, r.kind, a
            ));
        } else {
            out.push_str(&format!(
                "{:<14} {:<32} {:<9} {keys}\n",
                r.namespace, r.name, a
            ));
        }
    }
    out
}

/// `parse_tls_lines`: `namespace<TAB>name<TAB>base64 certificate` lines to TLS secrets.
pub fn parse_tls_lines(text: &str) -> Vec<kube_view::TlsSecret> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(kube_view::TlsSecret {
                namespace: f.next()?.into(),
                name: f.next()?.into(),
                crt_b64: f.next()?.trim().into(),
            })
        })
        .collect()
}

pub fn render_certs(rows: &[kube_view::CertRow], warn_days: i64) -> String {
    if rows.is_empty() {
        return "No Ingress or Gateway in this cluster serves a certificate from a Secret.\n"
            .into();
    }
    let bad = rows.iter().filter(|r| r.state != "ok").count();
    let n = format!(
        "{} certificate{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    let mut out = if bad == 0 {
        format!("{n}, all valid for more than {warn_days} days\n\n")
    } else {
        format!("{n}, {bad} need attention\n\n")
    };
    for r in rows {
        let ends = r
            .not_after
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{:<9} {}/{}  {}\n          {} (ends {ends}); used by {}\n",
            r.state.to_uppercase(),
            r.namespace,
            r.secret,
            if r.hosts.is_empty() {
                "-".to_owned()
            } else {
                r.hosts.join(", ")
            },
            r.summary,
            r.used_by.join(", ")
        ));
        if !r.fix.is_empty() {
            out.push_str(&format!("          fix: {}\n", r.fix));
        }
    }
    if bad > 0 {
        out.push_str("\nmore: fluxctl explain KUBE_CERT_PROBLEM\n");
    }
    out
}

/// Keeps the header and the last `limit` lines.
pub fn tail_lines(text: &str, limit: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= limit + 1 {
        return text.to_owned();
    }
    let mut out = vec![lines[0]];
    out.extend(&lines[lines.len() - limit..]);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// True when what kubectl printed means the cluster has no metrics API.
pub fn metrics_missing(stderr: &str) -> bool {
    let low = stderr.to_ascii_lowercase();
    [
        "metrics api not available",
        "metrics not available",
        "metrics.k8s.io",
        "the server is currently unable to handle the request",
        "service unavailable",
    ]
    .iter()
    .any(|m| low.contains(m))
}

pub const METRICS_HELP: &str = "This cluster has no metrics-server (or it has not collected anything yet), so there are no CPU and memory numbers.\n  k3s runs one by default: if the cluster started a minute ago, wait and try again.\n  To install one: kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml\n  (a cluster started with `--disable metrics-server` needs that).";

// ---- running -----------------------------------------------------------------------------------------------------------------

fn ns_args(ns: Option<&str>) -> Result<Vec<&str>> {
    match ns {
        Some(n) => {
            check_name("namespace", n)?;
            Ok(vec!["-n", n])
        }
        None => Ok(vec!["-A"]),
    }
}

fn run_kc(
    c: &Cluster,
    what: &str,
    rest: &[&str],
    stdin: Option<&[u8]>,
    scrub_with: &[u8],
) -> Result<String> {
    let r = kc(c, None, rest, stdin, 60)?;
    if r.code != 0 {
        let low = r.stderr.to_ascii_lowercase();
        let e = kerr(what, &r);
        let msg = scrub(&format!("{e:#}"), scrub_with);
        if low.contains("namespaces \"") && low.contains("not found") {
            return Err(anyhow!(
                "{what} failed: that namespace does not exist. List them with `kubectl get namespaces`"
            ));
        }
        return Err(anyhow!("{msg}"));
    }
    Ok(r.stdout)
}

fn describe(kind: &str, ns: &str, name: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => format!("key {k} of the {kind} {ns}/{name}"),
        None => format!("the {kind} {ns}/{name}"),
    }
}

fn set_key(
    c: &Cluster,
    kind: &str,
    ns: &str,
    name: &str,
    key: &str,
    value: &[u8],
    out: &mut dyn Write,
) -> Result<()> {
    let manifest = if kind == "Secret" {
        secret_manifest(ns, name, key, value)
    } else {
        config_manifest(ns, name, key, value)
    };
    let body = serde_json::to_vec(&manifest)?;
    let mut rest: Vec<&str> = APPLY.to_vec();
    rest.push("-");
    run_kc(
        c,
        &format!("setting {ns}/{name} {key}"),
        &rest,
        Some(&body),
        value,
    )?;
    writeln!(
        out,
        "{kind} {ns}/{name}: key {key} set ({} bytes)",
        value.len()
    )?;
    Ok(())
}

fn remove(
    c: &Cluster,
    resource: &str,
    kind: &str,
    ns: &str,
    name: &str,
    key: Option<&str>,
    yes: bool,
    out: &mut dyn Write,
) -> Result<()> {
    check_name("namespace", ns)?;
    check_name(kind, name)?;
    if let Some(k) = key
        && !key_ok(k)
    {
        bail!("{k:?} is not a valid key");
    }
    let what = describe(kind, ns, name, key);
    if !yes {
        return Err(coded(
            "KUBE_NEEDS_YES",
            format!("This would remove {what}; add --yes to do it"),
        ));
    }
    match key {
        None => {
            run_kc(
                c,
                &format!("removing {what}"),
                &["-n", ns, "delete", resource, name],
                None,
                b"",
            )?;
        }
        Some(k) => {
            let patch = json!([{"op": "remove", "path": format!("/data/{k}")}]).to_string();
            let r = kc(
                c,
                None,
                &[
                    "-n",
                    ns,
                    "patch",
                    resource,
                    name,
                    "--type=json",
                    "-p",
                    &patch,
                ],
                None,
                60,
            )?;
            if r.code != 0 {
                if r.stderr.contains("does not apply") || r.stderr.contains("missing key") {
                    bail!(
                        "{ns}/{name} has no key {k}. See its keys with `fluxctl kube {} ls -n {ns}`",
                        if kind == "Secret" {
                            "secrets"
                        } else {
                            "config"
                        }
                    );
                }
                return Err(kerr(&format!("removing {what}"), &r));
            }
        }
    }
    writeln!(out, "removed {what}")?;
    Ok(())
}

pub fn secrets_ls(
    c: &Cluster,
    ns: Option<&str>,
    json: bool,
    now: i64,
    out: &mut dyn Write,
) -> Result<()> {
    let mut rest = vec!["get", "secrets"];
    rest.extend(ns_args(ns)?);
    let tpl = format!("-o=go-template={}", list_template("secrets"));
    rest.push(&tpl);
    let text = run_kc(c, "listing Secrets", &rest, None, b"")?;
    let rows = parse_list(&text, true);
    if json {
        let v: Vec<Value> = rows.iter().map(|r| json!({"namespace": r.namespace, "name": r.name, "type": r.kind, "keys": r.keys, "created": r.created})).collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
    } else {
        write!(out, "{}", render_list(&rows, true, now))?;
        if !rows.is_empty() {
            writeln!(
                out,
                "\nValues are never shown. Set one: fluxctl kube secrets set NAME KEY"
            )?;
        }
    }
    Ok(())
}

pub fn config_ls(
    c: &Cluster,
    map: Option<&str>,
    ns: Option<&str>,
    json: bool,
    now: i64,
    out: &mut dyn Write,
) -> Result<()> {
    if let Some(name) = map {
        let ns = ns.unwrap_or("default");
        check_name("namespace", ns)?;
        check_name("ConfigMap", name)?;
        let text = run_kc(
            c,
            &format!("reading ConfigMap {ns}/{name}"),
            &["-n", ns, "get", "configmap", name, "-o", "json"],
            None,
            b"",
        )?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("kubectl answered something that is not JSON: {e}"))?;
        if json {
            let bin: Vec<&String> = v["binaryData"]
                .as_object()
                .map(|m| m.keys().collect())
                .unwrap_or_default();
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&json!({"data": v["data"], "binaryData": bin}))?
            )?;
            return Ok(());
        }
        let mut any = false;
        for (k, val) in v["data"].as_object().into_iter().flatten() {
            any = true;
            writeln!(out, "{k}={}", val.as_str().unwrap_or(""))?;
        }
        for k in v["binaryData"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.keys())
        {
            any = true;
            writeln!(out, "{k}=<binary>")?;
        }
        if !any {
            writeln!(out, "{ns}/{name} has no keys")?;
        }
        return Ok(());
    }
    let mut rest = vec!["get", "configmaps"];
    rest.extend(ns_args(ns)?);
    let tpl = format!("-o=go-template={}", list_template("configmaps"));
    rest.push(&tpl);
    let rows = parse_list(&run_kc(c, "listing ConfigMaps", &rest, None, b"")?, false);
    if json {
        let v: Vec<Value> = rows.iter().map(|r| json!({"namespace": r.namespace, "name": r.name, "keys": r.keys, "created": r.created})).collect();
        writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
    } else {
        write!(out, "{}", render_list(&rows, false, now))?;
    }
    Ok(())
}

pub fn certs(
    c: &Cluster,
    warn_days: i64,
    check: bool,
    json: bool,
    now: i64,
    out: &mut dyn Write,
) -> Result<()> {
    if !(0..=3650).contains(&warn_days) {
        bail!("--warn-days is 0 to 3650");
    }
    let ingresses = run_kc(
        c,
        "reading Ingresses",
        &["get", "ingress", "-A", "-o", "json"],
        None,
        b"",
    )?;
    // A cluster with no Gateway API installed has no `gateways` resource: that only means no Gateway certificates.
    let gateways = kc(
        c,
        None,
        &[
            "get",
            "gateways.gateway.networking.k8s.io",
            "-A",
            "-o",
            "json",
        ],
        None,
        60,
    )
    .ok()
    .filter(|r| r.code == 0)
    .map(|r| r.stdout)
    .unwrap_or_default();
    let tpl = format!("-o=go-template={}", tls_template());
    let secrets = run_kc(
        c,
        "reading TLS Secrets",
        &[
            "get",
            "secrets",
            "-A",
            "--field-selector",
            "type=kubernetes.io/tls",
            &tpl,
        ],
        None,
        b"",
    )?;
    let parse = |t: &str| serde_json::from_str::<Value>(t).unwrap_or_else(|_| json!({}));
    let rows = kube_view::certs(
        &parse(&ingresses),
        &parse(&gateways),
        &parse_tls_lines(&secrets),
        now,
        warn_days,
    );
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
    } else {
        write!(out, "{}", render_certs(&rows, warn_days))?;
    }
    if check && rows.iter().any(|r| r.state != "ok") {
        return Err(coded(
            "KUBE_CERT_PROBLEM",
            "A certificate needs attention: see the list above",
        ));
    }
    Ok(())
}

pub fn events(
    c: &Cluster,
    ns: Option<&str>,
    warnings: bool,
    target: Option<&str>,
    limit: usize,
    out: &mut dyn Write,
) -> Result<()> {
    let mut rest = vec!["get", "events", "--sort-by=.lastTimestamp"];
    rest.extend(ns_args(ns)?);
    if warnings {
        rest.push("--field-selector=type=Warning");
    }
    let obj;
    if let Some(t) = target {
        let (kind, name) = t.split_once('/').unwrap_or(("pod", t));
        if !name_ok(name) || kind.is_empty() || !kind.bytes().all(|c| c.is_ascii_alphabetic()) {
            bail!("--for takes KIND/NAME, for example pod/web-1 or deploy/web");
        }
        obj = format!("{kind}/{name}");
        rest.extend(["--for", &obj]);
    }
    let text = run_kc(c, "reading events", &rest, None, b"")?;
    if text.trim().is_empty() {
        writeln!(
            out,
            "No events: Kubernetes keeps them for about an hour, and nothing has happened since."
        )?;
    } else {
        write!(out, "{}", tail_lines(&text, limit.max(1)))?;
    }
    Ok(())
}

pub fn top(
    c: &Cluster,
    what: &str,
    ns: Option<&str>,
    sort: Option<&str>,
    out: &mut dyn Write,
) -> Result<()> {
    let mut rest = vec!["top", what];
    if what == "pods" {
        rest.extend(ns_args(ns)?);
    }
    let s;
    if let Some(k) = sort {
        if !matches!(k, "cpu" | "memory") {
            bail!("--sort-by is cpu or memory");
        }
        s = format!("--sort-by={k}");
        rest.push(&s);
    }
    let r = kc(c, None, &rest, None, 60)?;
    if r.code != 0 {
        if metrics_missing(&r.stderr) {
            return Err(coded("KUBE_METRICS_UNAVAILABLE", METRICS_HELP));
        }
        return Err(kerr(&format!("kubectl top {what}"), &r));
    }
    write!(out, "{}", r.stdout)?;
    Ok(())
}

pub fn run(cmd: OpsCommand) -> i32 {
    match execute(cmd, &SystemKubectl, &read_value, &mut std::io::stdout()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fluxctl kube: {e:#}");
            1
        }
    }
}

/// Where a `set` takes its value from (the real one prompts, reads stdin or a file; tests give bytes).
pub type ValueSource = dyn Fn(Option<&Path>, &str, bool) -> Result<Vec<u8>>;

pub fn execute(
    cmd: OpsCommand,
    runner: &dyn KubeRun,
    input: &ValueSource,
    out: &mut dyn Write,
) -> Result<i32> {
    let now = chrono::Utc::now().timestamp();
    let cluster = |t: &Target| -> Result<Vec<String>> { Ok(selection(t)?.1) };
    match cmd {
        OpsCommand::Secrets { command } => match command {
            SecretsCommand::Set {
                secret,
                key,
                namespace,
                from_file,
                target,
            } => {
                check_name("Secret", &secret)?;
                check_name("namespace", &namespace)?;
                check_key(&key, true)?;
                let value = input(
                    from_file.as_deref(),
                    &format!("Value for {namespace}/{secret} {key} (hidden)"),
                    true,
                )?;
                let c = Cluster::new(cluster(&target)?, runner);
                set_key(&c, "Secret", &namespace, &secret, &key, &value, out)?;
            }
            SecretsCommand::Ls {
                namespace,
                json,
                target,
            } => {
                let c = Cluster::new(cluster(&target)?, runner);
                secrets_ls(&c, namespace.as_deref(), json, now, out)?;
            }
            SecretsCommand::Rm {
                secret,
                key,
                namespace,
                yes,
                target,
            } => {
                let c = Cluster::new(cluster(&target)?, runner);
                remove(
                    &c,
                    "secret",
                    "Secret",
                    &namespace,
                    &secret,
                    key.as_deref(),
                    yes,
                    out,
                )?;
            }
        },
        OpsCommand::Config { command } => match command {
            ConfigCommand::Ls {
                map,
                namespace,
                json,
                target,
            } => {
                let c = Cluster::new(cluster(&target)?, runner);
                config_ls(&c, map.as_deref(), namespace.as_deref(), json, now, out)?;
            }
            ConfigCommand::Set {
                map,
                pair,
                namespace,
                from_file,
                target,
            } => {
                check_name("ConfigMap", &map)?;
                check_name("namespace", &namespace)?;
                let (key, value) = match pair.split_once('=') {
                    Some((k, v)) => {
                        check_key(k, false)?;
                        (k.to_owned(), finish_value(v.as_bytes().to_vec(), true)?)
                    }
                    None => {
                        check_key(&pair, false)?;
                        (
                            pair.clone(),
                            input(
                                from_file.as_deref(),
                                &format!("Value for {namespace}/{map} {pair}"),
                                false,
                            )?,
                        )
                    }
                };
                let c = Cluster::new(cluster(&target)?, runner);
                set_key(&c, "ConfigMap", &namespace, &map, &key, &value, out)?;
            }
            ConfigCommand::Rm {
                map,
                key,
                namespace,
                yes,
                target,
            } => {
                let c = Cluster::new(cluster(&target)?, runner);
                remove(
                    &c,
                    "configmap",
                    "ConfigMap",
                    &namespace,
                    &map,
                    key.as_deref(),
                    yes,
                    out,
                )?;
            }
        },
        OpsCommand::Certs {
            warn_days,
            check,
            json,
            target,
        } => {
            let c = Cluster::new(cluster(&target)?, runner);
            certs(&c, warn_days, check, json, now, out)?;
        }
        OpsCommand::Events {
            namespace,
            warnings,
            r#for,
            limit,
            target,
        } => {
            let c = Cluster::new(cluster(&target)?, runner);
            events(
                &c,
                namespace.as_deref(),
                warnings,
                r#for.as_deref(),
                limit,
                out,
            )?;
        }
        OpsCommand::Top {
            what,
            namespace,
            sort_by,
            target,
        } => {
            let c = Cluster::new(cluster(&target)?, runner);
            top(&c, &what, namespace.as_deref(), sort_by.as_deref(), out)?;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::kube::fake::FakeKubectl;

    /// A value built at run time, so no secret-looking literal sits in the source.
    fn word() -> String {
        ["pw", "-", &"x".repeat(9)].concat()
    }

    fn cl(f: &FakeKubectl) -> Cluster<'_> {
        Cluster::new(vec!["kubectl".into()], f)
    }

    #[test]
    fn keys_and_values_are_checked() {
        assert!(key_ok("tls.crt") && key_ok("A_b-1"));
        for bad in ["", ".", "..", "a/b", "a b", "a=b"] {
            assert!(!key_ok(bad), "{bad}");
        }
        assert_eq!(finish_value(b"abc\r\n".to_vec(), false).unwrap(), b"abc");
        assert_eq!(finish_value(b"a\n".to_vec(), true).unwrap(), b"a\n");
        let e = finish_value(b"\n".to_vec(), false).unwrap_err().to_string();
        assert!(e.contains("KUBE_VALUE_MISSING"), "{e}");
        assert!(finish_value(vec![b'a'; MAX_VALUE + 1], true).is_err());
    }

    #[test]
    fn a_secret_value_is_never_on_the_command_line_and_set_applies_it_on_stdin() {
        let v = word();
        let f = FakeKubectl::default().text(0, "", "");
        let mut out = vec![];
        set_key(
            &cl(&f),
            "Secret",
            "shop",
            "db",
            "password",
            v.as_bytes(),
            &mut out,
        )
        .unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("key password set (12 bytes)"), "{said}");
        assert!(!said.contains(&v));
        let calls = f.calls.borrow();
        let (argv, stdin) = &calls[0];
        assert!(!argv.join(" ").contains(&v), "{argv:?}");
        assert!(argv.join(" ").contains("apply --server-side"), "{argv:?}");
        let body: Value = serde_json::from_str(stdin.as_ref().unwrap()).unwrap();
        assert_eq!(body["kind"], "Secret");
        assert_eq!(
            body["data"]["password"],
            base64::engine::general_purpose::STANDARD.encode(&v)
        );
    }

    #[test]
    fn a_failure_that_echoes_the_value_is_scrubbed() {
        let v = word();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&v);
        let echo = format!("Invalid value: \"{b64}\" and also {v}");
        let f = FakeKubectl::default().text(1, "", &echo);
        let e = set_key(
            &cl(&f),
            "Secret",
            "shop",
            "db",
            "password",
            v.as_bytes(),
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(!e.contains(&v) && !e.contains(&b64), "{e}");
        assert!(e.contains("<value hidden>"), "{e}");
    }

    #[test]
    fn a_missing_namespace_is_said_plainly() {
        let f = FakeKubectl::default().text(
            1,
            "",
            "Error from server (NotFound): namespaces \"nope\" not found",
        );
        let e = set_key(&cl(&f), "Secret", "nope", "db", "k", b"abcd", &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("namespace does not exist"), "{e}");
    }

    #[test]
    fn a_key_with_equals_is_refused_with_its_code() {
        let e = check_key("password=hunter22", true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("KUBE_VALUE_MISSING"), "{e}");
        assert!(check_key("a=b", false).is_err());
    }

    #[test]
    fn config_manifest_puts_binary_in_binarydata() {
        let m = config_manifest("a", "b", "k", b"text");
        assert_eq!(m["data"]["k"], "text");
        let m = config_manifest("a", "b", "k", &[0xff, 0xfe]);
        assert!(m["data"].is_null() && m["binaryData"]["k"].is_string());
    }

    #[test]
    fn listing_secrets_shows_names_and_keys_only() {
        let v = word();
        let text = "shop\tdb\tOpaque\tpassword,user,\t2026-01-01T00:00:00Z\nkube-system\tsh.helm.release.v1.x\thelm.sh/release.v1\trelease,\t2026-01-01T00:00:00Z\nshop\tplain\t<no value>\t\t-\n";
        let f = FakeKubectl::default().text(0, text, "");
        let mut out = vec![];
        secrets_ls(&cl(&f), None, false, 1_800_000_000, &mut out).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("db") && said.contains("password,user") && !said.contains("helm"),
            "{said}"
        );
        assert!(!said.contains(&v));
        let argv = &f.argvs()[0];
        assert!(
            argv.contains("get secrets -A") && argv.contains("go-template"),
            "{argv}"
        );
        assert!(!argv.contains("-o json"), "{argv}");
        let rows = parse_list(text, true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].kind, "Opaque");
    }

    #[test]
    fn rm_needs_yes_then_deletes_or_patches_one_key() {
        let f = FakeKubectl::default();
        let e = remove(
            &cl(&f),
            "secret",
            "Secret",
            "shop",
            "db",
            None,
            false,
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("KUBE_NEEDS_YES") && e.contains("add --yes"),
            "{e}"
        );
        assert!(f.calls.borrow().is_empty());
        let f = FakeKubectl::default().text(0, "", "").text(0, "", "");
        let mut out = vec![];
        remove(
            &cl(&f),
            "secret",
            "Secret",
            "shop",
            "db",
            None,
            true,
            &mut out,
        )
        .unwrap();
        remove(
            &cl(&f),
            "secret",
            "Secret",
            "shop",
            "db",
            Some("user"),
            true,
            &mut out,
        )
        .unwrap();
        let a = f.argvs();
        assert!(a[0].ends_with("-n shop delete secret db"), "{a:?}");
        assert!(
            a[1].contains("patch secret db --type=json") && a[1].contains("/data/user"),
            "{a:?}"
        );
        let f = FakeKubectl::default().text(
            1,
            "",
            "the server rejected: remove operation does not apply: doc is missing key",
        );
        let e = remove(
            &cl(&f),
            "secret",
            "Secret",
            "shop",
            "db",
            Some("nokey"),
            true,
            &mut vec![],
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("has no key nokey"), "{e}");
    }

    #[test]
    fn config_ls_shows_one_map_with_values() {
        let f = FakeKubectl::default()
            .json(json!({"data": {"LOG": "debug"}, "binaryData": {"blob": "AAE="}}));
        let mut out = vec![];
        config_ls(&cl(&f), Some("app"), None, false, 0, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "LOG=debug\nblob=<binary>\n"
        );
        assert!(
            f.argvs()[0].contains("-n default get configmap app"),
            "{:?}",
            f.argvs()
        );
    }

    #[test]
    fn certs_reads_three_things_and_check_fails_on_a_problem() {
        let ing = json!({"items": [{"metadata": {"namespace": "shop", "name": "web"}, "spec": {"tls": [{"hosts": ["a.test"], "secretName": "ghost"}]}}]});
        let f = FakeKubectl::default()
            .json(ing.clone())
            .text(
                1,
                "",
                "the server doesn't have a resource type \"gateways\"",
            )
            .text(0, "", "");
        let mut out = vec![];
        let e = certs(&cl(&f), 30, true, false, 1_800_000_000, &mut out)
            .unwrap_err()
            .to_string();
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("MISSING")
                && said.contains("fix: kubectl -n shop create secret tls ghost"),
            "{said}"
        );
        assert!(e.contains("KUBE_CERT_PROBLEM"), "{e}");
        let f = FakeKubectl::default()
            .json(json!({"items": []}))
            .text(0, "", "")
            .text(0, "", "");
        let mut out = vec![];
        certs(&cl(&f), 30, true, false, 0, &mut out).unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("No Ingress or Gateway")
        );
    }

    #[test]
    fn events_tail_and_filters() {
        let text = "NAMESPACE LAST SEEN TYPE\n".to_owned()
            + &(0..10).map(|i| format!("ns e{i}\n")).collect::<String>();
        let f = FakeKubectl::default().text(0, &text, "");
        let mut out = vec![];
        events(&cl(&f), Some("shop"), true, Some("deploy/web"), 3, &mut out).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(said.lines().count(), 4, "{said}");
        assert!(said.contains("e9") && !said.contains("e6"));
        let a = &f.argvs()[0];
        assert!(
            a.contains("-n shop") && a.contains("type=Warning") && a.contains("--for deploy/web"),
            "{a}"
        );
        assert!(
            events(
                &cl(&FakeKubectl::default()),
                None,
                false,
                Some("bad name"),
                5,
                &mut vec![]
            )
            .is_err()
        );
        let f = FakeKubectl::default().text(0, "", "");
        let mut out = vec![];
        events(&cl(&f), None, false, None, 5, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("No events"));
    }

    #[test]
    fn top_says_plainly_when_there_is_no_metrics_server() {
        let f = FakeKubectl::default().text(1, "", "error: Metrics API not available");
        let e = top(&cl(&f), "pods", None, None, &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no metrics-server")
                && e.contains("components.yaml")
                && e.contains("KUBE_METRICS_UNAVAILABLE"),
            "{e}"
        );
        let f = FakeKubectl::default().text(0, "NAME CPU\nweb 1m\n", "");
        let mut out = vec![];
        top(&cl(&f), "pods", Some("shop"), Some("cpu"), &mut out).unwrap();
        assert!(
            f.argvs()[0].contains("top pods -n shop --sort-by=cpu"),
            "{:?}",
            f.argvs()
        );
        assert!(
            top(
                &cl(&FakeKubectl::default()),
                "pods",
                None,
                Some("disk"),
                &mut vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn an_unreachable_cluster_is_the_usual_sentence() {
        let f = FakeKubectl::default().text(
            1,
            "",
            "Unable to connect to the server: dial tcp: connection refused",
        );
        let e = top(&cl(&f), "nodes", None, None, &mut vec![])
            .unwrap_err()
            .to_string();
        assert!(e.contains("APP_NO_CLUSTER"), "{e}");
    }
}
