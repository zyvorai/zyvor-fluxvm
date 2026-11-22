// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Native Linux container sandboxes (`runtime: "container"`): FluxVM's own runtime, no Docker, no external OCI runtime.
//!
//! A container sandbox is a [`VmRecord`] (so tenant scoping, quotas, listing, TTL and delete work unchanged) with no VMM:
//! the image is pulled and unpacked once per digest into a shared rootfs, the process runs through the
//! `fluxvm-container-run` launcher as a transient systemd unit (`systemd-run`, `--user` when rootless; without systemd the
//! daemon starts the launcher itself), and exec, files and logs are answered by joining the container's namespaces.
//! See `docs/linux-runtime.md`.

use crate::VmManager;
use crate::oci_images;
use crate::sandbox::SandboxCreateRequest;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use fluxvm_core::config::{Config, ContainerRuntimeConfig};
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_ctr::probe::{HostFacts, find_launcher, probe};
use fluxvm_ctr::render::{self, Manager};
use fluxvm_ctr::runtime::{Decision, HostSupport, Request, Runtime, Trust, resolve};
use fluxvm_ctr::spec::{ContainerSpec, Hardening, Limits, Net, Restart, Seccomp};
use fluxvm_guest_protocol::{AgentResponse, MAX_FILE_TRANSFER_BYTES};
use fluxvm_image::oci_registry::{Arch, BlobStore, Compression as RegCompression, PulledImage};
use fluxvm_oci_init::config as init;
use fluxvm_oci_init::supervise::RestartPolicy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub use fluxvm_ctr::runtime::RuntimeError;

pub const MARKER_FILE: &str = "container.json";
pub const RUNTIME_LABEL: &str = "fluxvm.runtime";

/// Extra settings only the native runtime has (`SandboxCreateRequest.container`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerOptions {
    /// Percent of one CPU (100 = one core). Default: `vcpus` x 100 when `vcpus` is set, else unlimited.
    pub cpu_percent: Option<u32>,
    pub pids: Option<u64>,
    pub io_weight: Option<u16>,
    /// `none` (default: loopback only) or `host` (trusted/dev code only).
    pub net: Option<Net>,
    /// Replaces the default capability set (`CAP_*`).
    pub capabilities: Option<Vec<String>>,
    /// `unconfined` turns the seccomp filter off (trusted/dev code only).
    pub seccomp: Option<Seccomp>,
    /// Units this container starts after / needs (a stack), as `fx-XXXXXXXX.service`.
    pub after: Vec<String>,
    pub requires: Vec<String>,
}

/// Written to the workspace when the sandbox is created; everything else is derived from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerMarker {
    pub spec: ContainerSpec,
    pub decision: Decision,
    pub manager: Manager,
    pub rootless: bool,
    pub image: String,
    pub manifest_digest: String,
}

pub fn is_container(vm: &VmRecord) -> bool {
    vm.workspace.join(MARKER_FILE).exists()
}

pub fn load(vm: &VmRecord) -> Result<Option<ContainerMarker>> {
    match std::fs::read(vm.workspace.join(MARKER_FILE)) {
        Ok(raw) => Ok(Some(serde_json::from_slice(&raw).context("container marker file is corrupt")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("reading container marker"),
    }
}

pub fn host_arch() -> Arch {
    if cfg!(target_arch = "aarch64") { Arch::Arm64 } else { Arch::Amd64 }
}

/// Everything [`build_spec`] needs, resolved by the caller (so the function is pure and testable).
pub struct SpecInputs<'a> {
    pub id: Uuid,
    pub state_dir: &'a Path,
    pub rootfs: &'a Path,
    pub image_ref: &'a str,
    pub process: &'a init::ProcessSpec,
    pub oci: &'a crate::oci_sandbox::OciSandboxSpec,
    pub req: &'a SandboxCreateRequest,
    pub options: &'a ContainerOptions,
    pub cfg: &'a ContainerRuntimeConfig,
    pub trust: Trust,
    pub rootless: bool,
    pub launcher: &'a Path,
    pub manager: Manager,
}

fn short(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

pub fn unit_name_for(id: Uuid) -> String {
    format!("fx-{}", short(id))
}

/// What the native runtime cannot do, refused up front with the reason (never accepted and ignored).
pub fn refuse_unsupported(req: &SandboxCreateRequest, oci: &crate::oci_sandbox::OciSandboxSpec) -> Result<()> {
    let nope: [(bool, &str, &str); 12] = [
        (!oci.ports.is_empty(), "ports", "published ports need a veth and bridge or a user-mode network, which the native runtime does not have yet (use --runtime vm on a Mac, or reach the container through its host network with container.net = host for trusted code)"),
        (!oci.expose.is_empty(), "expose", "stacks over a private network are not implemented for native containers yet"),
        (!oci.gateway_hosts.is_empty(), "gateway_hosts", "there is no NAT gateway for native containers"),
        (!oci.networks.is_empty(), "networks", "private VM networks are a vz feature"),
        (!oci.hosts.is_empty(), "hosts", "extra /etc/hosts entries are not implemented for native containers yet"),
        (!oci.secret_env.is_empty(), "secret_env", "write-only secrets need the VM meta share; pass env explicitly"),
        (oci.healthcheck.is_some(), "healthcheck", "health checks are not implemented for native containers yet; use restart policy"),
        (oci.max_restarts.is_some(), "max_restarts", "use restart = on-failure|always; a restart cap is not implemented"),
        (!req.volumes.is_empty(), "volumes", "named volumes are not implemented for native containers yet"),
        (!req.allow_hosts.is_empty(), "allow_hosts", "the egress allow-list needs the vz gateway or an egress proxy; a native container has no network (offline) or the host network"),
        (req.http_proxy_port.is_some() || !req.http_proxy_ports.is_empty(), "http_proxy_port", "the HTTP proxy needs a guest network"),
        (req.host_gpu || req.gpus.unwrap_or(0) > 0, "gpu", "GPU passthrough is a VM feature"),
    ];
    if let Some((_, what, why)) = nope.iter().find(|(set, _, _)| *set) {
        bail!("runtime=container does not support {what}: {why}");
    }
    Ok(())
}

/// Turn a request plus the resolved image process into the container the launcher runs. Pure: no I/O.
pub fn build_spec(i: &SpecInputs<'_>) -> Result<ContainerSpec> {
    let c = i.cfg;
    let memory_mib = i
        .req
        .memory_mib
        .or_else(|| i.req.profile.map(|p| p.memory_mib()))
        .unwrap_or(c.default_memory_mib);
    if memory_mib > c.max_memory_mib {
        bail!("memory_mib {memory_mib} is above sandbox.container.max_memory_mib ({})", c.max_memory_mib);
    }
    let pids = i.options.pids.unwrap_or(c.default_pids);
    if pids > c.max_pids {
        bail!("pids {pids} is above sandbox.container.max_pids ({})", c.max_pids);
    }
    let cpu_percent = i.options.cpu_percent.or_else(|| i.req.vcpus.map(|v| u32::from(v) * 100));
    let net = if i.req.offline { Net::None } else { i.options.net.unwrap_or(Net::None) };
    if net == Net::Host && i.trust.is_untrusted() {
        bail!("container.net = host shares the host's network with {} code; use a VM, or trust dev/trusted", i.trust.as_str());
    }
    let seccomp = i.options.seccomp.unwrap_or_default();
    if seccomp == Seccomp::Unconfined && i.trust.is_untrusted() {
        bail!("container.seccomp = unconfined is for dev/trusted code only");
    }
    let mut hardening = Hardening { read_only_root: i.oci.read_only_root, seccomp, ..Hardening::default() };
    if let Some(caps) = &i.options.capabilities {
        hardening.capabilities = caps.clone();
    }
    // Rootless: your uid is mapped to root in the container and nothing else exists there.
    let user = match (i.rootless, i.oci.user.as_deref()) {
        (true, None) | (true, Some("0" | "0:0" | "root")) => String::new(),
        (true, Some(u)) => bail!("a rootless container runs as root in its own user namespace; user {u:?} needs a root daemon"),
        (false, _) => i.process.user.clone(),
    };
    let ttl = match (i.req.ttl_seconds, c.max_ttl_seconds) {
        (Some(t), 0) => Some(t),
        (Some(t), max) => Some(t.min(max)),
        (None, 0) => None,
        (None, max) => Some(max),
    };
    let name = unit_name_for(i.id);
    let mut env = i.process.env.clone();
    env.retain(|e| !e.starts_with("HOSTNAME="));
    let spec = ContainerSpec {
        name: name.clone(),
        image: i.image_ref.to_string(),
        rootfs: i.rootfs.to_path_buf(),
        argv: i.process.argv.clone(),
        env,
        workdir: i.process.cwd.clone(),
        user,
        limits: Limits { memory_mib: Some(memory_mib), cpu_percent, pids: Some(pids), io_weight: i.options.io_weight },
        hardening,
        net,
        restart: match i.oci.restart {
            RestartPolicy::No => Restart::No,
            RestartPolicy::OnFailure => Restart::OnFailure,
            RestartPolicy::Always => Restart::Always,
        },
        restart_sec: 1,
        ttl_seconds: ttl,
        after: i.options.after.clone(),
        requires: i.options.requires.clone(),
        state_dir: i.state_dir.to_path_buf(),
        log_path: i.state_dir.join("console.log"),
        cgroup_dir: (i.manager == Manager::Direct).then(|| direct_cgroup(&name)),
        launcher: i.launcher.to_path_buf(),
    };
    spec.validate().map_err(anyhow::Error::msg)?;
    Ok(spec)
}

/// Where the launcher puts the cgroup without a service manager: a sibling subtree of the daemon's own cgroup.
fn direct_cgroup(name: &str) -> PathBuf {
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| fluxvm_ctr::probe::parse_self_cgroup(&s))
        .unwrap_or_else(|| "/".into());
    PathBuf::from(format!("/sys/fs/cgroup{}/{name}", own.trim_end_matches('/')))
}

/// `uid`, `uid:gid`, `name` or `name:group` to numeric `uid:gid`, using the image's `/etc/passwd` and `/etc/group`.
pub fn resolve_user_str(user: &str, passwd: &str, group: &str) -> Result<String> {
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    let find = |db: &str, name: &str| -> Option<(u32, u32)> {
        db.lines().find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.first() == Some(&name) && f.len() >= 3).then(|| {
                let id = f[2].parse().ok()?;
                let gid = f.get(3).and_then(|g| g.parse().ok()).unwrap_or(id);
                Some((id, gid))
            })?
        })
    };
    let (uid, pgid) = match u.parse::<u32>() {
        Ok(n) => (n, find(passwd, u).map_or(n, |(_, g)| g)),
        Err(_) => find(passwd, u).with_context(|| format!("user {u:?} is not in the image's /etc/passwd"))?,
    };
    let gid = match g {
        None => pgid,
        Some(g) => match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => find(group, g).map(|(id, _)| id).with_context(|| format!("group {g:?} is not in the image's /etc/group"))?,
        },
    };
    Ok(format!("{uid}:{gid}"))
}

fn read_in_root(root: &Path, rel: &str) -> String {
    fluxvm_oci_init::unpack::secure_join(root, Path::new(rel))
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default()
}

fn hex_of(digest: &str) -> Result<&str> {
    digest.strip_prefix("sha256:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).with_context(|| format!("{digest:?} is not a sha256 digest"))
}

fn convert(c: RegCompression) -> init::Compression {
    match c {
        RegCompression::None => init::Compression::None,
        RegCompression::Gzip => init::Compression::Gzip,
        RegCompression::Zstd => init::Compression::Zstd,
    }
}

/// Mountpoints the launcher needs inside the image root, so a read-only root never has to create them.
fn ensure_mountpoints(root: &Path) -> Result<()> {
    for d in ["proc", "dev", "sys", "tmp", "run", "etc"] {
        std::fs::create_dir_all(root.join(d)).with_context(|| format!("creating /{d}"))?;
    }
    let resolv = root.join("etc/resolv.conf");
    if std::fs::symlink_metadata(&resolv).is_err() {
        std::fs::write(&resolv, b"")?;
    }
    Ok(())
}

static BUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The unpacked rootfs for `image`, built once per manifest digest and ownership mode and then shared read-only.
/// Layers are verified against their diff_ids and applied with whiteouts honoured.
pub fn ensure_cache(cfg: &Config, image: &PulledImage, privileged: bool) -> Result<PathBuf> {
    let hex = hex_of(&image.manifest_digest)?;
    let base = cfg.state_dir.join("ctr");
    let dest = base.join("rootfs").join(format!("{hex}-{}", if privileged { "root" } else { "user" }));
    let done = dest.join(".fluxvm-unpacked");
    if done.exists() {
        return Ok(dest);
    }
    let _guard = BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if done.exists() {
        return Ok(dest);
    }
    let store = BlobStore::for_state_dir(&cfg.state_dir);
    let tmp = base.join("build").join(format!("{}-{}", &hex[..12], Uuid::new_v4().simple()));
    std::fs::create_dir_all(&tmp)?;
    let opts = fluxvm_oci_init::unpack::Options { preserve_owner: privileged, special_files: privileged, xattrs: false };
    let built = (|| -> Result<()> {
        for l in &image.layers {
            let blob = std::fs::File::open(store.path(&l.digest)?).with_context(|| format!("layer {} is not in the blob store; pull again", l.digest))?;
            let got = fluxvm_oci_init::unpack::apply_layer(&tmp, std::io::BufReader::new(blob), convert(l.compression), &opts)
                .with_context(|| format!("unpacking layer {}", l.digest))?;
            if got != l.diff_id {
                bail!("layer {} unpacks to {got}, not the diff_id {} its image config names", l.digest, l.diff_id);
            }
        }
        ensure_mountpoints(&tmp)?;
        std::fs::write(tmp.join(".fluxvm-unpacked"), image.manifest_digest.as_bytes())?;
        Ok(())
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    if std::fs::rename(&tmp, &dest).is_err() {
        // Lost a race with another daemon process; theirs is as good as ours.
        let _ = std::fs::remove_dir_all(&tmp);
        if !done.exists() {
            bail!("could not install the unpacked rootfs at {}", dest.display());
        }
    }
    Ok(dest)
}

fn copy_rootfs(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    let out = std::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(format!("{}/.", from.display()))
        .arg(to)
        .output()
        .context("running cp")?;
    if !out.status.success() {
        bail!("copying the rootfs: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContainerState {
    pub running: bool,
    pub pid: Option<i32>,
    pub exit_code: Option<i32>,
}

pub fn state_of(state_dir: &Path) -> ContainerState {
    let pid: Option<i32> = std::fs::read_to_string(state_dir.join("pid")).ok().and_then(|s| s.trim().parse().ok());
    let exit_code: Option<i32> = std::fs::read_to_string(state_dir.join("exit")).ok().and_then(|s| s.trim().parse().ok());
    let alive = pid.is_some_and(|p| Path::new(&format!("/proc/{p}")).exists());
    ContainerState { running: alive && exit_code.is_none(), pid, exit_code }
}

fn clip(b: &[u8]) -> String {
    const MAX: usize = 4 << 20;
    String::from_utf8_lossy(&b[..b.len().min(MAX)]).into_owned()
}

/// `fluxctl sandbox inspect` for a container sandbox.
#[derive(Debug, Clone, Serialize)]
pub struct ContainerInspect {
    pub id: Uuid,
    pub runtime: &'static str,
    pub decision: Decision,
    pub manager: Manager,
    pub rootless: bool,
    pub image: String,
    pub manifest_digest: String,
    pub state: ContainerState,
    /// The container as a systemd unit file: read it, copy it, reuse it.
    pub unit: String,
    pub systemd_run: Vec<String>,
    pub cgroup_files: Vec<(String, String)>,
}

pub fn inspect_marker(id: Uuid, m: &ContainerMarker) -> ContainerInspect {
    ContainerInspect {
        id,
        runtime: "container",
        decision: m.decision.clone(),
        manager: m.manager,
        rootless: m.rootless,
        image: m.image.clone(),
        manifest_digest: m.manifest_digest.clone(),
        state: state_of(&m.spec.state_dir),
        unit: render::unit_file(&m.spec, m.rootless),
        systemd_run: render::systemd_run_argv(&m.spec, m.manager),
        cgroup_files: render::cgroup_files(&m.spec),
    }
}

impl VmManager {
    /// What this host can do for each runtime, honouring `sandbox.container`.
    pub fn runtime_facts(&self) -> HostFacts {
        let c = &self.cfg.sandbox.container;
        let mut f = probe();
        if !c.launcher.is_empty() {
            let p = PathBuf::from(&c.launcher);
            f.launcher = p.is_file().then_some(p);
        } else if f.launcher.is_none() {
            f.launcher = find_launcher();
        }
        f
    }

    fn container_manager(&self, f: &HostFacts) -> Manager {
        match self.cfg.sandbox.container.manager.as_str() {
            "direct" => Manager::Direct,
            "systemd" => if f.root { Manager::SystemdSystem } else { Manager::SystemdUser },
            _ => f.manager(),
        }
    }

    pub fn runtime_support(&self) -> HostSupport {
        if cfg!(target_os = "macos") {
            return HostSupport {
                container: Err("native containers are Linux only; on a Mac every container is its own VM (vz)".into()),
                vm_image: Ok(()),
                vm_plain: Ok(()),
            };
        }
        let mut s = self.runtime_facts().support();
        if !self.cfg.sandbox.container.enabled {
            s.container = Err("disabled by sandbox.container.enabled = false".into());
        }
        s
    }

    /// Decide the runtime for a create request. `None` when the request is a plain VM sandbox that asked for nothing.
    pub fn decide_runtime(&self, req: &SandboxCreateRequest) -> Result<Option<Decision>> {
        if req.oci.is_none() && req.runtime.is_none() {
            return Ok(None);
        }
        let c = &self.cfg.sandbox.container;
        let runtime = match req.runtime {
            Some(r) => r,
            None => c.default_runtime.parse::<Runtime>().map_err(anyhow::Error::msg).context("sandbox.container.default_runtime")?,
        };
        let trust = match req.trust {
            Some(t) => t,
            None => c.default_trust.parse::<Trust>().map_err(anyhow::Error::msg).context("sandbox.container.default_trust")?,
        };
        let d = resolve(
            &Request {
                runtime,
                trust,
                image: req.oci.is_some(),
                allow_container_untrusted: req.allow_container_untrusted,
                operator_allows_untrusted: c.allow_untrusted,
            },
            &self.runtime_support(),
        )?;
        Ok(Some(d))
    }

    pub(crate) async fn create_container_sandbox(
        self: &std::sync::Arc<Self>,
        req: SandboxCreateRequest,
        decision: Decision,
        token_tenant: Option<&str>,
        created_by_token: Option<&str>,
    ) -> Result<VmRecord> {
        crate::oci_sandbox::validate(&req)?;
        let oci = req.oci.clone().context("runtime=container needs oci.image")?;
        refuse_unsupported(&req, &oci)?;
        let options = req.container.clone().unwrap_or_default();
        let trust = match req.trust {
            Some(t) => t,
            None => self.cfg.sandbox.container.default_trust.parse::<Trust>().map_err(anyhow::Error::msg)?,
        };
        let facts = self.runtime_facts();
        let manager = self.container_manager(&facts);
        let launcher = facts.launcher.clone().context("the fluxvm-container-run launcher was not found")?;
        let rootless = !facts.root;
        let arch = match oci.platform.as_deref() {
            Some(p) => Arch::parse(p)?,
            None => host_arch(),
        };
        if arch != host_arch() {
            bail!("a native container runs images for this host's architecture ({}) only; there is no emulation", host_arch().platform());
        }

        let id = Uuid::new_v4();
        let mut create = crate::procbox_sandbox::minimal_request(
            req.name.clone(),
            &crate::procbox_sandbox::ProcboxSpec {
                net_ports: vec![],
                max_memory_mib: req.memory_mib.unwrap_or(self.cfg.sandbox.container.default_memory_mib),
                max_processes: 0,
                timeout_seconds: 0,
                cpu_seconds: None,
                uid: None,
            },
        );
        create.image = PathBuf::from("container");
        if let Some(t) = token_tenant {
            create.tenant = Some(t.to_string());
        }
        create.created_by_token = created_by_token.map(String::from);
        create.ttl_seconds = req.ttl_seconds;
        if create.name.is_empty() {
            create.name = format!("sandbox-{}", Uuid::new_v4());
        }
        self.enforce_token_quotas(created_by_token, &create).await?;

        let image = oci_images::pull(&self.cfg, &oci.image, arch).await?;
        let cfg2 = self.cfg.clone();
        let img2 = image.clone();
        let cache = tokio::task::spawn_blocking(move || ensure_cache(&cfg2, &img2, !rootless))
            .await
            .context("unpack worker panicked")??;

        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        let staged = (|| -> Result<(ContainerSpec, PathBuf)> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&workspace)?;
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o700))?;
            let rootfs = if oci.read_only_root {
                cache.clone()
            } else {
                let r = workspace.join("rootfs");
                copy_rootfs(&cache, &r)?;
                r
            };
            let image_proc = init::ImageProcess {
                entrypoint: image.config.entrypoint.clone(),
                cmd: image.config.cmd.clone(),
                env: image.config.env.clone(),
                working_dir: image.config.working_dir.clone(),
                user: image.config.user.clone(),
            };
            let overrides = init::ProcessOverrides {
                entrypoint: oci.entrypoint.clone(),
                command: oci.command.clone(),
                env: oci.env.clone(),
                workdir: oci.workdir.clone(),
                user: oci.user.clone(),
            };
            let mut process = init::resolve_process(&image_proc, &overrides)?;
            if !rootless {
                process.user = resolve_user_str(&process.user, &read_in_root(&rootfs, "etc/passwd"), &read_in_root(&rootfs, "etc/group"))?;
            }
            let spec = build_spec(&SpecInputs {
                id,
                state_dir: &workspace,
                rootfs: &rootfs,
                image_ref: &oci.image,
                process: &process,
                oci: &oci,
                req: &req,
                options: &options,
                cfg: &self.cfg.sandbox.container,
                trust,
                rootless,
                launcher: &launcher,
                manager,
            })?;
            std::fs::write(workspace.join("spec.json"), serde_json::to_vec_pretty(&spec)?)?;
            let marker = ContainerMarker {
                spec: spec.clone(),
                decision: decision.clone(),
                manager,
                rootless,
                image: oci.image.clone(),
                manifest_digest: image.manifest_digest.clone(),
            };
            std::fs::write(workspace.join(MARKER_FILE), serde_json::to_vec_pretty(&marker)?)?;
            Ok((spec, rootfs))
        })();
        let (spec, _rootfs) = match staged {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&workspace);
                return Err(e.context("preparing the container"));
            }
        };

        if let Err(e) = start(&spec, manager).await {
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(e.context("starting the container"));
        }
        // Up when the launcher has recorded the container's pid, or it has already exited (a short command).
        let began = std::time::Instant::now();
        loop {
            let st = state_of(&workspace);
            if st.pid.is_some() || st.exit_code.is_some() {
                break;
            }
            if began.elapsed() > Duration::from_secs(20) {
                let log = std::fs::read_to_string(&spec.log_path).unwrap_or_default();
                teardown(&spec, manager).await;
                let _ = std::fs::remove_dir_all(&workspace);
                bail!("the container did not start within 20s: {}", log.lines().last().unwrap_or("no output"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let log = std::fs::read_to_string(&spec.log_path).unwrap_or_default();
        if state_of(&workspace).pid.is_none() || (state_of(&workspace).exit_code == Some(125) && log.contains("fluxvm-container-run:")) {
            // The launcher itself failed (userns refused, limit not enforceable...): not the workload.
            if let Some(line) = log.lines().rev().find(|l| l.starts_with("fluxvm-container-run:")) {
                teardown(&spec, manager).await;
                let _ = std::fs::remove_dir_all(&workspace);
                bail!("the container could not be set up: {}", line.trim_start_matches("fluxvm-container-run:").trim());
            }
        }

        let expires_at = create.ttl_seconds.map(|s| chrono::Utc::now() + chrono::Duration::seconds(s as i64));
        let mut record = crate::procbox_sandbox::make_record(id, &workspace, create, expires_at);
        record.labels.insert(RUNTIME_LABEL.into(), "container".into());
        record.labels.insert("fluxvm.runtime.reason".into(), decision.reason.clone());
        record.labels.insert("fluxvm.runtime.unit".into(), spec.unit_name());
        record.labels.insert(oci_images::OCI_LABEL.into(), image.manifest_digest.clone());
        record.labels.insert(crate::oci_sandbox::OCI_IMAGE_LABEL.into(), oci.image.clone());
        if let Err(e) = self.store.insert(record.clone()).await {
            teardown(&spec, manager).await;
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(e);
        }
        let vm_id = id.to_string();
        let tenant = record.request.tenant.clone().unwrap_or_default();
        crate::audit_event("vm.create", &[("vm_id", &vm_id), ("tenant", &tenant), ("backend", "container")]);
        Ok(record)
    }

    pub(crate) async fn container_exec(
        &self,
        vm: &VmRecord,
        m: &ContainerMarker,
        argv: Vec<String>,
        timeout_seconds: Option<u64>,
    ) -> Result<AgentResponse> {
        let st = state_of(&m.spec.state_dir);
        let Some(pid) = st.pid.filter(|_| st.running) else {
            bail!(
                "container {} is not running{}; exec needs a running container",
                vm.name,
                st.exit_code.map(|c| format!(" (it exited with code {c})")).unwrap_or_default()
            );
        };
        let limit = Duration::from_secs(timeout_seconds.unwrap_or(fluxvm_guest_protocol::DEFAULT_EXEC_TIMEOUT_SECS));
        let mut cmd = tokio::process::Command::new(&m.spec.launcher);
        cmd.arg("exec").arg("--spec").arg(render::spec_path(&m.spec)).arg("--pid").arg(pid.to_string()).arg("--").args(&argv);
        cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = match tokio::time::timeout(limit, cmd.output()).await {
            Ok(r) => r.context("running the launcher")?,
            Err(_) => {
                return Ok(AgentResponse::Exec {
                    exit_code: 124,
                    stdout: String::new(),
                    stderr: format!("timed out after {}s", limit.as_secs()),
                    enforcement: None,
                });
            }
        };
        Ok(AgentResponse::Exec {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: clip(&out.stdout),
            stderr: clip(&out.stderr),
            enforcement: None,
        })
    }

    fn running_pid(m: &ContainerMarker) -> Result<i32> {
        let st = state_of(&m.spec.state_dir);
        st.pid.filter(|_| st.running).context("the container is not running")
    }

    pub(crate) async fn container_get_file(&self, m: &ContainerMarker, path: String) -> Result<AgentResponse> {
        let pid = Self::running_pid(m)?;
        let out = tokio::process::Command::new(&m.spec.launcher)
            .args(["read", "--pid", &pid.to_string(), "--path", &path, "--max", &MAX_FILE_TRANSFER_BYTES.to_string()])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .context("running the launcher")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let nl = out.stdout.iter().position(|b| *b == b'\n').context("malformed launcher output")?;
        let head = std::str::from_utf8(&out.stdout[..nl]).unwrap_or_default();
        let mode = head.strip_prefix("MODE ").and_then(|m| u32::from_str_radix(m, 8).ok()).context("malformed launcher output")?;
        Ok(AgentResponse::FileContent {
            content_base64: base64::engine::general_purpose::STANDARD.encode(&out.stdout[nl + 1..]),
            mode,
        })
    }

    pub(crate) async fn container_put_file(&self, m: &ContainerMarker, path: String, content_base64: String, mode: Option<u32>) -> Result<AgentResponse> {
        use tokio::io::AsyncWriteExt;
        let pid = Self::running_pid(m)?;
        let data = base64::engine::general_purpose::STANDARD.decode(content_base64.as_bytes()).context("content_base64 is not valid base64")?;
        if data.len() > MAX_FILE_TRANSFER_BYTES {
            bail!("file is {} bytes; the limit is {MAX_FILE_TRANSFER_BYTES}", data.len());
        }
        let mut child = tokio::process::Command::new(&m.spec.launcher)
            .args(["write", "--pid", &pid.to_string(), "--path", &path, "--mode", &format!("{:o}", mode.unwrap_or(0o644))])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("running the launcher")?;
        let mut stdin = child.stdin.take().context("launcher stdin")?;
        stdin.write_all(&data).await?;
        drop(stdin);
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(AgentResponse::FileWritten)
    }

    pub(crate) async fn container_logs(&self, vm: &VmRecord, m: &ContainerMarker, lines: usize) -> Result<crate::oci_sandbox::SandboxLogs> {
        let log = crate::oci_sandbox::read_console(&vm.log_path).await?;
        let st = state_of(&m.spec.state_dir);
        let init_error = log
            .lines()
            .rev()
            .find(|l| l.starts_with("fluxvm-container-run:"))
            .filter(|_| st.exit_code == Some(126) || st.exit_code == Some(125))
            .map(|l| l.trim_start_matches("fluxvm-container-run:").trim().to_string());
        Ok(crate::oci_sandbox::SandboxLogs {
            id: vm.id,
            status: if st.running { VmStatus::Running } else { VmStatus::Stopped },
            oci: true,
            exit_code: st.exit_code,
            init_error,
            restarts: 0,
            health: None,
            log: crate::oci_images::tail_text(&log, lines.max(1)),
        })
    }

    /// `GET /v1/sandbox/runtimes`: what this host supports for each runtime, the doctor lines and the configured defaults.
    pub fn runtime_report(&self) -> serde_json::Value {
        let f = self.runtime_facts();
        let s = self.runtime_support();
        let verdict = |r: &Result<(), String>| match r {
            Ok(()) => serde_json::json!({"available": true}),
            Err(why) => serde_json::json!({"available": false, "why": why}),
        };
        let c = &self.cfg.sandbox.container;
        serde_json::json!({
            "manager": self.container_manager(&f),
            "support": {
                "container": verdict(&s.container),
                "vm_image": verdict(&s.vm_image),
                "vm_plain": verdict(&s.vm_plain),
            },
            "defaults": {
                "runtime": c.default_runtime,
                "trust": c.default_trust,
                "allow_untrusted": c.allow_untrusted,
            },
            "doctor": f.doctor(),
            "facts": f,
        })
    }

    /// `GET /v1/sandboxes/{id}/inspect`: the runtime decision, the container as a systemd unit, and its live state.
    pub async fn container_inspect(&self, id: Uuid) -> Result<ContainerInspect> {
        let vm = self.get(id).await?;
        let m = load(&vm)?.context("this sandbox is not a native container (inspect shows the unit of a runtime=container sandbox)")?;
        Ok(inspect_marker(id, &m))
    }

    /// Stop the container and remove its transient unit and cgroup. Best effort: delete must always make progress.
    pub(crate) async fn container_teardown(&self, vm: &VmRecord) {
        if let Ok(Some(m)) = load(vm) {
            teardown(&m.spec, m.manager).await;
        }
    }
}

async fn run_cmd(argv: &[String]) -> Result<std::process::Output> {
    let (prog, args) = argv.split_first().context("empty command")?;
    tokio::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).output().await.with_context(|| format!("running {prog}"))
}

async fn start(spec: &ContainerSpec, manager: Manager) -> Result<()> {
    match manager {
        Manager::SystemdSystem | Manager::SystemdUser => {
            let out = run_cmd(&render::systemd_run_argv(spec, manager)).await?;
            if !out.status.success() {
                bail!("systemd-run: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(())
        }
        Manager::Direct => {
            use std::os::unix::process::CommandExt;
            let log = std::fs::OpenOptions::new().create(true).append(true).open(&spec.log_path)?;
            let mut cmd = std::process::Command::new(&spec.launcher);
            cmd.arg("run").arg("--spec").arg(render::spec_path(spec));
            cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log).process_group(0);
            let child = cmd.spawn().context("starting the launcher")?;
            let _ = std::fs::write(spec.state_dir.join("helper.pid"), child.id().to_string());
            // Reap it when it exits so it does not linger as a zombie of the daemon.
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
            Ok(())
        }
    }
}

async fn teardown(spec: &ContainerSpec, manager: Manager) {
    match manager {
        Manager::SystemdSystem | Manager::SystemdUser => {
            let user: &[&str] = if manager == Manager::SystemdUser { &["--user"] } else { &[] };
            for verb in ["stop", "reset-failed"] {
                let mut argv: Vec<String> = vec!["systemctl".into()];
                argv.extend(user.iter().map(|s| s.to_string()));
                argv.push(verb.into());
                argv.push(spec.unit_name());
                let _ = run_cmd(&argv).await;
            }
        }
        Manager::Direct => {
            let pid: Option<i32> = std::fs::read_to_string(spec.state_dir.join("helper.pid")).ok().and_then(|s| s.trim().parse().ok());
            if let Some(pid) = pid {
                unsafe { libc::kill(pid, libc::SIGTERM) };
                for _ in 0..120 {
                    if !Path::new(&format!("/proc/{pid}")).exists() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            if let Some(cg) = &spec.cgroup_dir {
                // Kill whatever is left in the cgroup, then remove it.
                let _ = std::fs::write(cg.join("cgroup.kill"), "1");
                for _ in 0..20 {
                    if std::fs::remove_dir(cg).is_ok() || !cg.exists() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::config::ContainerRuntimeConfig;

    fn process() -> init::ProcessSpec {
        init::ProcessSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: vec!["PATH=/usr/bin".into(), "HOSTNAME=old".into()],
            cwd: "/".into(),
            user: "1000:1000".into(),
        }
    }

    fn oci() -> crate::oci_sandbox::OciSandboxSpec {
        serde_json::from_value(serde_json::json!({"image": "alpine:3.22"})).unwrap()
    }

    fn req(v: serde_json::Value) -> SandboxCreateRequest {
        serde_json::from_value(v).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn spec_with(r: &SandboxCreateRequest, o: &ContainerOptions, trust: Trust, rootless: bool, oc: &crate::oci_sandbox::OciSandboxSpec) -> Result<ContainerSpec> {
        let cfg = ContainerRuntimeConfig::default();
        let p = process();
        build_spec(&SpecInputs {
            id: Uuid::parse_str("1a2b3c4d-0000-4000-8000-000000000000").unwrap(),
            state_dir: Path::new("/var/lib/fluxvm/instances/x"),
            rootfs: Path::new("/var/lib/fluxvm/ctr/rootfs/abc-user"),
            image_ref: "alpine:3.22",
            process: &p,
            oci: oc,
            req: r,
            options: o,
            cfg: &cfg,
            trust,
            rootless,
            launcher: Path::new("/usr/bin/fluxvm-container-run"),
            manager: Manager::SystemdUser,
        })
    }

    #[test]
    fn spec_generation_defaults() {
        let s = spec_with(&req(serde_json::json!({"oci": {"image": "alpine:3.22"}})), &ContainerOptions::default(), Trust::Dev, true, &oci()).unwrap();
        assert_eq!(s.name, "fx-1a2b3c4d");
        assert_eq!(s.user, "", "rootless: your uid is root in the container");
        assert_eq!(s.limits.memory_mib, Some(512));
        assert_eq!(s.limits.pids, Some(512));
        assert_eq!(s.limits.cpu_percent, None);
        assert!(s.hardening.read_only_root);
        assert_eq!(s.net, Net::None);
        assert!(s.env.iter().all(|e| !e.starts_with("HOSTNAME=")));
        assert!(s.cgroup_dir.is_none());
        assert_eq!(s.log_path, PathBuf::from("/var/lib/fluxvm/instances/x/console.log"));
    }

    #[test]
    fn spec_generation_applies_request_fields() {
        let r = req(serde_json::json!({"oci": {"image": "alpine:3.22", "restart": "on-failure", "read_only_root": false}, "memory_mib": 256, "vcpus": 2, "ttl_seconds": 90}));
        let o = ContainerOptions { pids: Some(64), io_weight: Some(50), after: vec!["fx-db.service".into()], ..Default::default() };
        let s = spec_with(&r, &o, Trust::Trusted, false, &r.oci.clone().unwrap()).unwrap();
        assert_eq!(s.user, "1000:1000", "a root daemon keeps the image user");
        assert_eq!(s.limits, Limits { memory_mib: Some(256), cpu_percent: Some(200), pids: Some(64), io_weight: Some(50) });
        assert_eq!(s.restart, Restart::OnFailure);
        assert_eq!(s.ttl_seconds, Some(90));
        assert!(!s.hardening.read_only_root);
        assert_eq!(s.after, ["fx-db.service"]);
    }

    #[test]
    fn spec_generation_refusals() {
        let base = req(serde_json::json!({"oci": {"image": "alpine"}}));
        let over = req(serde_json::json!({"oci": {"image": "alpine"}, "memory_mib": 1000000}));
        assert!(spec_with(&over, &Default::default(), Trust::Dev, true, &oci()).unwrap_err().to_string().contains("max_memory_mib"));
        let host = ContainerOptions { net: Some(Net::Host), ..Default::default() };
        assert!(spec_with(&base, &host, Trust::Untrusted, true, &oci()).unwrap_err().to_string().contains("host's network"));
        assert_eq!(spec_with(&base, &host, Trust::Dev, true, &oci()).unwrap().net, Net::Host);
        let offline = req(serde_json::json!({"oci": {"image": "alpine"}, "offline": true}));
        assert_eq!(spec_with(&offline, &host, Trust::Dev, true, &oci()).unwrap().net, Net::None);
        let unconf = ContainerOptions { seccomp: Some(Seccomp::Unconfined), ..Default::default() };
        assert!(spec_with(&base, &unconf, Trust::Agent, true, &oci()).is_err());
        let mut nonroot = oci();
        nonroot.user = Some("1000".into());
        assert!(spec_with(&base, &Default::default(), Trust::Dev, true, &nonroot).unwrap_err().to_string().contains("root daemon"));
    }

    #[test]
    fn unsupported_features_are_refused_with_a_reason() {
        let cases = [
            (serde_json::json!({"oci": {"image": "a", "ports": ["8080:80"]}}), "ports"),
            (serde_json::json!({"oci": {"image": "a"}, "allow_hosts": ["example.com"]}), "allow_hosts"),
            (serde_json::json!({"oci": {"image": "a"}, "volumes": [{"name": "v", "guest_path": "/v"}]}), "volumes"),
            (serde_json::json!({"oci": {"image": "a", "healthcheck": {"command": ["true"]}}}), "healthcheck"),
        ];
        for (v, what) in cases {
            let r = req(v);
            let e = refuse_unsupported(&r, r.oci.as_ref().unwrap()).unwrap_err().to_string();
            assert!(e.contains(&format!("does not support {what}")), "{e}");
        }
        let ok = req(serde_json::json!({"oci": {"image": "a"}, "offline": true}));
        refuse_unsupported(&ok, ok.oci.as_ref().unwrap()).unwrap();
    }

    #[test]
    fn user_resolution() {
        let passwd = "root:x:0:0:r:/root:/bin/sh\nnginx:x:101:101::/n:/sbin/nologin\n";
        let group = "root:x:0:\nnginx:x:101:\nstaff:x:50:\n";
        assert_eq!(resolve_user_str("nginx", passwd, group).unwrap(), "101:101");
        assert_eq!(resolve_user_str("nginx:staff", passwd, group).unwrap(), "101:50");
        assert_eq!(resolve_user_str("1000", passwd, group).unwrap(), "1000:1000");
        assert_eq!(resolve_user_str("101:7", passwd, group).unwrap(), "101:7");
        assert_eq!(resolve_user_str("65534:65534", "", "").unwrap(), "65534:65534");
        assert!(resolve_user_str("ghost", passwd, group).is_err());
        assert!(resolve_user_str("nginx:ghost", passwd, group).is_err());
    }

    // `state_of` decides liveness from /proc, so this runs on Linux only (the container runtime is Linux-only).
    #[cfg(target_os = "linux")]
    #[test]
    fn state_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(state_of(d.path()), ContainerState::default());
        std::fs::write(d.path().join("pid"), std::process::id().to_string()).unwrap();
        let s = state_of(d.path());
        assert!(s.running && s.pid == Some(std::process::id() as i32));
        std::fs::write(d.path().join("exit"), "3").unwrap();
        let s = state_of(d.path());
        assert!(!s.running && s.exit_code == Some(3));
    }

    #[test]
    fn inspect_shows_the_unit_and_the_decision() {
        let s = spec_with(&req(serde_json::json!({"oci": {"image": "alpine:3.22"}})), &Default::default(), Trust::Dev, true, &oci()).unwrap();
        let m = ContainerMarker {
            spec: s,
            decision: Decision { runtime: Runtime::Container, reason: "auto: trust=dev".into(), isolation: "x".into(), warning: None },
            manager: Manager::SystemdUser,
            rootless: true,
            image: "alpine:3.22".into(),
            manifest_digest: "sha256:abc".into(),
        };
        let i = inspect_marker(Uuid::nil(), &m);
        assert!(i.unit.contains("[X-FluxVM-Container]") && i.unit.contains("PrivateUsers=yes"));
        assert_eq!(i.systemd_run[1], "--user");
        assert!(i.cgroup_files.iter().any(|(k, _)| k == "memory.max"));
    }

    #[test]
    fn cache_rejects_bad_digests_and_builds_mountpoints() {
        assert!(hex_of("sha256:zz").is_err());
        let d = tempfile::tempdir().unwrap();
        ensure_mountpoints(d.path()).unwrap();
        for p in ["proc", "dev", "sys", "tmp", "run", "etc/resolv.conf"] {
            assert!(d.path().join(p).exists(), "{p}");
        }
    }

    /// Needs the network: pulls alpine from the registry, unpacks it and checks the layout. Run with
    /// `cargo test -p fluxvm-scheduler -- --ignored live_pull_and_unpack`.
    #[tokio::test]
    #[ignore = "needs registry access"]
    async fn live_pull_and_unpack() {
        let d = tempfile::tempdir().unwrap();
        let cfg = fluxvm_core::config::Config { state_dir: d.path().to_path_buf(), ..Default::default() };
        let image = oci_images::pull(&cfg, "alpine:3.22", host_arch()).await.unwrap();
        let cache = ensure_cache(&cfg, &image, false).unwrap();
        for p in ["bin/busybox", "etc/os-release", "proc", "dev", "tmp", "etc/resolv.conf", ".fluxvm-unpacked"] {
            assert!(std::fs::symlink_metadata(cache.join(p)).is_ok(), "{p} missing in {}", cache.display());
        }
        assert_eq!(ensure_cache(&cfg, &image, false).unwrap(), cache, "second call reuses the cache");
        let rootfs = d.path().join("copy");
        copy_rootfs(&cache, &rootfs).unwrap();
        assert!(rootfs.join("bin/busybox").exists());
        let proc_ = init::resolve_process(
            &init::ImageProcess { entrypoint: image.config.entrypoint.clone(), cmd: image.config.cmd.clone(), env: image.config.env.clone(), working_dir: image.config.working_dir.clone(), user: image.config.user.clone() },
            &init::ProcessOverrides::default(),
        )
        .unwrap();
        assert_eq!(proc_.argv, ["/bin/sh"]);
        eprintln!("unpacked {} layers into {}", image.layers.len(), cache.display());
    }
}
