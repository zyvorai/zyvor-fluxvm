// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl gateway demo apply | teardown | print`: the demo fixture from Zyvor Setura 0.1.0 (a namespace, a GatewayClass, a
//! Gateway with one HTTP listener, a small app and an HTTPRoute), applied to or removed from a cluster with kubectl.
//!
//! The plan is data ([`plan`]) and goes through [`KubeRun`], so `--dry-run` prints the commands and the tests use a fake;
//! nothing in this module's tests starts kubectl. `apply` and `teardown` change the cluster, including one cluster-scoped
//! object (the GatewayClass), so without `--dry-run` they need an explicit `--cluster` or `--kube-context` (or `--yes`).

use super::kube::{KubeRun, Selection, shell_join};
use anyhow::{Result, bail};
use std::io::Write;
use std::time::Duration;

/// The fixture, applied as one multi-document YAML on stdin.
pub const MANIFEST: &str = include_str!("demo.yaml");
pub const NAMESPACE: &str = "fluxvm-gateway-demo";
pub const GATEWAY: &str = "dev";
pub const LISTENER: &str = "web";
pub const ROUTE: &str = "orders";

/// Probes that pass against the demo once `gateway publish` is running, for `gateway check`.
pub const CHECKS: &str = r#"[
  {"url": "http://orders.fluxvm.test:8080/checkout", "address": "127.0.0.1", "status": 200, "backend": "orders-v1"},
  {"url": "http://orders.fluxvm.test:8080/unmatched", "address": "127.0.0.1", "status": 404}
]
"#;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Apply,
    Teardown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub argv: Vec<String>,
    /// Fed to kubectl's stdin: the fixture.
    pub stdin: Option<&'static str>,
    pub deadline: Duration,
}

fn step(prefix: &[String], rest: &[&str], stdin: Option<&'static str>, secs: u64) -> Step {
    Step { argv: prefix.iter().cloned().chain(rest.iter().map(|s| (*s).to_owned())).collect(), stdin, deadline: Duration::from_secs(secs) }
}

/// The kubectl calls for an action, in order. `prefix` is `kubectl` plus any `--kubeconfig` and `--context`.
pub fn plan(action: Action, prefix: &[String]) -> Vec<Step> {
    match action {
        Action::Apply => vec![
            step(prefix, &["apply", "-f", "-"], Some(MANIFEST), 60),
            step(prefix, &["-n", NAMESPACE, "rollout", "status", "deployment/orders", "--timeout=180s"], None, 200),
            step(prefix, &["-n", NAMESPACE, "wait", "gateway/dev", "--for=condition=Programmed", "--timeout=180s"], None, 200),
        ],
        // Deletes exactly the objects the fixture names: the namespace (and so everything in it) and the GatewayClass.
        Action::Teardown => vec![step(prefix, &["delete", "-f", "-", "--ignore-not-found", "--timeout=120s"], Some(MANIFEST), 150)],
    }
}

/// What to do next, printed after a successful apply.
pub fn next_steps(selection_flags: &str) -> String {
    format!(
        "Next, in one terminal:\n  fluxctl gateway publish {GATEWAY} -n {NAMESPACE} --listener {LISTENER}{selection_flags}\nand in another:\n  fluxctl gateway explain http://orders.fluxvm.test:8080/checkout --address 127.0.0.1 --backend orders-v1\n  fluxctl gateway demo print --checks > checks.json && fluxctl gateway check checks.json\n  fluxctl gateway inspect {ROUTE} -n {NAMESPACE}{selection_flags}\nRemove it with: fluxctl gateway demo teardown{selection_flags}\n"
    )
}

/// The `--cluster`/`--kube-context` flags to repeat in the printed commands.
pub fn selection_flags(s: &Selection) -> String {
    let mut out = String::new();
    if let Some(c) = &s.cluster {
        out.push_str(&format!(" --cluster {c}"));
    }
    if let Some(c) = &s.kube_context {
        out.push_str(&format!(" --kube-context {}", shell_join(std::slice::from_ref(c))));
    }
    out
}

/// Runs (or, with `dry_run`, prints) the plan. Stops at the first failing step and says which.
pub fn run(
    action: Action,
    sel: &Selection,
    prefix: &[String],
    dry_run: bool,
    yes: bool,
    runner: &dyn KubeRun,
    out: &mut dyn Write,
) -> Result<()> {
    let steps = plan(action, prefix);
    if dry_run {
        for s in &steps {
            let tail = if s.stdin.is_some() { "   # the demo fixture on stdin (see `fluxctl gateway demo print`)" } else { "" };
            writeln!(out, "{}{tail}", shell_join(&s.argv))?;
        }
        return Ok(());
    }
    if !sel.is_explicit() && !yes {
        bail!(
            "this changes whatever kubectl's current context points at (it creates a namespace and a GatewayClass); pass --cluster NAME or --kube-context CONTEXT to say which cluster, or --yes to use the current context. --dry-run shows the commands"
        );
    }
    for (i, s) in steps.iter().enumerate() {
        let r = runner.run(&s.argv, s.stdin.map(str::as_bytes), s.deadline).map_err(|e| anyhow::anyhow!("step {} ({}): {e}", i + 1, shell_join(&s.argv)))?;
        if r.code != 0 {
            let msg = r.stderr.trim();
            bail!("step {} ({}) failed: {}", i + 1, shell_join(&s.argv), if msg.is_empty() { format!("exit {}", r.code) } else { msg.to_owned() });
        }
        if !r.stdout.trim().is_empty() {
            writeln!(out, "{}", r.stdout.trim_end())?;
        }
    }
    if action == Action::Apply {
        write!(out, "{}", next_steps(&selection_flags(sel)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::kube::fake::FakeKubectl;
    use super::*;

    fn prefix() -> Vec<String> {
        vec!["kubectl".into(), "--context".into(), "lab".into()]
    }

    #[test]
    fn fixture_is_well_formed_yaml_with_the_expected_objects() {
        let mut kinds = vec![];
        for d in serde_yaml::Deserializer::from_str(MANIFEST) {
            let v: serde_yaml::Value = serde::Deserialize::deserialize(d).unwrap();
            if v.is_null() {
                continue;
            }
            kinds.push(v["kind"].as_str().unwrap().to_owned());
        }
        assert_eq!(kinds, ["Namespace", "GatewayClass", "EnvoyProxy", "Gateway", "Deployment", "Service", "HTTPRoute"]);
        assert!(MANIFEST.contains("gateway.envoyproxy.io/gatewayclass-controller"));
        assert!(MANIFEST.contains("@sha256:"), "the demo image is pinned by digest");
        assert!(!MANIFEST.contains("setura-demo"));
    }

    #[test]
    fn checks_are_valid_cases() {
        let v: serde_json::Value = serde_json::from_str(CHECKS).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn dry_run_prints_the_commands_and_runs_nothing() {
        let f = FakeKubectl::default();
        let mut out = Vec::new();
        run(Action::Apply, &Selection::default(), &prefix(), true, false, &f, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains("kubectl --context lab apply -f -"));
        assert!(text.contains("wait gateway/dev --for=condition=Programmed"));
        assert!(f.calls.borrow().is_empty());
        let mut out = Vec::new();
        run(Action::Teardown, &Selection::default(), &prefix(), true, false, &f, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("delete -f - --ignore-not-found"));
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn a_real_apply_needs_to_know_which_cluster() {
        let f = FakeKubectl::default();
        let e = run(Action::Apply, &Selection::default(), &["kubectl".into()], false, false, &f, &mut Vec::<u8>::new()).unwrap_err();
        assert!(e.to_string().contains("--cluster"));
        assert!(f.calls.borrow().is_empty());
    }

    #[test]
    fn apply_sends_the_fixture_then_waits_and_prints_next_steps() {
        let f = FakeKubectl::default().text(0, "namespace/x created\n", "").text(0, "deployment ready\n", "").text(0, "gateway condition met\n", "");
        let sel = Selection { kube_context: Some("lab".into()), cluster: None };
        let mut out = Vec::new();
        run(Action::Apply, &sel, &prefix(), false, false, &f, &mut out).unwrap();
        let calls = f.calls.borrow();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1.as_deref(), Some(MANIFEST));
        assert!(calls[1].0.join(" ").contains("rollout status deployment/orders"));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("fluxctl gateway publish dev -n fluxvm-gateway-demo --listener web --kube-context lab"));
    }

    #[test]
    fn a_failing_step_stops_the_rest() {
        let f = FakeKubectl::default().text(1, "", "error: no matches for kind \"Gateway\"\n");
        let e = run(Action::Apply, &Selection { cluster: Some("dev".into()), ..Default::default() }, &prefix(), false, false, &f, &mut Vec::<u8>::new()).unwrap_err();
        assert!(e.to_string().contains("step 1") && e.to_string().contains("no matches for kind"));
        assert_eq!(f.calls.borrow().len(), 1);
    }

    #[test]
    fn teardown_deletes_by_the_fixture_with_yes() {
        let f = FakeKubectl::default().text(0, "namespace deleted\n", "");
        run(Action::Teardown, &Selection::default(), &["kubectl".into()], false, true, &f, &mut Vec::<u8>::new()).unwrap();
        let calls = f.calls.borrow();
        assert_eq!(calls[0].0, ["kubectl", "delete", "-f", "-", "--ignore-not-found", "--timeout=120s"]);
        assert_eq!(calls[0].1.as_deref(), Some(MANIFEST));
    }
}
