// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac runner`: GitHub Actions self-hosted runners on macOS, one ephemeral VM per job.
//!
//! For every job a just-in-time runner is registered through the GitHub API (`generate-jitconfig`), a Mac is chosen by the
//! fleet scheduler (or this Mac, without `--fleet`), a fresh guest is created from the template with `fluxctl mac run --rm`,
//! the runner inside it takes exactly one job and exits, and the guest is deleted. The API token and the JIT configuration
//! are secrets: the token comes from a 0600 file or stdin, the JIT configuration travels on ssh stdin and is exported only
//! on the Mac that runs the guest; neither is ever an argument, written to a log or printed.
//!
//! Not run against GitHub or a real guest: see docs/mac-runner.md.

use crate::mac_fleet::{Conn, Which, jobs, resolve, transport};
use crate::mac_fleet_remote::Secret;
use crate::mac_fleet_run::{Clock, JobRun, PlaceFlags, Placer, SystemClock, new_job_id, run_job};
use crate::mac_fleet_sched::{JobSpec, Leases, state_path};
use crate::mac_fleet_ssh::{Target, Transport};
use crate::mac_fleet_timed::LocalTransport;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Runner name prefix: also how this process finds its own runners among all of the repository's.
pub const NAME_PREFIX: &str = "fluxvm-";
/// The variable the Actions runner reads its JIT configuration from (`./run.sh --jitconfig` takes the same value).
pub const JIT_ENV: &str = "ACTIONS_RUNNER_INPUT_JITCONFIG";
/// Apple's licence limit per Mac.
const SLOTS_PER_MAC: usize = 2;

#[derive(clap::Args)]
pub struct RunnerArgs {
    /// `OWNER/REPO` to serve.
    #[arg(
        long,
        value_name = "OWNER/REPO",
        conflicts_with = "org",
        required_unless_present = "org"
    )]
    pub repo: Option<String>,
    /// An organisation to serve (runners join its runner group). Queue depth cannot be read at this level: use `--warm`.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
    /// File with a GitHub token that may create runners (a fine-grained token with "Administration: write" on the repository,
    /// or "Self-hosted runners: write" on the organisation, plus "Actions: read" to see the queue), mode 0600.
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
    /// Read that token from this process's stdin.
    #[arg(long)]
    pub token_stdin: bool,
    /// GitHub API base URL (GitHub Enterprise Server: `https://HOST/api/v3`).
    #[arg(long, default_value = "https://api.github.com")]
    pub api_url: String,
    /// The template every job's guest is cloned from, `name:tag[@sha256:HEX]`; it must have the Actions runner unpacked in --runner-dir.
    #[arg(long, value_name = "REF")]
    pub template: String,
    /// Directory in the guest that holds `run.sh` of the Actions runner.
    #[arg(long, default_value = "/Users/admin/actions-runner")]
    pub runner_dir: String,
    /// Label to add to the runner (`runs-on:`). Repeatable. `self-hosted`, `macOS` and `ARM64` are always present.
    #[arg(long = "label", value_name = "LABEL")]
    pub labels: Vec<String>,
    /// Runner group id (organisation runners; a repository's runners use 1).
    #[arg(long, default_value_t = 1)]
    pub runner_group: u64,
    /// Runner group by name instead of id (organisation runners); looked up through the API at start.
    #[arg(long, value_name = "NAME", conflicts_with = "runner_group")]
    pub runner_group_name: Option<String>,
    /// Never more than N runners (guests) at once, however many jobs are queued: the pool grows with the queue up to N and
    /// shrinks as runners finish their one job. Lowers the 2-per-Mac ceiling, never raises it.
    #[arg(long, value_name = "N")]
    pub ephemeral_pool: Option<usize>,
    /// With --org: read the queue from this repository (`OWNER/REPO`). Repeatable. GitHub has no organisation-wide queue call.
    #[arg(long = "queue-repo", value_name = "OWNER/REPO", requires = "org")]
    pub queue_repo: Vec<String>,
    /// With --org: also read the queue from the organisation's most recently pushed repositories.
    #[arg(long, requires = "org")]
    pub queue_scan: bool,
    /// With --queue-scan: how many repositories at most.
    #[arg(long, value_name = "N", default_value_t = 30)]
    pub scan_limit: usize,
    /// Spread jobs over the Macs of this inventory (`fleet.yaml`). Without it jobs run on this Mac. The Macs are reached over ssh.
    #[arg(long, value_name = "FILE", num_args = 0..=1, default_missing_value = "fleet")]
    pub fleet: Option<PathBuf>,
    /// Most guests at once, across all Macs (default and ceiling: 2 per Mac, Apple's limit).
    #[arg(long, value_name = "N")]
    pub max_jobs: Option<usize>,
    /// Keep this many runners waiting even when no job is queued. Default 0 for a repository, 1 for an organisation.
    #[arg(long, value_name = "N")]
    pub warm: Option<usize>,
    /// Serve a single job and exit.
    #[arg(long)]
    pub once: bool,
    /// Seconds between looks at the queue.
    #[arg(long, default_value_t = 15)]
    pub interval: u64,
    /// Print what would happen and call nothing (no GitHub request, no Mac).
    #[arg(long)]
    pub plan: bool,
    /// Do not look, at start, for `fleet-*` guests and `fluxvm-*` runners left behind by a run that was killed.
    #[arg(long)]
    pub no_sweep: bool,
    #[command(flatten)]
    pub place: PlaceFlags,
    /// Seconds the guest may take to answer on SSH.
    #[arg(long, default_value_t = 900)]
    pub wait: u64,
    #[command(flatten)]
    pub conn: Conn,
}

// ---- GitHub ----------------------------------------------------------------------------------------------------------

pub struct Jit {
    pub runner_id: u64,
    pub config: Secret,
}

pub trait Github: Send + Sync {
    /// Jobs waiting for a runner with these labels; `None` where the API cannot tell (organisation scope).
    fn queued(&self, labels: &[String]) -> Result<Option<usize>>;
    /// This process's runners that are running a job right now.
    fn busy(&self) -> Result<usize>;
    fn jit(&self, name: &str, labels: &[String]) -> Result<Jit>;
    /// A JIT runner that never took a job stays registered until deleted.
    fn remove(&self, runner_id: u64) -> Result<()>;
    /// This process's kind of runners (`fluxvm-*`) as GitHub lists them; used by the startup sweep. Default: none known.
    fn runners(&self) -> Result<Vec<RunnerInfo>> {
        Ok(vec![])
    }
}

/// One `fluxvm-*` runner as GitHub lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerInfo {
    pub id: u64,
    pub name: String,
    pub busy: bool,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    Repo(String, String),
    Org(String),
}

fn name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

impl Scope {
    pub fn parse(repo: Option<&str>, org: Option<&str>) -> Result<Self> {
        match (repo, org) {
            (Some(r), None) => {
                let (o, n) = r.split_once('/').context("--repo is OWNER/REPO")?;
                if !name_ok(o) || !name_ok(n) {
                    bail!("--repo {r:?} is not OWNER/REPO");
                }
                Ok(Scope::Repo(o.into(), n.into()))
            }
            (None, Some(o)) if name_ok(o) => Ok(Scope::Org(o.into())),
            _ => bail!("name --repo OWNER/REPO or --org ORG"),
        }
    }

    pub fn path(&self) -> String {
        match self {
            Scope::Repo(o, r) => format!("repos/{o}/{r}"),
            Scope::Org(o) => format!("orgs/{o}"),
        }
    }
}

pub fn jit_body(name: &str, group: u64, labels: &[String]) -> serde_json::Value {
    serde_json::json!({"name": name, "runner_group_id": group, "labels": labels, "work_folder": "_work"})
}

/// Labels of a runner: the three GitHub expects on a macOS arm64 self-hosted runner, then the operator's, no duplicates.
pub fn runner_labels(extra: &[String]) -> Vec<String> {
    let mut v: Vec<String> = ["self-hosted", "macOS", "ARM64"].map(String::from).to_vec();
    for l in extra {
        if !v.iter().any(|x| x.eq_ignore_ascii_case(l)) {
            v.push(l.clone());
        }
    }
    v
}

/// A queued job can run here when every label it asks for is one of the runner's.
pub fn job_matches(job_labels: &[String], ours: &[String]) -> bool {
    !job_labels.is_empty()
        && job_labels
            .iter()
            .all(|l| ours.iter().any(|o| o.eq_ignore_ascii_case(l)))
}

pub struct RealGithub {
    http: reqwest::Client,
    handle: tokio::runtime::Handle,
    api: String,
    scope: Scope,
    group: u64,
    token: Secret,
}

impl RealGithub {
    pub fn new(api: &str, scope: Scope, group: u64, token: Secret) -> Result<Self> {
        if !(api.starts_with("https://")
            && api.bytes().all(|c| c.is_ascii_graphic())
            && !api.contains('\''))
        {
            bail!("--api-url must be an https:// URL");
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("fluxctl-mac-runner")
                .build()?,
            handle: tokio::runtime::Handle::try_current().context("no async runtime")?,
            api: api.trim_end_matches('/').into(),
            scope,
            group,
            token,
        })
    }

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        // The token is read out only here, into the Authorization header.
        let t = String::from_utf8_lossy(self.token.expose())
            .trim()
            .to_owned();
        self.http
            .request(m, format!("{}/{path}", self.api))
            .bearer_auth(t)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// GET `path` (relative to the API base).
    pub fn get(&self, path: &str, what: &str) -> Result<serde_json::Value> {
        self.json(self.req(reqwest::Method::GET, path), what)
    }

    fn json(&self, b: reqwest::RequestBuilder, what: &str) -> Result<serde_json::Value> {
        self.handle.block_on(async {
            let r = b
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("{what}: {}", e.without_url()))?;
            let status = r.status();
            if !status.is_success() {
                // The body of an error says what is wrong (bad credentials, missing permission) and carries no secret.
                let body = r.text().await.unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["message"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                bail!("{what}: HTTP {status} {msg}");
            }
            if status == reqwest::StatusCode::NO_CONTENT {
                return Ok(serde_json::Value::Null);
            }
            r.json()
                .await
                .map_err(|e| anyhow::anyhow!("{what}: {}", e.without_url()))
        })
    }
}

impl Github for RealGithub {
    fn queued(&self, labels: &[String]) -> Result<Option<usize>> {
        let Scope::Repo(..) = &self.scope else {
            return Ok(None);
        };
        let base = self.scope.path();
        let mut n = 0;
        // Jobs wait in runs that are queued, and in runs already in progress (a later job of the same workflow).
        for status in ["queued", "in_progress"] {
            let runs = self.json(
                self.req(
                    reqwest::Method::GET,
                    &format!("{base}/actions/runs?status={status}&per_page=50"),
                ),
                "listing workflow runs",
            )?;
            for run in runs["workflow_runs"].as_array().into_iter().flatten() {
                let Some(id) = run["id"].as_u64() else {
                    continue;
                };
                let jobs = self.json(
                    self.req(
                        reqwest::Method::GET,
                        &format!("{base}/actions/runs/{id}/jobs?per_page=100"),
                    ),
                    "listing jobs",
                )?;
                for j in jobs["jobs"].as_array().into_iter().flatten() {
                    let ls: Vec<String> = j["labels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str().map(str::to_owned))
                        .collect();
                    if j["status"].as_str() == Some("queued") && job_matches(&ls, labels) {
                        n += 1;
                    }
                }
            }
        }
        Ok(Some(n))
    }

    fn busy(&self) -> Result<usize> {
        let v = self.json(
            self.req(
                reqwest::Method::GET,
                &format!("{}/actions/runners?per_page=100", self.scope.path()),
            ),
            "listing runners",
        )?;
        Ok(v["runners"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                r["name"]
                    .as_str()
                    .is_some_and(|n| n.starts_with(NAME_PREFIX))
                    && r["busy"].as_bool() == Some(true)
            })
            .count())
    }

    fn runners(&self) -> Result<Vec<RunnerInfo>> {
        let v = self.get(
            &format!("{}/actions/runners?per_page=100", self.scope.path()),
            "listing runners",
        )?;
        Ok(v["runners"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let name = r["name"].as_str()?.to_owned();
                name.starts_with(NAME_PREFIX).then(|| RunnerInfo {
                    id: r["id"].as_u64().unwrap_or(0),
                    name,
                    busy: r["busy"].as_bool() == Some(true),
                    status: r["status"].as_str().unwrap_or("").to_owned(),
                })
            })
            .filter(|r| r.id != 0)
            .collect())
    }

    fn jit(&self, name: &str, labels: &[String]) -> Result<Jit> {
        let v = self.json(
            self.req(
                reqwest::Method::POST,
                &format!("{}/actions/runners/generate-jitconfig", self.scope.path()),
            )
            .json(&jit_body(name, self.group, labels)),
            "creating a just-in-time runner",
        )?;
        Ok(Jit {
            runner_id: v["runner"]["id"]
                .as_u64()
                .context("GitHub returned no runner id")?,
            config: Secret::new(
                v["encoded_jit_config"]
                    .as_str()
                    .context("GitHub returned no jit config")?
                    .as_bytes()
                    .to_vec(),
            ),
        })
    }

    fn remove(&self, runner_id: u64) -> Result<()> {
        self.json(
            self.req(
                reqwest::Method::DELETE,
                &format!("{}/actions/runners/{runner_id}", self.scope.path()),
            ),
            "removing the runner",
        )
        .map(|_| ())
    }
}

// ---- the supervisor --------------------------------------------------------------------------------------------------

/// How many new runners to start: enough for every queued job and the warm pool, never more than `cap` guests in total,
/// counting the ones already running a job (`busy`) and those still waiting for one.
pub fn plan_spawn(in_flight: usize, busy: usize, queued: usize, warm: usize, cap: usize) -> usize {
    let busy = busy.min(in_flight);
    let want_total = busy + (queued + warm).min(cap.saturating_sub(busy));
    want_total
        .saturating_sub(in_flight)
        .min(cap.saturating_sub(in_flight))
}

/// The ceiling on guests: Apple allows two macOS guests per Mac; `--max-jobs` can only lower it.
pub fn job_cap(macs: usize, max_jobs: Option<usize>) -> usize {
    let hard = macs * SLOTS_PER_MAC;
    max_jobs.map_or(hard, |m| m.min(hard)).max(1)
}

pub struct JobReport {
    pub runner: String,
    pub host: Option<String>,
    pub ok: bool,
    pub detail: String,
}

pub struct Ctx<'a> {
    pub gh: &'a dyn Github,
    pub tr: &'a dyn Transport,
    pub targets: &'a [Target],
    pub place: &'a PlaceFlags,
    pub job: &'a JobSpec,
    pub leases: &'a Mutex<Leases>,
    pub state: &'a Path,
    pub clock: &'a dyn Clock,
    pub labels: &'a [String],
    pub runner_dir: &'a str,
    pub wait: u64,
    pub jobs: usize,
    pub log: &'a (dyn Fn(&str) + Sync),
}

/// One runner, one guest, one job. Always ends with the guest deleted and the runner unregistered.
pub fn serve_one(c: &Ctx) -> JobReport {
    let id = new_job_id();
    let name = format!("{NAME_PREFIX}{id}");
    let mut rep = JobReport {
        runner: name.clone(),
        host: None,
        ok: false,
        detail: String::new(),
    };
    let jit = match c.gh.jit(&name, c.labels) {
        Ok(j) => j,
        Err(e) => {
            rep.detail = format!("{e:#}");
            return rep;
        }
    };
    crate::mac_runner_life::track_runner(&id, jit.runner_id);
    let cleanup = |detail: String, mut rep: JobReport| {
        // Unused JIT runners are removed; a used one is already gone (the call then fails, which is fine).
        let _ = c.gh.remove(jit.runner_id);
        crate::mac_runner_life::untrack(&id);
        rep.detail = detail;
        rep
    };
    let placer = Placer {
        tr: c.tr,
        targets: c.targets,
        jobs: c.jobs,
        probe_timeout: Duration::from_secs(c.place.probe_timeout),
        leases: c.leases,
        state_path: c.state,
        clock: c.clock,
        poll_secs: c.place.poll,
        log: c.log,
    };
    let placed = match placer.place(&id, c.job, c.place.queue_timeout) {
        Ok(p) => p,
        Err(e) => return cleanup(format!("{e:#}"), rep),
    };
    crate::mac_runner_life::track_target(&id, &placed.target);
    if crate::mac_runner_life::requested() {
        // Stopped while queued: do not start a guest for a runner that shutdown already unregistered.
        placer.release(&placed.lease);
        return cleanup("stopped before the job started".into(), rep);
    }
    rep.host = Some(placed.view.label.clone());
    (c.log)(&format!("{name}: started on {}", placed.view.label));
    let run = JobRun {
        id: id.clone(),
        image: c.job.template.name.clone(),
        cmd: vec![format!("{}/run.sh", c.runner_dir.trim_end_matches('/'))],
        cpus: (c.job.cpus > 0).then(|| u8::try_from(c.job.cpus).unwrap_or(u8::MAX)),
        memory: (c.job.memory_mib > 0).then(|| format!("{}m", c.job.memory_mib)),
        user: None,
        wait: c.wait,
        secret_env: Some(JIT_ENV.into()),
    };
    // The JIT configuration plus a newline: the Mac reads one line into the environment.
    let mut stdin = jit.config.expose().to_vec();
    stdin.push(b'\n');
    let stdin = Secret::new(stdin);
    let o = run_job(
        c.tr,
        &placed.target,
        &run,
        Some(&stdin),
        Duration::from_secs(c.place.timeout),
        true,
    );
    placer.release(&placed.lease);
    let detail = if let Some(e) = &o.transport_error {
        format!("lost contact with {}: {e}", o.host)
    } else if o.timed_out {
        format!("ran past --timeout {}s and was stopped", c.place.timeout)
    } else if o.code != 0 {
        format!("the runner exited with status {}", o.code)
    } else {
        "job finished".to_owned()
    };
    rep.ok = o.ok();
    cleanup(detail, rep)
}

/// The loop. Starts runners as the queue needs them until `once` is served (or forever).
pub fn supervise(
    c: &Ctx,
    cap: usize,
    warm: usize,
    once: bool,
    interval: u64,
    max_ticks: Option<usize>,
) -> Vec<JobReport> {
    let in_flight = AtomicUsize::new(0);
    let reports = Mutex::new(vec![]);
    let mut started = 0usize;
    std::thread::scope(|s| {
        let mut ticks = 0;
        loop {
            if crate::mac_runner_life::requested() {
                // The watcher deletes the guests; the scope joins their threads.
                (c.log)("stop requested: no new runners; cleaning up what is in flight");
                break;
            }
            let n = in_flight.load(Ordering::SeqCst);
            let (queued, busy) = match (c.gh.queued(c.labels), c.gh.busy()) {
                (Ok(q), Ok(b)) => (q, b),
                (q, b) => {
                    let e = q
                        .err()
                        .or(b.err())
                        .map(|e| format!("{e:#}"))
                        .unwrap_or_default();
                    (c.log)(&format!("could not read the queue: {e}"));
                    (None, n)
                }
            };
            let queued = queued.unwrap_or(0);
            let mut spawn = plan_spawn(n, busy, queued, warm, cap);
            if once {
                spawn = spawn.min(1usize.saturating_sub(started));
            }
            if spawn > 0 {
                (c.log)(&crate::mac_runner_pool::describe(
                    queued, busy, n, warm, cap, spawn,
                ));
            }
            for _ in 0..spawn {
                started += 1;
                in_flight.fetch_add(1, Ordering::SeqCst);
                let (inf, reps) = (&in_flight, &reports);
                s.spawn(move || {
                    let r = serve_one(c);
                    (c.log)(&format!(
                        "{}: {} ({})",
                        r.runner,
                        if r.ok { "done" } else { "failed" },
                        r.detail
                    ));
                    reps.lock().unwrap().push(r);
                    inf.fetch_sub(1, Ordering::SeqCst);
                });
            }
            ticks += 1;
            if once && started >= 1 {
                // Served: no more queue reads, just wait for the one guest to finish (the scope joins it).
                while in_flight.load(Ordering::SeqCst) > 0 {
                    std::thread::sleep(Duration::from_millis(50));
                }
                break;
            }
            if max_ticks.is_some_and(|m| ticks >= m) && in_flight.load(Ordering::SeqCst) == 0 {
                break;
            }
            // One second at a time, so a stop request is noticed within a second.
            for _ in 0..interval.max(1) {
                if crate::mac_runner_life::requested() {
                    break;
                }
                c.clock.sleep(1);
            }
        }
    });
    reports.into_inner().unwrap()
}

pub fn read_secret(what: &str, file: Option<&Path>, stdin: bool) -> Result<Secret> {
    let raw = match (file, stdin) {
        (Some(_), true) => bail!("use --token-file or --token-stdin, not both"),
        (Some(p), false) => {
            let p = crate::mac_fleet_inv::expand_home(p);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p)
                    .with_context(|| format!("{what} file {}", p.display()))?
                    .permissions()
                    .mode();
                if mode & 0o077 != 0 {
                    bail!(
                        "{what} file {} is readable by others (mode {:o}): `chmod 600 {}`",
                        p.display(),
                        mode & 0o777,
                        p.display()
                    );
                }
            }
            std::fs::read_to_string(&p).with_context(|| format!("{what} file {}", p.display()))?
        }
        (None, true) => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
        (None, false) => bail!(
            "give the {what} with --token-file FILE (mode 0600) or --token-stdin; never as an argument"
        ),
    };
    let t = raw.trim();
    if t.len() < 8 || t.len() > 512 || !t.bytes().all(|c| c.is_ascii_graphic()) {
        bail!("the {what} is empty or not a single printable word");
    }
    Ok(Secret::new(t.as_bytes().to_vec()))
}

pub fn run(a: RunnerArgs) -> Result<()> {
    let scope = Scope::parse(a.repo.as_deref(), a.org.as_deref())?;
    let labels = runner_labels(&a.labels);
    for l in &labels {
        crate::mac_fleet_sched::check_label(l)?;
    }
    let job = a.place.job(&a.template, 0, 0)?;
    let (targets, inv, tr): (Vec<Target>, _, Box<dyn Transport>) = match &a.fleet {
        Some(f) => {
            let which = Which {
                hosts: vec![],
                inventory: Some(f.clone()),
                all: true,
            };
            let (targets, inv) = resolve(&which, &a.conn)?;
            let tr = transport(&a.conn, inv.as_ref());
            (targets, inv, tr)
        }
        None => (
            vec![Target::parse("localhost")?],
            None,
            Box::new(LocalTransport { timeout: None }),
        ),
    };
    let _ = inv;
    let cap =
        crate::mac_runner_pool::pool_cap(job_cap(targets.len(), a.max_jobs), a.ephemeral_pool);
    let warm = a
        .warm
        .unwrap_or(if matches!(scope, Scope::Org(_)) { 1 } else { 0 });
    eprintln!(
        "runner for {} serving {} Mac(s), up to {cap} guest(s) at once (2 per Mac), template {}, labels {}",
        scope.path(),
        targets.len(),
        job.template,
        labels.join(",")
    );
    if a.plan || a.conn.dry_run {
        eprintln!("plan only: no GitHub request and no Mac was contacted");
        return Ok(());
    }
    let token = read_secret("GitHub token", a.token_file.as_deref(), a.token_stdin)?;
    let group = match (&a.runner_group_name, &scope) {
        (Some(n), Scope::Org(o)) => {
            crate::mac_runner_pool::resolve_group(&a.api_url, o, &token, n)?
        }
        (Some(_), _) => bail!("--runner-group-name needs --org"),
        _ => a.runner_group,
    };
    let explicit = a
        .queue_repo
        .iter()
        .map(|r| match Scope::parse(Some(r), None)? {
            Scope::Repo(o, n) => Ok((o, n)),
            _ => bail!("--queue-repo {r:?} is not OWNER/REPO"),
        })
        .collect::<Result<Vec<_>>>()?;
    let real = RealGithub::new(&a.api_url, scope.clone(), group, token.clone())?;
    let pool;
    let gh: &dyn Github =
        if matches!(scope, Scope::Org(_)) && (!explicit.is_empty() || a.queue_scan) {
            pool = crate::mac_runner_pool::OrgPool::new(
                real,
                &a.api_url,
                token,
                explicit,
                a.queue_scan.then_some(a.scan_limit),
            );
            &pool
        } else {
            &real
        };
    let leases = Mutex::new(Leases::default());
    let state = state_path()?;
    let log = |m: &str| eprintln!("{m}");
    let ctx = Ctx {
        gh,
        tr: tr.as_ref(),
        targets: &targets,
        place: &a.place,
        job: &job,
        leases: &leases,
        state: &state,
        clock: &SystemClock,
        labels: &labels,
        runner_dir: &a.runner_dir,
        wait: a.wait,
        jobs: jobs(&a.conn),
        log: &log,
    };
    // SIGINT/SIGTERM/SIGHUP: stop starting runners, delete the guests and unregister the runners in flight, then leave.
    crate::mac_runner_life::install_signals();
    crate::mac_fleet_lease::install(&state);
    // Ids another process holds a live lease for are theirs, not orphans.
    let claimed = |id: &str| {
        crate::mac_runner_life::is_live(id)
            || crate::mac_fleet_lease::claimed(&state, SystemClock.now())
                .iter()
                .any(|c| c == id)
    };
    if !a.no_sweep {
        for l in crate::mac_runner_life::sweep(gh, tr.as_ref(), &targets, &claimed, false) {
            log(&l);
        }
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    let reports = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                if crate::mac_runner_life::requested() {
                    log("stop requested: deleting guests and unregistering runners");
                    for l in crate::mac_runner_life::shutdown_cleanup(
                        gh,
                        tr.as_ref(),
                        &crate::mac_runner_life::live_snapshot(),
                    ) {
                        log(&l);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let r = supervise(&ctx, cap, warm, a.once, a.interval, None);
        done.store(true, Ordering::SeqCst);
        r
    });
    if crate::mac_runner_life::requested() {
        // Anything a thread created after the watcher looked.
        if !a.no_sweep {
            for l in crate::mac_runner_life::sweep(gh, tr.as_ref(), &targets, &claimed, false) {
                log(&l);
            }
        }
        log("stopped");
        return Ok(());
    }
    if a.once && reports.iter().any(|r| !r.ok) {
        bail!(
            "{}",
            reports
                .iter()
                .map(|r| format!("{}: {}", r.runner, r.detail))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_sched::tests::D1;
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn spawn_plan_never_exceeds_two_guests_per_mac() {
        // nothing queued, nothing running, no warm pool: nothing to start
        assert_eq!(plan_spawn(0, 0, 0, 0, 2), 0);
        // 5 jobs queued on one Mac: two runners, not five
        assert_eq!(plan_spawn(0, 0, 5, 0, 2), 2);
        // one runner already waiting for a job and a second job queued: one more
        assert_eq!(plan_spawn(1, 0, 2, 0, 2), 1);
        // a runner busy with a job and another job queued: start one, even though in_flight is 1
        assert_eq!(plan_spawn(1, 1, 1, 0, 2), 1);
        // both slots busy and jobs waiting: wait for a guest to end
        assert_eq!(plan_spawn(2, 2, 3, 0, 2), 0);
        // warm pool of one with an idle queue
        assert_eq!(plan_spawn(0, 0, 0, 1, 2), 1);
        assert_eq!(plan_spawn(1, 0, 0, 1, 2), 0);
        assert_eq!(job_cap(3, None), 6);
        assert_eq!(job_cap(3, Some(10)), 6);
        assert_eq!(job_cap(3, Some(1)), 1);
    }

    #[test]
    fn scope_and_labels_are_validated() {
        assert_eq!(
            Scope::parse(Some("acme/app"), None).unwrap().path(),
            "repos/acme/app"
        );
        assert_eq!(
            Scope::parse(None, Some("acme")).unwrap().path(),
            "orgs/acme"
        );
        assert!(Scope::parse(Some("acme/app/../x"), None).is_err());
        assert!(Scope::parse(Some("acme"), None).is_err());
        assert!(Scope::parse(Some("a b/c"), None).is_err());
        assert_eq!(
            runner_labels(&["fluxvm".into(), "macos".into()]),
            ["self-hosted", "macOS", "ARM64", "fluxvm"]
        );
        assert!(job_matches(
            &["self-hosted".into(), "macOS".into()],
            &runner_labels(&[])
        ));
        assert!(!job_matches(
            &["self-hosted".into(), "gpu".into()],
            &runner_labels(&[])
        ));
        assert!(!job_matches(&[], &runner_labels(&[])));
        let b = jit_body("fluxvm-1", 1, &runner_labels(&[]));
        assert_eq!(b["runner_group_id"], 1);
        assert_eq!(b["name"], "fluxvm-1");
    }

    #[test]
    fn the_token_comes_from_a_private_file_and_not_from_an_argument() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t");
        std::fs::write(&p, "github_pat_abcdef123456\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            let e = read_secret("GitHub token", Some(&p), false)
                .unwrap_err()
                .to_string();
            assert!(e.contains("readable by others"), "{e}");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let s = read_secret("GitHub token", Some(&p), false).unwrap();
        assert_eq!(s.expose(), b"github_pat_abcdef123456");
        assert!(
            format!("{s:?}").find("abcdef").is_none(),
            "Debug never shows it"
        );
        assert!(
            read_secret("GitHub token", None, false)
                .unwrap_err()
                .to_string()
                .contains("never as an argument")
        );
        assert!(read_secret("GitHub token", Some(&p), true).is_err());
    }

    struct FakeGh {
        queued: Mutex<Vec<usize>>,
        busy: AtomicUsize,
        jits: AtomicUsize,
        removed: Mutex<Vec<u64>>,
        names: Mutex<Vec<String>>,
        labels: Mutex<Vec<Vec<String>>>,
    }
    impl FakeGh {
        fn new(queued: Vec<usize>) -> Self {
            Self {
                queued: Mutex::new(queued),
                busy: AtomicUsize::new(0),
                jits: AtomicUsize::new(0),
                removed: Mutex::default(),
                names: Mutex::default(),
                labels: Mutex::default(),
            }
        }
    }
    impl Github for FakeGh {
        fn queued(&self, _: &[String]) -> Result<Option<usize>> {
            let mut q = self.queued.lock().unwrap();
            Ok(Some(if q.len() > 1 {
                q.remove(0)
            } else {
                q.first().copied().unwrap_or(0)
            }))
        }
        fn busy(&self) -> Result<usize> {
            Ok(self.busy.load(Ordering::SeqCst))
        }
        fn jit(&self, name: &str, labels: &[String]) -> Result<Jit> {
            let n = self.jits.fetch_add(1, Ordering::SeqCst) as u64 + 100;
            self.names.lock().unwrap().push(name.into());
            self.labels.lock().unwrap().push(labels.to_vec());
            Ok(Jit {
                runner_id: n,
                config: Secret::new(format!("JITSECRET-{n}").into_bytes()),
            })
        }
        fn remove(&self, id: u64) -> Result<()> {
            self.removed.lock().unwrap().push(id);
            Ok(())
        }
    }

    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep(&self, s: u64) {
            self.0.fetch_add(s, Ordering::SeqCst);
        }
    }

    fn probe_reply(macos: usize) -> String {
        let items: Vec<_> = (0..macos)
            .map(|_| serde_json::json!({"status":"running","request":{"vcpus":4,"memory_mib":8192,"apple":{"guest_os":"macos"}}}))
            .collect();
        format!(
            "os=Darwin\narch=arm64\nmacos=15.1\nfluxctl_version=0.4.21\ndaemon_healthy=yes\ndaemon_loaded=yes\ndisk_free_gib=300\nvms_json={}\nncpu=10\nmemsize={}\npower=ac\nimages_json=[{{\"reference\":\"reg.example/runner:1\",\"digest\":\"{D1}\"}}]\n",
            serde_json::json!({"items": items}),
            32u64 << 30
        )
    }

    #[test]
    fn one_job_gets_one_fresh_vm_with_the_jit_config_on_stdin_only() {
        let tr = FakeTransport::new(|_, c| {
            assert!(!c.contains("JITSECRET"), "never in a command line: {c}");
            if c.contains("ncpu=") {
                ok(&probe_reply(0))
            } else if c.contains("mac rm") {
                ok("")
            } else {
                ok("runner: job done JITSECRET-100\n")
            }
        });
        let targets = vec![Target::parse("admin@mini-1.local").unwrap()];
        let gh = FakeGh::new(vec![1, 0]);
        let d = tempfile::tempdir().unwrap();
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(0));
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_owned());
        let place = PlaceFlags::parse_for_test();
        let job = place
            .job(&format!("reg.example/runner:1@{D1}"), 0, 0)
            .unwrap();
        let labels = runner_labels(&["fluxvm".into()]);
        let state = d.path().join("s.json");
        let ctx = Ctx {
            gh: &gh,
            tr: &tr,
            targets: &targets,
            place: &place,
            job: &job,
            leases: &leases,
            state: &state,
            clock: &clock,
            labels: &labels,
            runner_dir: "/Users/admin/actions-runner",
            wait: 900,
            jobs: 1,
            log: &log,
        };
        let reps = supervise(&ctx, 2, 0, true, 5, None);
        assert_eq!(reps.len(), 1);
        assert!(reps[0].ok, "{}", reps[0].detail);
        // one JIT config, one runner removal afterwards, labels carried
        assert_eq!(gh.jits.load(Ordering::SeqCst), 1);
        assert_eq!(*gh.removed.lock().unwrap(), vec![100]);
        assert!(gh.names.lock().unwrap()[0].starts_with("fluxvm-"));
        assert!(gh.labels.lock().unwrap()[0].contains(&"fluxvm".to_owned()));
        let calls = tr.calls_for("mini-1.local");
        let run = calls
            .iter()
            .find(|c| c.command.contains("mac run --rm"))
            .unwrap();
        assert_eq!(run.stdin.as_deref(), Some(&b"JITSECRET-100\n"[..]));
        assert!(run.command.contains("-e ACTIONS_RUNNER_INPUT_JITCONFIG"));
        assert!(run.command.contains("'/Users/admin/actions-runner/run.sh'"));
        // the guest is deleted after the job even though the job succeeded
        assert!(
            calls
                .iter()
                .any(|c| c.command.contains("mac rm ") && c.command.contains("fleet-")),
            "one VM per job, deleted after"
        );
        // no log line carries the secret or the token
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .all(|l| !l.contains("JITSECRET")),
            "{:?}",
            logged.lock().unwrap()
        );
    }

    #[test]
    fn the_runner_loop_respects_the_two_vm_cap_and_waits_for_a_slot() {
        // Both slots of the only Mac are taken by someone else's guests: the job queues, then runs when one ends.
        let looks = AtomicUsize::new(0);
        let tr = FakeTransport::new(move |_, c| {
            if c.contains("ncpu=") {
                let n = looks.fetch_add(1, Ordering::SeqCst);
                ok(&probe_reply(if n < 2 { 2 } else { 1 }))
            } else {
                ok("")
            }
        });
        let targets = vec![Target::parse("admin@mini-1.local").unwrap()];
        let gh = FakeGh::new(vec![1]);
        let d = tempfile::tempdir().unwrap();
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(0));
        let logged = Mutex::new(vec![]);
        let log = |m: &str| logged.lock().unwrap().push(m.to_owned());
        let mut place = PlaceFlags::parse_for_test();
        place.queue_timeout = 120;
        let job = place
            .job(&format!("reg.example/runner:1@{D1}"), 0, 0)
            .unwrap();
        let labels = runner_labels(&[]);
        let state = d.path().join("s.json");
        let ctx = Ctx {
            gh: &gh,
            tr: &tr,
            targets: &targets,
            place: &place,
            job: &job,
            leases: &leases,
            state: &state,
            clock: &clock,
            labels: &labels,
            runner_dir: "/r",
            wait: 60,
            jobs: 1,
            log: &log,
        };
        let reps = supervise(&ctx, 2, 0, true, 5, None);
        assert!(reps[0].ok, "{}", reps[0].detail);
        assert!(
            logged
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("queued") && l.contains("both macOS slots"))
        );
        assert!(clock.now() > 0);
    }

    #[test]
    fn a_failed_placement_still_unregisters_the_runner() {
        let tr = FakeTransport::new(|_, c| {
            assert!(c.contains("ncpu="));
            ok(&probe_reply(2))
        });
        let targets = vec![Target::parse("admin@mini-1.local").unwrap()];
        let gh = FakeGh::new(vec![1]);
        let d = tempfile::tempdir().unwrap();
        let leases = Mutex::new(Leases::default());
        let clock = FakeClock(AtomicU64::new(0));
        let log = |_: &str| {};
        let place = PlaceFlags::parse_for_test();
        let job = place.job("reg.example/runner:1", 0, 0).unwrap();
        let labels = runner_labels(&[]);
        let state = d.path().join("s.json");
        let ctx = Ctx {
            gh: &gh,
            tr: &tr,
            targets: &targets,
            place: &place,
            job: &job,
            leases: &leases,
            state: &state,
            clock: &clock,
            labels: &labels,
            runner_dir: "/r",
            wait: 60,
            jobs: 1,
            log: &log,
        };
        let rep = serve_one(&ctx);
        assert!(!rep.ok);
        assert!(rep.detail.contains("no Mac has room"), "{}", rep.detail);
        assert_eq!(
            *gh.removed.lock().unwrap(),
            vec![100],
            "the unused JIT runner is deleted"
        );
    }
}
