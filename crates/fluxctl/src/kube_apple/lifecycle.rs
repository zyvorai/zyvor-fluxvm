// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The lifecycle of an Apple-silicon k3s cluster, written against [`Host`] so that the order of operations (journal before
//! VM, VM before labels, token on stdin before the service starts, servers before agents, cleanup after a failure) is
//! checked by tests with a fake.

use super::host::{Host, LabelledVm, Power};
use super::model::*;
use super::script::{self, NodeInfo, UnitOpts};
use super::script_ha;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Polls of the node list while waiting for Ready, five seconds apart: ten minutes for a first boot.
const READY_POLLS: u32 = 120;
pub(super) const POLL_SECS: u64 = 5;

/// How k3s gets into a guest.
pub enum Install {
    /// A binary from the Mac, already checked against its published digest. It is checked again inside the guest.
    Binary { data: Vec<u8>, sha256: String },
    /// The image already has the k3s binary and its systemd service.
    Preinstalled,
    /// Copy the server's binary (checked against the digest recorded when the cluster was created).
    FromServer,
}

impl std::fmt::Debug for Install {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Install::Binary { data, sha256 } => write!(f, "Binary({} bytes, {sha256})", data.len()),
            Install::Preinstalled => write!(f, "Preinstalled"),
            Install::FromServer => write!(f, "FromServer"),
        }
    }
}

pub struct CreateReq {
    pub name: String,
    pub params: Params,
    pub api_port: u16,
    pub datastore: Datastore,
    pub tls_sans: Vec<String>,
    pub install: Install,
    pub kubeconfig: Option<PathBuf>,
    pub user: String,
    /// Control-plane nodes to create (1, or an odd number up to [`MAX_SERVERS`]; more than one needs embedded etcd).
    pub servers: u8,
    /// The stable registration endpoint; `None` means the default for the server count (a forwarder when there are several).
    pub endpoint: Option<EndpointKind>,
    /// The kube-vip address, for `EndpointKind::KubeVip`.
    pub vip: Option<String>,
    pub flags: SharedFlags,
}

#[derive(Serialize, Debug)]
pub struct CreateReport {
    pub name: String,
    pub kubeconfig: PathBuf,
    pub k3s_version: Option<String>,
    pub resumed: bool,
    pub servers: Vec<String>,
    pub next: String,
}

pub(super) async fn sh<H: Host>(h: &H, st: &State, node: &str, script: &str) -> Result<String> {
    let out = h.ssh(st, node, script, None).await?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

pub(super) fn server_name(st: &State) -> Result<String> {
    st.server()
        .map(|n| n.name.clone())
        .context("this record has no local server node")
}

/// Runs a script on the first server that answers (journal order) and returns that server's name with the output. With one
/// server this is exactly `sh` on it; with several, a server that is down or restarting does not stop the command.
pub async fn sh_server<H: Host>(h: &H, st: &State, script: &str) -> Result<(String, String)> {
    let servers = st.servers();
    if servers.is_empty() {
        bail!("this record has no local server node");
    }
    let mut last = None;
    for s in servers {
        match sh(h, st, &s.name, script).await {
            Ok(o) => return Ok((s.name.clone(), o)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

/// Polls the server's node list until every node in `want` is Ready (and runs `version`, when given).
pub async fn wait_ready<H: Host>(
    h: &H,
    st: &State,
    want: &[String],
    version: Option<&str>,
) -> Result<Vec<NodeInfo>> {
    server_name(st)?;
    let mut last = String::from("no answer yet");
    for _ in 0..READY_POLLS {
        match sh_server(h, st, &script::nodes_json_script())
            .await
            .map(|(_, o)| o)
        {
            Ok(j) => match script::parse_nodes(&j) {
                Ok(nodes) => {
                    let missing: Vec<&String> = want
                        .iter()
                        .filter(|w| {
                            !nodes.iter().any(|n| {
                                &n.name == *w && n.ready && version.is_none_or(|v| n.version == v)
                            })
                        })
                        .collect();
                    if missing.is_empty() {
                        return Ok(nodes);
                    }
                    last = format!(
                        "not Ready{}: {}",
                        version.map(|v| format!(" at {v}")).unwrap_or_default(),
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Err(e) => last = e.to_string(),
            },
            Err(e) => last = e.to_string(),
        }
        h.pause(POLL_SECS).await;
    }
    bail!("timed out waiting for the nodes: {last}")
}

/// Makes sure a node's VM exists and is recorded, adopting one that an earlier, interrupted run created. The journal is
/// saved before the daemon is asked for anything, and again with the VM's id as soon as there is one.
async fn ensure_vm<H: Host>(h: &H, store: &Store, st: &mut State, node: &str) -> Result<bool> {
    let n = st.node(node).context("unknown node")?.clone();
    if let Some(id) = &n.id {
        if let Some(vm) = h.find_vm(&n.vm_name).await?
            && &vm.id == id
        {
            if vm.status != "running" {
                h.power(id, Power::Start).await?;
            }
            return Ok(false);
        }
        bail!(
            "{node}: the VM {} recorded in the journal no longer exists; run `fluxctl kube apple recover {}`",
            n.vm_name,
            st.name
        );
    }
    // An interrupted run may have created the VM before it could write the id down.
    if let Some(vm) = h.find_vm(&n.vm_name).await? {
        let ours = h
            .list_labelled(&st.name)
            .await?
            .into_iter()
            .any(|l| l.id == vm.id && l.owner == st.owner);
        if !ours {
            bail!(
                "a VM named {} already exists and is not owned by this cluster; refusing to touch it (run `fluxctl kube apple recover {}` to inspect)",
                n.vm_name,
                st.name
            );
        }
        st.node_mut(node).unwrap().id = Some(vm.id.clone());
        store.save(st)?;
        if vm.status != "running" {
            h.power(&vm.id, Power::Start).await?;
        }
        return Ok(false);
    }
    let key = h.new_host_key()?;
    st.node_mut(node).unwrap().host_key = key.public.clone();
    store.save(st)?;
    let backend = h.backend();
    let spec = script::node_spec_on(
        backend,
        if backend == crate::vm_net::Backend::Qemu { h.ssh_forward_port(&n.vm_name) } else { 0 },
        &st.params,
        &st.name,
        node,
        &st.user,
        &h.ssh_pub(),
        &key,
    );
    let id = h.create_vm(spec).await?;
    st.node_mut(node).unwrap().id = Some(id.clone());
    store.save(st)?;
    h.label_vm(&id, &st.name, n.role, node, &st.owner).await?;
    Ok(true)
}

fn new_node(name: &str, role: Role) -> Node {
    Node {
        name: name.to_owned(),
        role,
        vm_name: vm_name(name),
        id: None,
        host_key: String::new(),
        joined: false,
        version: None,
    }
}

/// The steps that run in a fresh guest, in this order: checks, binary, token (if any), unit.
async fn bootstrap_node<H: Host>(
    h: &H,
    st: &State,
    node: &str,
    install: &Install,
    token: Option<&str>,
    agent_token: Option<&str>,
    unit: &UnitOpts<'_>,
) -> Result<()> {
    let role = unit.role;
    sh(h, st, node, &script::preflight_script_for(h.backend().guest_arch())).await?;
    match install {
        Install::Binary { data, sha256 } => {
            h.ssh(
                st,
                node,
                &script::stage_binary_script(sha256, script::K3S_BIN, false),
                Some(data),
            )
            .await?;
        }
        Install::Preinstalled => {
            sh(h, st, node, &script::preinstalled_check_script(role)).await?;
        }
        Install::FromServer => {
            let want = st
                .k3s_sha256
                .as_deref()
                .context("the cluster has no recorded k3s digest; pass --binary and --sha256")?;
            let server = server_name(st)?;
            let data = h
                .ssh(st, &server, &script::read_binary_script(), None)
                .await?;
            if sha256_hex(&data) != want {
                bail!(
                    "the server's k3s binary no longer matches the digest recorded at creation; refusing to copy it"
                );
            }
            h.ssh(
                st,
                node,
                &script::stage_binary_script(want, script::K3S_BIN, false),
                Some(&data),
            )
            .await?;
        }
    }
    if let Some(t) = token {
        let res = h
            .ssh(st, node, &script::write_token_script(), Some(t.as_bytes()))
            .await;
        if let Err(e) = res {
            bail!("{}", redact(&e.to_string(), t));
        }
    }
    if let Some(t) = agent_token {
        if let Err(e) = h
            .ssh(
                st,
                node,
                &script::write_agent_token_script(),
                Some(t.as_bytes()),
            )
            .await
        {
            bail!("{}", redact(&e.to_string(), t));
        }
    }
    sh(h, st, node, &script::install_unit_script(unit)).await?;
    Ok(())
}

pub(super) async fn export_kubeconfig<H: Host>(h: &H, st: &State, path: Option<&Path>) -> Result<PathBuf> {
    server_name(st)?;
    let (_, raw) = sh_server(h, st, &script::kubeconfig_script()).await?;
    if !raw.contains("server:") {
        bail!("the server returned no kubeconfig");
    }
    h.write_kubeconfig(st, &raw, path)
}

pub async fn create<H: Host>(h: &H, store: &Store, req: &CreateReq) -> Result<CreateReport> {
    if !name_ok(&req.name) {
        bail!(
            "{:?} is not a valid name (lowercase letters, digits and dashes, at most 40)",
            req.name
        );
    }
    for s in &req.tls_sans {
        if !san_ok(s) {
            bail!("--tls-san {s:?} must be a DNS name or address");
        }
    }
    if req.servers == 0 || req.servers > MAX_SERVERS || (req.servers > 1 && req.servers % 2 == 0) {
        bail!(
            "--servers must be 1 or an odd number up to {MAX_SERVERS} (etcd needs a majority; an even count tolerates no more failures than one fewer)"
        );
    }
    if req.servers > 1 {
        h.backend().require_private_network(&format!("--servers {}", req.servers))?;
    }
    if req.servers > 1 && req.datastore != Datastore::Etcd {
        bail!("--servers {} needs embedded etcd (--embedded-etcd)", req.servers);
    }
    if req.servers > 1 && req.api_port > u16::MAX - 8 {
        bail!("--api-port must leave eight free ports above it for the servers' tunnels");
    }
    req.flags.validate()?;
    let mut tls_sans = req.tls_sans.clone();
    let endpoint = match (req.endpoint, req.servers > 1) {
        (Some(EndpointKind::KubeVip), false) => bail!("--endpoint kube-vip needs --servers 3 or more"),
        (None, false) => None,
        (None, true) => Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }),
        (Some(EndpointKind::Proxy), _) => Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }),
        (Some(EndpointKind::KubeVip), true) => {
            let vip = req
                .vip
                .clone()
                .context("--endpoint kube-vip needs --vip ADDRESS on the private network (10.89.x.y)")?;
            if !script_ha::private_vip_ok(&vip) {
                bail!("--vip {vip:?} must be an address on the private network (10.89.x.y)");
            }
            tls_sans.push(vip.clone());
            Some(EndpointCfg { kind: EndpointKind::KubeVip, vip: Some(vip) })
        }
    };
    if req.vip.is_some() && !matches!(endpoint.as_ref().map(|e| e.kind), Some(EndpointKind::KubeVip)) {
        bail!("--vip is only for --endpoint kube-vip");
    }
    let _lock = store.lock(&req.name)?;
    let (mut st, resumed) = match store.load(&req.name)? {
        Some(s) if s.phase == Phase::Ready => {
            if joined_servers(&s) >= usize::from(s.servers_wanted) {
                bail!(
                    "{} already exists; see `fluxctl kube apple status {}`",
                    req.name,
                    req.name
                )
            }
            // A multi-server cluster whose extra servers did not all join: finish them.
            let servers = add_servers(h, store, &req.name, &req.install).await?;
            let st = store.require(&req.name)?;
            let kc = export_kubeconfig(h, &st, req.kubeconfig.as_deref()).await?;
            return Ok(CreateReport {
                name: st.name.clone(),
                kubeconfig: kc,
                k3s_version: st.k3s_version.clone(),
                resumed: true,
                servers,
                next: next_hint(&st),
            });
        }
        Some(s) => (s, true),
        None => {
            let mut s = State {
                schema: 1,
                name: req.name.clone(),
                owner: h.new_id(),
                phase: Phase::Creating,
                user: req.user.clone(),
                api_port: req.api_port,
                datastore: req.datastore,
                external_url: None,
                tls_sans: tls_sans.clone(),
                k3s_sha256: None,
                k3s_version: None,
                params: req.params.clone(),
                nodes: vec![new_node(&req.name, Role::Server)],
                canary: None,
                servers_wanted: req.servers,
                flags: req.flags.clone(),
                endpoint: endpoint.clone(),
                snapshots: None,
                agent_secret_rotated: None,
                server_secret_rotated: None,
            };
            if let Install::Binary { sha256, .. } = &req.install {
                s.k3s_sha256 = Some(sha256.clone());
            }
            // The journal exists before the daemon is asked for a VM.
            store.save(&s)?;
            (s, false)
        }
    };
    let node = req.name.clone();
    ensure_vm(h, store, &mut st, &node).await?;
    st.phase = Phase::Bootstrapping;
    store.save(&st)?;
    h.wait_ssh(&st, &node).await?;
    let already = sh(h, &st, &node, &script::is_active_script(Role::Server))
        .await
        .is_ok_and(|s| s == "active");
    if !already {
        let sans = st.tls_sans.clone();
        let flags = st.flags.clone();
        let unit = UnitOpts {
            role: Role::Server,
            node: &node,
            tls_sans: &sans,
            cluster_init: st.datastore == Datastore::Etcd,
            join_url: None,
            token_file: false,
            agent_token_file: true,
            flags: Some(&flags),
        };
        // Agents join with their own token, made here and sent on stdin: it is never in the VM request.
        let agent_token = format!("{}{}", h.new_id(), h.new_id());
        bootstrap_node(h, &st, &node, &req.install, None, Some(&agent_token), &unit).await?;
        install_kube_vip(h, &st, &node).await?;
    }
    wait_ready(h, &st, std::slice::from_ref(&node), None).await?;
    let version = sh(h, &st, &node, &script::version_script(script::K3S_BIN))
        .await
        .ok()
        .and_then(|l| script::parse_version_line(&l));
    if st.k3s_sha256.is_none() {
        st.k3s_sha256 = sh(h, &st, &node, &script::sha_of_installed_script())
            .await
            .ok()
            .filter(|s| sha256_ok(s));
    }
    st.k3s_version = version.clone();
    {
        let n = st.node_mut(&node).unwrap();
        n.version = version.clone();
        n.joined = true;
    }
    st.phase = Phase::Ready;
    store.save(&st)?;
    // The other control-plane nodes, one at a time (etcd admits one new member at a time).
    let servers = add_servers(h, store, &req.name, &req.install).await?;
    let st = store.require(&req.name)?;
    let kc = export_kubeconfig(h, &st, req.kubeconfig.as_deref()).await?;
    Ok(CreateReport {
        name: st.name.clone(),
        kubeconfig: kc,
        k3s_version: version,
        resumed,
        servers,
        next: next_hint(&st),
    })
}

pub(super) fn joined_servers(st: &State) -> usize {
    st.servers().iter().filter(|n| n.joined).count()
}

fn next_hint(st: &State) -> String {
    format!(
        "fluxctl kube apple tunnel {0}{1}; then: KUBECONFIG=<kubeconfig> kubectl get nodes",
        st.name,
        if st.uses_proxy() {
            " (a forwarder over every server's tunnel; add --install-agent to keep it up)"
        } else {
            ""
        }
    )
}

/// Writes the kube-vip static pod on a server when the cluster uses kube-vip.
async fn install_kube_vip<H: Host>(h: &H, st: &State, node: &str) -> Result<()> {
    if let Some(EndpointCfg { kind: EndpointKind::KubeVip, vip: Some(vip) }) = &st.endpoint {
        h.ssh(
            st,
            node,
            &script_ha::install_kube_vip_script(),
            Some(script_ha::kube_vip_manifest(vip).as_bytes()),
        )
        .await?;
    }
    Ok(())
}

/// Joins servers until the cluster has as many as it was created for. Returns every server's name.
async fn add_servers<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    install: &Install,
) -> Result<Vec<String>> {
    loop {
        let st = store.require(name)?;
        if joined_servers(&st) >= usize::from(st.servers_wanted) {
            return Ok(st.servers().iter().map(|n| n.name.clone()).collect());
        }
        // A server that was journalled but never finished joining is retried, not duplicated.
        let node = st
            .servers()
            .iter()
            .find(|n| !n.joined)
            .map(|n| n.name.clone());
        let install = match install {
            Install::Preinstalled => Install::Preinstalled,
            _ => Install::FromServer,
        };
        let req = JoinReq {
            cluster: name.to_owned(),
            node,
            role: Role::Server,
            install,
            external: None,
            params: st.params.clone(),
            user: st.user.clone(),
            keep_failed: true,
        };
        join_inner(h, store, &req).await.with_context(|| {
            format!("adding a control-plane node to {name}; run `fluxctl kube apple create {name} ...` again to continue")
        })?;
    }
}

pub struct JoinReq {
    pub cluster: String,
    pub node: Option<String>,
    pub role: Role,
    pub install: Install,
    /// Join a cluster owned by another Mac: its API URL and a file holding its token.
    pub external: Option<(String, PathBuf)>,
    pub params: Params,
    pub user: String,
    pub keep_failed: bool,
}

#[derive(Serialize, Debug)]
pub struct JoinReport {
    pub cluster: String,
    pub node: String,
    pub role: Role,
    pub version: Option<String>,
}

/// Reads a token from a file that only this user can read, as ssh does with private keys.
pub fn read_token_file(p: &Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(p)
        .with_context(|| format!("reading {}", p.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by others (mode {:o}); run chmod 600 on it",
            p.display(),
            mode & 0o777
        );
    }
    let t = std::fs::read_to_string(p)?.trim().to_owned();
    if !token_ok(&t) {
        bail!("{} does not hold a k3s token", p.display());
    }
    Ok(t)
}

pub async fn join<H: Host>(h: &H, store: &Store, req: &JoinReq) -> Result<JoinReport> {
    if !name_ok(&req.cluster) {
        bail!("{:?} is not a valid cluster name", req.cluster);
    }
    let _lock = store.lock(&req.cluster)?;
    let report = join_inner(h, store, req).await?;
    if req.role == Role::Server && req.external.is_none() {
        // The kubeconfig lists one context per server.
        let st = store.require(&req.cluster)?;
        export_kubeconfig(h, &st, None).await?;
    }
    Ok(report)
}

/// `join` without taking the cluster lock, for callers that already hold it (`create --servers`).
async fn join_inner<H: Host>(h: &H, store: &Store, req: &JoinReq) -> Result<JoinReport> {
    if !name_ok(&req.cluster) {
        bail!("{:?} is not a valid cluster name", req.cluster);
    }
    if req.external.is_none() {
        h.backend().require_private_network("adding a node to a local cluster")?;
    }
    let (mut st, url, token, agent_token) = match &req.external {
        Some((url, file)) => {
            if !join_url_ok(url) {
                bail!("--url must be https://HOST[:PORT] of plain characters");
            }
            let token = read_token_file(file)?;
            let st = match store.load(&req.cluster)? {
                Some(s) => s,
                None => State {
                    schema: 1,
                    name: req.cluster.clone(),
                    owner: h.new_id(),
                    phase: Phase::Ready,
                    user: req.user.clone(),
                    api_port: DEFAULT_API_PORT,
                    datastore: Datastore::Etcd,
                    external_url: Some(url.clone()),
                    tls_sans: vec![],
                    k3s_sha256: None,
                    k3s_version: None,
                    params: req.params.clone(),
                    nodes: vec![],
                    canary: None,
                    servers_wanted: 1,
                    flags: SharedFlags::default(),
                    endpoint: None,
                    snapshots: None,
                    agent_secret_rotated: None,
                    server_secret_rotated: None,
                },
            };
            (st, url.clone(), token, None)
        }
        None => {
            let st = store.require(&req.cluster)?;
            if st.phase != Phase::Ready {
                bail!(
                    "{} is not ready; finish `fluxctl kube apple create {}` first",
                    st.name,
                    st.name
                );
            }
            if req.role == Role::Server && st.datastore != Datastore::Etcd {
                bail!(
                    "a second server needs embedded etcd; create the cluster with --embedded-etcd"
                );
            }
            if req.role == Role::Server && st.servers().iter().filter(|n| n.joined).count() >= usize::from(MAX_SERVERS) {
                bail!("a cluster may have at most {MAX_SERVERS} servers");
            }
            server_name(&st)?;
            // The first server that answers; the new node joins through it (its private address), then through whichever
            // servers k3s's own client-side balancing finds.
            let (server, ip) = sh_server(h, &st, &script::private_ip_script()).await?;
            if !script::private_ip_ok(&ip) {
                bail!("the server has no address on the cluster's private network");
            }
            let token = sh(h, &st, &server, &script::read_token_script(req.role)).await?;
            if !token_ok(&token) {
                bail!("could not read a usable node token from the server");
            }
            // A joining server must hold the same agent token as the others, or agents could not join through it.
            let agent_token = if req.role == Role::Server {
                let t = sh(h, &st, &server, &script::read_token_script(Role::Agent)).await?;
                if !token_ok(&t) {
                    bail!("could not read the agent token from the server");
                }
                Some(t)
            } else {
                None
            };
            (st, format!("https://{ip}:{GUEST_API_PORT}"), token, agent_token)
        }
    };
    if st.nodes.len() >= MAX_NODES {
        bail!("a cluster may have at most {MAX_NODES} nodes");
    }
    let node = match &req.node {
        Some(n) => n.clone(),
        None if req.external.is_some() => format!("{}-{}1", st.name, &req.role.as_str()[..1]),
        None if req.role == Role::Server => st.next_server_name(),
        None => st.next_agent_name(),
    };
    if !name_ok(&node) {
        bail!("{node:?} is not a valid node name");
    }
    // A node that an earlier, interrupted join left in the journal unjoined is picked up again.
    let retry = st.node(&node).is_some_and(|n| !n.joined && n.role == req.role);
    if st.node(&node).is_some() && !retry {
        bail!("{} already has a node named {node}", st.name);
    }
    if let Install::FromServer = req.install
        && req.external.is_some()
    {
        bail!("a node of a cluster on another Mac needs --binary and --sha256, or --preinstalled");
    }
    if !retry {
        st.nodes.push(new_node(&node, req.role));
    }
    store.save(&st)?;

    let result: Result<Option<String>> = async {
        ensure_vm(h, store, &mut st, &node).await?;
        h.wait_ssh(&st, &node).await?;
        let (sans, flags): (Vec<String>, SharedFlags) = if req.role == Role::Server {
            (st.tls_sans.clone(), st.flags.clone())
        } else {
            (vec![], SharedFlags::default())
        };
        let unit = UnitOpts {
            role: req.role,
            node: &node,
            tls_sans: &sans,
            cluster_init: false,
            join_url: Some(&url),
            token_file: true,
            agent_token_file: agent_token.is_some(),
            flags: (req.role == Role::Server).then_some(&flags),
        };
        bootstrap_node(
            h,
            &st,
            &node,
            &req.install,
            Some(&token),
            agent_token.as_deref(),
            &unit,
        )
        .await?;
        if req.role == Role::Server {
            install_kube_vip(h, &st, &node).await?;
        }
        if req.external.is_none() {
            let nodes = wait_ready(h, &st, std::slice::from_ref(&node), None).await?;
            Ok(nodes
                .into_iter()
                .find(|n| n.name == node)
                .map(|n| n.version))
        } else {
            Ok(None)
        }
    }
    .await;
    match result {
        Ok(version) => {
            let n = st.node_mut(&node).unwrap();
            n.joined = true;
            n.version = version.clone();
            if req.role == Role::Server && req.external.is_none() {
                // A server added by hand makes the cluster a multi-server one from now on.
                st.servers_wanted = st.servers_wanted.max(joined_servers(&st) as u8);
            }
            store.save(&st)?;
            Ok(JoinReport {
                cluster: st.name.clone(),
                node,
                role: req.role,
                version,
            })
        }
        Err(e) => {
            let mut msg = redact(&format!("{e:#}"), &token);
            if let Some(t) = &agent_token {
                msg = redact(&msg, t);
            }
            if req.keep_failed {
                bail!("{msg}; the node {node} was kept (--keep-failed) and is recorded unjoined");
            }
            // A half-joined node must not stay in the cluster or in the journal.
            let mut notes = vec![];
            if req.external.is_none()
                && let Ok(server) = server_name(&st)
            {
                let _ = sh(h, &st, &server, &script::delete_node_script(&node)).await;
            }
            if let Err(c) = remove_node_vm(h, &st, &node).await {
                notes.push(format!("could not delete {}: {c:#}", vm_name(&node)));
            }
            if notes.is_empty() {
                st.nodes.retain(|n| n.name != node);
            }
            if st.nodes.is_empty() {
                store.remove(&st.name);
            } else {
                store.save(&st)?;
            }
            bail!(
                "{msg}; the new node was {}{}",
                if notes.is_empty() {
                    "removed again"
                } else {
                    "left in the journal"
                },
                if notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", notes.join("; "))
                }
            )
        }
    }
}

/// Stops (if running) and deletes a node's VM. A node that never got a VM is fine.
pub(super) async fn remove_node_vm<H: Host>(h: &H, st: &State, node: &str) -> Result<()> {
    let n = st.node(node).context("unknown node")?;
    let Some(vm) = h.find_vm(&n.vm_name).await? else {
        return Ok(());
    };
    if let Some(id) = &n.id
        && id != &vm.id
    {
        bail!(
            "{} is not the VM this cluster created; leaving it alone",
            n.vm_name
        );
    }
    if n.id.is_none() {
        let ours = h
            .list_labelled(&st.name)
            .await?
            .iter()
            .any(|l| l.id == vm.id && l.owner == st.owner);
        if !ours {
            bail!(
                "{} is not owned by this cluster; leaving it alone",
                n.vm_name
            );
        }
    }
    if vm.status == "running" {
        h.power(&vm.id, Power::Stop).await?;
    }
    h.power(&vm.id, Power::Delete).await
}

#[derive(Serialize, Debug, Default)]
pub struct NodeStatus {
    pub name: String,
    pub role: String,
    pub vm: String,
    pub ssh: bool,
    pub service: Option<String>,
    pub ready: Option<bool>,
    pub version: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct Status {
    pub name: String,
    pub phase: Phase,
    pub datastore: Datastore,
    pub api_port: u16,
    pub tunnel: bool,
    pub k3s_version: Option<String>,
    pub readyz: Option<String>,
    pub nodes: Vec<NodeStatus>,
    pub healthy: bool,
    pub problems: Vec<String>,
}

pub async fn status<H: Host>(h: &H, store: &Store, name: &str) -> Result<Status> {
    let st = store.require(name)?;
    let mut problems = vec![];
    let mut out: Vec<NodeStatus> = vec![];
    for n in st.ordered() {
        let mut ns = NodeStatus {
            name: n.name.clone(),
            role: n.role.as_str().into(),
            vm: "missing".into(),
            ..Default::default()
        };
        if n.id.is_some() {
            if let Some(vm) = h.find_vm(&n.vm_name).await? {
                ns.vm = vm.status;
            }
        } else {
            ns.vm = "not-created".into();
        }
        if ns.vm == "running" {
            match sh(h, &st, &n.name, &script::is_active_script(n.role)).await {
                Ok(s) => {
                    ns.ssh = true;
                    ns.service = Some(s);
                }
                Err(e) => problems.push(format!("{}: ssh failed: {e}", n.name)),
            }
        } else {
            problems.push(format!("{}: VM is {}", n.name, ns.vm));
        }
        if ns.service.as_deref().is_some_and(|s| s != "active") {
            problems.push(format!("{}: {} is not active", n.name, n.role.unit()));
        }
        out.push(ns);
    }
    let mut readyz = None;
    if let Some(server) = st
        .servers()
        .into_iter()
        .find(|s| out.iter().any(|n| n.name == s.name && n.ssh))
    {
        readyz = sh(h, &st, &server.name, &script::readyz_script())
            .await
            .ok();
        if readyz.as_deref() != Some("ok") {
            problems.push("the API server's /readyz does not say ok".into());
        }
        match sh_server(h, &st, &script::nodes_json_script())
            .await
            .and_then(|(_, j)| script::parse_nodes(&j))
        {
            Ok(nodes) => {
                for ns in out.iter_mut() {
                    if let Some(i) = nodes.iter().find(|i| i.name == ns.name) {
                        ns.ready = Some(i.ready);
                        ns.version = Some(i.version.clone());
                        if !i.ready {
                            problems.push(format!("{}: node is not Ready", ns.name));
                        }
                    } else if st.node(&ns.name).is_some_and(|n| n.joined) {
                        problems.push(format!("{}: not registered in the cluster", ns.name));
                    }
                }
                // The oldest server sets the skew limit for agents.
                let sv = out
                    .iter()
                    .filter(|n| n.role == "server")
                    .filter_map(|n| n.version.clone())
                    .min_by_key(|v| parse_version(v));
                if st.servers().len() > 1 {
                    let ready = out
                        .iter()
                        .filter(|n| n.role == "server" && n.ready == Some(true))
                        .count();
                    let want = script_ha::majority(st.servers().len());
                    if ready < want {
                        problems.push(format!(
                            "etcd has no quorum: {ready} of {} servers Ready, {want} needed",
                            st.servers().len()
                        ));
                    }
                }
                for ns in out.iter().filter(|n| n.role == "agent") {
                    if let (Some(s), Some(a)) = (sv.as_deref(), ns.version.as_deref())
                        && let (Some(sp), Some(ap)) = (parse_version(s), parse_version(a))
                        && ap > sp
                    {
                        problems.push(format!(
                            "{}: agent {a} is newer than the server {s}",
                            ns.name
                        ));
                    }
                }
            }
            Err(e) => problems.push(format!("node list failed: {e}")),
        }
    }
    if st.external_url.is_none() && joined_servers(&st) < usize::from(st.servers_wanted) {
        problems.push(format!(
            "{} of {} servers have joined; run create again to add the rest",
            joined_servers(&st),
            st.servers_wanted
        ));
    }
    if st.phase != Phase::Ready {
        problems.push(format!(
            "the cluster is still {:?}; run create again to resume",
            st.phase
        ));
    }
    Ok(Status {
        name: st.name.clone(),
        phase: st.phase,
        datastore: st.datastore,
        api_port: st.api_port,
        tunnel: h.tunnel_up(&st),
        k3s_version: st.k3s_version.clone(),
        readyz,
        nodes: out,
        healthy: problems.is_empty(),
        problems,
    })
}

/// Stops k3s cleanly in each node and then the VMs, agents first.
pub async fn stop<H: Host>(h: &H, store: &Store, name: &str) -> Result<Vec<String>> {
    let st = store.require(name)?;
    let _lock = store.lock(name)?;
    h.stop_tunnel(&st);
    let mut done = vec![];
    let mut nodes = st.ordered();
    nodes.reverse();
    for n in nodes {
        let Some(vm) = h.find_vm(&n.vm_name).await? else {
            continue;
        };
        if vm.status == "running" {
            // The service is stopped by systemd at shutdown too; doing it first lets k3s flush its datastore.
            let _ = h
                .ssh(
                    &st,
                    &n.name,
                    &format!("sync; sudo -n systemctl stop {}.service", n.role.unit()),
                    None,
                )
                .await;
            h.power(&vm.id, Power::Stop).await?;
            done.push(n.name.clone());
        }
    }
    Ok(done)
}

/// Starts the VMs, server first, and waits until every joined node is Ready again.
pub async fn start<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    tunnel: bool,
) -> Result<Vec<NodeInfo>> {
    let st = store.require(name)?;
    let _lock = store.lock(name)?;
    let server = server_name(&st)?;
    let mut order = vec![];
    for n in st.ordered() {
        let vm = h.find_vm(&n.vm_name).await?.with_context(|| {
            format!(
                "{}: its VM is gone; run `fluxctl kube apple recover {name}`",
                n.name
            )
        })?;
        if vm.status != "running" {
            h.power(&vm.id, Power::Start).await?;
        }
        h.wait_ssh(&st, &n.name).await?;
        order.push(n.name.clone());
        // With several servers the first cannot be Ready before a majority of etcd members is up: start them all first.
        if n.name == server && st.servers().len() == 1 {
            wait_ready(h, &st, std::slice::from_ref(&server), None).await?;
        }
    }
    let joined: Vec<String> = st
        .nodes
        .iter()
        .filter(|n| n.joined)
        .map(|n| n.name.clone())
        .collect();
    let nodes = wait_ready(h, &st, &joined, None).await?;
    if tunnel {
        h.start_tunnel(&st).await?;
    }
    Ok(nodes)
}

#[derive(Serialize, Debug)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Serialize, Debug)]
pub struct PersistReport {
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Writes a canary into the datastore and onto the guest's disk and records the guest's boot id.
pub async fn persist_mark<H: Host>(h: &H, store: &Store, name: &str) -> Result<Canary> {
    let mut st = store.require(name)?;
    let server = server_name(&st)?;
    let nonce = h.new_id();
    sh(h, &st, &server, &script::canary_mark_script(&nonce)).await?;
    let boot_id = sh(h, &st, &server, &script::boot_id_script()).await?;
    let c = Canary {
        nonce,
        boot_id,
        written_at: h.stamp(),
    };
    st.canary = Some(c.clone());
    store.save(&st)?;
    Ok(c)
}

/// Checks that the canary survived: the guest really rebooted, the file is on the disk and the ConfigMap is in the datastore.
pub async fn persist_verify<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    allow_same_boot: bool,
) -> Result<PersistReport> {
    let st = store.require(name)?;
    let c = st
        .canary
        .clone()
        .context("no canary; run `fluxctl kube apple persist mark` first")?;
    let server = server_name(&st)?;
    let boot = sh(h, &st, &server, &script::boot_id_script()).await?;
    let (disk, cm) =
        script::parse_canary(&sh(h, &st, &server, &script::canary_read_script()).await?);
    let checks = vec![
        Check {
            name: "rebooted".into(),
            ok: boot != c.boot_id || allow_same_boot,
            detail: if boot != c.boot_id {
                "boot id changed".into()
            } else {
                "same boot id: the guest did not restart".into()
            },
        },
        Check {
            name: "disk-file".into(),
            ok: disk == c.nonce,
            detail: format!("found {disk:?}"),
        },
        Check {
            name: "datastore-configmap".into(),
            ok: cm == c.nonce,
            detail: format!("found {cm:?}"),
        },
    ];
    Ok(PersistReport {
        ok: checks.iter().all(|c| c.ok),
        checks,
    })
}

/// mark, stop, start, verify.
pub async fn persist_cycle<H: Host>(h: &H, store: &Store, name: &str) -> Result<PersistReport> {
    persist_mark(h, store, name).await?;
    stop(h, store, name).await?;
    start(h, store, name, false).await?;
    persist_verify(h, store, name, false).await
}

#[derive(Serialize, Debug)]
pub struct BackupReport {
    pub file: PathBuf,
    pub sha256: String,
    pub bytes: usize,
    pub datastore: Datastore,
    pub contains_cluster_token: bool,
}

pub async fn backup<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    out: Option<PathBuf>,
) -> Result<BackupReport> {
    let st = store.require(name)?;
    let server = server_name(&st)?;
    let stamp = h.stamp();
    let file = out.unwrap_or_else(|| {
        store
            .backups_dir(name)
            .join(format!("{name}-{stamp}.tar.gz"))
    });
    // Refuse before the (SQLite) service is stopped for nothing.
    if file.exists() {
        bail!(
            "{} already exists; backups are never overwritten",
            file.display()
        );
    }
    let data = h
        .ssh(
            &st,
            &server,
            &script::backup_script(st.datastore, &stamp),
            None,
        )
        .await?;
    if data.len() < 32 || data[..2] != [0x1f, 0x8b] {
        bail!("the server did not return a gzip archive; nothing was written");
    }
    write_private_bytes(&file, &data, false)?;
    let sha = sha256_hex(&data);
    let side = sidecar_path(&file);
    write_private(
        &side,
        &format!(
            "{sha}  {}\n",
            file.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("backup")
        ),
    )?;
    Ok(BackupReport {
        file,
        sha256: sha,
        bytes: data.len(),
        datastore: st.datastore,
        contains_cluster_token: true,
    })
}

pub fn sidecar_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

#[derive(Serialize, Debug)]
pub struct RestoreReport {
    pub file: PathBuf,
    pub sha256: String,
    pub previous_data: String,
    pub note: String,
}

pub async fn restore<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    file: &Path,
    sha256: Option<&str>,
    yes: bool,
) -> Result<RestoreReport> {
    if !yes {
        bail!("restore replaces the cluster's datastore; add --yes to confirm");
    }
    let st = store.require(name)?;
    let _lock = store.lock(name)?;
    let server = server_name(&st)?;
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let got = sha256_hex(&data);
    let want = match sha256 {
        Some(s) if sha256_ok(s) => s.to_ascii_lowercase(),
        Some(_) => bail!("--sha256 must be 64 hexadecimal characters"),
        None => {
            let t = std::fs::read_to_string(sidecar_path(file))
                .context("no --sha256 given and no .sha256 file next to the backup")?;
            t.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase()
        }
    };
    if got != want {
        bail!(
            "the backup does not match its SHA-256 (expected {want}, got {got}); nothing was changed"
        );
    }
    // Look inside before anything is stopped.
    h.ssh(
        &st,
        &server,
        &script::verify_archive_script(st.datastore),
        Some(&data),
    )
    .await
    .context("the archive is not a backup of this kind of datastore")?;
    let stamp = h.stamp();
    h.ssh(
        &st,
        &server,
        &script::restore_script(st.datastore, &stamp),
        Some(&data),
    )
    .await?;
    wait_ready(h, &st, std::slice::from_ref(&server), None).await?;
    Ok(RestoreReport {
        file: file.to_path_buf(),
        sha256: got,
        previous_data: match st.datastore {
            Datastore::Sqlite => format!("{}/db.pre-restore-{stamp} in the server", script::SERVER_DIR),
            Datastore::Etcd => "replaced by the snapshot (cluster-reset)".into(),
        },
        note: "agents re-register on their own; with several servers, wipe the others' etcd data and rejoin them".into(),
    })
}

pub struct UpgradeReq {
    pub binary: Vec<u8>,
    pub sha256: String,
    pub allow_downgrade: bool,
    pub backup: bool,
    pub drain: bool,
}

#[derive(Serialize, Debug)]
pub struct UpgradeReport {
    pub from: String,
    pub to: String,
    pub upgraded: Vec<String>,
    pub backup: Option<PathBuf>,
}

async fn clean_staged<H: Host>(h: &H, st: &State, names: &[String]) {
    for n in names {
        let _ = h.ssh(st, n, &script::cleanup_staged_script(), None).await;
    }
}

pub async fn upgrade<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    req: &UpgradeReq,
) -> Result<UpgradeReport> {
    let mut st = store.require(name)?;
    if st.external_url.is_some() {
        bail!("upgrade the nodes of a cluster that another Mac owns from that Mac");
    }
    if !sha256_ok(&req.sha256) || sha256_hex(&req.binary) != req.sha256.to_ascii_lowercase() {
        bail!("the binary does not match its SHA-256; nothing was sent");
    }
    let server = server_name(&st)?;
    let health = status(h, store, name).await?;
    if !health.healthy {
        bail!(
            "the cluster is not healthy ({}); fix it before upgrading",
            health.problems.join("; ")
        );
    }
    let from = health
        .nodes
        .iter()
        .find(|n| n.name == server)
        .and_then(|n| n.version.clone())
        .or_else(|| st.k3s_version.clone())
        .context("cannot tell the server's current version")?;
    let _lock = store.lock(name)?;

    // Stage on the server first: that is also how the new version is learned, from the binary itself.
    h.ssh(
        &st,
        &server,
        &script::stage_binary_script(&req.sha256, script::NEW_BIN, true),
        Some(&req.binary),
    )
    .await?;
    let to = match sh(h, &st, &server, &script::version_script(script::NEW_BIN))
        .await
        .ok()
        .and_then(|l| script::parse_version_line(&l))
    {
        Some(v) => v,
        None => {
            clean_staged(h, &st, std::slice::from_ref(&server)).await;
            bail!("the new binary does not report a k3s version");
        }
    };
    if let Err(e) = upgrade_allowed(&from, &to, req.allow_downgrade) {
        clean_staged(h, &st, std::slice::from_ref(&server)).await;
        return Err(e);
    }
    if from == to && st.k3s_sha256.as_deref() == Some(&req.sha256.to_ascii_lowercase()) {
        clean_staged(h, &st, std::slice::from_ref(&server)).await;
        return Ok(UpgradeReport {
            from,
            to,
            upgraded: vec![],
            backup: None,
        });
    }
    let mut backed_up = None;
    if req.backup {
        match backup(h, store, name, None).await {
            Ok(b) => backed_up = Some(b.file),
            Err(e) => {
                clean_staged(h, &st, std::slice::from_ref(&server)).await;
                return Err(e.context(
                    "the pre-upgrade backup failed; nothing was upgraded (--no-backup skips it)",
                ));
            }
        }
    }
    let order: Vec<(String, Role)> = st
        .ordered()
        .iter()
        .map(|n| (n.name.clone(), n.role))
        .collect();
    let mut upgraded: Vec<String> = vec![];
    for (node, role) in &order {
        let step: Result<()> = async {
            if node != &server {
                h.ssh(
                    &st,
                    node,
                    &script::stage_binary_script(&req.sha256, script::NEW_BIN, true),
                    Some(&req.binary),
                )
                .await?;
            }
            if req.drain && *role == Role::Agent {
                let _ = sh(h, &st, &server, &script::drain_script(node)).await;
            }
            sh(h, &st, node, &script::swap_script(*role)).await?;
            wait_ready(h, &st, std::slice::from_ref(node), Some(&to)).await?;
            if req.drain && *role == Role::Agent {
                let _ = sh(h, &st, &server, &script::uncordon_script(node)).await;
            }
            Ok(())
        }
        .await;
        if let Err(e) = step {
            // Put this node back. Nodes upgraded before it stay: the server is never older than an agent.
            let rolled = sh(h, &st, node, &script::rollback_script(*role)).await;
            let rest: Vec<String> = order
                .iter()
                .map(|(n, _)| n.clone())
                .filter(|n| !upgraded.contains(n))
                .collect();
            clean_staged(h, &st, &rest).await;
            bail!(
                "upgrading {node} to {to} failed: {e:#}; {}; upgraded before the failure: [{}]{}",
                if rolled.is_ok() {
                    "it was rolled back to the previous binary"
                } else {
                    "ROLLBACK FAILED, check the node by hand"
                },
                upgraded.join(", "),
                backed_up
                    .as_ref()
                    .map(|b| format!("; the pre-upgrade backup is {}", b.display()))
                    .unwrap_or_default()
            );
        }
        upgraded.push(node.clone());
        if let Some(n) = st.node_mut(node) {
            n.version = Some(to.clone());
        }
        st.k3s_version = Some(to.clone());
        st.k3s_sha256 = Some(req.sha256.to_ascii_lowercase());
        store.save(&st)?;
    }
    Ok(UpgradeReport {
        from,
        to,
        upgraded,
        backup: backed_up,
    })
}

#[derive(Serialize, Debug, Default)]
pub struct RecoverReport {
    pub adopted: Vec<String>,
    pub relabelled: Vec<String>,
    pub dropped_never_created: Vec<String>,
    pub ambiguous: Vec<String>,
    pub missing: Vec<String>,
    pub orphans: Vec<String>,
    pub deleted_orphans: Vec<String>,
    pub foreign: Vec<String>,
}

#[derive(Default)]
pub struct RecoverOpts {
    /// Adopt a VM with the expected name that carries no labels (the crash came before they were written).
    pub adopt_unlabelled: bool,
    pub delete_orphans: bool,
}

/// Repairs the journal from the VMs' ownership labels: finds guests an interrupted run created, re-applies lost labels,
/// reports guests that are ours but no longer in the journal and ones that belong to someone else (never touched).
pub async fn recover<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    opts: &RecoverOpts,
) -> Result<RecoverReport> {
    let mut st = store.require(name)?;
    let _lock = store.lock(name)?;
    let labelled: Vec<LabelledVm> = h.list_labelled(name).await?;
    let mut r = RecoverReport::default();
    let mut changed = false;
    let names: Vec<String> = st.nodes.iter().map(|n| n.name.clone()).collect();
    for node in names {
        let n = st.node(&node).unwrap().clone();
        let mine = labelled
            .iter()
            .find(|l| l.node == node && l.owner == st.owner);
        match &n.id {
            None => {
                if let Some(l) = mine {
                    st.node_mut(&node).unwrap().id = Some(l.id.clone());
                    r.adopted.push(format!("{node} -> {}", l.id));
                    changed = true;
                } else if let Some(vm) = h.find_vm(&n.vm_name).await? {
                    if labelled.iter().any(|l| l.id == vm.id) {
                        // Another owner's VM with our node's name: reported once, below, as foreign.
                    } else if opts.adopt_unlabelled {
                        h.label_vm(&vm.id, name, n.role, &node, &st.owner).await?;
                        st.node_mut(&node).unwrap().id = Some(vm.id.clone());
                        r.adopted
                            .push(format!("{node} -> {} (was unlabelled)", vm.id));
                        changed = true;
                    } else {
                        r.ambiguous.push(format!(
                            "{} exists without ownership labels; pass --adopt-unlabelled if you created it",
                            n.vm_name
                        ));
                    }
                } else if n.role == Role::Agent {
                    st.nodes.retain(|x| x.name != node);
                    r.dropped_never_created.push(node.clone());
                    changed = true;
                } else {
                    r.missing
                        .push(format!("{node}: no VM was ever created; run create again"));
                }
            }
            Some(id) => match h.find_vm(&n.vm_name).await? {
                Some(vm) if &vm.id == id => {
                    if mine.is_none_or(|l| &l.id != id) {
                        h.label_vm(id, name, n.role, &node, &st.owner).await?;
                        r.relabelled.push(node.clone());
                    }
                }
                _ => r.missing.push(format!("{node}: VM {id} no longer exists")),
            },
        }
    }
    for l in &labelled {
        if st
            .nodes
            .iter()
            .any(|n| n.id.as_deref() == Some(l.id.as_str()))
        {
            continue;
        }
        let what = format!("{} ({}, {}, {})", l.name, l.id, l.node, l.status);
        if l.owner != st.owner {
            r.foreign.push(what);
        } else if opts.delete_orphans {
            if l.status == "running" {
                h.power(&l.id, Power::Stop).await?;
            }
            h.power(&l.id, Power::Delete).await?;
            r.deleted_orphans.push(what);
        } else {
            r.orphans.push(what);
        }
    }
    if changed {
        store.save(&st)?;
    }
    Ok(r)
}

#[derive(Serialize, Debug)]
pub struct DeleteReport {
    pub deleted: Vec<String>,
    pub failed: Vec<String>,
    pub journal_removed: bool,
}

/// Deletes the agents, then the server, then the journal and kubeconfig. A node whose VM cannot be deleted stays in the
/// journal, so running it again retries only that node.
pub async fn delete<H: Host>(h: &H, store: &Store, name: &str, yes: bool) -> Result<DeleteReport> {
    if !yes {
        bail!("delete removes the cluster's VMs and their data; add --yes to confirm");
    }
    let mut st = store.require(name)?;
    let _lock = store.lock(name)?;
    h.stop_tunnel(&st);
    let mut nodes: Vec<String> = st.ordered().iter().map(|n| n.name.clone()).collect();
    nodes.reverse();
    let (mut deleted, mut failed) = (vec![], vec![]);
    for node in nodes {
        match remove_node_vm(h, &st, &node).await {
            Ok(()) => {
                st.nodes.retain(|n| n.name != node);
                deleted.push(node);
                if !st.nodes.is_empty() {
                    store.save(&st)?;
                }
            }
            Err(e) => failed.push(format!("{node}: {e:#}")),
        }
    }
    let journal_removed = failed.is_empty();
    if journal_removed {
        h.remove_kubeconfig(&st);
        store.remove(name);
    } else {
        store.save(&st)?;
    }
    Ok(DeleteReport {
        deleted,
        failed,
        journal_removed,
    })
}

pub async fn kubeconfig<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    path: Option<&Path>,
) -> Result<PathBuf> {
    let st = store.require(name)?;
    export_kubeconfig(h, &st, path).await
}

pub async fn logs<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    node: Option<&str>,
    lines: u32,
) -> Result<String> {
    let st = store.require(name)?;
    let n = match node {
        Some(n) => st.node(n).ok_or_else(|| anyhow!("no node {n}"))?,
        None => st.server().ok_or_else(|| anyhow!("no server"))?,
    };
    sh(
        h,
        &st,
        &n.name,
        &format!(
            "sudo -n journalctl -u {}.service -n {} --no-pager",
            n.role.unit(),
            lines.min(2000)
        ),
    )
    .await
}

// ---- etcd snapshots (embedded etcd only) ------------------------------------------------------------------------

pub(super) fn etcd_only(st: &State) -> Result<String> {
    if st.datastore != Datastore::Etcd {
        bail!(
            "snapshots need embedded etcd (create --embedded-etcd); a SQLite cluster uses `backup`"
        );
    }
    server_name(st)
}

fn snap_name(n: &str) -> Result<()> {
    if !script::snapshot_name_ok(n) {
        bail!("{n:?} is not a snapshot name (letters, digits and - _ . only)");
    }
    Ok(())
}

pub async fn snapshot_save<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    label: Option<&str>,
) -> Result<String> {
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    let label = label
        .map(str::to_owned)
        .unwrap_or_else(|| format!("fluxvm-{}", h.stamp()));
    snap_name(&label)?;
    sh(h, &st, &server, &script::snapshot_save_script(&label)).await
}

pub async fn snapshot_ls<H: Host>(h: &H, store: &Store, name: &str) -> Result<String> {
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    sh(h, &st, &server, &script::snapshot_ls_script()).await
}

pub async fn snapshot_delete<H: Host>(h: &H, store: &Store, name: &str, snap: &str) -> Result<()> {
    snap_name(snap)?;
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    sh(h, &st, &server, &script::snapshot_delete_script(snap))
        .await
        .map(drop)
}

pub async fn snapshot_prune<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    prefix: &str,
    retention: u32,
) -> Result<String> {
    snap_name(prefix)?;
    if retention == 0 {
        bail!("--retention must be at least 1");
    }
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    sh(
        h,
        &st,
        &server,
        &script::snapshot_prune_script(prefix, retention),
    )
    .await
}

/// Copies a snapshot (with the token) to the Mac as a private, checksummed archive that `restore` accepts.
pub async fn snapshot_pull<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    snap: &str,
    out: Option<PathBuf>,
) -> Result<BackupReport> {
    snap_name(snap)?;
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    let file = out.unwrap_or_else(|| store.backups_dir(name).join(format!("{snap}.tar.gz")));
    if file.exists() {
        bail!(
            "{} already exists; backups are never overwritten",
            file.display()
        );
    }
    let data = h
        .ssh(&st, &server, &script::snapshot_pull_script(snap), None)
        .await?;
    if data.len() < 32 || data[..2] != [0x1f, 0x8b] {
        bail!("the server did not return a gzip archive; nothing was written");
    }
    write_private_bytes(&file, &data, false)?;
    let sha = sha256_hex(&data);
    write_private(
        &sidecar_path(&file),
        &format!(
            "{sha}  {}\n",
            file.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("snapshot")
        ),
    )?;
    Ok(BackupReport {
        file,
        sha256: sha,
        bytes: data.len(),
        datastore: st.datastore,
        contains_cluster_token: true,
    })
}

/// Restores a snapshot already on the server's disk. With several servers the others must be wiped and rejoined.
pub async fn snapshot_restore<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    snap: &str,
    yes: bool,
) -> Result<()> {
    if !yes {
        bail!("restoring a snapshot replaces the cluster state; add --yes to confirm");
    }
    snap_name(snap)?;
    let st = store.require(name)?;
    let server = etcd_only(&st)?;
    let _lock = store.lock(name)?;
    sh(h, &st, &server, &script::snapshot_restore_script(snap)).await?;
    wait_ready(h, &st, std::slice::from_ref(&server), None).await?;
    Ok(())
}
