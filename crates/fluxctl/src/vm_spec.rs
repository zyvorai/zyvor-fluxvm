// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl vm-spec check`: lint a VM spec (the JSON of `fluxctl create --spec` and `POST /v1/vms`) without a daemon.
//! See `fluxvm_core::spec_lint` for the rules. Read-only; needs no daemon, VM or privileges.

use anyhow::{Context, Result};
use fluxvm_core::config_lint::{Finding, Severity};
use fluxvm_core::spec_lint::{SpecEnv, lint_str};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug, Clone)]
pub enum VmSpecCommand {
    /// Check a VM spec for unknown keys, values the daemon refuses, settings the chosen backend ignores and missing
    /// paths, with the key and the fix for each. Reads the file only (never starts a VM); exits 1 on an error (and on
    /// a warning with `--strict`).
    Check(CheckArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct CheckArgs {
    /// VM spec JSON file.
    pub file: PathBuf,
    /// Skip the checks that look at this machine (does the image, kernel or a shared folder exist), for a spec written
    /// for another host.
    #[arg(long)]
    pub offline: bool,
    /// Exit 1 on warnings too.
    #[arg(long)]
    pub strict: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(cmd: &VmSpecCommand) -> Result<i32> {
    let VmSpecCommand::Check(args) = cmd;
    let (out, code) = render(args)?;
    print!("{out}");
    Ok(code)
}

pub fn render(args: &CheckArgs) -> Result<(String, i32)> {
    let raw = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let host = |p: &Path| p.exists();
    let env = if args.offline {
        SpecEnv::OFFLINE
    } else {
        SpecEnv {
            exists: Some(&host),
        }
    };
    let findings = lint_str(&raw, &env);
    let count = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    let (errors, warnings, notes) = (
        count(Severity::Error),
        count(Severity::Warning),
        count(Severity::Info),
    );
    let code = i32::from(errors > 0 || (args.strict && warnings > 0));
    if args.json {
        let v = json!({
            "file": args.file.display().to_string(),
            "errors": errors,
            "warnings": warnings,
            "notes": notes,
            "findings": findings,
        });
        return Ok((serde_json::to_string_pretty(&v)? + "\n", code));
    }
    let mut out = String::new();
    for f in &findings {
        out.push_str(&render_finding(f));
    }
    out.push_str(&format!(
        "{}: {errors} error{}, {warnings} warning{}, {notes} note{}\n",
        args.file.display(),
        plural(errors),
        plural(warnings),
        plural(notes)
    ));
    Ok((out, code))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn render_finding(f: &Finding) -> String {
    let mut s = format!("{:<8}{}  {}\n", f.severity.label(), f.key, f.message);
    if let Some(fix) = &f.fix {
        s.push_str(&format!("        fix: {fix}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(tag: &str, body: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("fluxctl-vmspec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("spec.json");
        std::fs::write(&p, body).unwrap();
        (d, p)
    }
    fn args(file: PathBuf, strict: bool, json: bool) -> CheckArgs {
        CheckArgs {
            file,
            offline: true,
            strict,
            json,
        }
    }
    const GOOD: &str = r#"{"name":"t","backend":"qemu","image":"/a/b.qcow2"}"#;

    #[test]
    fn clean_spec_exits_0() {
        let (d, p) = write("clean", GOOD);
        let (out, code) = render(&args(p, true, false)).unwrap();
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("0 errors, 0 warnings"), "{out}");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn error_exits_1_with_key_and_fix() {
        let (d, p) = write(
            "err",
            r#"{"name":"t","backend":"qemu","image":"/a","vcpus":4,"max_vcpus":2}"#,
        );
        let (out, code) = render(&args(p, false, false)).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("error   max_vcpus"), "{out}");
        assert!(out.contains("fix: set max_vcpus to 4 or more"), "{out}");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn strict_makes_warnings_fail() {
        let (d, p) = write(
            "warn",
            r#"{"name":"t","backend":"qemu","image":"/a","memory_mb":1024}"#,
        );
        assert_eq!(render(&args(p.clone(), false, false)).unwrap().1, 0);
        assert_eq!(render(&args(p, true, false)).unwrap().1, 1);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn json_output_counts_and_lists_findings() {
        let (d, p) = write(
            "json",
            r#"{"name":"t","backend":"qemu","image":"/a","bogus":1}"#,
        );
        let (out, code) = render(&args(p, false, true)).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["warnings"], 1);
        assert_eq!(v["findings"][0]["key"], "bogus");
        assert_eq!(v["findings"][0]["severity"], "warning");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn missing_file_is_an_error_not_a_finding() {
        let e = render(&args(PathBuf::from("/nonexistent/spec.json"), false, false)).unwrap_err();
        assert!(format!("{e:#}").contains("reading /nonexistent/spec.json"));
    }

    #[test]
    fn online_run_reports_a_missing_image() {
        let (d, p) = write("host", GOOD);
        let mut a = args(p, false, false);
        a.offline = false;
        let (out, code) = render(&a).unwrap();
        assert_eq!(code, 0);
        assert!(out.contains("warning image"), "{out}");
        std::fs::remove_dir_all(d).ok();
    }
}
