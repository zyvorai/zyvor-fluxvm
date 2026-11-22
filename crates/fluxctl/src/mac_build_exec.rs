// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Step executor for `fluxctl mac build` (docs/macos-containers-design.md, "Build").
//!
//! The order of steps and the cleanup on failure live here, behind the small [`BuildHost`] trait, so they are unit tested with
//! a fake host. The real host (daemon clone, SSH, scp, store) is `DaemonHost` in `mac_cmd.rs`.

use crate::mac_build::{Step, render_dry_run};
use anyhow::{Result, bail};

/// SSH account used when the Macfile has no `USER` line (the same default as `mac run`).
pub const DEFAULT_USER: &str = "admin";

/// What a build needs from the outside world. One instance is created, used and deleted per build.
pub trait BuildHost {
    /// Create the throwaway instance from the template `reference` and start it. On error nothing is left behind.
    async fn create(&mut self, reference: &str) -> Result<()>;
    /// Block until the guest answers on SSH.
    async fn wait_ready(&mut self) -> Result<()>;
    async fn run(&mut self, user: &str, command: &str) -> Result<()>;
    async fn copy(&mut self, user: &str, src: &str, dst: &str) -> Result<()>;
    /// Stop the instance and store its disk, hardware model and auxiliary storage as the new template.
    async fn stop_and_store(&mut self) -> Result<()>;
    /// Delete the instance. Called exactly once after a successful `create`, on success and on failure alike.
    async fn delete(&mut self) -> Result<()>;
}

/// Reject, before anything is created, the steps a build cannot run.
pub fn check_supported(steps: &[Step]) -> Result<()> {
    for (i, s) in steps.iter().enumerate() {
        match s {
            Step::Snapshot { line, .. } => bail!(
                "step {} (line {line}): SNAPSHOT is not supported by `mac build`: a saved running state cannot be moved to another Mac. Remove it from the Macfile",
                i + 1
            ),
            Step::InstallIpswLatest | Step::InstallIpswPath { .. } => bail!(
                "step {}: `FROM IPSW ...` is not supported by `mac build` yet: start from a template (`fluxctl mac import` or `mac pull`). `fluxctl mac ipsw` prints the unattended install plan and what it needs",
                i + 1
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Run `steps` in order. Any failure deletes the instance (if one was created) and names the failing step, numbered as in
/// `--dry-run`. A failure to delete after a successful build is a warning: the image is already stored.
pub async fn execute<H: BuildHost>(host: &mut H, steps: &[Step]) -> Result<()> {
    check_supported(steps)?;
    let mut created = false;
    let mut outcome = Ok(());
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        let what = render_dry_run(std::slice::from_ref(step));
        let what = what
            .split_once(". ")
            .map_or(what.as_str(), |(_, t)| t)
            .trim_end();
        eprintln!("step {n}/{}: {what}", steps.len());
        let r = match step {
            Step::CloneTemplate { reference } => match host.create(reference).await {
                Ok(()) => {
                    created = true;
                    host.wait_ready().await
                }
                Err(e) => Err(e),
            },
            Step::Run { user, command, .. } => {
                host.run(user.as_deref().unwrap_or(DEFAULT_USER), command)
                    .await
            }
            Step::Copy { user, src, dst, .. } => {
                host.copy(user.as_deref().unwrap_or(DEFAULT_USER), src, dst)
                    .await
            }
            Step::StoreTemplate => host.stop_and_store().await,
            Step::InstallIpswLatest | Step::InstallIpswPath { .. } | Step::Snapshot { .. } => unreachable!("checked above"),
        };
        if let Err(e) = r {
            outcome = Err(e.context(format!("build failed at step {n} ({what})")));
            break;
        }
    }
    if created && let Err(e) = host.delete().await {
        match outcome {
            Ok(()) => eprintln!("warning: the build instance was not deleted: {e:#}"),
            Err(_) => eprintln!("warning: the build instance was not deleted either: {e:#}"),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_build::{parse, plan};

    /// Records every call; fails on the call whose label equals `fail_on`.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        fail_on: Option<&'static str>,
    }

    impl Fake {
        fn note(&mut self, label: &str, detail: String) -> Result<()> {
            self.calls
                .push(format!("{label} {detail}").trim().to_string());
            if self.fail_on == Some(label) {
                bail!("{label} exploded");
            }
            Ok(())
        }
    }

    impl BuildHost for Fake {
        async fn create(&mut self, r: &str) -> Result<()> {
            self.note("create", r.into())
        }
        async fn wait_ready(&mut self) -> Result<()> {
            self.note("wait", String::new())
        }
        async fn run(&mut self, u: &str, c: &str) -> Result<()> {
            self.note("run", format!("{u}: {c}"))
        }
        async fn copy(&mut self, u: &str, s: &str, d: &str) -> Result<()> {
            self.note("copy", format!("{u}: {s} -> {d}"))
        }
        async fn stop_and_store(&mut self) -> Result<()> {
            self.note("store", String::new())
        }
        async fn delete(&mut self) -> Result<()> {
            self.note("delete", String::new())
        }
    }

    fn steps(src: &str) -> Vec<Step> {
        plan(&parse(src).unwrap())
    }

    const FILE: &str = "FROM base:15\nUSER builder\nRUN one\nCOPY ./tools /opt/tools\nRUN two\n";

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn steps_run_in_order_then_store_then_delete() {
        let mut h = Fake::default();
        block_on(execute(&mut h, &steps(FILE))).unwrap();
        assert_eq!(
            h.calls,
            [
                "create base:15",
                "wait",
                "run builder: one",
                "copy builder: ./tools -> /opt/tools",
                "run builder: two",
                "store",
                "delete"
            ]
        );
    }

    #[test]
    fn default_user_applies_without_a_user_line() {
        let mut h = Fake::default();
        block_on(execute(&mut h, &steps("FROM base\nRUN x\n"))).unwrap();
        assert!(
            h.calls.contains(&"run admin: x".to_string()),
            "{:?}",
            h.calls
        );
    }

    #[test]
    fn a_failing_run_deletes_the_instance_names_the_step_and_skips_the_rest() {
        let mut h = Fake {
            fail_on: Some("run"),
            ..Default::default()
        };
        let e = block_on(execute(&mut h, &steps(FILE))).unwrap_err();
        let t = format!("{e:#}");
        assert!(t.contains("step 2") && t.contains("run exploded"), "{t}");
        assert_eq!(
            h.calls,
            ["create base:15", "wait", "run builder: one", "delete"]
        );
    }

    #[test]
    fn a_failing_store_deletes_the_instance() {
        let mut h = Fake {
            fail_on: Some("store"),
            ..Default::default()
        };
        let e = block_on(execute(&mut h, &steps(FILE))).unwrap_err();
        assert!(format!("{e:#}").contains("step 5"), "{e:#}");
        assert_eq!(h.calls.last().unwrap(), "delete");
    }

    #[test]
    fn a_boot_timeout_deletes_the_instance_but_a_failed_create_does_not() {
        let mut h = Fake {
            fail_on: Some("wait"),
            ..Default::default()
        };
        let e = block_on(execute(&mut h, &steps(FILE))).unwrap_err();
        assert!(format!("{e:#}").contains("step 1"));
        assert_eq!(h.calls, ["create base:15", "wait", "delete"]);

        let mut h = Fake {
            fail_on: Some("create"),
            ..Default::default()
        };
        assert!(block_on(execute(&mut h, &steps(FILE))).is_err());
        assert_eq!(h.calls, ["create base:15"]);
    }

    #[test]
    fn a_failed_delete_after_success_is_only_a_warning() {
        let mut h = Fake {
            fail_on: Some("delete"),
            ..Default::default()
        };
        block_on(execute(&mut h, &steps(FILE))).unwrap();
    }

    #[test]
    fn snapshot_and_ipsw_are_refused_before_anything_is_created() {
        let mut h = Fake::default();
        let e = block_on(execute(&mut h, &steps("FROM base\nRUN x\nSNAPSHOT warm\n")))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("SNAPSHOT is not supported") && e.contains("line 3"),
            "{e}"
        );
        let e = block_on(execute(&mut h, &steps("FROM IPSW latest\nRUN x\n")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not supported"), "{e}");
        assert!(h.calls.is_empty());
    }
}
