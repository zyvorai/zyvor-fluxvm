// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl up`: one command for a new user. Check the host, reuse or start the daemon, create the default single-server
//! k3s cluster with the same code as `fluxctl kube cluster create`, wait until it is healthy, and print the next three commands.
//!
//! The judging is `fluxvm_core::up` (pure, shared with the daemon's `GET /v1/up/plan` and the Studio Home page). The
//! flow here is written against the [`Steps`] trait so the tests drive it with a fake and never start anything.

use crate::run::VmApi;
use anyhow::{Result, bail};
use fluxvm_core::up::{self, ClusterState, StepState, UpFacts, UpPlan};
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(clap::Args, Clone, Debug, Default)]
pub struct UpArgs {
    /// Do not ask before changing anything.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print the plan and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Machine-readable output (schema version 1).
    #[arg(long)]
    pub json: bool,
}

impl UpArgs {
    /// `--yes`, `--dry-run` and `--json` belong to the new-user setup; with a stack they would be silently ignored.
    pub fn reject_for_stack(&self) -> Result<()> {
        if self.yes || self.dry_run || self.json {
            bail!(
                "--yes, --dry-run and --json are for `fluxctl up` without a fluxvm.toml (the new-user setup). Next: remove them to bring up the stack in this folder, or run `fluxctl up` in a folder without a fluxvm.toml"
            );
        }
        Ok(())
    }
}

/// What `up` needs from the machine. The real one acts; tests record.
pub trait Steps {
    /// This machine as it is now: the host probe, whether the daemon answers, the state of the cluster.
    async fn facts(&self) -> UpFacts;
    /// Start the daemon when that is safe to do by itself (the macOS LaunchAgent). Returns what was done.
    fn start_daemon(&self) -> Result<String>;
    /// Create, resume or start the default cluster: `fluxctl kube cluster create default` / `start`.
    async fn create_cluster(&self) -> Result<String>;
    /// Wait until the cluster answers; false when it did not in time.
    async fn wait_healthy(&self) -> bool;
}

fn mark(s: StepState) -> &'static str {
    match s {
        StepState::Done => "[ok]     ",
        StepState::Todo => "[to do]  ",
        StepState::Warn => "[warn]   ",
        StepState::Blocked => "[blocked]",
    }
}

pub fn render_plan(p: &UpPlan) -> String {
    let mut s = format!("{}\n  {}\n\n", p.headline, p.host);
    for st in &p.steps {
        s.push_str(&format!(
            "  {} {}: {}\n",
            mark(st.state),
            st.title,
            st.detail
        ));
        if st.state != StepState::Done {
            if let Some(f) = &st.fix {
                s.push_str(&format!("            fix: {f}\n"));
            }
            if let Some(c) = &st.code {
                s.push_str(&format!("            more: `fluxctl explain {c}`\n"));
            }
        }
    }
    s
}

pub fn render_next(p: &UpPlan) -> String {
    let mut s = String::from("\nNext, try:\n");
    for c in &p.next_commands {
        s.push_str(&format!("  {}\n      {}\n", c.command, c.why));
    }
    s
}

fn doc(
    before: &UpPlan,
    dry_run: bool,
    applied: &[String],
    error: Option<&str>,
    after: Option<&UpPlan>,
) -> serde_json::Value {
    json!({
        "schema": up::SCHEMA,
        "dry_run": dry_run,
        "changed": !applied.is_empty(),
        "applied": applied,
        "error": error,
        "plan": before,
        "after": after,
    })
}

/// The whole flow. Returns the exit code: 0 done (or nothing to do, or a dry run that can run), 1 when something blocks
/// or fails. `confirm` is asked once, only when something will change and `--yes` was not given.
pub async fn run<S: Steps>(
    s: &S,
    a: &UpArgs,
    out: &mut dyn Write,
    confirm: &mut dyn FnMut() -> Result<bool>,
) -> Result<i32> {
    let before = up::assess(&s.facts().await);
    if !a.json {
        write!(out, "{}", render_plan(&before))?;
    }
    if a.dry_run {
        if a.json {
            let d = doc(&before, true, &[], None, None);
            writeln!(out, "{}", serde_json::to_string_pretty(&d)?)?;
        } else {
            writeln!(
                out,
                "\nDry run: nothing was changed. Run `{}` to do it.",
                up::COMMAND
            )?;
        }
        return Ok(i32::from(!before.can_run));
    }
    if !before.can_run {
        if a.json {
            let d = doc(&before, false, &[], Some("blocked"), None);
            writeln!(out, "{}", serde_json::to_string_pretty(&d)?)?;
        } else {
            writeln!(
                out,
                "\nNothing was changed. Do the fix on each [blocked] line, then run `{}` again.",
                up::COMMAND
            )?;
        }
        return Ok(1);
    }
    if before.nothing_to_do {
        if a.json {
            let d = doc(&before, false, &[], None, None);
            writeln!(out, "{}", serde_json::to_string_pretty(&d)?)?;
        } else {
            write!(
                out,
                "\nNothing to do: this machine is already set up. Nothing was changed.\n{}",
                render_next(&before)
            )?;
        }
        return Ok(0);
    }
    if !a.yes && !confirm()? {
        writeln!(out, "Nothing changed.")?;
        return Ok(0);
    }
    let mut applied: Vec<String> = vec![];
    let mut failed: Option<String> = None;
    if before
        .steps
        .iter()
        .any(|st| st.id == "daemon" && st.state == StepState::Warn)
    {
        match s.start_daemon() {
            Ok(m) => {
                if !a.json {
                    writeln!(out, "\n{m}")?;
                }
                applied.push(m);
            }
            // Not fatal: the command line works without the daemon. The plan already printed how to start it.
            Err(e) => {
                if !a.json {
                    writeln!(
                        out,
                        "\nThe daemon was not started ({e:#}); continuing without it. Studio needs it: `fluxctl explain DAEMON_DOWN`."
                    )?;
                }
            }
        }
    }
    if before
        .steps
        .iter()
        .any(|st| st.id == "cluster" && st.state == StepState::Todo)
    {
        if !a.json {
            writeln!(
                out,
                "\nCreating the cluster (this is `fluxctl kube cluster create default`) ..."
            )?;
        }
        match s.create_cluster().await {
            Ok(m) => applied.push(m),
            Err(e) => {
                failed = Some(format!(
                    "could not create the cluster: {e:#}. Next: run `fluxctl kube doctor` to see which check fails, `fluxctl explain K3S_NOT_READY` for the usual causes, then `{}` again (it picks up where it stopped).",
                    up::COMMAND
                ));
            }
        }
    }
    if failed.is_none() && !s.wait_healthy().await {
        failed = Some(format!(
            "the cluster did not answer in time. Next: `fluxctl kube doctor` names the failing check, `fluxctl explain K3S_NOT_READY` lists the usual causes, then `{}` again.",
            up::COMMAND
        ));
    }
    let after = up::assess(&s.facts().await);
    if a.json {
        let d = doc(&before, false, &applied, failed.as_deref(), Some(&after));
        writeln!(out, "{}", serde_json::to_string_pretty(&d)?)?;
    } else if let Some(e) = &failed {
        writeln!(out, "\nFailed: {e}")?;
    } else {
        write!(
            out,
            "\nDone: the cluster is up and healthy.\n{}",
            render_next(&after)
        )?;
    }
    Ok(i32::from(failed.is_some()))
}

// ---- the real machine --------------------------------------------------------------------------------------------------

pub struct RealSteps<'a, A: VmApi> {
    pub api: &'a A,
    pub listen: String,
    pub state_dir: PathBuf,
}

impl<A: VmApi> Steps for RealSteps<'_, A> {
    async fn facts(&self) -> UpFacts {
        let mut f = up::probe(&self.state_dir);
        let doctor = crate::doctor::gather(&self.listen, &self.state_dir, false).await;
        f.daemon_up = Some(
            !doctor
                .iter()
                .any(|c| c.name == "daemon" && c.level == crate::doctor::Level::Fail),
        );
        f.cluster = crate::kube_apple::up_glue::probe(self.api).await;
        f
    }
    fn start_daemon(&self) -> Result<String> {
        use crate::cluster_cmd::Applier;
        if !cfg!(target_os = "macos") {
            bail!("on Linux the daemon is a system service: run `sudo systemctl start fluxvm`");
        }
        crate::cluster_cmd::RealApplier.start_service()?;
        Ok("started the FluxVM LaunchAgent".into())
    }
    async fn create_cluster(&self) -> Result<String> {
        use crate::kube_apple::up_glue;
        // A cluster that exists but is stopped (or not answering) is started; anything else is created or resumed by
        // `kube cluster create`, which journals every step.
        match up_glue::probe(self.api).await {
            ClusterState::Stopped | ClusterState::NotAnswering => up_glue::start(self.api).await,
            _ => up_glue::create(self.api).await,
        }
    }
    async fn wait_healthy(&self) -> bool {
        for _ in 0..24 {
            if crate::kube_apple::up_glue::healthy(self.api).await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        false
    }
}

/// Ask on the terminal. Without one, say what to do instead of guessing.
pub fn ask_on_terminal() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to go ahead, or --dry-run to only look");
    }
    eprint!("\nGo ahead? [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        facts: RefCell<Vec<UpFacts>>,
        calls: RefCell<Vec<&'static str>>,
        create_err: bool,
        healthy: bool,
    }
    impl Fake {
        /// `first` is what the first look sees, `then` every look after the changes.
        fn new(first: UpFacts, then: UpFacts) -> Self {
            Fake {
                facts: RefCell::new(vec![then, first]),
                calls: RefCell::default(),
                create_err: false,
                healthy: true,
            }
        }
    }
    impl Steps for Fake {
        async fn facts(&self) -> UpFacts {
            let mut v = self.facts.borrow_mut();
            if v.len() > 1 {
                v.pop().unwrap()
            } else {
                v[0].clone()
            }
        }
        fn start_daemon(&self) -> Result<String> {
            self.calls.borrow_mut().push("daemon");
            Ok("started".into())
        }
        async fn create_cluster(&self) -> Result<String> {
            self.calls.borrow_mut().push("create");
            if self.create_err {
                bail!("k3s download failed")
            }
            Ok("created".into())
        }
        async fn wait_healthy(&self) -> bool {
            self.calls.borrow_mut().push("wait");
            self.healthy
        }
    }

    fn good(cluster: ClusterState) -> UpFacts {
        UpFacts {
            hostname: "box".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            platform: fluxvm_core::health::Platform::Linux,
            kvm: up::Kvm::Ready,
            disk_free_gib: Some(100),
            mem_total_mib: Some(16384),
            mem_available_mib: Some(8000),
            daemon_up: Some(true),
            cluster,
            ..Default::default()
        }
    }
    async fn go(f: &Fake, a: UpArgs, answer: bool) -> (i32, String, Vec<&'static str>) {
        let mut out = Vec::new();
        let code = run(f, &a, &mut out, &mut || Ok(answer)).await.unwrap();
        (
            code,
            String::from_utf8(out).unwrap(),
            f.calls.borrow().clone(),
        )
    }
    fn yes() -> UpArgs {
        UpArgs {
            yes: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_healthy_setup_changes_nothing_and_says_so() {
        let f = Fake::new(good(ClusterState::Healthy), good(ClusterState::Healthy));
        let (code, out, calls) = go(&f, UpArgs::default(), false).await;
        assert_eq!(code, 0);
        assert!(calls.is_empty(), "{calls:?}");
        assert!(out.contains("Nothing to do") && out.contains("Nothing was changed"));
        assert!(out.contains("Next, try:") && out.contains("fluxctl cluster status"));
    }

    #[tokio::test]
    async fn dry_run_prints_the_plan_and_touches_nothing() {
        let f = Fake::new(good(ClusterState::Missing), good(ClusterState::Healthy));
        let a = UpArgs {
            dry_run: true,
            ..Default::default()
        };
        let (code, out, calls) = go(&f, a, true).await;
        assert_eq!(code, 0);
        assert!(calls.is_empty());
        assert!(out.contains("[to do]") && out.contains("Dry run: nothing was changed"));
    }

    #[tokio::test]
    async fn a_new_machine_is_set_up_with_yes_and_ends_with_next_commands() {
        let f = Fake::new(good(ClusterState::Missing), good(ClusterState::Healthy));
        let (code, out, calls) = go(&f, yes(), false).await;
        assert_eq!(code, 0);
        assert_eq!(calls, vec!["create", "wait"]);
        assert!(out.contains("Done: the cluster is up") && out.contains("fluxctl dashboard"));
    }

    #[tokio::test]
    async fn declining_the_question_changes_nothing() {
        let f = Fake::new(good(ClusterState::Missing), good(ClusterState::Healthy));
        let (code, out, calls) = go(&f, UpArgs::default(), false).await;
        assert_eq!(code, 0);
        assert!(calls.is_empty());
        assert!(out.contains("Nothing changed."));
    }

    #[tokio::test]
    async fn a_blocked_host_stops_before_changing_anything_and_names_the_fix() {
        let mut bad = good(ClusterState::Missing);
        bad.kvm = up::Kvm::Missing;
        let f = Fake::new(bad.clone(), bad);
        let (code, out, calls) = go(&f, yes(), true).await;
        assert_eq!(code, 1);
        assert!(calls.is_empty());
        assert!(
            out.contains("[blocked]")
                && out.contains("fluxctl explain NO_KVM")
                && out.contains("BIOS")
        );
    }

    #[tokio::test]
    async fn a_failed_create_names_the_next_step_and_exits_1() {
        let mut f = Fake::new(good(ClusterState::Missing), good(ClusterState::Missing));
        f.create_err = true;
        let (code, out, calls) = go(&f, yes(), true).await;
        assert_eq!(code, 1);
        assert_eq!(calls, vec!["create"]);
        assert!(
            out.contains("k3s download failed")
                && out.contains("fluxctl kube doctor")
                && out.contains("fluxctl up")
        );
    }

    #[tokio::test]
    async fn a_cluster_that_never_answers_says_what_to_do() {
        let mut f = Fake::new(
            good(ClusterState::Missing),
            good(ClusterState::NotAnswering),
        );
        f.healthy = false;
        let (code, out, _) = go(&f, yes(), true).await;
        assert_eq!(code, 1);
        assert!(out.contains("did not answer in time") && out.contains("K3S_NOT_READY"));
    }

    #[tokio::test]
    async fn a_down_daemon_is_started_first() {
        let mut facts = good(ClusterState::Missing);
        facts.daemon_up = Some(false);
        let f = Fake::new(facts, good(ClusterState::Healthy));
        let (code, _, calls) = go(&f, yes(), true).await;
        assert_eq!(code, 0);
        assert_eq!(calls, vec!["daemon", "create", "wait"]);
    }

    #[tokio::test]
    async fn json_output_is_one_versioned_document() {
        let f = Fake::new(good(ClusterState::Missing), good(ClusterState::Healthy));
        let a = UpArgs {
            yes: true,
            json: true,
            ..Default::default()
        };
        let (code, out, _) = go(&f, a, true).await;
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["changed"], true);
        assert_eq!(v["after"]["nothing_to_do"], true);
        let d = Fake::new(good(ClusterState::Missing), good(ClusterState::Missing));
        let a = UpArgs {
            dry_run: true,
            json: true,
            ..Default::default()
        };
        let (_, out, _) = go(&d, a, true).await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["plan"]["command"], "fluxctl up");
    }

    #[test]
    fn setup_flags_are_refused_for_a_stack() {
        assert!(UpArgs::default().reject_for_stack().is_ok());
        for a in [
            UpArgs {
                yes: true,
                ..Default::default()
            },
            UpArgs {
                dry_run: true,
                ..Default::default()
            },
            UpArgs {
                json: true,
                ..Default::default()
            },
        ] {
            let e = a.reject_for_stack().unwrap_err().to_string();
            assert!(e.contains("without a fluxvm.toml") && e.contains("Next:"));
        }
    }

    #[test]
    fn the_plan_text_has_fix_and_explain_lines_for_problems() {
        let mut f = good(ClusterState::Missing);
        f.disk_free_gib = Some(3);
        let t = render_plan(&up::assess(&f));
        assert!(
            t.contains("[blocked] Disk space")
                && t.contains("fluxctl explain LOW_DISK")
                && t.contains("fix:")
        );
    }
}
