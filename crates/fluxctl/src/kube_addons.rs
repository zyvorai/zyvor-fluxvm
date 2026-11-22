// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl kube addons ...`: complementary k3s configuration that is *generated*, not performed. Everything that
//! decides what goes into `/etc/rancher/k3s/config.yaml` (add-on toggles, Cilium, node labels/taints/reservations,
//! embedded registry, hardening) is a pure function that returns text, tested against golden
//! strings. The only side effects are behind [`GuestOps`], which has an SSH implementation and a test fake.
//!
//! This does not create, join, upgrade or back up a cluster; that lifecycle lives in the other `kube_*` modules.

use anyhow::{Context, Result, bail};
use serde_yaml::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const K3S_CONFIG_PATH: &str = "/etc/rancher/k3s/config.yaml";

// ---------------------------------------------------------------- guest access

/// What the generators need from a machine. The real one is SSH; tests use [`FakeGuest`].
pub trait GuestOps {
    /// Runs a shell command and returns its stdout.
    fn run(&self, cmd: &str) -> Result<String>;
    /// Writes a root-owned file with the given octal mode, creating its directory.
    fn write(&self, path: &str, content: &str, mode: &str) -> Result<()>;
    /// Copies a local file to a path on the guest (root-owned).
    fn upload(&self, local: &Path, remote: &str) -> Result<()>;
}

/// `ssh`/`scp` to `[user@]host`, using the user's own ssh config and agent.
pub struct SshGuest {
    pub target: String,
    pub identity: Option<PathBuf>,
}

impl SshGuest {
    pub fn new(target: String) -> Self {
        SshGuest {
            target,
            identity: None,
        }
    }

    fn ssh(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(k) = &self.identity {
            c.arg("-i").arg(k);
        }
        c.arg(&self.target);
        c
    }
}

impl GuestOps for SshGuest {
    fn run(&self, cmd: &str) -> Result<String> {
        let out = self.ssh().arg(cmd).output().context("running ssh")?;
        if !out.status.success() {
            bail!(
                "`{cmd}` failed on {}: {}",
                self.target,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn write(&self, path: &str, content: &str, mode: &str) -> Result<()> {
        use std::io::Write;
        let dir = path
            .rsplit_once('/')
            .map_or("/", |(d, _)| if d.is_empty() { "/" } else { d });
        let cmd = format!(
            "sudo mkdir -p {} && sudo install -m {} /dev/stdin {}",
            sh_quote(dir),
            sh_quote(mode),
            sh_quote(path)
        );
        let mut child = self
            .ssh()
            .arg(cmd)
            .stdin(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        child
            .stdin
            .take()
            .context("ssh stdin")?
            .write_all(content.as_bytes())?;
        if !child.wait()?.success() {
            bail!("writing {path} on {} failed", self.target);
        }
        Ok(())
    }

    fn upload(&self, local: &Path, remote: &str) -> Result<()> {
        let tmp = format!("/tmp/fluxvm-upload-{}", std::process::id());
        let mut scp = Command::new("scp");
        scp.args(["-o", "BatchMode=yes"]);
        if let Some(k) = &self.identity {
            scp.arg("-i").arg(k);
        }
        let st = scp
            .arg(local)
            .arg(format!("{}:{tmp}", self.target))
            .status()
            .context("running scp")?;
        if !st.success() {
            bail!("scp of {} failed", local.display());
        }
        self.run(&format!(
            "sudo mkdir -p \"$(dirname {r})\" && sudo install -m 0644 {tmp} {r} && rm -f {tmp}",
            r = sh_quote(remote)
        ))?;
        Ok(())
    }
}

/// Single-quotes a value for a POSIX shell.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Records every call and answers `run` from a table of (command substring, output).
#[derive(Default)]
pub struct FakeGuest {
    pub answers: Vec<(String, String)>,
    pub log: std::sync::Mutex<Vec<String>>,
}

impl FakeGuest {
    pub fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl GuestOps for FakeGuest {
    fn run(&self, cmd: &str) -> Result<String> {
        self.log.lock().unwrap().push(format!("run {cmd}"));
        Ok(self
            .answers
            .iter()
            .find(|(k, _)| cmd.contains(k.as_str()))
            .map(|(_, v)| v.clone())
            .unwrap_or_default())
    }
    fn write(&self, path: &str, content: &str, mode: &str) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("write {path} {mode} {}b", content.len()));
        Ok(())
    }
    fn upload(&self, local: &Path, remote: &str) -> Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("upload {} {remote}", local.display()));
        Ok(())
    }
}

// ---------------------------------------------------------------- config builder

/// A `config.yaml`, keys sorted. List keys (`disable`, `kubelet-arg`, ...) accumulate; scalar keys may be set once
/// to one value, so two features that disagree are an error instead of one silently winning.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct K3sConfig {
    map: BTreeMap<String, Value>,
}

impl K3sConfig {
    pub fn set(&mut self, key: &str, v: Value) -> Result<()> {
        match self.map.get(key) {
            Some(old) if *old != v => {
                bail!("{key} is set twice with different values ({old:?} and {v:?})")
            }
            _ => {
                self.map.insert(key.to_string(), v);
                Ok(())
            }
        }
    }
    pub fn set_bool(&mut self, key: &str, v: bool) -> Result<()> {
        self.set(key, Value::Bool(v))
    }
    pub fn set_str(&mut self, key: &str, v: &str) -> Result<()> {
        self.set(key, Value::String(v.to_string()))
    }
    /// Appends to a list key, skipping an item already present.
    pub fn push(&mut self, key: &str, item: &str) {
        let e = self
            .map
            .entry(key.to_string())
            .or_insert_with(|| Value::Sequence(vec![]));
        if let Value::Sequence(s) = e {
            let v = Value::String(item.to_string());
            if !s.contains(&v) {
                s.push(v);
            }
        }
    }
    pub fn merge(&mut self, other: &K3sConfig) -> Result<()> {
        for (k, v) in &other.map {
            match v {
                Value::Sequence(items) => {
                    for i in items {
                        if let Value::String(s) = i {
                            self.push(k, s);
                        }
                    }
                }
                _ => self.set(k, v.clone())?,
            }
        }
        Ok(())
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn render(&self) -> String {
        let body = serde_yaml::to_string(&self.map).unwrap_or_default();
        format!("# generated by `fluxctl kube addons`; edit with care\n{body}")
    }
}

// ---------------------------------------------------------------- add-on toggles

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Addon {
    Traefik,
    Servicelb,
    LocalStorage,
    MetricsServer,
    Coredns,
}

impl Addon {
    /// The name `--disable` takes.
    pub fn k3s_name(self) -> &'static str {
        match self {
            Addon::Traefik => "traefik",
            Addon::Servicelb => "servicelb",
            Addon::LocalStorage => "local-storage",
            Addon::MetricsServer => "metrics-server",
            Addon::Coredns => "coredns",
        }
    }
}

/// k3s ships all four enabled; "enable" is therefore the absence of a `disable` entry. Naming an add-on in both lists
/// is an error. Order in the output is the order of `disable`.
pub fn addon_toggles(enable: &[Addon], disable: &[Addon]) -> Result<K3sConfig> {
    for a in enable {
        if disable.contains(a) {
            bail!("{} is both enabled and disabled", a.k3s_name());
        }
    }
    let mut c = K3sConfig::default();
    for a in disable {
        c.push("disable", a.k3s_name());
    }
    Ok(c)
}

// ---------------------------------------------------------------- Cilium

/// Config that makes room for Cilium: no flannel, no k3s network policy controller, optionally no kube-proxy.
/// Cilium itself is installed afterwards (see [`cilium_install_hint`]); k3s cannot install it from this file.
pub fn cilium(kube_proxy_replacement: bool) -> Result<K3sConfig> {
    let mut c = K3sConfig::default();
    c.set_str("flannel-backend", "none")?;
    c.set_bool("disable-network-policy", true)?;
    if kube_proxy_replacement {
        c.set_bool("disable-kube-proxy", true)?;
    }
    Ok(c)
}

pub fn cilium_install_hint(version: &str, api_host: &str, kube_proxy_replacement: bool) -> String {
    let mut s = format!(
        "helm repo add cilium https://helm.cilium.io && helm install cilium cilium/cilium --version {version} \\\n  \
         --namespace kube-system --set operator.replicas=1"
    );
    if kube_proxy_replacement {
        s.push_str(&format!(
            " \\\n  --set kubeProxyReplacement=true --set k8sServiceHost={api_host} --set k8sServicePort=6443"
        ));
    }
    s.push('\n');
    s
}

// ---------------------------------------------------------------- node flags

#[derive(Debug, Clone, Default)]
pub struct NodeFlags {
    pub labels: Vec<String>,
    pub taints: Vec<String>,
    pub kube_reserved: Option<String>,
    pub system_reserved: Option<String>,
    pub eviction_hard: Option<String>,
}

fn alnum_edges(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.ends_with(|c: char| c.is_ascii_alphanumeric())
}

fn valid_label_key(k: &str) -> bool {
    let (prefix, name) = match k.rsplit_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, k),
    };
    let name_ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && alnum_edges(name);
    let prefix_ok = prefix.is_none_or(|p| {
        !p.is_empty()
            && p.len() <= 253
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-.".contains(c))
    });
    name_ok && prefix_ok
}

fn valid_label_value(v: &str) -> bool {
    v.is_empty()
        || (v.len() <= 63
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            && alnum_edges(v))
}

/// `key=value` (value may be empty).
pub fn check_label(s: &str) -> Result<()> {
    let (k, v) = s.split_once('=').unwrap_or((s, ""));
    if !valid_label_key(k) || !valid_label_value(v) {
        bail!("label {s:?} must be key=value with Kubernetes label syntax");
    }
    Ok(())
}

/// `key[=value]:Effect`.
pub fn check_taint(s: &str) -> Result<()> {
    let (kv, effect) = s
        .rsplit_once(':')
        .with_context(|| format!("taint {s:?} must be key[=value]:Effect"))?;
    if !["NoSchedule", "PreferNoSchedule", "NoExecute"].contains(&effect) {
        bail!("taint effect {effect:?} must be NoSchedule, PreferNoSchedule or NoExecute");
    }
    check_label(kv).with_context(|| format!("taint {s:?}"))
}

/// `cpu=100m,memory=256Mi,ephemeral-storage=1Gi`: only those resources, each with a quantity.
pub fn check_reservation(s: &str) -> Result<()> {
    for part in s.split(',') {
        let (k, v) = part
            .split_once('=')
            .with_context(|| format!("reservation {s:?} must be cpu=..,memory=.."))?;
        if !["cpu", "memory", "ephemeral-storage", "pid"].contains(&k) {
            bail!("reservation resource {k:?} must be cpu, memory, ephemeral-storage or pid");
        }
        if v.is_empty()
            || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
            || !v.starts_with(|c: char| c.is_ascii_digit())
        {
            bail!("reservation quantity {v:?} for {k} is not a Kubernetes quantity");
        }
    }
    Ok(())
}

pub fn node_flags(n: &NodeFlags) -> Result<K3sConfig> {
    let mut c = K3sConfig::default();
    for l in &n.labels {
        check_label(l)?;
        c.push("node-label", l);
    }
    for t in &n.taints {
        check_taint(t)?;
        c.push("node-taint", t);
    }
    if let Some(r) = &n.kube_reserved {
        check_reservation(r)?;
        c.push("kubelet-arg", &format!("kube-reserved={r}"));
    }
    if let Some(r) = &n.system_reserved {
        check_reservation(r)?;
        c.push("kubelet-arg", &format!("system-reserved={r}"));
    }
    if let Some(e) = &n.eviction_hard {
        if !e.contains('<') {
            bail!("eviction-hard {e:?} must look like memory.available<200Mi");
        }
        c.push("kubelet-arg", &format!("eviction-hard={e}"));
    }
    Ok(c)
}

// ---------------------------------------------------------------- applying

/// Writes the rendered config (keeping the previous file as `.fluxvm-bak`, once) and, if asked, restarts k3s.
pub fn apply_config(
    g: &dyn GuestOps,
    cfg: &K3sConfig,
    restart: Option<&str>,
) -> Result<Vec<String>> {
    let mut done = vec![];
    g.run(&format!(
        "sudo sh -c '[ -f {p} ] && [ ! -f {p}.fluxvm-bak ] && cp -p {p} {p}.fluxvm-bak; true'",
        p = K3S_CONFIG_PATH
    ))?;
    done.push(format!(
        "backed up {K3S_CONFIG_PATH} (if any) to {K3S_CONFIG_PATH}.fluxvm-bak"
    ));
    g.write(K3S_CONFIG_PATH, &cfg.render(), "0600")?;
    done.push(format!("wrote {K3S_CONFIG_PATH}"));
    if let Some(unit) = restart {
        if unit != "k3s" && unit != "k3s-agent" {
            bail!("unit {unit:?} must be k3s or k3s-agent");
        }
        g.run(&format!("sudo systemctl restart {unit}"))?;
        done.push(format!("restarted {unit}"));
    }
    Ok(done)
}

// ---------------------------------------------------------------- CLI

#[derive(clap::Args, Debug, Default)]
pub struct ConfigArgs {
    /// Keep this bundled add-on (the default; named only so a conflict with --disable is caught). Repeatable.
    #[arg(long, value_enum)]
    pub enable: Vec<Addon>,
    /// Turn a bundled add-on off. Repeatable.
    #[arg(long, value_enum)]
    pub disable: Vec<Addon>,
    /// Make room for Cilium (no flannel, no k3s network policy).
    #[arg(long)]
    pub cilium: bool,
    /// With --cilium: also disable kube-proxy (Cilium replaces it).
    #[arg(long, requires = "cilium")]
    pub cilium_kube_proxy_replacement: bool,
    /// Node label key=value. Repeatable.
    #[arg(long = "label")]
    pub labels: Vec<String>,
    /// Node taint key[=value]:Effect. Repeatable.
    #[arg(long = "taint")]
    pub taints: Vec<String>,
    /// kubelet kube-reserved, e.g. cpu=100m,memory=256Mi.
    #[arg(long)]
    pub kube_reserved: Option<String>,
    /// kubelet system-reserved, e.g. cpu=100m,memory=256Mi.
    #[arg(long)]
    pub system_reserved: Option<String>,
    /// kubelet hard eviction threshold, e.g. memory.available<200Mi.
    #[arg(long)]
    pub eviction_hard: Option<String>,
    /// Extra TLS SAN for the API server certificate. Repeatable.
    #[arg(long = "tls-san")]
    pub tls_sans: Vec<String>,
    /// Turn on k3s's embedded registry mirror (Spegel) on a server; pair it with `registry --embedded-mirror '*'`.
    #[arg(long)]
    pub embedded_registry: bool,
    /// Encrypt Secrets at rest.
    #[arg(long)]
    pub secrets_encryption: bool,
    /// CIS hardening profile (implies secrets encryption); `kube addons harden-files` prints the files it needs.
    #[arg(long)]
    pub cis: bool,
}

/// Everything the flags ask for, as one config. Pure.
pub fn build_config(a: &ConfigArgs) -> Result<K3sConfig> {
    let mut c = addon_toggles(&a.enable, &a.disable)?;
    if a.cilium {
        c.merge(&cilium(a.cilium_kube_proxy_replacement)?)?;
    }
    c.merge(&node_flags(&NodeFlags {
        labels: a.labels.clone(),
        taints: a.taints.clone(),
        kube_reserved: a.kube_reserved.clone(),
        system_reserved: a.system_reserved.clone(),
        eviction_hard: a.eviction_hard.clone(),
    })?)?;
    for s in &a.tls_sans {
        c.push("tls-san", s);
    }
    if a.embedded_registry {
        c.set_bool("embedded-registry", true)?;
    }
    if a.cis {
        c.merge(&crate::kube_harden::cis_config())?;
    } else if a.secrets_encryption {
        c.merge(&crate::kube_harden::secrets_encryption())?;
    }
    Ok(c)
}

#[derive(clap::Subcommand, Debug)]
pub enum AddonsCommand {
    /// Print (or with --ssh, install) a k3s config.yaml built from the flags.
    Config {
        #[command(flatten)]
        args: ConfigArgs,
        /// Write it on this host over ssh ([user@]host) instead of printing it.
        #[arg(long)]
        ssh: Option<String>,
        /// With --ssh: restart `k3s` or `k3s-agent` afterwards.
        #[arg(long)]
        restart: Option<String>,
    },
    /// Print the files the CIS profile needs next to config.yaml (PSA, audit policy, sysctl).
    HardenFiles {
        /// Install them on this host over ssh instead of printing them.
        #[arg(long)]
        ssh: Option<String>,
    },
    /// Print registries.yaml for mirrors / private registries.
    Registry(crate::kube_registry::RegistryArgs),
    /// Print a HelmChartConfig that customises the bundled Traefik (`--set KEY=VALUE`, `--gateway-api`), or install it with --ssh.
    Traefik {
        #[arg(long = "set", value_name = "KEY=VALUE")]
        sets: Vec<String>,
        #[arg(long)]
        gateway_api: bool,
        #[arg(long)]
        ssh: Option<String>,
    },
    /// Put nodes in a ServiceLB pool: prints the labels for config.yaml and the kubectl commands for running nodes.
    LbPool {
        pool: String,
        #[arg(long = "node")]
        nodes: Vec<String>,
    },
    /// Secrets-encryption status, or key rotation (`--rotate --yes`), on a server over ssh.
    Secrets {
        #[arg(long)]
        ssh: String,
        #[arg(long)]
        rotate: bool,
        #[arg(long, requires = "rotate")]
        yes: bool,
    },
    /// Read-only check that a server has the CIS profile's settings in place.
    CisCheck {
        #[arg(long)]
        ssh: String,
    },
    /// Plan (or with --ssh, perform) an airgap image preload from local tarballs.
    Airgap {
        files: Vec<PathBuf>,
        #[arg(long)]
        ssh: Option<String>,
    },
    /// Merge a kubeconfig into ~/.kube/config under a chosen context name.
    Kubeconfig(crate::kube_kubeconfig::MergeArgs),
    /// Preflight checks for a k3s host (RAM/CPU/disk, time sync, ports, cgroup, vz limits).
    Preflight(crate::kube_preflight::PreflightArgs),
    /// A hostPath PersistentVolume and claim on a virtiofs share.
    Pv(crate::kube_recipes::PvArgs),
    /// Guidance for running linux/amd64 images through Rosetta.
    Rosetta,
}

pub fn run(cmd: AddonsCommand) -> Result<()> {
    match cmd {
        AddonsCommand::Config { args, ssh, restart } => {
            let cfg = build_config(&args)?;
            if let Some(target) = ssh {
                let g = SshGuest::new(target);
                for l in apply_config(&g, &cfg, restart.as_deref())? {
                    println!("{l}");
                }
            } else {
                print!("{}", cfg.render());
                if args.cilium {
                    eprintln!(
                        "\nthen install Cilium:\n{}",
                        cilium_install_hint(
                            crate::kube_host::CILIUM_VERSION,
                            "127.0.0.1",
                            args.cilium_kube_proxy_replacement
                        )
                    );
                }
            }
        }
        AddonsCommand::HardenFiles { ssh } => match ssh {
            Some(target) => {
                let g = SshGuest::new(target);
                for l in crate::kube_harden::apply_files(&g, &crate::kube_harden::cis_files())? {
                    println!("{l}");
                }
            }
            None => {
                for f in crate::kube_harden::cis_files() {
                    println!("# --- {} (mode {})\n{}", f.path, f.mode, f.content);
                }
            }
        },
        AddonsCommand::Traefik {
            sets,
            gateway_api,
            ssh,
        } => {
            let y = crate::kube_ingress::traefik_helmchartconfig(&sets, gateway_api)?;
            match ssh {
                Some(t) => {
                    SshGuest::new(t).write(crate::kube_ingress::TRAEFIK_CONFIG_PATH, &y, "0600")?;
                    println!(
                        "wrote {}; k3s applies it by itself",
                        crate::kube_ingress::TRAEFIK_CONFIG_PATH
                    );
                }
                None => print!("{y}"),
            }
        }
        AddonsCommand::LbPool { pool, nodes } => {
            for l in crate::kube_ingress::lb_pool_node_labels(&pool)? {
                println!("node-label: {l}");
            }
            if !nodes.is_empty() {
                print!("{}", crate::kube_ingress::lb_pool_commands(&pool, &nodes)?);
            }
        }
        AddonsCommand::Secrets { ssh, rotate, yes } => {
            let g = SshGuest::new(ssh);
            let s = if rotate {
                crate::kube_secrets::rotate_keys(&g, yes)?
            } else {
                crate::kube_secrets::status(&g)?
            };
            println!(
                "enabled: {}\nstage: {}\nhashes: {}",
                s.enabled, s.stage, s.hashes
            );
        }
        AddonsCommand::CisCheck { ssh } => {
            let g = SshGuest::new(ssh);
            let r = crate::kube_harden::cis_audit(&crate::kube_harden::cis_gather(&g));
            print!("{}", crate::kube_preflight::render(&r));
            if crate::kube_preflight::failed(&r) {
                std::process::exit(1);
            }
        }
        AddonsCommand::Registry(a) => print!("{}", crate::kube_registry::run(&a)?),
        AddonsCommand::Airgap { files, ssh } => {
            let plan = crate::kube_registry::airgap_plan(&files)?;
            match ssh {
                Some(target) => {
                    let g = SshGuest::new(target);
                    for l in crate::kube_registry::apply_airgap(&g, &plan)? {
                        println!("{l}");
                    }
                }
                None => print!("{}", crate::kube_registry::render_plan(&plan)),
            }
        }
        AddonsCommand::Kubeconfig(a) => println!("{}", crate::kube_kubeconfig::run(&a)?),
        AddonsCommand::Preflight(a) => {
            if !crate::kube_preflight::run(&a)? {
                std::process::exit(1);
            }
        }
        AddonsCommand::Pv(a) => print!("{}", crate::kube_recipes::pv_yaml(&a.to_spec()?)?),
        AddonsCommand::Rosetta => print!("{}", crate::kube_recipes::rosetta_guidance()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDR: &str = "# generated by `fluxctl kube addons`; edit with care\n";

    #[test]
    fn toggles_golden() {
        let c = addon_toggles(
            &[Addon::LocalStorage],
            &[Addon::Traefik, Addon::Servicelb, Addon::MetricsServer],
        )
        .unwrap();
        assert_eq!(
            c.render(),
            format!("{HDR}disable:\n- traefik\n- servicelb\n- metrics-server\n")
        );
    }

    #[test]
    fn enable_and_disable_conflict() {
        assert!(addon_toggles(&[Addon::Traefik], &[Addon::Traefik]).is_err());
    }

    #[test]
    fn nothing_requested_renders_empty_map() {
        assert!(build_config(&ConfigArgs::default()).unwrap().is_empty());
    }

    #[test]
    fn cilium_golden_and_kube_proxy() {
        assert_eq!(
            cilium(true).unwrap().render(),
            format!(
                "{HDR}disable-kube-proxy: true\ndisable-network-policy: true\nflannel-backend: none\n"
            )
        );
        assert!(!cilium(false).unwrap().render().contains("kube-proxy"));
        assert!(cilium_install_hint("1.2.3", "10.0.0.1", true).contains("k8sServiceHost=10.0.0.1"));
        assert!(!cilium_install_hint("1.2.3", "x", false).contains("kubeProxyReplacement"));
    }

    #[test]
    fn node_flags_golden() {
        let c = node_flags(&NodeFlags {
            labels: vec!["disk=ssd".into(), "topology.kubernetes.io/zone=mac".into()],
            taints: vec!["dedicated=db:NoSchedule".into()],
            kube_reserved: Some("cpu=100m,memory=256Mi".into()),
            system_reserved: Some("memory=128Mi".into()),
            eviction_hard: Some("memory.available<200Mi".into()),
        })
        .unwrap();
        assert_eq!(
            c.render(),
            format!(
                "{HDR}kubelet-arg:\n- kube-reserved=cpu=100m,memory=256Mi\n- system-reserved=memory=128Mi\n- eviction-hard=memory.available<200Mi\n\
                 node-label:\n- disk=ssd\n- topology.kubernetes.io/zone=mac\n\
                 node-taint:\n- dedicated=db:NoSchedule\n"
            )
        );
    }

    #[test]
    fn node_flag_validation() {
        assert!(check_label("a b=c").is_err());
        assert!(check_label("-bad=x").is_err());
        assert!(check_label("ok").is_ok());
        assert!(check_taint("k=v:Nope").is_err());
        assert!(check_taint("k=v").is_err());
        assert!(check_taint("k:NoExecute").is_ok());
        assert!(check_reservation("gpu=1").is_err());
        assert!(check_reservation("cpu=").is_err());
        assert!(check_reservation("cpu=100m,memory=1Gi").is_ok());
        let bad = NodeFlags {
            eviction_hard: Some("memory".into()),
            ..Default::default()
        };
        assert!(node_flags(&bad).is_err());
    }

    #[test]
    fn conflicting_scalars_are_an_error_and_lists_dedupe() {
        let mut c = K3sConfig::default();
        c.set_str("a", "x").unwrap();
        assert!(c.set_str("a", "x").is_ok());
        assert!(c.set_str("a", "y").is_err());
        c.push("l", "one");
        c.push("l", "one");
        assert_eq!(c.render().matches("- one").count(), 1);
    }

    #[test]
    fn build_config_combines_and_guards() {
        let a = ConfigArgs {
            disable: vec![Addon::Traefik, Addon::Coredns],
            kube_reserved: Some("cpu=100m".into()),
            secrets_encryption: true,
            embedded_registry: true,
            tls_sans: vec!["10.0.0.9".into()],
            ..Default::default()
        };
        let c = build_config(&a).unwrap().render();
        for want in [
            "disable:\n- traefik\n- coredns",
            "embedded-registry: true",
            "kube-reserved=cpu=100m",
            "secrets-encryption: true",
            "tls-san:\n- 10.0.0.9",
        ] {
            assert!(c.contains(want), "{want}\n{c}");
        }
    }

    #[test]
    fn cis_and_node_reservation_share_kubelet_arg() {
        let a = ConfigArgs {
            cis: true,
            kube_reserved: Some("memory=256Mi".into()),
            ..Default::default()
        };
        let c = build_config(&a).unwrap().render();
        assert!(
            c.contains("kube-reserved=memory=256Mi")
                && c.contains("streaming-connection-idle-timeout=5m"),
            "{c}"
        );
    }

    #[test]
    fn apply_writes_backs_up_and_restarts() {
        let g = FakeGuest::default();
        let cfg = addon_toggles(&[], &[Addon::Traefik]).unwrap();
        let out = apply_config(&g, &cfg, Some("k3s")).unwrap();
        assert_eq!(out.len(), 3);
        let calls = g.calls();
        assert!(calls[0].contains("fluxvm-bak"));
        assert_eq!(
            calls[1],
            format!("write {K3S_CONFIG_PATH} 0600 {}b", cfg.render().len())
        );
        assert_eq!(calls[2], "run sudo systemctl restart k3s");
        assert!(apply_config(&g, &cfg, Some("sshd; rm -rf /")).is_err());
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
    }
}
