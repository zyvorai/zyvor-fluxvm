// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl kube`: a single-node Kubernetes (k3s) cluster inside a FluxVM VM, reachable from the Mac.
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
        }
    }
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

fn validate(o: &KubeOptions) -> Result<()> {
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
    let ports = parse_publish_all(&o.publish)?;
    if ports.iter().any(|(h, _)| *h == o.api_port) {
        bail!("--publish: host port {} is the API port", o.api_port);
    }
    Ok(())
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

fn forwarder_pid(o: &KubeOptions) -> PathBuf {
    pid_file(&run_dir().join(format!("kube-{}.sock", o.name)))
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
    a.extend(o.k3s_args.iter().cloned());
    a.join(" ")
}

/// The VM request: Debian 13, k3s installed on first boot, the user's SSH key, folders shared at matching paths.
pub fn kube_spec(o: &KubeOptions, user: &str, key: &str, shares: Vec<Value>) -> Value {
    let version = o
        .k3s_version
        .as_ref()
        .map(|v| format!("INSTALL_K3S_VERSION='{v}' "))
        .unwrap_or_default();
    json!({
        "name": o.name,
        "backend": "auto",
        "image": o.image.clone().unwrap_or_else(|| "debian-13".into()),
        "vcpus": o.cpus,
        "memory_mib": o.memory_mib,
        "disk_size_gib": o.disk_gib,
        "network": {"mode": "user", "forwards": []},
        "shared_folders": shares,
        "cloud_init": {
            "hostname": o.name,
            "user": user,
            "ssh_authorized_keys": [key],
            "packages": ["curl", "ca-certificates", "iptables", "git", "jq"],
            "runcmd": [
                format!("curl -sfL https://get.k3s.io | {version}INSTALL_K3S_EXEC='{}' sh -", k3s_exec(o)),
            ],
        },
    })
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

/// Finds or creates the VM, starts it if stopped, and waits until the node reports Ready.
pub async fn ensure_vm<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Cluster> {
    validate(o)?;
    let home = dh::home()?;
    let user = dh::user_name(&dh::EngineOptions {
        user: o.user.clone(),
        ..Default::default()
    })?;
    let (key, identity) = run::ssh_key(&home)?;
    let started = Instant::now();
    let id = match api.find(&o.name).await? {
        Some((id, status)) => {
            if status != "running" {
                eprintln!("starting {} ({status})…", o.name);
                api.start(id).await?;
            }
            id
        }
        None => {
            let cwd = std::env::current_dir()?;
            let shares = o
                .shares
                .iter()
                .map(|s| parse_share(s, &cwd, Some(&home)))
                .collect::<Result<Vec<_>>>()?;
            eprintln!(
                "creating {} (first boot installs k3s, this takes a few minutes)…",
                o.name
            );
            let id = api.create(kube_spec(o, &user, &key, shares)).await?;
            let labels =
                std::collections::BTreeMap::from([("fluxvm.kube".to_owned(), "1".to_owned())]);
            let _ = api.set_labels(id, labels).await; // a convenience for `list`, not needed to work
            id
        }
    };
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
    let mut last = 0u64;
    loop {
        let ready = ssh(
            &cluster,
            "sudo k3s kubectl get --raw /readyz 2>/dev/null | grep -qx ok && sudo k3s kubectl get nodes --no-headers 2>/dev/null | grep -q ' Ready'",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running ssh")?
        .success();
        if ready {
            break;
        }
        if started.elapsed() > FIRST_BOOT_BUDGET {
            bail!(
                "Kubernetes did not become ready in the VM within {}s",
                FIRST_BOOT_BUDGET.as_secs()
            );
        }
        let secs = started.elapsed().as_secs();
        if secs / 30 > last {
            last = secs / 30;
            eprintln!("waiting for the node to be Ready ({secs}s)…");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(cluster)
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
    let cluster = ensure_vm(api, o).await?;
    if write_kubeconfig(&cluster, &o.name)? {
        eprintln!("wrote {}", cluster.kubeconfig.display());
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
    Ok(json!({
        "name": o.name,
        "vm": vm.as_ref().map(|(id, status)| json!({"id": id, "status": status})),
        "guest_ip": ip,
        "api": format!("https://127.0.0.1:{}", o.api_port),
        "api_answers": api_answers(o.api_port),
        "kubeconfig": kubeconfig,
        "kubeconfig_exists": kubeconfig.exists(),
        "env": env_line(&kubeconfig),
    }))
}

/// `fluxctl kube down`: stop the forward and the VM; `purge` also deletes the VM, the cluster's state and the kubeconfig.
pub async fn down<A: VmApi>(api: &A, o: &KubeOptions, purge: bool) -> Result<()> {
    validate(o)?;
    dh::stop_recorded_ssh(&forwarder_pid(o), &api_needle());
    if let Some((id, status)) = api.find(&o.name).await? {
        if purge {
            if status == "running" {
                api.stop(id).await?;
            }
            api.delete(id).await?;
            let _ = std::fs::remove_file(home_kube(o)?);
            eprintln!("deleted {}, its cluster state and its kubeconfig", o.name);
        } else if status == "running" {
            api.stop(id).await?;
            eprintln!("stopped {}", o.name);
        }
    }
    Ok(())
}

/// `fluxctl kube supervise`: keeps the VM running and the API port answering, for launchd. Never returns.
pub async fn supervise<A: VmApi>(api: &A, o: &KubeOptions, every: Duration) -> Result<()> {
    validate(o)?;
    loop {
        let healthy = api_answers(o.api_port)
            && matches!(api.find(&o.name).await, Ok(Some((_, s))) if s == "running");
        if !healthy {
            match up(api, o).await {
                Ok(_) => eprintln!("kubernetes is up"),
                Err(e) => eprintln!("kubernetes is not ready, will retry: {e:#}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

/// The LaunchAgent that runs `fluxctl kube supervise` at login and restarts it if it exits.
pub fn render_agent(
    fluxctl: &Path,
    o: &KubeOptions,
    log: &Path,
    env: &[(String, String)],
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
    if o.traefik {
        args.push("--traefik".into());
    }
    dh::render_plist(AGENT_LABEL, &args, log, env)
}

fn agent_paths(home: &Path) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-kube.log"),
    )
}

/// `fluxctl kube agent install`: write the LaunchAgent and load it.
pub fn agent_install(o: &KubeOptions) -> Result<PathBuf> {
    validate(o)?;
    let (plist, log) = agent_paths(&dh::home()?);
    let exe = std::env::current_exe().context("finding fluxctl's own path")?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(&plist, render_agent(&exe, o, &log, &dh::carried_env()))?;
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
        assert!(!api_answers(port));
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
}
