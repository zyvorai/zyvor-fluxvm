// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac compose up|down|ps`: a small YAML or JSON file describing macOS instances, started in `depends_on` order.
//!
//! LIMIT, loudly: a Mac runs at most TWO macOS VMs at once (Apple's licence, enforced by the daemon). A file with more than two
//! services is refused up front, and the two must also fit beside any other macOS VM already running on the host.
//!
//! Supported service keys: `image`, `command` (string = shell text, list = argv), `environment` (map or `K=V` list),
//! `env_file`, `ports` (`HOST:GUEST`), `volumes` (`./host:/guest[:ro]`, relative to the compose file), `working_dir`,
//! `depends_on` (list, or map with a `condition`: `service_healthy` waits for the dependency's image HEALTHCHECK, other values mean "started"), `labels`, `restart`, `cpus`, `mem_limit`,
//! `user`. Anything else (`networks`, `deploy`, ...) is an error, not silently ignored.
//!
//! More keys: `secrets` and `configs` (file-based only, shared read-only), `extends`, top-level `include`, `x-` extension keys and
//! `${VAR}` interpolation with a `.env` file; see mac_compose_cfg. Commands: up, down, ps, logs, exec, restart, config, stop, start,
//! images, pull, push, ls, run, top, cp.
//!
//! `depends_on` means "started after, once SSH answers"; with `condition: service_healthy` it also waits (up to 10 minutes) for the
//! dependency's image HEALTHCHECK to pass, probed over SSH (see mac_health). A dependency whose image has no HEALTHCHECK is an error.
//! A service's `command` runs in the background in the guest (like `mac run -d`).
//!
//! Not verified against a real macOS VM: parsing, ordering and the request bodies are unit tested; `up`, `down` and `ps` were
//! never run against a daemon.

use crate::mac_compose_cfg as cfg;
use crate::mac_compose_ext::{self as ext, Condition, HealthcheckCfg, PlacementCfg};
use crate::mac_compose_watch::{Develop, WatchRule};
use crate::mac_config;
use crate::mac_inspect::{PROJECT_LABEL, SERVICE_LABEL};
use crate::mac_instance::{LaunchSpec, Restart, parse_label, parse_mount};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The most macOS VMs one Mac runs at once.
pub const MAX_SERVICES: usize = 2;

#[derive(Subcommand, Debug)]
#[command(
    after_help = "LIMIT: a Mac runs at most 2 macOS VMs at once, so a compose file has at most 2 services.\n\n\
Example mac-compose.yaml:\n  name: ci\n  services:\n    db:\n      image: macos-postgres:16\n      ports: [\"5432:5432\"]\n    tests:\n      image: macos-xcode:15\n      depends_on: [db]\n      command: [\"xcodebuild\", \"test\"]\n      environment:\n        DB_HOST: 192.168.64.1"
)]
pub enum ComposeCommand {
    /// Start the services in depends_on order (each as a macOS VM; at most 2 in total).
    #[command(
        after_help = "Examples:\n  fluxctl mac compose up\n  fluxctl mac compose up --wait            # also wait for HEALTHCHECKs, like docker compose up --wait\n  fluxctl mac compose up --dry-run         # print the order, change nothing\n  fluxctl mac compose up --profile debug --build"
    )]
    Up {
        /// The compose file (default: mac-compose.yaml, .yml or .json in this directory).
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Project name (default: `name:` in the file, else the directory name).
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Like `docker compose up --wait`: after everything started, wait until every service whose image has a HEALTHCHECK is
        /// healthy (same as --wait-healthy). An optional SECS is how long each guest may take to answer on SSH (default 900).
        #[arg(long, num_args = 0..=1, default_missing_value = "900", value_name = "SECS")]
        wait: Option<u64>,
        /// Print the order and what would be created, and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Run `mac build` for every service with `build:` even when its image already exists (otherwise only missing images
        /// are built).
        #[arg(long)]
        build: bool,
        /// Never build: fail when a service with `build:` has no image in the store.
        #[arg(long, conflicts_with = "build")]
        no_build: bool,
        /// After everything started, wait until every service whose image has a HEALTHCHECK is healthy.
        #[arg(long)]
        wait_healthy: bool,
        /// Seconds to wait for a `condition: service_healthy` dependency or for `--wait-healthy` (0 = forever).
        #[arg(
            long,
            default_value_t = 600,
            value_name = "SECS",
            visible_alias = "wait-timeout"
        )]
        health_timeout: u64,
        /// Also start services with this profile (repeatable; also `COMPOSE_PROFILES=a,b`).
        #[arg(long = "profile", value_name = "NAME")]
        profile: Vec<String>,
        /// Run SERVICE as N copies (`--scale web=2`, repeatable). Copies are named PROJECT-SERVICE-1, -2, ... and each is a macOS
        /// VM, so on one Mac the total at the same moment is still at most 2.
        #[arg(long, value_name = "SERVICE=N")]
        scale: Vec<String>,
        /// Spread the project over the Macs of this inventory (see `fluxctl mac-fleet`), at most 2 macOS VMs per Mac. With
        /// `--dry-run` it only explains where each service would go and why.
        #[arg(long, value_name = "INVENTORY")]
        fleet: Option<PathBuf>,
    },
    /// Start the project, then watch each service's `develop: watch:` files and apply restart policies until Ctrl-C.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose watch\n  fluxctl mac compose watch --no-up     # the project is already up\n\nA rule is {path, action, target, ignore}; actions: sync, sync+restart, restart, rebuild. `restart: on-failure:3` is enforced here (3 restarts, then it stays down)."
    )]
    Watch {
        #[command(flatten)]
        t: Target,
        /// Do not run `up` first: watch what is already running.
        #[arg(long)]
        no_up: bool,
        /// Seconds each guest may take to answer on SSH.
        #[arg(long, default_value_t = 900, value_name = "SECS")]
        wait: u64,
        /// Seconds between looks at the files.
        #[arg(long, default_value_t = 2, value_name = "SECS")]
        interval: u64,
        /// Stop after this many looks (0 = until Ctrl-C).
        #[arg(long, default_value_t = 0, hide = true)]
        polls: u64,
        #[arg(long = "profile", value_name = "NAME")]
        profile: Vec<String>,
        #[arg(long, value_name = "SERVICE=N")]
        scale: Vec<String>,
    },
    /// Stop and delete the project's instances (reverse depends_on order).
    Down {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Remove the project from every Mac of this inventory (the project was started with `up --fleet`).
        #[arg(long, value_name = "INVENTORY")]
        fleet: Option<PathBuf>,
    },
    /// Show a service's console log (the VM's serial output; `mac logs`).
    Logs {
        service: String,
        #[command(flatten)]
        t: Target,
        #[arg(short = 'F', long)]
        follow: bool,
        #[arg(long, default_value_t = 100)]
        tail: usize,
    },
    /// Run a command in a service's instance over SSH (`mac exec`).
    Exec {
        service: String,
        #[command(flatten)]
        t: Target,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Stop and start the project's instances again (all, or the named services).
    Restart {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
    },
    /// List the project's instances.
    Ps {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Include stopped instances.
        #[arg(short = 'a', long)]
        all: bool,
        /// List the project's instances on every Mac of this inventory.
        #[arg(long, value_name = "INVENTORY")]
        fleet: Option<PathBuf>,
    },
    /// Print the compose file as it will be used: `${VAR}` filled in (and `.env` read), `include` and `extends` merged, `env_file`
    /// read into `environment`, only the selected profiles. `--quiet` only checks it. Needs no daemon.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose config\n  fluxctl mac compose config --quiet && echo valid\n  fluxctl mac compose config --format json --profile debug\n  fluxctl mac compose config --services"
    )]
    Config {
        #[command(flatten)]
        t: Target,
        /// Also include services with this profile (repeatable; also `COMPOSE_PROFILES=a,b`).
        #[arg(long = "profile", value_name = "NAME")]
        profile: Vec<String>,
        /// Print nothing; the exit status says whether the file is valid.
        #[arg(short = 'q', long)]
        quiet: bool,
        /// `yaml` (default) or `json`.
        #[arg(long, default_value = "yaml", value_parser = ["yaml", "json"])]
        format: String,
        /// Print only the service names, in start order.
        #[arg(long)]
        services: bool,
    },
    /// Stop the project's running instances (all, or the named services), dependents first. The disks are kept; `compose start`
    /// boots them again.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose stop\n  fluxctl mac compose stop tests"
    )]
    Stop {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
    },
    /// Start the project's stopped instances (all, or the named services), dependencies first. Needs free macOS slots (a Mac
    /// runs at most 2 macOS VMs).
    #[command(
        after_help = "Examples:\n  fluxctl mac compose start\n  fluxctl mac compose start db"
    )]
    Start {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
    },
    /// The images the services use: service, image, id and size in the local store. Needs no daemon.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose images\n  fluxctl mac compose images -q"
    )]
    Images {
        #[command(flatten)]
        t: Target,
        /// Print only the image names.
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Pull the services' images (all, or the named services) into the local store. A service that only has `build:` is built
    /// locally, so it is skipped. Needs no daemon.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose pull\n  fluxctl mac compose pull db\n(credentials: FLUXVM_REGISTRY_USER / FLUXVM_REGISTRY_PASSWORD)"
    )]
    Pull {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
        /// Refuse an image without a signature from a trusted key (see `mac pull --verify`).
        #[arg(long)]
        verify: bool,
    },
    /// Push the services' images to their registry (all, or the named services). Each `image:` must name a registry
    /// (`ghcr.io/acme/app:1`). Apple's licence limits where macOS images may be stored: use a private registry.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose push\n  fluxctl mac compose push tests --sign"
    )]
    Push {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
        /// Sign the pushed manifest with your key (`mac keys generate`).
        #[arg(long)]
        sign: bool,
        /// Push to a well-known public registry anyway (see `mac push --help`).
        #[arg(long = "i-have-checked-the-licence")]
        licence_override: bool,
    },
    /// List the compose projects on this Mac (running ones; `-a` also those that are all stopped).
    #[command(
        after_help = "Examples:\n  fluxctl mac compose ls\n  fluxctl mac compose ls -a --json\n  fluxctl mac compose ls -q"
    )]
    Ls {
        /// Include projects whose instances are all stopped.
        #[arg(short = 'a', long)]
        all: bool,
        /// Print only the project names.
        #[arg(short = 'q', long)]
        quiet: bool,
        #[arg(long, conflicts_with = "quiet")]
        json: bool,
    },
    /// Run a command once in a NEW VM made from a service's image (not in the running one; use `compose exec` for that).
    /// It takes a macOS slot, so it is refused when 2 macOS VMs already run. Ports are not published and dependencies are not
    /// started: `compose up` first. Without a command the service's `command:` runs.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose run --rm tests -- xcodebuild -version\n  fluxctl mac compose run --rm -e MODE=fast -w /src -u admin tests -- ./check.sh\n\nA Mac runs at most 2 macOS VMs: with both slots in use (for example after `compose up` of 2 services) run `compose stop SERVICE` first."
    )]
    Run {
        service: String,
        #[command(flatten)]
        t: Target,
        /// Delete the one-off VM when the command ends.
        #[arg(long)]
        rm: bool,
        /// Set an environment variable for the command: `NAME=value`, or `NAME` to copy it from this shell. Repeatable.
        #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
        env: Vec<String>,
        /// Directory to run the command in.
        #[arg(short = 'w', long)]
        workdir: Option<String>,
        /// SSH user in the guest (default: the service's `user:`, the image's USER, else admin).
        #[arg(short = 'u', long)]
        user: Option<String>,
        /// Seconds to wait for the guest to answer on SSH.
        #[arg(long, default_value_t = 900, value_name = "SECS")]
        ssh_timeout: u64,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// The processes in the services' instances (`ps` over SSH): all services, or the named ones. Extra words after `--` go to
    /// `ps`.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose top\n  fluxctl mac compose top db -- aux"
    )]
    Top {
        services: Vec<String>,
        #[command(flatten)]
        t: Target,
        #[arg(long, default_value = "admin")]
        user: String,
        #[arg(last = true)]
        ps_args: Vec<String>,
    },
    /// Copy between this Mac and a service's instance: `SERVICE:/path` is the guest side, as in `docker compose cp`.
    #[command(
        after_help = "Examples:\n  fluxctl mac compose cp ./site web:/Users/admin/site\n  fluxctl mac compose cp db:/var/log/system.log ./system.log"
    )]
    Cp {
        src: String,
        dst: String,
        #[command(flatten)]
        t: Target,
        /// SSH user in the guest.
        #[arg(long, default_value = "admin")]
        user: String,
    },
}

/// Which compose file and project a command is about.
#[derive(clap::Args, Debug, Clone)]
pub struct Target {
    /// The compose file (default: mac-compose.yaml, .yml or .json in this directory).
    #[arg(short = 'f', long)]
    pub file: Option<PathBuf>,
    /// Project name (default: `name:` in the file, else the directory name).
    #[arg(short = 'p', long = "project-name")]
    pub project: Option<String>,
}

impl ComposeCommand {
    /// `up --dry-run` only reads the file and the store.
    pub fn is_client_side(&self) -> bool {
        matches!(
            self,
            ComposeCommand::Up { dry_run: true, .. }
                | ComposeCommand::Up { fleet: Some(_), .. }
                | ComposeCommand::Down { fleet: Some(_), .. }
                | ComposeCommand::Ps { fleet: Some(_), .. }
                | ComposeCommand::Config { .. }
                | ComposeCommand::Images { .. }
                | ComposeCommand::Pull { .. }
                | ComposeCommand::Push { .. }
        )
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    services: BTreeMap<String, Service>,
    /// Top-level `secrets:`: only `file:` sources work.
    #[serde(default)]
    secrets: BTreeMap<String, cfg::FileSource>,
    /// Top-level `configs:`: only `file:` sources work.
    #[serde(default)]
    configs: BTreeMap<String, cfg::FileSource>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct Service {
    /// May be omitted when `build` is set: the built image is then `PROJECT-SERVICE:latest`.
    #[serde(default)]
    image: String,
    /// `build: ./dir`, or `{context, dockerfile, args}`; `dockerfile` names the Macfile. Built with `mac build` on `up`.
    #[serde(default)]
    build: Option<BuildSpec>,
    /// The service only runs when one of these profiles is selected (`--profile`, `COMPOSE_PROFILES`).
    #[serde(default)]
    profiles: Vec<String>,
    #[serde(default)]
    command: Option<StrOrList>,
    #[serde(default)]
    environment: Option<MapOrList>,
    #[serde(default)]
    env_file: Option<EnvFiles>,
    /// Files from the top-level `secrets:` (read-only, default `/private/var/run/secrets/NAME` in the guest).
    #[serde(default)]
    secrets: Vec<cfg::FileRef>,
    /// Files from the top-level `configs:` (read-only, default `/private/var/run/configs/NAME` in the guest).
    #[serde(default)]
    configs: Vec<cfg::FileRef>,
    #[serde(default)]
    ports: Vec<Scalar>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    depends_on: Option<DependsOn>,
    #[serde(default)]
    labels: Option<MapOrList>,
    #[serde(default)]
    restart: Option<String>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    mem_limit: Option<Scalar>,
    #[serde(default)]
    user: Option<String>,
    /// Replaces the image's HEALTHCHECK (and is what `service_healthy` waits for).
    #[serde(default)]
    healthcheck: Option<HealthcheckCfg>,
    /// `develop: watch:` rules for `compose watch`.
    #[serde(default)]
    develop: Option<Develop>,
    /// Where the service may run in a Mac fleet (`--fleet`); ignored on one Mac.
    #[serde(default)]
    placement: Option<PlacementCfg>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum BuildSpec {
    Context(String),
    Full {
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        dockerfile: Option<String>,
        #[serde(default)]
        args: Option<MapOrList>,
    },
}

/// A service's `build:` resolved against the compose file's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    pub context: PathBuf,
    /// The Macfile, when not `CONTEXT/Macfile`.
    pub file: Option<PathBuf>,
    /// `NAME=value` build args.
    pub args: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum Scalar {
    Str(String),
    Int(u64),
}

impl Scalar {
    fn text(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::Int(n) => n.to_string(),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum StrOrList {
    One(String),
    Many(Vec<String>),
}

/// `env_file: a.env`, or a list of paths and `{path, required}` entries.
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum EnvFiles {
    One(String),
    Many(Vec<EnvFileEntry>),
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum EnvFileEntry {
    Path(String),
    Full(EnvFileFull),
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct EnvFileFull {
    path: String,
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    format: Option<String>,
}

impl EnvFiles {
    /// `(path, required)` for each file.
    fn entries(&self) -> Result<Vec<(String, bool)>> {
        let list = match self {
            EnvFiles::One(f) => return Ok(vec![(f.clone(), true)]),
            EnvFiles::Many(l) => l,
        };
        list.iter()
            .map(|e| match e {
                EnvFileEntry::Path(p) => Ok((p.clone(), true)),
                EnvFileEntry::Full(f) => {
                    if let Some(fmt) = &f.format {
                        ensure!(
                            fmt == "raw",
                            "env_file {:?}: `format: {fmt}` is not supported; only plain KEY=value files (remove `format`)",
                            f.path
                        );
                    }
                    Ok((f.path.clone(), f.required.unwrap_or(true)))
                }
            })
            .collect()
    }
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum MapOrList {
    Map(BTreeMap<String, Option<Scalar>>),
    List(Vec<String>),
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum DependsOn {
    List(Vec<String>),
    Map(BTreeMap<String, serde_yaml::Value>),
}

impl MapOrList {
    /// `K=V` strings; a map value of null (or a list item without `=`) is a bare `K`.
    fn pairs(&self) -> Vec<String> {
        match self {
            MapOrList::Map(m) => m
                .iter()
                .map(|(k, v)| match v {
                    Some(v) => format!("{k}={}", v.text()),
                    None => k.clone(),
                })
                .collect(),
            MapOrList::List(l) => l.clone(),
        }
    }
}

/// One service ready to start.
#[derive(Debug, Clone)]
pub struct ServicePlan {
    pub service: String,
    pub spec: LaunchSpec,
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub depends_on: Vec<String>,
    /// The `depends_on` entries with `condition: service_healthy`: `up` waits for their HEALTHCHECK before starting this service.
    pub healthy_deps: Vec<String>,
    pub has_dependents: bool,
    pub build: Option<BuildPlan>,
    /// Secrets and configs: files from this Mac shared read-only into the guest.
    pub files: Vec<cfg::FileMount>,
    /// Every `depends_on` entry with its condition.
    pub conditions: BTreeMap<String, Condition>,
    /// Copy number (1-based) and how many copies of this service there are (`up --scale`).
    pub replica: u32,
    pub replicas: u32,
    /// `develop: watch:` rules, absolute on this Mac.
    pub watch: Vec<WatchRule>,
    pub placement: PlacementCfg,
    /// Something waits for this service with `service_completed_successfully`: its command runs to the end in the foreground,
    /// and its VM is stopped when it succeeds (which frees its macOS slot).
    pub run_to_completion: bool,
    /// `restart: on-failure:N`: N restarts at most (enforced by `compose watch`).
    pub restart_limit: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub project: String,
    /// Most macOS VMs this project holds at the same moment on one Mac (see [`peak_slots`]).
    pub peak_slots: usize,
    /// In start order.
    pub services: Vec<ServicePlan>,
}

#[cfg(test)]
pub fn parse(text: &str) -> Result<ComposeFile> {
    // JSON is YAML, so one parser reads both; the error names the line and the offending key.
    let mut v: serde_yaml::Value =
        serde_yaml::from_str(text).context("reading the compose file")?;
    v.apply_merge()
        .context("a YAML merge key (`<<`) is wrong")?;
    if let serde_yaml::Value::Mapping(m) = &mut v {
        cfg::strip_extensions(m);
    }
    from_value(v)
}

/// The typed file from a merged YAML tree. A key this tool does not implement is named, with what to do about it.
fn from_value(v: serde_yaml::Value) -> Result<ComposeFile> {
    serde_yaml::from_value(v).map_err(|e| {
        let msg = e.to_string();
        let hint = if let Some(m) = cfg::unsupported_key(&e) {
            m
        } else if msg.contains("missing field `services`") {
            "the file has no `services:` key; add `services:` with at least one service (see docs/macos-containers.md#compose)".to_string()
        } else if msg.contains("untagged enum") {
            "a key has a value of the wrong shape (for example an env_file, build or depends_on entry); check it against docs/macos-containers.md#compose".to_string()
        } else {
            "the compose file has a value this tool cannot read; fix it as the message says".to_string()
        };
        anyhow::Error::new(e).context(hint)
    })
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Project name from a directory name: lowercase, anything else becomes `-`.
pub fn project_from_dir(dir: &Path) -> String {
    let raw = dir.file_name().and_then(|n| n.to_str()).unwrap_or("mac");
    let s: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches(['-', '_']).to_string();
    if s.is_empty() { "mac".into() } else { s }
}

/// Order services so each comes after what it `depends_on`; ties break alphabetically so the order is stable.
pub fn start_order(deps: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>> {
    for (s, ds) in deps {
        for d in ds {
            ensure!(d != s, "service {s:?} depends on itself");
            ensure!(
                deps.contains_key(d),
                "service {s:?} depends_on {d:?}, which is not defined"
            );
        }
    }
    let mut done: Vec<String> = Vec::new();
    let mut left: BTreeSet<&String> = deps.keys().collect();
    while !left.is_empty() {
        let ready: Vec<&String> = left
            .iter()
            .copied()
            .filter(|s| deps[*s].iter().all(|d| done.contains(d)))
            .collect();
        let Some(next) = ready.first().copied() else {
            let cycle: Vec<&str> = left.iter().map(|s| s.as_str()).collect();
            bail!("depends_on has a cycle among: {}", cycle.join(", "));
        };
        done.push(next.clone());
        left.remove(next);
    }
    Ok(done)
}

/// Validate and turn a parsed file into start-ordered plans. `base` is the directory relative volume paths are read from.
pub fn resolve(
    file: &ComposeFile,
    project: Option<&str>,
    base: &Path,
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Resolved> {
    resolve_with(file, project, base, home, lookup, &[])
}

/// A service runs when it has no `profiles`, or one of them is in `active`.
fn selected(svc: &Service, active: &[String]) -> bool {
    svc.profiles.is_empty()
        || active.iter().any(|a| a == "*")
        || svc.profiles.iter().any(|p| active.contains(p))
}

/// [`resolve`] with `--profile` selection. The two-VM limit applies to the SELECTED services.
pub fn resolve_with(
    file: &ComposeFile,
    project: Option<&str>,
    base: &Path,
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String> + Copy,
    profiles: &[String],
) -> Result<Resolved> {
    resolve_ext(
        file,
        project,
        base,
        home,
        lookup,
        profiles,
        &BTreeMap::new(),
        false,
    )
}

/// The VMs a project holds at the same moment on one Mac, in start order. A service that others wait for with
/// `service_completed_successfully` is stopped when it succeeds, before those start, so it gives its slot back.
pub fn peak_slots(services: &[ServicePlan]) -> usize {
    let mut live: Vec<&ServicePlan> = vec![];
    let mut peak = 0;
    for s in services {
        live.retain(|l| {
            !(l.run_to_completion && s.conditions.get(&l.service) == Some(&Condition::Completed))
        });
        live.push(s);
        peak = peak.max(live.len());
    }
    peak
}

/// What `depends_on` asks of each dependency (a list means `service_started`).
fn conditions_of(service: &str, svc: &Service) -> Result<BTreeMap<String, Condition>> {
    match &svc.depends_on {
        None => Ok(BTreeMap::new()),
        Some(DependsOn::List(l)) => Ok(l.iter().map(|d| (d.clone(), Condition::Started)).collect()),
        Some(DependsOn::Map(m)) => m
            .iter()
            .map(|(d, v)| Ok((d.clone(), ext::condition_of(service, d, v)?)))
            .collect(),
    }
}

/// [`resolve_with`] plus `--scale` copies. On one Mac the project may hold at most [`MAX_SERVICES`] VMs at once; with `fleet`
/// that limit is per Mac and is checked by the fleet planner instead.
#[allow(clippy::too_many_arguments)]
pub fn resolve_ext(
    file: &ComposeFile,
    project: Option<&str>,
    base: &Path,
    home: Option<&Path>,
    lookup: impl Fn(&str) -> Option<String> + Copy,
    profiles: &[String],
    scale: &BTreeMap<String, u32>,
    fleet: bool,
) -> Result<Resolved> {
    ensure!(
        !file.services.is_empty(),
        "the compose file defines no services"
    );
    let active: Vec<(&String, &Service)> = file
        .services
        .iter()
        .filter(|(_, s)| selected(s, profiles))
        .collect();
    ensure!(
        !active.is_empty(),
        "no service is selected: every service has profiles; pass --profile NAME (or COMPOSE_PROFILES)"
    );
    for s in scale.keys() {
        ensure!(
            active.iter().any(|(n, _)| *n == s),
            "{}",
            crate::mac_errors::coded(
                format!(
                    "--scale names {s:?}, which is not a selected service of this file (selected: {})",
                    active
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "MAC_COMPOSE_SCALE"
            )
        );
    }
    let project = project
        .map(str::to_string)
        .or_else(|| file.name.clone())
        .unwrap_or_else(|| project_from_dir(base));
    ensure!(
        valid_name(&project),
        "project name {project:?} must be lowercase letters, digits, - or _"
    );
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut conds: BTreeMap<String, BTreeMap<String, Condition>> = BTreeMap::new();
    for (name, svc) in active.iter().map(|(n, s)| (*n, *s)) {
        ensure!(
            valid_name(name),
            "service name {name:?} must be lowercase letters, digits, - or _"
        );
        let d = match &svc.depends_on {
            None => vec![],
            Some(DependsOn::List(l)) => l.clone(),
            Some(DependsOn::Map(m)) => m.keys().cloned().collect(),
        };
        conds.insert(name.clone(), conditions_of(name, svc)?);
        for dep in &d {
            if !active.iter().any(|(n, _)| *n == dep)
                && let Some(p) = file.services.get(dep)
            {
                bail!(
                    "service {name:?} depends_on {dep:?}, which is in the profile(s) {} that are not enabled: pass --profile {}",
                    p.profiles.join(", "),
                    p.profiles.first().map_or("NAME", String::as_str)
                );
            }
        }
        deps.insert(name.clone(), d);
    }
    let order = start_order(&deps).map_err(|e| {
        if file.services.len() > active.len() {
            e.context("(services outside the selected profiles are not considered)")
        } else {
            e
        }
    })?;
    let depended: BTreeSet<&String> = deps.values().flatten().collect();
    let mut services = Vec::new();
    for name in order {
        let svc = &file.services[&name];
        let ctx = |what: &str| format!("service {name:?}: {what}");
        ensure!(
            !svc.image.trim().is_empty() || svc.build.is_some(),
            "{}",
            ctx("needs `image` or `build`")
        );
        let build = svc.build.as_ref().map(|b| build_plan(b, base));
        let image = if svc.image.trim().is_empty() {
            format!("{project}-{name}:latest")
        } else {
            svc.image.clone()
        };
        let mounts = svc
            .volumes
            .iter()
            .map(|v| parse_mount(v, base, home))
            .collect::<Result<Vec<_>>>()
            .with_context(|| ctx("volumes"))?;
        let publish: Vec<String> = svc.ports.iter().map(Scalar::text).collect();
        for p in &publish {
            crate::run::parse_port(p).with_context(|| ctx("ports"))?;
        }
        let mut env_pairs: Vec<(String, String)> = Vec::new();
        if let Some(ef) = &svc.env_file {
            for (f, required) in ef.entries().with_context(|| ctx("env_file"))? {
                let p = base.join(&f);
                let text = match std::fs::read_to_string(&p) {
                    Ok(t) => t,
                    Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => bail!(
                        "{}: env_file {} cannot be read ({e}). Create it, or make it optional with `env_file: [{{path: {f}, required: false}}]`",
                        ctx(""),
                        p.display()
                    ),
                };
                env_pairs.extend(
                    mac_config::parse_env_file(&text, lookup).with_context(|| ctx("env_file"))?,
                );
            }
        }
        let mut files = cfg::resolve_files(
            &name,
            cfg::FileKind::Secret,
            &svc.secrets,
            &file.secrets,
            base,
        )?;
        files.extend(cfg::resolve_files(
            &name,
            cfg::FileKind::Config,
            &svc.configs,
            &file.configs,
            base,
        )?);
        if let Some(e) = &svc.environment {
            for p in e.pairs() {
                env_pairs.extend(
                    mac_config::parse_env_pair(&p, lookup).with_context(|| ctx("environment"))?,
                );
            }
        }
        let labels = match &svc.labels {
            Some(l) => l
                .pairs()
                .iter()
                .map(|p| parse_label(p))
                .collect::<Result<Vec<_>>>()
                .with_context(|| ctx("labels"))?,
            None => vec![],
        };
        let restart = match &svc.restart {
            Some(r) => Some(Restart::parse(r).with_context(|| ctx("restart"))?),
            None => None,
        };
        let restart_limit = match &svc.restart {
            Some(r) => ext::restart_limit(r)?,
            None => None,
        };
        let healthcheck = match &svc.healthcheck {
            Some(h) => Some(h.resolve(&name)?),
            None => None,
        };
        let placement = svc.placement.clone().unwrap_or_default();
        placement.check(&name)?;
        let watch = match &svc.develop {
            Some(d) => d.rules(&name, base, svc.build.is_some())?,
            None => vec![],
        };
        let cpus = match svc.cpus {
            Some(c) => {
                ensure!(
                    (1.0..=255.0).contains(&c),
                    "{}",
                    ctx("cpus must be a whole number from 1 to 255")
                );
                Some(c.round() as u8)
            }
            None => None,
        };
        let memory_mib = match &svc.mem_limit {
            Some(m) => {
                Some(mac_config::parse_memory_mib(&m.text()).with_context(|| ctx("mem_limit"))?)
            }
            None => None,
        };
        let command = match &svc.command {
            None => vec![],
            Some(StrOrList::One(s)) => vec![s.clone()],
            Some(StrOrList::Many(l)) => l.clone(),
        };
        services.push(ServicePlan {
            service: name.clone(),
            spec: LaunchSpec {
                name: format!("{project}-{name}"),
                image,
                publish,
                mounts,
                cpus,
                memory_mib,
                labels,
                restart,
                project: Some((project.clone(), name.clone())),
                healthcheck,
            },
            command,
            env: env_pairs,
            workdir: svc.working_dir.clone(),
            user: svc.user.clone(),
            depends_on: deps[&name].clone(),
            healthy_deps: conds[&name]
                .iter()
                .filter(|(_, c)| **c == Condition::Healthy)
                .map(|(d, _)| d.clone())
                .collect(),
            conditions: conds[&name].clone(),
            replica: 1,
            replicas: 1,
            watch,
            placement,
            run_to_completion: false,
            restart_limit,
            build,
            has_dependents: depended.contains(&name),
            files,
        });
    }
    let waited: BTreeSet<String> = services
        .iter()
        .flat_map(|s| {
            s.conditions
                .iter()
                .filter(|(_, c)| **c == Condition::Completed)
                .map(|(d, _)| d.clone())
        })
        .collect();
    let mut expanded = Vec::new();
    for mut s in services {
        s.run_to_completion = waited.contains(&s.service);
        let n = scale.get(&s.service).copied().unwrap_or(1);
        if n > 1 {
            if !fleet && let Some(p) = s.spec.publish.iter().find(|p| p.contains(':')) {
                return Err(crate::mac_errors::coded(
                    format!(
                        "service {:?} is scaled to {n} but publishes {p:?}: every copy would want the same port on this Mac. Remove the fixed host port from `ports:`, or place the copies on different Macs with --fleet",
                        s.service
                    ),
                    "MAC_COMPOSE_SCALE",
                ));
            }
            ensure!(
                !s.run_to_completion,
                "{}",
                crate::mac_errors::coded(
                    format!(
                        "service {:?} runs to completion and cannot be scaled",
                        s.service
                    ),
                    "MAC_COMPOSE_SCALE"
                )
            );
        }
        for k in 1..=n {
            let mut c = s.clone();
            c.replica = k;
            c.replicas = n;
            c.spec.name = ext::instance_name(&project, &s.service, k, n);
            expanded.push(c);
        }
    }
    let services = expanded;
    let peak = peak_slots(&services);
    if !fleet {
        ensure!(
            peak <= MAX_SERVICES,
            "this file selects {} services ({} VMs, at most {peak} at the same moment), but a Mac runs at most {MAX_SERVICES} macOS VMs at once (Apple's licence limit). Remove services or split the file, select fewer with `profiles:` and --profile, let a finishing service give its slot back (`depends_on: {{NAME: {{condition: service_completed_successfully}}}}`), or spread the project over several Macs with `--fleet INVENTORY`",
            active.len(),
            services.len()
        );
    }
    Ok(Resolved {
        project,
        peak_slots: peak,
        services,
    })
}

/// Like [`resolve_text`] for a fleet: no one-Mac limit.
#[cfg(test)]
pub(crate) fn resolve_fleet_text(y: &str) -> Result<Resolved> {
    resolve_ext(
        &parse(y)?,
        None,
        Path::new("/p"),
        None,
        |_| None,
        &[],
        &BTreeMap::new(),
        true,
    )
}

/// A parsed compose text resolved the way the tests want it (project `p`, nothing selected by profile).
#[cfg(test)]
pub(crate) fn resolve_text(y: &str) -> Result<Resolved> {
    resolve(&parse(y)?, None, Path::new("/p"), None, |_| None)
}

fn build_plan(b: &BuildSpec, base: &Path) -> BuildPlan {
    match b {
        BuildSpec::Context(c) => BuildPlan {
            context: base.join(c),
            file: None,
            args: vec![],
        },
        BuildSpec::Full {
            context,
            dockerfile,
            args,
        } => {
            let ctx = base.join(context.as_deref().unwrap_or("."));
            BuildPlan {
                file: dockerfile.as_ref().map(|f| ctx.join(f)),
                context: ctx,
                args: args.as_ref().map(MapOrList::pairs).unwrap_or_default(),
            }
        }
    }
}

/// Whether a service's image must be built now: `--build` always, otherwise only when the image is missing; `--no-build` turns a
/// missing image into an error.
fn needs_build(build_flag: bool, no_build: bool, image_in_store: bool) -> Result<bool> {
    Ok(match (build_flag, no_build, image_in_store) {
        (true, _, _) => true,
        (false, true, false) => bail!("the image is not in the store and --no-build was given"),
        (false, false, false) => true,
        (false, _, true) => false,
    })
}

pub fn render_plan(r: &Resolved) -> String {
    let mut out = format!(
        "project {} ({})\n",
        r.project,
        if r.peak_slots == r.services.len() && r.services.len() <= MAX_SERVICES {
            format!("{} of {MAX_SERVICES} macOS VMs", r.services.len())
        } else {
            format!(
                "{} macOS VMs, at most {} at once; one Mac allows {MAX_SERVICES}",
                r.services.len(),
                r.peak_slots
            )
        }
    );
    for (i, s) in r.services.iter().enumerate() {
        let after = if s.depends_on.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = s
                .depends_on
                .iter()
                .map(|d| {
                    if s.healthy_deps.contains(d) {
                        format!("{d} (healthy)")
                    } else if s.conditions.get(d) == Some(&Condition::Completed) {
                        format!("{d} (finished)")
                    } else {
                        d.clone()
                    }
                })
                .collect();
            format!(" after {}", list.join(", "))
        };
        let mut notes: Vec<String> = vec![];
        if s.run_to_completion {
            notes.push("runs to the end, then its VM is stopped".into());
        }
        if !s.watch.is_empty() {
            notes.push(format!("{} watch rule(s)", s.watch.len()));
        }
        if s.replicas > 1 {
            notes.push(format!("copy {} of {}", s.replica, s.replicas));
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", notes.join("; "))
        };
        out.push_str(&format!(
            "{}. {} = {} from {}{after}{notes}\n",
            i + 1,
            s.service,
            s.spec.name,
            s.spec.image
        ));
    }
    out
}

fn find_file(file: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(f) = file {
        return Ok(f);
    }
    for n in ["mac-compose.yaml", "mac-compose.yml", "mac-compose.json"] {
        if Path::new(n).is_file() {
            return Ok(PathBuf::from(n));
        }
    }
    bail!("no compose file: pass -f FILE, or create mac-compose.yaml in this directory")
}

fn load(file: Option<PathBuf>, project: Option<&str>) -> Result<Resolved> {
    load_with(file, project, &profiles_from_env(&[]))
}

/// `--profile` values plus `COMPOSE_PROFILES` (comma separated).
fn profiles_from_env(flags: &[String]) -> Vec<String> {
    let mut v = flags.to_vec();
    if let Ok(e) = std::env::var("COMPOSE_PROFILES") {
        v.extend(
            e.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        );
    }
    v
}

/// For commands about instances that already exist (`down`, `ps`, `logs`, ...): every profile counts, and a file that is over the
/// two-service limit is not an error here (only `up` starts anything). Returns the project and the services in start order.
fn load_existing(file: Option<PathBuf>, project: Option<&str>) -> Result<(String, Vec<String>)> {
    match load_with(file.clone(), project, &["*".to_string()]) {
        Ok(r) => Ok((
            r.project,
            r.services.into_iter().map(|s| s.service).collect(),
        )),
        Err(e) => match project {
            Some(p) => Ok((p.to_string(), vec![])),
            None => Err(e),
        },
    }
}

fn load_with(
    file: Option<PathBuf>,
    project: Option<&str>,
    profiles: &[String],
) -> Result<Resolved> {
    load_scaled(file, project, profiles, &BTreeMap::new(), false)
}

/// [`load_with`] with `--scale` copies; `fleet` lifts the one-Mac limit (the fleet planner checks it per Mac).
fn load_scaled(
    file: Option<PathBuf>,
    project: Option<&str>,
    profiles: &[String],
    scale: &BTreeMap<String, u32>,
    fleet: bool,
) -> Result<Resolved> {
    let path = find_file(file)?;
    let abs = std::fs::canonicalize(&path).unwrap_or(path.clone());
    let base = abs.parent().unwrap_or(Path::new("."));
    // Variables: the environment, then `.env` next to the file.
    let lookup = cfg::lookup_with_dotenv(base, |k| std::env::var(k).ok());
    let lk: &dyn Fn(&str) -> Option<String> = &*lookup;
    let top = cfg::load_tree(&path, lk, &mut vec![])?;
    let parsed =
        from_value(serde_yaml::Value::Mapping(top)).with_context(|| path.display().to_string())?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_ext(
        &parsed,
        project,
        base,
        home.as_deref(),
        lk,
        profiles,
        scale,
        fleet,
    )
    .with_context(|| path.display().to_string())
}

/// `compose up --dry-run`: no daemon.
pub fn run_client(cmd: ComposeCommand) -> Result<()> {
    match cmd {
        ComposeCommand::Up {
            file,
            project,
            profile,
            scale,
            fleet,
            wait,
            dry_run,
            ..
        } => {
            let scale = ext::parse_scale(&scale)?;
            let r = load_scaled(
                file,
                project.as_deref(),
                &profiles_from_env(&profile),
                &scale,
                fleet.is_some(),
            )?;
            if let Some(inv) = fleet {
                return crate::mac_compose_fleet::up_command(
                    &r,
                    &inv,
                    dry_run,
                    wait.unwrap_or(900),
                );
            }
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            for s in &r.services {
                // A service with `build:` may have no image yet: `up` builds it.
                if s.build.is_none() {
                    store
                        .resolve(&s.spec.image)
                        .with_context(|| format!("service {:?}", s.service))?;
                }
            }
            for s in &r.services {
                for dep in &s.healthy_deps {
                    let image = &r
                        .services
                        .iter()
                        .find(|d| &d.service == dep)
                        .map(|d| d.spec.image.clone());
                    let built = r
                        .services
                        .iter()
                        .any(|d| &d.service == dep && d.build.is_some());
                    let has = built
                        || r.services.iter().any(|d| {
                            &d.service == dep
                                && matches!(
                                    d.spec.healthcheck,
                                    Some(crate::mac_lifecycle::HcSetting::Cmd(_))
                                )
                        })
                        || image
                            .as_deref()
                            .and_then(|i| store.resolve(i).ok())
                            .and_then(|d| mac_config::load(&d).ok())
                            .is_some_and(|c| c.healthcheck.is_some());
                    ensure!(
                        has,
                        "service {:?} waits for {dep:?} to be healthy, but {dep:?}'s image has no HEALTHCHECK (add one to its Macfile)",
                        s.service
                    );
                }
            }
            print!("{}", render_plan(&r));
            Ok(())
        }
        ComposeCommand::Config {
            t,
            profile,
            quiet,
            format,
            services,
        } => {
            let r = load_with(t.file, t.project.as_deref(), &profiles_from_env(&profile))?;
            if quiet {
                return Ok(());
            }
            if services {
                for s in &r.services {
                    println!("{}", s.service);
                }
                return Ok(());
            }
            print!("{}", render_config(&r, format == "json")?);
            Ok(())
        }
        ComposeCommand::Images { t, quiet } => {
            let r = load_with(t.file, t.project.as_deref(), &["*".to_string()])?;
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            let rows = image_rows(&r, &store.list()?, store.root());
            if quiet {
                for row in &rows {
                    println!("{}", row.image);
                }
            } else {
                print!("{}", render_images(&rows));
            }
            Ok(())
        }
        ComposeCommand::Pull {
            services,
            t,
            verify,
        } => {
            let r = load_with(t.file, t.project.as_deref(), &["*".to_string()])?;
            let targets = image_targets(&r, &services, false)?;
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            let extra = crate::mac_plus::PullExtra {
                verify,
                no_reuse: false,
            };
            let mut failed = Vec::new();
            for (svc, image, buildable) in &targets {
                eprintln!("pulling {image} (service {svc}) ...");
                if let Err(e) = block_on(crate::mac_plus::pull(&store, image, &extra)) {
                    if *buildable {
                        eprintln!(
                            "{svc}: could not pull {image} ({e:#}); it has `build:`, so `compose up` builds it"
                        );
                    } else {
                        eprintln!("{svc}: {e:#}");
                        failed.push(svc.as_str());
                    }
                }
            }
            ensure!(
                failed.is_empty(),
                "could not pull the image of: {}. Check the name and tag (`fluxctl mac compose images`) and, for a private registry, FLUXVM_REGISTRY_USER and FLUXVM_REGISTRY_PASSWORD",
                failed.join(", ")
            );
            Ok(())
        }
        ComposeCommand::Push {
            services,
            t,
            sign,
            licence_override,
        } => {
            let r = load_with(t.file, t.project.as_deref(), &["*".to_string()])?;
            let targets = image_targets(&r, &services, true)?;
            for (svc, image, _) in &targets {
                ensure!(
                    crate::mac_cmd::names_a_registry(image),
                    "service {svc:?}: the image {image:?} names no registry, so there is nowhere to push it. Set `image:` to a registry reference such as ghcr.io/acme/{image} in the compose file"
                );
            }
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            let extra = crate::mac_plus::PushExtra {
                sign,
                licence_override,
                label: vec![],
            };
            for (svc, image, _) in &targets {
                eprintln!("pushing {image} (service {svc}) ...");
                let target = crate::mac_cmd::remote_reference(image, None)?;
                block_on(crate::mac_plus::push(&store, image, &target, &extra))
                    .with_context(|| format!("service {svc:?}: push {image}"))?;
            }
            Ok(())
        }
        ComposeCommand::Down {
            file,
            project,
            fleet: Some(inv),
        } => {
            let (project, _) = load_existing(file, project.as_deref())?;
            crate::mac_compose_fleet::down_command(&project, &inv)
        }
        ComposeCommand::Ps {
            file,
            project,
            fleet: Some(inv),
            ..
        } => {
            let (project, _) = load_existing(file, project.as_deref())?;
            crate::mac_compose_fleet::ps_command(&project, &inv)
        }
        _ => bail!("not a client-side compose command"),
    }
}

/// Run a future from the synchronous client-side commands (the CLI runs on a multi-threaded runtime).
fn block_on<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// A service whose image exists only because `build:` makes it (no `image:` was written).
fn built_only(project: &str, s: &ServicePlan) -> bool {
    s.build.is_some() && s.spec.image == format!("{project}-{}:latest", s.service)
}

/// `(service, image, has build:)` for pull and push: the named services (all when none), each image once. A built-only service
/// is skipped for pull (`with_built` false) and included for push.
fn image_targets(
    r: &Resolved,
    only: &[String],
    with_built: bool,
) -> Result<Vec<(String, String, bool)>> {
    for o in only {
        ensure!(
            r.services.iter().any(|s| &s.service == o),
            "no service {o:?} in the compose file (services: {})",
            r.services
                .iter()
                .map(|s| s.service.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for s in &r.services {
        if !only.is_empty() && !only.contains(&s.service) {
            continue;
        }
        if !with_built && built_only(&r.project, s) {
            eprintln!(
                "{}: built locally from `build:`, nothing to pull (`compose up` builds it)",
                s.service
            );
            continue;
        }
        if out.iter().any(|(_, i, _)| i == &s.spec.image) {
            continue;
        }
        out.push((s.service.clone(), s.spec.image.clone(), s.build.is_some()));
    }
    ensure!(
        !out.is_empty(),
        "no image to work on: every selected service is built locally from `build:`"
    );
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
struct ImageRow {
    service: String,
    image: String,
    id: String,
    size: String,
}

/// One row per service: the store's digest and size, or `-` when the image is not in the store (yet).
fn image_rows(
    r: &Resolved,
    store: &[(String, crate::mac_store::Entry)],
    root: &Path,
) -> Vec<ImageRow> {
    r.services
        .iter()
        .map(|s| {
            let key = crate::mac_store::normalize(&s.spec.image).unwrap_or_default();
            let found = store.iter().find(|(k, _)| *k == key);
            let (id, size) = match found {
                Some((_, e)) => (
                    if e.digest.is_empty() {
                        e.dir.chars().take(12).collect()
                    } else {
                        e.digest
                            .trim_start_matches("sha256:")
                            .chars()
                            .take(12)
                            .collect()
                    },
                    crate::mac_cmd::human(crate::mac_cmd::dir_size(&root.join(&e.dir))),
                ),
                None => ("-".into(), "-".into()),
            };
            ImageRow {
                service: s.service.clone(),
                image: s.spec.image.clone(),
                id,
                size,
            }
        })
        .collect()
}

fn render_images(rows: &[ImageRow]) -> String {
    let mut out = format!("{:<14}  {:<34}  {:<12}  SIZE\n", "SERVICE", "IMAGE", "ID");
    for r in rows {
        out.push_str(&format!(
            "{:<14}  {:<34}  {:<12}  {}\n",
            r.service, r.image, r.id, r.size
        ));
    }
    if rows.iter().any(|r| r.id == "-") {
        out.push_str("(`-` = not in the local store: `fluxctl mac compose pull`, or `compose up` builds a `build:` service)\n");
    }
    out
}

/// The resolved file, for `compose config`.
pub fn render_config(r: &Resolved, json: bool) -> Result<String> {
    use serde_json::{Map, Value, json};
    let mut services = Map::new();
    for s in &r.services {
        let mut o = Map::new();
        o.insert("image".into(), json!(s.spec.image));
        if let Some(b) = &s.build {
            let mut bm = Map::new();
            bm.insert("context".into(), json!(b.context));
            if let Some(f) = &b.file {
                bm.insert("dockerfile".into(), json!(f));
            }
            if !b.args.is_empty() {
                bm.insert("args".into(), json!(b.args));
            }
            o.insert("build".into(), Value::Object(bm));
        }
        if !s.command.is_empty() {
            o.insert("command".into(), json!(s.command));
        }
        if !s.env.is_empty() {
            let mut env = Map::new();
            for (k, v) in &s.env {
                env.insert(k.clone(), json!(v));
            }
            o.insert("environment".into(), Value::Object(env));
        }
        if !s.spec.publish.is_empty() {
            o.insert("ports".into(), json!(s.spec.publish));
        }
        let vols: Vec<String> = s
            .spec
            .mounts
            .iter()
            .map(|m| {
                format!(
                    "{}:{}{}",
                    m.host.display(),
                    m.guest,
                    if m.read_only { ":ro" } else { "" }
                )
            })
            .collect();
        if !vols.is_empty() {
            o.insert("volumes".into(), json!(vols));
        }
        for (kind, key) in [
            (cfg::FileKind::Secret, "secrets"),
            (cfg::FileKind::Config, "configs"),
        ] {
            let list: Vec<Value> = s
                .files
                .iter()
                .filter(|f| f.kind == kind)
                .map(|f| json!({"source": f.name, "file": f.source, "target": f.target}))
                .collect();
            if !list.is_empty() {
                o.insert(key.into(), Value::Array(list));
            }
        }
        if let Some(w) = &s.workdir {
            o.insert("working_dir".into(), json!(w));
        }
        if !s.depends_on.is_empty() {
            let mut d = Map::new();
            for dep in &s.depends_on {
                let cond = if s.healthy_deps.contains(dep) {
                    "service_healthy"
                } else {
                    "service_started"
                };
                d.insert(dep.clone(), json!({"condition": cond}));
            }
            o.insert("depends_on".into(), Value::Object(d));
        }
        if !s.spec.labels.is_empty() {
            let mut l = Map::new();
            for (k, v) in &s.spec.labels {
                l.insert(k.clone(), json!(v));
            }
            o.insert("labels".into(), Value::Object(l));
        }
        if let Some(rs) = s.spec.restart {
            o.insert("restart".into(), json!(rs.as_str()));
        }
        if let Some(c) = s.spec.cpus {
            o.insert("cpus".into(), json!(c));
        }
        if let Some(m) = s.spec.memory_mib {
            o.insert("mem_limit".into(), json!(format!("{m}m")));
        }
        if let Some(u) = &s.user {
            o.insert("user".into(), json!(u));
        }
        services.insert(s.service.clone(), Value::Object(o));
    }
    let doc = json!({"name": r.project, "services": services});
    Ok(if json {
        format!("{}\n", serde_json::to_string_pretty(&doc)?)
    } else {
        serde_yaml::to_string(&doc)?
    })
}

/// The instance names for stop/start/top: the named services (all when none), in start order (reversed with `reverse`), keeping
/// the ones whose status `keep` accepts. A named service with no instance is an error that says how to create it.
fn pick_instances(
    vms: &[serde_json::Value],
    project: &str,
    services: &[String],
    order: &[String],
    reverse: bool,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let svc_of = |v: &serde_json::Value| v["labels"][SERVICE_LABEL].as_str().map(str::to_string);
    for s in services {
        ensure!(
            vms.iter().any(|v| svc_of(v).as_deref() == Some(s)),
            "service {s:?} has no instance in project {project:?} (known: {}); `fluxctl mac compose up` creates them",
            vms.iter().filter_map(svc_of).collect::<Vec<_>>().join(", ")
        );
    }
    let mut picked: Vec<(usize, String)> = vms
        .iter()
        .filter(|v| services.is_empty() || svc_of(v).is_some_and(|s| services.contains(&s)))
        .filter(|v| keep(crate::mac_inspect::status(v)))
        .filter_map(|v| {
            let name = v["name"].as_str()?.to_string();
            let pos = svc_of(v)
                .and_then(|s| order.iter().position(|o| *o == s))
                .unwrap_or(usize::MAX);
            Some((pos, name))
        })
        .collect();
    picked.sort();
    if reverse {
        picked.reverse();
    }
    Ok(picked.into_iter().map(|(_, n)| n).collect())
}

/// The 2-VM limit as it applies right now: `adding` more macOS VMs on top of those that hold a slot (running or paused).
fn check_slots(all: &[serde_json::Value], adding: usize, what: &str) -> Result<()> {
    let used = all
        .iter()
        .filter(|v| crate::mac_inspect::visible_by_default(v))
        .count();
    ensure!(
        used + adding <= MAX_SERVICES,
        "cannot {what}: it needs {adding} macOS VM slot(s) but {used} of {MAX_SERVICES} are in use (a Mac runs at most {MAX_SERVICES} macOS guests at once: Apple's licence). Free one with `fluxctl mac compose stop SERVICE` or `fluxctl mac stop NAME` (see `fluxctl mac ps -a`), then retry"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectRow {
    name: String,
    running: usize,
    stopped: usize,
    services: Vec<String>,
}

impl ProjectRow {
    /// `running(2)`, `exited(1)` or both, as Docker prints it.
    fn status(&self) -> String {
        let mut p = Vec::new();
        if self.running > 0 {
            p.push(format!("running({})", self.running));
        }
        if self.stopped > 0 {
            p.push(format!("exited({})", self.stopped));
        }
        p.join(", ")
    }
}

/// Group instances by project. Without `all`, a project whose instances are all stopped is left out.
fn project_rows(vms: &[serde_json::Value], all: bool) -> Vec<ProjectRow> {
    let mut m: BTreeMap<String, ProjectRow> = BTreeMap::new();
    for v in vms {
        let Some(p) = v["labels"][PROJECT_LABEL].as_str() else {
            continue;
        };
        let row = m.entry(p.to_string()).or_insert_with(|| ProjectRow {
            name: p.to_string(),
            running: 0,
            stopped: 0,
            services: vec![],
        });
        if crate::mac_inspect::visible_by_default(v) {
            row.running += 1;
        } else {
            row.stopped += 1;
        }
        if let Some(s) = v["labels"][SERVICE_LABEL].as_str() {
            row.services.push(s.to_string());
        }
    }
    m.into_values()
        .filter(|r| all || r.running > 0)
        .map(|mut r| {
            r.services.sort();
            r
        })
        .collect()
}

fn render_projects(rows: &[ProjectRow]) -> String {
    let mut out = format!("{:<24}  {:<24}  SERVICES\n", "NAME", "STATUS");
    for r in rows {
        out.push_str(&format!(
            "{:<24}  {:<24}  {}\n",
            r.name,
            r.status(),
            r.services.join(", ")
        ));
    }
    out
}

/// The `compose cp` argument with the service name replaced by its instance name; a local path stays as it is.
fn cp_argument(arg: &str, instances: &BTreeMap<String, String>, project: &str) -> Result<String> {
    match crate::mac_cp::parse_endpoint(arg)? {
        crate::mac_cp::Endpoint::Local(_) => Ok(arg.to_string()),
        crate::mac_cp::Endpoint::Guest { name, path } => {
            let inst = instances.get(&name).with_context(|| {
                format!(
                    "service {name:?} has no instance in project {project:?} (known: {}); `fluxctl mac compose up` creates them",
                    instances.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            })?;
            Ok(format!("{inst}:{path}"))
        }
    }
}

/// `command` with the link commands for custom secret and config targets in front of it.
fn with_links(command: Option<String>, links: &[(String, String)]) -> Option<String> {
    let pre = cfg::link_prefix(links);
    match (command, pre.is_empty()) {
        (c, true) => c,
        (Some(c), false) => Some(format!("{pre}{c}")),
        (None, false) => Some(pre.trim_end_matches("; ").to_string()),
    }
}

/// The one-off VM `compose run` makes: the service's spec without published ports or restart policy. It is labelled as part of
/// the project (service `SERVICE-run-SUFFIX`), so `compose ps -a` shows it and `compose down` removes it.
fn oneoff_spec(project: &str, s: &ServicePlan, suffix: &str) -> LaunchSpec {
    let mut spec = s.spec.clone();
    spec.name = format!("{project}-{}-run-{suffix}", s.service);
    spec.publish = vec![];
    spec.restart = None;
    spec.project = Some((project.to_string(), format!("{}-run-{suffix}", s.service)));
    spec
}

/// `compose run` does not start dependencies (that would break the 2-VM limit): each must already run.
fn check_deps_running(
    existing: &[serde_json::Value],
    s: &ServicePlan,
    project: &str,
) -> Result<()> {
    for d in &s.depends_on {
        let up = existing.iter().any(|v| {
            v["labels"][SERVICE_LABEL].as_str() == Some(d)
                && crate::mac_inspect::status(v) == "running"
        });
        ensure!(
            up,
            "service {:?} depends_on {d:?}, which is not running in project {project:?}. `compose run` does not start dependencies (that would break the two-VM limit): run `fluxctl mac compose up` or `compose start {d}` first",
            s.service
        );
    }
    Ok(())
}

pub(crate) async fn project_instances(
    r: &crate::remote::Remote,
    project: &str,
) -> Result<Vec<serde_json::Value>> {
    let sel = format!(
        "{}=instance,{PROJECT_LABEL}={project}",
        crate::mac_inspect::INSTANCE_LABEL
    );
    r.list_vms(Some(&sel)).await
}

/// The `mac build` a service's `build:` stands for.
fn build_command(tag: &str, b: &BuildPlan, wait: u64) -> crate::mac_cmd::MacCommand {
    crate::mac_cmd::MacCommand::Build {
        file: b.file.clone(),
        tag: tag.to_string(),
        dry_run: false,
        wait,
        build_arg: b.args.clone(),
        no_cache: false,
        label: Vec::new(),
        secret: Vec::new(),
        ssh: Vec::new(),
        platform: None,
        target: None,
        context: b.context.clone(),
    }
}

/// The instance of `service` in `project`.
async fn service_instance(
    r: &crate::remote::Remote,
    project: &str,
    service: &str,
) -> Result<serde_json::Value> {
    project_instances(r, project)
        .await?
        .into_iter()
        .find(|v| v["labels"][SERVICE_LABEL].as_str() == Some(service))
        .with_context(|| format!("no instance for service {service:?} in project {project:?} (`fluxctl mac compose ps -a`)"))
}

/// Does the service have a healthcheck to wait for: its own `healthcheck:` (not disabled), else the image's?
fn has_healthcheck(store: &crate::mac_store::Store, s: &ServicePlan) -> Result<bool> {
    use crate::mac_lifecycle::HcSetting;
    Ok(match &s.spec.healthcheck {
        Some(HcSetting::Cmd(_)) => true,
        Some(HcSetting::Off) => false,
        None => mac_config::load(&store.resolve(&s.spec.image)?)?
            .healthcheck
            .is_some(),
    })
}

/// Build the images of services with `build:`: all with `--build`, otherwise only those missing from the store.
async fn build_images(
    r: &crate::remote::Remote,
    store: &crate::mac_store::Store,
    plan: &Resolved,
    build: bool,
    no_build: bool,
    wait: u64,
) -> Result<()> {
    let mut built: BTreeSet<&str> = BTreeSet::new();
    for s in &plan.services {
        let Some(b) = &s.build else { continue };
        if !built.insert(&s.spec.image) {
            continue; // copies of one service share its image
        }
        let present = store.resolve(&s.spec.image).is_ok();
        if !needs_build(build, no_build, present)
            .with_context(|| format!("service {:?}: image {}", s.service, s.spec.image))?
        {
            continue;
        }
        eprintln!("building {} from {} ...", s.spec.image, b.context.display());
        Box::pin(crate::mac_cmd::run_daemon(
            r,
            build_command(&s.spec.image, b, wait),
        ))
        .await
        .with_context(|| format!("service {:?}: build", s.service))?;
    }
    Ok(())
}

/// The plain-English error for a `condition: service_healthy` dependency that did not become healthy in time (or at all).
fn gate_error(
    service: &str,
    dep: &str,
    instance: &str,
    timeout: u64,
    cause: &anyhow::Error,
) -> anyhow::Error {
    let waited = if timeout == 0 {
        "forever".to_string()
    } else {
        format!("{timeout}s")
    };
    crate::mac_errors::coded(
        format!(
            "service {service:?} was not started because {dep:?} (instance {instance}, depends_on with condition: service_healthy) did not become healthy (waited {waited}): {cause:#}. Services started before it stay up: `fluxctl mac compose down` removes them"
        ),
        "MAC_COMPOSE_DEP_UNHEALTHY",
    )
}

/// Create and start one instance: wait for what it depends on, start the VM, record the owner, run its command (in the
/// foreground for a one-shot that others wait for, which is then stopped to free its slot).
async fn start_service(
    r: &crate::remote::Remote,
    store: &crate::mac_store::Store,
    plan: &Resolved,
    s: &ServicePlan,
    wait: u64,
    health_timeout: u64,
) -> Result<()> {
    use crate::mac_instance::{GuestJob, launch, run_in_guest};
    for dep in &s.healthy_deps {
        for d in plan.services.iter().filter(|d| &d.service == dep) {
            eprintln!("waiting for {} to be healthy ...", d.spec.name);
            crate::mac_health::wait_instance_healthy(r, &d.spec.name, health_timeout)
                .await
                .map_err(|e| gate_error(&s.service, dep, &d.spec.name, health_timeout, &e))?;
        }
    }
    eprintln!("starting {} ...", s.spec.name);
    let cfg = mac_config::load(&store.resolve(&s.spec.image)?)?;
    let mut spec = s.spec.clone();
    let staged = cfg::stage_files(
        &cfg::stage_root(),
        &plan.project,
        &s.service,
        &s.files,
        spec.mounts.len(),
    )?;
    spec.mounts.extend(staged.mounts);
    let command = with_links(
        mac_config::effective_command(&cfg, &s.command, &s.env, s.workdir.as_deref()),
        &staged.links,
    );
    if s.run_to_completion && command.is_none() {
        return Err(crate::mac_errors::coded(
            format!(
                "service {:?} is waited for with service_completed_successfully but has no `command:` and its image has no CMD, so there is nothing to run to the end",
                s.service
            ),
            "MAC_COMPOSE_DEP_FAILED",
        ));
    }
    let id = launch(r, store, &spec).await?;
    // Ownership for `mac reap --owner`: who brought the project up and when (never an expiry).
    let owner = crate::mac_reap::stamp(
        Some(&crate::mac_reap::current_user()),
        None,
        crate::mac_health::now_unix(),
    );
    r.call(
        reqwest::Method::PATCH,
        &format!("/v1/vms/{id}"),
        Some(owner),
    )
    .await
    .with_context(|| format!("could not record the owner of {}", s.spec.name))?;
    if command.is_some() || s.has_dependents || !spec.mounts.is_empty() {
        let job = GuestJob {
            user: s
                .user
                .clone()
                .or(cfg.user.clone())
                .unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.into()),
            wait,
            command,
            mounts: spec.mounts.clone(),
            detach: !s.run_to_completion,
            ..Default::default()
        };
        let ran = run_in_guest(r, &id, &job).await;
        if s.run_to_completion {
            if let Err(e) = ran {
                return Err(crate::mac_errors::coded(
                    format!(
                        "service {:?} did not finish successfully, so what waits for it is not started: {e:#} (its VM {} stays for you to look at: `fluxctl mac logs {}`; `fluxctl mac compose down` removes it)",
                        s.service, s.spec.name, s.spec.name
                    ),
                    "MAC_COMPOSE_DEP_FAILED",
                ));
            }
            // Done: stop it so its macOS slot is free for the services that waited for it.
            crate::mac_more::run_daemon(
                r,
                crate::mac_more::MacMore::Stop {
                    names: vec![s.spec.name.clone()],
                    time: None,
                },
            )
            .await
            .with_context(|| format!("{} finished but could not be stopped", s.spec.name))?;
            eprintln!("{} finished successfully and was stopped", s.spec.name);
            return Ok(());
        }
        ran.with_context(|| {
            format!(
                "service {:?} did not come up (instances left running: `fluxctl mac compose down`)",
                s.service
            )
        })?;
    }
    println!("{}", s.spec.name);
    Ok(())
}

/// Bring the project up for `compose watch`: build what is missing and start what is not there yet.
pub(crate) async fn up_project(
    r: &crate::remote::Remote,
    plan: &Resolved,
    wait: u64,
) -> Result<()> {
    let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
    let existing = project_instances(r, &plan.project).await?;
    build_images(r, &store, plan, false, false, wait).await?;
    for s in &plan.services {
        if existing
            .iter()
            .any(|v| v["name"].as_str() == Some(&s.spec.name))
        {
            eprintln!("{} is already there", s.spec.name);
            continue;
        }
        start_service(r, &store, plan, s, wait, 600).await?;
    }
    Ok(())
}

/// A rebuild rule: build the image again, delete the instance (a Mac runs only two macOS VMs) and start it from the new image.
pub(crate) async fn rebuild_service(
    r: &crate::remote::Remote,
    store: &crate::mac_store::Store,
    sp: &ServicePlan,
    wait: u64,
) -> Result<()> {
    let b = sp
        .build
        .as_ref()
        .with_context(|| format!("service {:?} has no `build:` to run", sp.service))?;
    Box::pin(crate::mac_cmd::run_daemon(
        r,
        build_command(&sp.spec.image, b, wait),
    ))
    .await?;
    if let Ok(vm) = crate::mac_cmd::find_instance(r, &sp.spec.name).await {
        crate::mac_cmd::delete_instance(r, vm["id"].as_str().context("no id")?).await?;
    }
    let plan = Resolved {
        project: sp
            .spec
            .project
            .as_ref()
            .map(|p| p.0.clone())
            .unwrap_or_default(),
        peak_slots: 1,
        services: vec![sp.clone()],
    };
    start_service(r, store, &plan, sp, wait, 600).await
}

/// `compose up|down|ps` against the daemon. Never run against a real daemon: see the module notes.
pub async fn run_daemon(r: &crate::remote::Remote, cmd: ComposeCommand) -> Result<()> {
    use crate::mac_instance::{GuestJob, launch, run_in_guest};
    match cmd {
        ComposeCommand::Up {
            file,
            project,
            wait,
            dry_run,
            build,
            no_build,
            wait_healthy,
            health_timeout,
            profile,
            scale,
            fleet: _,
        } => {
            // `--wait` is docker's spelling of `--wait-healthy`; its optional value is the SSH wait.
            let wait_healthy = wait_healthy || wait.is_some();
            let wait = wait.unwrap_or(900);
            let scale = ext::parse_scale(&scale)?;
            let plan = load_scaled(
                file,
                project.as_deref(),
                &profiles_from_env(&profile),
                &scale,
                false,
            )?;
            ensure!(!dry_run, "internal: dry-run is client side");
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            let existing = project_instances(r, &plan.project).await?;
            for s in &plan.services {
                ensure!(
                    !existing
                        .iter()
                        .any(|v| v["name"].as_str() == Some(&s.spec.name)),
                    "{} already exists: `fluxctl mac compose down` first, or remove it with `fluxctl mac rm {}`",
                    s.spec.name,
                    s.spec.name
                );
            }
            eprint!("{}", render_plan(&plan));
            build_images(r, &store, &plan, build, no_build, wait).await?;
            eprintln!(
                "note: at most {MAX_SERVICES} macOS VMs run on one Mac; the daemon refuses (409) if another macOS VM holds a slot"
            );
            let mut started: Vec<String> = Vec::new();
            for s in &plan.services {
                if let Err(e) = start_service(r, &store, &plan, s, wait, health_timeout).await {
                    if !started.is_empty() {
                        eprintln!(
                            "{} already started: `fluxctl mac compose down` removes them",
                            started.join(", ")
                        );
                    }
                    return Err(e);
                }
                started.push(s.spec.name.clone());
            }
            if wait_healthy {
                for s in &plan.services {
                    // Only a service with a healthcheck can be waited for; the others are up once SSH answered.
                    if !has_healthcheck(&store, s)? {
                        continue;
                    }
                    eprintln!("waiting for {} to be healthy ...", s.spec.name);
                    let waited =
                        crate::mac_health::wait_instance_healthy(r, &s.spec.name, health_timeout)
                            .await
                            .with_context(|| format!("service {:?} did not become healthy (instances stay: `fluxctl mac compose down`)", s.service))?;
                    eprintln!("{} is healthy (after {}s)", s.spec.name, waited.as_secs());
                }
            }
            Ok(())
        }
        ComposeCommand::Watch {
            t,
            no_up,
            wait,
            interval,
            polls,
            profile,
            scale,
        } => {
            let scale = ext::parse_scale(&scale)?;
            let plan = load_scaled(
                t.file,
                t.project.as_deref(),
                &profiles_from_env(&profile),
                &scale,
                false,
            )?;
            crate::mac_compose_watch::watch(r, plan, wait, no_up, interval, polls).await
        }
        ComposeCommand::Down { file, project, .. } => {
            let (project, mut order) = load_existing(file.clone(), project.as_deref())?;
            order.reverse();
            let mut vms = project_instances(r, &project).await?;
            // Staged secrets and configs go with the project.
            std::fs::remove_dir_all(cfg::stage_root().join(&project)).ok();
            ensure!(
                !vms.is_empty(),
                "no instances for project {project:?}; see the projects on this Mac with `fluxctl mac compose ls -a`"
            );
            vms.sort_by_key(|v| {
                order
                    .iter()
                    .position(|s| v["labels"][SERVICE_LABEL].as_str() == Some(s))
                    .unwrap_or(usize::MAX)
            });
            for v in &vms {
                let name = v["name"].as_str().unwrap_or("?");
                crate::mac_cmd::delete_instance(r, v["id"].as_str().context("no id")?).await?;
                println!("removed {name}");
            }
            Ok(())
        }
        ComposeCommand::Logs {
            service,
            t,
            follow,
            tail,
        } => {
            let (project, _) = load_existing(t.file, t.project.as_deref())?;
            let vm = service_instance(r, &project, &service).await?;
            let name = vm["name"].as_str().unwrap_or_default().to_string();
            crate::mac_more::run_daemon(
                r,
                crate::mac_more::MacMore::Logs {
                    name,
                    follow,
                    tail,
                    since: None,
                    timestamps: false,
                },
            )
            .await
        }
        ComposeCommand::Exec {
            service,
            t,
            user,
            cmd,
        } => {
            let (project, _) = load_existing(t.file, t.project.as_deref())?;
            let vm = service_instance(r, &project, &service).await?;
            let name = vm["name"].as_str().unwrap_or_default().to_string();
            crate::mac_more::exec(r, name, user, cmd, crate::mac_more::ExecOpts::default()).await
        }
        ComposeCommand::Restart { services, t } => {
            let (project, _) = load_existing(t.file, t.project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let names: Vec<String> = vms
                .iter()
                .filter(|v| {
                    services.is_empty()
                        || services
                            .iter()
                            .any(|s| v["labels"][SERVICE_LABEL].as_str() == Some(s))
                })
                .filter_map(|v| v["name"].as_str().map(String::from))
                .collect();
            ensure!(
                !names.is_empty(),
                "no matching instances in project {project:?}"
            );
            crate::mac_more::run_daemon(r, crate::mac_more::MacMore::Restart { names, time: None })
                .await
        }
        ComposeCommand::Stop { services, t } => {
            let (project, order) = load_existing(t.file, t.project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let names = pick_instances(&vms, &project, &services, &order, true, |st| {
                matches!(st, "running" | "paused")
            })?;
            if names.is_empty() {
                println!("nothing to stop: no instance of project {project:?} is running");
                return Ok(());
            }
            crate::mac_more::run_daemon(r, crate::mac_more::MacMore::Stop { names, time: None })
                .await
        }
        ComposeCommand::Start { services, t } => {
            let (project, order) = load_existing(t.file, t.project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let names = pick_instances(&vms, &project, &services, &order, false, |st| {
                matches!(st, "stopped" | "failed")
            })?;
            if names.is_empty() {
                println!("nothing to start: no instance of project {project:?} is stopped");
                return Ok(());
            }
            let all = r.list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR)).await?;
            check_slots(&all, names.len(), "start the stopped instances")?;
            crate::mac_more::run_daemon(r, crate::mac_more::MacMore::Start { names }).await
        }
        ComposeCommand::Ls { all, quiet, json } => {
            let vms = r.list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR)).await?;
            let rows = project_rows(&vms, all);
            if json {
                let doc: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|p| serde_json::json!({"name": p.name, "status": p.status(), "services": p.services}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else if quiet {
                for p in &rows {
                    println!("{}", p.name);
                }
            } else if rows.is_empty() {
                println!(
                    "no compose projects are running on this Mac (`-a` includes stopped ones; `fluxctl mac compose up` starts one)"
                );
            } else {
                print!("{}", render_projects(&rows));
            }
            Ok(())
        }
        ComposeCommand::Top {
            services,
            t,
            user,
            ps_args,
        } => {
            let (project, order) = load_existing(t.file, t.project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let names = pick_instances(&vms, &project, &services, &order, false, |st| {
                st == "running"
            })?;
            ensure!(
                !names.is_empty(),
                "no instance of project {project:?} is running; `fluxctl mac compose start` or `compose up`"
            );
            for n in names {
                println!("{n}");
                crate::mac_more::run_daemon(
                    r,
                    crate::mac_more::MacMore::Top {
                        name: n,
                        user: user.clone(),
                        ps_args: ps_args.clone(),
                    },
                )
                .await?;
            }
            Ok(())
        }
        ComposeCommand::Cp { src, dst, t, user } => {
            let (project, _) = load_existing(t.file, t.project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let by_service: BTreeMap<String, String> = vms
                .iter()
                .filter_map(|v| {
                    Some((
                        v["labels"][SERVICE_LABEL].as_str()?.to_string(),
                        v["name"].as_str()?.to_string(),
                    ))
                })
                .collect();
            let (src, dst) = (
                cp_argument(&src, &by_service, &project)?,
                cp_argument(&dst, &by_service, &project)?,
            );
            crate::mac_more::run_daemon(r, crate::mac_more::MacMore::Cp { src, dst, user }).await
        }
        ComposeCommand::Run {
            service,
            t,
            rm,
            env,
            workdir,
            user,
            ssh_timeout,
            cmd,
        } => {
            let plan = load_with(t.file, t.project.as_deref(), &["*".to_string()])?;
            let s = plan
                .services
                .iter()
                .find(|s| s.service == service)
                .with_context(|| {
                    format!(
                        "no service {service:?} in the compose file (services: {})",
                        plan.services
                            .iter()
                            .map(|s| s.service.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            let cfg_img = mac_config::load(&store.resolve(&s.spec.image).with_context(|| {
                format!(
                    "service {service:?}: image {} is not in the store; `fluxctl mac compose pull`, or `compose up` builds a `build:` service",
                    s.spec.image
                )
            })?)?;
            let all = r.list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR)).await?;
            check_slots(
                &all,
                1,
                &format!("run a one-off VM for service {service:?}"),
            )?;
            check_deps_running(
                &project_instances(r, &plan.project).await?,
                s,
                &plan.project,
            )?;
            let suffix = format!("{:x}", crate::mac_health::now_unix() % 0xfffff);
            let mut spec = oneoff_spec(&plan.project, s, &suffix);
            let staged = cfg::stage_files(
                &cfg::stage_root(),
                &plan.project,
                &format!("{service}-run-{suffix}"),
                &s.files,
                spec.mounts.len(),
            )?;
            spec.mounts.extend(staged.mounts);
            let mut envs = s.env.clone();
            envs.extend(crate::mac_instance::collect_env(&env, &[], |k| {
                std::env::var(k).ok()
            })?);
            let args = if cmd.is_empty() {
                s.command.clone()
            } else {
                cmd
            };
            let command = mac_config::effective_command(
                &cfg_img,
                &args,
                &envs,
                workdir.as_deref().or(s.workdir.as_deref()),
            );
            ensure!(
                command.is_some(),
                "service {service:?} has no `command:` and the image has no CMD: say what to run, `fluxctl mac compose run {service} -- CMD`"
            );
            let command = with_links(command, &staged.links);
            eprintln!("starting one-off {} ...", spec.name);
            let id = launch(r, &store, &spec).await?;
            let job = GuestJob {
                user: user
                    .or(s.user.clone())
                    .or(cfg_img.user.clone())
                    .unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.into()),
                wait: ssh_timeout,
                command,
                mounts: spec.mounts.clone(),
                detach: false,
                tty: crate::mac_exec::Tty::Force,
                shell: false,
            };
            let result = run_in_guest(r, &id, &job).await;
            if rm {
                crate::mac_cmd::delete_instance(r, &id).await.ok();
            } else {
                eprintln!(
                    "the one-off VM {} is still running (`fluxctl mac rm {}`, or `compose down` removes it)",
                    spec.name, spec.name
                );
            }
            result.map(|_| ())
        }
        ComposeCommand::Config { .. }
        | ComposeCommand::Images { .. }
        | ComposeCommand::Pull { .. }
        | ComposeCommand::Push { .. } => run_client(cmd),
        ComposeCommand::Ps {
            file, project, all, ..
        } => {
            let (project, _) = load_existing(file, project.as_deref())?;
            let vms = project_instances(r, &project).await?;
            let rows: Vec<&serde_json::Value> = vms.iter().collect();
            let rows = crate::mac_inspect::select(&rows, all, &[]);
            print!(
                "{}",
                crate::mac_inspect::render_format(
                    &rows,
                    "table {{.Service}}\t{{.Name}}\t{{.Status}}\t{{.IP}}\t{{.Ports}}"
                )?
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(y: &str) -> Result<Resolved> {
        resolve(
            &parse(y)?,
            None,
            Path::new("/proj/My App"),
            Some(Path::new("/Users/me")),
            |_| None,
        )
    }

    #[test]
    fn a_two_service_file_is_ordered_by_depends_on() {
        let r = res(
            "name: ci\nservices:\n  tests:\n    image: xcode:15\n    depends_on: [db]\n    command: [xcodebuild, test]\n    environment:\n      DB: 1\n      MODE: fast\n  db:\n    image: pg:16\n    ports: ['5432:5432']\n    restart: always\n    cpus: 4\n    mem_limit: 8g\n",
        )
        .unwrap();
        assert_eq!(r.project, "ci");
        let names: Vec<&str> = r.services.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(names, ["db", "tests"]);
        let db = &r.services[0];
        assert_eq!(db.spec.name, "ci-db");
        assert!(db.has_dependents);
        assert_eq!(db.spec.publish, ["5432:5432"]);
        assert_eq!(db.spec.cpus, Some(4));
        assert_eq!(db.spec.memory_mib, Some(8192));
        assert_eq!(db.spec.restart, Some(Restart::Always));
        assert_eq!(db.spec.project, Some(("ci".into(), "db".into())));
        let t = &r.services[1];
        assert_eq!(t.depends_on, ["db"]);
        assert_eq!(t.command, ["xcodebuild", "test"]);
        assert_eq!(
            t.env,
            [
                ("DB".to_string(), "1".to_string()),
                ("MODE".to_string(), "fast".to_string())
            ]
        );
        assert!(!t.has_dependents);
        let plan = render_plan(&r);
        assert!(
            plan.contains("project ci (2 of 2 macOS VMs)")
                && plan.contains("2. tests = ci-tests from xcode:15 after db"),
            "{plan}"
        );
    }

    #[test]
    fn service_healthy_is_recorded_and_other_conditions_are_plain_starts() {
        let r = res("services:\n  web:\n    image: w:1\n    depends_on:\n      db:\n        condition: service_healthy\n  db:\n    image: d:1\n").unwrap();
        let web = r.services.iter().find(|s| s.service == "web").unwrap();
        assert_eq!(web.healthy_deps, ["db"]);
        assert!(render_plan(&r).contains("after db (healthy)"));
        let r = res("services:\n  web:\n    image: w:1\n    depends_on:\n      db:\n        condition: service_started\n  db:\n    image: d:1\n").unwrap();
        assert!(r.services.iter().all(|s| s.healthy_deps.is_empty()));
    }

    #[test]
    fn build_services_default_their_image_and_resolve_against_the_file_directory() {
        let r = res("name: ci\nservices:\n  app:\n    build:\n      context: ./app\n      dockerfile: Macfile.ci\n      args:\n        MODE: fast\n    command: ['./run']\n").unwrap();
        let app = &r.services[0];
        assert_eq!(app.spec.image, "ci-app:latest");
        let b = app.build.as_ref().unwrap();
        assert_eq!(b.context, Path::new("/proj/My App/./app"));
        assert_eq!(
            b.file.as_deref(),
            Some(Path::new("/proj/My App/./app/Macfile.ci"))
        );
        assert_eq!(b.args, ["MODE=fast"]);
        let r = res("services:\n  a:\n    build: ./x\n    image: mine:1\n").unwrap();
        assert_eq!(r.services[0].spec.image, "mine:1");
        assert_eq!(r.services[0].build.as_ref().unwrap().args.len(), 0);
        let e = res("services:\n  a:\n    command: x\n").unwrap_err();
        assert!(format!("{e:#}").contains("image"), "{e:#}");
    }

    #[test]
    fn profiles_select_services_and_the_two_vm_limit_counts_the_selected() {
        let y = "services:\n  a:\n    image: a:1\n  b:\n    image: b:1\n  dbg:\n    image: d:1\n    profiles: [debug]\n";
        let f = parse(y).unwrap();
        let go = |p: &[&str]| {
            let p: Vec<String> = p.iter().map(|s| s.to_string()).collect();
            resolve_with(&f, None, Path::new("/p"), None, |_| None, &p)
        };
        assert_eq!(go(&[]).unwrap().services.len(), 2);
        let e = go(&["debug"]).unwrap_err().to_string();
        assert!(
            e.contains("at most 2 macOS VMs") && e.contains("--profile"),
            "{e}"
        );
        assert!(go(&["*"]).is_err());
        let only = parse("services:\n  a:\n    image: a:1\n    profiles: [x]\n").unwrap();
        let e = resolve_with(&only, None, Path::new("/p"), None, |_| None, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("no service is selected"), "{e}");
    }

    #[test]
    fn the_new_subcommands_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            c: ComposeCommand,
        }
        let p = |a: &[&str]| T::try_parse_from(std::iter::once("t").chain(a.iter().copied()));
        assert!(matches!(
            p(&["logs", "db", "--tail", "5", "-p", "ci"]).unwrap().c,
            ComposeCommand::Logs { tail: 5, .. }
        ));
        assert!(matches!(
            p(&["exec", "db", "--", "ls"]).unwrap().c,
            ComposeCommand::Exec { .. }
        ));
        assert!(matches!(
            p(&["restart"]).unwrap().c,
            ComposeCommand::Restart { .. }
        ));
        assert!(matches!(
            p(&["up", "--build", "--profile", "debug"]).unwrap().c,
            ComposeCommand::Up { build: true, .. }
        ));
        assert!(matches!(
            p(&[
                "up",
                "--no-build",
                "--wait-healthy",
                "--health-timeout",
                "5"
            ])
            .unwrap()
            .c,
            ComposeCommand::Up {
                no_build: true,
                wait_healthy: true,
                health_timeout: 5,
                ..
            }
        ));
        assert!(p(&["up", "--build", "--no-build"]).is_err());
        assert!(p(&["exec", "db"]).is_err());
    }

    #[test]
    fn json_files_work_and_the_project_defaults_to_the_directory() {
        let r = res(r#"{"services": {"one": {"image": "a:1", "command": "make && ls", "depends_on": {"two": {"condition": "service_started"}}}, "two": {"image": "b:1"}}}"#).unwrap();
        assert_eq!(r.project, "my-app");
        assert_eq!(r.services[0].service, "two");
        assert_eq!(r.services[1].command, ["make && ls"]);
        assert_eq!(project_from_dir(Path::new("/x/@@@")), "mac");
    }

    #[test]
    fn more_than_two_services_is_refused_loudly() {
        let e = res("services:\n  a: {image: x}\n  b: {image: x}\n  c: {image: x}\n")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("3 services") && e.contains("at most 2 macOS VMs") && e.contains("licence"),
            "{e}"
        );
        assert!(res("services:\n  a: {image: x}\n  b: {image: x}\n").is_ok());
    }

    #[test]
    fn dependency_problems_are_named() {
        let e =
            res("services:\n  a: {image: x, depends_on: [b]}\n  b: {image: x, depends_on: [a]}\n")
                .unwrap_err()
                .to_string();
        assert!(e.contains("cycle"), "{e}");
        let e = res("services:\n  a: {image: x, depends_on: [zzz]}\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("zzz") && e.contains("not defined"), "{e}");
        let e = res("services:\n  a: {image: x, depends_on: [a]}\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("itself"), "{e}");
    }

    #[test]
    fn unsupported_keys_and_bad_values_are_errors_not_ignored() {
        let e = format!(
            "{:#}",
            res("services:\n  a:\n    image: x\n    networks: [n]\n").unwrap_err()
        );
        assert!(e.contains("networks"), "{e}");
        // `build:` is supported now, but a service still needs an image or a build.
        let e = format!(
            "{:#}",
            res("services:\n  a:\n    command: [\"x\"]\n").unwrap_err()
        );
        assert!(e.contains("image"), "{e}");
        let e = format!(
            "{:#}",
            res("services:\n  a: {image: x, ports: ['nope']}\n").unwrap_err()
        );
        assert!(e.contains("ports"), "{e}");
        let e = format!(
            "{:#}",
            res("services:\n  a: {image: x, volumes: ['named:/d']}\n").unwrap_err()
        );
        assert!(e.contains("volumes") && e.contains("named volumes"), "{e}");
        let e = format!(
            "{:#}",
            res("services:\n  a: {image: x, restart: maybe}\n").unwrap_err()
        );
        assert!(e.contains("restart"), "{e}");
        let e = format!(
            "{:#}",
            res("services:\n  a: {image: x, cpus: 0}\n").unwrap_err()
        );
        assert!(e.contains("cpus"), "{e}");
        let e = format!("{:#}", res("services:\n  A: {image: x}\n").unwrap_err());
        assert!(e.contains("service name"), "{e}");
        assert!(
            res("services: {}\n")
                .unwrap_err()
                .to_string()
                .contains("no services")
        );
        assert!(res("not: [valid").is_err());
    }

    #[test]
    fn volumes_resolve_against_the_file_directory_and_ports_may_be_numbers() {
        let r = res("services:\n  a:\n    image: x\n    volumes: ['./src:/src:ro', '~/cache:/cache']\n    ports: [8080]\n    labels: {team: ci}\n").unwrap_err();
        // 8080 alone is not HOST:GUEST.
        assert!(format!("{r:#}").contains("ports"));
        let r = res("services:\n  a:\n    image: x\n    volumes: ['./src:/src:ro', '~/cache:/cache']\n    labels: [team=ci]\n").unwrap();
        let m = &r.services[0].spec.mounts;
        assert_eq!(m[0].host, PathBuf::from("/proj/My App/./src"));
        assert!(m[0].read_only);
        assert_eq!(m[1].host, PathBuf::from("/Users/me/cache"));
        assert_eq!(
            r.services[0].spec.labels,
            [("team".to_string(), "ci".to_string())]
        );
    }

    #[test]
    fn env_files_and_shell_passthrough() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.env"), "FROM_FILE=1\nHOST_VAR\n").unwrap();
        let y = "services:\n  a:\n    image: x\n    env_file: a.env\n    environment: [FROM_FILE=2, BARE]\n";
        let r = resolve(&parse(y).unwrap(), None, d.path(), None, |k| {
            (k == "HOST_VAR" || k == "BARE").then(|| "h".to_string())
        })
        .unwrap();
        assert_eq!(
            r.services[0].env,
            vec![
                ("FROM_FILE".to_string(), "1".to_string()),
                ("HOST_VAR".to_string(), "h".to_string()),
                ("FROM_FILE".to_string(), "2".to_string()),
                ("BARE".to_string(), "h".to_string()),
            ]
        );
        let missing = resolve(
            &parse("services:\n  a: {image: x, env_file: nope.env}\n").unwrap(),
            None,
            d.path(),
            None,
            |_| None,
        );
        assert!(format!("{:#}", missing.unwrap_err()).contains("env_file"));
    }

    #[test]
    fn start_order_is_stable_and_alphabetical_among_equals() {
        let mut d: BTreeMap<String, Vec<String>> = BTreeMap::new();
        d.insert("b".into(), vec![]);
        d.insert("a".into(), vec!["b".into()]);
        assert_eq!(start_order(&d).unwrap(), ["b", "a"]);
        d.insert("a".into(), vec![]);
        assert_eq!(start_order(&d).unwrap(), ["a", "b"]);
    }

    #[test]
    fn unsupported_conditions_and_dependencies_on_disabled_profiles_are_refused_with_a_reason() {
        let r = res("services:\n  web:\n    image: w:1\n    depends_on:\n      db:\n        condition: service_completed_successfully\n  db:\n    image: d:1\n    command: ./once\n").unwrap();
        let db = r.services.iter().find(|s| s.service == "db").unwrap();
        assert!(db.run_to_completion, "what is waited for runs to the end");
        let e = res("services:\n  web:\n    image: w:1\n    depends_on:\n      db:\n        condition: nope\n  db:\n    image: d:1\n").unwrap_err().to_string();
        assert!(e.contains("unknown condition"), "{e}");
        let e = res("services:\n  web:\n    image: w:1\n    depends_on: [dbg]\n  dbg:\n    image: d:1\n    profiles: [debug]\n").unwrap_err().to_string();
        assert!(e.contains("--profile debug"), "{e}");
    }

    #[test]
    fn needs_build_follows_the_flags_and_the_store() {
        assert!(needs_build(true, false, true).unwrap());
        assert!(needs_build(false, false, false).unwrap());
        assert!(!needs_build(false, false, true).unwrap());
        assert!(!needs_build(false, true, true).unwrap());
        let e = needs_build(false, true, false).unwrap_err().to_string();
        assert!(e.contains("--no-build"), "{e}");
    }

    #[test]
    fn only_dry_run_is_client_side() {
        let up = |dry_run| ComposeCommand::Up {
            file: None,
            project: None,
            wait: Some(1),
            dry_run,
            build: false,
            no_build: false,
            wait_healthy: false,
            health_timeout: 600,
            profile: vec![],
            scale: vec![],
            fleet: None,
        };
        assert!(up(true).is_client_side());
        assert!(!up(false).is_client_side());
        assert!(
            !ComposeCommand::Down {
                file: None,
                project: None,
                fleet: None
            }
            .is_client_side()
        );
        let fleet = parse_cmd(&["up", "--fleet", "hosts.yaml"]).unwrap();
        assert!(fleet.is_client_side(), "--fleet talks ssh, not the daemon");
        assert!(
            parse_cmd(&["down", "--fleet", "h.yaml"])
                .unwrap()
                .is_client_side()
        );
        assert!(
            parse_cmd(&["ps", "--fleet", "h.yaml"])
                .unwrap()
                .is_client_side()
        );
        assert!(!parse_cmd(&["watch"]).unwrap().is_client_side());
    }

    // ---- the Docker Compose parity commands and file features ---------------------------------------------------------

    fn parse_cmd(a: &[&str]) -> std::result::Result<ComposeCommand, clap::Error> {
        use clap::Parser;
        #[derive(Parser)]
        struct T {
            #[command(subcommand)]
            c: ComposeCommand,
        }
        T::try_parse_from(std::iter::once("t").chain(a.iter().copied())).map(|t| t.c)
    }

    #[test]
    fn wait_is_dockers_spelling_of_wait_healthy_and_keeps_its_ssh_seconds() {
        let ComposeCommand::Up { wait, .. } = parse_cmd(&["up", "--wait"]).unwrap() else {
            panic!()
        };
        assert_eq!(wait, Some(900), "bare --wait");
        let ComposeCommand::Up { wait, .. } = parse_cmd(&["up", "--wait", "30"]).unwrap() else {
            panic!()
        };
        assert_eq!(wait, Some(30), "the old `--wait SECS` still parses");
        let ComposeCommand::Up {
            wait,
            health_timeout,
            ..
        } = parse_cmd(&["up", "--wait-timeout", "7"]).unwrap()
        else {
            panic!()
        };
        assert_eq!((wait, health_timeout), (None, 7));
        let ComposeCommand::Up { wait, .. } = parse_cmd(&["up"]).unwrap() else {
            panic!()
        };
        assert_eq!(wait, None);
    }

    #[test]
    fn the_parity_commands_parse() {
        use ComposeCommand as C;
        assert!(matches!(
            parse_cmd(&["config", "--quiet", "--format", "json", "--profile", "x"]).unwrap(),
            C::Config { quiet: true, ref format, .. } if format == "json"
        ));
        assert!(parse_cmd(&["config", "--format", "toml"]).is_err());
        assert!(
            matches!(parse_cmd(&["stop", "a", "b"]).unwrap(), C::Stop { services, .. } if services == ["a", "b"])
        );
        assert!(matches!(parse_cmd(&["start"]).unwrap(), C::Start { .. }));
        assert!(matches!(
            parse_cmd(&["images", "-q"]).unwrap(),
            C::Images { quiet: true, .. }
        ));
        assert!(matches!(
            parse_cmd(&["pull", "db", "--verify"]).unwrap(),
            C::Pull { verify: true, .. }
        ));
        assert!(matches!(
            parse_cmd(&["push", "--sign", "--i-have-checked-the-licence"]).unwrap(),
            C::Push {
                sign: true,
                licence_override: true,
                ..
            }
        ));
        assert!(matches!(
            parse_cmd(&["ls", "-a", "--json"]).unwrap(),
            C::Ls {
                all: true,
                json: true,
                ..
            }
        ));
        assert!(parse_cmd(&["ls", "-q", "--json"]).is_err());
        let C::Run {
            service,
            rm,
            env,
            workdir,
            user,
            cmd,
            ..
        } = parse_cmd(&[
            "run", "--rm", "-e", "A=1", "-e", "B", "-w", "/src", "-u", "me", "tests", "--", "make",
            "test",
        ])
        .unwrap()
        else {
            panic!()
        };
        assert_eq!((service.as_str(), rm), ("tests", true));
        assert_eq!(env, ["A=1", "B"]);
        assert_eq!(
            (workdir.as_deref(), user.as_deref()),
            (Some("/src"), Some("me"))
        );
        assert_eq!(cmd, ["make", "test"]);
        assert!(
            matches!(parse_cmd(&["top", "db", "--", "aux"]).unwrap(), C::Top { ps_args, .. } if ps_args == ["aux"])
        );
        assert!(matches!(
            parse_cmd(&["cp", "./a", "db:/b", "-p", "ci"]).unwrap(),
            C::Cp { .. }
        ));
        assert!(parse_cmd(&["cp", "only-one"]).is_err());
        assert!(parse_cmd(&["run"]).is_err());
    }

    #[test]
    fn config_pull_push_and_images_are_client_side_and_the_rest_need_the_daemon() {
        for (a, client) in [
            (vec!["config"], true),
            (vec!["images"], true),
            (vec!["pull"], true),
            (vec!["push"], true),
            (vec!["stop"], false),
            (vec!["start"], false),
            (vec!["ls"], false),
            (vec!["run", "x"], false),
            (vec!["top"], false),
            (vec!["cp", "a", "b:/c"], false),
        ] {
            assert_eq!(parse_cmd(&a).unwrap().is_client_side(), client, "{a:?}");
        }
    }

    #[test]
    fn every_parity_command_help_has_an_example() {
        use clap::CommandFactory;
        #[derive(clap::Parser)]
        struct T {
            #[command(subcommand)]
            c: ComposeCommand,
        }
        let mut cmd = T::command();
        for sub in [
            "up", "config", "stop", "start", "images", "pull", "push", "ls", "run", "top", "cp",
        ] {
            let help = cmd
                .find_subcommand_mut(sub)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(
                help.contains("Example") && help.contains("fluxctl mac compose"),
                "{sub}: {help}"
            );
        }
    }

    fn vm(name: &str, status: &str, project: &str, service: &str) -> serde_json::Value {
        serde_json::json!({"name": name, "status": status, "labels": {PROJECT_LABEL: project, SERVICE_LABEL: service}})
    }

    #[test]
    fn stop_and_start_pick_instances_in_dependency_order() {
        let vms = [
            vm("ci-db", "running", "ci", "db"),
            vm("ci-tests", "running", "ci", "tests"),
        ];
        let order = ["db".to_string(), "tests".to_string()];
        let running = |st: &str| st == "running";
        assert_eq!(
            pick_instances(&vms, "ci", &[], &order, true, running).unwrap(),
            ["ci-tests", "ci-db"]
        );
        assert_eq!(
            pick_instances(&vms, "ci", &[], &order, false, running).unwrap(),
            ["ci-db", "ci-tests"]
        );
        assert_eq!(
            pick_instances(&vms, "ci", &["db".into()], &order, true, running).unwrap(),
            ["ci-db"]
        );
        let stopped = [vm("ci-db", "stopped", "ci", "db")];
        assert!(
            pick_instances(&stopped, "ci", &[], &order, true, running)
                .unwrap()
                .is_empty()
        );
        let e = pick_instances(&vms, "ci", &["nope".into()], &order, true, running)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("\"nope\"") && e.contains("db") && e.contains("compose up"),
            "{e}"
        );
        assert_eq!(
            crate::mac_errors::hint_for(&e).map(|c| c.code),
            Some("MAC_COMPOSE_NOT_UP")
        );
    }

    #[test]
    fn the_two_vm_limit_is_checked_before_run_and_start_and_maps_to_the_slot_code() {
        let two = [vm("a", "running", "p", "a"), vm("b", "paused", "p", "b")];
        let e = check_slots(&two, 1, "run a one-off VM")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("2 of 2 are in use") && e.contains("compose stop"),
            "{e}"
        );
        assert_eq!(
            crate::mac_errors::hint_for(&e).map(|c| c.code),
            Some("MAC_SLOT_LIMIT")
        );
        let one = [vm("a", "running", "p", "a"), vm("b", "stopped", "p", "b")];
        assert!(check_slots(&one, 1, "x").is_ok());
        assert!(check_slots(&one, 2, "x").is_err());
    }

    #[test]
    fn ls_groups_instances_by_project_and_hides_stopped_projects_by_default() {
        let vms = [
            vm("ci-db", "running", "ci", "db"),
            vm("ci-tests", "stopped", "ci", "tests"),
            vm("old-a", "stopped", "old", "a"),
            serde_json::json!({"name": "loose", "status": "running", "labels": {}}),
        ];
        let rows = project_rows(&vms, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status(), "running(1), exited(1)");
        assert_eq!(rows[0].services, ["db", "tests"]);
        let all = project_rows(&vms, true);
        assert_eq!(
            all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["ci", "old"]
        );
        assert_eq!(all[1].status(), "exited(1)");
        let t = render_projects(&all);
        assert!(
            t.starts_with("NAME") && t.contains("running(1), exited(1)"),
            "{t}"
        );
    }

    #[test]
    fn cp_rewrites_the_service_to_its_instance_and_leaves_local_paths() {
        let m = BTreeMap::from([("web".to_string(), "ci-web".to_string())]);
        assert_eq!(
            cp_argument("web:/etc/x", &m, "ci").unwrap(),
            "ci-web:/etc/x"
        );
        assert_eq!(cp_argument("./local", &m, "ci").unwrap(), "./local");
        assert_eq!(cp_argument("/abs/file", &m, "ci").unwrap(), "/abs/file");
        let e = cp_argument("db:/x", &m, "ci").unwrap_err().to_string();
        assert!(
            e.contains("\"db\"") && e.contains("known: web") && e.contains("compose up"),
            "{e}"
        );
    }

    #[test]
    fn a_one_off_run_is_unpublished_unrestarted_labelled_into_the_project_and_needs_its_dependencies_running()
     {
        let r = res("name: ci\nservices:\n  db:\n    image: pg:16\n  tests:\n    image: x:1\n    ports: ['8080:80']\n    restart: always\n    depends_on: [db]\n").unwrap();
        let tests = r.services.iter().find(|s| s.service == "tests").unwrap();
        let spec = oneoff_spec("ci", tests, "ab12");
        assert_eq!(spec.name, "ci-tests-run-ab12");
        assert!(spec.publish.is_empty() && spec.restart.is_none());
        assert_eq!(spec.project, Some(("ci".into(), "tests-run-ab12".into())));
        assert_eq!(
            tests.spec.publish,
            ["8080:80"],
            "the service itself is untouched"
        );
        let e = check_deps_running(&[], tests, "ci")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("\"db\"")
                && e.contains("does not start dependencies")
                && e.contains("compose up"),
            "{e}"
        );
        assert_eq!(
            crate::mac_errors::hint_for(&e).map(|c| c.code),
            Some("MAC_COMPOSE_NOT_UP")
        );
        assert!(check_deps_running(&[vm("ci-db", "running", "ci", "db")], tests, "ci").is_ok());
        assert!(check_deps_running(&[vm("ci-db", "stopped", "ci", "db")], tests, "ci").is_err());
    }

    #[test]
    fn link_prefix_goes_in_front_of_the_command_or_stands_alone() {
        let l = [(
            "/Volumes/My Shared Files/fs1/app".to_string(),
            "/etc/app".to_string(),
        )];
        let c = with_links(Some("./run".into()), &l).unwrap();
        assert!(c.contains("ln -sfn") && c.ends_with("; ./run"), "{c}");
        let alone = with_links(None, &l).unwrap();
        assert!(alone.ends_with("}") && !alone.ends_with("; "), "{alone}");
        assert_eq!(with_links(Some("x".into()), &[]).as_deref(), Some("x"));
        assert_eq!(with_links(None, &[]), None);
    }

    #[test]
    fn pull_and_push_targets_skip_or_include_built_only_services() {
        let r = res(
            "name: ci\nservices:\n  app:\n    build: ./app\n  db:\n    image: ghcr.io/acme/pg:16\n",
        )
        .unwrap();
        let pull = image_targets(&r, &[], false).unwrap();
        assert_eq!(
            pull,
            [("db".to_string(), "ghcr.io/acme/pg:16".to_string(), false)]
        );
        let push = image_targets(&r, &[], true).unwrap();
        assert_eq!(push.len(), 2);
        assert!(
            push.iter()
                .any(|(s, i, b)| s == "app" && i == "ci-app:latest" && *b)
        );
        let e = image_targets(&r, &["zzz".into()], false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"zzz\"") && e.contains("app, db"), "{e}");
        let e = image_targets(&r, &["app".into()], false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("built locally"), "{e}");
    }

    #[test]
    fn images_lists_the_store_digest_and_marks_missing_images() {
        let r = res(
            "services:\n  db:\n    image: ghcr.io/acme/pg:16\n  web:\n    image: macos-web:1\n",
        )
        .unwrap();
        let store = vec![(
            crate::mac_store::normalize("ghcr.io/acme/pg:16").unwrap(),
            crate::mac_store::Entry {
                dir: "abc".into(),
                digest: "sha256:0123456789abcdef0123".into(),
                source: "x".into(),
                added_unix: 0,
            },
        )];
        let rows = image_rows(&r, &store, Path::new("/nonexistent"));
        assert_eq!(rows[0].id, "0123456789ab");
        assert_eq!(rows[1].id, "-");
        let t = render_images(&rows);
        assert!(
            t.starts_with("SERVICE") && t.contains("not in the local store"),
            "{t}"
        );
    }

    #[test]
    fn config_prints_the_resolved_file_as_yaml_and_json() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.env"), "A=1\n").unwrap();
        std::fs::write(d.path().join("tok"), "t").unwrap();
        let y = "name: ci\nsecrets:\n  tok: {file: ./tok}\nservices:\n  db:\n    image: pg:16\n  tests:\n    image: x:1\n    env_file: a.env\n    environment: {B: '2'}\n    depends_on:\n      db: {condition: service_healthy}\n    mem_limit: 2g\n    secrets: [tok]\n";
        let r = resolve(&parse(y).unwrap(), None, d.path(), None, |_| None).unwrap();
        let json = render_config(&r, true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["name"], "ci");
        let t = &v["services"]["tests"];
        assert_eq!(
            t["environment"],
            serde_json::json!({"A": "1", "B": "2"}),
            "env_file is merged in"
        );
        assert_eq!(t["depends_on"]["db"]["condition"], "service_healthy");
        assert_eq!(t["mem_limit"], "2048m");
        assert_eq!(t["secrets"][0]["target"], "/private/var/run/secrets/tok");
        let yaml = render_config(&r, false).unwrap();
        assert!(
            yaml.contains("services:") && yaml.contains("service_healthy"),
            "{yaml}"
        );
        assert!(serde_yaml::from_str::<serde_yaml::Value>(&yaml).is_ok());
    }

    #[test]
    fn env_file_list_and_required_false_forms() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.env"), "A=1\n").unwrap();
        let go = |y: &str| resolve(&parse(y).unwrap(), None, d.path(), None, |_| None);
        let r = go("services:\n  a:\n    image: x\n    env_file:\n      - a.env\n      - path: nope.env\n        required: false\n").unwrap();
        assert_eq!(r.services[0].env, [("A".to_string(), "1".to_string())]);
        let e = format!(
            "{:#}",
            go("services:\n  a:\n    image: x\n    env_file:\n      - path: nope.env\n")
                .unwrap_err()
        );
        assert!(
            e.contains("nope.env") && e.contains("required: false"),
            "{e}"
        );
        assert_eq!(
            crate::mac_errors::hint_for(&e).map(|c| c.code),
            Some("MAC_COMPOSE_FILE_MISSING")
        );
        let e = format!("{:#}", go("services:\n  a:\n    image: x\n    env_file:\n      - path: a.env\n        format: json\n").unwrap_err());
        assert!(e.contains("format") && e.contains("not supported"), "{e}");
    }

    #[test]
    fn secrets_and_configs_become_file_mounts_and_bad_ones_are_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("tok"), "t").unwrap();
        std::fs::write(d.path().join("app.conf"), "c").unwrap();
        let go = |y: &str| resolve(&parse(y).unwrap(), None, d.path(), None, |_| None);
        let r = go("secrets:\n  tok: {file: tok}\nconfigs:\n  app: {file: app.conf}\nservices:\n  a:\n    image: x\n    secrets: [tok]\n    configs:\n      - source: app\n        target: /etc/app.conf\n").unwrap();
        let f = &r.services[0].files;
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].kind, cfg::FileKind::Secret);
        assert_eq!(f[1].target, "/etc/app.conf");
        for (y, needle) in [
            (
                "secrets:\n  t: {external: true}\nservices:\n  a: {image: x, secrets: [t]}\n",
                "external",
            ),
            (
                "secrets:\n  t: {environment: X}\nservices:\n  a: {image: x, secrets: [t]}\n",
                "environment",
            ),
            (
                "services:\n  a: {image: x, secrets: [ghost]}\n",
                "does not define",
            ),
            (
                "secrets:\n  t: {file: missing}\nservices:\n  a: {image: x, secrets: [t]}\n",
                "does not exist",
            ),
            (
                "secrets:\n  t: {file: tok}\nservices:\n  a:\n    image: x\n    secrets:\n      - source: t\n        mode: 0400\n",
                "mode",
            ),
        ] {
            let e = format!("{:#}", go(y).unwrap_err());
            assert!(e.contains(needle), "{needle}: {e}");
            assert!(
                crate::mac_errors::hint_for(&e).is_some(),
                "no explain code for: {e}"
            );
        }
    }

    #[test]
    fn an_unsupported_key_is_named_with_what_to_do_and_maps_to_its_code() {
        let e = format!(
            "{:#}",
            res("services:\n  a:\n    image: x\n    networks: [n]\n").unwrap_err()
        );
        assert!(e.contains("`networks`") && e.contains("remove it"), "{e}");
        assert_eq!(
            crate::mac_errors::hint_for(&e).map(|c| c.code),
            Some("MAC_COMPOSE_KEY_UNSUPPORTED")
        );
        let e = format!("{:#}", res("name: x\n").unwrap_err());
        assert!(e.contains("services:"), "{e}");
    }

    #[test]
    fn extension_keys_version_and_merge_keys_are_accepted() {
        let r = res("version: '3.9'\nx-base: &b\n  image: x:1\nservices:\n  a:\n    <<: *b\n    x-note: hi\n").unwrap();
        assert_eq!(r.services[0].spec.image, "x:1");
    }

    #[test]
    fn a_whole_project_with_include_extends_dotenv_and_interpolation_resolves() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, t: &str| {
            let p = d.path().join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, t).unwrap();
        };
        w(".env", "TAG=16\n");
        w(
            "db/db.yaml",
            "services:\n  db:\n    image: pg:${TAG}\n    volumes: ['./data:/var/db']\n",
        );
        w("base.yaml", "services:\n  base:\n    image: x:${VER:-1}\n");
        w(
            "main.yaml",
            "name: ci\ninclude: [db/db.yaml]\nservices:\n  tests:\n    extends:\n      service: base\n      file: base.yaml\n    command: ['go']\n    depends_on: [db]\n",
        );
        std::fs::create_dir_all(d.path().join("db/data")).unwrap();
        let lookup = cfg::lookup_with_dotenv(d.path(), |_| None);
        let lk: &dyn Fn(&str) -> Option<String> = &*lookup;
        let top = cfg::load_tree(&d.path().join("main.yaml"), lk, &mut vec![]).unwrap();
        let f = from_value(serde_yaml::Value::Mapping(top)).unwrap();
        let r = resolve_with(&f, None, d.path(), None, lk, &[]).unwrap();
        let names: Vec<&str> = r.services.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(
            names,
            ["db", "tests"],
            "extended base is not a service of its own"
        );
        assert_eq!(r.services[0].spec.image, "pg:16");
        assert_eq!(r.services[0].spec.mounts[0].host, d.path().join("db/data"));
        assert_eq!(r.services[1].spec.image, "x:1");
        assert_eq!(r.services[1].command, ["go"]);
        assert_eq!(r.services[1].depends_on, ["db"]);
    }

    // ---- watch, scale, one-shots, healthchecks, restart limits ----------------------------------------------------------

    #[test]
    fn one_shots_free_their_slot_so_three_services_fit_two_at_a_time() {
        let y = "services:\n  mig: {image: m:1, command: ./migrate}\n  db: {image: d:1}\n  app:\n    image: a:1\n    depends_on:\n      mig: {condition: service_completed_successfully}\n      db: {condition: service_started}\n";
        let r = res(y).unwrap();
        assert_eq!(r.services.len(), 3);
        assert_eq!(r.peak_slots, 2);
        assert!(
            render_plan(&r).contains("mig (finished)")
                && render_plan(&r).contains("runs to the end")
        );
        // Without the one-shot semantics three services are still refused.
        assert!(res("services:\n  a: {image: x}\n  b: {image: x}\n  c: {image: x}\n").is_err());
    }

    #[test]
    fn scale_makes_numbered_copies_within_the_two_vm_limit() {
        let f = parse("services:\n  web: {image: w:1}\n").unwrap();
        let go = |n: u32, fleet: bool| {
            let m: BTreeMap<String, u32> = [("web".to_string(), n)].into();
            resolve_ext(
                &f,
                Some("p"),
                Path::new("/p"),
                None,
                |_| None,
                &[],
                &m,
                fleet,
            )
        };
        let r = go(2, false).unwrap();
        let names: Vec<_> = r.services.iter().map(|s| s.spec.name.as_str()).collect();
        assert_eq!(names, ["p-web-1", "p-web-2"]);
        assert!(
            r.services
                .iter()
                .all(|s| s.service == "web" && s.replicas == 2)
        );
        let e = go(3, false).unwrap_err().to_string();
        assert!(
            e.contains("at most 2 macOS VMs") && e.contains("--fleet"),
            "{e}"
        );
        assert_eq!(go(3, true).unwrap().services.len(), 3);
        let e = resolve_ext(
            &f,
            Some("p"),
            Path::new("/p"),
            None,
            |_| None,
            &[],
            &[("nope".to_string(), 2)].into(),
            false,
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("MAC_COMPOSE_SCALE"));
        let f = parse("services:\n  web: {image: w:1, ports: ['8080:80']}\n").unwrap();
        let m: BTreeMap<String, u32> = [("web".to_string(), 2)].into();
        let e = resolve_ext(
            &f,
            Some("p"),
            Path::new("/p"),
            None,
            |_| None,
            &[],
            &m,
            false,
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("same port"), "{e:#}");
    }

    #[test]
    fn watch_healthcheck_placement_and_restart_keys_resolve() {
        let r = res("services:\n  a:\n    image: x\n    restart: 'on-failure:3'\n    build: app\n    healthcheck: {test: ./ok, interval: 3s}\n    develop:\n      watch:\n        - {action: sync, path: ./src, target: /app}\n        - {action: rebuild, path: Package.swift}\n    placement: {host: mini-1, labels: [xcode=15], ac_only: true}\n").unwrap();
        let a = &r.services[0];
        assert_eq!(a.restart_limit, Some(3));
        assert_eq!(a.watch.len(), 2);
        assert_eq!(a.placement.host.as_deref(), Some("mini-1"));
        assert!(matches!(
            a.spec.healthcheck,
            Some(crate::mac_lifecycle::HcSetting::Cmd(_))
        ));
        assert!(render_plan(&r).contains("2 watch rule(s)"));
        let e = res("services:\n  a: {image: x, restart: 'always:2'}\n").unwrap_err();
        assert!(format!("{e:#}").contains("MAC_COMPOSE_RESTART"));
        let e = res("services:\n  a: {image: x, develop: {watch: [{action: rebuild, path: p}]}}\n")
            .unwrap_err();
        assert!(format!("{e:#}").contains("MAC_COMPOSE_WATCH_RULE"));
    }

    #[test]
    fn watch_and_the_new_up_flags_parse() {
        match parse_cmd(&["up", "--scale", "web=2", "--scale", "db=1"]).unwrap() {
            ComposeCommand::Up { scale, .. } => assert_eq!(scale.len(), 2),
            c => panic!("{c:?}"),
        }
        assert!(matches!(
            parse_cmd(&["watch", "--no-up", "--interval", "5"]).unwrap(),
            ComposeCommand::Watch {
                no_up: true,
                interval: 5,
                ..
            }
        ));
    }

    #[test]
    fn unhealthy_dependency_gives_a_coded_plain_error() {
        let e = gate_error(
            "web",
            "db",
            "ci-db-1",
            30,
            &anyhow::anyhow!("ci-db-1 was not healthy after 30s: 2 probe(s) failed so far"),
        );
        let m = format!("{e:#}");
        assert!(m.contains("MAC_COMPOSE_DEP_UNHEALTHY"), "{m}");
        assert!(
            m.contains("\"web\"") && m.contains("\"db\"") && m.contains("30s"),
            "{m}"
        );
        assert!(m.contains("compose down"), "{m}");
        let f = gate_error("a", "b", "i", 0, &anyhow::anyhow!("x"));
        assert!(format!("{f:#}").contains("forever"));
    }
}
