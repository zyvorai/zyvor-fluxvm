// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl sandbox run IMAGE -- CMD…`: one container sandbox (one VM), its console output, its exit code.

use crate::remote::Remote;
use anyhow::{Context, Result, bail};
use fluxvm_core::model::VmStatus;
use fluxvm_scheduler::oci_sandbox::SandboxLogs;
use fluxvm_scheduler::{SandboxCreateRequest, VmManager};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The most console lines one poll asks for; output beyond it between two polls is skipped.
const POLL_LINES: usize = 10_000;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    /// Image, e.g. `alpine:3.22`.
    pub image: String,
    /// `linux/arm64` (default) or `linux/amd64`, which runs under Rosetta.
    #[arg(long)]
    pub platform: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    /// Sandbox size: tiny (1 vCPU, 512 MiB), small or standard; see docs/agent-density.md.
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub vcpus: Option<u8>,
    #[arg(long)]
    pub memory_mib: Option<u64>,
    /// No network card at all.
    #[arg(long, conflicts_with = "allow_hosts")]
    pub offline: bool,
    /// Allow egress to HOST (repeatable); everything else is refused.
    #[arg(long = "allow-host")]
    pub allow_hosts: Vec<String>,
    /// `KEY=value` (repeatable).
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Pass this variable from your environment as a write-only secret (repeatable): it is never stored in the VM
    /// record or shown by the API.
    #[arg(long = "secret-env")]
    pub secret_env: Vec<String>,
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,
    /// `uid[:gid]` or `name[:group]`.
    #[arg(short = 'u', long)]
    pub user: Option<String>,
    /// Replace the image's entrypoint (space-separated argv).
    #[arg(long)]
    pub entrypoint: Option<String>,
    /// Mount the sandbox's own copy of the image read-write (default: read-only; `/tmp` and `/run` are tmpfs either way).
    #[arg(long)]
    pub writable_root: bool,
    /// Publish a TCP port, `HOST:CONTAINER` (repeatable): the Mac's 127.0.0.1:HOST reaches the container.
    #[arg(short = 'p', long = "publish")]
    pub publish: Vec<String>,
    /// Persistent named volume, `NAME:/path[:ro]` (repeatable); it survives the sandbox.
    #[arg(short = 'v', long = "volume")]
    pub volumes: Vec<String>,
    /// Restart the process: no, on-failure or always.
    #[arg(long)]
    pub restart: Option<String>,
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Health check command, run without a shell (space-separated argv).
    #[arg(long)]
    pub health_cmd: Option<String>,
    #[arg(long, requires = "health_cmd")]
    pub health_interval: Option<u64>,
    /// Delete the sandbox when the process has exited.
    #[arg(long)]
    pub rm: bool,
    /// Give up (and stop following) after this many seconds.
    #[arg(long, default_value_t = 3600)]
    pub timeout_seconds: u64,
    #[command(flatten)]
    pub rt: RuntimeArgs,
    /// Replaces the image's `Cmd`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Which runtime runs the image, and the settings only FluxVM's own Linux container runtime has.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct RuntimeArgs {
    /// `auto` (default: a container for --trust dev/trusted, a microVM for untrusted/agent), `container` (FluxVM's own
    /// Linux container runtime: fast, shares the host kernel) or `vm` (a microVM: its own kernel). See docs/linux-runtime.md.
    #[arg(long)]
    pub runtime: Option<String>,
    /// `dev`, `trusted`, `untrusted` or `agent`. Drives `auto`, and `--runtime container` is refused for the last two.
    /// Default: the daemon's `sandbox.container.default_trust` (untrusted).
    #[arg(long)]
    pub trust: Option<String>,
    /// Run untrusted/agent code in a container anyway (shared host kernel). The operator must also allow it
    /// (`sandbox.container.allow_untrusted`).
    #[arg(long)]
    pub allow_container_untrusted: bool,
    /// Container CPU limit in percent of one core (`CPUQuota=`).
    #[arg(long)]
    pub cpu_percent: Option<u32>,
    /// Container process limit (`TasksMax=`).
    #[arg(long)]
    pub pids: Option<u64>,
    /// Container block I/O weight, 1-10000 (`IOWeight=`).
    #[arg(long)]
    pub io_weight: Option<u16>,
    /// Container network: `none` (default, loopback only) or `host` (trust dev/trusted only).
    #[arg(long)]
    pub net: Option<String>,
    /// Print the container as the systemd unit it would be, and the systemd-run command, then exit. Runs nothing.
    #[arg(long)]
    pub print_unit: bool,
}

impl RunArgs {
    pub fn request(&self) -> Result<Value> {
        let mut oci = json!({
            "image": self.image,
            "env": self.env,
            "workdir": self.workdir,
            "user": self.user,
            "read_only_root": !self.writable_root,
            "exit_policy": "poweroff",
        });
        if !self.command.is_empty() {
            oci["command"] = json!(self.command);
        }
        if let Some(p) = &self.platform {
            oci["platform"] = json!(p);
        }
        if let Some(ep) = &self.entrypoint {
            oci["entrypoint"] = json!(ep.split_whitespace().collect::<Vec<_>>());
        }
        if !self.publish.is_empty() {
            oci["ports"] = json!(self.publish);
        }
        if let Some(r) = &self.restart {
            oci["restart"] = json!(r);
        }
        if !self.secret_env.is_empty() {
            let mut secrets = serde_json::Map::new();
            for name in &self.secret_env {
                let value = std::env::var(name)
                    .with_context(|| format!("--secret-env {name}: it is not set in this shell"))?;
                secrets.insert(name.clone(), json!(value));
            }
            oci["secret_env"] = Value::Object(secrets);
        }
        if let Some(n) = self.max_restarts {
            oci["max_restarts"] = json!(n);
        }
        if let Some(cmd) = &self.health_cmd {
            let mut hc = json!({"command": cmd.split_whitespace().collect::<Vec<_>>()});
            if let Some(i) = self.health_interval {
                hc["interval_seconds"] = json!(i);
            }
            oci["healthcheck"] = hc;
        }
        let mut req = json!({
            "name": self.name.clone().unwrap_or_else(|| default_name(&self.image)),
            "oci": oci,
            "offline": self.offline,
            "allow_hosts": self.allow_hosts,
        });
        let rt = &self.rt;
        if let Some(r) = &rt.runtime {
            req["runtime"] = json!(
                r.parse::<fluxvm_ctr::runtime::Runtime>()
                    .map_err(anyhow::Error::msg)?
                    .as_str()
            );
        }
        if let Some(t) = &rt.trust {
            req["trust"] = json!(
                t.parse::<fluxvm_ctr::runtime::Trust>()
                    .map_err(anyhow::Error::msg)?
                    .as_str()
            );
        }
        if rt.allow_container_untrusted {
            req["allow_container_untrusted"] = json!(true);
        }
        let mut container = serde_json::Map::new();
        if let Some(c) = rt.cpu_percent {
            container.insert("cpu_percent".into(), json!(c));
        }
        if let Some(p) = rt.pids {
            container.insert("pids".into(), json!(p));
        }
        if let Some(w) = rt.io_weight {
            container.insert("io_weight".into(), json!(w));
        }
        if let Some(n) = &rt.net {
            if !matches!(n.as_str(), "none" | "host") {
                bail!("--net {n:?}: use none or host");
            }
            container.insert("net".into(), json!(n));
        }
        if !container.is_empty() {
            req["container"] = Value::Object(container);
        }
        for (k, v) in [
            ("profile", json!(self.profile)),
            ("vcpus", json!(self.vcpus)),
            ("memory_mib", json!(self.memory_mib)),
            ("volumes", volumes_json(&self.volumes)?),
        ] {
            if !v.is_null() {
                req[k] = v;
            }
        }
        Ok(req)
    }
}

/// `NAME:/path[:ro]` → `{"name", "guest_path", "read_only"}`; `null` when there are none.
pub(crate) fn volumes_json(specs: &[String]) -> Result<Value> {
    if specs.is_empty() {
        return Ok(Value::Null);
    }
    let mut out = Vec::new();
    for v in specs {
        let mut parts = v.splitn(3, ':');
        let (Some(name), Some(path)) = (parts.next(), parts.next()) else {
            bail!("volume {v:?}: use NAME:/path[:ro]");
        };
        let read_only = match parts.next() {
            None | Some("rw") => false,
            Some("ro") => true,
            Some(o) => bail!("volume {v:?}: unknown option {o:?} (ro or rw)"),
        };
        out.push(json!({"name": name, "guest_path": path, "read_only": read_only}));
    }
    Ok(Value::Array(out))
}

/// `alpine:3.22` → `alpine-1a2b3c`: the image's last path segment and a short random suffix.
fn default_name(image: &str) -> String {
    let base = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(['@', ':'])
        .next()
        .unwrap_or("sandbox");
    let base: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let suffix = &Uuid::new_v4().simple().to_string()[..6];
    format!("{}-{suffix}", base.trim_matches('-'))
}

pub enum Target<'a> {
    Local(&'a Arc<VmManager>),
    Remote(&'a Remote),
}

impl Target<'_> {
    async fn create(&self, req: Value) -> Result<(Uuid, Vec<String>)> {
        Ok(match self {
            Target::Local(m) => {
                let req: SandboxCreateRequest = serde_json::from_value(req)?;
                let vm = m.create_sandbox(req, None, None).await?;
                (vm.id, runtime_note(&serde_json::to_value(&vm.labels)?))
            }
            Target::Remote(r) => {
                let vm = r.call(Method::POST, "/v1/sandboxes", Some(req)).await?;
                let id = vm["id"]
                    .as_str()
                    .context("the daemon returned no sandbox id")?
                    .parse()?;
                (id, runtime_note(&vm["labels"]))
            }
        })
    }

    pub async fn logs(&self, id: Uuid, lines: usize) -> Result<SandboxLogs> {
        match self {
            Target::Local(m) => m.sandbox_logs(id, lines).await,
            Target::Remote(r) => Ok(serde_json::from_value(
                r.call(
                    Method::GET,
                    &format!("/v1/sandboxes/{id}/logs?lines={lines}"),
                    None,
                )
                .await?,
            )?),
        }
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        match self {
            Target::Local(m) => m.delete(id).await,
            Target::Remote(r) => r
                .call(Method::DELETE, &format!("/v1/vms/{id}"), None)
                .await
                .map(drop),
        }
    }
}

/// The runtime the daemon chose and why, from the record's labels: printed so the choice is never silent.
fn runtime_note(labels: &Value) -> Vec<String> {
    let Some(rt) = labels["fluxvm.runtime"].as_str() else {
        return Vec::new();
    };
    let mut out = vec![format!(
        "runtime: {rt} ({})",
        labels["fluxvm.runtime.reason"].as_str().unwrap_or("")
    )];
    out.push(
        match rt {
            "container" => fluxvm_ctr::runtime::CONTAINER_ISOLATION,
            _ => fluxvm_ctr::runtime::VM_ISOLATION,
        }
        .to_string(),
    );
    out
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:=@%+,".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `--print-unit`: the container as the systemd unit it would be, built with the same code the daemon uses
/// (and this machine's defaults, so the daemon's `sandbox.container` limits may differ).
pub fn print_unit(args: &RunArgs) -> Result<String> {
    use fluxvm_core::config::ContainerRuntimeConfig;
    use fluxvm_ctr::{render, runtime::Trust};
    use fluxvm_scheduler::container_sandbox::{ContainerOptions, SpecInputs, build_spec};
    if let Some(r) = &args.rt.runtime
        && r != "container"
    {
        bail!(
            "--print-unit renders the native container runtime: use --runtime container (not {r})"
        );
    }
    if args.rt.runtime.is_none() {
        bail!("--print-unit needs --runtime container: it prints what that runtime would start");
    }
    let req: SandboxCreateRequest = serde_json::from_value(args.request()?)?;
    let oci = req.oci.clone().context("no image")?;
    fluxvm_scheduler::container_sandbox::refuse_unsupported(&req, &oci)?;
    let trust: Trust = args
        .rt
        .trust
        .as_deref()
        .unwrap_or("untrusted")
        .parse()
        .map_err(anyhow::Error::msg)?;
    let facts = fluxvm_ctr::probe::probe();
    let id = Uuid::new_v4();
    let argv: Vec<String> = match (&oci.entrypoint, &oci.command) {
        (None, None) => {
            vec!["<the image's Entrypoint and Cmd, read when the image is pulled>".into()]
        }
        (e, c) => e.iter().chain(c.iter()).flatten().cloned().collect(),
    };
    let process = fluxvm_oci_init::config::ProcessSpec {
        argv,
        env: oci.env.clone(),
        cwd: oci.workdir.clone().unwrap_or_else(|| "/".into()),
        user: oci.user.clone().unwrap_or_else(|| "0:0".into()),
    };
    let launcher = facts
        .launcher
        .clone()
        .unwrap_or_else(|| "/usr/bin/fluxvm-container-run".into());
    let manager = facts.manager();
    let options: ContainerOptions = req.container.clone().unwrap_or_default();
    let state = std::path::PathBuf::from(format!("/var/lib/fluxvm/instances/{id}"));
    let rootfs = std::path::PathBuf::from("/var/lib/fluxvm/ctr/rootfs/<manifest-digest>");
    let spec = build_spec(&SpecInputs {
        id,
        state_dir: &state,
        rootfs: &rootfs,
        image_ref: &args.image,
        process: &process,
        oci: &oci,
        req: &req,
        options: &options,
        cfg: &ContainerRuntimeConfig::default(),
        trust,
        rootless: !facts.root,
        launcher: &launcher,
        manager,
    })?;
    let mut out = format!(
        "# Nothing was run. This is the container `fluxctl sandbox run {} --runtime container` would start on this host
# (manager: {}; limits and defaults of this machine's config).
",
        args.image,
        manager.as_str()
    );
    out.push_str(&render::unit_file(&spec, !facts.root));
    out.push_str(
        "
# The same thing as a command:
",
    );
    out.push_str(
        &render::systemd_run_argv(&spec, manager)
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" \\\n  "),
    );
    out.push('\n');
    Ok(out)
}

/// Console lines written by init, the guest agent, or as protocol markers rather than by the container.
fn from_guest_system(line: &str) -> bool {
    [
        "fluxvm-oci-init",
        "fluxvm-guest-agent",
        "FLUXVM-",
        "VELORA-IP ",
    ]
    .iter()
    .any(|p| line.starts_with(p))
}

/// Lines of `now` not yet printed, given the `printed` lines of the previous poll's tail.
fn unseen<'a>(printed: &[&str], now: &'a str) -> Vec<&'a str> {
    let now: Vec<&str> = now.lines().collect();
    if printed.is_empty() {
        return now;
    }
    // The longest suffix of what was printed that is a prefix of the new tail.
    for start in 0..printed.len() {
        let overlap = &printed[start..];
        if now.len() >= overlap.len() && now[..overlap.len()] == *overlap {
            return now[overlap.len()..].to_vec();
        }
    }
    now
}

/// Creates the sandbox, follows its console until the process exits, and returns the process's exit code.
pub async fn run(target: Target<'_>, args: &RunArgs) -> Result<i32> {
    if args.rt.print_unit {
        print!("{}", print_unit(args)?);
        return Ok(0);
    }
    let (id, note) = target.create(args.request()?).await?;
    eprintln!("sandbox {id} started from {}", args.image);
    for line in note {
        eprintln!("{line}");
    }
    let outcome = follow(&target, id, Duration::from_secs(args.timeout_seconds)).await;
    if args.rm
        && let Err(e) = target.delete(id).await
    {
        eprintln!("warning: deleting sandbox {id}: {e:#}");
    }
    outcome
}

async fn follow(target: &Target<'_>, id: Uuid, timeout: Duration) -> Result<i32> {
    let started = Instant::now();
    let mut last = String::new();
    let mut stopped = false;
    loop {
        let logs = target.logs(id, POLL_LINES).await?;
        {
            let printed: Vec<&str> = last.lines().collect();
            for line in unseen(&printed, &logs.log) {
                if from_guest_system(line) {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
        }
        last = logs.log;
        if let Some(reason) = logs.init_error {
            bail!("the container did not start: {reason}");
        }
        if let Some(code) = logs.exit_code {
            return Ok(code);
        }
        // Init logs the exit code before powering off; one more look covers a log read that raced the stop.
        if matches!(logs.status, VmStatus::Stopped | VmStatus::Failed) {
            if stopped {
                bail!("sandbox {id} stopped without reporting an exit code");
            }
            stopped = true;
        }
        if started.elapsed() > timeout {
            bail!(
                "sandbox {id} still running after {}s; it was left as is",
                timeout.as_secs()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        args: RunArgs,
    }

    impl RunArgs {
        fn try_parse_from<I: IntoIterator<Item = &'static str>>(
            it: I,
        ) -> Result<RunArgs, clap::Error> {
            Wrap::try_parse_from(it).map(|w| w.args)
        }
    }

    #[test]
    fn the_runtime_choice_is_always_explained() {
        let labels =
            json!({"fluxvm.runtime": "container", "fluxvm.runtime.reason": "auto: trust=dev"});
        let n = runtime_note(&labels);
        assert_eq!(n[0], "runtime: container (auto: trust=dev)");
        assert!(n[1].contains("shares the host kernel"));
        let n = runtime_note(&json!({"fluxvm.runtime": "vm", "fluxvm.runtime.reason": "r"}));
        assert!(n[1].contains("own kernel"));
        assert!(runtime_note(&json!({})).is_empty());
    }

    #[test]
    fn bad_runtime_trust_and_net_are_rejected_before_anything_is_sent() {
        let mut a = RunArgs::try_parse_from(["run", "alpine"]).unwrap();
        a.rt.runtime = Some("docker".into());
        assert!(
            a.request()
                .unwrap_err()
                .to_string()
                .contains("unknown runtime")
        );
        a.rt.runtime = Some("vm".into());
        a.rt.trust = Some("hostile".into());
        assert!(
            a.request()
                .unwrap_err()
                .to_string()
                .contains("unknown trust")
        );
        a.rt.trust = None;
        a.rt.net = Some("bridge".into());
        assert!(
            a.request()
                .unwrap_err()
                .to_string()
                .contains("none or host")
        );
    }

    #[test]
    fn print_unit_needs_the_container_runtime_and_runs_nothing() {
        let mut a = RunArgs::try_parse_from(["run", "alpine:3.22", "--print-unit"]).unwrap();
        assert!(
            print_unit(&a)
                .unwrap_err()
                .to_string()
                .contains("--runtime container")
        );
        a.rt.runtime = Some("vm".into());
        assert!(print_unit(&a).unwrap_err().to_string().contains("not vm"));
        let a = RunArgs::try_parse_from([
            "run",
            "alpine:3.22",
            "--runtime",
            "container",
            "--trust",
            "dev",
            "--memory-mib",
            "128",
            "--pids",
            "32",
            "--print-unit",
            "--",
            "echo",
            "hi",
        ])
        .unwrap();
        let u = print_unit(&a).unwrap();
        assert!(u.contains("Nothing was run"), "{u}");
        assert!(
            u.contains("MemoryMax=128M") && u.contains("TasksMax=32"),
            "{u}"
        );
        assert!(
            u.contains("[X-FluxVM-Container]") && u.contains("PrivateNetwork=yes"),
            "{u}"
        );
        assert!(u.contains("systemd-run"), "{u}");
        assert!(u.contains("'--description=FluxVM container"), "{u}");
        let ports = RunArgs::try_parse_from([
            "run",
            "nginx",
            "--runtime",
            "container",
            "-p",
            "8080:80",
            "--print-unit",
        ])
        .unwrap();
        assert!(
            print_unit(&ports)
                .unwrap_err()
                .to_string()
                .contains("does not support ports")
        );
    }

    #[test]
    fn unseen_lines_follow_a_moving_tail() {
        assert_eq!(unseen(&[], "a\nb"), ["a", "b"]);
        assert_eq!(unseen(&["a", "b"], "a\nb\nc"), ["c"]);
        assert_eq!(unseen(&["a", "b", "c"], "b\nc\nd\ne"), ["d", "e"]);
        assert!(unseen(&["a", "b"], "a\nb").is_empty());
    }

    #[test]
    fn guest_system_lines_are_told_apart() {
        for l in [
            "fluxvm-oci-init: powering off",
            "fluxvm-guest-agent listening on vsock port 17777",
            "FLUXVM-EXIT 7",
            "VELORA-IP 192.168.64.2",
        ] {
            assert!(from_guest_system(l), "{l}");
        }
        assert!(!from_guest_system("x86_64"));
        assert!(!from_guest_system("uid=65534"));
    }

    #[test]
    fn names_come_from_the_image() {
        let n = default_name("ghcr.io/org/My_App:1.2");
        assert!(n.starts_with("my-app-"), "{n}");
        assert_eq!(n.len(), "my-app-".len() + 6);
        assert!(default_name("alpine@sha256:abc").starts_with("alpine-"));
    }

    #[test]
    fn run_requests_poweroff_and_a_read_only_root() {
        let args = RunArgs {
            image: "alpine:3.22".into(),
            platform: Some("linux/amd64".into()),
            name: Some("t".into()),
            profile: None,
            vcpus: None,
            memory_mib: Some(256),
            offline: false,
            allow_hosts: vec![],
            env: vec!["A=1".into()],
            secret_env: vec!["PATH".into()],
            workdir: None,
            user: None,
            entrypoint: Some("/bin/sh -c".into()),
            writable_root: false,
            publish: vec!["8080:80".into()],
            volumes: vec!["data:/data".into(), "cfg:/srv/cfg:ro".into()],
            restart: Some("on-failure".into()),
            max_restarts: Some(2),
            health_cmd: Some("wget -q -O /dev/null http://127.0.0.1".into()),
            health_interval: Some(5),
            rm: true,
            timeout_seconds: 10,
            command: vec!["echo".into(), "hi".into()],
            rt: RuntimeArgs {
                runtime: Some("container".into()),
                trust: Some("dev".into()),
                cpu_percent: Some(50),
                pids: Some(64),
                ..Default::default()
            },
        };
        let req: SandboxCreateRequest = serde_json::from_value(args.request().unwrap()).unwrap();
        let oci = req.oci.unwrap();
        assert_eq!(oci.command.unwrap(), ["echo", "hi"]);
        assert_eq!(oci.entrypoint.unwrap(), ["/bin/sh", "-c"]);
        assert_eq!(oci.platform.as_deref(), Some("linux/amd64"));
        assert!(oci.read_only_root);
        assert_eq!(
            serde_json::to_value(oci.exit_policy).unwrap(),
            json!("poweroff")
        );
        assert_eq!(oci.ports, ["8080:80"]);
        assert_eq!(
            oci.secret_env["PATH"].expose(),
            std::env::var("PATH").unwrap()
        );
        assert_eq!(oci.max_restarts, Some(2));
        assert_eq!(oci.healthcheck.unwrap().interval_seconds, 5);
        assert_eq!(req.volumes.len(), 2);
        assert!(req.volumes[1].read_only && req.volumes[1].guest_path == "/srv/cfg");
        assert_eq!(req.memory_mib, Some(256));
        assert_eq!(req.runtime, Some(fluxvm_ctr::runtime::Runtime::Container));
        assert_eq!(req.trust, Some(fluxvm_ctr::runtime::Trust::Dev));
        assert_eq!(req.container.unwrap().pids, Some(64));
        assert!(volumes_json(&["bad".into()]).is_err());
        assert!(volumes_json(&["a:/b:xx".into()]).is_err());
    }
}
