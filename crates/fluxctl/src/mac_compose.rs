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
//! `depends_on` means "started after, once SSH answers"; with `condition: service_healthy` it also waits (up to 10 minutes) for the
//! dependency's image HEALTHCHECK to pass, probed over SSH (see mac_health). A dependency whose image has no HEALTHCHECK is an error.
//! A service's `command` runs in the background in the guest (like `mac run -d`).
//!
//! Not verified against a real macOS VM: parsing, ordering and the request bodies are unit tested; `up`, `down` and `ps` were
//! never run against a daemon.

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
    Up {
        /// The compose file (default: mac-compose.yaml, .yml or .json in this directory).
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        /// Project name (default: `name:` in the file, else the directory name).
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
        /// Seconds to wait for each guest to answer on SSH.
        #[arg(long, default_value_t = 900)]
        wait: u64,
        /// Print the order and what would be created, and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Run `mac build` for every service with `build:` even when its image already exists (otherwise only missing images
        /// are built).
        #[arg(long)]
        build: bool,
        /// Also start services with this profile (repeatable; also `COMPOSE_PROFILES=a,b`).
        #[arg(long = "profile", value_name = "NAME")]
        profile: Vec<String>,
    },
    /// Stop and delete the project's instances (reverse depends_on order).
    Down {
        #[arg(short = 'f', long)]
        file: Option<PathBuf>,
        #[arg(short = 'p', long = "project-name")]
        project: Option<String>,
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
        matches!(self, ComposeCommand::Up { dry_run: true, .. })
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ComposeFile {
    #[serde(default)]
    name: Option<String>,
    services: BTreeMap<String, Service>,
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
    env_file: Option<StrOrList>,
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
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub project: String,
    /// In start order.
    pub services: Vec<ServicePlan>,
}

pub fn parse(text: &str) -> Result<ComposeFile> {
    // JSON is YAML, so one parser reads both; the error names the line and the offending key.
    serde_yaml::from_str(text).context("reading the compose file")
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
    ensure!(
        active.len() <= MAX_SERVICES,
        "this file selects {} services, but a Mac runs at most {MAX_SERVICES} macOS VMs at once (Apple's licence limit). Remove services or split the file, or select fewer with `profiles:` and --profile; macOS guests cannot be scaled past 2 per host",
        active.len()
    );
    let project = project
        .map(str::to_string)
        .or_else(|| file.name.clone())
        .unwrap_or_else(|| project_from_dir(base));
    ensure!(
        valid_name(&project),
        "project name {project:?} must be lowercase letters, digits, - or _"
    );
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
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
            let files = match ef {
                StrOrList::One(f) => vec![f.clone()],
                StrOrList::Many(l) => l.clone(),
            };
            for f in files {
                let p = base.join(f);
                let text = std::fs::read_to_string(&p)
                    .with_context(|| ctx(&format!("reading env_file {}", p.display())))?;
                env_pairs.extend(
                    mac_config::parse_env_file(&text, lookup).with_context(|| ctx("env_file"))?,
                );
            }
        }
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
            },
            command,
            env: env_pairs,
            workdir: svc.working_dir.clone(),
            user: svc.user.clone(),
            depends_on: deps[&name].clone(),
            healthy_deps: healthy_deps(svc),
            build,
            has_dependents: depended.contains(&name),
        });
    }
    Ok(Resolved { project, services })
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

/// The `depends_on` entries asking for `condition: service_healthy`.
fn healthy_deps(svc: &Service) -> Vec<String> {
    match &svc.depends_on {
        Some(DependsOn::Map(m)) => m
            .iter()
            .filter(|(_, v)| v["condition"].as_str() == Some("service_healthy"))
            .map(|(k, _)| k.clone())
            .collect(),
        _ => vec![],
    }
}

pub fn render_plan(r: &Resolved) -> String {
    let mut out = format!(
        "project {} ({} of {MAX_SERVICES} macOS VMs)\n",
        r.project,
        r.services.len()
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
                    } else {
                        d.clone()
                    }
                })
                .collect();
            format!(" after {}", list.join(", "))
        };
        out.push_str(&format!(
            "{}. {} = {} from {}{after}\n",
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
        v.extend(e.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    v
}

/// For commands about instances that already exist (`down`, `ps`, `logs`, ...): every profile counts, and a file that is over the
/// two-service limit is not an error here (only `up` starts anything). Returns the project and the services in start order.
fn load_existing(file: Option<PathBuf>, project: Option<&str>) -> Result<(String, Vec<String>)> {
    match load_with(file.clone(), project, &["*".to_string()]) {
        Ok(r) => Ok((r.project, r.services.into_iter().map(|s| s.service).collect())),
        Err(e) => match project {
            Some(p) => Ok((p.to_string(), vec![])),
            None => Err(e),
        },
    }
}

fn load_with(file: Option<PathBuf>, project: Option<&str>, profiles: &[String]) -> Result<Resolved> {
    let path = find_file(file)?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse(&text).with_context(|| path.display().to_string())?;
    let abs = std::fs::canonicalize(&path).unwrap_or(path.clone());
    let base = abs.parent().unwrap_or(Path::new("."));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_with(&parsed, project, base, home.as_deref(), |k| {
        std::env::var(k).ok()
    }, profiles)
    .with_context(|| path.display().to_string())
}

/// `compose up --dry-run`: no daemon.
pub fn run_client(cmd: ComposeCommand) -> Result<()> {
    match cmd {
        ComposeCommand::Up {
            file,
            project,
            profile,
            ..
        } => {
            let r = load_with(file, project.as_deref(), &profiles_from_env(&profile))?;
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
                    let image = &r.services.iter().find(|d| &d.service == dep).map(|d| d.spec.image.clone());
                    let built = r.services.iter().any(|d| &d.service == dep && d.build.is_some());
                    let has = built
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
        _ => bail!("not a client-side compose command"),
    }
}

async fn project_instances(
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
            profile,
        } => {
            let plan = load_with(file, project.as_deref(), &profiles_from_env(&profile))?;
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
            {
                let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
                for s in &plan.services {
                    let Some(b) = &s.build else { continue };
                    if !build && store.resolve(&s.spec.image).is_ok() {
                        continue;
                    }
                    eprintln!("building {} from {} ...", s.spec.image, b.context.display());
                    Box::pin(crate::mac_cmd::run_daemon(r, build_command(&s.spec.image, b, wait)))
                        .await
                        .with_context(|| format!("service {:?}: build", s.service))?;
                }
            }
            eprintln!(
                "note: at most {MAX_SERVICES} macOS VMs run on one Mac; the daemon refuses (409) if another macOS VM holds a slot"
            );
            let mut started: Vec<String> = Vec::new();
            for s in &plan.services {
                for dep in &s.healthy_deps {
                    let dep_name = format!("{}-{dep}", plan.project);
                    eprintln!("waiting for {dep_name} to be healthy ...");
                    crate::mac_health::wait_instance_healthy(r, &dep_name, 600)
                        .await
                        .with_context(|| format!("service {:?} depends_on {dep:?} being healthy (started instances stay: `fluxctl mac compose down`)", s.service))?;
                }
                eprintln!("starting {} ...", s.spec.name);
                let cfg = mac_config::load(&store.resolve(&s.spec.image)?)?;
                let command =
                    mac_config::effective_command(&cfg, &s.command, &s.env, s.workdir.as_deref());
                let id = match launch(r, &store, &s.spec).await {
                    Ok(id) => id,
                    Err(e) => {
                        if !started.is_empty() {
                            eprintln!(
                                "{} already started: `fluxctl mac compose down` removes them",
                                started.join(", ")
                            );
                        }
                        return Err(e);
                    }
                };
                started.push(s.spec.name.clone());
                if command.is_some() || s.has_dependents || !s.spec.mounts.is_empty() {
                    let job = GuestJob {
                        user: s
                            .user
                            .clone()
                            .or(cfg.user.clone())
                            .unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.into()),
                        wait,
                        command,
                        mounts: s.spec.mounts.clone(),
                        detach: true,
                    };
                    run_in_guest(r, &id, &job)
                        .await
                        .with_context(|| format!("service {:?} did not come up (instances left running: `fluxctl mac compose down`)", s.service))?;
                }
                println!("{}", s.spec.name);
            }
            Ok(())
        }
        ComposeCommand::Down { file, project } => {
            let (project, mut order) = load_existing(file.clone(), project.as_deref())?;
            order.reverse();
            let mut vms = project_instances(r, &project).await?;
            ensure!(!vms.is_empty(), "no instances for project {project:?}");
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
            ensure!(!names.is_empty(), "no matching instances in project {project:?}");
            crate::mac_more::run_daemon(r, crate::mac_more::MacMore::Restart { names }).await
        }
        ComposeCommand::Ps { file, project, all } => {
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
        assert_eq!(b.file.as_deref(), Some(Path::new("/proj/My App/./app/Macfile.ci")));
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
        assert!(e.contains("at most 2 macOS VMs") && e.contains("--profile"), "{e}");
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
        assert!(matches!(p(&["logs", "db", "--tail", "5", "-p", "ci"]).unwrap().c, ComposeCommand::Logs { tail: 5, .. }));
        assert!(matches!(p(&["exec", "db", "--", "ls"]).unwrap().c, ComposeCommand::Exec { .. }));
        assert!(matches!(p(&["restart"]).unwrap().c, ComposeCommand::Restart { .. }));
        assert!(matches!(p(&["up", "--build", "--profile", "debug"]).unwrap().c, ComposeCommand::Up { build: true, .. }));
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
        let e = format!("{:#}", res("services:\n  a:\n    command: [\"x\"]\n").unwrap_err());
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
    fn only_dry_run_is_client_side() {
        let up = |dry_run| ComposeCommand::Up {
            file: None,
            project: None,
            wait: 1,
            dry_run,
            build: false,
            profile: vec![],
        };
        assert!(up(true).is_client_side());
        assert!(!up(false).is_client_side());
        assert!(
            !ComposeCommand::Down {
                file: None,
                project: None
            }
            .is_client_side()
        );
    }
}
