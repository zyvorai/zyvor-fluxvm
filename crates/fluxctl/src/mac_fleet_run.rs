// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet run | drain | resume | templates`, `apply --prune`, and the same placement behind `fluxctl mac run --fleet`.
//!
//! The scheduler (`mac_fleet_sched`) decides which Mac; the Mac's own daemon creates the guest, by running `fluxctl mac run`
//! there over ssh. No guest is created from here and no daemon is reached from here. Every ssh call has a deadline.

use crate::mac_fleet::{
    Conn, HostResult, StepResult, Which, blank, fail, finish, jobs, last_lines, par_map, render_table,
    resolve, transport,
};
use crate::mac_fleet_remote::{
    PRELUDE, Secret, parse_facts, probe_script, remove_script, remote, scrub, sh_quote,
};
use crate::mac_fleet_sched::*;
use crate::mac_fleet_ssh::{Target, Transport, explain_ssh_failure};
use anyhow::{Result, bail};
use std::sync::Mutex;
use std::time::Duration;

// ---- clock -----------------------------------------------------------------------------------------------------------

pub trait Clock: Send + Sync {
    fn now(&self) -> u64;
    fn sleep(&self, secs: u64);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
    fn sleep(&self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

// ---- flags -----------------------------------------------------------------------------------------------------------

/// Placement and deadline flags shared by `mac-fleet run`, `mac run --fleet` and `mac runner`.
#[derive(clap::Args, Clone, Debug)]
pub struct PlaceFlags {
    /// Only Macs with this inventory label (`key=value` or a word). Repeatable; all must match.
    #[arg(long = "host-label", value_name = "LABEL")]
    pub host_labels: Vec<String>,
    /// Run on this Mac only (its inventory name).
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,
    /// Only a Mac with at least this much free disk.
    #[arg(long, value_name = "GIB", default_value_t = 0)]
    pub disk_gib: u64,
    /// Skip a Mac that runs on battery.
    #[arg(long)]
    pub ac_only: bool,
    /// Wait up to this many seconds for a free macOS slot (each Mac runs 2 at most) instead of failing at once.
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub queue_timeout: u64,
    /// Seconds between looks for a free slot while waiting.
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    pub poll: u64,
    /// Kill the job after this many seconds (the guest is deleted).
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    pub timeout: u64,
    /// Deadline for the read-only look at each Mac.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub probe_timeout: u64,
}

impl PlaceFlags {
    pub fn job(&self, template: &str, cpus: u32, memory_mib: u64) -> Result<JobSpec> {
        for l in &self.host_labels {
            check_label(l)?;
        }
        Ok(JobSpec {
            template: TemplateRef::parse(template)?,
            cpus,
            memory_mib,
            disk_gib: self.disk_gib,
            labels: self.host_labels.clone(),
            ac_only: self.ac_only,
            host: self.host.clone(),
        })
    }
}

/// Flattened into `fluxctl mac run`: `--fleet` hands the placement to the scheduler instead of this Mac's own daemon.
#[derive(clap::Args, Clone, Debug)]
pub struct FleetRun {
    /// Run on whichever Mac of this inventory (`fleet.yaml`) the scheduler picks, over ssh: one that holds the image and has a
    /// free macOS slot. IMAGE is then the template (`name:tag[@sha256:HEX]`) on that Mac. Needs CMD.
    #[arg(long, value_name = "INVENTORY", num_args = 0..=1, default_missing_value = "fleet")]
    pub fleet: Option<std::path::PathBuf>,
    #[command(flatten)]
    pub place: RunPlaceFlags,
}

/// `PlaceFlags` minus `--queue-timeout`: `mac run` already has `--queue [--queue-timeout]`, and `--fleet` uses those.
#[derive(clap::Args, Clone, Debug)]
pub struct RunPlaceFlags {
    /// Only Macs with this inventory label (`key=value` or a word). Repeatable; all must match.
    #[arg(long = "host-label", value_name = "LABEL")]
    pub host_labels: Vec<String>,
    /// Run on this Mac only (its inventory name).
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,
    /// Only a Mac with at least this much free disk.
    #[arg(long, value_name = "GIB", default_value_t = 0)]
    pub disk_gib: u64,
    /// Skip a Mac that runs on battery.
    #[arg(long)]
    pub ac_only: bool,
    /// Seconds between looks for a free slot while waiting (with --queue).
    #[arg(long, value_name = "SECS", default_value_t = 10)]
    pub poll: u64,
    /// Kill the job after this many seconds (the guest is deleted).
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    pub timeout: u64,
    /// Deadline for the read-only look at each Mac.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub probe_timeout: u64,
}

impl RunPlaceFlags {
    /// The full set, with the wait taken from `mac run --queue [--queue-timeout]` (0 = fail at once).
    pub fn into_place(self, queue: &crate::mac_queue::QueueOpts) -> PlaceFlags {
        PlaceFlags {
            host_labels: self.host_labels,
            host: self.host,
            disk_gib: self.disk_gib,
            ac_only: self.ac_only,
            queue_timeout: if queue.queue { queue.queue_timeout } else { 0 },
            poll: self.poll,
            timeout: self.timeout,
            probe_timeout: self.probe_timeout,
        }
    }
}

fn default_conn() -> Conn {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct W {
        #[command(flatten)]
        c: Conn,
    }
    W::parse_from(["fluxctl"]).c
}

/// `fluxctl mac run --fleet`: the flags that only make sense against this Mac's daemon are refused, not silently dropped.
#[allow(clippy::too_many_arguments)]
pub fn mac_run_fleet(
    publish: Vec<String>,
    name: Option<String>,
    user: Option<String>,
    wait: u64,
    opts: crate::mac_instance::RunOpts,
    fleet: FleetRun,
    queue: &crate::mac_queue::QueueOpts,
    image: String,
    cmd: Vec<String>,
) -> Result<()> {
    let mut unsupported = vec![];
    for (flag, used) in [
        ("-p/--publish", !publish.is_empty()),
        ("--name", name.is_some()),
        ("-e/--env", !opts.env.is_empty()),
        ("--env-file", !opts.env_file.is_empty()),
        ("-v/--volume", !opts.volume.is_empty()),
        ("-w/--workdir", opts.workdir.is_some()),
        ("--label", !opts.label.is_empty()),
        ("--restart", opts.restart != "no"),
        ("-d/--detach", opts.detach),
    ] {
        if used {
            unsupported.push(flag);
        }
    }
    if !unsupported.is_empty() {
        bail!(
            "{} cannot be used with --fleet (the guest is created on another Mac, runs CMD and is deleted); use `fluxctl mac run` on that Mac",
            unsupported.join(", ")
        );
    }
    if cmd.is_empty() {
        bail!("--fleet needs a command after `--`: the guest is deleted when it finishes");
    }
    let Some(inv) = fleet.fleet else {
        bail!("--fleet needs an inventory file");
    };
    let conn = default_conn();
    let which = Which {
        hosts: vec![],
        inventory: Some(inv),
        all: true,
    };
    let (targets, inv) = resolve(&which, &conn)?;
    let memory_mib = match &opts.memory {
        Some(m) => crate::mac_config::parse_memory_mib(m)?,
        None => 0,
    };
    let place = fleet.place.into_place(queue);
    let job = place.job(&image, u32::from(opts.cpus.unwrap_or(0)), memory_mib)?;
    let tr = transport(&conn, inv.as_ref());
    let run = JobRun {
        id: new_job_id(),
        image: job.template.name.clone(),
        cmd,
        cpus: opts.cpus,
        memory: opts.memory,
        user,
        wait,
        secret_env: None,
    };
    run_placed(
        tr.as_ref(),
        &targets,
        jobs(&conn),
        &place,
        &job,
        run,
        false,
        &SystemClock,
        &state_path()?,
    )
}

#[derive(clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub which: Which,
    /// The template to run: `name:tag`, or `name:tag@sha256:HEX` to refuse a Mac that holds another digest.
    #[arg(long, value_name = "REF")]
    pub template: String,
    /// Virtual CPUs for the guest (also what the Mac must have free).
    #[arg(long)]
    pub cpus: Option<u8>,
    /// Guest memory: `8g`, `512m`, or MiB.
    #[arg(long, value_name = "SIZE")]
    pub memory: Option<String>,
    /// SSH user inside the guest (default: the template's USER, else `admin`).
    #[arg(long)]
    pub user: Option<String>,
    /// Seconds for the guest to answer on SSH.
    #[arg(long, default_value_t = 900)]
    pub wait: u64,
    #[command(flatten)]
    pub place: PlaceFlags,
    /// The command to run in the guest, after `--`.
    #[arg(last = true, required = true)]
    pub cmd: Vec<String>,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(clap::Args)]
pub struct DrainArgs {
    #[command(flatten)]
    pub which: Which,
    /// Show which Macs are drained instead of changing anything.
    #[arg(long)]
    pub list: bool,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(clap::Subcommand)]
pub enum TemplatesCommand {
    /// Which template (`name:tag`, digest) each Mac holds.
    Ls(TemplatesLsArgs),
    /// Make every Mac hold a template: each Mac pulls it from the registry, then its digest is compared with the one you
    /// pinned; a Mac that ends up with another digest is reported and not used for jobs pinned to that digest.
    Distribute(DistributeArgs),
}

#[derive(clap::Args)]
pub struct TemplatesLsArgs {
    #[command(flatten)]
    pub which: Which,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(clap::Args)]
pub struct DistributeArgs {
    #[command(flatten)]
    pub which: Which,
    /// `registry/repo:tag@sha256:HEX`. The digest is required: it is what is verified on each Mac.
    #[arg(long, value_name = "REF@DIGEST")]
    pub template: String,
    /// Seconds each Mac may take to pull (templates are tens of GB).
    #[arg(long, value_name = "SECS", default_value_t = 3600)]
    pub pull_timeout: u64,
    #[command(flatten)]
    pub conn: Conn,
}

// ---- probing ---------------------------------------------------------------------------------------------------------

/// The read-only look at one Mac, with the scheduler's extra lines.
pub fn view_host(tr: &dyn Transport, target: &Target, timeout: Duration) -> HostView {
    let script = format!("{}{}", probe_script(), sched_probe_script());
    match tr.run_timed(target, &remote(&script), None, Some(timeout)) {
        Err(e) => HostView::unavailable(target, format!("{e:#}")),
        Ok(o) if o.ssh_failed() => {
            HostView::unavailable(target, explain_ssh_failure(target, &o.stderr))
        }
        Ok(o) if !o.ok() => HostView::unavailable(
            target,
            format!("the probe failed: {}", last_lines(&o.stderr, 2)),
        ),
        Ok(o) => HostView::from_facts(target, &parse_facts(&o.stdout), &parse_sched_facts(&o.stdout)),
    }
}

pub fn snapshot(
    tr: &dyn Transport,
    targets: &[Target],
    jobs: usize,
    timeout: Duration,
) -> Vec<HostView> {
    par_map(targets, jobs, |t| view_host(tr, t, timeout))
}

// ---- placing ---------------------------------------------------------------------------------------------------------

pub struct Placer<'a> {
    pub tr: &'a dyn Transport,
    pub targets: &'a [Target],
    pub jobs: usize,
    pub probe_timeout: Duration,
    pub leases: &'a Mutex<Leases>,
    /// Read before every decision, so `mac-fleet drain` from another terminal takes effect within a poll.
    pub state_path: &'a std::path::Path,
    pub clock: &'a dyn Clock,
    pub poll_secs: u64,
    pub log: &'a (dyn Fn(&str) + Sync),
}

#[derive(Debug)]
pub struct Placed {
    pub target: Target,
    pub view: HostView,
    pub lease: String,
}

impl Placer<'_> {
    /// Choose a Mac and lease its slot. With `queue_timeout`, waits (polling) while the only obstacle is a busy Mac.
    pub fn place(&self, lease_id: &str, job: &JobSpec, queue_timeout: u64) -> Result<Placed> {
        let started = self.clock.now();
        let mut last_note = String::new();
        loop {
            if crate::mac_runner_life::requested() {
                bail!("stopping: the runner was asked to stop");
            }
            let views = snapshot(self.tr, self.targets, self.jobs, self.probe_timeout);
            let state = FleetState::load(self.state_path)?;
            let now = self.clock.now();
            let decision = {
                let mut leases = self.leases.lock().unwrap();
                // Other processes' leases (runner, `mac-fleet run`) are read under a file lock; see mac_fleet_lease.
                let guard = crate::mac_fleet_lease::begin(&mut leases, now)?;
                let d = decide(&views, job, &leases, &state, now);
                let mut placed = None;
                if let Decision::Place(c) = &d {
                    leases.acquire(lease_id, &views[c.index].name, job, now);
                    placed = Some((lease_id, views[c.index].name.as_str(), job));
                }
                crate::mac_fleet_lease::commit(guard, placed, now)?;
                d
            };
            match decision {
                Decision::Place(c) => {
                    return Ok(Placed {
                        target: self.targets[c.index].clone(),
                        view: views[c.index].clone(),
                        lease: lease_id.into(),
                    });
                }
                Decision::Never(b) => {
                    bail!("no Mac can take this job:\n  {}", explain(&b));
                }
                Decision::Wait(b) => {
                    let waited = now.saturating_sub(started);
                    if waited >= queue_timeout {
                        bail!(
                            "no Mac has room right now{}:\n  {}",
                            if queue_timeout == 0 {
                                " (--queue-timeout SECS waits for one)".to_owned()
                            } else {
                                format!(" after waiting {waited}s")
                            },
                            explain(&b)
                        );
                    }
                    let note = explain(&b);
                    if note != last_note {
                        (self.log)(&format!("queued: {}", note.replace("\n  ", "; ")));
                        last_note = note;
                    }
                    self.clock.sleep(self.poll_secs.min(queue_timeout - waited).max(1));
                }
            }
        }
    }

    pub fn release(&self, lease: &str) {
        self.leases.lock().unwrap().release(lease);
        crate::mac_fleet_lease::forget(lease);
    }
}

// ---- running a job on the chosen Mac ---------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JobRun {
    /// Unique per job; becomes the instance name `fleet-<id>` and the label `fluxvm.job`.
    pub id: String,
    pub image: String,
    pub cmd: Vec<String>,
    pub cpus: Option<u8>,
    pub memory: Option<String>,
    pub user: Option<String>,
    pub wait: u64,
    /// Name of an environment variable whose value arrives on stdin (read on the Mac, copied into the guest by `-e NAME`),
    /// so it is in no argument and no script.
    pub secret_env: Option<String>,
}

impl JobRun {
    pub fn instance(&self) -> String {
        format!("fleet-{}", self.id)
    }
}

fn safe_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

/// The script that runs the job on a Mac: `fluxctl mac run --rm` there, so the Mac's own daemon creates the guest.
pub fn run_job_script(j: &JobRun) -> Result<String> {
    if !safe_word(&j.id) {
        bail!("bad job id {:?}", j.id);
    }
    if let Some(e) = &j.secret_env
        && !(e.bytes().all(|c| c.is_ascii_uppercase() || c == b'_' || c.is_ascii_digit())
            && !e.is_empty())
    {
        bail!("bad environment variable name {e:?}");
    }
    let mut s = format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac' >&2; exit 127; }}\n"
    );
    if let Some(e) = &j.secret_env {
        s.push_str(&format!("IFS= read -r {e}\nexport {e}\n"));
    }
    s.push_str(&format!(
        "exec \"$FLUXCTL\" mac run --rm --name {} --label fluxvm.owner=mac-fleet --label {} --wait {}",
        sh_quote(&j.instance()),
        sh_quote(&format!("fluxvm.job={}", j.id)),
        j.wait
    ));
    if let Some(c) = j.cpus {
        s.push_str(&format!(" --cpus {c}"));
    }
    if let Some(m) = &j.memory {
        s.push_str(&format!(" --memory {}", sh_quote(m)));
    }
    if let Some(u) = &j.user {
        s.push_str(&format!(" --user {}", sh_quote(u)));
    }
    if let Some(e) = &j.secret_env {
        s.push_str(&format!(" -e {e}"));
    }
    s.push_str(&format!(" {} --", sh_quote(&j.image)));
    for w in &j.cmd {
        s.push(' ');
        s.push_str(&sh_quote(w));
    }
    s.push('\n');
    Ok(s)
}

#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub host: String,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// The ssh connection itself failed, so the job never ran (or its result is unknown).
    pub transport_error: Option<String>,
}

impl JobOutcome {
    pub fn ok(&self) -> bool {
        self.code == 0 && self.transport_error.is_none()
    }
}

/// Run the job and make sure its guest is gone afterwards. `always_delete` also asks the Mac to delete the instance after a
/// clean exit (the runner promises one VM per job); otherwise only a failed, killed or lost job gets that second call.
pub fn run_job(
    tr: &dyn Transport,
    target: &Target,
    j: &JobRun,
    secret: Option<&Secret>,
    timeout: Duration,
    always_delete: bool,
) -> JobOutcome {
    let mut out = JobOutcome {
        host: target.label(),
        code: 255,
        stdout: String::new(),
        stderr: String::new(),
        timed_out: false,
        transport_error: None,
    };
    let script = match run_job_script(j) {
        Ok(s) => s,
        Err(e) => {
            out.transport_error = Some(format!("{e:#}"));
            return out;
        }
    };
    match tr.run_timed(target, &remote(&script), secret, Some(timeout)) {
        Err(e) => out.transport_error = Some(scrub(&format!("{e:#}"), secret)),
        Ok(o) if o.ssh_failed() => {
            out.transport_error = Some(scrub(&explain_ssh_failure(target, &o.stderr), secret));
        }
        Ok(o) => {
            out.code = o.code;
            out.timed_out = o.code == crate::mac_fleet_timed::TIMED_OUT && o.stderr.contains("timed out after");
            out.stdout = scrub(&o.stdout, secret);
            out.stderr = scrub(&o.stderr, secret);
        }
    }
    if always_delete || !out.ok() {
        delete_instance(tr, target, &j.instance());
    }
    out
}

/// Best effort: the instance is usually already gone (`--rm`), and a Mac that cannot be reached cannot be asked.
pub fn delete_instance(tr: &dyn Transport, target: &Target, name: &str) -> bool {
    let script = format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] && \"$FLUXCTL\" mac rm {} 2>&1\n",
        sh_quote(name)
    );
    tr.run_timed(target, &remote(&script), None, Some(Duration::from_secs(120)))
        .map(|o| o.ok())
        .unwrap_or(false)
}

fn lease_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", n & 0xffff_ffff_ffff, std::process::id() & 0xffff)
}

pub fn new_job_id() -> String {
    lease_id()
}

/// `mac-fleet run` and `mac run --fleet`: place, run, report. The exit status is the job's.
#[allow(clippy::too_many_arguments)]
pub fn run_placed(
    tr: &dyn Transport,
    targets: &[Target],
    conn_jobs: usize,
    place: &PlaceFlags,
    job: &JobSpec,
    mut run: JobRun,
    json: bool,
    clock: &dyn Clock,
    state_path: &std::path::Path,
) -> Result<()> {
    crate::mac_fleet_lease::install(state_path);
    let leases = Mutex::new(Leases::default());
    let log = |m: &str| eprintln!("{m}");
    let placer = Placer {
        tr,
        targets,
        jobs: conn_jobs,
        probe_timeout: Duration::from_secs(place.probe_timeout),
        leases: &leases,
        state_path,
        clock,
        poll_secs: place.poll,
        log: &log,
    };
    let placed = placer.place(&run.id, job, place.queue_timeout)?;
    eprintln!(
        "placed on {} ({}; macOS slots in use {}/2)",
        placed.view.label,
        match holds(&placed.view.templates, &job.template) {
            Holds::Yes => "holds the template",
            _ => "template not verified",
        },
        placed.view.caps.macos_guests
    );
    run.image = job.template.name.clone();
    let o = run_job(
        tr,
        &placed.target,
        &run,
        None,
        Duration::from_secs(place.timeout),
        false,
    );
    placer.release(&placed.lease);
    if json {
        println!(
            "{}",
            serde_json::json!({"host": o.host, "exit_code": o.code, "timed_out": o.timed_out,
                "error": o.transport_error, "stdout": o.stdout, "stderr": o.stderr})
        );
    } else {
        print!("{}", o.stdout);
        eprint!("{}", o.stderr);
    }
    if let Some(e) = &o.transport_error {
        bail!("{}: {e}", o.host);
    }
    if o.timed_out {
        bail!(
            "the job ran past --timeout {}s on {} and was stopped (its guest was deleted)",
            place.timeout,
            o.host
        );
    }
    if !o.ok() {
        bail!("the job exited with status {} on {}", o.code, o.host);
    }
    Ok(())
}

// ---- drain and resume ------------------------------------------------------------------------------------------------

pub fn set_drain(
    state_path: &std::path::Path,
    names: &[String],
    drain: bool,
    now: u64,
) -> Result<Vec<String>> {
    let mut st = FleetState::load(state_path)?;
    let mut lines = vec![];
    for n in names {
        let changed = if drain { st.drain(n, now) } else { st.resume(n) };
        lines.push(match (drain, changed) {
            (true, true) => format!("{n}: drained; running jobs finish, no new job is placed"),
            (true, false) => format!("{n}: already drained"),
            (false, true) => format!("{n}: resumed"),
            (false, false) => format!("{n}: was not drained"),
        });
    }
    st.save(state_path)?;
    Ok(lines)
}

pub fn render_drained(st: &FleetState) -> String {
    if st.drained.is_empty() {
        return "no Mac is drained\n".into();
    }
    render_table(
        &["HOST", "DRAINED SINCE (unix)"],
        &st.drained
            .iter()
            .map(|(n, d)| vec![n.clone(), d.since_unix.to_string()])
            .collect::<Vec<_>>(),
    )
}

// ---- templates -------------------------------------------------------------------------------------------------------

/// Registry credentials for the pull, read from this shell's `FLUXVM_REGISTRY_USER` / `FLUXVM_REGISTRY_PASSWORD`, sent on
/// stdin and exported on the Mac only for the pull.
pub struct RegistryAuth {
    pub user: Secret,
    pub password: Secret,
}

impl RegistryAuth {
    pub fn from_env() -> Option<Self> {
        let u = std::env::var("FLUXVM_REGISTRY_USER").ok().filter(|s| !s.is_empty())?;
        let p = std::env::var("FLUXVM_REGISTRY_PASSWORD").ok().filter(|s| !s.is_empty())?;
        Some(Self {
            user: Secret::new(u.into_bytes()),
            password: Secret::new(p.into_bytes()),
        })
    }

    fn stdin(&self) -> Secret {
        let mut b = self.user.expose().to_vec();
        b.push(b'\n');
        b.extend_from_slice(self.password.expose());
        b.push(b'\n');
        Secret::new(b)
    }

    fn scrub(&self, t: &str) -> String {
        scrub(&scrub(t, Some(&self.password)), Some(&self.user))
    }
}

pub fn images_script() -> String {
    format!("{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac' >&2; exit 127; }}\n\"$FLUXCTL\" mac images --json\n")
}

pub fn pull_script(template: &TemplateRef, auth: bool) -> String {
    let mut s = format!("{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac' >&2; exit 127; }}\n");
    if auth {
        s.push_str("IFS= read -r FLUXVM_REGISTRY_USER\nIFS= read -r FLUXVM_REGISTRY_PASSWORD\nexport FLUXVM_REGISTRY_USER FLUXVM_REGISTRY_PASSWORD\n");
    }
    s.push_str(&format!("\"$FLUXCTL\" mac pull {}\n", sh_quote(&template.name)));
    s
}

fn held(tr: &dyn Transport, t: &Target, timeout: Duration) -> std::result::Result<Vec<HeldTemplate>, String> {
    match tr.run_timed(t, &remote(&images_script()), None, Some(timeout)) {
        Err(e) => Err(format!("{e:#}")),
        Ok(o) if o.ssh_failed() => Err(explain_ssh_failure(t, &o.stderr)),
        Ok(o) if !o.ok() => Err(format!("mac images failed: {}", last_lines(&format!("{}\n{}", o.stdout, o.stderr), 2))),
        Ok(o) => Ok(parse_images_json(&o.stdout)),
    }
}

/// Make one Mac hold `template` at the pinned digest.
pub fn distribute_host(
    tr: &dyn Transport,
    target: &Target,
    template: &TemplateRef,
    auth: Option<&RegistryAuth>,
    pull_timeout: Duration,
) -> HostResult {
    let r = blank(target);
    let want = match template.require_digest() {
        Ok(d) => d.to_owned(),
        Err(e) => return fail(r, format!("{e:#}")),
    };
    let step = |name: &str, ok: bool, output: String| StepResult {
        name: name.into(),
        ok,
        output,
    };
    let mut r = r;
    let before = match held(tr, target, Duration::from_secs(60)) {
        Ok(h) => h,
        Err(e) => return fail(r, e),
    };
    if holds(&before, template) == Holds::Yes {
        r.steps.push(step("check", true, format!("already holds {template}")));
        r.ok = true;
        return r;
    }
    let stdin = auth.map(RegistryAuth::stdin);
    let o = match tr.run_timed(
        target,
        &remote(&pull_script(template, auth.is_some())),
        stdin.as_ref(),
        Some(pull_timeout),
    ) {
        Err(e) => return fail(r, format!("pull: {e:#}")),
        Ok(o) => o,
    };
    let clean = |t: &str| match auth {
        Some(a) => a.scrub(t),
        None => t.to_owned(),
    };
    if o.ssh_failed() {
        return fail(r, clean(&explain_ssh_failure(target, &o.stderr)));
    }
    if !o.ok() {
        let text = clean(&last_lines(&format!("{}\n{}", o.stdout, o.stderr), 3));
        r.steps.push(step("pull", false, text.clone()));
        return fail(r, format!("pull failed: {text}"));
    }
    r.steps.push(step("pull", true, clean(&last_lines(&o.stdout, 1))));
    let after = match held(tr, target, Duration::from_secs(60)) {
        Ok(h) => h,
        Err(e) => return fail(r, format!("pulled, but could not verify: {e}")),
    };
    match holds(&after, template) {
        Holds::Yes => {
            r.steps.push(step("verify", true, format!("{} is {want}", template.name)));
            r.ok = true;
            r
        }
        Holds::Stale(got) => {
            r.steps.push(step("verify", false, format!("got {got}")));
            fail(
                r,
                format!(
                    "digest mismatch: {} now is {got}, pinned {want} (the tag moved, or the registry served something else); this Mac will not be used for jobs pinned to {want}",
                    template.name
                ),
            )
        }
        Holds::No => fail(r, format!("pulled, but {} is not in the store", template.name)),
    }
}

pub fn render_templates(views: &[HostView]) -> String {
    let mut rows = vec![];
    for v in views {
        if let Some(u) = &v.unavailable {
            rows.push(vec![v.label.clone(), "-".into(), format!("unavailable: {u}")]);
        } else if !v.templates_known {
            rows.push(vec![v.label.clone(), "-".into(), "(could not list)".into()]);
        } else if v.templates.is_empty() {
            rows.push(vec![v.label.clone(), "-".into(), "(none)".into()]);
        }
        for t in &v.templates {
            rows.push(vec![
                v.label.clone(),
                t.reference.clone(),
                if t.digest.is_empty() { "local".into() } else { t.digest.clone() },
            ]);
        }
    }
    render_table(&["HOST", "TEMPLATE", "DIGEST"], &rows)
}

// ---- apply --prune ---------------------------------------------------------------------------------------------------

/// Remove Macs that left the inventory, one by one: reach it, refuse when it still runs VMs, then `remove` (LaunchAgents and
/// install; VM images and data stay). The caller records which Macs `apply` saw, so the next `apply` can tell.
pub fn prune_host(tr: &dyn Transport, target: &Target) -> HostResult {
    let r = blank(target);
    let o = match tr.run_timed(target, &remote(&probe_script()), None, Some(Duration::from_secs(30))) {
        Err(e) => return fail(r, format!("{e:#}")),
        Ok(o) if o.ssh_failed() => return fail(r, explain_ssh_failure(target, &o.stderr)),
        Ok(o) => o,
    };
    let facts = parse_facts(&o.stdout);
    if facts.running_vms() > 0 {
        return fail(
            r,
            format!(
                "{} VM(s) still run there: stop them (and `mac-fleet drain` it first), then apply --prune again",
                facts.running_vms()
            ),
        );
    }
    let mut r = r;
    match tr.run(target, &remote(&remove_script(false, false)), None) {
        Err(e) => return fail(r, format!("{e:#}")),
        Ok(o) if o.ssh_failed() => return fail(r, explain_ssh_failure(target, &o.stderr)),
        Ok(o) => {
            r.steps.push(StepResult {
                name: "remove".into(),
                ok: o.ok(),
                output: last_lines(&format!("{}\n{}", o.stdout, o.stderr), 2),
            });
            if !o.ok() {
                return fail(r, "remove failed");
            }
        }
    }
    r.ok = true;
    r
}

/// After `apply`: remember the inventory, and list (or with `prune`, remove) the Macs that left it.
pub fn after_apply(
    conn: &Conn,
    tr: &dyn Transport,
    inventory: &str,
    targets: &[Target],
    prune: bool,
    state_path: &std::path::Path,
) -> Result<()> {
    let mut st = FleetState::load(state_path)?;
    let left = st.left_inventory(inventory, targets);
    let mut keep: Vec<AppliedHost> = targets.iter().map(AppliedHost::from_target).collect();
    let mut failed = 0;
    if !left.is_empty() {
        let names = left.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ");
        if !prune {
            eprintln!(
                "{} Mac(s) left the inventory and were not touched: {names} (apply --prune removes FluxVM from them)",
                left.len()
            );
            keep.extend(left);
        } else {
            let ts: Vec<Target> = left.iter().map(AppliedHost::to_target).collect();
            let rs = par_map(&ts, jobs(conn), |t| prune_host(tr, t));
            if !conn.dry_run {
                println!("{}", crate::mac_fleet::render_results("PRUNE", &rs));
            }
            for (l, r) in left.iter().zip(&rs) {
                if !r.ok {
                    failed += 1;
                    keep.push(l.clone()); // still there: try again next time
                }
            }
            if failed > 0 {
                eprintln!("{failed} Mac(s) could not be pruned; they stay on the list for the next `apply --prune`");
            }
        }
    }
    if !conn.dry_run {
        st.applied.insert(inventory.into(), keep);
        st.save(state_path)?;
    }
    if failed > 0 {
        bail!("prune failed on {failed} Mac(s)");
    }
    Ok(())
}

// ---- entry points called from mac_fleet::run -------------------------------------------------------------------------

pub fn run_cmd(a: RunArgs) -> Result<()> {
    let (targets, inv) = resolve(&a.which, &a.conn)?;
    let memory_mib = match &a.memory {
        Some(m) => crate::mac_config::parse_memory_mib(m)?,
        None => 0,
    };
    let job = a.place.job(&a.template, u32::from(a.cpus.unwrap_or(0)), memory_mib)?;
    let tr = transport(&a.conn, inv.as_ref());
    if a.conn.dry_run {
        println!("# dry run: only the read-only look at each Mac is shown; the job is not placed");
        let _ = snapshot(tr.as_ref(), &targets, 1, Duration::from_secs(a.place.probe_timeout));
        if let Some(d) = tr.as_dry() {
            for l in d.lines.lock().unwrap().iter() {
                println!("{l}");
            }
        }
        return Ok(());
    }
    let run = JobRun {
        id: new_job_id(),
        image: job.template.name.clone(),
        cmd: a.cmd,
        cpus: a.cpus,
        memory: a.memory,
        user: a.user,
        wait: a.wait,
        secret_env: None,
    };
    run_placed(
        tr.as_ref(),
        &targets,
        jobs(&a.conn),
        &a.place,
        &job,
        run,
        a.conn.json,
        &SystemClock,
        &state_path()?,
    )
}

pub fn drain_cmd(a: DrainArgs, drain: bool) -> Result<()> {
    let p = state_path()?;
    if a.list {
        let st = FleetState::load(&p)?;
        if a.conn.json {
            println!("{}", serde_json::to_string_pretty(&st.drained)?);
        } else {
            print!("{}", render_drained(&st));
        }
        return Ok(());
    }
    let (targets, _) = resolve(&a.which, &a.conn)?;
    let names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    if a.conn.dry_run {
        println!("# dry run: would {} {}", if drain { "drain" } else { "resume" }, names.join(", "));
        return Ok(());
    }
    for l in set_drain(&p, &names, drain, SystemClock.now())? {
        println!("{l}");
    }
    Ok(())
}

pub fn templates_cmd(c: TemplatesCommand) -> Result<()> {
    match c {
        TemplatesCommand::Ls(a) => {
            let (targets, inv) = resolve(&a.which, &a.conn)?;
            let tr = transport(&a.conn, inv.as_ref());
            let views = snapshot(tr.as_ref(), &targets, jobs(&a.conn), Duration::from_secs(30));
            if a.conn.dry_run {
                if let Some(d) = tr.as_dry() {
                    for l in d.lines.lock().unwrap().iter() {
                        println!("{l}");
                    }
                }
                return Ok(());
            }
            if a.conn.json {
                let v: Vec<_> = views
                    .iter()
                    .map(|v| serde_json::json!({"host": v.label, "unavailable": v.unavailable, "templates": v.templates}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", render_templates(&views));
            }
            Ok(())
        }
        TemplatesCommand::Distribute(a) => {
            let (targets, inv) = resolve(&a.which, &a.conn)?;
            let t = TemplateRef::parse(&a.template)?;
            t.require_digest()?;
            let auth = RegistryAuth::from_env();
            let tr = transport(&a.conn, inv.as_ref());
            let rs = par_map(&targets, jobs(&a.conn), |h| {
                distribute_host(tr.as_ref(), h, &t, auth.as_ref(), Duration::from_secs(a.pull_timeout))
            });
            if a.conn.dry_run {
                if let Some(d) = tr.as_dry() {
                    for l in d.lines.lock().unwrap().iter() {
                        println!("{l}");
                    }
                }
                return Ok(());
            }
            if a.conn.json {
                println!("{}", serde_json::to_string_pretty(&rs)?);
            } else {
                println!("{}", crate::mac_fleet::render_results("TEMPLATE", &rs));
            }
            finish(&rs, "templates distribute")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_sched::tests::{D1, D2};
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep(&self, s: u64) {
            self.0.fetch_add(s, Ordering::SeqCst);
        }
    }

    fn tgt(h: &str) -> Target {
        Target::parse(&format!("admin@{h}")).unwrap()
    }

    /// A probe answer: `macos` running macOS guests, holding `digest` for xcode:15 (or nothing).
    fn probe_reply(macos: usize, digest: Option<&str>) -> String {
        let items: Vec<_> = (0..macos)
            .map(|_| serde_json::json!({"status":"running","request":{"vcpus":4,"memory_mib":8192,"apple":{"guest_os":"macos"}}}))
            .collect();
        let vms = serde_json::json!({"items": items});
        let imgs = match digest {
            Some(d) => format!("[{{\"reference\":\"reg.example/xcode:15\",\"digest\":\"{d}\"}}]"),
            None => "[]".into(),
        };
        format!(
            "os=Darwin\narch=arm64\nmacos=15.1\nfluxctl_version=0.4.21\ndaemon_healthy=yes\ndaemon_loaded=yes\ndisk_free_gib=300\nvms_json={vms}\nncpu=10\nmemsize={}\npower=ac\nimages_json={imgs}\n",
            32u64 << 30
        )
    }

    fn is_probe(c: &str) -> bool {
        c.contains("ncpu=")
    }

    fn pinned() -> String {
        format!("reg.example/xcode:15@{D1}")
    }

    fn placer<'a>(
        tr: &'a dyn Transport,
        targets: &'a [Target],
        leases: &'a Mutex<Leases>,
        state: &'a std::path::Path,
        clock: &'a dyn Clock,
        log: &'a (dyn Fn(&str) + Sync),
    ) -> Placer<'a> {
        Placer {
            tr,
            targets,
            jobs: 2,
            probe_timeout: Duration::from_secs(5),
            leases,
            state_path: state,
            clock,
            poll_secs: 10,
            log,
        }
    }

    #[test]
    fn the_scheduler_picks_the_mac_that_holds_the_template_and_has_a_slot() {
        let targets = vec![tgt("a.local"), tgt("b.local"), tgt("c.local")];
        let tr = FakeTransport::new(|t, c| {
            assert!(is_probe(c));
            match t.host.as_str() {
                "a.local" => ok(&probe_reply(2, Some(D1))), // full
                "b.local" => ok(&probe_reply(0, None)),     // no template
                _ => ok(&probe_reply(1, Some(D1))),
            }
        });
        let d = tempfile::tempdir().unwrap();
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(1000));
        let log = |_: &str| {};
        let state = d.path().join("s.json");
        let p = placer(&tr, &targets, &leases, &state, &clock, &log);
        let job = PlaceFlags::parse_for_test().job(&pinned(), 4, 8192).unwrap();
        let placed = p.place("j1", &job, 0).unwrap();
        assert_eq!(placed.target.host, "c.local");
        // the lease keeps the second macOS slot of c while the first job starts
        let placed2 = p.place("j2", &job, 0);
        assert!(placed2.is_err(), "c is now at 2/2 and a is full, b lacks the template");
    }

    #[test]
    fn a_job_waits_for_a_free_slot_then_places_and_gives_up_at_the_deadline() {
        let targets = vec![tgt("a.local")];
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let tr = FakeTransport::new(move |_, _| {
            // full for the first two looks, then a job ends
            let n = calls.fetch_add(1, Ordering::SeqCst);
            ok(&probe_reply(if n < 2 { 2 } else { 1 }, Some(D1)))
        });
        let d = tempfile::tempdir().unwrap();
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(0));
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_owned());
        let state = d.path().join("s.json");
        let p = placer(&tr, &targets, &leases, &state, &clock, &log);
        let job = PlaceFlags::parse_for_test().job(&pinned(), 4, 8192).unwrap();
        let placed = p.place("j1", &job, 60).unwrap();
        assert_eq!(placed.target.host, "a.local");
        assert!(clock.now() >= 20, "it slept between looks: {}", clock.now());
        let l = logged.lock().unwrap();
        assert_eq!(l.len(), 1, "one queued note, not one per poll: {l:?}");
        assert!(l[0].contains("both macOS slots"), "{l:?}");
        drop(l);
        // always full: gives up after the queue timeout
        let tr2 = FakeTransport::new(|_, _| ok(&probe_reply(2, Some(D1))));
        let clock2 = FakeClock(AtomicU64::new(0));
        let leases2 = Mutex::new(Leases::default());
        let p2 = placer(&tr2, &targets, &leases2, &state, &clock2, &log);
        let e = p2.place("j", &job, 25).unwrap_err().to_string();
        assert!(e.contains("after waiting 25s"), "{e}");
        let e = p2.place("j", &job, 0).unwrap_err().to_string();
        assert!(e.contains("--queue-timeout"), "{e}");
    }

    #[test]
    fn a_drained_mac_gets_no_new_job_until_resumed() {
        let targets = vec![tgt("a.local")];
        let tr = FakeTransport::new(|_, _| ok(&probe_reply(0, Some(D1))));
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(0));
        let log = |_: &str| {};
        let p = placer(&tr, &targets, &leases, &state, &clock, &log);
        let job = PlaceFlags::parse_for_test().job(&pinned(), 0, 0).unwrap();
        let lines = set_drain(&state, &["a".to_owned()], true, 5).unwrap();
        assert!(lines[0].contains("drained"), "{lines:?}");
        let e = p.place("j", &job, 30).unwrap_err().to_string();
        assert!(e.contains("drained"), "never waits on a drained Mac: {e}");
        assert_eq!(clock.now(), 0);
        set_drain(&state, &["a".to_owned()], false, 6).unwrap();
        assert!(p.place("j", &job, 0).is_ok());
    }

    #[test]
    fn the_job_script_has_no_secret_and_quotes_every_word() {
        let j = JobRun {
            id: "ab12".into(),
            image: "reg.example/xcode:15".into(),
            cmd: vec!["./run.sh".into(), "it's".into()],
            cpus: Some(4),
            memory: Some("8g".into()),
            user: None,
            wait: 900,
            secret_env: Some("ACTIONS_RUNNER_INPUT_JITCONFIG".into()),
        };
        let s = run_job_script(&j).unwrap();
        assert!(s.contains("IFS= read -r ACTIONS_RUNNER_INPUT_JITCONFIG"));
        assert!(s.contains("exec \"$FLUXCTL\" mac run --rm --name 'fleet-ab12' --label fluxvm.owner=mac-fleet --label 'fluxvm.job=ab12' --wait 900 --cpus 4 --memory '8g' -e ACTIONS_RUNNER_INPUT_JITCONFIG 'reg.example/xcode:15' -- './run.sh' 'it'\\''s'"), "{s}");
        let mut bad = j.clone();
        bad.secret_env = Some("x; reboot".into());
        assert!(run_job_script(&bad).is_err());
        bad = j;
        bad.id = "a b".into();
        assert!(run_job_script(&bad).is_err());
    }

    #[test]
    fn a_timed_out_job_is_reported_and_its_guest_deleted() {
        let tr = FakeTransport::new(|_, c| {
            if c.contains("mac rm") {
                ok("")
            } else {
                crate::mac_fleet_ssh::ExecOut {
                    code: 124,
                    stdout: "partial\n".into(),
                    stderr: "\ntimed out after 5s".into(),
                }
            }
        });
        let j = JobRun {
            id: "t1".into(),
            image: "x:1".into(),
            cmd: vec!["sleep".into(), "9".into()],
            cpus: None,
            memory: None,
            user: None,
            wait: 60,
            secret_env: None,
        };
        let o = run_job(&tr, &tgt("a.local"), &j, None, Duration::from_secs(5), false);
        assert!(o.timed_out && !o.ok());
        assert_eq!(o.stdout, "partial\n");
        let calls = tr.calls_for("a.local");
        assert_eq!(calls.len(), 2);
        assert!(calls[1].command.contains("mac rm"), "{}", calls[1].command);
        assert!(calls[1].command.contains("fleet-t1"));
    }

    #[test]
    fn a_clean_job_is_not_followed_by_a_delete_unless_asked() {
        let tr = FakeTransport::new(|_, _| ok("done\n"));
        let j = JobRun {
            id: "c1".into(),
            image: "x:1".into(),
            cmd: vec!["true".into()],
            cpus: None,
            memory: None,
            user: None,
            wait: 60,
            secret_env: None,
        };
        assert!(run_job(&tr, &tgt("a.local"), &j, None, Duration::from_secs(5), false).ok());
        assert_eq!(tr.calls_for("a.local").len(), 1);
        assert!(run_job(&tr, &tgt("a.local"), &j, None, Duration::from_secs(5), true).ok());
        assert_eq!(tr.calls_for("a.local").len(), 3);
    }

    #[test]
    fn the_secret_goes_on_stdin_and_is_scrubbed_from_output() {
        let tr = FakeTransport::new(|_, c| {
            assert!(!c.contains("hunter2-token"));
            ok("echo hunter2-token\n")
        });
        let j = JobRun {
            id: "s1".into(),
            image: "x:1".into(),
            cmd: vec!["./run.sh".into()],
            cpus: None,
            memory: None,
            user: None,
            wait: 60,
            secret_env: Some("JIT".into()),
        };
        let s = Secret::new(b"hunter2-token\n".to_vec());
        let o = run_job(&tr, &tgt("a.local"), &j, Some(&s), Duration::from_secs(5), false);
        let c = &tr.calls_for("a.local")[0];
        assert_eq!(c.stdin.as_deref(), Some(&b"hunter2-token\n"[..]));
        assert!(!o.stdout.contains("hunter2-token"), "{}", o.stdout);
    }

    fn distribute_fake(after: &'static str, pulled: &'static std::sync::atomic::AtomicBool) -> FakeTransport {
        FakeTransport::new(move |_, c| {
            if c.contains("mac pull") {
                pulled.store(true, Ordering::SeqCst);
                ok("pulled\n")
            } else if pulled.load(Ordering::SeqCst) {
                ok(after)
            } else {
                ok("[]")
            }
        })
    }

    #[test]
    fn distribute_pulls_then_verifies_the_digest_and_sends_credentials_on_stdin() {
        static PULLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let good: &'static str = Box::leak(format!("[{{\"reference\":\"reg.example/xcode:15\",\"digest\":\"{D1}\"}}]").into_boxed_str());
        let tr = distribute_fake(good, &PULLED);
        let auth = RegistryAuth {
            user: Secret::new(b"robot".to_vec()),
            password: Secret::new(b"p4ssw0rd-x".to_vec()),
        };
        let t = TemplateRef::parse(&pinned()).unwrap();
        let r = distribute_host(&tr, &tgt("a.local"), &t, Some(&auth), Duration::from_secs(10));
        assert!(r.ok, "{r:?}");
        let calls = tr.calls_for("a.local");
        let pull = calls.iter().find(|c| c.command.contains("mac pull")).unwrap();
        assert_eq!(pull.stdin.as_deref(), Some(&b"robot\np4ssw0rd-x\n"[..]));
        assert!(!pull.command.contains("p4ssw0rd-x") && !pull.command.contains("robot"));
    }

    #[test]
    fn distribute_reports_a_digest_mismatch_and_skips_a_mac_that_already_has_it() {
        static PULLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let bad: &'static str = Box::leak(format!("[{{\"reference\":\"reg.example/xcode:15\",\"digest\":\"{D2}\"}}]").into_boxed_str());
        let tr = distribute_fake(bad, &PULLED);
        let t = TemplateRef::parse(&pinned()).unwrap();
        let r = distribute_host(&tr, &tgt("a.local"), &t, None, Duration::from_secs(10));
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("digest mismatch"), "names both digests");
        // already present: no pull
        let have = format!("[{{\"reference\":\"reg.example/xcode:15\",\"digest\":\"{D1}\"}}]");
        let tr2 = FakeTransport::new(move |_, _| ok(&have));
        let r = distribute_host(&tr2, &tgt("a.local"), &t, None, Duration::from_secs(10));
        assert!(r.ok);
        assert_eq!(tr2.calls_for("a.local").len(), 1);
        // an unpinned template is refused up front
        let u = TemplateRef::parse("reg.example/xcode:15").unwrap();
        assert!(!distribute_host(&tr2, &tgt("a.local"), &u, None, Duration::from_secs(1)).ok);
    }

    #[test]
    fn prune_removes_only_idle_macs_that_left_and_remembers_failures() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        let keep = tgt("keep.local");
        let gone = tgt("gone.local");
        let busy = tgt("busy.local");
        let mut st = FleetState::default();
        st.applied.insert(
            "/inv.yaml".into(),
            [&keep, &gone, &busy].iter().map(|t| AppliedHost::from_target(t)).collect(),
        );
        st.save(&state).unwrap();
        let tr = FakeTransport::new(|t, c| {
            if c.contains("launchctl bootout") || c.contains("rm -rf") {
                ok("removed\n")
            } else if t.host == "busy.local" {
                ok("os=Darwin\nvms_json={\"items\":[{\"status\":\"running\"}]}\n")
            } else {
                ok("os=Darwin\nvms_json={\"items\":[]}\n")
            }
        });
        let conn = test_conn();
        // without --prune: nothing is sent, the Mac is only mentioned, and stays remembered
        after_apply(&conn, &tr, "/inv.yaml", std::slice::from_ref(&keep), false, &state).unwrap();
        assert!(tr.calls.lock().unwrap().is_empty());
        let st = FleetState::load(&state).unwrap();
        assert_eq!(st.applied["/inv.yaml"].len(), 3);
        // with --prune: gone is removed, busy is refused and stays on the list
        let e = after_apply(&conn, &tr, "/inv.yaml", std::slice::from_ref(&keep), true, &state).unwrap_err();
        assert!(e.to_string().contains("1 Mac"), "{e}");
        assert!(tr.calls_for("keep.local").is_empty(), "inventory hosts are never pruned");
        assert!(tr.calls_for("gone.local").iter().any(|c| c.command.contains("bootout") || c.command.contains("rm -rf")));
        assert!(!tr.calls_for("busy.local").iter().any(|c| c.command.contains("rm -rf")));
        let st = FleetState::load(&state).unwrap();
        let hosts: Vec<_> = st.applied["/inv.yaml"].iter().map(|h| h.host.as_str()).collect();
        assert_eq!(hosts, ["keep.local", "busy.local"]);
    }

    pub fn test_conn() -> Conn {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct W {
            #[command(flatten)]
            c: Conn,
        }
        W::parse_from(["x"]).c
    }

    impl PlaceFlags {
        pub fn parse_for_test() -> Self {
            use clap::Parser;
            #[derive(clap::Parser)]
            struct W {
                #[command(flatten)]
                p: PlaceFlags,
            }
            W::parse_from(["x"]).p
        }
    }
}
