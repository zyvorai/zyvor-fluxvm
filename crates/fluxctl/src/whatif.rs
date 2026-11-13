// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl what-if`: offline policy simulator. Reads one or two `VmNetworkPolicy` JSON files, answers "would this
//! connection be allowed, and which rule decides", and shows which connections change between two policies. Needs no
//! daemon, VM or privileges; see `fluxvm_network::policy_sim` for the decision order and what it does not model.

use anyhow::{Context, Result, bail};
use fluxvm_network::dataplane::VmNetworkPolicy;
use fluxvm_network::policy_sim::{self, Change, Decision, Finding, Probe, Severity, Verdict};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug, Clone)]
pub struct WhatIfArgs {
    /// Network policy JSON (the body of `network set-policy`, or the response of `network effective`).
    pub policy: PathBuf,
    /// A hypothetical connection: `tcp://10.0.0.5:443`, `udp://[2001:db8::1]:53`, `icmp://10.0.0.5`.
    /// Without a scheme the protocol is tcp. Repeatable.
    #[arg(long = "probe", value_name = "URL")]
    pub probes: Vec<String>,
    /// Compare with this policy: print every probe whose verdict (or deciding rule) differs between the two.
    #[arg(long, value_name = "NEW_POLICY")]
    pub against: Option<PathBuf>,
    /// Also test the first and last address of every CIDR in the policy(ies), each with the allowed ports, plus a few
    /// well-known public and private addresses.
    #[arg(long)]
    pub auto: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Loads a policy file: either a bare `VmNetworkPolicy` or an object with the policy under `effective`/`policy`.
/// Unknown keys are reported as findings: serde would silently ignore them and an all-default policy allows everything.
pub fn load_policy(path: &Path) -> Result<(VmNetworkPolicy, Vec<Finding>)> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_policy(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse_policy(raw: &str) -> Result<(VmNetworkPolicy, Vec<Finding>)> {
    let mut v: Value = serde_json::from_str(raw).context("not valid JSON")?;
    for wrapper in ["effective", "policy"] {
        if let Some(inner) = v.get(wrapper).filter(|i| i.is_object()).cloned() {
            v = inner;
            break;
        }
    }
    let Some(obj) = v.as_object() else {
        bail!("expected a JSON object");
    };
    let mut findings = Vec::new();
    let mut known: Vec<String> = match serde_json::to_value(VmNetworkPolicy::default())? {
        Value::Object(m) => m.keys().cloned().collect(),
        _ => Vec::new(),
    };
    known.push("compiled_group_ids".into());
    for k in obj.keys() {
        if !known.contains(k) {
            findings.push(Finding {
                severity: Severity::Warning,
                field: "(file)",
                message: format!(
                    "unknown key {k:?} is ignored; a misspelled field leaves the default (allow all) in place"
                ),
            });
        }
    }
    if obj.is_empty() {
        findings.push(Finding {
            severity: Severity::Warning,
            field: "(file)",
            message: "the policy is empty: that is the default, which allows all egress".into(),
        });
    }
    let policy = serde_json::from_value(v).context("not a network policy")?;
    Ok((policy, findings))
}

fn parse_probes(args: &WhatIfArgs) -> Result<Vec<Probe>> {
    args.probes
        .iter()
        .map(|p| p.parse::<Probe>().map_err(anyhow::Error::msg))
        .collect()
}

/// Runs the command and returns the process exit status: 1 when a policy has an error-level finding.
pub fn run(args: &WhatIfArgs) -> Result<i32> {
    let (out, code) = render(args)?;
    print!("{out}");
    Ok(code)
}

pub fn render(args: &WhatIfArgs) -> Result<(String, i32)> {
    let (policy, mut findings) = load_policy(&args.policy)?;
    findings.extend(policy_sim::lint(&policy));
    let new = args.against.as_deref().map(load_policy).transpose()?;
    let mut probes = parse_probes(args)?;
    if args.auto {
        let mut pols = vec![&policy];
        if let Some((n, _)) = &new {
            pols.push(n);
        }
        probes.extend(policy_sim::derive_probes(&pols));
    }
    if probes.is_empty() && new.is_some() {
        bail!("--against compares probes: pass --probe URL and/or --auto");
    }
    let has_error = |f: &[Finding]| f.iter().any(|f| f.severity == Severity::Error);
    let mut code = i32::from(has_error(&findings));
    let mut new_findings = Vec::new();
    if let Some((n, extra)) = &new {
        new_findings.extend(extra.iter().cloned());
        new_findings.extend(policy_sim::lint(n));
        code |= i32::from(has_error(&new_findings));
    }
    if code != 0 {
        // A malformed policy cannot be simulated; show the findings only.
        return Ok((render_findings(args, &findings, &new_findings)?, code));
    }
    let mut body = Value::Null;
    let mut text = String::new();
    if let Some((n, _)) = &new {
        let changes =
            policy_sim::compare(&policy, n, &probes).map_err(|e| anyhow::anyhow!(e.join("; ")))?;
        text.push_str(&render_changes(&changes, probes.len()));
        body = json!({ "probes": probes.len(), "changes": changes });
    } else if !probes.is_empty() {
        let mut ds = Vec::new();
        for p in &probes {
            ds.push(policy_sim::simulate(&policy, p).map_err(|e| anyhow::anyhow!(e.join("; ")))?);
        }
        text.push_str(&render_decisions(&ds));
        body = json!({ "decisions": ds });
    }
    if args.json {
        let mut v = json!({
            "findings": findings,
            "result": body,
        });
        if new.is_some() {
            v["new_policy_findings"] = json!(new_findings);
        }
        return Ok((serde_json::to_string_pretty(&v)? + "\n", code));
    }
    let mut out = render_findings(args, &findings, &new_findings)?;
    out.push_str(&text);
    if probes.is_empty() {
        out.push_str("(no --probe or --auto given: static checks only)\n");
    }
    Ok((out, code))
}

fn sev(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn render_findings(args: &WhatIfArgs, a: &[Finding], b: &[Finding]) -> Result<String> {
    if args.json {
        return Ok(serde_json::to_string_pretty(
            &json!({"findings": a, "new_policy_findings": b}),
        )? + "\n");
    }
    let mut out = String::new();
    for (label, list) in [("policy", a), ("new policy", b)] {
        for f in list {
            out.push_str(&format!(
                "{label}: {} [{}] {}\n",
                sev(f.severity),
                f.field,
                f.message
            ));
        }
    }
    Ok(out)
}

fn verdict(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "ALLOW",
        Verdict::Drop => "DROP ",
    }
}

fn render_decisions(ds: &[Decision]) -> String {
    let mut out = String::new();
    for d in ds {
        out.push_str(&format!(
            "{} {}  {}\n",
            verdict(d.verdict),
            d.probe,
            d.reason
        ));
        for c in &d.caveats {
            out.push_str(&format!("      note: {c}\n"));
        }
    }
    out
}

fn render_changes(changes: &[Change], total: usize) -> String {
    let flips = changes.iter().filter(|c| c.flipped).count();
    let mut out = format!(
        "{total} probes, {flips} change verdict, {} change only the deciding rule\n",
        changes.len() - flips
    );
    for c in changes {
        let mark = if c.flipped { "CHANGED" } else { "same verdict" };
        out.push_str(&format!(
            "{} -> {}  {}  ({mark})\n    before: {}\n    after:  {}\n",
            verdict(c.before.verdict).trim_end(),
            verdict(c.after.verdict).trim_end(),
            c.probe,
            c.before.reason,
            c.after.reason
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(policy: &Path) -> WhatIfArgs {
        WhatIfArgs {
            policy: policy.to_owned(),
            probes: vec![],
            against: None,
            auto: false,
            json: false,
        }
    }
    fn file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }
    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxctl-whatif-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unknown_keys_and_empty_policy_are_flagged() {
        let (_, f) = parse_policy(r#"{"allow_cidr": ["10.0.0.0/8"]}"#).unwrap();
        assert!(
            f.iter()
                .any(|f| f.message.contains("unknown key \"allow_cidr\""))
        );
        let (_, f) = parse_policy("{}").unwrap();
        assert!(f.iter().any(|f| f.message.contains("empty")));
        assert!(parse_policy("[]").is_err());
        assert!(parse_policy("nope").is_err());
        assert!(parse_policy(r#"{"allow_cidrs": "10.0.0.0/8"}"#).is_err());
    }

    #[test]
    fn effective_response_wrapper_is_unwrapped() {
        let (p, f) = parse_policy(
            r#"{"vm":"x","effective":{"default_allow":false,"compiled_group_ids":[1]}}"#,
        )
        .unwrap();
        assert!(!p.default_allow);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn simulate_text_and_json() {
        let d = tmp("sim");
        let p = file(
            &d,
            "p.json",
            r#"{"allow_cidrs":["10.0.0.0/8"],"allow_ports":["tcp/443"]}"#,
        );
        let mut a = args(&p);
        a.probes = vec![
            "tcp://10.1.1.1:443".into(),
            "tcp://10.1.1.1:22".into(),
            "8.8.8.8:443".into(),
        ];
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("ALLOW tcp://10.1.1.1:443"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("DROP  tcp://10.1.1.1:22") && l.contains("allow_ports"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("DROP  tcp://8.8.8.8:443") && l.contains("allow_cidrs"))
        );
        a.json = true;
        let (out, _) = render(&a).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["result"]["decisions"].as_array().unwrap().len(), 3);
        assert_eq!(v["result"]["decisions"][0]["verdict"], "allow");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn malformed_policy_exits_1_and_lists_errors() {
        let d = tmp("bad");
        let p = file(&d, "p.json", r#"{"allow_cidrs":["10.0.0.0/40"]}"#);
        let mut a = args(&p);
        a.probes = vec!["tcp://1.1.1.1:1".into()];
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("error [allow_cidrs]"));
        assert!(!out.contains("ALLOW"));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn compare_shows_what_a_tightening_breaks() {
        let d = tmp("cmp");
        let old = file(&d, "old.json", r#"{"allow_cidrs":["10.0.0.0/8"]}"#);
        let new = file(&d, "new.json", r#"{"allow_cidrs":["10.0.0.0/16"]}"#);
        let mut a = args(&old);
        a.against = Some(new);
        a.probes = vec!["tcp://10.0.1.1:80".into(), "tcp://10.5.1.1:80".into()];
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("2 probes, 1 change verdict"), "{out}");
        assert!(
            out.contains("ALLOW -> DROP  tcp://10.5.1.1:80  (CHANGED)"),
            "{out}"
        );
        a.probes.clear();
        a.auto = true;
        let (out, _) = render(&a).unwrap();
        assert!(out.contains("CHANGED"), "{out}");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn compare_needs_probes_and_bad_probe_is_a_usage_error() {
        let d = tmp("use");
        let p = file(&d, "p.json", "{\"default_allow\":false}");
        let mut a = args(&p);
        a.against = Some(p.clone());
        assert!(render(&a).is_err());
        let mut b = args(&p);
        b.probes = vec!["tcp://nonsense".into()];
        assert!(render(&b).is_err());
        let (out, code) = render(&args(&p)).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("static checks only"));
        std::fs::remove_dir_all(d).ok();
    }
}
