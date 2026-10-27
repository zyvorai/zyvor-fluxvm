// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl kube`: a Kubernetes (k3s) cluster inside FluxVM VMs (a server and optional workers), reachable from the Mac.
//!
//! The same shape as `fluxctl docker`: one managed Debian VM, folders shared at matching paths (for `hostPath`
//! volumes), and a supervised SSH forward that publishes the guest's API server on `127.0.0.1:<api-port>`. The
//! cluster's kubeconfig is fetched over SSH, pointed at that local port, renamed after the VM, and written to
//! `~/.kube/fluxvm-<name>.yaml` (mode 600). `fluxctl kube env` prints the `KUBECONFIG` line for it.
//!
//! This is the full-fidelity alternative to `fluxvm-kube`, which is an operator that runs *inside* an existing cluster.
//! Images built in the `fluxctl docker` engine are not visible to this cluster: push them to a registry, or import them
//! with `docker save IMG | ssh <vm> sudo k3s ctr images import -`.

use crate::docker_host::{self as dh, parse_publish_all, parse_share, pid_file};
use crate::run::{self, VmApi};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const DEFAULT_NAME: &str = "fluxvm-kube";
pub const AGENT_LABEL: &str = "dev.zyvor.fluxvm.kube";
/// k3s's API server port inside the guest.
const GUEST_API_PORT: u16 = 6443;
/// The first boot downloads and starts k3s, so allow for a slow network.
const FIRST_BOOT_BUDGET: Duration = Duration::from_secs(900);
/// Tries at an image import before its ssh connection failures are reported.
const IMPORT_ATTEMPTS: u32 = 4;
/// Agent nodes a cluster may have. `down` looks for this many.
pub const MAX_WORKERS: u8 = 8;
/// The Cilium chart installed by default (`helm.cilium.io`).
pub const CILIUM_VERSION: &str = "1.20.3";
/// The files `fluxctl kube fetch` leaves in an offline folder (Apple silicon guests are arm64).
pub const OFFLINE_BINARY: &str = "k3s-arm64";
pub const OFFLINE_IMAGES: &str = "k3s-airgap-images-arm64.tar.zst";
pub const OFFLINE_INSTALLER: &str = "install.sh";
const OFFLINE_SUMS: &str = "sha256sum-arm64.txt";

/// The pod network. Cilium is the default; k3s's bundled flannel is kept for `--offline`, which cannot fetch Cilium.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cni {
    Cilium,
    Flannel,
}

impl Cni {
    pub fn parse(s: &str) -> Result<Cni> {
        match s {
            "cilium" => Ok(Cni::Cilium),
            "flannel" => Ok(Cni::Flannel),
            other => bail!("--cni {other:?} must be cilium or flannel"),
        }
    }

    /// What `--cni` means when it is not given: Cilium, except offline.
    pub fn default_for(offline: bool) -> Cni {
        if offline { Cni::Flannel } else { Cni::Cilium }
    }

    pub fn name(self) -> &'static str {
        match self {
            Cni::Cilium => "cilium",
            Cni::Flannel => "flannel",
        }
    }
}

#[derive(Clone, Debug)]
pub struct KubeOptions {
    pub name: String,
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub image: Option<String>,
    /// `HOST` or `HOST:GUEST[:ro]`, as for `fluxctl docker`.
    pub shares: Vec<String>,
    pub user: Option<String>,
    /// `HOST[:GUEST]` loopback ports forwarded to the guest (for NodePort services and the like).
    pub publish: Vec<String>,
    /// Where the Mac reaches the API server.
    pub api_port: u16,
    /// A k3s release such as `v1.31.4+k3s1`; default is the installer's stable channel.
    pub k3s_version: Option<String>,
    /// Extra `k3s server` arguments.
    pub k3s_args: Vec<String>,
    /// Keep k3s's bundled Traefik ingress (off by default, as Colima does).
    pub traefik: bool,
    pub kubeconfig: Option<PathBuf>,
    /// Extra agent nodes (`NAME-w1` and so on), each its own VM joined to the server.
    pub workers: u8,
    /// A folder made by `fluxctl kube fetch`: k3s and its system images are installed from it, not the internet.
    pub offline: Option<PathBuf>,
    /// The pod network (see [`Cni`]).
    pub cni: Cni,
    /// The Cilium chart version, such as `1.20.3`.
    pub cilium_version: String,
    /// Do not add the cluster to `~/.kube/config` (see [`merge_default_kubeconfig`]).
    pub no_merge: bool,
}

impl Default for KubeOptions {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.into(),
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 30,
            image: None,
            shares: vec![],
            user: None,
            publish: vec![],
            api_port: GUEST_API_PORT,
            k3s_version: None,
            k3s_args: vec![],
            traefik: false,
            kubeconfig: None,
            workers: 0,
            offline: None,
            cni: Cni::Cilium,
            cilium_version: CILIUM_VERSION.into(),
            no_merge: false,
        }
    }
}

/// A Kubernetes resource name (a Deployment, Namespace, container, ...): a lowercase DNS label of up to 63 characters.
pub fn resource_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// A Kubernetes node name: lowercase DNS label, which is also what the VM's hostname becomes.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// `1.20.3`: three dot-separated numbers, nothing else reaches the manifest.
pub fn cilium_version_ok(v: &str) -> bool {
    let p: Vec<&str> = v.split('.').collect();
    p.len() == 3
        && p.iter()
            .all(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|c| c.is_ascii_digit()))
}

/// The HelmChart that k3s's built-in helm controller turns into a Cilium install. `bootstrap` lets it run before the
/// node is Ready, which it cannot be without a pod network.
pub fn cilium_manifest(o: &KubeOptions) -> String {
    format!(
        "apiVersion: helm.cattle.io/v1\nkind: HelmChart\nmetadata:\n  name: cilium\n  namespace: kube-system\nspec:\n  bootstrap: true\n  repo: https://helm.cilium.io\n  chart: cilium\n  version: {}\n  targetNamespace: kube-system\n  valuesContent: |-\n    operator:\n      replicas: 1\n    ipam:\n      mode: kubernetes\n    kubeProxyReplacement: false\n",
        o.cilium_version
    )
}

/// `v1.31.4+k3s1` (or without the `+k3sN`). Nothing else reaches the installer's environment.
pub fn version_ok(v: &str) -> bool {
    let (core, rev) = match v.split_once("+k3s") {
        Some((c, r)) => (c, Some(r)),
        None => (v, None),
    };
    let nums = core
        .strip_prefix('v')
        .map(|c| c.split('.').collect::<Vec<_>>());
    matches!(nums, Some(p) if p.len() == 3 && p.iter().all(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit())))
        && rev.is_none_or(|r| !r.is_empty() && r.bytes().all(|c| c.is_ascii_digit()))
}

/// A `k3s server` flag or value: plain characters only, because it is placed inside single quotes in a shell line.
pub fn k3s_arg_ok(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 200
        && a.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_=./:,@+".contains(&c))
}

pub(crate) fn validate(o: &KubeOptions) -> Result<()> {
    if !name_ok(&o.name) {
        bail!(
            "--name {:?} must be 1 to 40 lowercase letters, digits or dashes, not starting or ending with a dash",
            o.name
        );
    }
    if let Some(v) = &o.k3s_version
        && !version_ok(v)
    {
        bail!("--k3s-version {v:?} must look like v1.31.4+k3s1");
    }
    if let Some(bad) = o.k3s_args.iter().find(|a| !k3s_arg_ok(a)) {
        bail!(
            "--k3s-arg {bad:?}: use plain characters only (letters, digits and - _ = . / : , @ +)"
        );
    }
    if o.api_port == 0 {
        bail!("--api-port must be 1 to 65535");
    }
    if !cilium_version_ok(&o.cilium_version) {
        bail!(
            "--cilium-version {:?} must look like 1.20.3",
            o.cilium_version
        );
    }
    if o.cni == Cni::Cilium && o.offline.is_some() {
        bail!(
            "--cni cilium cannot be used with --offline: its chart and images come from helm.cilium.io and quay.io. Use --cni flannel"
        );
    }
    if o.workers > MAX_WORKERS {
        bail!("--workers must be 0 to {MAX_WORKERS}");
    }
    if let Some(d) = &o.offline
        && !offline_path_ok(d)
    {
        bail!(
            "--offline {}: use an absolute path of letters, digits and - _ . / only",
            d.display()
        );
    }
    let ports = parse_publish_all(&o.publish)?;
    if ports.iter().any(|(h, _)| *h == o.api_port) {
        bail!("--publish: host port {} is the API port", o.api_port);
    }
    Ok(())
}

pub(crate) fn kubeconfig_path(o: &KubeOptions) -> Result<PathBuf> {
    home_kube(o)
}

fn home_kube(o: &KubeOptions) -> Result<PathBuf> {
    Ok(match &o.kubeconfig {
        Some(p) => p.clone(),
        None => dh::home()?
            .join(".kube")
            .join(format!("fluxvm-{}.yaml", o.name)),
    })
}

fn run_dir() -> PathBuf {
    dh::default_socket()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// The one-line status `kube prompt` reads; `supervise` rewrites it on every check.
pub(crate) fn prompt_file(name: &str) -> PathBuf {
    run_dir().join(format!("kube-{name}.prompt"))
}

/// Records the cluster's state for `kube prompt`; a failure to write it is not worth stopping the supervisor for.
fn write_prompt(o: &KubeOptions, state: crate::kube_prompt::State, rows: &[crate::kube_doctor::PodRow]) {
    let _ = write_private(
        &prompt_file(&o.name),
        &crate::kube_prompt::encode(state, rows, crate::kube_prompt::now()),
    );
}

fn forwarder_pid(o: &KubeOptions) -> PathBuf {
    pid_file(&run_dir().join(format!("kube-{}.sock", o.name)))
}

/// Present while `kube sleep` runs. The API port closes a few seconds before the VM is stopped, and a supervisor that sees a
/// closed port and a running VM would start `up` and sit in it waiting for a VM that is going away.
fn sleeping_marker(o: &KubeOptions) -> PathBuf {
    run_dir().join(format!("kube-{}.sleeping", o.name))
}

pub(crate) fn mark_sleeping(o: &KubeOptions, on: bool) {
    let p = sleeping_marker(o);
    if on {
        let _ = std::fs::create_dir_all(run_dir());
        let _ = std::fs::write(&p, b"");
    } else {
        let _ = std::fs::remove_file(&p);
    }
}

/// A `kube sleep` started less than five minutes ago and has not finished.
fn sleep_in_progress(o: &KubeOptions) -> bool {
    std::fs::metadata(sleeping_marker(o))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(300))
}

/// The `k3s server` arguments the VM is installed with.
pub fn k3s_exec(o: &KubeOptions) -> String {
    let mut a = vec![
        "server".to_owned(),
        "--write-kubeconfig-mode".into(),
        "644".into(),
        "--tls-san".into(),
        "127.0.0.1".into(),
        "--node-name".into(),
        o.name.clone(),
    ];
    if !o.traefik {
        a.push("--disable".into());
        a.push("traefik".into());
    }
    if o.cni == Cni::Cilium {
        // Cilium brings its own pod network and network policy.
        a.push("--flannel-backend=none".into());
        a.push("--disable-network-policy".into());
    }
    a.extend(o.k3s_args.iter().cloned());
    a.join(" ")
}

/// Whether a node runs the control plane or joins one.
pub enum Role {
    Server,
    Agent { url: String, token: String },
}

/// A k3s node token such as `K10…::server:…`: plain characters only, because it is placed inside single quotes.
pub fn token_ok(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 512
        && t.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b":_.-".contains(&c))
}

/// An absolute folder path of plain characters, because it is placed inside single quotes in a shell line.
pub fn offline_path_ok(p: &Path) -> bool {
    p.is_absolute()
        && !p.components().any(|c| c == std::path::Component::ParentDir)
        && p.to_str().is_some_and(|s| {
            s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_./".contains(&c))
        })
}

fn worker_name(o: &KubeOptions, i: u8) -> String {
    format!("{}-w{i}", o.name)
}

/// The shell lines cloud-init runs as root.
///
/// A single node installed online pipes the installer from get.k3s.io, as k3s documents. Anything else (an offline
/// folder, or a cluster with workers) runs as a transient unit that cloud-init does not wait for, because
/// * FluxVM mounts shared folders with commands appended after these, so the offline folder is not there yet, and
/// * the private network card that joins the VMs (`apple.networks`) is configured at about the same time.
///
/// Inside the single quotes of that unit only double quotes are used: the paths, node name, URL and token are all
/// validated to plain characters.
fn install_commands(o: &KubeOptions, role: &Role, node: &str) -> Vec<String> {
    let private = o.workers > 0;
    // Flannel binds its tunnel to the interface of the default route, which is the NAT card, and VMs cannot reach each
    // other over NAT; point it at the private card. (Cilium follows the node's InternalIP, which is `--node-ip`.)
    let node_ip = match (private, o.cni) {
        (false, _) => "",
        (true, Cni::Cilium) => " --node-ip $PRIV",
        (true, Cni::Flannel) => " --node-ip $PRIV --flannel-iface $IFACE",
    };
    let exec = match role {
        Role::Server => format!("{}{node_ip}", k3s_exec(o)),
        Role::Agent { .. } => format!("agent --node-name {node}{node_ip}"),
    };
    // The environment that makes the installer join an existing server, quoted with `q`.
    let join = |q: &str| match role {
        Role::Server => String::new(),
        Role::Agent { url, token } => format!("K3S_URL={q}{url}{q} K3S_TOKEN={q}{token}{q} "),
    };
    let version = |q: &str| {
        o.k3s_version
            .as_ref()
            .map(|v| format!("INSTALL_K3S_VERSION={q}{v}{q} "))
            .unwrap_or_default()
    };
    if !private && o.offline.is_none() {
        return vec![format!(
            "curl -sfL https://get.k3s.io | {}{}INSTALL_K3S_EXEC='{exec}' sh -",
            version("'"),
            join("'")
        )];
    }
    // Wait for the card on the cluster's private network, whose address becomes the node's.
    let wait_ip = if private {
        "PRIV=\"\"; for i in $(seq 1 90); do PRIV=$(ip -4 -o addr show | grep -o \"10\\.89\\.[0-9]*\\.[0-9]*\" | head -1); [ -n \"$PRIV\" ] && break; sleep 2; done; IFACE=$(ip -o -4 addr show | grep -F \"$PRIV/\" | cut -d\" \" -f2); "
    } else {
        ""
    };
    let script = match &o.offline {
        None => format!(
            "{wait_ip}curl -sfL https://get.k3s.io | {}{}INSTALL_K3S_EXEC=\"{exec}\" sh -",
            version("\""),
            join("\"")
        ),
        Some(dir) => {
            let d = dir.display();
            format!(
                "{wait_ip}for i in $(seq 1 150); do [ -f \"{d}/{OFFLINE_BINARY}\" ] && [ -f \"{d}/{OFFLINE_IMAGES}\" ] && [ -f \"{d}/{OFFLINE_INSTALLER}\" ] && break; sleep 2; done; \
                 mkdir -p /var/lib/rancher/k3s/agent/images && \
                 cp \"{d}/{OFFLINE_IMAGES}\" /var/lib/rancher/k3s/agent/images/ && \
                 install -m 0755 \"{d}/{OFFLINE_BINARY}\" /usr/local/bin/k3s && \
                 {}INSTALL_K3S_SKIP_DOWNLOAD=true INSTALL_K3S_EXEC=\"{exec}\" sh \"{d}/{OFFLINE_INSTALLER}\"",
                join("\"")
            )
        }
    };
    vec![format!(
        "systemd-run --no-block --unit=fluxvm-k3s-install /bin/bash -c '{script}'"
    )]
}

/// The VM request for one node: Debian 13, k3s installed on first boot, the user's SSH key, folders shared at matching
/// paths. The default image needs no packages (see below) and offline nodes install none, so offline first boot needs no
/// network at all.
pub fn node_spec(
    o: &KubeOptions,
    role: &Role,
    node: &str,
    user: &str,
    key: &str,
    shares: Vec<Value>,
) -> Value {
    // The default image (Debian 13) already has curl, wget, iptables and ca-certificates, which is all the k3s installer
    // needs, so nothing is installed: cloud-init's `apt update` also downloads the source indexes that image's apt sources
    // list (10 MB and more), took over three minutes on a slow link, and when cloud-init's apt wrapper timed out the whole
    // first-boot sequence after it never ran, including the step that tells the host the guest's address. Offline nodes
    // install nothing either. Only another image, which may lack curl, gets it installed.
    let packages: Vec<&str> = if o.offline.is_some() || o.image.is_none() {
        vec![]
    } else {
        vec!["curl", "ca-certificates"]
    };
    // The default NAT network keeps each VM apart from the others. Nodes of one cluster share a private network
    // (`10.89.N.0/24`, one switch per name) as a second card, which carries the node-to-node and pod traffic.
    let apple = if o.workers > 0 {
        json!({"networks": [{"name": o.name}]})
    } else {
        json!({})
    };
    let write_files = match (role, o.cni) {
        (Role::Server, Cni::Cilium) => vec![json!({
            "path": "/var/lib/rancher/k3s/server/manifests/cilium.yaml",
            "content": cilium_manifest(o),
            "permissions": "0600",
        })],
        _ => vec![],
    };
    json!({
        "name": node,
        "backend": "auto",
        "image": o.image.clone().unwrap_or_else(|| "debian-13".into()),
        "vcpus": o.cpus,
        "memory_mib": o.memory_mib,
        "disk_size_gib": o.disk_gib,
        "network": {"mode": "user", "forwards": []},
        "apple": apple,
        "shared_folders": shares,
        "cloud_init": {
            "hostname": node,
            "user": user,
            "ssh_authorized_keys": [key],
            "packages": packages,
            "write_files": write_files,
            "runcmd": install_commands(o, role, node),
        },
    })
}

/// The control-plane VM.
pub fn kube_spec(o: &KubeOptions, user: &str, key: &str, shares: Vec<Value>) -> Value {
    node_spec(o, &Role::Server, &o.name, user, key, shares)
}

/// Points the k3s kubeconfig (cluster, context and user all named `default`) at the Mac's forwarded port and names
/// everything after the VM, so it does not collide with another cluster's `default` when files are merged.
pub fn rewrite_kubeconfig(raw: &str, name: &str, port: u16) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let (dash, rest) = match body.strip_prefix("- ") {
            Some(r) => ("- ", r),
            None => ("", body),
        };
        let new = match rest {
            r if r.starts_with("server: https://127.0.0.1:") => {
                format!("server: https://127.0.0.1:{port}")
            }
            "name: default" => format!("name: {name}"),
            "cluster: default" => format!("cluster: {name}"),
            "user: default" => format!("user: {name}"),
            "current-context: default" => format!("current-context: {name}"),
            r => r.to_owned(),
        };
        out.push_str(indent);
        out.push_str(dash);
        out.push_str(&new);
        out.push('\n');
    }
    out
}

/// Adds this cluster's cluster, user and context entries (named after the VM) to the kubeconfig `existing` and returns the
/// merged text. An entry of the same name is replaced; everything else is kept. `current-context` is only set when there is
/// none, so a `kubectl` pointed at another cluster is never switched behind its owner's back (`kube use` switches).
pub fn merge_kubeconfig(
    existing: Option<&str>,
    ours: &str,
    name: &str,
    use_it: bool,
) -> Result<String> {
    use serde_yaml::{Mapping, Value};
    let ours: Value = serde_yaml::from_str(ours).context("reading the cluster's kubeconfig")?;
    let mut merged: Value = match existing.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => serde_yaml::from_str(t).context("reading ~/.kube/config")?,
        None => Value::Mapping(Mapping::new()),
    };
    let Value::Mapping(top) = &mut merged else {
        bail!("~/.kube/config is not a kubeconfig (not a YAML mapping)");
    };
    top.entry("apiVersion".into()).or_insert("v1".into());
    top.entry("kind".into()).or_insert("Config".into());
    for key in ["clusters", "users", "contexts"] {
        let list = top
            .entry(key.into())
            .or_insert_with(|| Value::Sequence(vec![]));
        let Value::Sequence(items) = list else {
            bail!("~/.kube/config has a {key:?} that is not a list");
        };
        items.retain(|i| i.get("name").and_then(Value::as_str) != Some(name));
        if let Some(Value::Sequence(mine)) = ours.get(key) {
            items.extend(mine.iter().cloned());
        }
    }
    let has_current = top
        .get("current-context")
        .and_then(Value::as_str)
        .is_some_and(|c| !c.is_empty());
    if use_it || !has_current {
        top.insert("current-context".into(), name.into());
    }
    Ok(serde_yaml::to_string(&merged)?)
}

/// Removes this cluster's entries from a kubeconfig; `current-context` is cleared if it named this cluster.
pub fn unmerge_kubeconfig(existing: &str, name: &str) -> Result<String> {
    use serde_yaml::Value;
    let mut v: Value = serde_yaml::from_str(existing).context("reading ~/.kube/config")?;
    let Value::Mapping(top) = &mut v else {
        bail!("~/.kube/config is not a kubeconfig");
    };
    for key in ["clusters", "users", "contexts"] {
        if let Some(Value::Sequence(items)) = top.get_mut(key) {
            items.retain(|i| i.get("name").and_then(Value::as_str) != Some(name));
        }
    }
    if top.get("current-context").and_then(Value::as_str) == Some(name) {
        top.insert("current-context".into(), "".into());
    }
    Ok(serde_yaml::to_string(&v)?)
}

/// A kubeconfig with no clusters, users or contexts left.
fn kubeconfig_is_empty(text: &str) -> bool {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return false;
    };
    ["clusters", "users", "contexts"].iter().all(|k| {
        v.get(*k)
            .and_then(serde_yaml::Value::as_sequence)
            .is_none_or(|l| l.is_empty())
    })
}

fn default_kubeconfig() -> Result<PathBuf> {
    Ok(dh::home()?.join(".kube").join("config"))
}

/// Makes plain `kubectl`, `helm`, `k9s` and IDE plugins see this cluster with no `KUBECONFIG` to export. The user's
/// `~/.kube/config` is backed up once as `config.pre-fluxvm`. Returns true when the file changed.
pub fn merge_default_kubeconfig(o: &KubeOptions, use_it: bool) -> Result<bool> {
    let ours =
        std::fs::read_to_string(home_kube(o)?).context("reading the cluster's kubeconfig")?;
    let path = default_kubeconfig()?;
    let existing = std::fs::read_to_string(&path).ok();
    let merged = merge_kubeconfig(existing.as_deref(), &ours, &o.name, use_it)?;
    if existing.as_deref() == Some(merged.as_str()) {
        return Ok(false);
    }
    if let Some(old) = &existing {
        let backup = path.with_file_name("config.pre-fluxvm");
        if !backup.exists() {
            write_private(&backup, old)?;
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(&path, &merged)?;
    Ok(true)
}

/// Takes this cluster out of `~/.kube/config` again (best effort; `down --purge` calls it).
pub fn unmerge_default_kubeconfig(o: &KubeOptions) {
    let Ok(path) = default_kubeconfig() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Ok(out) = unmerge_kubeconfig(&text, &o.name)
        && out != text
    {
        if kubeconfig_is_empty(&out) {
            let _ = std::fs::remove_file(&path); // we created it, and nothing else is left in it
        } else {
            let _ = write_private(&path, &out);
        }
    }
}

/// True when something accepts TCP connections on the Mac's API port. (The VM's own readiness is checked over SSH.)
pub fn api_answers(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(2),
    )
    .is_ok()
}

pub struct Cluster {
    #[allow(dead_code)] // kept for callers that want to address the VM after `up`
    pub id: Uuid,
    pub ip: String,
    pub user: String,
    pub identity: Option<PathBuf>,
    pub api_port: u16,
    pub publish: Vec<(u16, u16)>,
    pub kubeconfig: PathBuf,
}

fn ssh(c: &Cluster, remote: &str) -> Command {
    let mut cmd = run::ssh_base(&c.ip, &c.user, &c.identity);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", remote]);
    cmd
}

/// A restored VM wakes with the clock it was saved with, minutes behind. The guest's time service fixes that within a minute
/// on its own; restarting it makes it step the clock now. Best effort: a failure only means waiting for the normal sync.
pub(crate) async fn resync_clock<A: VmApi>(api: &A, o: &KubeOptions) {
    let Ok(cluster) = ensure_vm(api, o).await else {
        return;
    };
    let _ = ssh(&cluster, "sudo systemctl restart systemd-timesyncd").output();
}

/// Runs a shell command in the cluster's server VM, feeding it `stdin` if given, and returns its trimmed output, or the
/// error text. Unlike `ensure_vm` it does not wait for Ready nodes, so it works while the control plane is down.
pub(crate) async fn guest_run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    script: &str,
    stdin: Option<&str>,
) -> Result<String> {
    guest_run_bytes(api, o, script, stdin.map(str::as_bytes)).await
}

/// `guest_run` with binary input, for streaming a tar archive.
pub(crate) async fn guest_run_bytes<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    script: &str,
    stdin: Option<&[u8]>,
) -> Result<String> {
    let out = guest_output_bytes(api, o, script, stdin).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// Runs a command in the server VM with optional binary input and returns its standard output untouched, for dumps.
pub(crate) async fn guest_output_bytes<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    script: &str,
    stdin: Option<&[u8]>,
) -> Result<Vec<u8>> {
    use std::{io::Write, process::Stdio};
    let Some((id, _)) = api.find(&o.name).await? else {
        bail!("no cluster named {}", o.name);
    };
    let Some(ip) = api.guest_ip(id).await? else {
        bail!("{} has no address yet", o.name);
    };
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (_, identity) = run::ssh_key(&home)?;
    let mut cmd = run::ssh_base(&ip, &user, &identity);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", script]);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running a command in the server VM")?;
    // Feed the input from a thread, so a command that prints while it reads cannot fill the output pipe and stall us.
    let out = std::thread::scope(|scope| {
        let writer = child
            .stdin
            .take()
            .zip(stdin)
            .map(|(mut pipe, data)| scope.spawn(move || pipe.write_all(data)));
        let out = child.wait_with_output();
        if let Some(w) = writer {
            let _ = w.join(); // a write error shows up as the command's own failure
        }
        out
    })?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

pub(crate) async fn guest_sh<A: VmApi>(api: &A, o: &KubeOptions, script: &str) -> Result<String> {
    guest_run(api, o, script, None).await
}

/// Starts a command in the server VM and hands back the child with its standard output piped, for streams such as logs.
/// Standard error stays on this terminal.
pub(crate) async fn guest_spawn<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    script: &str,
) -> Result<std::process::Child> {
    use std::process::Stdio;
    let Some((id, _)) = api.find(&o.name).await? else {
        bail!("no cluster named {}", o.name);
    };
    let Some(ip) = api.guest_ip(id).await? else {
        bail!("{} has no address yet", o.name);
    };
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (_, identity) = run::ssh_key(&home)?;
    let mut cmd = run::ssh_base(&ip, &user, &identity);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", script]);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    cmd.spawn().context("starting a command in the server VM")
}

/// Runs a command in the server VM with a terminal, for shells; returns its exit code.
pub(crate) async fn guest_interactive<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    script: &str,
) -> Result<i32> {
    let Some((id, _)) = api.find(&o.name).await? else {
        bail!("no cluster named {}", o.name);
    };
    let Some(ip) = api.guest_ip(id).await? else {
        bail!("{} has no address yet", o.name);
    };
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (_, identity) = run::ssh_key(&home)?;
    let mut cmd = run::ssh_base(&ip, &user, &identity);
    cmd.args([
        "-t",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        script,
    ]);
    let status = cmd.status().context("opening a shell in the server VM")?;
    Ok(status.code().unwrap_or(1))
}

/// Whether the names listener is up on this Mac (port 80 answers).
pub(crate) fn names_are_served() -> bool {
    api_answers(80)
}

/// The published services, for `kube open`.
pub(crate) async fn published_services<A: VmApi>(
    api: &A,
    o: &KubeOptions,
) -> Result<Vec<NodePortService>> {
    let cluster = ensure_vm(api, o).await?;
    read_services(&cluster)
}

/// `kubectl apply -f -` inside the server VM.
pub(crate) async fn guest_apply<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    manifest: &str,
) -> Result<String> {
    guest_run(api, o, "sudo k3s kubectl apply -f -", Some(manifest)).await
}

/// Opens the forwards for the services that exist now, without waiting for a supervisor's next check.
pub(crate) async fn publish_now<A: VmApi>(api: &A, o: &KubeOptions) -> Result<()> {
    publish_services(api, o, &mut vec![], None)
        .await
        .map(|_| ())
}

/// The Mac ports the recorded forwarder was started with (the `host:guest` entries of its spec).
pub(crate) fn forwarded_hosts(o: &KubeOptions) -> Vec<u16> {
    spec_hosts(
        std::fs::read_to_string(forwarder_pid(o))
            .unwrap_or_default()
            .lines()
            .nth(1)
            .unwrap_or(""),
    )
}

/// `user@ip|6443|30080:30080|5432:30432` -> [30080, 5432]
pub(crate) fn spec_hosts(spec: &str) -> Vec<u16> {
    spec.split('|')
        .filter_map(|f| f.split_once(':')?.0.parse().ok())
        .collect()
}

/// Whether the Mac port `host` is forwarded to the guest's `guest` right now.
pub(crate) fn is_forwarded(o: &KubeOptions, host: u16, guest: u16) -> bool {
    std::fs::read_to_string(forwarder_pid(o))
        .unwrap_or_default()
        .lines()
        .nth(1)
        .is_some_and(|spec| spec.split('|').any(|f| f == format!("{host}:{guest}")))
}

/// Finds or creates one node's VM and starts it if stopped.
async fn ensure_one<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    vm: &str,
    role: &Role,
    user: &str,
    key: &str,
) -> Result<Uuid> {
    if let Some((id, status)) = api.find(vm).await? {
        if status != "running" {
            eprintln!("starting {vm} ({status})…");
            api.start(id).await?;
        }
        return Ok(id);
    }
    let home = dh::home()?;
    let cwd = std::env::current_dir()?;
    let mut folders = o.shares.clone();
    if let Some(d) = &o.offline {
        folders.push(format!("{}:ro", d.display()));
    }
    let shares = folders
        .iter()
        .map(|s| parse_share(s, &cwd, Some(&home)))
        .collect::<Result<Vec<_>>>()?;
    let how = if o.offline.is_some() {
        "installing k3s from the offline folder"
    } else {
        "first boot installs k3s, this takes a few minutes"
    };
    eprintln!("creating {vm} ({how})…");
    let id = api
        .create(node_spec(o, role, vm, user, key, shares))
        .await?;
    let labels = std::collections::BTreeMap::from([("fluxvm.kube".to_owned(), o.name.clone())]);
    let _ = api.set_labels(id, labels).await; // a convenience for `list`, not needed to work
    Ok(id)
}

/// Waits until the server reports at least `want` Ready nodes and an OK `/readyz`.
async fn wait_nodes(c: &Cluster, want: usize, started: Instant) -> Result<()> {
    let probe = format!(
        "[ \"$(sudo k3s kubectl get nodes --no-headers 2>/dev/null | grep -c ' Ready')\" -ge {want} ] && sudo k3s kubectl get --raw /readyz 2>/dev/null | grep -qx ok"
    );
    let mut last = 0u64;
    loop {
        let ready = ssh(c, &probe)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running ssh")?
            .success();
        if ready {
            return Ok(());
        }
        if started.elapsed() > FIRST_BOOT_BUDGET {
            bail!(
                "{}",
                run::with_guest_report(
                    format!(
                        "Kubernetes did not report {want} Ready node(s) within {}s",
                        FIRST_BOOT_BUDGET.as_secs()
                    ),
                    run::cloud_init_report(&c.ip, &c.user, &c.identity)
                )
            );
        }
        let secs = started.elapsed().as_secs();
        if secs / 30 > last {
            last = secs / 30;
            eprintln!("waiting for {want} node(s) to be Ready ({secs}s)…");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Fails early, with the missing file named, when an offline folder is incomplete.
fn check_offline(dir: &Path) -> Result<()> {
    for f in [OFFLINE_BINARY, OFFLINE_IMAGES, OFFLINE_INSTALLER] {
        if !dir.join(f).is_file() {
            bail!(
                "--offline {}: {f} is missing (make the folder with `fluxctl kube fetch`)",
                dir.display()
            );
        }
    }
    Ok(())
}

/// Finds or creates the server VM (and any workers), starts them if stopped, and waits until every node is Ready.
pub async fn ensure_vm<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Cluster> {
    validate(o)?;
    if let Some(d) = &o.offline {
        check_offline(d)?;
    }
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (key, identity) = run::ssh_key(&home)?;
    let started = Instant::now();
    let id = ensure_one(api, o, &o.name, &Role::Server, &user, &key).await?;
    let ip = run::wait_ssh(api, id, &user, &identity).await?;
    let cluster = Cluster {
        id,
        ip,
        user,
        identity,
        api_port: o.api_port,
        publish: parse_publish_all(&o.publish)?,
        kubeconfig: home_kube(o)?,
    };
    wait_nodes(&cluster, 1, started).await?;
    let mut missing = false;
    for i in 1..=o.workers {
        missing |= api.find(&worker_name(o, i)).await?.is_none();
    }
    if o.workers > 0 {
        // Workers get their own first-boot budget; the server's boot time does not count against them.
        let started = Instant::now();
        let mut role = None;
        if missing {
            let out = ssh(&cluster, "sudo cat /var/lib/rancher/k3s/server/node-token")
                .output()
                .context("running ssh")?;
            let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !out.status.success() || !token_ok(&token) {
                bail!("could not read the cluster's node token from the server");
            }
            let private = private_ip(&cluster)?;
            role = Some(Role::Agent {
                url: format!("https://{private}:{GUEST_API_PORT}"),
                token,
            });
        }
        // Workers that already exist are only started if stopped; the role matters for the ones that must be created.
        let role = role.unwrap_or(Role::Server);
        for i in 1..=o.workers {
            ensure_one(api, o, &worker_name(o, i), &role, &cluster.user, &key).await?;
        }
        wait_nodes(&cluster, 1 + usize::from(o.workers), started).await?;
    }
    Ok(cluster)
}

/// The server's address on the cluster's private network, which is how workers reach it. It is missing when the server
/// was created without `--workers`, since only then does it get that card.
fn private_ip(c: &Cluster) -> Result<String> {
    let out = ssh(
        c,
        "ip -4 -o addr show | grep -o '10\\.89\\.[0-9]*\\.[0-9]*' | head -1",
    )
    .output()
    .context("running ssh")?;
    let ip = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    match ip.parse::<std::net::Ipv4Addr>() {
        Ok(a) if a.octets()[..2] == [10, 89] => Ok(ip),
        _ => bail!(
            "the server has no private-network address: it was created without --workers. Run `fluxctl kube down --purge` and `up --workers N` again"
        ),
    }
}

/// The worker VMs that exist, by name, id and status.
pub(crate) async fn worker_vms<A: VmApi>(
    api: &A,
    o: &KubeOptions,
) -> Result<Vec<(String, Uuid, String)>> {
    let mut found = vec![];
    for i in 1..=MAX_WORKERS {
        let name = worker_name(o, i);
        if let Some((id, status)) = api.find(&name).await? {
            found.push((name, id, status));
        }
    }
    Ok(found)
}

/// Fetches the guest's kubeconfig over SSH and writes the rewritten copy (mode 600) unless it is already current.
pub fn write_kubeconfig(c: &Cluster, name: &str) -> Result<bool> {
    let out = ssh(c, "sudo cat /etc/rancher/k3s/k3s.yaml")
        .output()
        .context("running ssh")?;
    if !out.status.success() {
        bail!(
            "could not read the cluster's kubeconfig: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = rewrite_kubeconfig(&String::from_utf8_lossy(&out.stdout), name, c.api_port);
    if !text.contains("certificate-authority-data") {
        bail!("the guest's kubeconfig does not look like k3s's");
    }
    if std::fs::read_to_string(&c.kubeconfig).is_ok_and(|current| current == text) {
        return Ok(false);
    }
    if let Some(dir) = c.kubeconfig.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(&c.kubeconfig, &text)?;
    Ok(true)
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let tmp = path.with_extension("yaml.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// What the forwarder was started with, so a change of options restarts it instead of being ignored.
fn forward_spec(c: &Cluster) -> String {
    let mut s = format!("{}@{}|{}", c.user, c.ip, c.api_port);
    for (h, g) in &c.publish {
        s.push_str(&format!("|{h}:{g}"));
    }
    s
}

/// Starts the SSH forward of the API server (and any published ports) unless one with the same settings already
/// answers. Returns the forwarder's pid when it started a new one.
pub fn ensure_forward(o: &KubeOptions, c: &Cluster) -> Result<Option<u32>> {
    let spec = forward_spec(c);
    let pidfile = forwarder_pid(o);
    let recorded = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|t| t.lines().nth(1).map(str::to_owned));
    if api_answers(c.api_port) && recorded.as_deref() == Some(spec.as_str()) {
        return Ok(None);
    }
    std::fs::create_dir_all(run_dir())?;
    dh::stop_recorded_ssh(&pidfile, &api_needle());
    let mut forwards = vec![format!(
        "127.0.0.1:{}:127.0.0.1:{GUEST_API_PORT}",
        c.api_port
    )];
    forwards.extend(
        c.publish
            .iter()
            .map(|(h, g)| format!("127.0.0.1:{h}:127.0.0.1:{g}")),
    );
    let child = dh::forward_command(&c.ip, &c.user, &c.identity, &forwards)?
        .spawn()
        .context("starting the SSH forward")?;
    let pid = child.id();
    std::fs::write(&pidfile, format!("{pid}\n{spec}\n"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if api_answers(c.api_port) {
            return Ok(Some(pid));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!(
        "the SSH forward started but 127.0.0.1:{} does not answer",
        c.api_port
    )
}

/// Marks our forwarder's command line, so a stale pid is never mistaken for another process.
fn api_needle() -> String {
    format!("127.0.0.1:{GUEST_API_PORT}")
}

/// `fluxctl kube up`: idempotent. Returns the kubeconfig path.
pub async fn up<A: VmApi>(api: &A, o: &KubeOptions) -> Result<PathBuf> {
    // A sleeping cluster resumes from its sleep checkpoint (seconds) instead of cold-booting.
    if crate::kube_checkpoint::is_asleep(api, o).await {
        // `wake` ends in `up`, so the future is boxed; by then the VM runs and this branch is not taken again.
        eprintln!("{}", Box::pin(crate::kube_checkpoint::wake(api, o)).await?);
    }
    let cluster = ensure_vm(api, o).await?;
    if write_kubeconfig(&cluster, &o.name)? {
        eprintln!("wrote {}", cluster.kubeconfig.display());
    }
    if !o.no_merge && o.kubeconfig.is_none() {
        match merge_default_kubeconfig(o, false) {
            Ok(true) => eprintln!(
                "added context {:?} to ~/.kube/config: plain `kubectl` works (`fluxctl kube use` makes it the current one)",
                o.name
            ),
            Ok(false) => {}
            Err(e) => eprintln!(
                "could not add it to ~/.kube/config (the cluster still works with KUBECONFIG): {e:#}"
            ),
        }
    }
    match ensure_forward(o, &cluster)? {
        Some(pid) => eprintln!(
            "forwarding the API server to 127.0.0.1:{} (pid {pid})",
            cluster.api_port
        ),
        None => eprintln!(
            "the API server already answers on 127.0.0.1:{}",
            cluster.api_port
        ),
    }
    Ok(cluster.kubeconfig)
}

pub fn env_line(kubeconfig: &Path) -> String {
    format!("export KUBECONFIG={}", kubeconfig.display())
}

/// `fluxctl kube status` as JSON.
pub async fn status<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Value> {
    validate(o)?;
    let kubeconfig = home_kube(o)?;
    let vm = api.find(&o.name).await?;
    let ip = match &vm {
        Some((id, _)) => api.guest_ip(*id).await.unwrap_or(None),
        None => None,
    };
    let mut workers = vec![];
    for (name, id, status) in worker_vms(api, o).await? {
        let ip = api.guest_ip(id).await.unwrap_or(None);
        workers.push(json!({"name": name, "status": status, "guest_ip": ip}));
    }
    Ok(json!({
        "name": o.name,
        "vm": vm.as_ref().map(|(id, status)| json!({"id": id, "status": status})),
        "guest_ip": ip,
        "workers": workers,
        "api": format!("https://127.0.0.1:{}", o.api_port),
        "api_answers": api_answers(o.api_port),
        "kubeconfig": kubeconfig,
        "kubeconfig_exists": kubeconfig.exists(),
        "env": env_line(&kubeconfig),
    }))
}

/// Stops the supervised SSH forward of the API port, if one is recorded and still running.
pub(crate) fn stop_forward(o: &KubeOptions) {
    dh::stop_recorded_ssh(&forwarder_pid(o), &api_needle());
}

/// `fluxctl kube down`: stop the forward and the VMs; `purge` also deletes them, the cluster's state and the kubeconfig.
pub async fn down<A: VmApi>(api: &A, o: &KubeOptions, purge: bool) -> Result<()> {
    validate(o)?;
    dh::stop_recorded_ssh(&forwarder_pid(o), &api_needle());
    let mut vms = worker_vms(api, o).await?; // workers first, so the server outlives them
    if let Some((id, status)) = api.find(&o.name).await? {
        vms.push((o.name.clone(), id, status));
    }
    for (name, id, status) in vms {
        if purge {
            if status == "running" {
                api.stop(id).await?;
            }
            api.delete(id).await?;
            eprintln!("deleted {name}");
        } else if status == "running" {
            api.stop(id).await?;
            eprintln!("stopped {name}");
        }
    }
    if purge {
        let _ = std::fs::remove_file(home_kube(o)?);
        unmerge_default_kubeconfig(o);
    }
    Ok(())
}

/// What `kube supervise` does besides keeping the cluster up. Shared by `supervise` and `agent install`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct SuperviseFlags {
    /// Do not forward NodePort and LoadBalancer services to 127.0.0.1.
    #[arg(long)]
    pub no_auto_publish: bool,
    /// Save a checkpoint this often while the cluster runs: seconds, or with a suffix s, m, h, d (`1h`). Off by default.
    /// Single-node clusters only; they are named `auto-<time>`.
    #[arg(long, value_parser = crate::parse_age_secs)]
    pub auto_checkpoint: Option<u64>,
    /// How many automatic checkpoints to keep; older ones are deleted.
    #[arg(long, default_value_t = 6)]
    pub keep_auto: usize,
    /// Put the cluster to sleep after this long with no traffic through its API port or published services (`30m`). Off by
    /// default. Single-node clusters only. A Studio page that is open counts as traffic.
    #[arg(long, value_parser = crate::parse_age_secs)]
    pub idle_sleep: Option<u64>,
    /// Do not serve `http://<service>.<namespace>.localhost` on 127.0.0.1:80 for the published services.
    #[arg(long)]
    pub no_names: bool,
    /// Do not show a macOS notification when a pod starts crash-looping, runs out of memory or cannot pull its image.
    #[arg(long)]
    pub no_notify: bool,
    /// Also put the cluster to sleep while the Mac runs on battery, once there has been no traffic for 2 minutes.
    #[arg(long)]
    pub sleep_on_battery: bool,
    /// Also put the cluster to sleep while the Mac's lid is closed, once there has been no traffic for 2 minutes.
    #[arg(long)]
    pub sleep_on_lid_close: bool,
}

impl SuperviseFlags {
    pub fn check(&self) -> Result<()> {
        if self.keep_auto == 0 {
            bail!("--keep-auto must be at least 1");
        }
        if self.idle_sleep.is_some_and(|s| s < 60) {
            bail!("--idle-sleep must be at least 1 minute");
        }
        Ok(())
    }

    /// The flags as `supervise` takes them, for the LaunchAgent.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec![];
        if self.no_auto_publish {
            a.push("--no-auto-publish".to_owned());
        }
        if let Some(s) = self.auto_checkpoint {
            a.extend(["--auto-checkpoint".to_owned(), s.to_string()]);
            a.extend(["--keep-auto".to_owned(), self.keep_auto.to_string()]);
        }
        if let Some(s) = self.idle_sleep {
            a.extend(["--idle-sleep".to_owned(), s.to_string()]);
        }
        if self.no_names {
            a.push("--no-names".to_owned());
        }
        if self.no_notify {
            a.push("--no-notify".to_owned());
        }
        if self.sleep_on_battery {
            a.push("--sleep-on-battery".to_owned());
        }
        if self.sleep_on_lid_close {
            a.push("--sleep-on-lid-close".to_owned());
        }
        a
    }
}

/// `pmset -g batt` says the Mac is drawing from the battery.
pub(crate) fn on_battery(pmset: &str) -> bool {
    pmset.contains("'Battery Power'")
}

/// `ioreg -r -k AppleClamshellState -d 4` says the lid is closed (a laptop only; a desktop has no such key).
pub(crate) fn lid_closed(ioreg: &str) -> bool {
    ioreg
        .lines()
        .any(|l| l.contains("\"AppleClamshellState\"") && l.trim_end().ends_with("Yes"))
}

/// How long the forwarder must have been quiet before a power condition parks the cluster, so a `kubectl` that just woke it
/// is not undone at once.
/// How often `supervise` looks at the pods for the notifications.
const POD_CHECK: Duration = Duration::from_secs(30);
const POWER_QUIET: Duration = Duration::from_secs(120);

fn power_says_sleep(battery: bool, lid: bool) -> Option<&'static str> {
    let out = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    if battery && on_battery(&out("pmset", &["-g", "batt"])) {
        return Some("the Mac is on battery");
    }
    if lid
        && lid_closed(&out(
            "ioreg",
            &["-r", "-k", "AppleClamshellState", "-d", "4"],
        ))
    {
        return Some("the Mac's lid is closed");
    }
    None
}

/// Total bytes in and out of a process from `nettop -P -L 1 -x -p PID` (CSV).
pub(crate) fn parse_nettop(csv: &str) -> Option<u64> {
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next()?.split(',').collect();
    let (i, o) = (
        header.iter().position(|c| *c == "bytes_in")?,
        header.iter().position(|c| *c == "bytes_out")?,
    );
    let row: Vec<&str> = lines.next()?.split(',').collect();
    Some(row.get(i)?.trim().parse::<u64>().ok()? + row.get(o)?.trim().parse::<u64>().ok()?)
}

/// Bytes a tick may move through the forwarder (keep-alives, our own health probe) and still count as idle.
const IDLE_BYTES: u64 = 16 * 1024;

/// Tracks whether the forwarder has been quiet: feed it each tick's byte total.
#[derive(Debug, Default)]
pub(crate) struct IdleWatch {
    last: Option<u64>,
    quiet_since: Option<Instant>,
}

impl IdleWatch {
    /// How long traffic has been below the threshold, or zero when it just moved or this is the first reading.
    pub(crate) fn tick(&mut self, total: u64, now: Instant) -> Duration {
        let moved = self
            .last
            .is_none_or(|l| total < l || total - l > IDLE_BYTES);
        self.last = Some(total);
        if moved {
            self.quiet_since = Some(now);
        }
        self.quiet_since.map_or(Duration::ZERO, |t| now - t)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The pid of the SSH forwarder, from its pid file.
fn forwarder_pid_of(o: &KubeOptions) -> Option<u32> {
    std::fs::read_to_string(forwarder_pid(o))
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Bytes the forwarder has carried so far, from `nettop` (macOS; no sudo).
fn forwarder_bytes(o: &KubeOptions) -> Option<u64> {
    let pid = forwarder_pid_of(o)?;
    let out = Command::new("nettop")
        .args(["-P", "-L", "1", "-x", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_nettop(&String::from_utf8_lossy(&out.stdout))
}

/// A Service the cluster exposes on a node port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePortService {
    pub namespace: String,
    pub name: String,
    /// The node port inside the VM.
    pub port: u16,
    /// The port to open on the Mac instead, from the Service's `fluxvm.dev/local-ports` annotation (`30432=5432`), so a
    /// database answers on its own number.
    pub local: Option<u16>,
    /// The Service's own port that this node port leads to (80 for a web server); 0 when not known.
    pub service_port: u16,
}

impl NodePortService {
    /// The loopback port on the Mac.
    pub fn host_port(&self) -> u16 {
        self.local.unwrap_or(self.port)
    }
}

/// `30432=5432,30900=9000` -> [(30432, 5432), (30900, 9000)]; bad pairs are ignored.
pub(crate) fn parse_local_ports(annotation: &str) -> Vec<(u16, u16)> {
    annotation
        .split(',')
        .filter_map(|pair| {
            let (node, local) = pair.trim().split_once('=')?;
            Some((node.trim().parse().ok()?, local.trim().parse().ok()?))
        })
        .collect()
}

/// The NodePort and LoadBalancer services in `kubectl get svc -A -o json`, one entry per node port, sorted.
pub(crate) fn node_port_services(list: &Value) -> Vec<NodePortService> {
    let mut out = vec![];
    for item in list["items"].as_array().into_iter().flatten() {
        if !matches!(
            item["spec"]["type"].as_str(),
            Some("NodePort" | "LoadBalancer")
        ) {
            continue;
        }
        let local = parse_local_ports(
            item["metadata"]["annotations"]["fluxvm.dev/local-ports"]
                .as_str()
                .unwrap_or(""),
        );
        for p in item["spec"]["ports"].as_array().into_iter().flatten() {
            if let Some(port) = p["nodePort"].as_u64().and_then(|n| u16::try_from(n).ok()) {
                out.push(NodePortService {
                    namespace: item["metadata"]["namespace"].as_str().unwrap_or("").into(),
                    name: item["metadata"]["name"].as_str().unwrap_or("").into(),
                    port,
                    local: local.iter().find(|(n, _)| *n == port).map(|(_, l)| *l),
                    service_port: p["port"]
                        .as_u64()
                        .and_then(|n| u16::try_from(n).ok())
                        .unwrap_or(0),
                });
            }
        }
    }
    out.sort_by(|a, b| (a.port, &a.namespace, &a.name).cmp(&(b.port, &b.namespace, &b.name)));
    out
}

/// The node ports to forward on top of the fixed `--publish` ones: never below 1024, never the API port or a
/// port already forwarded for another reason, each once. `taken` says whether the Mac already uses a port.
pub(crate) fn ports_to_open(
    found: &[NodePortService],
    fixed: &[(u16, u16)],
    api_port: u16,
    taken: impl Fn(u16) -> bool,
) -> Vec<(u16, u16)> {
    let mut out: Vec<(u16, u16)> = vec![];
    for s in found {
        let host = s.host_port();
        if host < 1024
            || host == api_port
            || fixed.iter().any(|(h, _)| *h == host)
            || out.iter().any(|(h, _)| *h == host)
            || taken(host)
        {
            continue;
        }
        out.push((host, s.port));
    }
    out
}

/// `http://127.0.0.1:<port>` for a published node port.
pub(crate) fn service_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// `http://<service>.<namespace>.localhost:<port>`: `*.localhost` resolves to loopback in browsers and on macOS,
/// so two services on different ports can be told apart by name.
pub(crate) fn friendly_url(namespace: &str, name: &str, port: u16) -> String {
    format!("http://{name}.{namespace}.localhost:{port}")
}

/// Whether something on the Mac already listens on this loopback port.
pub(crate) fn port_in_use(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// Reads the cluster's services through the server VM.
fn read_services(c: &Cluster) -> Result<Vec<NodePortService>> {
    let out = ssh(c, "sudo k3s kubectl get svc -A -o json")
        .output()
        .context("listing services")?;
    if !out.status.success() {
        bail!("listing services failed");
    }
    Ok(node_port_services(&serde_json::from_slice(&out.stdout)?))
}

/// Opens a loopback forward for every NodePort and LoadBalancer service and closes it when the service goes away.
/// `open` holds the node ports this process forwards now; returns true when the forward was changed.
async fn publish_services<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    open: &mut Vec<(u16, u16)>,
    names: Option<&crate::kube_names::Names>,
) -> Result<bool> {
    let mut cluster = ensure_vm(api, o).await?;
    let found = read_services(&cluster)?;
    let ours = open.clone();
    // Ports the recorded forwarder already holds are ours too, even when another process (`kube add`) opened them;
    // taking them for someone else's would drop and reopen them in turn, and every flip restarts the SSH forward.
    let held = forwarded_hosts(o);
    let desired = ports_to_open(&found, &cluster.publish, o.api_port, |p| {
        !ours.iter().any(|(h, _)| *h == p) && !held.contains(&p) && port_in_use(p)
    });
    if let Some(names) = names {
        let published: Vec<NodePortService> = found
            .iter()
            .filter(|s| desired.contains(&(s.host_port(), s.port)))
            .cloned()
            .collect();
        *names.lock().unwrap() = crate::kube_names::route_table(&published);
    }
    if desired == *open {
        return Ok(false);
    }
    for (h, _) in desired.iter().filter(|d| !open.contains(d)) {
        eprintln!("publishing {}", service_url(*h));
    }
    cluster.publish.extend(desired.iter().copied());
    ensure_forward(o, &cluster)?;
    *open = desired;
    Ok(true)
}

/// `fluxctl kube supervise`: keeps the VM running and the API port answering, for launchd. Never returns.
/// Unless `auto_publish` is off, it also forwards each NodePort and LoadBalancer service to 127.0.0.1.
#[allow(clippy::too_many_arguments)]
pub async fn supervise<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    every: Duration,
    auto_publish: bool,
    auto_checkpoint: Option<(Duration, usize)>,
    idle_sleep: Option<Duration>,
    sleep_on_power: (bool, bool),
    local_names: bool,
    notify: bool,
) -> Result<()> {
    validate(o)?;
    let names = (auto_publish && local_names).then(crate::kube_names::Names::default);
    if let Some(n) = &names {
        tokio::spawn(crate::kube_names::serve(n.clone()));
        // HTTPS only once `fluxctl kube trust` has made a CA; without one the names stay on plain http.
        if let Some(acceptor) = crate::kube_tls::acceptor() {
            tokio::spawn(crate::kube_names::serve_tls(n.clone(), acceptor));
        }
    }
    let mut idle = IdleWatch::default();
    let mut open: Vec<(u16, u16)> = vec![];
    let mut parked: Vec<u16> = vec![]; // the service ports that were published when the cluster went to sleep
    let mut last_auto = Instant::now();
    let mut pods = crate::kube_notify::Watch::default();
    let mut last_pods: Option<Instant> = None;
    loop {
        if sleep_in_progress(o) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        if crate::kube_checkpoint::is_asleep(api, o).await {
            if !open.is_empty() {
                parked = open.iter().map(|(h, _)| *h).collect();
            }
            open.clear();
            idle.reset();
            write_prompt(o, crate::kube_prompt::State::Asleep, &[]);
            wake_on_demand(
                api,
                o,
                every,
                &parked,
                auto_publish,
                &mut open,
                names.as_ref(),
            )
            .await;
            continue;
        }
        let mut healthy = api_answers(o.api_port)
            && matches!(api.find(&o.name).await, Ok(Some((_, s))) if s == "running");
        for i in 1..=o.workers {
            healthy = healthy
                && matches!(api.find(&worker_name(o, i)).await, Ok(Some((_, s))) if s == "running");
        }
        if !healthy {
            open.clear();
            match up(api, o).await {
                Ok(_) => {
                    eprintln!("kubernetes is up");
                    // A restore (the Studio's, or a sleep checkpoint) leaves the guest clock behind; step it now.
                    resync_clock(api, o).await;
                }
                Err(e) => eprintln!("kubernetes is not ready, will retry: {e:#}"),
            }
        }
        if auto_publish {
            match publish_services(api, o, &mut open, names.as_ref()).await {
                // remembered apart from `open`, which is cleared whenever the cluster looks down (a hand-run `kube sleep`
                // does that for a moment before supervise sees it is asleep)
                Ok(_) => parked = open.iter().map(|(h, _)| *h).collect(),
                Err(e) => eprintln!("publishing services, will retry: {e:#}"),
            }
        }
        if last_pods.is_none_or(|t| t.elapsed() >= POD_CHECK) {
            last_pods = Some(Instant::now());
            // Unreachable while the VM boots or sleeps; that is not worth a message.
            if let Ok(out) = guest_sh(api, o, "sudo k3s kubectl get pods -A --no-headers").await {
                let rows = crate::kube_doctor::parse_pods(&out);
                write_prompt(o, crate::kube_prompt::State::Running, &rows);
                if notify {
                    for alert in pods.update(&o.name, &rows) {
                        eprintln!("{}: {}", alert.title, alert.body);
                        crate::kube_notify::send(&alert);
                    }
                }
            } else if !healthy {
                write_prompt(o, crate::kube_prompt::State::Down, &[]);
            }
        }
        let watching = idle_sleep.is_some() || sleep_on_power.0 || sleep_on_power.1;
        if watching
            && healthy_enough(api, o).await
            && let Some(total) = forwarder_bytes(o)
        {
            let quiet = idle.tick(total, Instant::now());
            let why = if idle_sleep.is_some_and(|after| quiet >= after) {
                Some(format!("no traffic for {}s", quiet.as_secs()))
            } else if quiet >= POWER_QUIET {
                power_says_sleep(sleep_on_power.0, sleep_on_power.1).map(str::to_owned)
            } else {
                None
            };
            if let Some(why) = why {
                eprintln!("{why}: putting the cluster to sleep");
                idle.reset();
                match crate::kube_checkpoint::sleep(api, o).await {
                    // asleep: go straight back to the top, which opens the wake port at once
                    Ok(m) => eprintln!("{m}"),
                    Err(e) => {
                        eprintln!("could not sleep, will retry: {e:#}");
                        tokio::time::sleep(every).await;
                    }
                }
                continue;
            }
        }
        if let Some((gap, keep)) = auto_checkpoint
            && healthy_enough(api, o).await
            && last_auto.elapsed() >= gap
        {
            last_auto = Instant::now();
            match crate::kube_checkpoint::auto_checkpoint(api, o, keep).await {
                Ok(Some(m)) => eprintln!("{m}"),
                Ok(None) => {}
                Err(e) => eprintln!("automatic checkpoint failed, will retry later: {e:#}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// A parked cluster: holds 127.0.0.1:<api-port> open, and the ports of the services that were published when it went to
/// sleep (`parked`), and the first client to connect to any of them wakes it. That client waits for the cluster and is then
/// proxied through. Returns after one wake, or after `every` with nobody connecting.
async fn wake_on_demand<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    every: Duration,
    parked: &[u16],
    auto_publish: bool,
    open: &mut Vec<(u16, u16)>,
    names: Option<&crate::kube_names::Names>,
) {
    use tokio::net::{TcpListener, TcpStream};
    let mut waiting = tokio::task::JoinSet::new();
    for port in std::iter::once(o.api_port).chain(parked.iter().copied()) {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            waiting.spawn(async move { listener.accept().await.ok().map(|(c, _)| (port, c)) });
        }
    }
    if waiting.is_empty() {
        tokio::time::sleep(every).await; // something else holds the ports; try again later
        return;
    }
    let first = tokio::time::timeout(every, waiting.join_next()).await;
    waiting.shutdown().await; // closes every listener; the wake re-opens the real forwards on these ports
    let Ok(Some(Ok(Some((port, mut client))))) = first else {
        return;
    };
    eprintln!("a client connected to the sleeping cluster on port {port}: waking it");
    match crate::kube_checkpoint::wake(api, o).await {
        Ok(m) => eprintln!("{m}"),
        Err(e) => return eprintln!("waking failed, will retry: {e:#}"),
    }
    if port != o.api_port
        && auto_publish
        && let Err(e) = publish_services(api, o, open, names).await
    {
        return eprintln!("publishing services after the wake, will retry: {e:#}");
    }
    for _ in 0..40 {
        if let Ok(mut side) = TcpStream::connect(("127.0.0.1", port)).await {
            tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut client, &mut side).await;
            });
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await; // the service may need a moment after the restore
    }
}

/// The API answers and the server VM is running: a good moment to take a checkpoint.
async fn healthy_enough<A: VmApi>(api: &A, o: &KubeOptions) -> bool {
    api_answers(o.api_port) && matches!(api.find(&o.name).await, Ok(Some((_, s))) if s == "running")
}

/// `fluxctl kube services`: the cluster's node-port services and whether each is reachable on 127.0.0.1.
pub async fn services<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Value> {
    let cluster = ensure_vm(api, o).await?;
    let recorded = std::fs::read_to_string(forwarder_pid(o)).unwrap_or_default();
    let forwarded = |host: u16, guest: u16| {
        recorded
            .lines()
            .nth(1)
            .is_some_and(|spec| spec.split('|').any(|f| f == format!("{host}:{guest}")))
    };
    let items: Vec<Value> = read_services(&cluster)?
        .iter()
        .map(|s| {
            serde_json::json!({
                "namespace": s.namespace,
                "name": s.name,
                "port": s.host_port(),
                "node_port": s.port,
                "published": forwarded(s.host_port(), s.port),
                "url": service_url(s.host_port()),
                "friendly_url": friendly_url(&s.namespace, &s.name, s.host_port()),
            })
        })
        .collect();
    Ok(serde_json::json!({ "services": items }))
}

/// The LaunchAgent that runs `fluxctl kube supervise` at login and restarts it if it exits.
pub fn render_agent(
    fluxctl: &Path,
    o: &KubeOptions,
    log: &Path,
    env: &[(String, String)],
    extras: &[String],
) -> String {
    let mut args = vec![
        fluxctl.display().to_string(),
        "kube".into(),
        "supervise".into(),
        "--name".into(),
        o.name.clone(),
        "--cpus".into(),
        o.cpus.to_string(),
        "--memory-mib".into(),
        o.memory_mib.to_string(),
        "--disk-gib".into(),
        o.disk_gib.to_string(),
        "--api-port".into(),
        o.api_port.to_string(),
    ];
    let mut flag = |name: &str, value: String| {
        args.push(name.into());
        args.push(value);
    };
    if let Some(i) = &o.image {
        flag("--image", i.clone());
    }
    for s in &o.shares {
        flag("--share", s.clone());
    }
    if let Some(u) = &o.user {
        flag("--user", u.clone());
    }
    for p in &o.publish {
        flag("--publish", p.clone());
    }
    if let Some(v) = &o.k3s_version {
        flag("--k3s-version", v.clone());
    }
    for a in &o.k3s_args {
        flag("--k3s-arg", a.clone());
    }
    if let Some(k) = &o.kubeconfig {
        flag("--kubeconfig", k.display().to_string());
    }
    if o.workers > 0 {
        flag("--workers", o.workers.to_string());
    }
    if let Some(d) = &o.offline {
        flag("--offline", d.display().to_string());
    }
    flag("--cni", o.cni.name().to_owned());
    if o.cni == Cni::Cilium && o.cilium_version != CILIUM_VERSION {
        flag("--cilium-version", o.cilium_version.clone());
    }
    if o.traefik {
        args.push("--traefik".into());
    }
    args.extend(extras.iter().cloned());
    dh::render_plist(AGENT_LABEL, &args, log, env)
}

/// A container image reference: plain characters, never starting with a dash, so it cannot be read as a docker option.
pub fn image_ok(i: &str) -> bool {
    !i.is_empty()
        && i.len() <= 255
        && !i.starts_with('-')
        && i.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._/:@-".contains(&c))
}

/// A `docker save` archive that is deleted when it goes out of scope, whether the import worked or not.
struct TempArchive(PathBuf);

impl Drop for TempArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `fluxctl kube load`: copy images into every node's containerd, from the Docker CLI (`docker save`) or an archive.
/// Returns one line per node. Use a tag other than `:latest`, which Kubernetes pulls again by default.
pub async fn load<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    images: &[String],
    archive: Option<&Path>,
) -> Result<Vec<String>> {
    validate(o)?;
    match (images.is_empty(), archive) {
        (true, None) => bail!("name at least one image, or pass --archive FILE"),
        (false, Some(_)) => bail!("give images or --archive, not both"),
        _ => {}
    }
    if let Some(bad) = images.iter().find(|i| !image_ok(i)) {
        bail!("image {bad:?} is not a plain image reference");
    }
    if let Some(a) = archive
        && !a.is_file()
    {
        bail!("--archive {}: not a file", a.display());
    }
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (_, identity) = run::ssh_key(&home)?;
    let mut nodes = vec![o.name.clone()];
    nodes.extend(worker_vms(api, o).await?.into_iter().map(|(n, _, _)| n));
    let mut targets = vec![];
    for name in &nodes {
        let (id, status) = api
            .find(name)
            .await?
            .with_context(|| format!("{name} does not exist (run `fluxctl kube up`)"))?;
        if status != "running" {
            bail!("{name} is {status}, not running");
        }
        let ip = api
            .guest_ip(id)
            .await?
            .with_context(|| format!("{name} has no address yet"))?;
        targets.push((name.clone(), ip));
    }
    let saved;
    let path: PathBuf = match archive {
        Some(p) => p.to_path_buf(),
        None => {
            std::fs::create_dir_all(run_dir())?;
            saved = TempArchive(run_dir().join(format!(
                "kube-load-{}-{}.tar",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            )));
            let out = Command::new("docker")
                .arg("save")
                .arg("-o")
                .arg(&saved.0)
                .args(images)
                .output()
                .context("running `docker save` (install the Docker CLI; `fluxctl docker env` points it at the engine)")?;
            if !out.status.success() {
                bail!(
                    "docker save failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            saved.0.clone()
        }
    };
    let mut report = vec![];
    for (name, ip) in targets {
        // ssh exits 255 when the connection itself fails (sshd busy, a reset while the VM is under load). That says
        // nothing about the import, so try again a few times; any other failure is the import's and is reported.
        let mut attempt = 1;
        let out = loop {
            let mut c = run::ssh_base(&ip, &user, &identity);
            c.args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "sudo k3s ctr images import -",
            ]);
            c.stdin(
                std::fs::File::open(&path)
                    .with_context(|| format!("opening {}", path.display()))?,
            );
            let out = c.output().context("running ssh")?;
            if out.status.code() == Some(255) && attempt < IMPORT_ATTEMPTS {
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            break out;
        };
        if !out.status.success() {
            bail!(
                "importing into {name} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let imported = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("saved") || l.contains("importing") || l.contains("unpacking"))
            .count();
        report.push(format!("{name}: imported ({imported} steps reported)"));
    }
    Ok(report)
}

/// `hash  name` lines of a release's `sha256sum-*.txt`.
pub fn parse_sums(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (hash, name) = l.split_once(char::is_whitespace)?;
            let name = name.trim().trim_start_matches('*');
            (hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()) && !name.is_empty())
                .then(|| (name.to_owned(), hash.to_ascii_lowercase()))
        })
        .collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn curl(url: &str, out: Option<&Path>) -> Result<String> {
    let mut c = Command::new("curl");
    c.args(["-fSL", "--retry", "3", "--connect-timeout", "20"]);
    match out {
        Some(p) => {
            c.arg("-#").arg("-o").arg(p);
        }
        None => {
            c.arg("-s");
        }
    }
    let o = c
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .context("running curl")?;
    if !o.status.success() {
        bail!("downloading {url} failed (curl exit {:?})", o.status.code());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// `…/releases/tag/v1.31.4%2Bk3s1` -> `v1.31.4+k3s1`.
fn tag_of(url: &str) -> String {
    url.trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .replace("%2B", "+")
}

/// The k3s release the installer's stable channel currently points at, such as `v1.31.4+k3s1`.
fn stable_version() -> Result<String> {
    let o = Command::new("curl")
        .args([
            "-fsSL",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            "https://update.k3s.io/v1-release/channels/stable",
        ])
        .stdin(Stdio::null())
        .output()
        .context("running curl")?;
    let url = String::from_utf8_lossy(&o.stdout).into_owned();
    let tag = tag_of(&url);
    if !o.status.success() || !version_ok(&tag) {
        bail!("could not find the stable k3s release (got {url:?}); pass --k3s-version");
    }
    Ok(tag)
}

/// `fluxctl kube fetch DIR`: download k3s, its air-gap images and the installer into `dir`, verifying the binary and
/// the images against the release's SHA-256 list. The folder is what `--offline` installs from.
pub fn fetch_offline(dir: &Path, version: Option<&str>) -> Result<Value> {
    let version = match version {
        Some(v) if version_ok(v) => v.to_owned(),
        Some(v) => bail!("--k3s-version {v:?} must look like v1.31.4+k3s1"),
        None => stable_version()?,
    };
    std::fs::create_dir_all(dir)?;
    let dir = dir.canonicalize()?;
    if !offline_path_ok(&dir) {
        bail!(
            "{}: use a folder whose path has only letters, digits and - _ . /",
            dir.display()
        );
    }
    let base = format!(
        "https://github.com/k3s-io/k3s/releases/download/{}",
        version.replace('+', "%2B")
    );
    let sums = parse_sums(&curl(&format!("{base}/{OFFLINE_SUMS}"), None)?);
    let mut files = vec![];
    for (name, url, verify) in [
        (OFFLINE_BINARY, format!("{base}/{OFFLINE_BINARY}"), true),
        (OFFLINE_IMAGES, format!("{base}/{OFFLINE_IMAGES}"), true),
        (OFFLINE_INSTALLER, "https://get.k3s.io".to_owned(), false),
    ] {
        eprintln!("downloading {name}…");
        let part = TempArchive(dir.join(format!(".{name}.part"))); // removed again on any failure
        let tmp = part.0.clone();
        curl(&url, Some(&tmp))?;
        let hash = sha256_file(&tmp)?;
        if verify {
            let want = sums
                .get(name)
                .with_context(|| format!("{OFFLINE_SUMS} has no entry for {name}"))?;
            if *want != hash {
                bail!("{name}: SHA-256 {hash} does not match the release's {want}");
            }
        }
        let mode = if name == OFFLINE_IMAGES { 0o644 } else { 0o755 };
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(mode))?;
        let size = std::fs::metadata(&tmp)?.len();
        std::fs::rename(&tmp, dir.join(name))?;
        files.push(json!({"name": name, "bytes": size, "sha256": hash, "verified": verify}));
    }
    Ok(json!({"version": version, "dir": dir, "files": files}))
}

fn agent_paths(home: &Path) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-kube.log"),
    )
}

/// `fluxctl kube agent install`: write the LaunchAgent and load it.
pub fn agent_install(o: &KubeOptions, extras: &[String]) -> Result<PathBuf> {
    validate(o)?;
    let (plist, log) = agent_paths(&dh::home()?);
    let exe = std::env::current_exe().context("finding fluxctl's own path")?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(
        &plist,
        render_agent(&exe, o, &log, &dh::carried_env(), extras),
    )?;
    dh::launchctl_load(AGENT_LABEL, &plist)?;
    Ok(plist)
}

/// `fluxctl kube agent uninstall`.
pub fn agent_uninstall() -> Result<bool> {
    let (plist, _) = agent_paths(&dh::home()?);
    dh::launchctl_unload(AGENT_LABEL, &plist)
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S_YAML: &str = "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: QUJD\n    server: https://127.0.0.1:6443\n  name: default\ncontexts:\n- context:\n    cluster: default\n    user: default\n  name: default\ncurrent-context: default\nkind: Config\npreferences: {}\nusers:\n- name: default\n  user:\n    client-certificate-data: REVG\n    client-key-data: R0hJ\n";

    #[test]
    fn spec_installs_k3s_with_the_right_flags() {
        let o = KubeOptions::default();
        let spec = kube_spec(&o, "mini", "ssh-ed25519 AAAA test", vec![]);
        assert_eq!(spec["name"], "fluxvm-kube");
        assert_eq!(spec["cloud_init"]["hostname"], "fluxvm-kube");
        assert_eq!(spec["memory_mib"], 4096);
        assert!(
            spec["cloud_init"]["runcmd"]
                .as_array()
                .unwrap()
                .iter()
                .all(Value::is_string)
        );
        let run = spec["cloud_init"]["runcmd"][0].as_str().unwrap();
        assert!(
            run.starts_with("curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC='server "),
            "{run}"
        );
        assert!(run.contains("--disable traefik") && run.contains("--tls-san 127.0.0.1"));
        assert!(!run.contains("INSTALL_K3S_VERSION"));
    }

    #[test]
    fn options_change_the_install_line() {
        let o = KubeOptions {
            k3s_version: Some("v1.31.4+k3s1".into()),
            k3s_args: vec!["--disable=metrics-server".into()],
            traefik: true,
            ..Default::default()
        };
        let run = kube_spec(&o, "u", "k", vec![])["cloud_init"]["runcmd"][0]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            run.contains("INSTALL_K3S_VERSION='v1.31.4+k3s1' INSTALL_K3S_EXEC="),
            "{run}"
        );
        assert!(!run.contains("--disable traefik"));
        assert!(run.contains("--disable=metrics-server"));
    }

    #[test]
    fn names_versions_and_args_are_validated() {
        for ok in ["fluxvm-kube", "a", "k8s-1"] {
            assert!(name_ok(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "A", "a_b", "a b", "a.b", &"x".repeat(41)] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        for ok in ["v1.31.4+k3s1", "v1.30.0", "v1.2.3+k3s12"] {
            assert!(version_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "1.31.4",
            "v1.31",
            "v1.31.4+k3s",
            "v1.31.4+rke2",
            "v1.31.4'; rm -rf /",
            "v1.x.4",
        ] {
            assert!(!version_ok(bad), "{bad:?}");
        }
        for ok in ["--disable=servicelb", "--cluster-cidr=10.42.0.0/16", "-v"] {
            assert!(k3s_arg_ok(ok), "{ok}");
        }
        for bad in ["", "a b", "x'y", "x;y", "$(id)", "a`b`", "a\nb"] {
            assert!(!k3s_arg_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn validate_rejects_a_port_clash_and_bad_input() {
        assert!(validate(&KubeOptions::default()).is_ok());
        assert!(
            validate(&KubeOptions {
                publish: vec!["6443:80".into()],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&KubeOptions {
                publish: vec!["30080".into()],
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&KubeOptions {
                name: "Bad".into(),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&KubeOptions {
                api_port: 0,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate(&KubeOptions {
                k3s_args: vec!["a b".into()],
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn kubeconfig_is_pointed_at_the_forward_and_renamed() {
        let out = rewrite_kubeconfig(K3S_YAML, "dev", 16443);
        assert!(out.contains("    server: https://127.0.0.1:16443\n"));
        assert!(!out.contains("default"));
        assert!(out.contains("  name: dev\n") && out.contains("- name: dev\n"));
        assert!(out.contains("    cluster: dev\n    user: dev\n"));
        assert!(out.contains("current-context: dev\n"));
        // Credentials and structure are untouched.
        assert!(
            out.contains("certificate-authority-data: QUJD")
                && out.contains("client-key-data: R0hJ")
        );
        assert_eq!(
            rewrite_kubeconfig(&out, "dev", 16443),
            out,
            "rewriting is idempotent"
        );
    }

    #[test]
    fn api_port_check_sees_a_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(api_answers(port));
        drop(l);
        // another test may briefly bind the same ephemeral port, so give it a moment to let go
        let gone = (0..40).any(|_| {
            if api_answers(port) {
                std::thread::sleep(Duration::from_millis(50));
                false
            } else {
                true
            }
        });
        assert!(gone);
    }

    #[test]
    fn forward_spec_changes_when_ports_change() {
        let mk = |publish: Vec<(u16, u16)>| Cluster {
            id: Uuid::nil(),
            ip: "192.168.64.7".into(),
            user: "mini".into(),
            identity: None,
            api_port: 6443,
            publish,
            kubeconfig: PathBuf::from("/x"),
        };
        assert_ne!(
            forward_spec(&mk(vec![])),
            forward_spec(&mk(vec![(30080, 30080)]))
        );
        assert_eq!(
            forward_spec(&mk(vec![(30080, 80)])),
            "mini@192.168.64.7|6443|30080:80"
        );
    }

    #[test]
    fn env_line_and_default_kubeconfig_path() {
        assert_eq!(
            env_line(Path::new("/h/.kube/x.yaml")),
            "export KUBECONFIG=/h/.kube/x.yaml"
        );
        let o = KubeOptions {
            kubeconfig: Some("/tmp/k.yaml".into()),
            ..Default::default()
        };
        assert_eq!(home_kube(&o).unwrap(), PathBuf::from("/tmp/k.yaml"));
        let d = home_kube(&KubeOptions::default()).unwrap();
        assert!(d.ends_with(".kube/fluxvm-fluxvm-kube.yaml"), "{d:?}");
    }

    #[test]
    fn private_files_are_mode_600_and_replaced_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k.yaml");
        write_private(&p, "one").unwrap();
        write_private(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!p.with_extension("yaml.tmp").exists());
    }

    #[test]
    fn agent_plist_runs_kube_supervise_with_the_options() {
        let o = KubeOptions {
            name: "dev".into(),
            api_port: 16443,
            publish: vec!["30080".into()],
            k3s_version: Some("v1.31.4+k3s1".into()),
            traefik: true,
            ..Default::default()
        };
        let env = vec![("FLUXVM_URL".to_owned(), "http://127.0.0.1:7788".to_owned())];
        let p = render_agent(
            Path::new("/opt/homebrew/bin/fluxctl"),
            &o,
            Path::new("/tmp/log"),
            &env,
            &[],
        );
        for want in [
            AGENT_LABEL,
            "<string>kube</string>",
            "<string>supervise</string>",
            "<string>16443</string>",
            "<string>--publish</string>",
            "<string>v1.31.4+k3s1</string>",
            "<string>--traefik</string>",
            "<key>FLUXVM_URL</key>",
            "<key>KeepAlive</key><true/>",
        ] {
            assert!(p.contains(want), "{want}");
        }
        assert!(!p.contains("TOKEN"));
    }

    fn agent() -> Role {
        Role::Agent {
            url: "https://10.89.1.2:6443".into(),
            token: "K10abc::server:xyz".into(),
        }
    }

    #[test]
    fn workers_join_over_a_private_network_with_the_servers_token() {
        let o = KubeOptions {
            name: "dev".into(),
            workers: 2,
            k3s_version: Some("v1.31.4+k3s1".into()),
            ..Default::default()
        };
        assert_eq!(worker_name(&o, 2), "dev-w2");
        let worker = node_spec(&o, &agent(), "dev-w1", "u", "k", vec![]);
        assert_eq!(worker["name"], "dev-w1");
        assert_eq!(worker["cloud_init"]["hostname"], "dev-w1");
        // Both nodes get a second card on a private network named after the cluster.
        assert_eq!(worker["apple"]["networks"][0]["name"], "dev");
        let server = kube_spec(&o, "u", "k", vec![]);
        assert_eq!(server["apple"]["networks"][0]["name"], "dev");
        let w = worker["cloud_init"]["runcmd"][0].as_str().unwrap();
        assert!(
            w.starts_with("systemd-run --no-block --unit=fluxvm-k3s-install /bin/bash -c '"),
            "{w}"
        );
        assert!(
            w.contains("PRIV=$(ip -4 -o addr show | grep -o \"10\\.89\\."),
            "waits for the private address: {w}"
        );
        assert!(w.contains("INSTALL_K3S_VERSION=\"v1.31.4+k3s1\""), "{w}");
        assert!(
            w.contains("K3S_URL=\"https://10.89.1.2:6443\" K3S_TOKEN=\"K10abc::server:xyz\""),
            "{w}"
        );
        assert!(
            w.contains("INSTALL_K3S_EXEC=\"agent --node-name dev-w1 --node-ip $PRIV\""),
            "{w}"
        );
        assert!(
            !w.contains("--disable traefik"),
            "agents take no server flags"
        );
        assert_eq!(w.matches('\'').count(), 2, "{w}");
        // The server advertises its private address too, and carries no token.
        let sv = server["cloud_init"]["runcmd"][0].as_str().unwrap();
        assert!(
            sv.contains("--node-ip $PRIV") && sv.contains("--disable traefik"),
            "{sv}"
        );
        assert!(!sv.contains("K3S_TOKEN"), "{sv}");
        assert_eq!(sv.matches('\'').count(), 2, "{sv}");
    }

    #[test]
    fn a_single_node_keeps_the_plain_installer_line_and_no_private_network() {
        let o = KubeOptions::default();
        let spec = kube_spec(&o, "u", "k", vec![]);
        assert_eq!(spec["apple"], json!({}));
        let c = spec["cloud_init"]["runcmd"][0].as_str().unwrap();
        assert!(
            c.starts_with("curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC='server "),
            "{c}"
        );
        assert!(!c.contains("PRIV"), "{c}");
    }

    #[test]
    fn offline_nodes_wait_for_the_folder_then_install_from_it() {
        let o = KubeOptions {
            offline: Some("/Users/me/k3s".into()),
            ..Default::default()
        };
        let spec = kube_spec(&o, "u", "k", vec![]);
        assert_eq!(spec["cloud_init"]["packages"].as_array().unwrap().len(), 0);
        let cmds = spec["cloud_init"]["runcmd"].as_array().unwrap();
        assert_eq!(
            cmds.len(),
            1,
            "one transient unit, so cloud-init is not blocked"
        );
        let c = cmds[0].as_str().unwrap();
        assert!(
            c.starts_with("systemd-run --no-block --unit=fluxvm-k3s-install /bin/bash -c '"),
            "{c}"
        );
        assert!(!c.contains("get.k3s.io") && !c.contains("curl"), "{c}");
        // It waits for the files (FluxVM mounts the share after the user's commands), then installs.
        assert!(
            c.contains("for i in $(seq 1 150); do [ -f \"/Users/me/k3s/k3s-arm64\" ]"),
            "{c}"
        );
        assert!(c.contains(
            "cp \"/Users/me/k3s/k3s-airgap-images-arm64.tar.zst\" /var/lib/rancher/k3s/agent/images/"
        ));
        assert!(c.contains("install -m 0755 \"/Users/me/k3s/k3s-arm64\" /usr/local/bin/k3s"));
        assert!(c.contains("INSTALL_K3S_SKIP_DOWNLOAD=true INSTALL_K3S_EXEC=\"server "));
        assert!(c.contains("sh \"/Users/me/k3s/install.sh\"") && c.ends_with('\''));
        assert!(
            !c.contains("PRIV"),
            "a single node needs no private address: {c}"
        );
        // Single quotes appear only around the whole script, so the shell nesting holds.
        assert_eq!(c.matches('\'').count(), 2, "{c}");
        let worker = node_spec(&o, &agent(), "w", "u", "k", vec![]);
        let w = worker["cloud_init"]["runcmd"][0].as_str().unwrap();
        assert!(
            w.contains(
                "K3S_URL=\"https://10.89.1.2:6443\" K3S_TOKEN=\"K10abc::server:xyz\" INSTALL_K3S_SKIP_DOWNLOAD=true"
            ),
            "{w}"
        );
        assert_eq!(w.matches('\'').count(), 2, "{w}");
    }

    #[test]
    fn workers_offline_and_tokens_are_validated() {
        assert!(
            validate(&KubeOptions {
                workers: 8,
                ..Default::default()
            })
            .is_ok()
        );
        assert!(
            validate(&KubeOptions {
                workers: 9,
                ..Default::default()
            })
            .is_err()
        );
        for ok in ["/Users/me/k3s", "/opt/k3s-1.31/offline_dir"] {
            assert!(offline_path_ok(Path::new(ok)), "{ok}");
        }
        for bad in [
            "relative/dir",
            "/a/../b",
            "/Users/me/my k3s",
            "/x'y",
            "/x;y",
            "/x$y",
            "/a\"b",
        ] {
            assert!(!offline_path_ok(Path::new(bad)), "{bad:?}");
        }
        assert!(
            validate(&KubeOptions {
                offline: Some("/a b".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(token_ok("K10ab12::server:cd34"));
        for bad in ["", "a b", "x'y", "a;b", "$(id)", "a\nb"] {
            assert!(!token_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_incomplete_offline_folder_names_the_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_offline(dir.path()).unwrap_err().to_string();
        assert!(err.contains(OFFLINE_BINARY), "{err}");
        for f in [OFFLINE_BINARY, OFFLINE_IMAGES, OFFLINE_INSTALLER] {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        assert!(check_offline(dir.path()).is_ok());
    }

    #[test]
    fn image_references_are_plain() {
        for ok in [
            "app:dev",
            "ghcr.io/zyvorai/app:1.2.3",
            "alpine@sha256:abcd",
            "a/b_c-d.e:f",
        ] {
            assert!(image_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "-v",
            "--privileged",
            "a b",
            "a;b",
            "$(id)",
            "a'b",
            &"x".repeat(256),
        ] {
            assert!(!image_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn release_checksums_parse_and_files_hash() {
        let h = "a".repeat(64);
        let text = format!(
            "{h}  k3s-arm64\n{} *k3s-airgap-images-arm64.tar.zst\nnot a line\nshort  x\n",
            "B".repeat(64)
        );
        let m = parse_sums(&text);
        assert_eq!(m.len(), 2);
        assert_eq!(m["k3s-arm64"], h);
        assert_eq!(m["k3s-airgap-images-arm64.tar.zst"], "b".repeat(64));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn release_tags_come_from_the_redirect_url() {
        assert_eq!(
            tag_of("https://github.com/k3s-io/k3s/releases/tag/v1.36.5+k3s1"),
            "v1.36.5+k3s1"
        );
        assert_eq!(
            tag_of("https://github.com/k3s-io/k3s/releases/tag/v1.31.4%2Bk3s1\n"),
            "v1.31.4+k3s1"
        );
        assert!(!version_ok(&tag_of(
            "https://update.k3s.io/v1-release/channels/stable"
        )));
    }

    #[test]
    fn plist_carries_workers_and_offline() {
        let o = KubeOptions {
            workers: 2,
            offline: Some("/Users/me/k3s".into()),
            ..Default::default()
        };
        let p = render_agent(Path::new("/x/fluxctl"), &o, Path::new("/l"), &[], &[]);
        for want in [
            "<string>--workers</string>",
            "<string>2</string>",
            "<string>--offline</string>",
            "<string>/Users/me/k3s</string>",
        ] {
            assert!(p.contains(want), "{want}");
        }
    }

    #[test]
    fn cilium_is_the_default_pod_network() {
        let o = KubeOptions::default();
        assert_eq!(o.cni, Cni::Cilium);
        assert_eq!(o.cilium_version, CILIUM_VERSION);
        // k3s starts without flannel or its network-policy controller.
        let exec = k3s_exec(&o);
        assert!(
            exec.contains("--flannel-backend=none") && exec.contains("--disable-network-policy"),
            "{exec}"
        );
        // The server gets the chart as a root-only manifest; other nodes get none.
        let server = kube_spec(&o, "u", "k", vec![]);
        let f = &server["cloud_init"]["write_files"][0];
        assert_eq!(
            f["path"],
            "/var/lib/rancher/k3s/server/manifests/cilium.yaml"
        );
        assert_eq!(f["permissions"], "0600");
        let m = f["content"].as_str().unwrap();
        assert!(
            m.contains("kind: HelmChart") && m.contains("bootstrap: true"),
            "{m}"
        );
        assert!(
            m.contains(&format!("version: {CILIUM_VERSION}\n"))
                && m.contains("repo: https://helm.cilium.io"),
            "{m}"
        );
        let worker = node_spec(&o, &agent(), "w", "u", "k", vec![]);
        assert_eq!(
            worker["cloud_init"]["write_files"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(
            !install_commands(&o, &agent(), "w")
                .join(" ")
                .contains("flannel")
        );
    }

    #[test]
    fn flannel_keeps_k3s_defaults_and_offline_cannot_use_cilium() {
        let o = KubeOptions {
            cni: Cni::Flannel,
            ..Default::default()
        };
        assert!(!k3s_exec(&o).contains("flannel"));
        let spec = kube_spec(&o, "u", "k", vec![]);
        assert_eq!(
            spec["cloud_init"]["write_files"].as_array().unwrap().len(),
            0
        );
        assert!(validate(&o).is_ok());
        let offline = KubeOptions {
            offline: Some("/Users/me/k3s".into()),
            ..Default::default()
        };
        let err = validate(&offline).unwrap_err().to_string();
        assert!(err.contains("--cni flannel"), "{err}");
        let flannel_offline = KubeOptions {
            cni: Cni::Flannel,
            ..offline
        };
        assert!(validate(&flannel_offline).is_ok());
        assert_eq!(Cni::default_for(true), Cni::Flannel);
        assert_eq!(Cni::default_for(false), Cni::Cilium);
        assert_eq!(Cni::parse("cilium").unwrap(), Cni::Cilium);
        assert!(Cni::parse("calico").is_err());
    }

    #[test]
    fn cilium_versions_are_plain() {
        for ok in ["1.20.3", "1.9.10", "10.0.0"] {
            assert!(cilium_version_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "1.20",
            "1.20.3.4",
            "v1.20.3",
            "1.x.3",
            "1.20.3'; id",
            "1..3",
        ] {
            assert!(!cilium_version_ok(bad), "{bad:?}");
        }
        let o = KubeOptions {
            cilium_version: "1.20".into(),
            ..Default::default()
        };
        assert!(validate(&o).is_err());
    }

    #[test]
    fn flannel_is_pinned_to_the_private_card_on_every_node() {
        let o = KubeOptions {
            cni: Cni::Flannel,
            workers: 1,
            ..Default::default()
        };
        for run in [
            install_commands(&o, &Role::Server, "s"),
            install_commands(&o, &agent(), "w"),
        ] {
            let c = &run[0];
            assert!(
                c.contains("IFACE=$(ip -o -4 addr show | grep -F \"$PRIV/\" | cut -d\" \" -f2)"),
                "{c}"
            );
            assert!(c.contains("--node-ip $PRIV --flannel-iface $IFACE"), "{c}");
            assert_eq!(c.matches('\'').count(), 2, "{c}");
        }
        // Cilium needs no interface; a single node needs neither.
        let cilium = install_commands(
            &KubeOptions {
                workers: 1,
                ..Default::default()
            },
            &Role::Server,
            "s",
        );
        assert!(cilium[0].contains("--node-ip $PRIV") && !cilium[0].contains("--flannel-iface"));
        let single = install_commands(
            &KubeOptions {
                cni: Cni::Flannel,
                ..Default::default()
            },
            &Role::Server,
            "s",
        );
        assert!(!single[0].contains("IFACE"));
    }

    #[test]
    fn the_default_image_installs_no_packages_so_apt_cannot_stall_first_boot() {
        let o = KubeOptions::default();
        for spec in [
            kube_spec(&o, "u", "k", vec![]),
            node_spec(&o, &agent(), "w", "u", "k", vec![]),
        ] {
            assert_eq!(
                spec["cloud_init"]["packages"],
                json!([]),
                "apt is not needed on debian-13"
            );
        }
        // Another image may not have curl, so it gets curl and the certificates, and nothing else.
        let custom = KubeOptions {
            image: Some("ubuntu-24.04".into()),
            ..Default::default()
        };
        assert_eq!(
            kube_spec(&custom, "u", "k", vec![])["cloud_init"]["packages"],
            json!(["curl", "ca-certificates"])
        );
        let offline = KubeOptions {
            image: Some("ubuntu-24.04".into()),
            offline: Some("/a".into()),
            cni: Cni::Flannel,
            ..Default::default()
        };
        assert_eq!(
            kube_spec(&offline, "u", "k", vec![])["cloud_init"]["packages"],
            json!([])
        );
    }

    #[test]
    fn node_ports_come_from_nodeport_and_loadbalancer_services_only() {
        let list = serde_json::json!({"items": [
            {"metadata": {"namespace": "web", "name": "a"}, "spec": {"type": "NodePort",
                "ports": [{"nodePort": 30080}, {"nodePort": 30443}]}},
            {"metadata": {"namespace": "web", "name": "lb"}, "spec": {"type": "LoadBalancer",
                "ports": [{"nodePort": 31000}]}},
            {"metadata": {"namespace": "web", "name": "c"}, "spec": {"type": "ClusterIP",
                "ports": [{"port": 80}]}},
        ]});
        let ports: Vec<u16> = node_port_services(&list).iter().map(|s| s.port).collect();
        assert_eq!(ports, vec![30080, 30443, 31000]);
    }

    #[test]
    fn ports_to_open_skips_low_reserved_duplicate_and_taken_ports() {
        let svc = |port| NodePortService {
            namespace: "n".into(),
            name: "s".into(),
            port,
            local: None,
            service_port: 0,
        };
        let found = [
            svc(80),
            svc(6443),
            svc(30080),
            svc(30080),
            svc(30081),
            svc(30082),
        ];
        let fixed = [(30082, 80)];
        let got = ports_to_open(&found, &fixed, 6443, |p| p == 30081);
        assert_eq!(got, vec![(30080, 30080)]);
    }

    #[test]
    fn the_forwarders_ports_are_read_from_its_spec() {
        assert_eq!(
            spec_hosts("me@192.168.64.7|6443|30080:30080|5432:30432"),
            vec![30080, 5432]
        );
        assert!(spec_hosts("me@192.168.64.7|6443").is_empty());
        assert!(spec_hosts("").is_empty());
    }

    #[test]
    fn a_service_can_ask_for_its_own_port_on_the_mac() {
        assert_eq!(
            parse_local_ports("30432=5432, 30900=9000,bad,x=1"),
            vec![(30432, 5432), (30900, 9000)]
        );
        let list = serde_json::json!({"items": [
            {"metadata": {"namespace": "default", "name": "postgres",
                "annotations": {"fluxvm.dev/local-ports": "30432=5432"}},
             "spec": {"type": "NodePort", "ports": [{"nodePort": 30432}]}},
            {"metadata": {"namespace": "default", "name": "web"},
             "spec": {"type": "NodePort", "ports": [{"nodePort": 30080}]}},
        ]});
        let found = node_port_services(&list);
        let pg = found.iter().find(|s| s.name == "postgres").unwrap();
        assert_eq!((pg.port, pg.host_port()), (30432, 5432));
        assert_eq!(
            found.iter().find(|s| s.name == "web").unwrap().host_port(),
            30080
        );
        // the Mac port is the one opened, forwarding to the node port; a busy Mac port is skipped
        assert_eq!(
            ports_to_open(&found, &[], 6443, |_| false),
            vec![(30080, 30080), (5432, 30432)]
        );
        assert_eq!(
            ports_to_open(&found, &[], 6443, |p| p == 5432),
            vec![(30080, 30080)]
        );
    }

    const OURS: &str = "apiVersion: v1\nclusters:\n- cluster:\n    server: https://127.0.0.1:6443\n  name: dev\ncontexts:\n- context:\n    cluster: dev\n    user: dev\n  name: dev\ncurrent-context: dev\nkind: Config\nusers:\n- name: dev\n  user:\n    token: t\n";
    const MINE: &str = "apiVersion: v1\nclusters:\n- cluster:\n    server: https://prod\n  name: prod\ncontexts:\n- context:\n    cluster: prod\n    user: me\n  name: prod\ncurrent-context: prod\nkind: Config\nusers:\n- name: me\n  user:\n    token: secret\n";

    #[test]
    fn merging_adds_the_cluster_and_never_steals_the_current_context() {
        let out = merge_kubeconfig(Some(MINE), OURS, "dev", false).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(
            v["current-context"].as_str(),
            Some("prod"),
            "someone's prod context stays current"
        );
        assert_eq!(v["clusters"].as_sequence().unwrap().len(), 2);
        assert_eq!(v["users"].as_sequence().unwrap().len(), 2);
        assert_eq!(v["contexts"].as_sequence().unwrap().len(), 2);
        assert!(out.contains("token: secret"), "their entries are kept");
        // merging again changes nothing
        assert_eq!(
            merge_kubeconfig(Some(&out), OURS, "dev", false).unwrap(),
            out
        );
        // `use` switches
        let used = merge_kubeconfig(Some(&out), OURS, "dev", true).unwrap();
        assert!(used.contains("current-context: dev"));
    }

    #[test]
    fn merging_into_nothing_makes_it_current_and_unmerging_removes_it() {
        let out = merge_kubeconfig(None, OURS, "dev", false).unwrap();
        assert!(out.contains("current-context: dev"));
        let back = unmerge_kubeconfig(&out, "dev").unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&back).unwrap();
        assert!(v["clusters"].as_sequence().unwrap().is_empty());
        assert_eq!(v["current-context"].as_str(), Some(""));
        let kept = unmerge_kubeconfig(
            &merge_kubeconfig(Some(MINE), OURS, "dev", false).unwrap(),
            "dev",
        )
        .unwrap();
        assert!(
            kept.contains("name: prod")
                && !kept.contains("name: dev")
                && kept.contains("current-context: prod")
        );
        assert!(merge_kubeconfig(Some("- not a mapping"), OURS, "dev", false).is_err());
        assert!(kubeconfig_is_empty(&back));
        assert!(!kubeconfig_is_empty(&kept));
    }

    #[test]
    fn friendly_url_names_the_service() {
        assert_eq!(
            friendly_url("shop", "web", 30080),
            "http://web.shop.localhost:30080"
        );
    }

    #[test]
    fn service_url_is_loopback_only() {
        assert_eq!(service_url(30080), "http://127.0.0.1:30080");
    }

    #[test]
    fn nettop_csv_gives_total_bytes() {
        let csv = "time,,interface,state,bytes_in,bytes_out,rx_dupe,\n11:20:59.876,ssh.20565,,,139406,199354,648,\n";
        assert_eq!(parse_nettop(csv), Some(139406 + 199354));
        assert_eq!(parse_nettop("time,,state\n"), None);
        assert_eq!(parse_nettop(""), None);
    }

    #[test]
    fn idle_watch_counts_quiet_time_and_restarts_when_traffic_moves() {
        let t0 = Instant::now();
        let s = |n| Duration::from_secs(n);
        let mut w = IdleWatch::default();
        assert_eq!(w.tick(1000, t0), Duration::ZERO); // first reading
        assert_eq!(w.tick(1500, t0 + s(30)), s(30)); // keep-alives only
        assert_eq!(w.tick(1600, t0 + s(60)), s(60));
        assert_eq!(w.tick(900_000, t0 + s(90)), Duration::ZERO); // a kubectl burst
        assert_eq!(w.tick(900_100, t0 + s(120)), s(30));
        assert_eq!(w.tick(10, t0 + s(150)), Duration::ZERO); // forwarder restarted
    }

    #[test]
    fn power_state_is_read_from_pmset_and_ioreg() {
        assert!(on_battery(
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t80%; discharging"
        ));
        assert!(!on_battery(
            "Now drawing from 'AC Power'\n -InternalBattery-0\t80%; charging"
        ));
        assert!(!on_battery(""));
        assert!(lid_closed("    | |   \"AppleClamshellState\" = Yes\n"));
        assert!(!lid_closed("    | |   \"AppleClamshellState\" = No\n"));
        assert!(!lid_closed(""));
    }

    #[test]
    fn supervise_flags_become_agent_arguments_and_are_checked() {
        let f = SuperviseFlags {
            no_auto_publish: true,
            auto_checkpoint: Some(3600),
            keep_auto: 4,
            idle_sleep: Some(1800),
            no_names: true,
            no_notify: true,
            sleep_on_battery: true,
            sleep_on_lid_close: true,
        };
        assert_eq!(
            f.args(),
            [
                "--no-auto-publish",
                "--auto-checkpoint",
                "3600",
                "--keep-auto",
                "4",
                "--idle-sleep",
                "1800",
                "--no-names",
                "--no-notify",
                "--sleep-on-battery",
                "--sleep-on-lid-close"
            ]
        );
        assert!(SuperviseFlags::default().args().is_empty());
        assert!(f.check().is_ok());
        assert!(
            SuperviseFlags {
                idle_sleep: Some(30),
                keep_auto: 6,
                ..Default::default()
            }
            .check()
            .is_err()
        );
        assert!(
            SuperviseFlags {
                keep_auto: 0,
                ..Default::default()
            }
            .check()
            .is_err()
        );
    }
}
