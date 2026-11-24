// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac`: the Docker-style workflow for macOS guests (docs/macos-containers-design.md).
//! Phase 1 (images: pull, push, tag, rmi, import) is client-side only: a registry and the local store, never the daemon.
//! Phase 2 (run, ps, rm, exec) needs the daemon: an instance is a VM created from a template's `disk.raw` and labelled
//! `fluxvm.mac=instance`. Not verified against a real VM: no macOS guest could be booted when this was written.

use crate::mac_store::{self, Entry, REQUIRED_FILES, Store};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use fluxvm_image as image;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum MacCommand {
    /// List the local macOS images (`name:tag`, digest, size on disk).
    Images {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        extra: crate::mac_plus::ImagesExtra,
    },
    /// Pull `registry/repo:tag` into the local store (credentials: FLUXVM_REGISTRY_USER / FLUXVM_REGISTRY_PASSWORD).
    /// `--verify` demands a signature from a trusted key and pulls exactly the verified digest; chunks identical to another local
    /// tag of the same repository are reused instead of downloaded.
    Pull {
        reference: String,
        #[command(flatten)]
        extra: crate::mac_plus::PullExtra,
    },
    /// Push a local image to a registry. REMOTE defaults to REFERENCE when that already names a registry.
    /// Apple's licence terms limit where macOS images may be stored: use a private registry.
    /// Public registries (Docker Hub, GHCR, Quay, ...) are refused unless you pass --i-have-checked-the-licence.
    Push {
        reference: String,
        remote: Option<String>,
        #[command(flatten)]
        extra: crate::mac_plus::PushExtra,
    },
    /// Give an image a second name (no copy).
    Tag { source: String, target: String },
    /// Remove a name; the files go with the last name that uses them.
    Rmi { reference: String },
    /// Add a template directory (disk.raw, hardware.bin, auxiliary.bin) to the store as `name:tag`; an APFS clone, so it is instant
    /// on the same volume. Given a tar file from `mac save` or `mac export` instead, unpacks it (like `mac load`).
    Import { dir: PathBuf, reference: String },
    /// Build an image from a Macfile (FROM, USER, RUN, COPY): the steps run over SSH in a throwaway instance cloned from the FROM
    /// template, which is then stopped and stored as `-t name:tag`. `--dry-run` only prints the plan. Needs the daemon unless
    /// `--dry-run`. SNAPSHOT is not supported.
    Build {
        /// The Macfile (default `Macfile` in CONTEXT).
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Name for the resulting image (`name:tag`).
        #[arg(short = 't', long)]
        tag: String,
        /// Print the plan and change nothing. Without it the build runs in a throwaway instance (needs the daemon).
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait for the build instance to answer on SSH.
        #[arg(long, default_value_t = 900)]
        wait: u64,
        /// Set an `ARG` of the Macfile: `NAME=value`. Repeatable.
        #[arg(long = "build-arg", value_name = "NAME=VALUE")]
        build_arg: Vec<String>,
        /// Ignore the build cache and run every step from FROM (the cache is also not written). See `mac cache`.
        #[arg(long)]
        no_cache: bool,
        /// Add an image label: `KEY=VALUE`. Repeatable. Overrides a `LABEL` of the same key in the Macfile.
        #[arg(long = "label", value_name = "KEY=VALUE")]
        label: Vec<String>,
        /// Make a secret available to `RUN --mount=type=secret,id=NAME` steps: `id=NAME,src=FILE` or `id=NAME,env=VARIABLE`.
        /// It is copied into the guest for that one step and removed before the step ends; it is never in the image, the
        /// cache, the plan or this command's output. Repeatable.
        #[arg(long = "secret", value_name = "id=NAME,src=FILE")]
        secret: Vec<String>,
        /// Forward this shell's SSH agent to `RUN --mount=type=ssh` steps: `--ssh default`. No key is copied into the guest.
        #[arg(long = "ssh", value_name = "default")]
        ssh: Vec<String>,
        /// Only `darwin/arm64` (Apple silicon) exists for macOS images; accepted so Docker scripts keep working.
        #[arg(long, value_name = "darwin/arm64")]
        platform: Option<String>,
        /// Docker's multi-stage `--target`. A Macfile has one stage, so this is refused with an explanation.
        #[arg(long, value_name = "STAGE")]
        target: Option<String>,
        /// Build context directory: COPY sources are relative to it.
        // Own id: `context` is the global --context flag (a String) and clap panics on the clash.
        #[arg(id = "build_context", value_name = "CONTEXT", default_value = ".")]
        context: PathBuf,
    },
    /// Start a macOS VM from a local image (an APFS clone of its disk) and, with CMD, run it over SSH. Needs the daemon
    /// (`--server`/context). At most 2 macOS VMs run at once. Without CMD the instance keeps running: see `mac ps`.
    #[command(
        after_help = "Examples:\n  fluxctl mac run --rm macos-xcode:15 -- xcodebuild -version\n  fluxctl mac run -d --name ci -e TOKEN -v ./src:/src:ro --cpus 4 --memory 8g --restart unless-stopped macos-xcode:15 -- ./serve.sh\n\nA Mac runs at most 2 macOS VMs: a 409 means both slots are taken."
    )]
    Run {
        /// Delete the instance when CMD finishes (needs CMD, or an image with CMD/ENTRYPOINT).
        #[arg(long)]
        rm: bool,
        /// Instance name (default: mac-<random>).
        #[arg(long)]
        name: Option<String>,
        /// Publish a guest port on this Mac: `[ADDR:]HOST:GUEST`. Repeatable.
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        /// SSH user inside the guest (default: the image's USER, else `admin`). Docker's `-u`.
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Seconds to wait for the guest to answer on SSH.
        #[arg(long, default_value_t = 900)]
        wait: u64,
        #[command(flatten)]
        opts: crate::mac_instance::RunOpts,
        #[command(flatten)]
        queue: crate::mac_queue::QueueOpts,
        /// Print one JSON object (schema in docs/macos-containers.md) as the last line of stdout instead of the instance name.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        sched: crate::mac_fleet_run::FleetRun,
        #[command(flatten)]
        xrun: crate::mac_ops::RunExtra,
        /// A local image (`fluxctl mac images`). Not given with `--from-checkpoint`.
        #[arg(required_unless_present = "from_checkpoint", default_value = "")]
        image: String,
        /// The command to run in the guest, after `--`.
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// List the instances (VMs labelled `fluxvm.mac=instance`). Shows running ones unless `-a`.
    #[command(
        after_help = "Examples:\n  fluxctl mac ps -a\n  fluxctl mac ps -f status=running -f name=build --format '{{.Name}} {{.IP}}'\n  fluxctl mac ps -q"
    )]
    Ps {
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        opts: crate::mac_more::PsOpts,
    },
    /// Stop and delete an instance.
    Rm { name: String },
    /// Run a command in a running instance over SSH, like `docker exec`: `fluxctl mac exec NAME -- sw_vers`. `-it` gives a
    /// terminal (a shell, `top`, an editor); with no command you get the guest's login shell. The command's exit code is yours.
    #[command(
        after_help = "Examples:\n  fluxctl mac exec web -- sw_vers\n  fluxctl mac exec -it web                      (a login shell)\n  fluxctl mac exec -it -w /Users/admin/site -e DEBUG=1 web -- zsh\n  fluxctl mac exec -u builder web -- whoami\n  fluxctl mac exec --print web -- uptime        (show the ssh command, run nothing)"
    )]
    Exec {
        name: String,
        /// The guest account to run as (Docker's `-u`). It logs in over SSH, so it must exist in the guest.
        #[arg(short = 'u', long, default_value = "admin")]
        user: String,
        #[command(flatten)]
        opts: crate::mac_more::ExecOpts,
        /// The command, after `--`. Empty means the login shell.
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Preflight: Apple silicon, macOS version, free macOS VM slots, free disk and APFS clone volume, login session, daemon,
    /// ssh key, FileVault, store location. Read-only (it creates the store directory); every problem prints a fix. Exits 1
    /// when a check fails.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Block until an instance is running, has an address and answers on SSH (and, with --check, until CMD exits 0 in the guest).
    #[command(
        name = "wait-ready",
        after_help = "Example:
  fluxctl mac run -d --name ci macos-xcode:15 && fluxctl mac wait-ready ci --check 'xcodebuild -version'"
    )]
    WaitReady {
        name: String,
        /// Give up after this many seconds (0 = never).
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// SSH user for --check.
        #[arg(long, default_value = "admin")]
        user: String,
        /// A command that must exit 0 in the guest (over SSH) before the instance counts as ready.
        #[arg(long, value_name = "CMD")]
        check: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// GitHub Actions runners on macOS: one ephemeral VM per job, registered just in time, deleted after the job. Spreads over
    /// a Mac fleet with --fleet and never runs more than 2 guests per Mac. See docs/mac-runner.md.
    Runner(crate::mac_runner::RunnerArgs),
    #[command(flatten)]
    More(crate::mac_more::MacMore),
    /// The Mac that runs FluxVM as a server: power and sleep status, a keep-awake LaunchAgent, the unlocked-session check,
    /// recommended settings and Rosetta. Read-only unless you pass `--apply`. See docs/macos-host.md.
    Host {
        #[command(subcommand)]
        command: crate::mac_host::HostCommand,
    },
    #[command(flatten)]
    Plus(crate::mac_plus::MacPlus),
    /// `quickstart`/`setup`, `cheatsheet` and shell completion: the easy way in.
    #[command(flatten)]
    Ux(crate::mac_ux::MacUx),
    #[command(flatten)]
    Ops(crate::mac_ops::MacOps),
    /// `schedule` (run a Mac image on a cron expression) and `notify`.
    #[command(flatten)]
    Sched(crate::mac_sched::MacSched),
    /// Registry login (`login`, `logout`, `logins`) and `fork`.
    #[command(flatten)]
    Dp(crate::mac_dp::MacDp),
    /// `create`, `kill`, `update`: the rest of the Docker container lifecycle.
    #[command(flatten)]
    Life(crate::mac_lifecycle::MacLife),
    /// `export`, `manifest inspect`: instance export and registry manifests.
    #[command(flatten)]
    Imgx(crate::mac_imgx::MacImgx),
    /// `checkpoint`, `checkpoints`, `diff`: named checkpoints of an instance, and `run --from-checkpoint`.
    #[command(flatten)]
    Ckpt(crate::mac_checkpoint::MacCkpt),
    /// `volume` and `network`: Docker resource parity.
    #[command(flatten)]
    Res(crate::mac_res::MacRes),
}

impl MacCommand {
    /// Image subcommands never contact the daemon; the instance ones do.
    pub fn is_client_side(&self) -> bool {
        match self {
            Self::Build { dry_run, .. } => *dry_run,
            // The fleet scheduler talks to the Macs over ssh, not to a daemon from here.
            // A flag the macOS guest cannot use is refused without a daemon.
            Self::Run { sched, opts, .. } => {
                sched.fleet.is_some() || opts.life.check_supported().is_err()
            }
            Self::Life(l) => l.is_client_side(),
            Self::Imgx(i) => i.is_client_side(),
            Self::Ckpt(_) => false,
            Self::Ps { opts, .. } => opts.fleet.is_some(),
            Self::More(m) => m.is_client_side(),
            Self::Plus(_) | Self::Ux(_) => true,
            Self::Ops(o) => o.is_client_side(),
            Self::Sched(s) => s.is_client_side(),
            Self::Dp(d) => d.is_client_side(),
            Self::Res(x) => x.is_client_side(),
            _ => !matches!(
                self,
                Self::Run { .. }
                    | Self::Ps { .. }
                    | Self::Rm { .. }
                    | Self::Exec { .. }
                    | Self::WaitReady { .. }
            ),
        }
    }
}

/// The label that marks a VM as a `fluxctl mac` instance, and the selector `ps` and `rm` send to the daemon.
pub const INSTANCE_LABEL: &str = "fluxvm.mac";
pub const INSTANCE_SELECTOR: &str = "fluxvm.mac=instance";
const IMAGE_LABEL: &str = "fluxvm.mac.image";
/// Apple's licence limit, as the daemon enforces it (`MAX_MACOS_GUESTS`).
const MAX_MACOS: usize = 2;

/// POST /v1/vms body: a macOS guest on `vz`, the image being the template's disk (the daemon clones it).
pub fn build_create_body(name: &str, disk: &Path, publish: &[String]) -> Result<Value> {
    ensure!(!name.is_empty(), "the instance name is empty");
    let forwards = publish
        .iter()
        .map(|p| crate::run::parse_port(p))
        .collect::<Result<Vec<_>>>()?;
    let mut body = json!({
        "name": name,
        "backend": "vz",
        "image": disk,
        "apple": {"guest_os": "macos"},
    });
    if !forwards.is_empty() {
        body["network"] = json!({"mode": "user", "forwards": forwards});
    }
    Ok(body)
}

/// PATCH body that labels a new VM as an instance of `image`.
pub fn instance_labels(image: &str) -> Value {
    json!({"labels": {INSTANCE_LABEL: "instance", IMAGE_LABEL: image}})
}

/// Keep only the instances among VM records (the daemon filters too; this guards a daemon that ignores `label`).
pub fn filter_instances(items: &Value) -> Vec<&Value> {
    items
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|v| v["labels"][INSTANCE_LABEL].as_str() == Some("instance"))
                .collect()
        })
        .unwrap_or_default()
}

fn forwards_text(vm: &Value) -> String {
    let f = vm["request"]["network"]["forwards"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(format!(
                        "{}->{}",
                        f["host_port"].as_u64()?,
                        f["guest_port"].as_u64()?
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if f.is_empty() {
        "-".into()
    } else {
        f.join(",")
    }
}

/// The `ps` table and the "N of 2" line.
pub fn render_ps(instances: &[&Value]) -> String {
    let running = instances
        .iter()
        .filter(|v| v["status"].as_str() == Some("running"))
        .count();
    let mut out = format!(
        "{:<20}  {:<24}  {:<9}  {:<15}  {:<14}  CREATED\n",
        "NAME", "IMAGE", "STATUS", "IP", "PORTS"
    );
    for v in instances {
        out.push_str(&format!(
            "{:<20}  {:<24}  {:<9}  {:<15}  {:<14}  {}\n",
            v["name"].as_str().unwrap_or("-"),
            v["labels"][IMAGE_LABEL].as_str().unwrap_or("-"),
            v["status"].as_str().unwrap_or("-"),
            v["guest_ip"].as_str().unwrap_or("-"),
            forwards_text(v),
            v["created_at"].as_str().unwrap_or("-"),
        ));
    }
    out.push_str(&format!(
        "{running} of {MAX_MACOS} macOS VMs running from this list (other macOS VMs on the host count toward the limit too)\n"
    ));
    out
}

/// Turn the daemon's `METHOD path: 409 Conflict: msg` / `503` into the message a user can act on; other errors pass through.
pub fn explain_error(e: anyhow::Error) -> anyhow::Error {
    let t = format!("{e:#}");
    if t.contains(": 409 ") {
        anyhow::anyhow!(
            "no room: this Mac already runs {MAX_MACOS} macOS VMs (Apple's licence limit). Stop one (`fluxctl mac rm NAME`, or `fluxctl stop ID`) and retry. Daemon said: {t}"
        )
    } else if t.contains(": 503 ") {
        anyhow::anyhow!(
            "this Mac's login session is locked, and a macOS guest cannot start from a locked session. Unlock it (a headless host needs an auto-login user with the lock off) and retry. Daemon said: {t}"
        )
    } else {
        e
    }
}

pub(crate) fn random_name() -> String {
    format!("mac-{}", &short_id()[..6])
}

/// The VM record of the instance called `name`, found through the label selector so only instances can be touched.
pub(crate) async fn find_instance(r: &crate::remote::Remote, name: &str) -> Result<Value> {
    let q = format!(
        "/v1/vms?label={}&name={}",
        crate::remote::encode_query(INSTANCE_SELECTOR),
        crate::remote::encode_query(name)
    );
    let v = r.call(reqwest::Method::GET, &q, None).await?;
    filter_instances(&v["items"])
        .into_iter()
        .next()
        .cloned()
        .with_context(|| format!("no instance named {name:?} (see `fluxctl mac ps`)"))
}

pub(crate) async fn delete_instance(r: &crate::remote::Remote, id: &str) -> Result<()> {
    use reqwest::Method;
    r.call(Method::POST, &format!("/v1/vms/{id}/stop"), None)
        .await
        .ok();
    r.call(Method::DELETE, &format!("/v1/vms/{id}"), None)
        .await?;
    Ok(())
}

/// Wait until the guest has an address and SSH answers there; returns the address.
pub(crate) async fn wait_for_ssh(r: &crate::remote::Remote, id: &str, secs: u64) -> Result<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let vm = r
            .call(reqwest::Method::GET, &format!("/v1/vms/{id}"), None)
            .await?;
        if let Some(ip) = vm["guest_ip"].as_str() {
            let up = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::net::TcpStream::connect((ip, 22)),
            )
            .await
            .is_ok_and(|c| c.is_ok());
            if up {
                return Ok(ip.to_string());
            }
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "the guest did not answer on SSH within {secs}s (a first boot of a clone can take minutes; raise --wait)"
        );
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

/// `run`, `ps`, `rm`, `exec`: they need the daemon.
pub async fn run_daemon(r: &crate::remote::Remote, command: MacCommand) -> Result<()> {
    match command {
        MacCommand::Run {
            rm,
            name,
            publish,
            user,
            wait,
            opts,
            queue,
            json,
            xrun,
            image,
            cmd,
            ..
        } => {
            if let Some(reference) = &xrun.from_checkpoint {
                crate::mac_checkpoint::check_run_flags(
                    &image,
                    name.as_deref(),
                    &publish,
                    rm,
                    false,
                )?;
                return crate::mac_checkpoint::run_from_checkpoint(
                    r, reference, user, wait, cmd, json,
                )
                .await;
            }
            xrun.check(rm)?;
            opts.life.check_supported()?;
            crate::mac_lifecycle::ensure_image(&image, opts.life.pull).await?;
            crate::mac_ops::preflight(&image, &xrun)?;
            let started = crate::mac_health::now_unix();
            let outcome = crate::mac_queue::run_queued(r, &queue, || {
                crate::mac_instance::run_outcome(
                    r,
                    rm,
                    name.clone(),
                    publish.clone(),
                    user.clone(),
                    wait,
                    image.clone(),
                    cmd.clone(),
                    opts.clone(),
                )
            })
            .await;
            let outcome = match outcome {
                Ok(o) => o,
                Err(e) => {
                    let t = xrun.notify_targets();
                    if t.any() {
                        let first = format!("{e:#}");
                        let ev = crate::mac_notify::Event {
                            what: name.clone().unwrap_or_else(|| image.clone()),
                            ok: false,
                            detail: format!(
                                "the run failed: {}. Run it again in a terminal to see why, or try `fluxctl mac doctor`",
                                first.lines().next().unwrap_or("see the terminal")
                            ),
                        };
                        crate::mac_notify::warn(
                            &crate::mac_notify::deliver(&crate::mac_notify::Real, &t, &ev).await,
                        );
                    }
                    return Err(e);
                }
            };
            crate::mac_ops::after_run(r, &outcome.name, started, &xrun).await?;
            // A command that ran to the end is the moment to notify; an instance that keeps running is watched until it stops.
            {
                let t = xrun.notify_targets();
                if t.any() {
                    if cmd.is_empty() && !rm {
                        notify_when_gone(r, &outcome.name, &t);
                    } else {
                        let ev = crate::mac_notify::Event {
                            what: outcome.name.clone(),
                            ok: true,
                            detail: format!("{} ran to the end.", outcome.name),
                        };
                        crate::mac_notify::warn(
                            &crate::mac_notify::deliver(&crate::mac_notify::Real, &t, &ev).await,
                        );
                    }
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&crate::mac_json::run_document(&outcome))?
                );
            } else if outcome.print_name {
                println!("{}", outcome.name);
            }
            Ok(())
        }
        MacCommand::WaitReady {
            name,
            timeout,
            user,
            check,
            json,
        } => crate::mac_ready::run(r, &name, timeout, &user, check, json).await,
        MacCommand::Ps { json, opts } => crate::mac_more::ps(r, json, opts).await,
        MacCommand::Rm { name } => {
            let vm = find_instance(r, &name).await?;
            delete_instance(r, vm["id"].as_str().context("no id")?).await?;
            println!("removed {name}");
            Ok(())
        }
        MacCommand::Exec {
            name,
            user,
            opts,
            cmd,
        } => crate::mac_more::exec(r, name, user, cmd, opts).await,
        MacCommand::More(m) => crate::mac_more::run_daemon(r, m).await,
        MacCommand::Dp(d) => crate::mac_dp::run_daemon(r, d).await,
        MacCommand::Life(l) => crate::mac_lifecycle::run_daemon(r, l).await,
        MacCommand::Imgx(i) => crate::mac_imgx::run_daemon(r, i).await,
        MacCommand::Ckpt(c) => crate::mac_checkpoint::run_daemon(r, c).await,
        MacCommand::Ops(o) => crate::mac_ops::run_daemon(r, o).await,
        MacCommand::Sched(s) => crate::mac_sched::run_daemon(r, s).await,
        MacCommand::Res(x) => crate::mac_res::run_daemon(r, x).await,
        MacCommand::Build {
            file,
            tag,
            context,
            wait,
            build_arg,
            no_cache,
            label,
            secret,
            ssh,
            platform,
            target,
            ..
        } => {
            let src = read_macfile(file, &context)?;
            let key = mac_store::normalize(&tag)?;
            let macfile = crate::mac_build::parse(&src)?;
            let build_args = parse_build_args(&build_arg)?;
            let (steps, mut config) = crate::mac_build::plan_with(&macfile, &build_args);
            crate::mac_build_exec::check_supported(&steps)?;
            let (secrets, ssh_forward) = check_build_flags(
                &macfile,
                &steps,
                &build_args,
                &BuildFlags {
                    target: target.as_deref(),
                    platform: platform.as_deref(),
                    label: &label,
                    secret: &secret,
                    ssh: &ssh,
                },
                &mut config,
            )?;
            let mut host = DaemonHost {
                r,
                store: Store::open(mac_store::default_root()?)?,
                key: key.clone(),
                context,
                wait,
                instance: None,
                secrets,
                ssh_forward,
            };
            let built = if no_cache {
                crate::mac_build_exec::execute(&mut host, &steps).await
            } else {
                run_cached_build(&mut host, &steps).await
            };
            // A last guard: no secret value may reach the terminal through an error message of ours.
            built.map_err(|e| anyhow::anyhow!("{}", host.secrets.redact(&format!("{e:#}"))))?;
            crate::mac_image_ops::write_build_config(&host.store, &key, config)?;
            println!("built {key}");
            Ok(())
        }
        _ => bail!("not an instance command"),
    }
}

fn read_macfile(file: Option<PathBuf>, context: &Path) -> Result<String> {
    let path = file.unwrap_or_else(|| context.join("Macfile"));
    std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

/// The real [`BuildHost`](crate::mac_build_exec::BuildHost): the daemon makes and stops the instance, `ssh`/`scp` run the steps,
/// the store keeps the result. Not verified against a real macOS guest.
struct DaemonHost<'a> {
    r: &'a crate::remote::Remote,
    store: Store,
    /// The normalised `name:tag` of the result.
    key: String,
    context: PathBuf,
    wait: u64,
    /// `(id, guest address once SSH answers)`.
    instance: Option<(String, Option<String>)>,
    /// `--secret` files for `RUN --mount=type=secret` steps.
    secrets: crate::mac_secrets::Secrets,
    /// `--ssh default` was given.
    ssh_forward: bool,
}

/// The guest end of a mounted step: `scp` a file in, `ssh` a command, with the agent forwarded when asked.
struct GuestIo {
    ip: String,
}

impl crate::mac_secrets::MountIo for GuestIo {
    async fn put(&mut self, user: &str, local: &Path, dst: &str) -> Result<()> {
        scp_to(&self.ip, user, local, dst).await
    }
    async fn run(&mut self, user: &str, command: &str, forward_agent: bool) -> Result<()> {
        crate::run::ssh_to_agent(
            &self.ip,
            user,
            22,
            true,
            forward_agent,
            &[command.to_string()],
        )
        .await
    }
}

/// What the Docker-style build flags asked for.
struct BuildFlags<'a> {
    target: Option<&'a str>,
    platform: Option<&'a str>,
    label: &'a [String],
    secret: &'a [String],
    ssh: &'a [String],
}

/// Check every build flag before anything is created, apply `--label` to `config`, and print the warnings that do not stop a
/// build. Returns the secrets and whether the SSH agent is forwarded.
fn check_build_flags(
    macfile: &crate::mac_build::Macfile,
    steps: &[crate::mac_build::Step],
    build_args: &std::collections::BTreeMap<String, String>,
    f: &BuildFlags<'_>,
    config: &mut crate::mac_config::ImageConfig,
) -> Result<(crate::mac_secrets::Secrets, bool)> {
    use crate::mac_errors::coded;
    if let Some(t) = f.target {
        return Err(coded(
            format!(
                "--target {t}: a Macfile has a single stage (one FROM), so there is no stage to stop at. Build the whole Macfile, or split it into two Macfiles and build the first one as its own image (`fluxctl mac build -t base:1 -f Macfile.base .`, then `FROM base:1`)"
            ),
            "MAC_BUILD_TARGET_UNSUPPORTED",
        ));
    }
    if let Some(p) = f.platform {
        crate::mac_build::check_platform(p).map_err(|m| coded(m, "MAC_PLATFORM_UNSUPPORTED"))?;
    }
    crate::mac_build::apply_labels(config, f.label)
        .map_err(|m| coded(m, "MAC_BUILD_FLAG_INVALID"))?;
    for w in crate::mac_build::check_build_args(macfile, build_args) {
        eprintln!("warning: {w}\n  more: fluxctl explain MAC_BUILD_ARG_UNUSED");
    }
    let secrets = crate::mac_secrets::Secrets::from_flags(f.secret, &|k| std::env::var(k).ok())?;
    let ssh = crate::mac_secrets::parse_ssh(f.ssh)?;
    let sock = std::env::var("SSH_AUTH_SOCK").ok();
    for w in crate::mac_secrets::preflight(steps, &secrets, ssh, sock.as_deref())? {
        eprintln!("warning: {w}");
    }
    Ok((secrets, ssh))
}

impl DaemonHost<'_> {
    fn ready(&self) -> Result<(&str, &str)> {
        match &self.instance {
            Some((id, Some(ip))) => Ok((id, ip)),
            _ => bail!("the build instance is not running"),
        }
    }
}

impl crate::mac_build_exec::BuildHost for DaemonHost<'_> {
    async fn create(&mut self, reference: &str) -> Result<()> {
        use reqwest::Method;
        let r = self.r;
        let disk = self.store.resolve(reference)?.join("disk.raw");
        let name = format!("build-{}", &short_id()[..6]);
        let body = build_create_body(&name, &disk, &[])?;
        let vm = r
            .call(Method::POST, "/v1/vms", Some(body))
            .await
            .map_err(explain_error)?;
        let id = vm["id"]
            .as_str()
            .context("the daemon returned no id")?
            .to_string();
        let started = async {
            r.call(
                Method::PATCH,
                &format!("/v1/vms/{id}"),
                Some(instance_labels(reference)),
            )
            .await?;
            r.call(Method::POST, &format!("/v1/vms/{id}/start"), None)
                .await
                .map_err(explain_error)
        }
        .await;
        if let Err(e) = started {
            delete_instance(r, &id).await.ok();
            return Err(e);
        }
        self.instance = Some((id, None));
        Ok(())
    }

    async fn wait_ready(&mut self) -> Result<()> {
        let Some((id, _)) = self.instance.clone() else {
            bail!("the build instance was not created");
        };
        let ip = wait_for_ssh(self.r, &id, self.wait).await?;
        self.instance = Some((id, Some(ip)));
        Ok(())
    }

    async fn run(&mut self, user: &str, command: &str) -> Result<()> {
        let (_, ip) = self.ready()?;
        crate::run::ssh_to(ip, user, 22, true, &[command.to_string()]).await
    }

    async fn run_mounted(
        &mut self,
        user: &str,
        command: &str,
        mounts: &[crate::mac_build::Mount],
    ) -> Result<()> {
        if mounts.is_empty() {
            return self.run(user, command).await;
        }
        let mut io = GuestIo {
            ip: self.ready()?.1.to_string(),
        };
        crate::mac_secrets::run_mounted(&mut io, &self.secrets, user, command, mounts).await
    }

    async fn copy(&mut self, user: &str, src: &str, dst: &str) -> Result<()> {
        let (_, ip) = self.ready()?;
        // `.macignore` in the context filters a directory source through a scratch copy (see mac_ignore).
        let staged = crate::mac_ignore::stage(&self.context, src)?;
        scp_to(ip, user, &staged.path, dst).await
    }

    async fn stop_and_store(&mut self) -> Result<()> {
        let key = self.key.clone();
        self.stop_and_store_as(&key).await
    }

    async fn delete(&mut self) -> Result<()> {
        match self.instance.take() {
            Some((id, _)) => delete_instance(self.r, &id).await,
            None => Ok(()),
        }
    }
}

impl DaemonHost<'_> {
    /// Stop the instance and store its files as the template `key`.
    async fn stop_and_store_as(&mut self, key: &str) -> Result<()> {
        use reqwest::Method;
        let id = self.ready()?.0.to_string();
        let r = self.r;
        r.call(Method::POST, &format!("/v1/vms/{id}/stop"), None)
            .await?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let vm = loop {
            let vm = r.call(Method::GET, &format!("/v1/vms/{id}"), None).await?;
            match vm["status"].as_str() {
                Some("stopped") => break vm,
                Some("failed") => bail!("the build instance failed while stopping"),
                _ => {}
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "the build instance did not stop within 300s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        };
        let disk = PathBuf::from(vm["disk"].as_str().context("the VM record has no disk")?);
        let ws = PathBuf::from(
            vm["workspace"]
                .as_str()
                .context("the VM record has no workspace")?,
        );
        store_instance_files(&self.store, key, &disk, &ws)
    }
}

impl crate::mac_cache::CachedHost for DaemonHost<'_> {
    fn cache_has(&self, cache_ref: &str) -> bool {
        self.store.resolve(cache_ref).is_ok()
    }

    fn cache_touch(&mut self, cache_ref: &str) {
        // Re-adding with a fresh time keeps what a build uses out of the way of `mac cache prune`.
        if let Ok(rows) = self.store.list()
            && let Some((_, mut e)) = rows.into_iter().find(|(n, _)| n == cache_ref)
        {
            e.added_unix = crate::mac_image_ops::now_unix();
            self.store.add(cache_ref, e).ok();
        }
    }

    async fn checkpoint(&mut self, cache_ref: &str, describe: &str, restart: bool) -> Result<()> {
        use crate::mac_build_exec::BuildHost;
        self.stop_and_store_as(cache_ref).await?;
        crate::mac_cache::note_step(&self.store, cache_ref, describe);
        if restart {
            self.delete().await?;
            self.create(cache_ref).await?;
            self.wait_ready().await?;
        }
        Ok(())
    }

    fn materialize(&mut self, cache_ref: &str) -> Result<()> {
        crate::mac_cache::materialize(&self.store, cache_ref, &self.key)
    }
}

/// Plan the cache keys for `steps` and run them through the cached executor; falls back to a plain build when the plan has
/// nothing to cache. Trims the cache to its budget afterwards.
async fn run_cached_build(
    host: &mut DaemonHost<'_>,
    steps: &[crate::mac_build::Step],
) -> Result<()> {
    let Some((base, work)) = crate::mac_cache::split(steps) else {
        return crate::mac_build_exec::execute(host, steps).await;
    };
    let base_id = crate::mac_cache::base_id(&host.store, base)?;
    let context = host.context.clone();
    let keys = crate::mac_cache::chain_keys(&base_id, work, &|src| {
        let staged = crate::mac_ignore::stage(&context, src)?;
        crate::mac_cache::hash_context(&staged.path)
    })?;
    let rep = crate::mac_cache::execute_cached(host, steps, &keys).await?;
    eprintln!("cache: {} steps reused, {} run", rep.cached, rep.ran);
    match crate::mac_cache::enforce_budget(&host.store, &keys) {
        Ok((0, _)) => {}
        Ok((n, freed)) => eprintln!(
            "cache: removed {n} old entries ({}) to stay within FLUXVM_MAC_CACHE_MAX",
            human(freed)
        ),
        Err(e) => eprintln!("warning: could not trim the build cache: {e:#}"),
    }
    Ok(())
}

/// Clone a stopped instance's disk, hardware model and auxiliary storage into the store as a template named `key`.
pub(crate) fn store_instance_files(
    store: &Store,
    key: &str,
    disk: &Path,
    workspace: &Path,
) -> Result<()> {
    let sources = [
        ("disk.raw", disk.to_path_buf()),
        ("hardware.bin", workspace.join("hardware.bin")),
        ("auxiliary.bin", workspace.join("auxiliary.bin")),
    ];
    for (_, p) in &sources {
        ensure!(p.is_file(), "the build instance has no {}", p.display());
    }
    let name = format!("local-{}", short_id());
    let scratch = store.scratch("build");
    std::fs::create_dir_all(&scratch)?;
    for (f, src) in &sources {
        if let Err(e) = clone_file(src, &scratch.join(f)) {
            std::fs::remove_dir_all(&scratch).ok();
            return Err(e);
        }
    }
    store.adopt(&scratch, &name)?;
    store.add(
        key,
        Entry {
            dir: name,
            digest: String::new(),
            source: "build".into(),
            added_unix: 0,
        },
    )?;
    Ok(())
}

/// `scp -r LOCAL USER@IP:DST` with the same host-key and identity handling as `mac exec`.
pub(crate) async fn scp_to(ip: &str, user: &str, local: &Path, dst: &str) -> Result<()> {
    ensure!(
        local.exists(),
        "COPY source {} does not exist",
        local.display()
    );
    let mut c = tokio::process::Command::new("scp");
    c.args([
        "-r",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]);
    if let Some(k) = crate::run::generated_identity() {
        c.arg("-i").arg(k);
    }
    let status = c
        .arg(local)
        .arg(format!("{user}@{ip}:{dst}"))
        .status()
        .await
        .context("running scp (is the OpenSSH client installed?)")?;
    ensure!(status.success(), "scp exited with {status}");
    Ok(())
}

pub async fn run(command: MacCommand) -> Result<()> {
    ensure!(
        command.is_client_side(),
        "`fluxctl mac run/ps/rm/exec` need the daemon: pass --server (or select a context)"
    );
    let command = match command {
        MacCommand::Host { command } => return crate::mac_host::run(command),
        // A refusal that needs no daemon (a flag with no meaning for a macOS guest) is shown without one.
        MacCommand::Run { opts, sched, .. } if sched.fleet.is_none() => {
            opts.life.check_supported()?;
            bail!("internal: mac run needs the daemon");
        }
        MacCommand::Life(l) => return l.run_client(),
        MacCommand::Imgx(i) => return crate::mac_imgx::run_client(i).await,
        c => c,
    };
    match command {
        MacCommand::Runner(a) => return tokio::task::block_in_place(|| crate::mac_runner::run(a)),
        MacCommand::More(crate::mac_more::MacMore::System {
            command: crate::mac_more::SystemCommand::Info { json },
        }) => return crate::mac_sysinfo::run_info(json, crate::remote::from_argv_env()).await,
        MacCommand::More(crate::mac_more::MacMore::System {
            command: crate::mac_more::SystemCommand::Version { json },
        }) => return crate::mac_sysinfo::run_version(json, crate::remote::from_argv_env()).await,
        MacCommand::Ps { json, opts } if opts.fleet.is_some() => {
            let inv = opts.fleet.clone().unwrap_or_default();
            return tokio::task::block_in_place(|| {
                crate::mac_fleet_agg::ps_fleet(&inv, json, &opts)
            });
        }
        MacCommand::Images { json, extra } if extra.fleet.is_some() => {
            let inv = extra.fleet.clone().unwrap_or_default();
            return tokio::task::block_in_place(|| {
                crate::mac_fleet_agg::images_fleet(&inv, json, &extra)
            });
        }
        MacCommand::Run {
            publish,
            name,
            user,
            wait,
            opts,
            sched,
            queue,
            xrun,
            image,
            cmd,
            ..
        } => {
            if xrun.from_checkpoint.is_some() {
                crate::mac_checkpoint::check_run_flags(&image, None, &[], false, true)?;
            }
            return tokio::task::block_in_place(|| {
                crate::mac_fleet_run::mac_run_fleet(
                    publish, name, user, wait, opts, sched, &queue, image, cmd,
                )
            });
        }
        _ => {}
    }
    let command = match command {
        MacCommand::Ops(o) => return crate::mac_ops::run_client(o),
        MacCommand::Sched(s) => return run_sched_client(s).await,
        MacCommand::Dp(d) => return crate::mac_dp::run_client(d).await,
        MacCommand::Res(x) => return crate::mac_res::run_client(x),
        c => c,
    };
    let store = Store::open(mac_store::default_root()?)?;
    match command {
        MacCommand::Images { json, extra } => crate::mac_plus::images(&store, json, &extra)?,
        MacCommand::Pull { reference, extra } => {
            crate::mac_plus::pull(&store, &reference, &extra).await?
        }
        MacCommand::Push {
            reference,
            remote,
            extra,
        } => {
            let target = remote_reference(&reference, remote.as_deref())?;
            crate::mac_plus::push(&store, &reference, &target, &extra).await?;
        }
        MacCommand::Tag { source, target } => println!("tagged {}", store.tag(&source, &target)?),
        MacCommand::Rmi { reference } => {
            let deleted = store.remove(&reference)?;
            println!(
                "untagged {reference}{}",
                if deleted {
                    "; files deleted"
                } else {
                    "; files kept (another name uses them)"
                }
            );
        }
        MacCommand::Build {
            file,
            tag,
            dry_run,
            context,
            build_arg,
            label,
            secret,
            ssh,
            platform,
            target,
            ..
        } => {
            ensure!(
                dry_run,
                "a real build needs the daemon: pass --server (or select a context)"
            );
            let src = read_macfile(file, &context)?;
            let build_args = parse_build_args(&build_arg)?;
            let macfile = crate::mac_build::parse(&src)?;
            let (steps, mut config) = crate::mac_build::plan_with(&macfile, &build_args);
            check_build_flags(
                &macfile,
                &steps,
                &build_args,
                &BuildFlags {
                    target: target.as_deref(),
                    platform: platform.as_deref(),
                    label: &label,
                    secret: &secret,
                    ssh: &ssh,
                },
                &mut config,
            )?;
            print!("{}", build_plan_text_with(&src, &tag, &build_args)?);
            if !config.labels.is_empty() {
                println!(
                    "labels: {}",
                    config
                        .labels
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        MacCommand::More(m) => return crate::mac_more::run_client(m),
        MacCommand::Plus(p) => return crate::mac_plus::run(&store, p).await,
        MacCommand::Ux(u) => return crate::mac_ux::run(u).await,
        MacCommand::Import { dir, reference } if dir.is_file() => {
            // A `mac save` / `mac export` tar (checked against its .sha256 when present).
            let key = crate::mac_image_ops::load(&store, &dir, Some(&reference))?;
            println!("imported {} as {key}", dir.display());
        }
        MacCommand::Import { dir, reference } => {
            check_template(&dir)?;
            let key = mac_store::normalize(&reference)?;
            let name = format!("local-{}", short_id());
            let scratch = store.scratch("import");
            std::fs::create_dir_all(&scratch)?;
            for f in REQUIRED_FILES {
                if let Err(e) = clone_file(&dir.join(f), &scratch.join(f)) {
                    std::fs::remove_dir_all(&scratch).ok();
                    return Err(e);
                }
            }
            store.adopt(&scratch, &name)?;
            store.add(
                &key,
                Entry {
                    dir: name,
                    digest: String::new(),
                    source: "local".into(),
                    added_unix: 0,
                },
            )?;
            println!("imported {} as {key}", dir.display());
        }
        MacCommand::Doctor { json } => {
            return crate::mac_doctor::run(json, crate::remote::from_argv_env()).await;
        }
        MacCommand::Run { .. }
        | MacCommand::Runner(_)
        | MacCommand::Ps { .. }
        | MacCommand::Rm { .. }
        | MacCommand::Exec { .. }
        | MacCommand::Host { .. }
        | MacCommand::WaitReady { .. }
        | MacCommand::Dp(_)
        | MacCommand::Life(_)
        | MacCommand::Imgx(_)
        | MacCommand::Ckpt(_)
        | MacCommand::Res(_)
        | MacCommand::Ops(_)
        | MacCommand::Sched(_) => unreachable!("rejected above"),
    }
    Ok(())
}

/// For an instance that keeps running: a detached `mac notify wait` fires when it stops or is removed.
fn notify_when_gone(r: &crate::remote::Remote, name: &str, t: &crate::mac_notify::Targets) {
    if let Err(e) = crate::mac_sched::spawn_notify_wait(r, name, t) {
        eprintln!(
            "warning: could not start the notifier ({e:#}); run `fluxctl mac notify wait {name}` yourself"
        );
    }
}

/// `schedule add|ls|rm|...` and `notify test`: no daemon.
async fn run_sched_client(s: crate::mac_sched::MacSched) -> Result<()> {
    match s {
        crate::mac_sched::MacSched::Notify { command } => {
            crate::mac_notify::run_client(command).await
        }
        s => crate::mac_sched::run_client(s),
    }
}

/// Parse a Macfile and render the plan for `tag`; errors carry the Macfile line.
#[cfg(test)]
fn build_plan_text(src: &str, tag: &str) -> Result<String> {
    build_plan_text_with(src, tag, &Default::default())
}

/// `NAME=value` pairs of `--build-arg`.
fn parse_build_args(args: &[String]) -> Result<std::collections::BTreeMap<String, String>> {
    args.iter()
        .map(|a| {
            a.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .with_context(|| format!("--build-arg {a:?} must be NAME=value"))
        })
        .collect()
}

fn build_plan_text_with(
    src: &str,
    tag: &str,
    args: &std::collections::BTreeMap<String, String>,
) -> Result<String> {
    let key = mac_store::normalize(tag)?;
    let file = crate::mac_build::parse(src)?;
    let steps = crate::mac_build::plan_with(&file, args).0;
    Ok(format!(
        "build {key}\n{}",
        crate::mac_build::render_dry_run(&steps)
    ))
}

/// Credentials for the registry that `reference` names: the environment, else the login saved by `mac login`.
pub(crate) fn creds(reference: &str) -> Option<image::oci_registry::Credentials> {
    crate::mac_login::credentials_for(reference)
}

pub(crate) fn env_creds() -> Option<image::oci_registry::Credentials> {
    let username = std::env::var("FLUXVM_REGISTRY_USER").ok()?;
    let password = std::env::var("FLUXVM_REGISTRY_PASSWORD").unwrap_or_default();
    Some(image::oci_registry::Credentials { username, password })
}

pub(crate) fn check_template(dir: &Path) -> Result<()> {
    for f in REQUIRED_FILES {
        ensure!(
            dir.join(f).is_file(),
            "{} is not a macOS template: {f} is missing (needs {})",
            dir.display(),
            REQUIRED_FILES.join(", ")
        );
    }
    Ok(())
}

/// APFS copy-on-write clone where available (`cp -c`), else a plain copy.
pub(crate) fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    let cloned = std::process::Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !cloned {
        std::fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

pub(crate) fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Whether the first path component names a registry host (`ghcr.io`, `localhost:5000`), as Docker decides it.
pub(crate) fn names_a_registry(reference: &str) -> bool {
    match reference.split_once('/') {
        Some((host, _)) => host.contains('.') || host.contains(':') || host == "localhost",
        None => false,
    }
}

pub(crate) fn remote_reference(local: &str, explicit: Option<&str>) -> Result<String> {
    match explicit {
        Some(r) => {
            ensure!(
                names_a_registry(r),
                "{r:?} does not name a registry (e.g. ghcr.io/acme/macos-xcode:15)"
            );
            mac_store::normalize(r)
        }
        None if names_a_registry(local) => mac_store::normalize(local),
        None => bail!(
            "{local:?} is a local name: pass a registry reference, e.g. `fluxctl mac push {local} ghcr.io/acme/{local}`"
        ),
    }
}

pub(crate) fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

pub(crate) fn human(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= G {
        format!("{:.1} GiB", b / G)
    } else {
        format!("{:.0} MiB", b / M)
    }
}

pub(crate) fn render_images(store: &Store, rows: &[(String, Entry)]) -> String {
    if rows.is_empty() {
        return "no macOS images (fluxctl mac pull REF, or fluxctl mac import DIR NAME)\n".into();
    }
    let w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(9).max(9);
    let mut out = format!(
        "{:<w$}  {:<14}  {:>9}  SOURCE\n",
        "REFERENCE", "DIGEST", "SIZE"
    );
    for (name, e) in rows {
        let digest = if e.digest.is_empty() {
            "-".to_string()
        } else {
            e.digest
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect()
        };
        out.push_str(&format!(
            "{name:<w$}  {digest:<14}  {:>9}  {}\n",
            human(dir_size(&store.root().join(&e.dir))),
            e.source
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_plan_text_shows_steps_and_reports_line_numbers() {
        let t = build_plan_text("FROM base:15\nUSER bob\nRUN make\n", "out").unwrap();
        assert!(
            t.starts_with("build out:latest\n") && t.contains("run as bob"),
            "{t}"
        );
        let e = build_plan_text("RUN x\n", "out").unwrap_err().to_string();
        assert!(e.contains("line 1"), "{e}");
        assert!(build_plan_text("FROM a\n", "bad tag").is_err());
    }

    #[test]
    fn registry_hosts_are_told_from_local_names() {
        assert!(names_a_registry("ghcr.io/acme/mac:15"));
        assert!(names_a_registry("localhost:5000/mac"));
        assert!(names_a_registry("localhost/mac"));
        assert!(!names_a_registry("macos-xcode:15"));
        assert!(!names_a_registry("acme/mac"));
    }

    #[test]
    fn push_targets() {
        assert_eq!(
            remote_reference("ghcr.io/a/m", None).unwrap(),
            "ghcr.io/a/m:latest"
        );
        assert_eq!(
            remote_reference("m:15", Some("ghcr.io/a/m:15")).unwrap(),
            "ghcr.io/a/m:15"
        );
        let e = remote_reference("m:15", None).unwrap_err().to_string();
        assert!(
            e.contains("local name") && e.contains("ghcr.io/acme/m:15"),
            "{e}"
        );
        assert!(remote_reference("m", Some("acme/m")).is_err());
    }

    #[test]
    fn a_template_missing_a_file_is_named() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("disk.raw"), b"x").unwrap();
        let e = check_template(t.path()).unwrap_err().to_string();
        assert!(e.contains("hardware.bin is missing"), "{e}");
    }

    #[test]
    fn import_images_and_rmi_round_trip_through_the_store() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(src.path().join(f), vec![7u8; 2048]).unwrap();
        }
        // SAFETY of env use: this is the only test in the crate that sets FLUXVM_MAC_STORE.
        unsafe { std::env::set_var("FLUXVM_MAC_STORE", root.path()) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(run(MacCommand::Import {
            dir: src.path().into(),
            reference: "base:15".into(),
        }))
        .unwrap();
        rt.block_on(run(MacCommand::Tag {
            source: "base:15".into(),
            target: "ci".into(),
        }))
        .unwrap();
        let store = Store::open(root.path()).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["base:15", "ci:latest"]
        );
        let text = render_images(&store, &rows);
        assert!(
            text.contains("REFERENCE") && text.contains("base:15") && text.contains("local"),
            "{text}"
        );
        rt.block_on(run(MacCommand::Rmi {
            reference: "base:15".into(),
        }))
        .unwrap();
        assert!(store.resolve("ci").is_ok(), "the other name still resolves");
        rt.block_on(run(MacCommand::Rmi {
            reference: "ci".into(),
        }))
        .unwrap();
        assert!(store.list().unwrap().is_empty());
        let e = rt
            .block_on(run(MacCommand::Import {
                dir: root.path().into(),
                reference: "x".into(),
            }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a macOS template"), "{e}");
    }

    #[test]
    fn create_body_is_a_macos_vz_request_the_core_accepts() {
        let b = build_create_body(
            "ci1",
            Path::new("/s/t/disk.raw"),
            &["2222:22".into(), "127.0.0.1:8080:80".into()],
        )
        .unwrap();
        assert_eq!(b["backend"], "vz");
        assert_eq!(b["apple"]["guest_os"], "macos");
        assert_eq!(b["image"], "/s/t/disk.raw");
        assert_eq!(b["network"]["forwards"][0]["host_port"], 2222);
        assert_eq!(b["network"]["forwards"][1]["host_addr"], "127.0.0.1");
        let req: fluxvm_core::model::CreateVmRequest = serde_json::from_value(b).unwrap();
        assert_eq!(req.name, "ci1");
        assert!(build_create_body("x", Path::new("/d"), &["nope".into()]).is_err());
        assert!(build_create_body("", Path::new("/d"), &[]).is_err());
        let plain = build_create_body("x", Path::new("/d"), &[]).unwrap();
        assert!(plain.get("network").is_none());
    }

    fn vm(name: &str, label: Option<&str>, status: &str) -> Value {
        let mut v = json!({"name": name, "status": status, "guest_ip": "192.168.64.5",
            "created_at": "2026-11-21T10:00:00Z", "labels": {},
            "request": {"network": {"forwards": [{"host_port": 2222, "guest_port": 22}]}}});
        if let Some(l) = label {
            v["labels"] = json!({"fluxvm.mac": l, "fluxvm.mac.image": "xcode:15"});
        }
        v
    }

    #[test]
    fn labels_and_filtering() {
        let l = instance_labels("xcode:15");
        assert_eq!(l["labels"]["fluxvm.mac"], "instance");
        assert_eq!(INSTANCE_SELECTOR, "fluxvm.mac=instance");
        let items = json!([
            vm("a", Some("instance"), "running"),
            vm("b", None, "running"),
            vm("c", Some("other"), "stopped")
        ]);
        let kept = filter_instances(&items);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["name"], "a");
        assert!(filter_instances(&json!(null)).is_empty());
    }

    #[test]
    fn ps_renders_rows_and_the_limit() {
        let items = json!([
            vm("a", Some("instance"), "running"),
            vm("b", Some("instance"), "stopped")
        ]);
        let text = render_ps(&filter_instances(&items));
        assert!(
            text.contains("NAME") && text.contains("xcode:15") && text.contains("2222->22"),
            "{text}"
        );
        assert!(text.contains("1 of 2 macOS VMs"), "{text}");
        assert!(render_ps(&[]).contains("0 of 2"));
    }

    #[test]
    fn errors_name_the_limit_and_the_locked_session() {
        let conflict = anyhow::anyhow!(
            "POST /v1/vms/x/start: 409 Conflict: this Mac already runs 2 macOS guests"
        );
        let m = explain_error(conflict).to_string();
        assert!(
            m.contains("already runs 2 macOS VMs") && m.contains("mac rm"),
            "{m}"
        );
        let locked =
            anyhow::anyhow!("POST /v1/vms/x/start: 503 Service Unavailable: session locked");
        assert!(
            explain_error(locked)
                .to_string()
                .contains("login session is locked")
        );
        let other =
            explain_error(anyhow::anyhow!("POST /v1/vms: 400 Bad Request: nope")).to_string();
        assert_eq!(other, "POST /v1/vms: 400 Bad Request: nope");
    }

    #[test]
    fn only_image_commands_are_client_side() {
        assert!(
            MacCommand::Rmi {
                reference: "x".into()
            }
            .is_client_side()
        );
        assert!(!MacCommand::Rm { name: "x".into() }.is_client_side());
        assert!(
            !MacCommand::Ps {
                json: false,
                opts: Default::default()
            }
            .is_client_side()
        );
    }

    #[test]
    fn doctor_is_client_side_and_wait_ready_needs_the_daemon() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            c: MacCommand,
        }
        let p = |a: &[&str]| {
            let mut v = vec!["t"];
            v.extend(a);
            T::try_parse_from(v).unwrap().c
        };
        assert!(p(&["doctor", "--json"]).is_client_side());
        let w = p(&[
            "wait-ready",
            "ci",
            "--timeout",
            "30",
            "--check",
            "true",
            "--json",
        ]);
        assert!(!w.is_client_side());
        let MacCommand::WaitReady {
            name,
            timeout,
            check,
            json,
            user,
        } = w
        else {
            panic!("not wait-ready")
        };
        assert_eq!(
            (name.as_str(), timeout, json, user.as_str()),
            ("ci", 30, true, "admin")
        );
        assert_eq!(check.as_deref(), Some("true"));
        let MacCommand::Run {
            queue, json, rm, ..
        } = p(&[
            "run",
            "--rm",
            "--queue",
            "--queue-timeout",
            "120",
            "--json",
            "img",
            "--",
            "true",
        ])
        else {
            panic!("not run")
        };
        assert!(queue.queue && json && rm);
        assert_eq!(queue.queue_timeout, 120);
        let MacCommand::Run { queue, json, .. } = p(&["run", "img"]) else {
            panic!("not run")
        };
        assert!(!queue.queue && !json);
    }
}
