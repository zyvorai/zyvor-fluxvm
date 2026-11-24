// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Docker-style container lifecycle for `fluxctl mac`: `run --entrypoint|--pull|--health-*`, `mac create`, `mac kill` and
//! `mac update` (docs/macos-containers.md, "Create, kill, update and the run flags").
//!
//! What the daemon can do shapes every verb here:
//! - A macOS VM stores no command: `run` sends CMD over SSH from this client. So `create` makes the VM only (no CMD, `-e`,
//!   `-w`, `-d`, `--entrypoint`) and says so, instead of pretending to store a command.
//! - `PATCH /v1/vms/{id}` edits the name and the labels only. `update` therefore changes `--restart` and `--label`, and refuses
//!   `--cpus` / `--memory`: there is no daemon call that resizes a VM that exists, running or stopped.
//! - `POST /v1/vms/{id}/kill` ends the VM's runner process at once (no guest shutdown). It is a signal-less operation, so the
//!   only `--signal` that maps to it is KILL.
//!
//! The request bodies, patches and refusal texts are unit tested. Nothing here has run against a real daemon or macOS VM.

use crate::mac_config::{Cmd, Healthcheck, ImageConfig, parse_duration_secs};
use crate::mac_errors::coded;
use crate::mac_health::{HEALTH_START_LABEL, HEALTHCHECK_LABEL, now_unix, override_label};
use crate::mac_inspect::{STOPPED_LABEL, is_internal_label};
use crate::mac_instance::{LaunchSpec, Restart, RunOpts};
use crate::mac_store::{self, Store};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Map, Value, json};

/// Set on an instance made by `mac create` until `mac start` boots it: policies (`always`, ...) must not start it by themselves.
pub const NOT_STARTED_LABEL: &str = "fluxvm.mac.notstarted";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum PullPolicy {
    /// Pull the image from its registry before every run.
    Always,
    /// Pull only when the image is not in the local store (the default).
    #[default]
    Missing,
    /// Never pull: use the local store or fail.
    Never,
}

/// How an instance's healthcheck differs from its image's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HcSetting {
    /// `--health-cmd` (and the tuning flags): this healthcheck.
    Cmd(Healthcheck),
    /// `--no-healthcheck`: none, whatever the image says.
    Off,
}

/// The `mac run` / `mac create` flags of this module. Flattened into [`RunOpts`].
#[derive(clap::Args, Debug, Default, Clone)]
pub struct LifeOpts {
    /// Replace the image's ENTRYPOINT (and drop its CMD, as Docker does): shell form, or a JSON array. `""` clears both. CMD after
    /// `--` still applies.
    #[arg(long, value_name = "CMD")]
    pub entrypoint: Option<String>,
    /// When to pull the image from its registry: `always`, `missing` (not in the local store; the default) or `never`. Only an
    /// image named with a registry (`host/repo:tag`) can be pulled.
    #[arg(long, value_enum, default_value_t = PullPolicy::Missing, value_name = "POLICY")]
    pub pull: PullPolicy,
    /// Run CMD in the guest (over SSH) to check the instance is healthy; exit 0 = healthy. Replaces the image's HEALTHCHECK and
    /// is what `mac health`, `--wait-healthy` and `mac supervise` use.
    #[arg(long, value_name = "CMD")]
    pub health_cmd: Option<String>,
    /// Time between probes: `30s`, `2m` (default 30s).
    #[arg(long, value_name = "DURATION")]
    pub health_interval: Option<String>,
    /// How long one probe may take before it counts as failed (default 30s). This is not `--health-timeout`, which is how long
    /// `--wait-healthy` waits.
    #[arg(long, value_name = "DURATION")]
    pub health_probe_timeout: Option<String>,
    /// Consecutive failed probes before the instance is `unhealthy` (default 3).
    #[arg(long, value_name = "N")]
    pub health_retries: Option<u32>,
    /// Failures in this long after a start do not count (default 0s).
    #[arg(long, value_name = "DURATION")]
    pub health_start_period: Option<String>,
    /// No healthcheck for this instance, even if the image has a HEALTHCHECK.
    #[arg(long)]
    pub no_healthcheck: bool,
    // Docker flags with no meaning for a macOS guest: parsed so that the refusal can say why.
    #[arg(long, hide = true, value_name = "NETWORK")]
    pub network: Option<String>,
    #[arg(long, hide = true, value_name = "PLATFORM")]
    pub platform: Option<String>,
}

impl LifeOpts {
    /// `--network` and `--platform` mean nothing for a macOS guest.
    pub fn check_supported(&self) -> Result<()> {
        if let Some(n) = &self.network {
            return Err(coded(
                format!(
                    "--network {n}: a macOS guest has one network, a NAT interface from Virtualization.framework, so there is no network to choose. Publish a port with `-p HOST:GUEST` and reach the guest at the address `fluxctl mac ps` shows"
                ),
                "MAC_FLAG_NOT_APPLICABLE",
            ));
        }
        if let Some(p) = &self.platform {
            return Err(coded(
                format!(
                    "--platform {p}: every `fluxctl mac` image is macOS on Apple silicon (darwin/arm64), so there is no platform to choose. Leave --platform out"
                ),
                "MAC_FLAG_NOT_APPLICABLE",
            ));
        }
        Ok(())
    }

    /// The names of the flags that are not their default: `mac run --fleet` cannot carry them to another Mac.
    pub fn used_flags(&self) -> Vec<&'static str> {
        [
            ("--entrypoint", self.entrypoint.is_some()),
            ("--pull", self.pull != PullPolicy::Missing),
            ("--health-cmd", self.health_cmd.is_some()),
            ("--health-interval", self.health_interval.is_some()),
            (
                "--health-probe-timeout",
                self.health_probe_timeout.is_some(),
            ),
            ("--health-retries", self.health_retries.is_some()),
            ("--health-start-period", self.health_start_period.is_some()),
            ("--no-healthcheck", self.no_healthcheck),
        ]
        .into_iter()
        .filter_map(|(f, u)| u.then_some(f))
        .collect()
    }

    fn tuning_given(&self) -> bool {
        self.health_interval.is_some()
            || self.health_probe_timeout.is_some()
            || self.health_retries.is_some()
            || self.health_start_period.is_some()
    }

    /// The healthcheck this run asks for, given the image's own (`--health-interval` alone tunes the image's).
    pub fn resolve_healthcheck(&self, image: Option<&Healthcheck>) -> Result<Option<HcSetting>> {
        let bad = |m: String| coded(m, "MAC_HEALTHCHECK_FLAGS");
        if self.no_healthcheck {
            if self.health_cmd.is_some() || self.tuning_given() {
                return Err(bad(
                    "--no-healthcheck cannot be combined with --health-cmd or the other --health-* flags: it means there is no healthcheck. Drop one or the other".into(),
                ));
            }
            return Ok(Some(HcSetting::Off));
        }
        if self.health_cmd.is_none() && !self.tuning_given() {
            return Ok(None);
        }
        let dur = |flag: &str, v: &Option<String>, min: u64| -> Result<Option<u64>> {
            let Some(v) = v else { return Ok(None) };
            let n = parse_duration_secs(v).map_err(|e| bad(format!("{flag}: {e}")))?;
            if n < min {
                return Err(bad(format!(
                    "{flag} {v}: must be at least {min}s (healthchecks run with a granularity of one second)"
                )));
            }
            Ok(Some(n))
        };
        let mut hc = match (&self.health_cmd, image) {
            (Some(c), _) => Healthcheck {
                cmd: Cmd::parse(c).map_err(|m| bad(format!("--health-cmd {m}")))?,
                interval_secs: 30,
                timeout_secs: 30,
                retries: 3,
                start_period_secs: 0,
            },
            (None, Some(h)) => h.clone(),
            (None, None) => {
                return Err(bad(
                    "--health-interval, --health-probe-timeout, --health-retries and --health-start-period tune a healthcheck, and this image has none. Add --health-cmd \"CMD\" to say what to run".into(),
                ));
            }
        };
        if let Some(n) = dur("--health-interval", &self.health_interval, 1)? {
            hc.interval_secs = n;
        }
        if let Some(n) = dur("--health-probe-timeout", &self.health_probe_timeout, 1)? {
            hc.timeout_secs = n;
        }
        if let Some(n) = dur("--health-start-period", &self.health_start_period, 0)? {
            hc.start_period_secs = n;
        }
        if let Some(n) = self.health_retries {
            if n == 0 {
                return Err(bad(
                    "--health-retries 0: use at least 1 (the number of failed probes in a row that make the instance unhealthy)".into(),
                ));
            }
            hc.retries = n;
        }
        Ok(Some(HcSetting::Cmd(hc)))
    }
}

/// `--entrypoint`: replace the image's ENTRYPOINT and, like Docker, drop its CMD.
pub fn apply_entrypoint(cfg: &mut ImageConfig, entrypoint: Option<&str>) -> Result<()> {
    let Some(text) = entrypoint else {
        return Ok(());
    };
    cfg.cmd = None;
    cfg.entrypoint = if text.trim().is_empty() {
        None
    } else {
        Some(Cmd::parse(text).map_err(|m| {
            coded(
                format!("--entrypoint {m}: give the program, e.g. --entrypoint /bin/sh or --entrypoint '[\"/bin/sh\",\"-c\"]'"),
                "MAC_HEALTHCHECK_FLAGS",
            )
        })?)
    };
    Ok(())
}

/// The label a launch records for an instance's own healthcheck.
pub fn healthcheck_label(h: &HcSetting) -> (&'static str, String) {
    let value = match h {
        HcSetting::Off => override_label(None),
        HcSetting::Cmd(c) => override_label(Some(c)),
    };
    (HEALTHCHECK_LABEL, value)
}

/// What `--pull` does for an image.
#[derive(Debug, PartialEq, Eq)]
pub enum PullStep {
    /// Use what the local store has.
    UseLocal,
    Pull,
    /// Cannot proceed; the text says why and what to do.
    Refuse(String),
}

/// `host.tld/repo:tag`, `host:5000/repo`, `localhost/repo`: a reference that names a registry (a bare name is a local one).
pub fn is_registry_ref(reference: &str) -> bool {
    match reference.split_once('/') {
        Some((host, _)) => host.contains('.') || host.contains(':') || host == "localhost",
        None => false,
    }
}

pub fn decide_pull(policy: PullPolicy, image: &str, present: bool) -> PullStep {
    let registry = is_registry_ref(image);
    match (policy, present, registry) {
        (PullPolicy::Never | PullPolicy::Missing, true, _) => PullStep::UseLocal,
        (PullPolicy::Never, false, _) => PullStep::Refuse(format!(
            "{image} is not in the local store and --pull never forbids pulling it. Pull it with `fluxctl mac pull {image}` (or drop --pull never), or see what you have with `fluxctl mac images`"
        )),
        (PullPolicy::Missing, false, false) => PullStep::Refuse(format!(
            "no image {image} in the local store, and it is not a registry reference (host/repo:tag), so it cannot be pulled. See `fluxctl mac images`, or add it with `fluxctl mac import DIR {image}`"
        )),
        (PullPolicy::Always, _, false) => PullStep::Refuse(format!(
            "--pull always needs a registry reference (host/repo:tag), and {image} is a local name. Use --pull missing (the default) to run the local copy, or pull from a registry first"
        )),
        (PullPolicy::Missing, false, true) | (PullPolicy::Always, _, true) => PullStep::Pull,
    }
}

/// Apply `--pull` before a run or create. Prints one line when it pulls.
pub async fn ensure_image(image: &str, policy: PullPolicy) -> Result<()> {
    let store = Store::open(mac_store::default_root()?)?;
    let present = store.resolve(image).is_ok();
    match decide_pull(policy, image, present) {
        PullStep::UseLocal => Ok(()),
        PullStep::Refuse(why) => Err(coded(why, "MAC_PULL_POLICY")),
        PullStep::Pull => {
            if policy == PullPolicy::Missing {
                eprintln!(
                    "{image} is not in the local store: pulling it (macOS images are tens of GB; --pull never turns this off)"
                );
            } else {
                eprintln!("pulling {image} (--pull always)");
            }
            crate::mac_plus::pull(&store, image, &Default::default())
                .await
                .map_err(|e| {
                    coded(
                        format!(
                            "could not pull {image}: {e:#}. Check the name and your registry login (FLUXVM_REGISTRY_USER / FLUXVM_REGISTRY_PASSWORD){}",
                            if present {
                                "; the local copy is untouched, and --pull never runs it"
                            } else {
                                ""
                            }
                        ),
                        "MAC_PULL_POLICY",
                    )
                })
        }
    }
}

// ---------------------------------------------------------------- the verbs

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum MacLife {
    /// Make an instance from an image without starting it (the Docker `create`); `mac start NAME` boots it. Takes the flags of
    /// `mac run` that a VM can hold (`--name`, `-p`, `-v`, `--label`, `--restart`, `--cpus`, `--memory`, `--pull`, `--health-*`).
    /// A macOS VM stores no command, so CMD, `-e`, `--env-file`, `-w`, `-d` and `--entrypoint` belong to `mac run`/`mac exec`.
    #[command(
        after_help = "Examples:\n  fluxctl mac create --name ci --cpus 4 --memory 8g --restart unless-stopped macos-xcode:15\n  fluxctl mac start ci && fluxctl mac exec ci -- xcodebuild -version\n\nThe instance counts against the 2-VM limit only once it starts."
    )]
    Create {
        /// Instance name (default: mac-<random>).
        #[arg(long)]
        name: Option<String>,
        /// Publish a guest port on this Mac: `[ADDR:]HOST:GUEST`. Repeatable.
        #[arg(short = 'p', long = "publish")]
        publish: Vec<String>,
        #[command(flatten)]
        opts: RunOpts,
        /// Print one JSON object (schema in docs/macos-containers.md) instead of the instance name.
        #[arg(long)]
        json: bool,
        #[arg(long, hide = true)]
        rm: bool,
        /// A local image (`fluxctl mac images`), or a registry reference to pull (see --pull).
        image: String,
        #[arg(last = true, hide = true)]
        cmd: Vec<String>,
    },
    /// Stop an instance at once, without asking the guest to shut down (the Docker `kill`): the VM's runner process is ended.
    /// The guest may need a disk check on its next boot; prefer `mac stop`. `-s KILL` is accepted; no other signal can be sent to
    /// a VM.
    #[command(
        after_help = "Examples:\n  fluxctl mac kill build-1\n  fluxctl mac kill -s KILL build-1 build-2\n\nA clean shutdown is `fluxctl mac stop NAME`. To signal a process inside the guest: `fluxctl mac exec NAME -- kill -TERM PID`."
    )]
    Kill {
        /// `KILL`, `SIGKILL` or `9`: the only signal a VM can receive from outside.
        #[arg(short = 's', long, value_name = "SIGNAL")]
        signal: Option<String>,
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Change an instance the way `docker update` does. `--restart`, `--label` and `--rm-label` apply at once, running or stopped.
    /// `--cpus` and `--memory` are read when a macOS VM boots: on a stopped instance they apply now; on a running one the command
    /// says so plainly and changes nothing about the size unless you add `--restart-now` (checkpoint, stop, apply, start; same
    /// name and disk).
    #[command(
        after_help = "Examples:\n  fluxctl mac update ci --restart always\n  fluxctl mac update ci --label team=ios --rm-label old\n  fluxctl mac update ci --cpus 8 --memory 16g            (stopped: applied now; running: says a restart is needed)\n  fluxctl mac update ci --cpus 8 --restart-now            (saves a checkpoint first; --no-checkpoint skips it)\n\nEnvironment variables cannot be stored on a macOS VM: pass them per command with `fluxctl mac exec ci -e K=V -- CMD`. Studio: the Edit panel on the Mac page does the same. If it fails: fluxctl explain MAC_UPDATE_NEEDS_RESTART"
    )]
    Update {
        name: String,
        /// New CPU count (read when the VM boots).
        #[arg(long, value_name = "N")]
        cpus: Option<u8>,
        /// New memory: `8g`, `512m` or MiB (read when the VM boots).
        #[arg(long, value_name = "SIZE")]
        memory: Option<String>,
        /// Restart policy: `no`, `always`, `unless-stopped` or `on-failure` (applied by `mac supervise` / `mac reconcile`).
        #[arg(long, value_name = "POLICY")]
        restart: Option<String>,
        /// Set a label: `KEY=VALUE`. Repeatable; an existing key is replaced.
        #[arg(long = "label", value_name = "KEY=VALUE")]
        label: Vec<String>,
        /// Remove a label by key. Repeatable.
        #[arg(long = "rm-label", value_name = "KEY")]
        rm_label: Vec<String>,
        /// Not stored on a macOS VM: explained, nothing changed. Use `mac exec NAME -e K=V -- CMD`.
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// A running instance: save a checkpoint, stop it, apply `--cpus` / `--memory` and start it again (same name, same disk).
        #[arg(long)]
        restart_now: bool,
        /// With `--restart-now`: do not save the `auto-before-update-<time>` checkpoint first.
        #[arg(long, requires = "restart_now")]
        no_checkpoint: bool,
        /// Print one JSON object (schema in docs/macos-containers.md).
        #[arg(long)]
        json: bool,
    },
}

impl MacLife {
    /// A refusal that needs no daemon: shown straight away.
    pub fn refusal(&self) -> Option<anyhow::Error> {
        match self {
            MacLife::Create { opts, rm, cmd, .. } => opts
                .life
                .check_supported()
                .err()
                .or_else(|| create_refusal(opts, *rm, cmd)),
            MacLife::Kill { signal, .. } => signal.as_deref().and_then(|s| signal_refusal(s).err()),
            MacLife::Update { name, env, .. } => env_refusal(name, env),
        }
    }

    pub fn is_client_side(&self) -> bool {
        self.refusal().is_some()
    }

    /// The client-side outcome: the refusal (callers only get here when there is one).
    pub fn run_client(&self) -> Result<()> {
        match self.refusal() {
            Some(e) => Err(e),
            None => bail!("internal: this command needs the daemon"),
        }
    }
}

/// `create` takes what a VM can hold; the rest belongs to a command run over SSH.
fn create_refusal(opts: &RunOpts, rm: bool, cmd: &[String]) -> Option<anyhow::Error> {
    let used: Vec<&str> = [
        ("--rm", rm),
        ("CMD after --", !cmd.is_empty()),
        ("-e/--env", !opts.env.is_empty()),
        ("--env-file", !opts.env_file.is_empty()),
        ("-w/--workdir", opts.workdir.is_some()),
        ("-d/--detach", opts.detach),
        ("--entrypoint", opts.life.entrypoint.is_some()),
    ]
    .into_iter()
    .filter_map(|(f, u)| u.then_some(f))
    .collect();
    if used.is_empty() {
        return None;
    }
    Some(coded(
        format!(
            "{} cannot be used with `mac create`: a macOS VM stores no command or environment, `mac run` sends them over SSH when it starts. Create the VM with the other flags and run things after `mac start` with `fluxctl mac exec NAME -e K=V -- CMD`, or use `fluxctl mac run` for the whole thing",
            used.join(", ")
        ),
        "MAC_CREATE_RUN_ONLY",
    ))
}

/// `--env` on an instance: a macOS VM keeps no environment, so there is nothing to change.
pub fn env_refusal(name: &str, env: &[String]) -> Option<anyhow::Error> {
    if env.is_empty() {
        return None;
    }
    Some(coded(
        format!(
            "--env: a macOS VM stores no environment (the guest keeps none between runs; `mac run` and `mac exec` send it over SSH each time), so there is nothing on {name} to update. Nothing was changed. Pass it when you run something: `fluxctl mac exec {name} -e KEY=VALUE -- CMD`"
        ),
        "MAC_UPDATE_ENV_NOT_STORED",
    ))
}

/// `-s SIGNAL`: only KILL maps to what the daemon does.
pub fn signal_refusal(signal: &str) -> Result<()> {
    let up = signal.trim().to_ascii_uppercase();
    let s = up.strip_prefix("SIG").unwrap_or(&up);
    if matches!(s, "KILL" | "9") {
        return Ok(());
    }
    let hint = if matches!(s, "TERM" | "15" | "INT" | "2" | "HUP" | "1") {
        "To ask the guest to shut down cleanly use `fluxctl mac stop NAME`"
    } else {
        "To signal a process inside the guest use `fluxctl mac exec NAME -- kill -SIGNAL PID`"
    };
    Err(coded(
        format!(
            "--signal {signal}: a macOS VM is not a process you can signal. `mac kill` ends the VM's runner at once (that is KILL), and no other signal exists for it. {hint}"
        ),
        "MAC_KILL_SIGNAL",
    ))
}

/// Why `kill` would not do anything for this record, in plain words.
pub fn check_killable(vm: &Value, name: &str) -> Result<()> {
    match crate::mac_inspect::status(vm) {
        "stopped" | "failed" | "created" => bail!(
            "{name} is not running (it is {}), so there is nothing to kill. Start it with `fluxctl mac start {name}`",
            crate::mac_inspect::status(vm)
        ),
        _ => Ok(()),
    }
}

fn kill_unavailable(name: &str, e: &anyhow::Error) -> anyhow::Error {
    coded(
        format!(
            "cannot kill {name}: this daemon has no kill call (it is older than this fluxctl). Use `fluxctl mac stop {name}` for a clean stop, or update the daemon. Daemon said: {e:#}"
        ),
        "MAC_KILL_UNAVAILABLE",
    )
}

/// The label part of an update: `--restart` (`no` removes the label), `--label` and `--rm-label`.
fn labels_part(
    restart: Option<&str>,
    labels: &[String],
    rm: &[String],
) -> Result<Map<String, Value>> {
    let mut l = Map::new();
    if let Some(r) = restart {
        let p = Restart::parse(r)?;
        l.insert(
            crate::mac_inspect::RESTART_LABEL.into(),
            if p == Restart::No {
                Value::Null
            } else {
                json!(p.as_str())
            },
        );
    }
    for raw in labels {
        let (k, v) = crate::mac_instance::parse_label(raw)?;
        anyhow::ensure!(
            !is_internal_label(&k),
            "label {k:?}: keys starting with fluxvm.mac are reserved"
        );
        l.insert(k, json!(v));
    }
    for k in rm {
        anyhow::ensure!(
            !is_internal_label(k) && !k.is_empty(),
            "label {k:?}: keys starting with fluxvm.mac are reserved"
        );
        anyhow::ensure!(
            !labels
                .iter()
                .any(|x| x.split('=').next() == Some(k.as_str())),
            "label {k:?} is both set (--label) and removed (--rm-label); pick one"
        );
        l.insert(k.clone(), Value::Null);
    }
    Ok(l)
}

/// The `PATCH /v1/vms/{id}` body of `mac update` for labels and the restart policy (what an older daemon understands).
pub fn update_patch(restart: Option<&str>, labels: &[String], rm: &[String]) -> Result<Value> {
    let l = labels_part(restart, labels, rm)?;
    anyhow::ensure!(
        !l.is_empty(),
        "nothing to update: pass --cpus, --memory, --restart POLICY, --label KEY=VALUE or --rm-label KEY (see `fluxctl mac update --help`)"
    );
    Ok(json!({ "labels": l }))
}

/// What `mac update` was asked to do.
#[derive(Debug, Default, Clone)]
pub struct UpdateArgs {
    pub name: String,
    pub cpus: Option<u8>,
    pub memory: Option<String>,
    pub restart: Option<String>,
    pub label: Vec<String>,
    pub rm_label: Vec<String>,
    pub restart_now: bool,
    pub no_checkpoint: bool,
    pub json: bool,
}

impl UpdateArgs {
    fn resizes(&self) -> bool {
        self.cpus.is_some() || self.memory.is_some()
    }
}

/// The body of `PATCH /v1/mac/instances/{id}`, checked here so a typo never reaches the daemon.
pub fn update_body(a: &UpdateArgs) -> Result<Value> {
    let l = labels_part(a.restart.as_deref(), &a.label, &a.rm_label)?;
    let mut b = Map::new();
    if let Some(c) = a.cpus {
        anyhow::ensure!(c >= 1, "--cpus must be at least 1");
        b.insert("cpus".into(), json!(c));
    }
    if let Some(m) = &a.memory {
        let mib = crate::mac_config::parse_memory_mib(m)?;
        anyhow::ensure!(
            mib >= 512,
            "--memory {m}: a macOS VM needs at least 512 MiB (use 8g, 16g, ...)"
        );
        b.insert("memory_mib".into(), json!(mib));
    }
    if let Some(r) = &a.restart {
        Restart::parse(r)?;
        b.insert("restart".into(), json!(r));
    }
    let set: Map<String, Value> = l
        .iter()
        .filter(|(k, v)| !v.is_null() && k.as_str() != crate::mac_inspect::RESTART_LABEL)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !set.is_empty() {
        b.insert("labels".into(), Value::Object(set));
    }
    if !a.rm_label.is_empty() {
        b.insert("rm_labels".into(), json!(a.rm_label));
    }
    anyhow::ensure!(
        !b.is_empty(),
        "nothing to update: pass --cpus, --memory, --restart POLICY, --label KEY=VALUE or --rm-label KEY (see `fluxctl mac update --help`)"
    );
    anyhow::ensure!(
        !a.restart_now || a.resizes(),
        "--restart-now only matters together with --cpus or --memory (those are what need a restart)"
    );
    if a.restart_now {
        b.insert("restart_now".into(), json!(true));
    }
    if a.no_checkpoint {
        b.insert("checkpoint".into(), json!(false));
    }
    Ok(Value::Object(b))
}

/// The labels `mac start` changes: it is no longer "created only" or "stopped by you", and a new start period begins.
pub fn start_patch(vm: &Value, now: u64) -> Value {
    let mut l = Map::new();
    for k in [STOPPED_LABEL, NOT_STARTED_LABEL] {
        if vm["labels"][k].is_string() {
            l.insert(k.into(), Value::Null);
        }
    }
    l.insert(HEALTH_START_LABEL.into(), json!(now.to_string()));
    json!({ "labels": l })
}

/// The bookkeeping after `mac start` / `mac restart` booted an instance.
pub async fn after_start(r: &crate::remote::Remote, id: &str, vm: &Value) -> Result<()> {
    r.call(
        reqwest::Method::PATCH,
        &format!("/v1/vms/{id}"),
        Some(start_patch(vm, now_unix())),
    )
    .await
    .map(|_| ())
}

/// The launch spec shared by `create`; `mac run` builds its own in `mac_instance`.
fn spec_from(
    name: Option<String>,
    publish: Vec<String>,
    image: &str,
    opts: &RunOpts,
    health: Option<HcSetting>,
) -> Result<LaunchSpec> {
    let cwd = std::env::current_dir().context("current directory")?;
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let name = name.unwrap_or_else(crate::mac_cmd::random_name);
    let token = uuid::Uuid::new_v4().to_string();
    let mut labels: Vec<(String, String)> = opts
        .label
        .iter()
        .map(|l| crate::mac_instance::parse_label(l))
        .collect::<Result<_>>()?;
    labels.push((crate::mac_volume::INSTANCE_ID_LABEL.into(), token.clone()));
    Ok(LaunchSpec {
        name: name.clone(),
        image: image.to_string(),
        publish,
        mounts: opts
            .volume
            .iter()
            .map(|v| crate::mac_volume::parse_mount_default(v, &cwd, home.as_deref(), Some(&token)))
            .collect::<Result<_>>()?,
        cpus: opts.cpus,
        memory_mib: opts
            .memory
            .as_deref()
            .map(crate::mac_config::parse_memory_mib)
            .transpose()?,
        labels,
        restart: Some(Restart::parse(&opts.restart)?),
        project: None,
        healthcheck: health,
    })
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacLife) -> Result<()> {
    if let Some(e) = cmd.refusal() {
        return Err(e);
    }
    match cmd {
        MacLife::Create {
            name,
            publish,
            opts,
            json,
            image,
            ..
        } => create(r, name, publish, opts, json, image).await,
        MacLife::Kill { names, .. } => kill(r, &names).await,
        MacLife::Update {
            name,
            cpus,
            memory,
            restart,
            label,
            rm_label,
            restart_now,
            no_checkpoint,
            json,
            ..
        } => {
            let a = UpdateArgs {
                name,
                cpus,
                memory,
                restart,
                label,
                rm_label,
                restart_now,
                no_checkpoint,
                json,
            };
            let out = update_with(r, &a).await?;
            if let Some(t) = &out.progress {
                eprintln!("{t}");
            }
            if let Some(d) = &out.document {
                println!("{}", serde_json::to_string(d)?);
            } else if out.error.is_none() {
                println!("{}", a.name);
            }
            out.error.map_or(Ok(()), Err)
        }
    }
}

async fn create(
    r: &crate::remote::Remote,
    name: Option<String>,
    publish: Vec<String>,
    opts: RunOpts,
    json_out: bool,
    image: String,
) -> Result<()> {
    ensure_image(&image, opts.life.pull).await?;
    crate::mac_ops::preflight(&image, &crate::mac_ops::RunExtra::default())?;
    let store = Store::open(mac_store::default_root()?)?;
    let cfg = crate::mac_config::load(&store.resolve(&image)?)?;
    let health = opts.life.resolve_healthcheck(cfg.healthcheck.as_ref())?;
    let spec = spec_from(name, publish, &image, &opts, health)?;
    let mut extra = Map::new();
    extra.insert(NOT_STARTED_LABEL.into(), json!("true"));
    let stamp = crate::mac_reap::stamp(Some(&crate::mac_reap::current_user()), None, now_unix());
    if let Some(m) = stamp["labels"].as_object() {
        extra.extend(m.clone());
    }
    if !spec.mounts.is_empty() {
        eprintln!(
            "note: the shared folders are attached when the VM starts and appear in the guest under {}/fsN; the GUEST path links are only made by `mac run`",
            crate::mac_instance::GUEST_SHARES_DIR
        );
    }
    let id = crate::mac_instance::create_instance(r, &store, &spec, extra).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string(&crate::mac_json::create_document(&spec.name, &id, &image))?
        );
    } else {
        println!("{}", spec.name);
    }
    eprintln!(
        "created {} (not started). Boot it with `fluxctl mac start {}`",
        spec.name, spec.name
    );
    Ok(())
}

async fn kill(r: &crate::remote::Remote, names: &[String]) -> Result<()> {
    let mut failures = Vec::new();
    for n in names {
        let res = async {
            let vm = crate::mac_cmd::find_instance(r, n).await?;
            check_killable(&vm, n)?;
            let id = vm["id"].as_str().context("the daemon record has no id")?;
            r.call(reqwest::Method::POST, &format!("/v1/vms/{id}/kill"), None)
                .await
                // The instance was just found, so a 404/405 here is the route, not the VM.
                .map_err(|e| {
                    let t = format!("{e:#}");
                    if t.contains(": 404 ") || t.contains(": 405 ") {
                        kill_unavailable(n, &e)
                    } else {
                        crate::mac_instance::explain(e, &format!("kill {n}"))
                    }
                })?;
            // Killed by you: `unless-stopped` must not bring it back.
            r.call(
                reqwest::Method::PATCH,
                &format!("/v1/vms/{id}"),
                Some(json!({"labels": {STOPPED_LABEL: "true"}})),
            )
            .await
            .map(|_| ())
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
    anyhow::ensure!(
        failures.is_empty(),
        "kill failed for: {}",
        failures.join(", ")
    );
    Ok(())
}

/// What `mac update` needs from the daemon; a fake in tests.
#[allow(async_fn_in_trait)]
pub trait UpdateApi {
    async fn find(&self, name: &str) -> Result<Value>;
    /// `PATCH /v1/mac/instances/{id}`.
    async fn update_instance(&self, id: &str, body: Value) -> Result<Value>;
    /// `PATCH /v1/vms/{id}`: labels only (an older daemon).
    async fn patch_vm(&self, id: &str, body: Value) -> Result<Value>;
}

impl UpdateApi for crate::remote::Remote {
    async fn find(&self, name: &str) -> Result<Value> {
        crate::mac_cmd::find_instance(self, name).await
    }
    async fn update_instance(&self, id: &str, body: Value) -> Result<Value> {
        self.call(
            reqwest::Method::PATCH,
            &format!("/v1/mac/instances/{id}"),
            Some(body),
        )
        .await
    }
    async fn patch_vm(&self, id: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::PATCH, &format!("/v1/vms/{id}"), Some(body))
            .await
    }
}

/// What `mac update` prints: a progress text (stderr), the `--json` document (stdout, when asked) and the failure, if any.
/// A change that needs a restart is both: the rest is applied and reported, and the command still fails so a script notices.
#[derive(Debug, Default)]
pub struct UpdateOutcome {
    pub progress: Option<String>,
    pub document: Option<Value>,
    pub error: Option<anyhow::Error>,
}

fn route_missing(e: &anyhow::Error) -> bool {
    let t = format!("{e:#}");
    t.contains(": 404 ") || t.contains(": 405 ")
}

/// The older-daemon path: labels and the restart policy through `PATCH /v1/vms/{id}`; a size change cannot be done there.
async fn update_via_vm_patch<A: UpdateApi>(
    api: &A,
    a: &UpdateArgs,
    id: &str,
    cause: &anyhow::Error,
) -> Result<Value> {
    if a.resizes() || a.restart_now {
        return Err(coded(
            format!(
                "this daemon is older than this fluxctl and has no call to change the CPUs or memory of {n} (it only edits labels and the restart policy). Nothing was changed. Update the daemon on the Mac, or resize the long way: `fluxctl mac stop {n}`, `fluxctl mac commit {n} {n}-img:1` (keeps its disk), `fluxctl mac rm {n}`, `fluxctl mac create --name {n} --cpus N --memory SIZE {n}-img:1`. Daemon said: {cause:#}",
                n = a.name
            ),
            "MAC_UPDATE_NOT_SUPPORTED",
        ));
    }
    api.patch_vm(
        id,
        update_patch(a.restart.as_deref(), &a.label, &a.rm_label)?,
    )
    .await
    .map_err(|e| crate::mac_instance::explain(e, &format!("update {}", a.name)))?;
    Ok(
        json!({"ok": true, "applied": ["labels and restart policy"], "pending_restart": [],
        "restarted": false, "checkpoint": null, "explain": null, "next": null}),
    )
}

/// The progress text of an answer: what was applied, what needs a restart, and what to do next.
pub fn answer_text(name: &str, answer: &Value) -> String {
    let mut lines = Vec::new();
    let list = |k: &str| -> Vec<String> {
        answer[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                v.as_str()
                    .or_else(|| v["change"].as_str())
                    .map(str::to_owned)
            })
            .collect()
    };
    let applied = list("applied");
    if !applied.is_empty() {
        lines.push(format!("{name}: applied {}", applied.join("; ")));
    }
    if let Some(t) = answer["checkpoint"].as_str() {
        lines.push(format!(
            "{name}: saved checkpoint {name}:{t} first; to go back: fluxctl mac checkpoint restore {name} {t}"
        ));
    }
    if answer["restarted"].as_bool() == Some(true) {
        lines.push(format!("{name}: restarted with the new size"));
    }
    let pending = list("pending_restart");
    if !pending.is_empty() {
        lines.push(format!(
            "{name}: NOT applied, needs a restart: {}",
            pending.join("; ")
        ));
    }
    if let Some(n) = answer["next"].as_str() {
        lines.push(n.to_owned());
    }
    lines.join("\n")
}

/// `mac update`: validate, call the daemon (falling back to the label-only call on an older one) and describe the result.
pub async fn update_with<A: UpdateApi>(api: &A, a: &UpdateArgs) -> Result<UpdateOutcome> {
    let body = update_body(a)?;
    let vm = api.find(&a.name).await?;
    let id = vm["id"]
        .as_str()
        .context("the daemon record has no id")?
        .to_owned();
    let answer = match api.update_instance(&id, body.clone()).await {
        Ok(v) => v,
        Err(e) if route_missing(&e) => update_via_vm_patch(api, a, &id, &e).await?,
        Err(e) => {
            return Err(crate::mac_instance::explain(
                e,
                &format!("update {}", a.name),
            ));
        }
    };
    let pending = answer["pending_restart"]
        .as_array()
        .is_some_and(|p| !p.is_empty());
    let text = answer_text(&a.name, &answer);
    let document = a.json.then(|| {
        let labels = labels_part(a.restart.as_deref(), &a.label, &a.rm_label).unwrap_or_default();
        crate::mac_json::update_document_ext(
            &a.name,
            &id,
            &json!({ "labels": labels }),
            &answer,
            body["cpus"].clone(),
            body["memory_mib"].clone(),
        )
    });
    let error = pending.then(|| {
        coded(
            format!(
                "{} was not resized: {}",
                a.name,
                answer["next"].as_str().unwrap_or("it needs a restart")
            ),
            "MAC_UPDATE_NEEDS_RESTART",
        )
    });
    Ok(UpdateOutcome {
        // The pending text is in the error already; do not print it twice.
        progress: (!text.is_empty())
            .then(|| {
                if pending {
                    text.lines()
                        .filter(|l| {
                            !l.contains("NOT applied") && Some(*l) != answer["next"].as_str()
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    text.clone()
                }
            })
            .filter(|t| !t.is_empty()),
        document,
        error,
    })
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

    fn life(args: &[&str]) -> LifeOpts {
        match parse(&[&["run"][..], args, &["img:1"]].concat()) {
            crate::mac_cmd::MacCommand::Run { opts, .. } => opts.life,
            _ => unreachable!(),
        }
    }

    fn hc() -> Healthcheck {
        Healthcheck {
            cmd: Cmd::Shell("true".into()),
            interval_secs: 60,
            timeout_secs: 10,
            retries: 5,
            start_period_secs: 0,
        }
    }

    fn msg(r: Result<Option<HcSetting>>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn defaults_change_nothing() {
        let l = life(&[]);
        assert_eq!(l.pull, PullPolicy::Missing);
        assert!(l.used_flags().is_empty());
        assert_eq!(l.resolve_healthcheck(Some(&hc())).unwrap(), None);
        l.check_supported().unwrap();
    }

    #[test]
    fn health_cmd_builds_a_healthcheck_with_docker_defaults() {
        let l = life(&["--health-cmd", "curl -f localhost"]);
        let Some(HcSetting::Cmd(h)) = l.resolve_healthcheck(None).unwrap() else {
            panic!()
        };
        assert_eq!(h.cmd, Cmd::Shell("curl -f localhost".into()));
        assert_eq!(
            (
                h.interval_secs,
                h.timeout_secs,
                h.retries,
                h.start_period_secs
            ),
            (30, 30, 3, 0)
        );
    }

    #[test]
    fn health_flags_override_and_tune_the_image_check() {
        let l = life(&[
            "--health-cmd",
            "true",
            "--health-interval",
            "2m",
            "--health-probe-timeout",
            "5s",
            "--health-retries",
            "7",
            "--health-start-period",
            "90s",
        ]);
        let Some(HcSetting::Cmd(h)) = l.resolve_healthcheck(Some(&hc())).unwrap() else {
            panic!()
        };
        assert_eq!(
            (
                h.interval_secs,
                h.timeout_secs,
                h.retries,
                h.start_period_secs
            ),
            (120, 5, 7, 90)
        );
        let tune = life(&["--health-retries", "9"]);
        let Some(HcSetting::Cmd(h)) = tune.resolve_healthcheck(Some(&hc())).unwrap() else {
            panic!()
        };
        assert_eq!(
            (h.retries, h.interval_secs, h.cmd),
            (9, 60, Cmd::Shell("true".into()))
        );
    }

    #[test]
    fn health_flag_mistakes_say_what_to_do() {
        let e = msg(life(&["--health-retries", "2"]).resolve_healthcheck(None));
        assert!(
            e.contains("this image has none") && e.contains("--health-cmd"),
            "{e}"
        );
        assert!(e.contains("fluxctl explain MAC_HEALTHCHECK_FLAGS"));
        let e = msg(life(&["--no-healthcheck", "--health-cmd", "x"]).resolve_healthcheck(None));
        assert!(e.contains("cannot be combined"), "{e}");
        let e = msg(
            life(&["--health-cmd", "x", "--health-interval", "soon"]).resolve_healthcheck(None)
        );
        assert!(
            e.contains("--health-interval") && e.contains("not a duration"),
            "{e}"
        );
        let e =
            msg(life(&["--health-cmd", "x", "--health-interval", "0s"]).resolve_healthcheck(None));
        assert!(e.contains("at least 1s"), "{e}");
        let e =
            msg(life(&["--health-cmd", "x", "--health-retries", "0"]).resolve_healthcheck(None));
        assert!(e.contains("at least 1"), "{e}");
    }

    #[test]
    fn no_healthcheck_turns_the_image_check_off() {
        let l = life(&["--no-healthcheck"]);
        assert_eq!(
            l.resolve_healthcheck(Some(&hc())).unwrap(),
            Some(HcSetting::Off)
        );
        assert_eq!(
            healthcheck_label(&HcSetting::Off),
            (HEALTHCHECK_LABEL, "none".into())
        );
    }

    #[test]
    fn the_label_round_trips_into_the_health_module() {
        let (k, v) = healthcheck_label(&HcSetting::Cmd(hc()));
        let vm = json!({"labels": {k: v}});
        assert_eq!(crate::mac_health::override_of(&vm), Some(Some(hc())));
        assert_eq!(crate::mac_health::healthcheck_of(&vm), Some(hc()));
        let off = json!({"labels": {HEALTHCHECK_LABEL: "none"}});
        assert_eq!(crate::mac_health::healthcheck_of(&off), None);
        assert_eq!(crate::mac_health::override_of(&json!({"labels": {}})), None);
    }

    #[test]
    fn entrypoint_replaces_and_drops_cmd() {
        let mut cfg = ImageConfig {
            entrypoint: Some(Cmd::Shell("old".into())),
            cmd: Some(Cmd::Shell("default-arg".into())),
            ..Default::default()
        };
        apply_entrypoint(&mut cfg, None).unwrap();
        assert!(cfg.cmd.is_some());
        apply_entrypoint(&mut cfg, Some("/bin/sh -c")).unwrap();
        assert_eq!(cfg.entrypoint, Some(Cmd::Shell("/bin/sh -c".into())));
        assert_eq!(cfg.cmd, None);
        let text = crate::mac_config::effective_command(&cfg, &["echo hi".into()], &[], None);
        assert_eq!(text.as_deref(), Some("/bin/sh -c echo hi"));
        apply_entrypoint(&mut cfg, Some("")).unwrap();
        assert_eq!(cfg.entrypoint, None);
        apply_entrypoint(&mut cfg, Some(r#"["/bin/echo","a b"]"#)).unwrap();
        assert_eq!(
            cfg.entrypoint,
            Some(Cmd::Exec(vec!["/bin/echo".into(), "a b".into()]))
        );
    }

    #[test]
    fn pull_policy_decisions() {
        let reg = "reg.example.com/mac/xcode:15";
        assert_eq!(
            decide_pull(PullPolicy::Missing, reg, true),
            PullStep::UseLocal
        );
        assert_eq!(decide_pull(PullPolicy::Missing, reg, false), PullStep::Pull);
        assert_eq!(decide_pull(PullPolicy::Always, reg, true), PullStep::Pull);
        assert_eq!(
            decide_pull(PullPolicy::Never, reg, true),
            PullStep::UseLocal
        );
        assert!(
            matches!(decide_pull(PullPolicy::Never, reg, false), PullStep::Refuse(m) if m.contains("--pull never") && m.contains("mac pull"))
        );
        assert!(
            matches!(decide_pull(PullPolicy::Missing, "xcode:15", false), PullStep::Refuse(m) if m.contains("cannot be pulled"))
        );
        assert!(
            matches!(decide_pull(PullPolicy::Always, "xcode:15", true), PullStep::Refuse(m) if m.contains("--pull always needs a registry"))
        );
        assert_eq!(
            decide_pull(PullPolicy::Missing, "xcode:15", true),
            PullStep::UseLocal
        );
        assert!(is_registry_ref("localhost:5000/x") && is_registry_ref("ghcr.io/a/b:1"));
        assert!(!is_registry_ref("macos-xcode:15") && !is_registry_ref("team/xcode:15"));
    }

    #[test]
    fn network_and_platform_are_refused_helpfully() {
        let e = life(&["--network", "host"])
            .check_supported()
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("one network") && e.contains("-p HOST:GUEST"),
            "{e}"
        );
        assert!(e.contains("fluxctl explain MAC_FLAG_NOT_APPLICABLE"));
        let e = life(&["--platform", "linux/amd64"])
            .check_supported()
            .unwrap_err()
            .to_string();
        assert!(e.contains("darwin/arm64"), "{e}");
        // Hidden from --help, still refused with a reason rather than "unexpected argument".
        assert!(parse(&["create", "--network", "x", "img:1"]).is_client_side());
        assert!(parse(&["run", "--platform", "x", "img:1"]).is_client_side());
    }

    #[test]
    fn run_flags_parse() {
        let l = life(&["--pull", "always", "--entrypoint", "/bin/sh"]);
        assert_eq!(l.pull, PullPolicy::Always);
        assert_eq!(l.entrypoint.as_deref(), Some("/bin/sh"));
        assert_eq!(l.used_flags(), vec!["--entrypoint", "--pull"]);
        assert!(T::try_parse_from(["t", "run", "--pull", "sometimes", "i:1"]).is_err());
    }

    #[test]
    fn create_takes_vm_flags_and_refuses_run_only_ones() {
        use crate::mac_cmd::MacCommand as C;
        let c = parse(&[
            "create",
            "--name",
            "ci",
            "--cpus",
            "4",
            "--memory",
            "8g",
            "--restart",
            "always",
            "-p",
            "2222:22",
            "--health-cmd",
            "true",
            "img:1",
        ]);
        let C::Life(l) = &c else { panic!() };
        assert!(l.refusal().is_none() && !c.is_client_side());
        for args in [
            vec!["create", "-e", "A=1", "img:1"],
            vec!["create", "-d", "img:1"],
            vec!["create", "--entrypoint", "x", "img:1"],
            vec!["create", "img:1", "--", "ls"],
            vec!["create", "--rm", "img:1"],
        ] {
            let c = parse(&args);
            let C::Life(l) = &c else { panic!() };
            let e = l
                .refusal()
                .unwrap_or_else(|| panic!("{args:?}"))
                .to_string();
            assert!(
                e.contains("stores no command") && e.contains("mac exec"),
                "{e}"
            );
            assert!(e.contains("fluxctl explain MAC_CREATE_RUN_ONLY"));
            assert!(c.is_client_side(), "the refusal needs no daemon");
        }
    }

    #[test]
    fn kill_signal_maps_or_refuses() {
        use crate::mac_cmd::MacCommand as C;
        for ok in ["KILL", "kill", "SIGKILL", "9"] {
            signal_refusal(ok).unwrap();
        }
        let e = signal_refusal("TERM").unwrap_err().to_string();
        assert!(
            e.contains("mac stop") && e.contains("MAC_KILL_SIGNAL"),
            "{e}"
        );
        let e = signal_refusal("USR1").unwrap_err().to_string();
        assert!(e.contains("mac exec NAME -- kill"), "{e}");
        assert!(parse(&["kill", "-s", "HUP", "a"]).is_client_side());
        let c = parse(&["kill", "--signal", "KILL", "a", "b"]);
        assert!(!c.is_client_side());
        let C::Life(MacLife::Kill { names, .. }) = c else {
            panic!()
        };
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn kill_needs_a_live_instance() {
        for st in ["stopped", "failed", "created"] {
            let e = check_killable(&json!({"status": st}), "ci")
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("nothing to kill") && e.contains("mac start ci"),
                "{e}"
            );
        }
        check_killable(&json!({"status": "running"}), "ci").unwrap();
        check_killable(&json!({"status": "paused"}), "ci").unwrap();
        let e = kill_unavailable("ci", &anyhow::anyhow!("POST /v1/vms/x/kill: 404 Not Found"))
            .to_string();
        assert!(
            e.contains("older than this fluxctl") && e.contains("mac stop ci"),
            "{e}"
        );
    }

    #[test]
    fn update_patch_sets_and_clears() {
        let p = update_patch(Some("always"), &["team=ios".into()], &[]).unwrap();
        assert_eq!(
            p,
            json!({"labels": {"fluxvm.mac.restart": "always", "team": "ios"}})
        );
        let p = update_patch(Some("no"), &[], &["old".into()]).unwrap();
        assert_eq!(
            p,
            json!({"labels": {"fluxvm.mac.restart": null, "old": null}})
        );
        assert!(
            update_patch(Some("sometimes"), &[], &[])
                .unwrap_err()
                .to_string()
                .contains("restart policy")
        );
        assert!(
            update_patch(None, &["fluxvm.mac.image=x".into()], &[])
                .unwrap_err()
                .to_string()
                .contains("reserved")
        );
        assert!(update_patch(None, &[], &["fluxvm.mac.image".into()]).is_err());
        assert!(update_patch(None, &["a=1".into()], &["a".into()]).is_err());
        assert!(update_patch(None, &["novalue".into()], &[]).is_err());
        let e = update_patch(None, &[], &[]).unwrap_err().to_string();
        assert!(
            e.contains("nothing to update") && e.contains("--restart"),
            "{e}"
        );
    }

    fn args(v: &[&str]) -> UpdateArgs {
        let c = parse(&[&["update", "ci"][..], v].concat());
        let crate::mac_cmd::MacCommand::Life(MacLife::Update {
            cpus,
            memory,
            restart,
            label,
            rm_label,
            restart_now,
            no_checkpoint,
            json,
            ..
        }) = c
        else {
            panic!()
        };
        UpdateArgs {
            name: "ci".into(),
            cpus,
            memory,
            restart,
            label,
            rm_label,
            restart_now,
            no_checkpoint,
            json,
        }
    }

    #[test]
    fn update_body_carries_size_policy_and_labels() {
        let b = update_body(&args(&[
            "--cpus",
            "8",
            "--memory",
            "16g",
            "--restart",
            "always",
            "--label",
            "team=ios",
            "--rm-label",
            "old",
            "--restart-now",
            "--no-checkpoint",
        ]))
        .unwrap();
        assert_eq!(
            b,
            json!({"cpus": 8, "memory_mib": 16384, "restart": "always", "labels": {"team": "ios"},
                "rm_labels": ["old"], "restart_now": true, "checkpoint": false})
        );
        assert_eq!(
            update_body(&args(&["--memory", "4096"])).unwrap(),
            json!({"memory_mib": 4096})
        );
    }

    #[test]
    fn update_mistakes_are_caught_before_the_daemon() {
        let msg = |v: &[&str]| update_body(&args(v)).unwrap_err().to_string();
        assert!(msg(&[]).contains("nothing to update"));
        assert!(msg(&["--memory", "lots"]).contains("not a memory size"));
        assert!(msg(&["--memory", "64m"]).contains("512 MiB"));
        assert!(msg(&["--cpus", "0"]).contains("at least 1"));
        assert!(msg(&["--label", "a=b", "--restart-now"]).contains("--cpus or --memory"));
        assert!(
            T::try_parse_from(["t", "update", "ci", "--no-checkpoint", "--cpus", "2"]).is_err(),
            "--no-checkpoint needs --restart-now"
        );
    }

    #[test]
    fn update_refuses_env_before_touching_anything() {
        use crate::mac_cmd::MacCommand as C;
        let c = parse(&["update", "ci", "-e", "A=b", "--restart", "always"]);
        assert!(c.is_client_side());
        let C::Life(l) = &c else { panic!() };
        let e = l.refusal().unwrap().to_string();
        assert!(
            e.contains("Nothing was changed") && e.contains("mac exec ci -e KEY=VALUE"),
            "{e}"
        );
        assert!(e.contains("fluxctl explain MAC_UPDATE_ENV_NOT_STORED"));
        let ok = parse(&["update", "ci", "--cpus", "8", "--label", "a=b"]);
        assert!(!ok.is_client_side(), "sizes go to the daemon now");
    }

    #[derive(Default)]
    struct Fake {
        calls: std::cell::RefCell<Vec<String>>,
        /// The daemon's answer to PATCH /v1/mac/instances/{id}, or an error text.
        answer: Option<Result<Value, String>>,
    }

    impl UpdateApi for Fake {
        async fn find(&self, name: &str) -> Result<Value> {
            self.calls.borrow_mut().push(format!("find {name}"));
            Ok(json!({"id": "vm-1", "name": name}))
        }
        async fn update_instance(&self, id: &str, body: Value) -> Result<Value> {
            self.calls
                .borrow_mut()
                .push(format!("instance {id} {body}"));
            match self.answer.clone() {
                Some(Ok(v)) => Ok(v),
                Some(Err(e)) => bail!("{e}"),
                None => bail!("PATCH /v1/mac/instances/vm-1: 404 Not Found"),
            }
        }
        async fn patch_vm(&self, id: &str, body: Value) -> Result<Value> {
            self.calls.borrow_mut().push(format!("vm {id} {body}"));
            Ok(json!({}))
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn a_new_daemon_answer_is_described_and_a_restart_need_fails_the_command() {
        let f = Fake {
            answer: Some(Ok(
                json!({"ok": false, "applied": ["labels and restart policy"],
                "pending_restart": [{"change": "cpus 4 -> 8"}], "restarted": false, "checkpoint": null,
                "explain": "MAC_UPDATE_NEEDS_RESTART",
                "next": "ci is running; to apply it now: `fluxctl mac update ci --cpus 8 --restart-now`"}),
            )),
            ..Fake::default()
        };
        let out = run(update_with(
            &f,
            &args(&["--cpus", "8", "--restart", "always", "--json"]),
        ))
        .unwrap();
        let e = out.error.unwrap().to_string();
        assert!(
            e.contains("not resized")
                && e.contains("--restart-now")
                && e.contains("fluxctl explain MAC_UPDATE_NEEDS_RESTART"),
            "{e}"
        );
        assert_eq!(
            out.progress.as_deref(),
            Some("ci: applied labels and restart policy")
        );
        let d = out.document.unwrap();
        assert_eq!(
            (d["cpus"].as_u64(), d["restarted"].as_bool()),
            (Some(8), Some(false))
        );
        assert_eq!(d["pending_restart"][0]["change"], "cpus 4 -> 8");
        assert_eq!(d["restart_policy"], "always");
    }

    #[test]
    fn a_restart_now_answer_says_what_was_saved_and_how_to_go_back() {
        let f = Fake {
            answer: Some(Ok(
                json!({"ok": true, "applied": ["cpus 4 -> 8"], "pending_restart": [],
                "restarted": true, "checkpoint": "auto-before-update-5", "next": null}),
            )),
            ..Fake::default()
        };
        let out = run(update_with(&f, &args(&["--cpus", "8", "--restart-now"]))).unwrap();
        assert!(out.error.is_none());
        let t = out.progress.unwrap();
        assert!(
            t.contains("applied cpus 4 -> 8")
                && t.contains("checkpoint restore ci auto-before-update-5")
                && t.contains("restarted"),
            "{t}"
        );
        assert!(f.calls.borrow()[1].contains("\"restart_now\":true"));
    }

    #[test]
    fn an_older_daemon_still_takes_labels_but_explains_a_resize() {
        let f = Fake::default();
        let out = run(update_with(
            &f,
            &args(&["--label", "a=b", "--rm-label", "x"]),
        ))
        .unwrap();
        assert!(out.error.is_none());
        assert!(
            f.calls
                .borrow()
                .last()
                .unwrap()
                .starts_with("vm vm-1 {\"labels\":{\"a\":\"b\",\"x\":null}}")
        );
        let g = Fake::default();
        let e = run(update_with(&g, &args(&["--cpus", "8"])))
            .err()
            .unwrap()
            .to_string();
        assert!(
            e.contains("older than this fluxctl")
                && e.contains("Nothing was changed")
                && e.contains("mac commit ci"),
            "{e}"
        );
        assert!(e.contains("fluxctl explain MAC_UPDATE_NOT_SUPPORTED"));
        assert!(!g.calls.borrow().iter().any(|c| c.starts_with("vm ")));
    }

    #[test]
    fn a_daemon_refusal_is_passed_on_with_its_advice() {
        let f = Fake {
            answer: Some(Err(
                "PATCH x: 409 Conflict: ci: the checkpoint before the update could not be saved"
                    .into(),
            )),
            ..Fake::default()
        };
        let e = run(update_with(&f, &args(&["--cpus", "8", "--restart-now"])))
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("checkpoint before the update"), "{e}");
    }

    #[test]
    fn start_patch_clears_markers_and_stamps_the_start() {
        let vm = json!({"labels": {STOPPED_LABEL: "true", NOT_STARTED_LABEL: "true"}});
        assert_eq!(
            start_patch(&vm, 42),
            json!({"labels": {STOPPED_LABEL: null, NOT_STARTED_LABEL: null, HEALTH_START_LABEL: "42"}})
        );
        assert_eq!(
            start_patch(&json!({"labels": {}}), 7),
            json!({"labels": {HEALTH_START_LABEL: "7"}})
        );
    }

    #[test]
    fn create_body_and_labels_carry_the_healthcheck() {
        let spec = LaunchSpec {
            name: "ci".into(),
            image: "img:1".into(),
            cpus: Some(4),
            memory_mib: Some(8192),
            healthcheck: Some(HcSetting::Cmd(hc())),
            ..Default::default()
        };
        let body =
            crate::mac_instance::create_body(&spec, std::path::Path::new("/d/disk.raw")).unwrap();
        assert_eq!(body["vcpus"], 4);
        assert_eq!(body["memory_mib"], 8192);
        let patch = crate::mac_instance::labels_patch(&spec).unwrap();
        let vm = json!({"labels": patch["labels"]});
        assert_eq!(crate::mac_health::healthcheck_of(&vm), Some(hc()));
        let none = LaunchSpec {
            healthcheck: Some(HcSetting::Off),
            ..spec.clone()
        };
        let vm = json!({"labels": crate::mac_instance::labels_patch(&none).unwrap()["labels"]});
        assert_eq!(crate::mac_health::healthcheck_of(&vm), None);
        let plain = crate::mac_instance::labels_patch(&LaunchSpec {
            healthcheck: None,
            ..spec
        })
        .unwrap();
        assert!(plain["labels"].get(HEALTHCHECK_LABEL).is_none());
    }

    #[test]
    fn a_created_instance_is_not_started_by_a_policy() {
        let vm = json!({"status": "created", "labels": {
            "fluxvm.mac.restart": "always", NOT_STARTED_LABEL: "true"}});
        assert!(!crate::mac_more::should_restart(&vm));
        let vm = json!({"status": "stopped", "labels": {"fluxvm.mac.restart": "always"}});
        assert!(crate::mac_more::should_restart(&vm));
    }

    #[test]
    fn start_period_failures_do_not_count() {
        let h = Healthcheck {
            start_period_secs: 60,
            ..hc()
        };
        let vm = json!({"labels": {HEALTH_START_LABEL: "100"}});
        assert!(crate::mac_health::in_start_period(&vm, &h, None, 120));
        assert!(!crate::mac_health::in_start_period(&vm, &h, None, 160));
        let healthy = crate::mac_health::Health {
            status: crate::mac_health::Status::Healthy,
            streak: 0,
            checked_unix: 110,
        };
        assert!(!crate::mac_health::in_start_period(
            &vm,
            &h,
            Some(&healthy),
            120
        ));
        assert!(!crate::mac_health::in_start_period(
            &json!({"labels": {}}),
            &h,
            None,
            120
        ));
        assert!(!crate::mac_health::in_start_period(&vm, &hc(), None, 120));
    }
}
