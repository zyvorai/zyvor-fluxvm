// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl exec-policy check`: preflight of a per-exec confinement policy (the JSON of `fluxctl exec --policy`).
//! Shows what a guest with a given Landlock ABI would enforce or refuse, and the mistakes that stop the command
//! from starting. Pure and offline; see `fluxvm_guest_protocol::exec_preflight`.

use anyhow::{Context, Result};
use fluxvm_guest_protocol::ExecPolicy;
use fluxvm_guest_protocol::exec_preflight::{
    Finding, MAX_KNOWN_ABI, Preflight, Severity, lint, preflight,
};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ExecPolicyCommand {
    /// Check an exec policy JSON file: lint it (shell unreachable, writable system paths, unknown fields, fail-open
    /// settings) and show, per Landlock ABI, what the guest would enforce or refuse. Reads the file only; exits 1 on
    /// an error, or when `--abi` is given and a strict guest at that ABI would refuse to run it.
    Check(CheckArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct CheckArgs {
    /// Policy JSON (the same shape `fluxctl exec --policy` and `fluxvm-procbox` take).
    pub policy: PathBuf,
    /// The guest's Landlock ABI (see `fluxvm-procbox probe`, or `uname -r`: 5.13 is ABI 1, 5.19 is 2, 6.2 is 3,
    /// 6.7 is 4, 6.10 is 5, 6.12 is 6). Without it the table covers ABI 0 to 7.
    #[arg(long, value_name = "N")]
    pub abi: Option<u32>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cmd: &ExecPolicyCommand) -> Result<i32> {
    let ExecPolicyCommand::Check(args) = cmd;
    let (out, code) = render(args)?;
    print!("{out}");
    Ok(code)
}

pub fn render(args: &CheckArgs) -> Result<(String, i32)> {
    let text = std::fs::read_to_string(&args.policy)
        .with_context(|| format!("reading {}", args.policy.display()))?;
    let raw: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}: not valid JSON", args.policy.display()))?;
    let policy: ExecPolicy = serde_json::from_value(raw.clone())
        .with_context(|| format!("parsing {}: not an exec policy", args.policy.display()))?;
    let findings = lint(&policy, Some(&raw));
    let abis: Vec<u32> = match args.abi {
        Some(a) => vec![a],
        None => (0..=MAX_KNOWN_ABI).collect(),
    };
    let table: Vec<Preflight> = abis.iter().map(|a| preflight(&policy, *a)).collect();
    let errors = findings.iter().any(|f| f.severity == Severity::Error);
    let refused_target = args.abi.is_some() && table.iter().any(|p| p.refused.is_some());
    let code = i32::from(errors || refused_target);
    if args.json {
        let v = json!({ "findings": findings, "abi": table });
        return Ok((serde_json::to_string_pretty(&v)? + "\n", code));
    }
    let mut out = String::new();
    for f in &findings {
        out.push_str(&render_finding(f));
    }
    if findings.is_empty() {
        out.push_str("no findings\n");
    }
    out.push('\n');
    for p in &table {
        out.push_str(&render_abi(p, policy.best_effort));
    }
    Ok((out, code))
}

fn label(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn render_finding(f: &Finding) -> String {
    format!("{:<8}{}  {}\n", label(f.severity), f.field, f.message)
}

fn render_abi(p: &Preflight, best_effort: bool) -> String {
    let head = match (&p.refused, p.blocking.is_empty()) {
        (Some(_), _) => "REFUSED".to_owned(),
        (None, true) => "runs".to_owned(),
        (None, false) => format!(
            "runs with {} gap{} (best_effort)",
            p.blocking.len(),
            if p.blocking.len() == 1 { "" } else { "s" }
        ),
    };
    let mut s = format!("Landlock ABI {}: {head}\n", p.abi);
    if let Some(r) = &p.refused {
        s.push_str(&format!("    strict mode refuses: {r}\n    (set best_effort to run with the gaps, or use a newer guest kernel)\n"));
    } else if best_effort {
        for n in &p.not_enforced {
            s.push_str(&format!("    not enforced: {n}\n"));
        }
    } else {
        for n in p.not_enforced.iter().filter(|n| !p.blocking.contains(n)) {
            s.push_str(&format!("    note: {n}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(tag: &str, body: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxctl-execpol-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("p.json");
        std::fs::write(&p, body).unwrap();
        p
    }
    fn args(p: PathBuf) -> CheckArgs {
        CheckArgs {
            policy: p,
            abi: None,
            json: false,
        }
    }
    const GOOD: &str = r#"{"read":["/bin","/usr","/lib","/lib64"],"write":["/tmp/w"],"tcp_connect":{"ports":[443]},"timeout_secs":30}"#;

    #[test]
    fn table_covers_all_abis_and_shows_refusal_below_6() {
        let (out, code) = render(&args(write("tab", GOOD))).unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("Landlock ABI 0: REFUSED"));
        assert!(out.contains("Landlock ABI 5: REFUSED"));
        assert!(out.contains("Landlock ABI 6: runs\n"), "{out}");
        assert!(out.contains("Landlock ABI 7: runs\n"));
    }

    #[test]
    fn explicit_abi_exit_code_reflects_refusal() {
        let mut a = args(write("abi", GOOD));
        a.abi = Some(4);
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 1);
        assert!(
            out.contains("strict mode refuses") && out.contains("scoping"),
            "{out}"
        );
        a.abi = Some(6);
        assert_eq!(render(&a).unwrap().1, 0);
    }

    #[test]
    fn best_effort_lists_gaps_and_runs() {
        let mut a = args(write("be", r#"{"read":["/usr"],"best_effort":true}"#));
        a.abi = Some(3);
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("runs with 3 gaps (best_effort)"), "{out}");
        assert!(out.contains("not enforced: abstract-unix-socket"), "{out}");
    }

    #[test]
    fn errors_exit_1_and_json_is_structured() {
        let mut a = args(write("err", r#"{"read":["/etc"],"scope_ipc":false}"#));
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("/bin/sh"));
        a.json = true;
        let (out, _) = render(&a).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["abi"].as_array().unwrap().len(), 8);
        assert_eq!(v["findings"][0]["severity"], "error");
    }

    #[test]
    fn bad_files_are_usage_errors() {
        assert!(render(&args(write("bad1", "nope"))).is_err());
        assert!(render(&args(write("bad2", r#"{"tcp_connect":"maybe"}"#))).is_err());
        assert!(render(&args(PathBuf::from("/nonexistent/p.json"))).is_err());
    }
}
