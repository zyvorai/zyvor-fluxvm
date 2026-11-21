// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Day-2 operations on a (possibly multi-server) cluster: the snapshot schedule, token rotation, node removal and the rebuild
//! of a lost journal. Written against [`Host`] like the rest of the lifecycle, and tested with the same fake.

use super::host::{Host, Power};
use super::lifecycle::{
    POLL_SECS, etcd_only, export_kubeconfig, joined_servers, remove_node_vm, server_name, sh, sh_server, wait_ready,
};
use super::model::*;
use super::script::{self};
use super::script_ha as sha;
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Restarts one server and waits until it is Ready again.
async fn restart_and_wait<H: Host>(h: &H, st: &State, node: &str) -> Result<()> {
    sh(h, st, node, &sha::restart_script(Role::Server)).await?;
    wait_ready(h, st, &[node.to_owned()], None).await?;
    Ok(())
}

// ---- scheduled snapshots ----------------------------------------------------------------------------------------

pub enum ScheduleAction {
    /// Write the policy (and S3 credentials) to every server and restart them one at a time.
    Set(Box<SnapshotPolicy>, Option<S3Secret>),
    /// Remove the drop-in: back to k3s's default (every 12 hours, keep 5, local).
    Reset,
    /// Read what the servers have.
    Show,
}

#[derive(Serialize, Debug, Default)]
pub struct ScheduleReport {
    pub policy: Option<SnapshotPolicy>,
    pub applied_to: Vec<String>,
    /// Per server: the non-secret lines of its drop-in and the age of its newest snapshot in seconds.
    pub servers: Vec<ServerSnapshots>,
    pub note: String,
}

#[derive(Serialize, Debug)]
pub struct ServerSnapshots {
    pub node: String,
    pub config: String,
    pub newest_age_secs: Option<u64>,
}

pub async fn snapshot_schedule<H: Host>(
    h: &H,
    store: &Store,
    name: &str,
    action: ScheduleAction,
) -> Result<ScheduleReport> {
    let mut st = store.require(name)?;
    etcd_only(&st)?;
    if st.external_url.is_some() {
        bail!("configure the snapshots of a cluster that another Mac owns from that Mac");
    }
    let servers: Vec<String> = st.servers().iter().map(|n| n.name.clone()).collect();
    match action {
        ScheduleAction::Show => {
            let mut r = ScheduleReport { policy: st.snapshots.clone(), ..Default::default() };
            for s in &servers {
                let config = sh(h, &st, s, &sha::snapshot_config_probe_script()).await.unwrap_or_default();
                let age = sh(h, &st, s, &sha::snapshot_age_script())
                    .await
                    .ok()
                    .and_then(|o| sha::parse_snapshot_age(&o));
                r.servers.push(ServerSnapshots { node: s.clone(), config, newest_age_secs: age });
            }
            r.note = "an empty config means k3s's defaults (every 12 hours, keep 5, on the server's disk)".into();
            Ok(r)
        }
        ScheduleAction::Set(policy, secret) => {
            policy.validate()?;
            match (&policy.s3, &secret) {
                (Some(_), None) => bail!("an S3 target needs credentials"),
                (None, Some(_)) => bail!("S3 credentials were given but no --s3-bucket"),
                _ => {}
            }
            let _lock = store.lock(name)?;
            let text = policy.dropin(secret.as_ref())?;
            let mut done = vec![];
            for s in &servers {
                let res = async {
                    h.ssh(&st, s, &sha::write_dropin_script(sha::SNAPSHOT_DROPIN), Some(text.as_bytes())).await?;
                    restart_and_wait(h, &st, s).await
                }
                .await;
                if let Err(e) = res {
                    let mut msg = format!("{e:#}");
                    if let Some(c) = &secret {
                        msg = redact(&redact(&msg, &c.secret_key), &c.access_key);
                    }
                    bail!("{s}: {msg}; configured so far: [{}]; run the command again to finish", done.join(", "));
                }
                done.push(s.clone());
            }
            st.snapshots = Some((*policy).clone());
            store.save(&st)?;
            Ok(ScheduleReport {
                policy: Some(*policy),
                applied_to: done,
                servers: vec![],
                note: "servers were restarted one at a time (a single server means a short API outage); S3 keys are in a root-only file on each server and are not in the journal".into(),
            })
        }
        ScheduleAction::Reset => {
            let _lock = store.lock(name)?;
            let mut done = vec![];
            for s in &servers {
                sh(h, &st, s, &sha::remove_dropin_script(sha::SNAPSHOT_DROPIN)).await?;
                restart_and_wait(h, &st, s).await?;
                done.push(s.clone());
            }
            st.snapshots = None;
            store.save(&st)?;
            Ok(ScheduleReport { policy: None, applied_to: done, servers: vec![], note: "back to k3s's defaults".into() })
        }
    }
}

// ---- token rotation ---------------------------------------------------------------------------------------------

pub struct RotateReq {
    pub agent: bool,
    pub server: bool,
}

#[derive(Serialize, Debug, Default)]
pub struct RotateReport {
    pub agent_secret_rotated: bool,
    pub server_secret_rotated: bool,
    pub restarted: Vec<String>,
    /// For the server secret: whether k3s's own `server/token` on the first server shows the new value.
    pub server_secret_confirmed_in_k3s_dir: Option<bool>,
    pub notes: Vec<String>,
}

async fn send_secret<H: Host>(h: &H, st: &State, node: &str, script: &str, secrets: &[&str], payload: &str) -> Result<()> {
    if let Err(e) = h.ssh(st, node, script, Some(payload.as_bytes())).await {
        let mut m = format!("{e:#}");
        for s in secrets {
            m = redact(&m, s);
        }
        bail!("{m}");
    }
    Ok(())
}

/// Replaces the agent token and/or the server token. Servers go first, one at a time, each restarted and waited for;
/// then, for the agent token, every agent's token file is rewritten and the agent restarted. Nothing is printed or
/// put in an argument: the new values are made here, sent on stdin and recorded nowhere but in the guests' root-only files.
/// A run that stops half-way can simply be run again.
pub async fn token_rotate<H: Host>(h: &H, store: &Store, name: &str, req: &RotateReq) -> Result<RotateReport> {
    if !req.agent && !req.server {
        bail!("say what to rotate: --agent, --server or both");
    }
    let mut st = store.require(name)?;
    if st.external_url.is_some() {
        bail!("rotate the tokens of a cluster that another Mac owns from that Mac");
    }
    if st.phase != Phase::Ready {
        bail!("{name} is not ready; finish `create` first");
    }
    server_name(&st)?;
    let _lock = store.lock(name)?;
    let servers: Vec<String> = st.servers().iter().map(|n| n.name.clone()).collect();
    let agents: Vec<String> = st.nodes.iter().filter(|n| n.role == Role::Agent).map(|n| n.name.clone()).collect();
    let mut r = RotateReport::default();

    if req.server {
        let (via, old) = sh_server(h, &st, &script::read_token_script(Role::Server)).await?;
        if !token_ok(&old) {
            bail!("could not read the current server token");
        }
        let new = format!("{}{}", h.new_id(), h.new_id());
        send_secret(h, &st, &via, &sha::rotate_server_secret_script(), &[&old, &new], &format!("{old}\n{new}\n"))
            .await
            .context("k3s token rotate failed; nothing was changed")?;
        r.restarted.push(format!("{via}: token rotated in the datastore"));
        // From here the old token no longer works for joining: bring every server onto the new one.
        for s in &servers {
            send_secret(h, &st, s, &sha::write_server_secret_script(), &[&old, &new], &new)
                .await
                .with_context(|| format!("{s}: the new server token could not be written; the datastore already holds it, run rotate again with the current token in /etc/rancher/k3s/token"))?;
            restart_and_wait(h, &st, s).await?;
            r.restarted.push(s.clone());
        }
        let dir = sh(h, &st, &via, &sha::server_dir_secret_script()).await.unwrap_or_default();
        r.server_secret_confirmed_in_k3s_dir = Some(dir == new);
        if dir != new {
            r.notes.push(format!(
                "{via}: k3s's own server/token file does not show the new value. Servers now read the new token from /etc/rancher/k3s/token, but check `k3s token` behaviour on this k3s build before relying on a restore."
            ));
        }
        r.notes.push("Backups and snapshots made before this need the OLD server token to restore; keep them with it until they are superseded.".into());
        r.notes.push("A node joined from another Mac (`join --url`) keeps the old token in its own files and cannot be reached from here.".into());
        st.server_secret_rotated = Some(h.stamp());
        store.save(&st)?;
        r.server_secret_rotated = true;
    }

    if req.agent {
        let new = format!("{}{}", h.new_id(), h.new_id());
        for s in &servers {
            send_secret(h, &st, s, &script::write_agent_token_script(), &[&new], &new).await?;
            restart_and_wait(h, &st, s).await.with_context(|| {
                format!("{s} did not come back after its agent token changed; the other servers still have the old one, run rotate again")
            })?;
            r.restarted.push(s.clone());
        }
        for a in &agents {
            send_secret(h, &st, a, &script::write_token_script(), &[&new], &new).await?;
            sh(h, &st, a, &sha::restart_script(Role::Agent)).await?;
            wait_ready(h, &st, std::slice::from_ref(a), None).await?;
            r.restarted.push(a.clone());
        }
        st.agent_secret_rotated = Some(h.stamp());
        store.save(&st)?;
        r.agent_secret_rotated = true;
        r.notes.push("Running agents keep working through the change; new agents join with the new agent token.".into());
    }
    Ok(r)
}

// ---- node removal -----------------------------------------------------------------------------------------------

pub struct RemoveReq {
    pub node: String,
    /// The node is dead or broken: skip the steps that need it (cordon, drain, stop) and tolerate their failure.
    pub force: bool,
    pub yes: bool,
    pub no_drain: bool,
}

#[derive(Serialize, Debug, Default)]
pub struct RemoveReport {
    pub node: String,
    pub role: String,
    pub steps: Vec<String>,
    /// k3s's own log lines about the etcd member, when the remaining servers logged any.
    pub etcd_evidence: Vec<String>,
    pub servers_left: usize,
    pub notes: Vec<String>,
}

/// Cordon, drain, stop k3s on the node, delete the node object (k3s removes an etcd member when its node is deleted), delete
/// the VM and drop it from the journal. Refuses to break etcd quorum or to remove the last server. Each step checks what is
/// already done, so a run that stopped half-way is finished by running it again.
pub async fn remove_node<H: Host>(h: &H, store: &Store, name: &str, req: &RemoveReq) -> Result<RemoveReport> {
    if !req.yes {
        bail!("removing a node deletes its VM and its data; add --yes to confirm");
    }
    let mut st = store.require(name)?;
    if st.external_url.is_some() {
        bail!("remove nodes of a cluster that another Mac owns from that Mac");
    }
    let _lock = store.lock(name)?;
    let node = st.node(&req.node).with_context(|| format!("no node {} in {name}", req.node))?.clone();
    let mut r = RemoveReport { node: node.name.clone(), role: node.role.as_str().into(), ..Default::default() };

    // Commands about the cluster go to a server that is not the one being removed.
    let others: Vec<String> = st.servers().iter().filter(|s| s.name != node.name && s.joined).map(|s| s.name.clone()).collect();
    if others.is_empty() {
        bail!(
            "{} is the only server; use `fluxctl kube apple delete {name} --yes` to remove the cluster",
            node.name
        );
    }
    let list = script::parse_nodes(&sh_any(h, &st, &others, &script::nodes_json_script()).await.context("no remaining server answers; the cluster has no quorum, nothing was changed")?)?;

    if node.role == Role::Server {
        if st.datastore != Datastore::Etcd {
            bail!("only embedded-etcd clusters have more than one server");
        }
        let total = joined_servers(&st);
        let healthy = others.iter().filter(|o| list.iter().any(|n| &n.name == *o && n.ready)).count()
            + usize::from(list.iter().any(|n| n.name == node.name && n.ready));
        let need = sha::majority(total);
        if healthy < need {
            bail!("only {healthy} of {total} servers are Ready and etcd needs {need} to change its membership; bring a server back first, nothing was changed");
        }
        if total - 1 < 3 {
            r.notes.push(format!("{} server(s) will remain: the cluster no longer survives losing one", total - 1));
        }
    }

    let present = list.iter().any(|n| n.name == node.name);
    let vm = h.find_vm(&node.vm_name).await?;
    let running = vm.as_ref().is_some_and(|v| v.status == "running");

    if present && running {
        match sh(h, &st, &others[0], &sha::cordon_script(&node.name)).await {
            Ok(_) => r.steps.push("cordoned".into()),
            Err(e) if req.force => r.steps.push(format!("cordon failed, ignored (--force): {e}")),
            Err(e) => return Err(e.context("cordon failed; nothing else was done (--force skips it for a dead node)")),
        }
        if !req.no_drain {
            match sh_any(h, &st, &others, &script::drain_script(&node.name)).await {
                Ok(_) => r.steps.push("drained".into()),
                Err(e) if req.force => r.steps.push(format!("drain failed, ignored (--force): {e}")),
                Err(e) => return Err(e.context("drain failed; the node stays cordoned (uncordon it, or use --force / --no-drain)")),
            }
        }
    } else if present {
        r.steps.push("the node's VM is not running: cordon and drain skipped".into());
    }
    if running {
        match sh(h, &st, &node.name, &sha::disable_unit_script(node.role)).await {
            Ok(_) => r.steps.push(format!("{} stopped and disabled on the node", node.role.unit())),
            Err(e) if req.force => r.steps.push(format!("could not stop k3s on the node, ignored (--force): {e}")),
            Err(e) => return Err(e.context("could not stop k3s on the node (--force skips it)")),
        }
    }
    if present {
        sh_any(h, &st, &others, &script::delete_node_script(&node.name)).await.context("kubectl delete node failed")?;
        r.steps.push("node object deleted".into());
        let mut gone = false;
        for _ in 0..12 {
            if let Ok(o) = sh_any(h, &st, &others, &script::nodes_json_script()).await
                && let Ok(l) = script::parse_nodes(&o)
                && !l.iter().any(|n| n.name == node.name)
            {
                gone = true;
                break;
            }
            h.pause(POLL_SECS).await;
        }
        if !gone {
            bail!("the node object {} is still listed after a minute; run the command again", node.name);
        }
    } else {
        r.steps.push("the node object was already gone".into());
    }
    if node.role == Role::Server {
        // k3s's etcd controller removes the member when the node object goes. Say what the servers logged; do not claim more.
        for o in &others {
            if let Ok(l) = sh(h, &st, o, &sha::etcd_member_log_script()).await {
                r.etcd_evidence.extend(l.lines().map(str::to_owned).filter(|l| !l.trim().is_empty()));
            }
            if !r.etcd_evidence.is_empty() {
                break;
            }
        }
        if r.etcd_evidence.is_empty() {
            r.notes.push("no log line about the etcd member was found; check on a remaining server: journalctl -u k3s | grep -i member".into());
        }
        let members = sh(h, &st, &others[0], &sha::etcd_members_script()).await.map(|o| sha::parse_etcd_members(&o)).unwrap_or_default();
        if members.iter().any(|m| m == &node.name) {
            bail!("{} is still listed as an etcd node; run the command again", node.name);
        }
        r.steps.push(format!("etcd nodes left: {}", members.join(", ")));
    }
    if let Err(e) = remove_node_vm(h, &st, &node.name).await {
        bail!("the node left the cluster but its VM was not deleted: {e:#}; run the command again to retry");
    }
    r.steps.push("VM deleted".into());
    st.nodes.retain(|n| n.name != node.name);
    if node.role == Role::Server {
        st.servers_wanted = (joined_servers(&st) as u8).max(1);
    }
    store.save(&st)?;
    r.servers_left = st.servers().len();
    if node.role == Role::Server {
        let _ = export_kubeconfig(h, &st, None).await;
    }
    Ok(r)
}

/// Runs a script on the first of `nodes` that answers.
async fn sh_any<H: Host>(h: &H, st: &State, nodes: &[String], script: &str) -> Result<String> {
    let mut last = None;
    for n in nodes {
        match sh(h, st, n, script).await {
            Ok(o) => return Ok(o),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no node to ask")))
}

// ---- rebuild of a lost journal ----------------------------------------------------------------------------------

pub struct RebuildReq {
    pub user: String,
    pub api_port: u16,
    pub params: Params,
    /// Pin whatever host key each guest presents now. Without this nothing is written (a plan is returned).
    pub accept_host_keys: bool,
}

#[derive(Serialize, Debug, Default)]
pub struct RebuildReport {
    pub written: bool,
    pub owner_recovered: bool,
    pub nodes: Vec<String>,
    pub recovered: Vec<String>,
    pub not_recovered: Vec<String>,
    pub next: String,
}

const NOT_RECOVERABLE: [&str; 6] = [
    "the SSH host keys pinned at creation: they lived only in the journal and in the daemon's VM request. They are pinned again from what each guest presents now (trust on first use over the daemon's own address for a VM this daemon owns)",
    "the published SHA-256 of the k3s binary: only the digest of the binary installed in the guest is recorded, and it is not checked against anything",
    "VM sizing (cpus, memory, disk, image): the ownership labels do not carry it; the values given on the command line (or the defaults) are recorded and only matter for nodes added later",
    "the canary of `persist mark`, the rotation stamps, and which clusters on other Macs joined by `join --url`",
    "the tunnel LaunchAgent and any API port other than the one you pass with --api-port",
    "any VM without ownership labels, and any VM whose owner differs: they are never adopted",
];

/// Writes a new journal for a cluster whose journal is gone, from what the VMs' ownership labels and the guests themselves
/// say. Refuses when a journal exists (use `recover`), when labels disagree about the owner, and writes nothing unless the
/// host keys are accepted. Never creates or deletes a VM.
pub async fn rebuild<H: Host>(h: &H, store: &Store, name: &str, req: &RebuildReq) -> Result<RebuildReport> {
    if !name_ok(name) {
        bail!("{name:?} is not a valid cluster name");
    }
    if store.load(name)?.is_some() {
        bail!("{name} already has a journal; use `fluxctl kube apple recover {name}` to repair it");
    }
    let _lock = store.lock(name)?;
    let labelled = h.list_labelled(name).await?;
    if labelled.is_empty() {
        bail!("no VM carries the labels of a cluster named {name}; there is nothing to rebuild from (VMs from before labels were written cannot be told apart from others)");
    }
    let mut owners: Vec<&str> = labelled.iter().map(|l| l.owner.as_str()).collect();
    owners.sort();
    owners.dedup();
    if owners.len() != 1 {
        bail!("the VMs labelled for {name} carry {} different owner ids ({}); refusing to guess which are yours", owners.len(), owners.join(", "));
    }
    let owner = owners[0].to_owned();
    let mut nodes: Vec<Node> = labelled
        .iter()
        .map(|l| Node {
            name: l.node.clone(),
            role: l.role,
            vm_name: l.name.clone(),
            id: Some(l.id.clone()),
            host_key: String::new(),
            joined: false,
            version: None,
        })
        .collect();
    nodes.sort_by_key(|n| (n.role == Role::Agent, n.name != name, n.name.clone()));
    let mut r = RebuildReport {
        owner_recovered: !owner.is_empty(),
        nodes: nodes.iter().map(|n| format!("{} ({}, VM {})", n.name, n.role.as_str(), n.id.clone().unwrap_or_default())).collect(),
        not_recovered: NOT_RECOVERABLE.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    };
    if !nodes.iter().any(|n| n.role == Role::Server) {
        bail!("no server among the labelled VMs of {name}; cannot rebuild a cluster without its control plane");
    }
    r.recovered.push(format!("owner id, {} node(s), their roles, names and VM ids (from the ownership labels)", nodes.len()));
    if !req.accept_host_keys {
        r.next = format!(
            "nothing was written. To write the journal and pin each guest's current host key: fluxctl kube apple rebuild {name} --accept-host-keys"
        );
        return Ok(r);
    }

    let mut st = State {
        schema: 1,
        name: name.to_owned(),
        owner,
        phase: Phase::Creating,
        user: req.user.clone(),
        api_port: req.api_port,
        datastore: Datastore::Sqlite,
        external_url: None,
        tls_sans: vec![],
        k3s_sha256: None,
        k3s_version: None,
        params: req.params.clone(),
        nodes,
        canary: None,
        servers_wanted: 1,
        flags: SharedFlags::default(),
        endpoint: None,
        snapshots: None,
        agent_secret_rotated: None,
        server_secret_rotated: None,
    };
    // Pin the guests' keys, starting a stopped VM first. The journal exists only once every key is in it.
    let names: Vec<String> = st.nodes.iter().map(|n| n.name.clone()).collect();
    for node in &names {
        let n = st.node(node).unwrap().clone();
        let vm = h.find_vm(&n.vm_name).await?;
        match vm {
            Some(v) if Some(&v.id) == n.id.as_ref() => {
                if v.status != "running" {
                    h.power(&v.id, Power::Start).await?;
                }
            }
            _ => bail!("{node}: the VM {} named by the labels is not the one the daemon lists under that name", n.vm_name),
        }
        let mut key = None;
        for _ in 0..20 {
            match h.scan_host_key(&st, node).await {
                Ok(k) => {
                    key = Some(k);
                    break;
                }
                Err(_) => h.pause(3).await,
            }
        }
        st.node_mut(node).unwrap().host_key = key.with_context(|| format!("{node} did not present a host key; nothing was written"))?;
    }
    store.save(&st)?;
    // Everything below is read from the guests; a failure leaves the journal with what is known and says so.
    let first = st.servers().first().map(|s| s.name.clone()).unwrap();
    for n in &names {
        h.wait_ssh(&st, n).await.with_context(|| format!("{n}: ssh with the new pin failed; the journal was written without the facts read from the guests"))?;
    }
    let unit = sh_server(h, &st, &sha::unit_probe_script(Role::Server)).await.ok().and_then(|(_, t)| sha::parse_unit_flags(&t));
    match &unit {
        Some(u) => {
            st.datastore = if u.cluster_init || u.join_url.is_some() { Datastore::Etcd } else { Datastore::Sqlite };
            st.tls_sans = u.tls_sans.clone();
            st.flags = u.shared.clone();
            r.recovered.push("datastore kind, --tls-san list and shared server flags (from the server's unit)".into());
        }
        None => r.not_recovered.push("datastore kind, tls SANs and shared flags: the server's unit could not be read (assumed SQLite, no extra flags)".into()),
    }
    st.servers_wanted = st.servers().len().max(1) as u8;
    if let Ok(l) = sh(h, &st, &first, &script::version_script(script::K3S_BIN)).await {
        st.k3s_version = script::parse_version_line(&l);
    }
    st.k3s_sha256 = sh(h, &st, &first, &script::sha_of_installed_script()).await.ok().filter(|s| sha256_ok(s));
    if let Ok((_, j)) = sh_server(h, &st, &script::nodes_json_script()).await
        && let Ok(list) = script::parse_nodes(&j)
    {
        for n in st.nodes.iter_mut() {
            if let Some(i) = list.iter().find(|i| i.name == n.name) {
                n.joined = true;
                n.version = Some(i.version.clone());
            }
        }
        r.recovered.push("which nodes are registered, and their versions (from the cluster)".into());
    }
    if st.datastore == Datastore::Etcd {
        if let Ok(o) = sh(h, &st, &first, &sha::snapshot_config_probe_script()).await {
            st.snapshots = sha::parse_snapshot_config(&o);
            if st.snapshots.is_some() {
                r.recovered.push("the snapshot schedule (S3 keys are not read back)".into());
            }
        }
        match sh(h, &st, &first, &sha::kube_vip_probe_script()).await.ok().and_then(|o| sha::parse_kube_vip_probe(&o)) {
            Some(vip) => {
                st.endpoint = Some(EndpointCfg { kind: EndpointKind::KubeVip, vip: vip.clone() });
                r.recovered.push("the kube-vip endpoint".into());
                if vip.is_none() {
                    r.not_recovered.push("the kube-vip address (the manifest was found but its address could not be read)".into());
                }
            }
            None if st.servers().len() > 1 => st.endpoint = Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }),
            None => {}
        }
    }
    st.phase = if st.nodes.iter().all(|n| n.joined) { Phase::Ready } else { Phase::Bootstrapping };
    if st.phase != Phase::Ready {
        r.not_recovered.push("some nodes are not registered in the cluster; the journal is left in Bootstrapping, see `status`".into());
    }
    store.save(&st)?;
    r.written = true;
    r.next = format!("fluxctl kube apple status {name}; then `kubeconfig {name}` and `tunnel {name}`");
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unrecoverable_list_is_not_empty_and_names_the_host_keys() {
        assert!(NOT_RECOVERABLE.iter().any(|s| s.contains("host keys")));
    }
}
