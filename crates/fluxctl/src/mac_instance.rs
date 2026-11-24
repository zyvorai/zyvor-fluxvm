// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Starting `fluxctl mac` instances with Docker-style options: `-e`, `--env-file`, `-v`, `--workdir`, `--label`, `--restart`,
//! `--cpus`, `--memory`, `-d`. `mac run` and `mac compose up` both go through [`launch`].
//!
//! What the daemon can and cannot do shapes the options:
//! - `--cpus` / `--memory` become `vcpus` / `memory_mib` of the VM request.
//! - `-v HOST:GUEST[:ro]` becomes a virtiofs `shared_folders` entry. A macOS guest shows shares under
//!   `/Volumes/My Shared Files`; the GUEST path is made with a symlink over SSH after boot (best effort, a warning when it
//!   fails). The mount name under that directory (`fs0`, `fs1`, ...) is a guess from the Linux tags: UNVERIFIED on a real guest.
//! - `-e` and `--workdir` apply to the command `run` sends over SSH. They are not stored in the VM, so `mac exec` does not see
//!   them (use `mac exec -e`).
//! - `--restart` is a label. Nothing in the daemon restarts a macOS VM: `fluxctl mac reconcile` applies the policy when run.
//!
//! Not verified against a real macOS VM: the request bodies and shell text are checked as data only.

use crate::mac_config::{self, ImageConfig};
use crate::mac_inspect::{
    IMAGE_LABEL, INSTANCE_LABEL, MOUNTS_LABEL, PROJECT_LABEL, RESTART_LABEL, SERVICE_LABEL,
    is_internal_label,
};
use crate::mac_store::{self, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Where a macOS guest shows VZ shared folders.
pub const GUEST_SHARES_DIR: &str = "/Volumes/My Shared Files";

/// Flags shared by `mac run` and (through `LaunchSpec`) compose. Flattened into `mac run`.
#[derive(clap::Args, Debug, Default, Clone)]
pub struct RunOpts {
    /// Set an environment variable for CMD: `NAME=value`, or `NAME` to copy it from this shell. Repeatable. Not stored in the VM.
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
    /// Read `NAME=value` lines (`#` comments allowed) for CMD. Repeatable.
    #[arg(long = "env-file", value_name = "FILE")]
    pub env_file: Vec<PathBuf>,
    /// Share a folder from this Mac into the guest (virtiofs): `HOST:GUEST[:ro]`. Repeatable. The daemon must run on this Mac.
    #[arg(short = 'v', long = "volume", value_name = "HOST:GUEST[:ro]")]
    pub volume: Vec<String>,
    /// Directory to run CMD in (overrides the image's WORKDIR).
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,
    /// Label the instance: `KEY=value`. Repeatable. Shown by `mac inspect`, matched by `mac ps --filter label=`.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub label: Vec<String>,
    /// Restart policy recorded on the instance: `no`, `always`, `unless-stopped` or `on-failure`. Applied by `fluxctl mac
    /// reconcile`; the daemon itself never restarts a macOS VM.
    #[arg(long, default_value = "no", value_name = "POLICY")]
    pub restart: String,
    /// Virtual CPUs for the guest.
    #[arg(long, value_name = "N")]
    pub cpus: Option<u8>,
    /// Guest memory: `8g`, `512m`, or MiB.
    #[arg(long, value_name = "SIZE")]
    pub memory: Option<String>,
    /// With CMD: start it in the background in the guest (output in /tmp/fluxvm-mac-cmd.log there) and return. Without CMD the
    /// instance already runs detached.
    #[arg(short = 'd', long)]
    pub detach: bool,
    #[command(flatten)]
    pub life: crate::mac_lifecycle::LifeOpts,
    #[command(flatten)]
    pub tty: crate::mac_exec::TtyFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

impl Restart {
    pub fn parse(s: &str) -> Result<Restart> {
        // `on-failure:3` is Docker's retry count: accepted, count ignored (nothing retries on a schedule here).
        Ok(match s.split(':').next().unwrap_or("") {
            "no" | "" => Restart::No,
            "always" => Restart::Always,
            "unless-stopped" => Restart::UnlessStopped,
            "on-failure" => Restart::OnFailure,
            _ => bail!("restart policy {s:?}: use no, always, unless-stopped or on-failure"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Restart::No => "no",
            Restart::Always => "always",
            Restart::UnlessStopped => "unless-stopped",
            Restart::OnFailure => "on-failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
    pub read_only: bool,
}

/// `HOST:GUEST[:ro|:rw]`. A relative HOST (`./src`, `../x`, `~/x`) is resolved against `cwd` / `home`; a bare name (a Docker
/// named volume) is refused because there is nothing to back it.
pub fn parse_mount(s: &str, cwd: &Path, home: Option<&Path>) -> Result<Mount> {
    let parts: Vec<&str> = s.split(':').collect();
    let (host, guest, mode) = match parts.as_slice() {
        [h, g] => (*h, *g, ""),
        [h, g, m] => (*h, *g, *m),
        _ => bail!("volume {s:?} must be HOST:GUEST[:ro]"),
    };
    let read_only = match mode {
        "" | "rw" => false,
        "ro" => true,
        m => bail!("volume {s:?}: mode {m:?} must be ro or rw"),
    };
    ensure!(
        guest.starts_with('/') && !guest.contains(".."),
        "volume {s:?}: the guest path must be absolute (and not contain ..)"
    );
    let host_path = if let Some(rest) = host.strip_prefix("~/") {
        home.context("HOME is not set: cannot expand ~")?.join(rest)
    } else if host.starts_with('/') {
        PathBuf::from(host)
    } else if host.starts_with("./") || host.starts_with("../") || host == "." || host == ".." {
        cwd.join(host)
    } else {
        bail!(
            "volume {s:?}: {host:?} is not a path (named volumes are not supported; use ./{host} or an absolute path)"
        );
    };
    Ok(Mount {
        host: host_path,
        guest: guest.to_string(),
        read_only,
    })
}

/// Shell text, run in the guest after boot, that links each guest path to its share. Best effort: it tries without `sudo`,
/// then with `sudo -n`, and prints a warning on stderr instead of failing the run.
pub fn link_command(mounts: &[Mount]) -> Option<String> {
    if mounts.is_empty() {
        return None;
    }
    let parts: Vec<String> = mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let src = mac_config::sq(&format!("{GUEST_SHARES_DIR}/fs{i}"));
            let dst = mac_config::sq(&m.guest);
            let parent = mac_config::sq(
                Path::new(&m.guest)
                    .parent()
                    .and_then(|p| p.to_str())
                    .filter(|p| !p.is_empty())
                    .unwrap_or("/"),
            );
            format!(
                "{{ {{ mkdir -p {parent} && ln -sfn {src} {dst}; }} 2>/dev/null || {{ sudo -n mkdir -p {parent} && sudo -n ln -sfn {src} {dst}; }} 2>/dev/null || echo {} >&2; }}",
                mac_config::sq(&format!("warning: could not link {} to its share", m.guest))
            )
        })
        .collect();
    Some(parts.join("; "))
}

/// `-- CMD` for `-d`: run it in the background in the guest.
pub fn detach_command(command: &str) -> String {
    format!(
        "nohup sh -c {} >/tmp/fluxvm-mac-cmd.log 2>&1 </dev/null &",
        mac_config::sq(command)
    )
}

/// Everything needed to create and start one instance.
#[derive(Debug, Clone, Default)]
pub struct LaunchSpec {
    pub name: String,
    pub image: String,
    pub publish: Vec<String>,
    pub mounts: Vec<Mount>,
    pub cpus: Option<u8>,
    pub memory_mib: Option<u64>,
    /// User labels (`--label`), without ours.
    pub labels: Vec<(String, String)>,
    pub restart: Option<Restart>,
    /// `(project, service)` for compose.
    pub project: Option<(String, String)>,
    /// `--health-cmd` / `--no-healthcheck`: this instance's own healthcheck, recorded in a label.
    pub healthcheck: Option<crate::mac_lifecycle::HcSetting>,
}

/// POST /v1/vms body: [`crate::mac_cmd::build_create_body`] plus CPU, memory and shares.
pub fn create_body(spec: &LaunchSpec, disk: &Path) -> Result<Value> {
    let mut body = crate::mac_cmd::build_create_body(&spec.name, disk, &spec.publish)?;
    if let Some(c) = spec.cpus {
        ensure!(c >= 1, "--cpus must be at least 1");
        body["vcpus"] = json!(c);
    }
    if let Some(m) = spec.memory_mib {
        body["memory_mib"] = json!(m);
    }
    if !spec.mounts.is_empty() {
        for m in &spec.mounts {
            ensure!(
                m.host.is_dir(),
                "{} is not an existing directory on this Mac (volumes are directories)",
                m.host.display()
            );
        }
        body["shared_folders"] = spec
            .mounts
            .iter()
            .map(|m| json!({"host_path": m.host, "guest_path": m.guest, "read_only": m.read_only}))
            .collect();
    }
    Ok(body)
}

/// The PATCH body that marks the VM as an instance and records the options that have no VM field of their own.
pub fn labels_patch(spec: &LaunchSpec) -> Result<Value> {
    let mut labels = serde_json::Map::new();
    labels.insert(INSTANCE_LABEL.into(), json!("instance"));
    labels.insert(IMAGE_LABEL.into(), json!(spec.image));
    for (k, v) in &spec.labels {
        ensure!(
            !is_internal_label(k),
            "label {k:?}: keys starting with fluxvm.mac are reserved"
        );
        labels.insert(k.clone(), json!(v));
    }
    if let Some(r) = spec.restart.filter(|r| *r != Restart::No) {
        labels.insert(RESTART_LABEL.into(), json!(r.as_str()));
    }
    if let Some(h) = &spec.healthcheck {
        let (k, v) = crate::mac_lifecycle::healthcheck_label(h);
        labels.insert(k.into(), json!(v));
    }
    if let Some((p, s)) = &spec.project {
        labels.insert(PROJECT_LABEL.into(), json!(p));
        labels.insert(SERVICE_LABEL.into(), json!(s));
    }
    if !spec.mounts.is_empty() {
        let m: Vec<String> = spec
            .mounts
            .iter()
            .enumerate()
            .map(|(i, m)| format!("fs{i}={}", m.guest))
            .collect();
        labels.insert(MOUNTS_LABEL.into(), json!(m.join(",")));
    }
    Ok(json!({ "labels": labels }))
}

pub fn parse_label(s: &str) -> Result<(String, String)> {
    let Some((k, v)) = s.split_once('=') else {
        bail!("label {s:?} must be KEY=VALUE");
    };
    ensure!(
        !k.is_empty() && !k.contains(char::is_whitespace),
        "label {s:?} has an invalid key"
    );
    Ok((k.to_string(), v.to_string()))
}

/// `-e` and `--env-file` into one ordered list (files first, then `-e`, so a flag wins).
pub fn collect_env(
    env: &[String],
    env_files: &[PathBuf],
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for f in env_files {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        out.extend(
            mac_config::parse_env_file(&text, lookup)
                .with_context(|| format!("in {}", f.display()))?,
        );
    }
    for e in env {
        out.extend(mac_config::parse_env_pair(e, lookup)?);
    }
    Ok(out)
}

fn host_env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// Turn a daemon error from `action` into something a user can act on. 409 and 503 are told apart by the daemon's text:
/// the guest limit and "start it first" are both 409.
pub fn explain(e: anyhow::Error, action: &str) -> anyhow::Error {
    let t = format!("{e:#}");
    if t.contains(": 409 ") && t.contains("macOS guests") {
        anyhow::anyhow!(
            "cannot {action}: this Mac already runs 2 macOS VMs (Apple's licence limit). Free a slot with `fluxctl mac stop NAME` or `fluxctl mac rm NAME` (see `fluxctl mac ps -a`), then retry. Daemon said: {t}"
        )
    } else if t.contains(": 409 ") && t.contains("start it first") {
        anyhow::anyhow!(
            "cannot {action}: the instance is not running. Start it with `fluxctl mac start NAME`. Daemon said: {t}"
        )
    } else if t.contains(": 409 ") {
        anyhow::anyhow!(
            "cannot {action}: the daemon reports a conflict (409): the instance is in a state that does not allow it. Daemon said: {t}"
        )
    } else if t.contains(": 503 ") {
        anyhow::anyhow!(
            "cannot {action}: this Mac's login session is locked (503), and a macOS guest cannot start or restore from a locked session. Unlock it (a headless host needs an auto-login user with the lock off) and retry. Daemon said: {t}"
        )
    } else if t.contains(": 404 ") {
        anyhow::anyhow!(
            "cannot {action}: the daemon has no such VM (was it removed? see `fluxctl mac ps -a`). Daemon said: {t}"
        )
    } else {
        e
    }
}

/// Create the VM and label it, without starting it (`mac create`). `extra` are more labels to record. On failure after creation
/// the VM is deleted. Returns the VM id.
pub async fn create_instance(
    r: &crate::remote::Remote,
    store: &Store,
    spec: &LaunchSpec,
    extra: serde_json::Map<String, Value>,
) -> Result<String> {
    use reqwest::Method;
    crate::mac_ports::preflight(r, &spec.publish).await?;
    let disk = store.resolve(&spec.image)?.join("disk.raw");
    let body = create_body(spec, &disk)?;
    let mut patch = labels_patch(spec)?;
    if let Some(l) = patch["labels"].as_object_mut() {
        l.extend(extra);
    }
    let vm = r
        .call(Method::POST, "/v1/vms", Some(body))
        .await
        .map_err(|e| explain(e, &format!("create {}", spec.name)))?;
    let id = vm["id"]
        .as_str()
        .context("the daemon returned no id")?
        .to_string();
    if let Err(e) = r
        .call(Method::PATCH, &format!("/v1/vms/{id}"), Some(patch))
        .await
    {
        crate::mac_cmd::delete_instance(r, &id).await.ok();
        return Err(e);
    }
    // The daemon never restarts a macOS VM: a policy needs the supervisor, so make sure it is there.
    if let Some(p) = spec.restart.filter(|p| *p != Restart::No) {
        crate::mac_supervise::ensure_for_policy(r, p.as_str());
    }
    Ok(id)
}

/// Create the VM, label it, start it. On failure after creation the VM is deleted. Returns the VM id.
pub async fn launch(r: &crate::remote::Remote, store: &Store, spec: &LaunchSpec) -> Result<String> {
    use reqwest::Method;
    let mut start = serde_json::Map::new();
    // The start period of a healthcheck counts from here.
    start.insert(
        crate::mac_health::HEALTH_START_LABEL.into(),
        json!(crate::mac_health::now_unix().to_string()),
    );
    let id = create_instance(r, store, spec, start).await?;
    let started = r
        .call(Method::POST, &format!("/v1/vms/{id}/start"), None)
        .await
        .map_err(|e| explain(e, &format!("start {}", spec.name)));
    if let Err(e) = started {
        crate::mac_cmd::delete_instance(r, &id).await.ok();
        return Err(e);
    }
    Ok(id)
}

/// What to do inside the guest once it answers on SSH.
#[derive(Debug, Clone, Default)]
pub struct GuestJob {
    pub user: String,
    pub wait: u64,
    pub command: Option<String>,
    pub mounts: Vec<Mount>,
    pub detach: bool,
    /// Terminal for the foreground command.
    pub tty: crate::mac_exec::Tty,
    /// No command, but the user asked for one (`run -it`): open the guest's login shell.
    pub shell: bool,
}

/// Wait for SSH, link the shares, run the command (foreground, or background with `detach`). Returns the guest address.
pub async fn run_in_guest(r: &crate::remote::Remote, id: &str, job: &GuestJob) -> Result<String> {
    let ip = crate::mac_cmd::wait_for_ssh(r, id, job.wait).await?;
    if let Some(link) = link_command(&job.mounts) {
        // A failed link is already reduced to a warning by the command itself.
        crate::run::ssh_to(&ip, &job.user, 22, true, &[link])
            .await
            .ok();
    }
    if let Some(cmd) = &job.command {
        let text = if job.detach {
            detach_command(cmd)
        } else {
            cmd.clone()
        };
        run_ssh(&ip, job, &[text]).await?;
    } else if job.shell {
        run_ssh(&ip, job, &[]).await?;
    }
    Ok(ip)
}

async fn run_ssh(ip: &str, job: &GuestJob, args: &[String]) -> Result<()> {
    let code = crate::run::ssh_to_code(ip, &job.user, 22, true, job.tty, args).await?;
    if code != 0 {
        bail!("ssh exited with exit status: {code}");
    }
    Ok(())
}

/// What `mac run` did, for `--json` and for the plain output.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub name: String,
    pub id: String,
    pub image: String,
    /// The guest address, when the run waited for SSH.
    pub ip: Option<String>,
    /// The instance was deleted at the end (`--rm`).
    pub removed: bool,
    /// A command ran in the guest (in the background with `-d`).
    pub ran_command: bool,
    pub detached: bool,
    /// The plain output prints the instance name (the instance stays up and nothing else was printed).
    pub print_name: bool,
}

/// `fluxctl mac run`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    r: &crate::remote::Remote,
    rm: bool,
    name: Option<String>,
    publish: Vec<String>,
    user: Option<String>,
    wait: u64,
    image: String,
    cmd: Vec<String>,
    opts: RunOpts,
) -> Result<()> {
    let o = run_outcome(r, rm, name, publish, user, wait, image, cmd, opts).await?;
    if o.print_name {
        println!("{}", o.name);
    }
    Ok(())
}

/// `fluxctl mac run`, returning what happened instead of printing it.
#[allow(clippy::too_many_arguments)]
pub async fn run_outcome(
    r: &crate::remote::Remote,
    rm: bool,
    name: Option<String>,
    publish: Vec<String>,
    user: Option<String>,
    wait: u64,
    image: String,
    cmd: Vec<String>,
    opts: RunOpts,
) -> Result<RunOutcome> {
    let store = Store::open(mac_store::default_root()?)?;
    let dir = store.resolve(&image)?;
    let mut cfg: ImageConfig = mac_config::load(&dir)?;
    crate::mac_lifecycle::apply_entrypoint(&mut cfg, opts.life.entrypoint.as_deref())?;
    let health = opts.life.resolve_healthcheck(cfg.healthcheck.as_ref())?;
    let env = collect_env(&opts.env, &opts.env_file, host_env)?;
    let command = mac_config::effective_command(&cfg, &cmd, &env, opts.workdir.as_deref());
    ensure!(
        !(opts.detach && (opts.tty.tty || opts.tty.interactive)),
        "-d runs in the background, so -i/-t do not apply: use `fluxctl mac run -it IMAGE` for a shell, or `fluxctl mac exec -it NAME` later"
    );
    // `run -it` with no command is a shell in the new instance, as `docker run -it IMAGE` is.
    let want_shell = command.is_none() && (opts.tty.tty || opts.tty.interactive);
    let command = if want_shell {
        let mut all = cfg.env.clone();
        all.extend(env.iter().cloned());
        let wd = opts.workdir.as_deref().or(cfg.workdir.as_deref());
        crate::mac_exec::shell_args(&all, wd).pop()
    } else {
        command
    };
    let command_or_shell = command.is_some() || want_shell;
    ensure!(
        !rm || command_or_shell,
        "--rm needs a command to run (fluxctl mac run --rm IMAGE -- CMD, or an image with CMD/ENTRYPOINT)"
    );
    ensure!(
        !(rm && opts.detach),
        "--rm and -d cannot be combined: the instance would be deleted while CMD still runs"
    );
    let cwd = std::env::current_dir().context("current directory")?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let name = name.unwrap_or_else(crate::mac_cmd::random_name);
    let token = uuid::Uuid::new_v4().to_string();
    let mounts = opts
        .volume
        .iter()
        .map(|v| crate::mac_volume::parse_mount_default(v, &cwd, home.as_deref(), Some(&token)))
        .collect::<Result<Vec<_>>>()?;
    let mut labels = opts
        .label
        .iter()
        .map(|l| parse_label(l))
        .collect::<Result<Vec<_>>>()?;
    labels.push((crate::mac_volume::INSTANCE_ID_LABEL.into(), token));
    let spec = LaunchSpec {
        name: name.clone(),
        image: image.clone(),
        publish,
        mounts: mounts.clone(),
        cpus: opts.cpus,
        memory_mib: opts
            .memory
            .as_deref()
            .map(mac_config::parse_memory_mib)
            .transpose()?,
        labels,
        restart: Some(Restart::parse(&opts.restart)?),
        project: None,
        healthcheck: health,
    };
    let id = launch(r, &store, &spec).await?;
    let outcome = |ip: Option<String>, removed: bool, print_name: bool| RunOutcome {
        name: name.clone(),
        id: id.clone(),
        image: image.clone(),
        ip,
        removed,
        ran_command: command_or_shell,
        detached: opts.detach,
        print_name,
    };
    if !command_or_shell && mounts.is_empty() {
        return Ok(outcome(None, false, true));
    }
    let job = GuestJob {
        user: user
            .or(cfg.user.clone())
            .unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.to_string()),
        wait,
        command: command.clone(),
        mounts,
        detach: opts.detach,
        tty: if command_or_shell {
            crate::mac_exec::resolve_here(opts.tty)
        } else {
            Default::default()
        },
        shell: want_shell,
    };
    let result = run_in_guest(r, &id, &job).await;
    if !command_or_shell || opts.detach {
        // Nothing to wait for: the instance stays up.
        return Ok(outcome(Some(result?), false, true));
    }
    if rm {
        crate::mac_cmd::delete_instance(r, &id).await.ok();
    } else {
        eprintln!("instance {name} is still running (fluxctl mac rm {name})");
    }
    Ok(outcome(Some(result?), rm, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(s: &str) -> Result<Mount> {
        parse_mount(s, Path::new("/work"), Some(Path::new("/Users/me")))
    }

    #[test]
    fn mounts_parse_and_resolve() {
        assert_eq!(
            mount("/a/b:/src").unwrap(),
            Mount {
                host: "/a/b".into(),
                guest: "/src".into(),
                read_only: false
            }
        );
        assert!(mount("./x:/src:ro").unwrap().read_only);
        assert_eq!(mount("./x:/src").unwrap().host, PathBuf::from("/work/./x"));
        assert_eq!(
            mount("~/code:/c:rw").unwrap().host,
            PathBuf::from("/Users/me/code")
        );
        assert!(
            mount("data:/d")
                .unwrap_err()
                .to_string()
                .contains("named volumes")
        );
        assert!(mount("/a:rel").is_err());
        assert!(mount("/a:/b/../c").is_err());
        assert!(mount("/a").is_err());
        assert!(
            mount("/a:/b:zz")
                .unwrap_err()
                .to_string()
                .contains("ro or rw")
        );
        assert!(mount("/a:/b:ro:x").is_err());
        assert!(parse_mount("~/x:/y", Path::new("/"), None).is_err());
    }

    #[test]
    fn link_command_quotes_and_warns_instead_of_failing() {
        assert_eq!(link_command(&[]), None);
        let c = link_command(&[
            Mount {
                host: "/h".into(),
                guest: "/work space/src".into(),
                read_only: false,
            },
            Mount {
                host: "/h2".into(),
                guest: "/data".into(),
                read_only: true,
            },
        ])
        .unwrap();
        assert!(
            c.contains("ln -sfn '/Volumes/My Shared Files/fs0' '/work space/src'"),
            "{c}"
        );
        assert!(c.contains("'/Volumes/My Shared Files/fs1' '/data'"), "{c}");
        assert!(c.contains("sudo -n mkdir -p '/work space'"), "{c}");
        assert!(c.contains("warning: could not link /data"), "{c}");
        assert!(c.contains(">&2"));
    }

    #[test]
    fn detach_wraps_in_nohup_with_quoting() {
        let d = detach_command("echo 'hi' && sleep 1");
        assert!(
            d.starts_with("nohup sh -c 'echo '\\''hi'\\'' && sleep 1' >/tmp/fluxvm-mac-cmd.log"),
            "{d}"
        );
        assert!(d.ends_with('&'));
    }

    #[test]
    fn restart_policies() {
        assert_eq!(Restart::parse("no").unwrap(), Restart::No);
        assert_eq!(Restart::parse("always").unwrap(), Restart::Always);
        assert_eq!(
            Restart::parse("unless-stopped").unwrap(),
            Restart::UnlessStopped
        );
        assert_eq!(Restart::parse("on-failure:3").unwrap(), Restart::OnFailure);
        assert!(Restart::parse("sometimes").is_err());
        assert_eq!(Restart::OnFailure.as_str(), "on-failure");
    }

    #[test]
    fn create_body_carries_cpus_memory_and_shares() {
        let d = tempfile::tempdir().unwrap();
        let spec = LaunchSpec {
            name: "n".into(),
            image: "i:1".into(),
            publish: vec!["2222:22".into()],
            mounts: vec![Mount {
                host: d.path().into(),
                guest: "/src".into(),
                read_only: true,
            }],
            cpus: Some(6),
            memory_mib: Some(16384),
            ..Default::default()
        };
        let b = create_body(&spec, Path::new("/s/disk.raw")).unwrap();
        assert_eq!(b["vcpus"], 6);
        assert_eq!(b["memory_mib"], 16384);
        assert_eq!(b["apple"]["guest_os"], "macos");
        assert_eq!(b["network"]["forwards"][0]["guest_port"], 22);
        assert_eq!(b["shared_folders"][0]["guest_path"], "/src");
        assert_eq!(b["shared_folders"][0]["read_only"], true);
        // and the core accepts the shape.
        let req: fluxvm_core::model::CreateVmRequest = serde_json::from_value(b).unwrap();
        assert_eq!(req.vcpus, 6);
        assert_eq!(req.memory_mib, 16384);
        assert_eq!(req.shared_folders.len(), 1);
    }

    #[test]
    fn a_missing_host_directory_or_zero_cpus_is_refused() {
        let mut spec = LaunchSpec {
            name: "n".into(),
            image: "i".into(),
            mounts: vec![Mount {
                host: "/nonexistent-dir-xyz".into(),
                guest: "/s".into(),
                read_only: false,
            }],
            ..Default::default()
        };
        assert!(
            create_body(&spec, Path::new("/d"))
                .unwrap_err()
                .to_string()
                .contains("not an existing directory")
        );
        spec.mounts.clear();
        spec.cpus = Some(0);
        assert!(create_body(&spec, Path::new("/d")).is_err());
    }

    #[test]
    fn labels_record_policy_project_mounts_and_refuse_reserved_keys() {
        let spec = LaunchSpec {
            name: "n".into(),
            image: "i:1".into(),
            mounts: vec![Mount {
                host: "/h".into(),
                guest: "/g".into(),
                read_only: false,
            }],
            labels: vec![("team".into(), "ci".into())],
            restart: Some(Restart::Always),
            project: Some(("proj".into(), "svc".into())),
            ..Default::default()
        };
        let p = labels_patch(&spec).unwrap();
        let l = &p["labels"];
        assert_eq!(l["fluxvm.mac"], "instance");
        assert_eq!(l["fluxvm.mac.image"], "i:1");
        assert_eq!(l["team"], "ci");
        assert_eq!(l["fluxvm.mac.restart"], "always");
        assert_eq!(l["fluxvm.mac.project"], "proj");
        assert_eq!(l["fluxvm.mac.service"], "svc");
        assert_eq!(l["fluxvm.mac.mounts"], "fs0=/g");
        let plain = labels_patch(&LaunchSpec {
            name: "n".into(),
            image: "i".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(plain["labels"].get("fluxvm.mac.restart").is_none());
        let bad = LaunchSpec {
            labels: vec![("fluxvm.mac.image".into(), "x".into())],
            ..spec
        };
        assert!(
            labels_patch(&bad)
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
    }

    #[test]
    fn label_flags() {
        assert_eq!(parse_label("a=b=c").unwrap(), ("a".into(), "b=c".into()));
        assert!(parse_label("nokey").is_err());
        assert!(parse_label("=v").is_err());
        assert!(parse_label("a b=c").is_err());
    }

    #[test]
    fn env_flags_override_env_files() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("e.env");
        std::fs::write(&f, "A=file\nB=file\n# c\n").unwrap();
        let look = |k: &str| (k == "FROMSHELL").then(|| "sh".to_string());
        let env = collect_env(
            &["B=flag".into(), "FROMSHELL".into(), "NOPE".into()],
            std::slice::from_ref(&f),
            look,
        )
        .unwrap();
        assert_eq!(
            env,
            vec![
                ("A".into(), "file".into()),
                ("B".into(), "file".into()),
                ("B".into(), "flag".into()),
                ("FROMSHELL".into(), "sh".into())
            ]
        );
        // the later pair wins when rendered.
        let cmd = mac_config::effective_command(&ImageConfig::default(), &["x".into()], &env, None)
            .unwrap();
        assert!(
            cmd.contains("export B='flag';") && !cmd.contains("export B='file'"),
            "{cmd}"
        );
        assert!(collect_env(&[], &[d.path().join("missing")], look).is_err());
        let e = collect_env(&["1bad=x".into()], &[], look)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a valid environment variable"), "{e}");
    }

    #[test]
    fn errors_are_told_apart_by_status_and_text() {
        let limit = explain(anyhow::anyhow!("POST /v1/vms/x/start: 409 Conflict: this Mac already runs 2 macOS guests (Apple's licence limit); stop one"), "start web").to_string();
        assert!(
            limit.contains("cannot start web")
                && limit.contains("already runs 2 macOS VMs")
                && limit.contains("mac stop"),
            "{limit}"
        );
        let not_live = explain(
            anyhow::anyhow!(
                "POST /v1/vms/x/pause: 409 Conflict: the VM is Stopped; start it first"
            ),
            "pause web",
        )
        .to_string();
        assert!(
            not_live.contains("not running") && not_live.contains("mac start"),
            "{not_live}"
        );
        assert!(!not_live.contains("licence"), "{not_live}");
        let other409 = explain(
            anyhow::anyhow!("POST /v1/vms/x/start: 409 Conflict: busy"),
            "start web",
        )
        .to_string();
        assert!(other409.contains("conflict (409)"), "{other409}");
        let locked = explain(
            anyhow::anyhow!("POST /v1/vms/x/start: 503 Service Unavailable: locked"),
            "start web",
        )
        .to_string();
        assert!(
            locked.contains("login session is locked") && locked.contains("(503)"),
            "{locked}"
        );
        let nf = explain(
            anyhow::anyhow!("GET /v1/vms/x: 404 Not Found: gone"),
            "inspect web",
        )
        .to_string();
        assert!(nf.contains("no such VM"), "{nf}");
        assert_eq!(explain(anyhow::anyhow!("boom"), "x").to_string(), "boom");
    }
}
