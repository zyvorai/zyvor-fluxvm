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
                "127.0.0.1:{} is already in use; pick another --api-port",
                st.api_port
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
        if !cmdline.contains(&needle) {
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
        let text =
            crate::kube_host::rewrite_kubeconfig(raw, &format!("apple-{}", st.name), st.api_port);
        write_private(&p, &text)?;
        Ok(p)
    }

    fn remove_kubeconfig(&self, st: &State) {
        if let Ok(p) = self.kubeconfig_path(st) {
            let _ = std::fs::remove_file(p);
        }
    }
}
