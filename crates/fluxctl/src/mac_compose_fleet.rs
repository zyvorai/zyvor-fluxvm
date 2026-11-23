// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac compose up --fleet INVENTORY`: one compose project spread over a Mac fleet. Each Mac runs at most 2 macOS VMs,
//! so the planner puts every service (and every `--scale` copy) on a Mac that holds its image and has a free slot, with the same
//! hard filters and scoring as `mac-fleet run` ([`crate::mac_fleet_sched::evaluate`]), and says why in plain English.
//!
//! The plan is pure and is what `compose up --dry-run --fleet` prints. Starting is `fluxctl mac run` on the chosen Mac over ssh
//! (like the fleet scheduler), so that Mac's own daemon creates the guest.
//!
//! Secrets: the `environment:` values of a service travel on the ssh STDIN pipe, never in the command line (so not in `ps`, in
//! the dry-run output or in a log). On the Mac they go to a fresh 0600 file that is sourced and removed at once (and also on an
//! error or a signal), the same way `mac-fleet` forwards a registry login. Only the variable NAMES are in the command line.
//!
//! Not verified on real Macs: the plan, the scripts and the ordering are unit tested against a fake ssh only.

use crate::mac_compose::{Resolved, ServicePlan};
use crate::mac_compose_ext::Condition;
use crate::mac_errors::coded;
use crate::mac_fleet::{last_lines, par_map};
use crate::mac_fleet_remote::{PRELUDE, Secret, remote, sh_quote};
use crate::mac_fleet_sched::{
    FleetState, HostView, JobSpec, Kind, Leases, Reason, TemplateRef, evaluate,
};
use crate::mac_fleet_ssh::{Target, Transport, explain_ssh_failure};
use crate::mac_lifecycle::HcSetting;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

pub const PROJECT_KEY: &str = "fluxvm.compose.project";
pub const SERVICE_KEY: &str = "fluxvm.compose.service";
const MAX_PER_MAC: u32 = 2;

/// Variables that would break the remote shell or `fluxctl` itself if they were set there before `mac run` reads them.
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "PATH" | "HOME" | "TMPDIR" | "IFS" | "FLUXCTL" | "ROOT" | "GUI" | "ENVF" | "SHELL" | "USER"
    ) || name.starts_with("DYLD_")
        || name.starts_with("LD_")
}

// ---- refusals --------------------------------------------------------------------------------------------------------

/// A fleet Mac cannot see this Mac's folders: bind-mount `volumes:`, `secrets:` and `configs:` are refused with the way out.
pub fn refuse_local_files(r: &Resolved) -> Result<()> {
    for s in &r.services {
        if let Some(m) = s.spec.mounts.first() {
            return Err(coded(
                format!(
                    "service {:?} has `volumes:` ({}): that folder is on this Mac, and the other Macs of a fleet cannot see it. Remove it, or bake the files into the image (Macfile COPY) and push the image",
                    s.service,
                    s.spec
                        .mounts
                        .iter()
                        .map(|m| m.host.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ) + &format!(" [first: {} -> {}]", m.host.display(), m.guest),
                "MAC_COMPOSE_FLEET_VOLUMES",
            ));
        }
        if !s.files.is_empty() {
            return Err(coded(
                format!(
                    "service {:?} uses `secrets:` or `configs:`, which are files shared from this Mac, and the other Macs of a fleet cannot see them. Pass the value in `environment:` (it travels over ssh stdin, not in the command line), or bake it into the image",
                    s.service
                ),
                "MAC_COMPOSE_FLEET_VOLUMES",
            ));
        }
    }
    Ok(())
}

// ---- the plan --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Assignment {
    /// Index into `Resolved::services`.
    pub idx: usize,
    pub instance: String,
    /// `None` when no Mac can take it.
    pub host: Option<String>,
    /// Why this Mac, in words.
    pub why: String,
    /// Every other Mac and why it was not used.
    pub others: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct FleetPlan {
    pub assignments: Vec<Assignment>,
}

impl FleetPlan {
    pub fn ok(&self) -> bool {
        self.assignments.iter().all(|a| a.host.is_some())
    }
}

/// The fixed host ports a service publishes (`HOST:GUEST` or `ADDR:HOST:GUEST`; `0` means "any").
fn fixed_host_ports(sp: &ServicePlan) -> Vec<String> {
    sp.spec
        .publish
        .iter()
        .filter_map(|p| {
            let parts: Vec<&str> = p.split(':').collect();
            match parts.as_slice() {
                [h, _] | [_, h, _] => Some((*h).to_string()),
                _ => None,
            }
        })
        .filter(|h| h != "0")
        .collect()
}

fn job_for(sp: &ServicePlan) -> Result<JobSpec> {
    Ok(JobSpec {
        template: TemplateRef::parse(&sp.spec.image)?,
        cpus: u32::from(sp.spec.cpus.unwrap_or(0)),
        memory_mib: sp.spec.memory_mib.unwrap_or(0),
        disk_gib: sp.placement.disk_gib,
        labels: sp.placement.labels.clone(),
        ac_only: sp.placement.ac_only,
        host: sp.placement.host.clone(),
    })
}

fn reasons_text(s: &std::result::Result<i64, Vec<Reason>>) -> String {
    match s {
        Ok(_) => "could also take it, but another Mac ranked higher (fewer copies of this service, then the tightest fit)".into(),
        Err(rs) => format!(
            "cannot take it: {}",
            rs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("; ")
        ),
    }
}

/// Place every instance of the project, in start order. A one-shot that others wait for gives its slot back before they are
/// placed, because its VM is stopped as soon as it succeeds.
pub fn plan(r: &Resolved, views: &[HostView], drain: &FleetState) -> FleetPlan {
    let mut leases = Leases::default();
    let mut ports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut copies: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut out = Vec::new();
    for (idx, sp) in r.services.iter().enumerate() {
        for (dep, cond) in &sp.conditions {
            if *cond == Condition::Completed {
                for d in r.services.iter().filter(|d| &d.service == dep) {
                    leases.release(&d.spec.name);
                }
            }
        }
        let instance = sp.spec.name.clone();
        let job = match job_for(sp) {
            Ok(j) => j,
            Err(e) => {
                out.push(Assignment {
                    idx,
                    instance,
                    host: None,
                    why: format!(
                        "the image {:?} is not a usable reference: {e:#}",
                        sp.spec.image
                    ),
                    others: vec![],
                });
                continue;
            }
        };
        let want_ports = fixed_host_ports(sp);
        let mut best: Option<((u32, i64, usize), u32)> = None;
        let mut scores: Vec<std::result::Result<i64, Vec<Reason>>> = vec![];
        for (i, h) in views.iter().enumerate() {
            let mut res = evaluate(h, &job, leases.usage(&h.name, 0), drain.is_drained(&h.name));
            if res.is_ok()
                && let Some(p) = want_ports
                    .iter()
                    .find(|p| ports.get(&h.name).is_some_and(|u| u.contains(*p)))
            {
                res = Err(vec![Reason {
                    kind: Kind::Permanent,
                    text: format!(
                        "host port {p} is already published by another service placed there"
                    ),
                }]);
            }
            if let Ok(score) = &res {
                let same = copies
                    .get(&(h.name.clone(), sp.service.clone()))
                    .copied()
                    .unwrap_or(0);
                // Fewest copies of this service first, then the best score, then inventory order.
                let key = (same, -*score, i);
                if best.is_none_or(|(b, _)| key < b) {
                    best = Some((key, same));
                }
            }
            scores.push(res);
        }
        let others_except = |skip: Option<usize>| -> Vec<(String, String)> {
            views
                .iter()
                .zip(&scores)
                .enumerate()
                .filter(|(i, _)| Some(*i) != skip)
                .map(|(_, (h, s))| (h.name.clone(), reasons_text(s)))
                .collect()
        };
        let Some(((_, _, bi), same)) = best else {
            out.push(Assignment {
                idx,
                instance,
                host: None,
                why: "no Mac can take it".into(),
                others: others_except(None),
            });
            continue;
        };
        let h = &views[bi];
        let slot = h.caps.macos_guests + leases.usage(&h.name, 0).macos + 1;
        let mut why = vec![
            "it holds the image".to_string(),
            format!("it will then run {slot} of {MAX_PER_MAC} macOS VMs"),
        ];
        if sp.replicas > 1 {
            why.push(if same == 0 {
                format!(
                    "no other copy of {} is there, so the copies are spread out",
                    sp.service
                )
            } else {
                format!(
                    "every Mac that could take it already has a copy of {}, so it shares one",
                    sp.service
                )
            });
        }
        if job.host.is_some() {
            why.push("the service is pinned to it by `placement: host`".into());
        }
        leases.acquire(&instance, &h.name, &job, 0);
        ports.entry(h.name.clone()).or_default().extend(want_ports);
        *copies
            .entry((h.name.clone(), sp.service.clone()))
            .or_default() += 1;
        out.push(Assignment {
            idx,
            instance,
            host: Some(h.name.clone()),
            why: why.join("; "),
            others: others_except(Some(bi)),
        });
    }
    FleetPlan { assignments: out }
}

/// The preview `compose up --dry-run --fleet` prints.
pub fn render(r: &Resolved, p: &FleetPlan, views: &[HostView]) -> String {
    let mut by_host: BTreeMap<&str, Vec<&Assignment>> = BTreeMap::new();
    for a in &p.assignments {
        if let Some(h) = a.host.as_deref() {
            by_host.entry(h).or_default().push(a);
        }
    }
    let placed: usize = by_host.values().map(Vec::len).sum();
    let mut s = format!(
        "project {}: {} macOS VM(s) over {} of {} Mac(s). Each Mac runs at most {MAX_PER_MAC} macOS VMs at once (Apple's licence).\n\n",
        r.project,
        r.services.len(),
        by_host.len(),
        views.len()
    );
    for (h, list) in &by_host {
        let names: Vec<&str> = list.iter().map(|a| a.instance.as_str()).collect();
        s.push_str(&format!("  {h}: {}\n", names.join(", ")));
        if list.len() > MAX_PER_MAC as usize {
            s.push_str("    (more than 2 in total, but the one-shot services stop before the later ones start)\n");
        }
    }
    s.push_str("\nWhy:\n");
    for a in &p.assignments {
        let sp = &r.services[a.idx];
        match &a.host {
            Some(h) => s.push_str(&format!("  {} -> {h}: {}.\n", a.instance, a.why)),
            None => s.push_str(&format!("  {} -> NOWHERE: {}.\n", a.instance, a.why)),
        }
        for (o, t) in &a.others {
            s.push_str(&format!("      {o}: {t}\n"));
        }
        if !sp.env.is_empty() {
            s.push_str(&format!(
                "      note: its {} environment value(s) go to that Mac over ssh stdin, not in the command line.\n",
                sp.env.len()
            ));
        }
        if let Some(n) = sp.restart_limit {
            s.push_str(&format!(
                "      note: `restart: on-failure:{n}` is not counted on a fleet Mac: that Mac's `mac reconcile` starts it after every failure.\n"
            ));
        }
        if sp.conditions.values().any(|c| *c == Condition::Completed) {
            s.push_str("      it starts only after the one-shot service it needs has finished and been stopped.\n");
        }
    }
    s.push_str(&format!(
        "\n{placed} of {} placed. Services find each other through COMPOSE_<SERVICE>_HOST (the Mac's address) in their environment.\n",
        p.assignments.len()
    ));
    s
}

/// The error for a plan with an instance that has no Mac.
pub fn unplaced_error(p: &FleetPlan) -> anyhow::Error {
    let names: Vec<&str> = p
        .assignments
        .iter()
        .filter(|a| a.host.is_none())
        .map(|a| a.instance.as_str())
        .collect();
    coded(
        format!(
            "no Mac of the inventory can take: {}. Each Mac runs at most {MAX_PER_MAC} macOS VMs at once and must hold the image; the lines above say why for each Mac",
            names.join(", ")
        ),
        "MAC_COMPOSE_FLEET_NO_ROOM",
    )
}

// ---- starting, listing and removing over ssh -------------------------------------------------------------------------

fn host_env_name(service: &str) -> String {
    format!(
        "COMPOSE_{}_HOST",
        service.to_ascii_uppercase().replace(['-', '.'], "_")
    )
}

fn valid_env_name(n: &str) -> bool {
    n.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The `KEY='value'` lines that go to the Mac on stdin.
fn env_file(env: &[(String, String)]) -> Secret {
    let mut t = String::new();
    for (k, v) in env {
        t.push_str(&format!("{k}={}\n", sh_quote(v)));
    }
    Secret::new(t.into_bytes())
}

/// The ssh script that creates one service on its Mac, and the secret that goes on its stdin (the environment, when it has one).
/// `healthy` says another service waits for this one to be healthy.
pub fn start_job(
    sp: &ServicePlan,
    extra_env: &[(String, String)],
    wait: u64,
    healthy: bool,
) -> Result<(String, Option<Secret>)> {
    let env: Vec<(String, String)> = sp.env.iter().chain(extra_env).cloned().collect();
    for (k, _) in &env {
        if !valid_env_name(k) || reserved(k) {
            return Err(coded(
                format!(
                    "service {:?}: the environment variable {k:?} cannot be sent to a fleet Mac (names are letters, digits and _, and PATH, HOME, TMPDIR, IFS, SHELL, USER, DYLD_* and LD_* are reserved there). Rename it",
                    sp.service
                ),
                "MAC_COMPOSE_FLEET_START",
            ));
        }
    }
    let mut s = format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac (fluxctl mac-fleet up)' >&2; exit 127; }}\n"
    );
    if !env.is_empty() {
        s.push_str(
            "umask 077\n\
ENVF=$(mktemp \"${TMPDIR:-/tmp}/fluxvm-env.XXXXXX\") || exit 1\n\
trap 'rm -f \"$ENVF\"' EXIT HUP INT TERM\n\
chmod 600 \"$ENVF\"\n\
cat > \"$ENVF\"\n\
set -a; . \"$ENVF\"; set +a\n\
rm -f \"$ENVF\"\n",
        );
    }
    let detach = !sp.run_to_completion && !sp.command.is_empty();
    s.push_str(&format!(
        "\"$FLUXCTL\" mac run{} --name {} --label {} --label {} --wait {wait}",
        if detach { " -d" } else { "" },
        sh_quote(&sp.spec.name),
        sh_quote(&format!(
            "{PROJECT_KEY}={}",
            sp.spec.project.as_ref().map_or("", |p| p.0.as_str())
        )),
        sh_quote(&format!("{SERVICE_KEY}={}", sp.service)),
    ));
    for (k, v) in &sp.spec.labels {
        s.push_str(&format!(" --label {}", sh_quote(&format!("{k}={v}"))));
    }
    if let Some(c) = sp.spec.cpus {
        s.push_str(&format!(" --cpus {c}"));
    }
    if let Some(m) = sp.spec.memory_mib {
        s.push_str(&format!(" --memory {m}m"));
    }
    if let Some(r) = sp.spec.restart {
        s.push_str(&format!(" --restart {}", r.as_str()));
    }
    for p in &sp.spec.publish {
        s.push_str(&format!(" -p {}", sh_quote(p)));
    }
    // Names only: `mac run -e NAME` reads the value from the environment the sourced file filled.
    for (k, _) in &env {
        s.push_str(&format!(" -e {k}"));
    }
    match &sp.spec.healthcheck {
        Some(HcSetting::Off) => s.push_str(" --no-healthcheck"),
        Some(HcSetting::Cmd(h)) => s.push_str(&format!(
            " --health-cmd {} --health-interval {}s --health-probe-timeout {}s --health-retries {} --health-start-period {}s",
            sh_quote(&h.cmd.to_shell()),
            h.interval_secs,
            h.timeout_secs,
            h.retries,
            h.start_period_secs
        )),
        None => {}
    }
    if healthy {
        s.push_str(" --wait-healthy");
    }
    if let Some(w) = &sp.workdir {
        s.push_str(&format!(" -w {}", sh_quote(w)));
    }
    if let Some(u) = &sp.user {
        s.push_str(&format!(" --user {}", sh_quote(u)));
    }
    s.push_str(&format!(" {}", sh_quote(&sp.spec.image)));
    if !sp.command.is_empty() {
        s.push_str(" --");
        for w in &sp.command {
            s.push(' ');
            s.push_str(&sh_quote(w));
        }
    }
    s.push('\n');
    if sp.run_to_completion {
        // The command ran in the foreground; its status is the script's, and the VM is stopped either way is NOT done: a
        // failed one stays for the operator to look at.
        s.push_str(&format!(
            "rc=$?\n[ $rc -eq 0 ] && \"$FLUXCTL\" mac stop {} >/dev/null 2>&1\nexit $rc\n",
            sh_quote(&sp.spec.name)
        ));
    }
    Ok((s, (!env.is_empty()).then(|| env_file(&env))))
}

fn run_on(
    tr: &dyn Transport,
    t: &Target,
    script: &str,
    stdin: Option<&Secret>,
    timeout: Duration,
) -> std::result::Result<String, String> {
    match tr.run_timed(t, &remote(script), stdin, Some(timeout)) {
        Err(e) => Err(format!("{e:#}")),
        Ok(o) if o.ssh_failed() => Err(explain_ssh_failure(t, &o.stderr)),
        Ok(o) if !o.ok() => Err(format!(
            "exit {}: {}",
            o.code,
            last_lines(
                if o.stderr.trim().is_empty() {
                    &o.stdout
                } else {
                    &o.stderr
                },
                4
            )
        )),
        Ok(o) => Ok(o.stdout),
    }
}

/// Start everything in the plan, in order. On a failure the ones already started stay (the error says so).
pub fn up(
    tr: &dyn Transport,
    targets: &[Target],
    r: &Resolved,
    p: &FleetPlan,
    wait: u64,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<()> {
    let mut started: Vec<String> = vec![];
    let mut addr: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for a in &p.assignments {
        let sp = &r.services[a.idx];
        let host = a.host.as_deref().ok_or_else(|| unplaced_error(p))?;
        let t = targets.iter().find(|t| t.name == host).ok_or_else(|| {
            anyhow::anyhow!("the plan names {host}, which is not in the inventory")
        })?;
        let extra: Vec<(String, String)> = sp
            .depends_on
            .iter()
            .filter_map(|d| addr.get(d).map(|h| (host_env_name(d), h.join(","))))
            .collect();
        let healthy = r
            .services
            .iter()
            .any(|o| o.healthy_deps.contains(&sp.service));
        log(&format!("starting {} on {host} ...", a.instance));
        let (script, stdin) = start_job(sp, &extra, wait, healthy)?;
        let tmo = if sp.run_to_completion || healthy {
            timeout
        } else {
            Duration::from_secs(wait + 120)
        };
        if let Err(e) = run_on(tr, t, &script, stdin.as_ref(), tmo) {
            let already = if started.is_empty() {
                String::new()
            } else {
                format!(
                    " (already started: {}; `fluxctl mac compose down --fleet` removes them)",
                    started.join(", ")
                )
            };
            let code = if sp.run_to_completion {
                "MAC_COMPOSE_DEP_FAILED"
            } else {
                "MAC_COMPOSE_FLEET_START"
            };
            return Err(coded(
                format!("{} on {host} did not start: {e}{already}", a.instance),
                code,
            ));
        }
        started.push(a.instance.clone());
        if !sp.run_to_completion {
            addr.entry(sp.service.clone())
                .or_default()
                .push(t.host.clone());
        }
        println!("{} {host}", a.instance);
    }
    Ok(())
}

fn project_script(project: &str, body: &str) -> String {
    format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || exit 0\nfor n in $(\"$FLUXCTL\" mac ps -a --filter {} --format '{{{{.Name}}}}'); do\n{body}\ndone\n",
        sh_quote(&format!("label={PROJECT_KEY}={project}"))
    )
}

/// Remove the project's instances from every Mac.
pub fn down(
    tr: &dyn Transport,
    targets: &[Target],
    project: &str,
    jobs: usize,
    timeout: Duration,
) -> Vec<(String, std::result::Result<String, String>)> {
    let script = project_script(
        project,
        "  \"$FLUXCTL\" mac rm \"$n\" >/dev/null && echo \"removed $n\"",
    );
    par_map(targets, jobs, |t| {
        (t.name.clone(), run_on(tr, t, &script, None, timeout))
    })
}

/// The project's instances on every Mac.
pub fn ps(
    tr: &dyn Transport,
    targets: &[Target],
    project: &str,
    jobs: usize,
    timeout: Duration,
) -> Vec<(String, std::result::Result<String, String>)> {
    let script = format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || exit 0\n\"$FLUXCTL\" mac ps -a --filter {} --format '{{{{.Name}}}}\\t{{{{.Status}}}}\\t{{{{.IP}}}}'\n",
        sh_quote(&format!("label={PROJECT_KEY}={project}"))
    );
    par_map(targets, jobs, |t| {
        (t.name.clone(), run_on(tr, t, &script, None, timeout))
    })
}

pub fn render_down(rs: &[(String, std::result::Result<String, String>)]) -> String {
    let mut s = String::new();
    for (h, r) in rs {
        match r {
            Ok(out) if out.trim().is_empty() => {
                s.push_str(&format!("{h}: nothing of the project\n"))
            }
            Ok(out) => out.lines().for_each(|l| s.push_str(&format!("{h}: {l}\n"))),
            Err(e) => s.push_str(&format!("{h}: could not remove: {e}\n")),
        }
    }
    s
}

pub fn render_ps(rs: &[(String, std::result::Result<String, String>)]) -> String {
    let mut s = format!("{:<16}  {:<28}  {:<10}  IP\n", "MAC", "NAME", "STATUS");
    for (h, r) in rs {
        match r {
            Ok(out) => {
                for l in out.lines().filter(|l| !l.trim().is_empty()) {
                    let c: Vec<&str> = l.split('\t').collect();
                    s.push_str(&format!(
                        "{:<16}  {:<28}  {:<10}  {}\n",
                        h,
                        c.first().unwrap_or(&""),
                        c.get(1).unwrap_or(&""),
                        c.get(2).unwrap_or(&"")
                    ));
                }
            }
            Err(e) => s.push_str(&format!("{h:<16}  could not be asked: {e}\n")),
        }
    }
    s
}

// ---- the commands ----------------------------------------------------------------------------------------------------

struct Ctx {
    targets: Vec<Target>,
    tr: Box<dyn Transport>,
    jobs: usize,
}

fn ctx(inventory: &Path) -> Result<Ctx> {
    use crate::mac_fleet::{Which, jobs, resolve, transport};
    let conn = crate::mac_fleet_run::default_conn();
    let which = Which {
        hosts: vec![],
        inventory: Some(inventory.to_path_buf()),
        all: true,
    };
    let (targets, inv) = resolve(&which, &conn).map_err(|e| {
        coded(
            format!("cannot read the Macs from {}: {e:#}", inventory.display()),
            "MAC_COMPOSE_FLEET_INVENTORY",
        )
    })?;
    Ok(Ctx {
        targets,
        tr: transport(&conn, inv.as_ref()),
        jobs: jobs(&conn),
    })
}

/// `compose up --fleet` (and `--dry-run --fleet`): plan, print why, and start.
pub fn up_command(r: &Resolved, inventory: &Path, dry_run: bool, wait: u64) -> Result<()> {
    refuse_local_files(r)?;
    let c = ctx(inventory)?;
    let views =
        crate::mac_fleet_run::snapshot(c.tr.as_ref(), &c.targets, c.jobs, Duration::from_secs(30));
    let drain = FleetState::load(&crate::mac_fleet_sched::state_path()?)?;
    let p = plan(r, &views, &drain);
    print!("{}", crate::mac_compose::render_plan(r));
    println!();
    print!("{}", render(r, &p, &views));
    if !p.ok() {
        return Err(unplaced_error(&p));
    }
    if dry_run {
        println!("\nDry run: nothing was started.");
        return Ok(());
    }
    up(
        c.tr.as_ref(),
        &c.targets,
        r,
        &p,
        wait,
        Duration::from_secs(3600),
        &|m| eprintln!("{m}"),
    )
}

pub fn down_command(project: &str, inventory: &Path) -> Result<()> {
    let c = ctx(inventory)?;
    print!(
        "{}",
        render_down(&down(
            c.tr.as_ref(),
            &c.targets,
            project,
            c.jobs,
            Duration::from_secs(600)
        ))
    );
    Ok(())
}

pub fn ps_command(project: &str, inventory: &Path) -> Result<()> {
    let c = ctx(inventory)?;
    print!(
        "{}",
        render_ps(&ps(
            c.tr.as_ref(),
            &c.targets,
            project,
            c.jobs,
            Duration::from_secs(60)
        ))
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_sched::tests::{D1, view};
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};

    const SECRET: &str = "s3cr3t-value-9f2";

    fn res(y: &str) -> Resolved {
        crate::mac_compose::resolve_fleet_text(y).unwrap()
    }

    fn target(name: &str) -> Target {
        Target::parse(&format!("admin@{name}")).unwrap()
    }

    fn views(n: usize, image: &str) -> Vec<HostView> {
        (1..=n)
            .map(|i| view(&format!("mac{i}"), &[(image, D1)]))
            .collect()
    }

    #[test]
    fn four_services_spread_over_two_macs_two_each() {
        let r = res(
            "name: p\nservices:\n  a: {image: 'reg.example/x:1'}\n  b: {image: 'reg.example/x:1'}\n  c: {image: 'reg.example/x:1'}\n  d: {image: 'reg.example/x:1'}\n",
        );
        let p = plan(&r, &views(2, "reg.example/x:1"), &FleetState::default());
        assert!(p.ok(), "{p:?}");
        let on = |h: &str| {
            p.assignments
                .iter()
                .filter(|a| a.host.as_deref() == Some(h))
                .count()
        };
        assert_eq!((on("mac1"), on("mac2")), (2, 2));
        let text = render(&r, &p, &views(2, "reg.example/x:1"));
        assert!(
            text.contains("2 of 2 macOS VMs") && text.contains("4 of 4 placed"),
            "{text}"
        );
    }

    #[test]
    fn a_fifth_vm_has_no_room_and_the_error_says_why() {
        let r = res(
            "name: p\nservices:\n  a: {image: 'reg.example/x:1'}\n  b: {image: 'reg.example/x:1'}\n  c: {image: 'reg.example/x:1'}\n",
        );
        let v = views(1, "reg.example/x:1");
        let p = plan(&r, &v, &FleetState::default());
        assert!(!p.ok());
        let text = render(&r, &p, &v);
        assert!(text.contains("NOWHERE") && text.contains("mac1"), "{text}");
        let e = format!("{:#}", unplaced_error(&p));
        assert!(
            e.contains("p-c") && e.contains("MAC_COMPOSE_FLEET_NO_ROOM"),
            "{e}"
        );
    }

    #[test]
    fn scaled_copies_are_spread_and_a_pinned_service_stays_put() {
        let r =
            res("name: p\nservices:\n  w: {image: 'reg.example/x:1', placement: {host: mac2}}\n");
        let p = plan(&r, &views(2, "reg.example/x:1"), &FleetState::default());
        assert_eq!(p.assignments[0].host.as_deref(), Some("mac2"));
        assert!(p.assignments[0].why.contains("placement: host"));
    }

    #[test]
    fn a_one_shot_gives_its_slot_back_so_three_services_fit_on_one_mac() {
        let r = res(
            "name: p\nservices:\n  mig: {image: 'reg.example/x:1', command: ./migrate}\n  db: {image: 'reg.example/x:1'}\n  app:\n    image: 'reg.example/x:1'\n    depends_on:\n      mig: {condition: service_completed_successfully}\n      db: {condition: service_started}\n",
        );
        let v = views(1, "reg.example/x:1");
        let p = plan(&r, &v, &FleetState::default());
        assert!(p.ok(), "{}", render(&r, &p, &v));
    }

    #[test]
    fn volumes_secrets_and_configs_are_refused_in_plain_english() {
        let d = tempfile::tempdir().unwrap();
        let r = crate::mac_compose::resolve_fleet_text(&format!(
            "name: p\nservices:\n  a:\n    image: x:1\n    volumes: ['{}:/data']\n",
            d.path().display()
        ))
        .unwrap();
        let e = format!("{:#}", refuse_local_files(&r).unwrap_err());
        assert!(
            e.contains("cannot see it") && e.contains("MAC_COMPOSE_FLEET_VOLUMES"),
            "{e}"
        );
    }

    #[test]
    fn environment_values_travel_on_stdin_never_in_the_command_line() {
        let r = res(&format!(
            "name: p\nservices:\n  a:\n    image: 'reg.example/x:1'\n    command: ['./serve']\n    environment:\n      TOKEN: {SECRET}\n      PLAIN: it'sfine\n"
        ));
        let sp = &r.services[0];
        let (script, stdin) = start_job(sp, &[], 900, false).unwrap();
        assert!(
            !script.contains(SECRET) && !script.contains("it'sfine"),
            "{script}"
        );
        assert!(script.contains("-e TOKEN") && script.contains("-e PLAIN"));
        assert!(
            script.contains("umask 077")
                && script.contains("chmod 600")
                && script.contains("cat > \"$ENVF\"")
        );
        assert!(script.contains("trap 'rm -f \"$ENVF\"' EXIT"));
        assert!(script.contains(". \"$ENVF\"") && script.contains("rm -f \"$ENVF\""));
        let body = String::from_utf8(stdin.unwrap().expose().to_vec()).unwrap();
        assert!(
            body.contains(&format!("TOKEN='{SECRET}'")) && body.contains(r"PLAIN='it'\''sfine'"),
            "{body}"
        );
    }

    #[test]
    fn no_value_appears_in_any_argv_over_a_whole_fleet_up() {
        let r = res(&format!(
            "name: p\nservices:\n  db:\n    image: 'reg.example/x:1'\n    environment: {{PGPASSWORD: {SECRET}}}\n  app:\n    image: 'reg.example/x:1'\n    command: ['./serve']\n    depends_on: [db]\n    environment: {{API_KEY: {SECRET}-2}}\n"
        ));
        let v = views(2, "reg.example/x:1");
        let p = plan(&r, &v, &FleetState::default());
        let tr = FakeTransport::new(|_, _| ok(""));
        let targets = vec![target("mac1"), target("mac2")];
        up(&tr, &targets, &r, &p, 60, Duration::from_secs(5), &|_| {}).unwrap();
        let calls: Vec<_> = tr.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        let mut with_stdin = 0;
        for c in &calls {
            assert!(
                !c.command.contains(SECRET),
                "a value is in argv: {}",
                c.command
            );
            if let Some(b) = &c.stdin {
                with_stdin += 1;
                assert!(String::from_utf8_lossy(b).contains(SECRET));
            }
        }
        assert_eq!(
            with_stdin, 2,
            "both services had values, so both sent stdin"
        );
        // The dependent also learns where its dependency is, by name only in argv and by value on stdin.
        let app = calls.iter().find(|c| c.command.contains("p-app")).unwrap();
        assert!(app.command.contains("-e COMPOSE_DB_HOST"));
        assert!(String::from_utf8_lossy(app.stdin.as_ref().unwrap()).contains("COMPOSE_DB_HOST="));
    }

    #[test]
    fn reserved_or_odd_variable_names_are_refused() {
        for bad in ["PATH", "DYLD_INSERT_LIBRARIES", "1X", "A-B"] {
            let r = res("name: p\nservices:\n  a: {image: 'reg.example/x:1'}\n");
            let mut sp = r.services[0].clone();
            sp.env = vec![(bad.to_string(), "v".into())];
            let e = format!("{:#}", start_job(&sp, &[], 60, false).unwrap_err());
            assert!(e.contains("MAC_COMPOSE_FLEET_START"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_one_shot_runs_in_the_foreground_and_is_stopped_only_on_success() {
        let r = res(
            "name: p\nservices:\n  mig: {image: 'reg.example/x:1', command: ['./migrate']}\n  app:\n    image: 'reg.example/x:1'\n    depends_on:\n      mig: {condition: service_completed_successfully}\n",
        );
        let (s, stdin) = start_job(&r.services[0], &[], 60, false).unwrap();
        assert!(stdin.is_none() && !s.contains(" -d "), "{s}");
        assert!(s.contains("[ $rc -eq 0 ] && \"$FLUXCTL\" mac stop") && s.contains("exit $rc"));
    }

    #[test]
    fn healthy_dependencies_wait_on_the_remote_run_with_the_compose_healthcheck() {
        let r = res(
            "name: p\nservices:\n  db:\n    image: 'reg.example/x:1'\n    healthcheck: {test: pg_isready, interval: 5s}\n  app:\n    image: 'reg.example/x:1'\n    depends_on:\n      db: {condition: service_healthy}\n",
        );
        let (s, _) = start_job(&r.services[0], &[], 60, true).unwrap();
        assert!(
            s.contains("--wait-healthy")
                && s.contains("--health-cmd 'pg_isready'")
                && s.contains("--health-interval 5s"),
            "{s}"
        );
    }

    #[test]
    fn a_failing_start_names_the_mac_and_what_already_started() {
        let r = res(
            "name: p\nservices:\n  a: {image: 'reg.example/x:1'}\n  b: {image: 'reg.example/x:1'}\n",
        );
        let v = views(1, "reg.example/x:1");
        let p = plan(&r, &v, &FleetState::default());
        let tr = FakeTransport::new(|_, c| {
            if c.contains("p-b") {
                crate::mac_fleet_ssh::fake::failed(1, "boom")
            } else {
                ok("")
            }
        });
        let e = format!(
            "{:#}",
            up(
                &tr,
                &[target("mac1")],
                &r,
                &p,
                60,
                Duration::from_secs(5),
                &|_| {}
            )
            .unwrap_err()
        );
        assert!(
            e.contains("p-b on mac1")
                && e.contains("already started: p-a")
                && e.contains("MAC_COMPOSE_FLEET_START"),
            "{e}"
        );
    }

    #[test]
    fn down_and_ps_filter_by_the_project_label_on_every_mac() {
        let tr = FakeTransport::new(|_, c| {
            ok(if c.contains(" rm ") {
                "removed p-a\n"
            } else {
                "p-a\trunning\t10.0.0.2\n"
            })
        });
        let targets = vec![target("mac1"), target("mac2")];
        let d = down(&tr, &targets, "p", 2, Duration::from_secs(5));
        assert!(render_down(&d).contains("mac1: removed p-a"));
        let l = render_ps(&ps(&tr, &targets, "p", 2, Duration::from_secs(5)));
        assert!(l.contains("p-a") && l.contains("running"), "{l}");
        assert!(
            tr.calls
                .lock()
                .unwrap()
                .iter()
                .all(|c| c.command.contains("fluxvm.compose.project=p"))
        );
    }
}
