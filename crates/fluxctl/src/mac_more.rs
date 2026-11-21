// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The rest of the `fluxctl mac` command set, kept out of `mac_cmd.rs`: instance lifecycle (`stop`, `start`, `restart`,
//! `pause`, `unpause`, `rename`, `wait`, `reconcile`, `prune`), inspection (`inspect`, `logs`, `stats`, `top`, `port`, `cp`),
//! image tools (`history`, `save`, `load`, `commit`, `image inspect|prune|...`), `system df|prune` and `compose`.
//!
//! Instance commands need the daemon (`--server` or a context) and talk to it through the REST routes `fluxctl mac run` already
//! uses (`/v1/vms/{id}/stop|start|restart|pause|resume|stats|logs`, `PATCH /v1/vms/{id}`). Image commands read the local store.
//!
//! None of this has been run against a real daemon or a real macOS VM; the pure parts are unit tested.

use crate::mac_config;
use crate::mac_image_ops as ops;
use crate::mac_inspect::{self as view, IMAGE_LABEL, STOPPED_LABEL};
use crate::mac_instance::explain;
use crate::mac_store::{self, Store};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(clap::Args, Debug, Default)]
pub struct PsOpts {
    /// Include stopped and failed instances (by default only running, paused and starting ones are shown).
    #[arg(short = 'a', long)]
    pub all: bool,
    /// Keep instances that match `KEY=VALUE`: name, id, status, image or label. Repeatable; all must match.
    #[arg(short = 'f', long = "filter", value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
    /// Print each instance with a template such as `{{.Name}} {{.Status}}`, or `table {{.Name}}\t{{.IP}}`.
    /// Fields: ID, Name, Image, Status, IP, Ports, Created, Restart, Project, Service, Labels.
    #[arg(long, value_name = "TEMPLATE")]
    pub format: Option<String>,
    /// Print only the short ids.
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

#[derive(clap::Args, Debug, Default)]
pub struct ExecOpts {
    /// Set an environment variable for CMD: `NAME=value`, or `NAME` to copy it from this shell. Repeatable.
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    pub env: Vec<String>,
    /// Directory to run CMD in.
    #[arg(short = 'w', long)]
    pub workdir: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Everything the store knows about an image: files, sizes, other names and the Macfile-derived config, as JSON.
    Inspect { reference: String },
    /// Remove store directories no image name uses (a crashed pull) and stale scratch directories. With `--all`, also remove
    /// images no instance uses (needs the daemon to know which are in use).
    Prune {
        #[arg(long)]
        all: bool,
        /// Show what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask.
        #[arg(short = 'f', long)]
        force: bool,
        /// Per repository, keep the N most recently used tags and untag the rest (store only; build-cache entries are left to
        /// `mac cache prune`). Last use is recorded by `mac run`; an image never run counts from when it was added. Instances keep
        /// their own disk clones, but `mac health` and `mac exec` read the image's config, which goes with the image.
        #[arg(long, value_name = "N", conflicts_with = "all")]
        keep_last: Option<usize>,
    },
    /// How an image was built (same as `mac history`).
    History { reference: String },
    /// Every file of the image bundle with size and sha256, as JSON (same as `mac sbom`).
    Sbom {
        reference: String,
        #[arg(long = "to", value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Write an image to a tar file (same as `mac save`).
    Save {
        reference: String,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Read an image from a tar file (same as `mac load`).
    Load {
        #[arg(short = 'i', long)]
        input: PathBuf,
        reference: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SystemCommand {
    /// Disk used by images and instances, and what could be reclaimed. Needs the daemon for the instances.
    Df {
        #[arg(long)]
        json: bool,
    },
    /// Remove stopped instances and unreferenced store directories. With `--all`, also images no instance uses.
    Prune {
        #[arg(long)]
        all: bool,
        #[arg(short = 'f', long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MacMore {
    /// Everything about an instance as JSON: state, ports, resources, mounts, labels, restart policy and the image's config.
    #[command(after_help = "Example:\n  fluxctl mac inspect build-1 | jq '.[0].network.ip'")]
    Inspect {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Show an instance's console log (the VM's serial output). `-f` follows it until Ctrl-C.
    #[command(
        after_help = "Examples:\n  fluxctl mac logs build-1 --tail 50\n  fluxctl mac logs -f build-1"
    )]
    Logs {
        name: String,
        #[arg(short = 'f', long)]
        follow: bool,
        /// Lines from the end to show first.
        #[arg(long, default_value_t = 100)]
        tail: usize,
    },
    /// Copy between this Mac and an instance, either way: `NAME:/path` is the guest side. Uses scp -r over SSH.
    #[command(
        after_help = "Examples:\n  fluxctl mac cp ./site web:/Users/admin/site\n  fluxctl mac cp web:/var/log/system.log ./system.log"
    )]
    Cp {
        src: String,
        dst: String,
        /// SSH user in the guest.
        #[arg(long, default_value = "admin")]
        user: String,
    },
    /// CPU, memory and disk I/O of instances (all running ones when no name is given). One reading, not a live stream; CPU is the
    /// average over the VM's whole life.
    Stats {
        names: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Stop instances (the disk is kept; `mac start` boots them again).
    Stop {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Start stopped instances. Needs a free slot: a Mac runs at most 2 macOS VMs.
    Start {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Stop and start instances again.
    Restart {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Freeze a running instance's vCPUs (the VM keeps its memory and its macOS slot).
    Pause {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Resume a paused instance.
    Unpause {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Change an instance's name.
    Rename { old: String, new: String },
    /// Block until an instance stops. Prints its final state (`stopped` or `failed`); a macOS VM has no exit code, and a
    /// `failed` state is an error exit.
    Wait {
        name: String,
        /// Give up after this many seconds (0 = never).
        #[arg(long, default_value_t = 0)]
        timeout: u64,
    },
    /// The processes in an instance (`ps` over SSH). Extra words after `--` go to `ps`.
    #[command(after_help = "Example:\n  fluxctl mac top build-1 -- aux")]
    Top {
        name: String,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(last = true)]
        ps_args: Vec<String>,
    },
    /// Show the ports an instance publishes (`-p` of `mac run`), optionally for one guest port.
    Port { name: String, port: Option<u16> },
    /// Delete stopped and failed instances. With `--orphaned`, also instances (not running) whose image is no longer in the
    /// store.
    Prune {
        #[arg(long)]
        orphaned: bool,
        /// Do not ask.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Apply the restart policies (`mac run --restart`): start stopped instances that are `always`, `unless-stopped` (and not
    /// stopped by `mac stop`) or `on-failure` (and failed). Nothing in the daemon does this by itself: run it from launchd or cron.
    Reconcile {
        #[arg(long)]
        dry_run: bool,
    },
    /// Make a new image from a stopped instance's disk (the daemon must run on this Mac): `mac commit NAME NEWIMAGE:TAG`.
    Commit {
        instance: String,
        reference: String,
        /// A note recorded in the image's history.
        #[arg(short = 'm', long, default_value = "")]
        message: String,
        #[arg(short = 'a', long, default_value = "")]
        author: String,
    },
    /// How an image was built, newest step first.
    History { reference: String },
    /// Write an image to a tar file (`--output FILE`). A sparse disk is archived at its full size: expect tens of GiB.
    Save {
        reference: String,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Read an image written by `mac save`. REFERENCE overrides the name inside the archive.
    Load {
        #[arg(short = 'i', long)]
        input: PathBuf,
        reference: Option<String>,
    },
    /// Image tools: inspect, prune, history, save, load.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Disk usage and cleanup across images and instances.
    System {
        #[command(subcommand)]
        command: SystemCommand,
    },
    /// Start several macOS instances from a small YAML or JSON file, in depends_on order. At most 2 (Apple's licence limit).
    Compose {
        #[command(subcommand)]
        command: crate::mac_compose::ComposeCommand,
    },
    /// Recorded lifecycle events of instances and `kube apple` nodes (`mac.*`, `kube.apple.*`) from the daemon's event log.
    /// `mac watch --record-to DIR` produces them. `-f` keeps printing new ones.
    Events {
        /// Newest N events.
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(short = 'f', long)]
        follow: bool,
        /// One JSON object per line (the event log's own shape).
        #[arg(long)]
        json: bool,
    },
    /// Watch instances (and, with `--kube`, `kube apple` nodes) and print one event per change: created, started, paused,
    /// resumed, stopped, failed, removed, `mac.slots.full|free`, node and cluster changes. The first poll is the baseline.
    /// `--record-to DIR` also appends them to DIR/events.jsonl, the daemon's state directory, so `fluxctl events`, the SSE
    /// stream and `[[event_sinks]]` webhooks (`events = ["mac.", "kube.apple."]`) carry them. See docs/mac-integrations.md.
    Watch {
        /// Seconds between polls.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Stop after N polls (0 = until interrupted).
        #[arg(long, default_value_t = 0)]
        polls: u32,
        /// Also watch the local `kube apple` clusters.
        #[arg(long)]
        kube: bool,
        /// This Mac's name in the fleet, put on every event as `node`.
        #[arg(long)]
        node: Option<String>,
        /// Append events to DIR/events.jsonl (the daemon's state directory; needs write access to it).
        #[arg(long, value_name = "DIR")]
        record_to: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Prometheus text: instances by state, macOS slots used (of 2), template store size and, with `--kube`, `kube apple`
    /// node readiness and snapshot age. `--textfile PATH` writes it atomically for node_exporter's textfile collector.
    Metrics {
        /// Value of the `node` label (this Mac's name in the fleet).
        #[arg(long)]
        node: Option<String>,
        /// Also read the `kube apple` clusters (connects to each node over ssh).
        #[arg(long)]
        kube: bool,
        #[arg(long, value_name = "PATH")]
        textfile: Option<PathBuf>,
    },
}

impl MacMore {
    /// Image commands read the local store; instance commands need the daemon.
    pub fn is_client_side(&self) -> bool {
        match self {
            MacMore::History { .. } | MacMore::Save { .. } | MacMore::Load { .. } => true,
            MacMore::Image { command } => !matches!(command, ImageCommand::Prune { all: true, .. }),
            MacMore::Compose { command } => command.is_client_side(),
            _ => false,
        }
    }
}

fn confirm(what: &str, force: bool) -> Result<bool> {
    if force {
        return Ok(true);
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "{what}: pass -f to go ahead without asking (stdin is not a terminal)"
    );
    eprint!("{what}. Continue? [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn id_of(vm: &Value) -> Result<&str> {
    vm["id"].as_str().context("the daemon record has no id")
}

fn name_of(vm: &Value) -> &str {
    vm["name"].as_str().unwrap_or("?")
}

async fn set_label(
    r: &crate::remote::Remote,
    id: &str,
    key: &str,
    val: Option<&str>,
) -> Result<()> {
    r.call(
        reqwest::Method::PATCH,
        &format!("/v1/vms/{id}"),
        Some(json!({"labels": {key: val}})),
    )
    .await
    .map(|_| ())
}

async fn instances(r: &crate::remote::Remote) -> Result<Vec<Value>> {
    let v = r
        .call(
            reqwest::Method::GET,
            &format!(
                "/v1/vms?label={}",
                crate::remote::encode_query(crate::mac_cmd::INSTANCE_SELECTOR)
            ),
            None,
        )
        .await?;
    Ok(crate::mac_cmd::filter_instances(&v["items"])
        .into_iter()
        .cloned()
        .collect())
}

/// Whether `reconcile` starts this instance.
pub fn should_restart(vm: &Value) -> bool {
    let policy = vm["labels"][view::RESTART_LABEL].as_str().unwrap_or("no");
    let st = view::status(vm);
    let manual = vm["labels"][STOPPED_LABEL].as_str() == Some("true");
    match policy {
        "always" => matches!(st, "stopped" | "failed"),
        "unless-stopped" => matches!(st, "stopped" | "failed") && !manual,
        "on-failure" => st == "failed",
        _ => false,
    }
}

/// `exec` text: the image's ENV and WORKDIR (when the instance's image is still in the store), then `-e` and `-w`.
pub fn exec_shell(
    cfg: &mac_config::ImageConfig,
    cmd: &[String],
    env: &[(String, String)],
    workdir: Option<&str>,
) -> String {
    let mut all = cfg.env.clone();
    all.extend(env.iter().cloned());
    format!(
        "{}{}",
        mac_config::prefix(&all, workdir.or(cfg.workdir.as_deref())),
        mac_config::argv_to_shell(cmd)
    )
}

fn image_config_of(vm: &Value) -> Option<mac_config::ImageConfig> {
    let image = vm["labels"][IMAGE_LABEL].as_str()?;
    let store = Store::open(mac_store::default_root().ok()?).ok()?;
    mac_config::load(&store.resolve(image).ok()?).ok()
}

/// `mac ps`.
pub async fn ps(r: &crate::remote::Remote, json_out: bool, o: PsOpts) -> Result<()> {
    let filters = view::parse_filters(&o.filter)?;
    let items = instances(r).await?;
    let all: Vec<&Value> = items.iter().collect();
    let rows = view::select(&all, o.all, &filters);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&crate::mac_json::ps_document(&rows))?
        );
    } else if o.quiet {
        print!("{}", view::render_quiet(&rows));
    } else if let Some(f) = &o.format {
        print!("{}", view::render_format(&rows, f)?);
    } else {
        print!("{}", crate::mac_cmd::render_ps(&rows));
    }
    Ok(())
}

/// `mac exec` with `-e` / `-w`.
pub async fn exec(
    r: &crate::remote::Remote,
    name: String,
    user: String,
    cmd: Vec<String>,
    o: ExecOpts,
) -> Result<()> {
    let vm = crate::mac_cmd::find_instance(r, &name).await?;
    ensure!(
        view::status(&vm) == "running",
        "{name} is {} (not running; `fluxctl mac start {name}`)",
        view::status(&vm)
    );
    let ip = vm["guest_ip"]
        .as_str()
        .context("the instance has no address yet: wait for it to boot")?;
    let env = crate::mac_instance::collect_env(&o.env, &[], |k| std::env::var(k).ok())?;
    let cfg = image_config_of(&vm).unwrap_or_default();
    crate::run::ssh_to(
        ip,
        &user,
        22,
        true,
        &[exec_shell(&cfg, &cmd, &env, o.workdir.as_deref())],
    )
    .await
}

async fn lifecycle(
    r: &crate::remote::Remote,
    names: &[String],
    verb: &str,
    route: &str,
    after: impl AsyncFn(&crate::remote::Remote, &str, &Value) -> Result<()>,
) -> Result<()> {
    let mut failures = Vec::new();
    for n in names {
        let res = async {
            let vm = crate::mac_cmd::find_instance(r, n).await?;
            let id = id_of(&vm)?;
            r.call(
                reqwest::Method::POST,
                &format!("/v1/vms/{id}/{route}"),
                None,
            )
            .await
            .map_err(|e| explain(e, &format!("{verb} {n}")))?;
            after(r, id, &vm).await
        }
        .await;
        match res {
            Ok(()) => println!("{n}"),
            Err(e) => {
                eprintln!("{n}: {e:#}");
                failures.push(n.as_str());
            }
        }
    }
    ensure!(
        failures.is_empty(),
        "{verb} failed for: {}",
        failures.join(", ")
    );
    Ok(())
}

async fn follow_logs(r: &crate::remote::Remote, path: &str) -> Result<()> {
    use std::io::Write;
    let url = format!("{}{path}", r.base());
    let mut req = reqwest::Client::new().get(&url);
    if let Some(t) = r.bearer() {
        req = req.bearer_auth(t);
    }
    let mut resp = req.send().await.with_context(|| format!("GET {url}"))?;
    ensure!(resp.status().is_success(), "GET {path}: {}", resp.status());
    let mut out = std::io::stdout();
    while let Some(chunk) = resp.chunk().await? {
        out.write_all(&chunk)?;
        out.flush()?;
    }
    Ok(())
}

async fn wait_stopped(
    r: &crate::remote::Remote,
    id: &str,
    name: &str,
    timeout: u64,
) -> Result<String> {
    let deadline =
        (timeout > 0).then(|| std::time::Instant::now() + std::time::Duration::from_secs(timeout));
    loop {
        let vm = r
            .call(reqwest::Method::GET, &format!("/v1/vms/{id}"), None)
            .await?;
        let st = view::status(&vm).to_string();
        if matches!(st.as_str(), "stopped" | "failed") {
            return Ok(st);
        }
        if let Some(d) = deadline {
            ensure!(
                std::time::Instant::now() < d,
                "{name} was still {st} after {timeout}s"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

fn open_store() -> Result<Store> {
    Store::open(mac_store::default_root()?)
}

/// History, save, load and the image group without the daemon.
pub fn run_client(cmd: MacMore) -> Result<()> {
    match cmd {
        MacMore::History { reference }
        | MacMore::Image {
            command: ImageCommand::History { reference },
        } => {
            print!(
                "{}",
                ops::render_history(&ops::history(&open_store()?, &reference)?)
            );
        }
        MacMore::Save { reference, output }
        | MacMore::Image {
            command: ImageCommand::Save { reference, output },
        } => {
            ops::save(&open_store()?, &reference, &output)?;
            println!(
                "saved {} to {}",
                mac_store::normalize(&reference)?,
                output.display()
            );
        }
        MacMore::Load { input, reference }
        | MacMore::Image {
            command: ImageCommand::Load { input, reference },
        } => {
            let key = ops::load(&open_store()?, &input, reference.as_deref())?;
            println!("loaded {key}");
        }
        MacMore::Image {
            command: ImageCommand::Inspect { reference },
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ops::image_inspect(&open_store()?, &reference)?)?
            );
        }
        MacMore::Image {
            command:
                ImageCommand::Prune {
                    keep_last: Some(n),
                    dry_run,
                    force,
                    ..
                },
        } => {
            let store = open_store()?;
            let unused = crate::mac_imgstat::beyond_keep_last(&store, n)?;
            prune_store(&store, &unused, dry_run, force)?;
        }
        MacMore::Image {
            command:
                ImageCommand::Prune {
                    all: false,
                    dry_run,
                    force,
                    ..
                },
        } => {
            prune_store(&open_store()?, &[], dry_run, force)?;
        }
        MacMore::Image {
            command: ImageCommand::Sbom { reference, output },
        } => {
            let text =
                serde_json::to_string_pretty(&crate::mac_plus::sbom(&open_store()?, &reference)?)?;
            match output {
                Some(p) => std::fs::write(&p, text)
                    .with_context(|| format!("writing {}", p.display()))?,
                None => println!("{text}"),
            }
        }
        MacMore::Compose { command } => crate::mac_compose::run_client(command)?,
        _ => bail!("this command needs the daemon: pass --server (or select a context)"),
    }
    Ok(())
}

/// Remove orphan directories, and `unused` image names; with `dry_run` only list them.
fn prune_store(store: &Store, unused: &[String], dry_run: bool, force: bool) -> Result<()> {
    let orph = ops::orphans(store, 3600)?;
    if orph.is_empty() && unused.is_empty() {
        println!("nothing to remove");
        return Ok(());
    }
    for d in &orph {
        println!("unreferenced directory {}", d.display());
    }
    for u in unused {
        println!("unused image {u}");
    }
    if dry_run {
        return Ok(());
    }
    if !confirm(
        &format!(
            "This removes {} directories and {} images",
            orph.len(),
            unused.len()
        ),
        force,
    )? {
        println!("cancelled");
        return Ok(());
    }
    let (n, bytes) = ops::remove_orphans(store, &orph)?;
    for u in unused {
        store.remove(u)?;
    }
    println!(
        "removed {n} directories and {} images, freed about {}",
        unused.len(),
        view::human_bytes(bytes)
    );
    Ok(())
}

/// Everything that needs the daemon.
pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacMore) -> Result<()> {
    use reqwest::Method;
    match cmd {
        MacMore::Events {
            limit,
            follow,
            json,
        } => crate::mac_events::run_events(r, limit, follow, json).await?,
        MacMore::Watch {
            interval,
            polls,
            kube,
            node,
            record_to,
            json,
        } => {
            let o = crate::mac_events::WatchOpts {
                interval: std::time::Duration::from_secs(interval.max(1)),
                polls: (polls > 0).then_some(polls),
                node,
                record_to,
            };
            crate::mac_events::run_watch(r, o, json, kube).await?
        }
        MacMore::Metrics {
            node,
            kube,
            textfile,
        } => crate::mac_metrics::run(r, node, kube, textfile.as_deref()).await?,
        MacMore::Inspect { names } => {
            let mut out = Vec::new();
            for n in &names {
                let vm = crate::mac_cmd::find_instance(r, n).await?;
                let full = r
                    .call(Method::GET, &format!("/v1/vms/{}", id_of(&vm)?), None)
                    .await
                    .map_err(|e| explain(e, &format!("inspect {n}")))?;
                let cfg = image_config_of(&full)
                    .map(serde_json::to_value)
                    .transpose()?;
                out.push(crate::mac_json::inspect_v1(view::inspect(&full, cfg)));
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        MacMore::Logs { name, follow, tail } => {
            let vm = crate::mac_cmd::find_instance(r, &name).await?;
            let path = format!("/v1/vms/{}/logs?lines={tail}&follow={follow}", id_of(&vm)?);
            if follow {
                follow_logs(r, &path).await?;
            } else {
                let (st, body) = r.get_raw(&path).await?;
                ensure!(
                    st.is_success(),
                    "GET {path}: {st}: {}",
                    String::from_utf8_lossy(&body)
                );
                print!("{}", String::from_utf8_lossy(&body));
            }
        }
        MacMore::Cp { src, dst, user } => {
            let t = crate::mac_cp::plan(&src, &dst)?;
            let vm = crate::mac_cmd::find_instance(r, &t.name).await?;
            ensure!(
                view::status(&vm) == "running",
                "{} is {} (not running)",
                t.name,
                view::status(&vm)
            );
            let ip = vm["guest_ip"]
                .as_str()
                .context("the instance has no address yet")?;
            crate::mac_cp::copy(&t, &user, ip).await?;
        }
        MacMore::Stats { names, json } => {
            let vms = if names.is_empty() {
                instances(r)
                    .await?
                    .into_iter()
                    .filter(|v| view::status(v) == "running")
                    .collect()
            } else {
                let mut v = Vec::new();
                for n in &names {
                    v.push(crate::mac_cmd::find_instance(r, n).await?);
                }
                v
            };
            let mut table = view::stats_header();
            let mut raw = Vec::new();
            for vm in &vms {
                let m = r
                    .call(Method::GET, &format!("/v1/vms/{}/stats", id_of(vm)?), None)
                    .await
                    .map_err(|e| explain(e, &format!("read stats of {}", name_of(vm))))?;
                table.push_str(&view::stats_row(name_of(vm), &m));
                raw.push(json!({"name": name_of(vm), "stats": m}));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&raw)?);
            } else if vms.is_empty() {
                println!("no running instances");
            } else {
                print!("{table}");
            }
        }
        MacMore::Stop { names } => {
            lifecycle(r, &names, "stop", "stop", async |r, id, _| {
                set_label(r, id, STOPPED_LABEL, Some("true")).await
            })
            .await?
        }
        MacMore::Start { names } => {
            lifecycle(r, &names, "start", "start", async |r, id, vm| {
                if vm["labels"][STOPPED_LABEL].is_string() {
                    set_label(r, id, STOPPED_LABEL, None).await?;
                }
                Ok(())
            })
            .await?
        }
        MacMore::Restart { names } => {
            lifecycle(r, &names, "restart", "restart", async |r, id, vm| {
                if vm["labels"][STOPPED_LABEL].is_string() {
                    set_label(r, id, STOPPED_LABEL, None).await?;
                }
                Ok(())
            })
            .await?
        }
        MacMore::Pause { names } => {
            lifecycle(r, &names, "pause", "pause", async |_, _, _| Ok(())).await?
        }
        MacMore::Unpause { names } => {
            lifecycle(r, &names, "unpause", "resume", async |_, _, _| Ok(())).await?
        }
        MacMore::Rename { old, new } => {
            ensure!(
                !new.trim().is_empty() && !new.contains(char::is_whitespace),
                "{new:?} is not a valid name"
            );
            let vm = crate::mac_cmd::find_instance(r, &old).await?;
            ensure!(
                crate::mac_cmd::find_instance(r, &new).await.is_err(),
                "an instance named {new:?} already exists"
            );
            r.call(
                Method::PATCH,
                &format!("/v1/vms/{}", id_of(&vm)?),
                Some(json!({"name": new})),
            )
            .await
            .map_err(|e| explain(e, &format!("rename {old}")))?;
            println!("{old} -> {new}");
        }
        MacMore::Wait { name, timeout } => {
            let vm = crate::mac_cmd::find_instance(r, &name).await?;
            let st = wait_stopped(r, id_of(&vm)?, &name, timeout).await?;
            println!("{st}");
            ensure!(st != "failed", "{name} failed");
        }
        MacMore::Top {
            name,
            user,
            ps_args,
        } => {
            let vm = crate::mac_cmd::find_instance(r, &name).await?;
            ensure!(
                view::status(&vm) == "running",
                "{name} is {} (not running)",
                view::status(&vm)
            );
            let ip = vm["guest_ip"]
                .as_str()
                .context("the instance has no address yet")?;
            crate::run::ssh_to(ip, &user, 22, true, &[view::top_command(&ps_args)]).await?;
        }
        MacMore::Port { name, port } => {
            let vm = crate::mac_cmd::find_instance(r, &name).await?;
            print!("{}", view::port_lines(&vm, port)?);
        }
        MacMore::Prune { orphaned, force } => {
            let items = instances(r).await?;
            let all: Vec<&Value> = items.iter().collect();
            let names: BTreeSet<String> =
                open_store()?.list()?.into_iter().map(|(k, _)| k).collect();
            let victims = ops::prunable_instances(&all, &names, orphaned);
            if victims.is_empty() {
                println!("no instances to remove");
                return Ok(());
            }
            for v in &victims {
                println!("{} ({})", name_of(v), view::status(v));
            }
            if !confirm(
                &format!("This deletes {} instances and their disks", victims.len()),
                force,
            )? {
                println!("cancelled");
                return Ok(());
            }
            for v in victims {
                crate::mac_cmd::delete_instance(r, id_of(v)?).await?;
                println!("removed {}", name_of(v));
            }
        }
        MacMore::Reconcile { dry_run } => {
            let items = instances(r).await?;
            let todo: Vec<&Value> = items.iter().filter(|v| should_restart(v)).collect();
            if todo.is_empty() {
                println!("nothing to start");
            }
            let mut failed = Vec::new();
            for v in todo {
                let n = name_of(v);
                if dry_run {
                    println!(
                        "would start {n} ({}, restart={})",
                        view::status(v),
                        v["labels"][view::RESTART_LABEL].as_str().unwrap_or("no")
                    );
                    continue;
                }
                match r
                    .call(Method::POST, &format!("/v1/vms/{}/start", id_of(v)?), None)
                    .await
                {
                    Ok(_) => println!("started {n}"),
                    Err(e) => {
                        eprintln!("{n}: {:#}", explain(e, &format!("start {n}")));
                        failed.push(n);
                    }
                }
            }
            ensure!(failed.is_empty(), "could not start: {}", failed.join(", "));
        }
        MacMore::Commit {
            instance,
            reference,
            message,
            author,
        } => {
            let vm = crate::mac_cmd::find_instance(r, &instance).await?;
            ensure!(
                matches!(view::status(&vm), "stopped" | "failed"),
                "{instance} is {}: stop it first (`fluxctl mac stop {instance}`), a running disk is not consistent",
                view::status(&vm)
            );
            let key = mac_store::normalize(&reference)?;
            let disk = PathBuf::from(vm["disk"].as_str().context("the VM record has no disk")?);
            let ws = PathBuf::from(
                vm["workspace"]
                    .as_str()
                    .context("the VM record has no workspace")?,
            );
            ensure!(
                disk.is_file(),
                "{} is not readable here: `mac commit` needs the daemon on this Mac",
                disk.display()
            );
            let store = open_store()?;
            crate::mac_cmd::store_instance_files(&store, &key, &disk, &ws)?;
            let base = image_config_of(&vm);
            let mut cfg = ops::commit_config(base, &instance, &message, &author, 0);
            if let Some(h) = cfg.history.last_mut() {
                h.created_unix = ops::now_unix();
            }
            mac_config::save(&store.resolve(&key)?, &cfg)?;
            println!("committed {instance} as {key}");
        }
        MacMore::System {
            command: SystemCommand::Df { json },
        } => {
            let items = instances(r).await?;
            let all: Vec<&Value> = items.iter().collect();
            let df = ops::compute_df(&open_store()?, &all)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&df)?);
            } else {
                print!("{}", ops::render_df(&df));
            }
        }
        MacMore::System {
            command: SystemCommand::Prune { all, force },
        } => {
            let items = instances(r).await?;
            let refs: Vec<&Value> = items.iter().collect();
            let store = open_store()?;
            let names: BTreeSet<String> = store.list()?.into_iter().map(|(k, _)| k).collect();
            let victims = ops::prunable_instances(&refs, &names, true);
            for v in &victims {
                println!("instance {} ({})", name_of(v), view::status(v));
            }
            let unused = if all {
                let used: Vec<String> = items
                    .iter()
                    .filter_map(|v| v["labels"][IMAGE_LABEL].as_str().map(String::from))
                    .collect();
                ops::unused_images(&store, &used)?
            } else {
                vec![]
            };
            if !confirm(&format!("This deletes {} instances", victims.len()), force)? {
                println!("cancelled");
                return Ok(());
            }
            for v in victims {
                crate::mac_cmd::delete_instance(r, id_of(v)?).await?;
                println!("removed {}", name_of(v));
            }
            prune_store(&store, &unused, false, true)?;
        }
        MacMore::Image {
            command:
                ImageCommand::Prune {
                    all: true,
                    dry_run,
                    force,
                    ..
                },
        } => {
            let items = instances(r).await?;
            let used: Vec<String> = items
                .iter()
                .filter_map(|v| v["labels"][IMAGE_LABEL].as_str().map(String::from))
                .collect();
            let store = open_store()?;
            let unused = ops::unused_images(&store, &used)?;
            prune_store(&store, &unused, dry_run, force)?;
        }
        MacMore::Compose { command } => crate::mac_compose::run_daemon(r, command).await?,
        _ => bail!("not an instance command"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        c: crate::mac_cmd::MacCommand,
    }

    fn parse(args: &[&str]) -> crate::mac_cmd::MacCommand {
        let mut v = vec!["t"];
        v.extend(args);
        T::try_parse_from(v).unwrap().c
    }

    fn vm(st: &str, policy: &str, manual: bool) -> Value {
        let mut l = json!({"fluxvm.mac": "instance", "fluxvm.mac.restart": policy});
        if manual {
            l["fluxvm.mac.stopped"] = json!("true");
        }
        json!({"name": "x", "status": st, "labels": l})
    }

    #[test]
    fn restart_policies_decide_what_reconcile_starts() {
        assert!(should_restart(&vm("stopped", "always", true)));
        assert!(should_restart(&vm("failed", "always", false)));
        assert!(should_restart(&vm("stopped", "unless-stopped", false)));
        assert!(!should_restart(&vm("stopped", "unless-stopped", true)));
        assert!(should_restart(&vm("failed", "unless-stopped", false)));
        assert!(should_restart(&vm("failed", "on-failure", false)));
        assert!(!should_restart(&vm("stopped", "on-failure", false)));
        assert!(!should_restart(&vm("stopped", "no", false)));
        assert!(!should_restart(&vm("running", "always", false)));
        assert!(!should_restart(&vm("paused", "always", false)));
        assert!(!should_restart(&json!({"status": "stopped", "labels": {}})));
    }

    #[test]
    fn exec_text_applies_image_env_then_flags() {
        let mut cfg = mac_config::ImageConfig::default();
        cfg.set_env("A", "1");
        cfg.workdir = Some("/w".into());
        assert_eq!(
            exec_shell(
                &cfg,
                &["ls".into(), "x y".into()],
                &[("B".into(), "2".into())],
                None
            ),
            "export A='1'; export B='2'; cd '/w' && 'ls' 'x y'"
        );
        assert_eq!(
            exec_shell(&Default::default(), &["sw_vers".into()], &[], Some("/t")),
            "cd '/t' && sw_vers"
        );
        assert_eq!(
            exec_shell(&Default::default(), &["uname -a".into()], &[], None),
            "uname -a"
        );
    }

    #[test]
    fn only_image_commands_and_dry_runs_are_client_side() {
        use crate::mac_cmd::MacCommand as C;
        for (args, client) in [
            (vec!["history", "a"], true),
            (vec!["save", "a", "--output", "f.tar"], true),
            (vec!["load", "-i", "f.tar"], true),
            (vec!["image", "inspect", "a"], true),
            (vec!["image", "prune"], true),
            (vec!["image", "prune", "--all"], false),
            (vec!["inspect", "a"], false),
            (vec!["logs", "a"], false),
            (vec!["stop", "a"], false),
            (vec!["prune"], false),
            (vec!["system", "df"], false),
            (vec!["commit", "a", "b:1"], false),
            (vec!["compose", "up", "--dry-run"], true),
            (vec!["compose", "up"], false),
            (vec!["compose", "down"], false),
            (vec!["ps"], false),
            (vec!["images"], true),
            (vec!["run", "--rm", "img", "--", "x"], false),
        ] {
            assert_eq!(C::is_client_side(&parse(&args)), client, "{args:?}");
        }
    }

    #[test]
    fn run_ps_exec_and_build_flags_parse() {
        use crate::mac_cmd::MacCommand as C;
        let C::Run {
            opts,
            image,
            cmd,
            user,
            ..
        } = parse(&[
            "run",
            "-e",
            "A=1",
            "-e",
            "B",
            "--env-file",
            "e.env",
            "-v",
            "./s:/s:ro",
            "-w",
            "/s",
            "--label",
            "k=v",
            "--restart",
            "always",
            "--cpus",
            "4",
            "--memory",
            "8g",
            "-d",
            "img:1",
            "--",
            "make",
            "test",
        ])
        else {
            panic!()
        };
        assert_eq!(
            (opts.env.len(), opts.volume.len(), opts.cpus, opts.detach),
            (2, 1, Some(4), true)
        );
        assert_eq!(opts.restart, "always");
        assert_eq!(opts.memory.as_deref(), Some("8g"));
        assert_eq!((image.as_str(), cmd.len(), user), ("img:1", 2, None));
        let C::Ps { json, opts } = parse(&[
            "ps",
            "-a",
            "-f",
            "status=running",
            "-f",
            "name=x",
            "--format",
            "{{.Name}}",
            "-q",
        ]) else {
            panic!()
        };
        assert!(!json && opts.all && opts.quiet);
        assert_eq!(opts.filter.len(), 2);
        let C::Exec { opts, cmd, .. } =
            parse(&["exec", "web", "-e", "A=1", "-w", "/tmp", "--", "ls", "-l"])
        else {
            panic!()
        };
        assert_eq!(
            (opts.env.len(), opts.workdir.as_deref(), cmd.len()),
            (1, Some("/tmp"), 2)
        );
        let C::Build { build_arg, .. } = parse(&[
            "build",
            "-t",
            "x",
            "--build-arg",
            "A=1",
            "--build-arg",
            "B=2",
            "--dry-run",
        ]) else {
            panic!()
        };
        assert_eq!(build_arg, ["A=1", "B=2"]);
    }

    #[test]
    fn instance_verbs_parse() {
        use crate::mac_cmd::MacCommand as C;
        assert!(
            matches!(parse(&["stop", "a", "b"]), C::More(MacMore::Stop { names }) if names.len() == 2)
        );
        assert!(matches!(
            parse(&["logs", "-f", "--tail", "5", "a"]),
            C::More(MacMore::Logs {
                follow: true,
                tail: 5,
                ..
            })
        ));
        assert!(matches!(
            parse(&["port", "a", "22"]),
            C::More(MacMore::Port { port: Some(22), .. })
        ));
        assert!(
            matches!(parse(&["top", "a", "--", "aux"]), C::More(MacMore::Top { ps_args, .. }) if ps_args == ["aux"])
        );
        assert!(matches!(
            parse(&["rename", "a", "b"]),
            C::More(MacMore::Rename { .. })
        ));
        assert!(
            matches!(parse(&["commit", "a", "img:2", "-m", "msg"]), C::More(MacMore::Commit { message, .. }) if message == "msg")
        );
        assert!(matches!(
            parse(&["system", "prune", "--all", "-f"]),
            C::More(MacMore::System {
                command: SystemCommand::Prune {
                    all: true,
                    force: true
                }
            })
        ));
        assert!(
            T::try_parse_from(["t", "stop"]).is_err(),
            "stop needs a name"
        );
        assert!(T::try_parse_from(["t", "cp", "only-one"]).is_err());
    }

    #[test]
    fn every_new_command_is_in_the_shell_completions() {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Bash,
            &mut crate::Cli::command(),
            "fluxctl",
            &mut buf,
        );
        let text = String::from_utf8(buf).unwrap();
        // The generated function names differ between clap_complete versions, so look for words only the new commands use.
        for word in [
            "reconcile",
            "unpause",
            "--build-arg",
            "--env-file",
            "--restart",
            "--orphaned",
            "compose",
            "--project-name",
        ] {
            assert!(text.contains(word), "bash completions lack {word}");
        }
    }

    #[test]
    fn help_carries_examples() {
        use clap::CommandFactory;
        let mut c = crate::Cli::command();
        let mac = c.find_subcommand_mut("mac").unwrap();
        for (sub, needle) in [
            ("logs", "fluxctl mac logs -f"),
            ("cp", "fluxctl mac cp ./site"),
            ("inspect", "jq"),
            ("top", "fluxctl mac top"),
        ] {
            let help = mac
                .find_subcommand_mut(sub)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(help.contains(needle), "{sub}: {help}");
        }
        let run = mac
            .find_subcommand_mut("run")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(
            run.contains("Examples") && run.contains("--restart"),
            "{run}"
        );
    }
}
