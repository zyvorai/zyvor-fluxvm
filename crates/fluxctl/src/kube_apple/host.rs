// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Everything `kube apple` needs from the machine it runs on, as one trait, so the lifecycle logic can be tested with a fake
//! that records the order of its calls. [`RealHost`] is the one that talks to the FluxVM daemon and the system `ssh`.

use super::model::*;
use super::script::{self, HostKey};
use crate::run::{self, VmApi};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Start,
    Stop,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmInfo {
    pub id: String,
    pub status: String,
}

/// A VM carrying this cluster's ownership labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelledVm {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub node: String,
    pub owner: String,
    pub status: String,
}

#[allow(async_fn_in_trait)]
pub trait Host {
    /// The VM backend this host runs guests with; decides the VM request's network, the guest architecture and whether
    /// several nodes can share a private network.
    fn backend(&self) -> crate::vm_net::Backend {
        crate::vm_net::Backend::Vz
    }
    /// The host port for a new VM's loopback SSH forward (`Qemu` only): free now, stable per VM name.
    fn ssh_forward_port(&self, vm_name: &str) -> u16 {
        crate::stack::ssh_forward_port(vm_name)
    }
    fn user(&self) -> String;
    fn ssh_pub(&self) -> String;
    fn new_host_key(&self) -> Result<HostKey>;
    /// A fresh random hex string: owner ids and canary nonces.
    fn new_id(&self) -> String;
    /// `YYYYMMDD-HHMMSS` in UTC.
    fn stamp(&self) -> String;
    async fn pause(&self, secs: u64);

    async fn create_vm(&self, spec: Value) -> Result<String>;
    /// Writes the ownership labels (stack, `role.node`, owner) of a VM.
    async fn label_vm(
        &self,
        id: &str,
        cluster: &str,
        role: Role,
        node: &str,
        owner: &str,
    ) -> Result<()>;
    async fn find_vm(&self, vm_name: &str) -> Result<Option<VmInfo>>;
    async fn list_labelled(&self, cluster: &str) -> Result<Vec<LabelledVm>>;
    async fn power(&self, id: &str, op: Power) -> Result<()>;

    /// Waits until the node answers ssh with its pinned host key.
    async fn wait_ssh(&self, st: &State, node: &str) -> Result<()>;
    /// Runs a script in the node and returns its stdout. A non-zero exit is an error carrying the stderr.
    async fn ssh(
        &self,
        st: &State,
        node: &str,
        script: &str,
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>>;

    fn tunnel_up(&self, st: &State) -> bool;
    async fn start_tunnel(&self, st: &State) -> Result<u32>;
    fn stop_tunnel(&self, st: &State) -> bool;
    /// Writes the kubeconfig (private) and returns where.
    fn write_kubeconfig(
        &self,
        st: &State,
        raw: &str,
        path: Option<&std::path::Path>,
    ) -> Result<PathBuf>;
    fn remove_kubeconfig(&self, st: &State);
    fn kubeconfig_exists(&self, st: &State) -> bool;

    /// Runs the tunnel in the foreground until it fails or the process is told to stop: one ssh tunnel for a single
    /// server, the failover forwarder over every server's tunnel for several. What a LaunchAgent runs.
    async fn serve_tunnel(&self, st: &State) -> Result<()>;
    /// Installs the LaunchAgent that runs [`Host::serve_tunnel`] at login and restarts it.
    fn install_tunnel_agent(&self, st: &State) -> Result<PathBuf>;
    /// True when there was an agent to remove.
    fn remove_tunnel_agent(&self, st: &State) -> Result<bool>;
    fn tunnel_agent_status(&self, st: &State) -> Result<Value>;
    /// The node's ed25519 host key as it answers now (for `rebuild`, which has lost the pinned one). Trust on first use.
    async fn scan_host_key(&self, st: &State, node: &str) -> Result<String>;
}

struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct RealHost<'a, A: VmApi> {
    pub api: &'a A,
    pub store: &'a Store,
    pub user: String,
    pub ssh_pub: String,
    pub identity: Option<PathBuf>,
}

impl<'a, A: VmApi> RealHost<'a, A> {
    pub fn new(api: &'a A, store: &'a Store, user: Option<String>) -> Result<Self> {
        let home = crate::docker_host::home()?;
        let user = crate::docker_host::user_name(&crate::docker_host::EngineOptions {
            user,
            ..Default::default()
        })?;
        let (ssh_pub, identity) = run::ssh_key(&home)?;
        Ok(Self {
            api,
            store,
            user,
            ssh_pub,
            identity,
        })
    }

    async fn endpoint(&self, st: &State, node: &str) -> Result<String> {
        let n = st.node(node).with_context(|| format!("no node {node}"))?;
        let id: Uuid =
            n.id.as_deref()
                .with_context(|| format!("{node} has no VM yet"))?
                .parse()?;
        self.api
            .guest_ip(id)
            .await?
            .with_context(|| format!("{node} has no address yet (is it running?)"))
    }

    fn base_args(&self, st: &State, node: &str, endpoint: &str) -> Vec<String> {
        script::ssh_args(
            &self.store.known_hosts_path(&st.name),
            node,
            &st.user,
            endpoint,
            self.identity.as_deref(),
        )
    }

    pub fn kubeconfig_path(&self, st: &State) -> Result<PathBuf> {
        Ok(crate::docker_host::home()?
            .join(".kube")
            .join(format!("fluxvm-apple-{}.yaml", st.name)))
    }
}

fn describe_exit(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    format!(
        "exit {}{}{}",
        out.status.code().map_or("signal".into(), |c| c.to_string()),
        if err.is_empty() { "" } else { ": " },
        err.lines().last().unwrap_or_default()
    )
}

impl<A: VmApi> Host for RealHost<'_, A> {
    fn backend(&self) -> crate::vm_net::Backend {
        crate::vm_net::Backend::host()
    }
    fn ssh_forward_port(&self, vm_name: &str) -> u16 {
        crate::vm_net::pick_ssh_port(vm_name, &crate::vm_net::loopback_port_free)
            .unwrap_or_else(|| crate::stack::ssh_forward_port(vm_name))
    }
    fn user(&self) -> String {
        self.user.clone()
    }
    fn ssh_pub(&self) -> String {
        self.ssh_pub.clone()
    }

    fn new_host_key(&self) -> Result<HostKey> {
        // ssh-keygen only writes files, so make them in a private temporary directory that goes away at once.
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir().join(format!("fluxvm-hostkey-{}", Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .context("making a temporary directory")?;
        let _cleanup = RemoveDir(dir.clone());
        let f = dir.join("k");
        let ok = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "fluxvm-k3s", "-f"])
            .arg(&f)
            .status()
            .context("running ssh-keygen")?
            .success();
        if !ok {
            bail!("ssh-keygen failed");
        }
        let private = std::fs::read_to_string(&f)?;
        let public =
            script::pub_key_line(&std::fs::read_to_string(format!("{}.pub", f.display()))?);
        Ok(HostKey { private, public })
    }

    fn new_id(&self) -> String {
        Uuid::new_v4().simple().to_string()
    }

    fn stamp(&self) -> String {
        chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
    }

    async fn pause(&self, secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    async fn create_vm(&self, spec: Value) -> Result<String> {
        Ok(self.api.create(spec).await?.to_string())
    }

    async fn label_vm(
        &self,
        id: &str,
        cluster: &str,
        role: Role,
        node: &str,
        owner: &str,
    ) -> Result<()> {
        use crate::stack::{L_HASH, L_SERVICE, L_STACK};
        let labels = BTreeMap::from([
            (L_STACK.to_owned(), stack_label(cluster)),
            (L_SERVICE.to_owned(), service_label(role, node)),
            (L_HASH.to_owned(), owner.to_owned()),
        ]);
        self.api.set_labels(id.parse()?, labels).await
    }

    async fn find_vm(&self, vm_name: &str) -> Result<Option<VmInfo>> {
        Ok(self.api.find(vm_name).await?.map(|(id, status)| VmInfo {
            id: id.to_string(),
            status,
        }))
    }

    async fn list_labelled(&self, cluster: &str) -> Result<Vec<LabelledVm>> {
        Ok(self
            .api
            .list_stack(&stack_label(cluster))
            .await?
            .into_iter()
            .filter_map(|v| {
                let (role, node) = parse_service_label(&v.service)?;
                Some(LabelledVm {
                    id: v.id.to_string(),
                    name: v.name,
                    role,
                    node,
                    owner: v.hash,
                    status: v.status,
                })
            })
            .collect())
    }

    async fn power(&self, id: &str, op: Power) -> Result<()> {
        let id: Uuid = id.parse()?;
        match op {
            Power::Start => self.api.start(id).await,
            Power::Stop => self.api.stop(id).await,
            Power::Delete => self.api.delete(id).await,
        }
    }

    async fn wait_ssh(&self, st: &State, node: &str) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(240);
        let mut mismatch = false;
        loop {
            if let Ok(ep) = self.endpoint(st, node).await {
                let mut c = std::process::Command::new("ssh");
                c.args(self.base_args(st, node, &ep)).arg("true");
                if let Ok(out) = c.output() {
                    if out.status.success() {
                        return Ok(());
                    }
                    let e = String::from_utf8_lossy(&out.stderr).to_lowercase();
                    mismatch = e.contains("host key verification failed")
                        || e.contains("identification has changed");
                }
            }
            if std::time::Instant::now() > deadline {
                if mismatch {
                    bail!(
                        "{node} answers ssh with a host key that is not the one pinned for it; refusing to continue (the guest was replaced, or something else holds its address)"
                    );
                }
                bail!("{node} did not answer ssh within four minutes");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn ssh(
        &self,
        st: &State,
        node: &str,
        script: &str,
        stdin: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let ep = self.endpoint(st, node).await?;
        let mut cmd = std::process::Command::new("ssh");
        cmd.args(self.base_args(st, node, &ep)).arg(script);
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("running ssh")?;
        // Feed stdin from a thread so a command that prints while it reads cannot fill the pipe and stall.
        let out = std::thread::scope(|s| {
            let w = child.stdin.take().map(|mut si| {
                let data = stdin.unwrap_or_default();
                s.spawn(move || {
                    use std::io::Write;
                    let _ = si.write_all(data);
                })
            });
            let out = child.wait_with_output();
            if let Some(w) = w {
                let _ = w.join();
            }
            out
        })
        .context("waiting for ssh")?;
        if !out.status.success() {
            bail!("{node}: {}", describe_exit(&out));
        }
        Ok(out.stdout)
    }

    fn tunnel_up(&self, st: &State) -> bool {
        crate::kube_host::api_answers(st.api_port)
    }

    async fn start_tunnel(&self, st: &State) -> Result<u32> {
        let server = st
            .server()
            .context("this record has no server to tunnel to")?;
        let pidfile = self.store.tunnel_pid_path(&st.name);
        self.stop_tunnel(st);
        if crate::kube_host::api_answers(st.api_port) {
            bail!(
                "127.0.0.1:{} is already in use (is the tunnel LaunchAgent running? see `tunnel {} --agent-status`); or pick another --api-port",
                st.api_port,
                st.name
            );
        }
        if st.uses_proxy() {
            // The forwarder is its own process: this very program, in the foreground, detached from the terminal.
            let child =
                std::process::Command::new(std::env::current_exe().context("locating fluxctl")?)
                    .args(["kube", "apple", "tunnel", &st.name, "--foreground"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .context("starting the forwarder")?;
            write_private(&pidfile, &format!("{}\n", child.id()))?;
            for _ in 0..60 {
                if crate::kube_host::api_answers(st.api_port) {
                    return Ok(child.id());
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            self.stop_tunnel(st);
            bail!(
                "the forwarder started but 127.0.0.1:{} does not answer (run `fluxctl kube apple tunnel {} --foreground` to see why)",
                st.api_port,
                st.name
            );
        }
        let ep = self.endpoint(st, &server.name).await?;
        let args = script::tunnel_args(self.base_args(st, &server.name, &ep), st.api_port);
        let child = std::process::Command::new("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the tunnel")?;
        write_private(&pidfile, &format!("{}\n", child.id()))?;
        for _ in 0..60 {
            if crate::kube_host::api_answers(st.api_port) {
                return Ok(child.id());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.stop_tunnel(st);
        bail!(
            "the tunnel started but 127.0.0.1:{} does not answer",
            st.api_port
        )
    }

    fn stop_tunnel(&self, st: &State) -> bool {
        let pidfile = self.store.tunnel_pid_path(&st.name);
        let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok())
        else {
            return false;
        };
        let needle = format!("127.0.0.1:{}:127.0.0.1:{}", st.api_port, GUEST_API_PORT);
        let cmdline = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&pidfile);
        let forwarder = format!("apple tunnel {} --foreground", st.name);
        if !cmdline.contains(&needle) && !cmdline.contains(&forwarder) {
            return false; // the pid was reused by something else: never signal it
        }
        // SAFETY: plain kill(2) on a pid whose command line we just confirmed is our own ssh tunnel.
        unsafe { libc::kill(pid, libc::SIGTERM) == 0 }
    }

    fn write_kubeconfig(
        &self,
        st: &State,
        raw: &str,
        path: Option<&std::path::Path>,
    ) -> Result<PathBuf> {
        let p = match path {
            Some(p) => p.to_path_buf(),
            None => self.kubeconfig_path(st)?,
        };
        let text = if st.servers().len() > 1 {
            super::ha::multi_kubeconfig(raw, &st.name, st.api_port, st.servers().len())?
        } else {
            crate::kube_host::rewrite_kubeconfig(raw, &format!("apple-{}", st.name), st.api_port)
        };
        write_private(&p, &text)?;
        Ok(p)
    }

    fn remove_kubeconfig(&self, st: &State) {
        if let Ok(p) = self.kubeconfig_path(st) {
            let _ = std::fs::remove_file(p);
        }
    }

    fn kubeconfig_exists(&self, st: &State) -> bool {
        self.kubeconfig_path(st).is_ok_and(|p| p.is_file())
    }

    async fn serve_tunnel(&self, st: &State) -> Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
        let spawn = |st: &State, node: &str, ep: &str, port: u16| -> Result<std::process::Child> {
            let args = script::tunnel_args(self.base_args(st, node, ep), port);
            std::process::Command::new("ssh")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting an ssh tunnel")
        };
        if !st.uses_proxy() {
            let server = st
                .server()
                .context("this record has no server to tunnel to")?;
            let ep = self.endpoint(st, &server.name).await?;
            let mut child = spawn(st, &server.name, &ep, st.api_port)?;
            // The tunnel ends when the guest does; exiting lets launchd start a fresh one with the guest's new address.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {
                        if let Some(code) = child.try_wait()? {
                            bail!("the ssh tunnel exited ({code})");
                        }
                    }
                    _ = tokio::signal::ctrl_c() => break,
                    _ = term.recv() => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        let n = st.servers().len();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", st.api_port))
            .await
            .with_context(|| format!("listening on 127.0.0.1:{}", st.api_port))?;
        let backends = (0..n)
            .map(|i| std::net::SocketAddr::from(([127, 0, 0, 1], st.backend_port(i))))
            .collect();
        let mut proxy = tokio::spawn(super::ha::serve(listener, backends));
        let mut children: Vec<Option<std::process::Child>> = (0..n).map(|_| None).collect();
        let result = loop {
            // The journal is read again each round: a server added or removed means a new set of backends, so exit and let
            // launchd restart this with it.
            if let Ok(Some(now)) = self.store.load(&st.name)
                && now.servers().len() != n
            {
                break Ok(());
            }
            for (i, node) in st.servers().iter().enumerate() {
                let alive = children[i]
                    .as_mut()
                    .is_some_and(|c| c.try_wait().ok().flatten().is_none());
                if alive {
                    continue;
                }
                // A server that is down has no address; its backend simply refuses until it is back.
                if let Ok(ep) = self.endpoint(st, &node.name).await
                    && let Ok(c) = spawn(st, &node.name, &ep, st.backend_port(i))
                {
                    children[i] = Some(c);
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                _ = tokio::signal::ctrl_c() => break Ok(()),
                _ = term.recv() => break Ok(()),
                r = &mut proxy => break Err(anyhow::anyhow!("the forwarder stopped: {r:?}")),
            }
        };
        for c in children.iter_mut().flatten() {
            let _ = c.kill();
            let _ = c.wait();
        }
        proxy.abort();
        result
    }

    fn install_tunnel_agent(&self, st: &State) -> Result<PathBuf> {
        if !cfg!(target_os = "macos") {
            bail!(
                "--install-agent writes a macOS LaunchAgent; on Linux run `fluxctl kube cluster tunnel {} --foreground` from a systemd unit",
                st.name
            );
        }
        let home = crate::docker_host::home()?;
        let exe = crate::stable_exe::stable_exe().context("locating fluxctl")?;
        let root = std::env::var_os("FLUXVM_APPLE_K3S_HOME").map(PathBuf::from);
        super::agent::install(
            &home,
            &exe,
            &st.name,
            root.as_deref(),
            &super::agent::real_launchctl,
        )
    }

    fn remove_tunnel_agent(&self, st: &State) -> Result<bool> {
        super::agent::uninstall(
            &crate::docker_host::home()?,
            &st.name,
            &super::agent::real_launchctl,
        )
    }

    fn tunnel_agent_status(&self, st: &State) -> Result<Value> {
        let home = crate::docker_host::home()?;
        crate::launch_agent::status_of(
            &super::agent::label(&st.name),
            &super::agent::plist_path(&home, &st.name),
            &super::agent::log_path(&home, &st.name),
        )
    }

    async fn scan_host_key(&self, st: &State, node: &str) -> Result<String> {
        let ep = self.endpoint(st, node).await?;
        let (host, port) = run::split_endpoint(&ep);
        let mut c = std::process::Command::new("ssh-keyscan");
        c.args(["-t", "ed25519", "-T", "8"]);
        if let Some(p) = port {
            c.args(["-p", &p.to_string()]);
        }
        let out = c.arg(host).output().context("running ssh-keyscan")?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| {
                let mut f = l.split_whitespace().skip(1);
                Some(format!("{} {}", f.next()?, f.next()?))
            })
            .filter(|k| k.starts_with("ssh-ed25519 "))
            .with_context(|| format!("{node} returned no ed25519 host key"))
    }
}
