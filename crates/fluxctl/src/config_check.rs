// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl config check`: lint a host config file without starting the daemon. See `fluxvm_core::config_lint` for
//! the rules. Read-only; needs no daemon, VM or privileges.

use anyhow::{Context, Result, bail};
use fluxvm_core::config_lint::{Finding, LintEnv, Severity, lint_str};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ConfigCommand {
    /// Check a config file for unknown or misplaced keys, values that cannot work and settings that quietly weaken a
    /// protection, with the key and the fix for each. Reads the file only; exits 1 on an error (and on a warning with
    /// `--strict`).
    Check(CheckArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct CheckArgs {
    /// Config file (default: the global `--config` / `FLUXVM_CONFIG`).
    pub file: Option<PathBuf>,
    /// Skip the checks that look at this machine (does a certificate or kernel path exist), for a config written
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

pub fn run(cmd: &ConfigCommand, global_config: Option<&Path>) -> Result<i32> {
    let ConfigCommand::Check(args) = cmd;
    let (out, code) = render(args, global_config)?;
    print!("{out}");
    Ok(code)
}

pub fn render(args: &CheckArgs, global_config: Option<&Path>) -> Result<(String, i32)> {
    let Some(path) = args.file.as_deref().or(global_config) else {
        bail!("no config file: pass FILE or --config PATH");
    };
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let host = |p: &Path| p.exists();
    let env = if args.offline {
        LintEnv::OFFLINE
    } else {
        LintEnv {
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
            "file": path.display().to_string(),
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
        path.display(),
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

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxctl-cfgcheck-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn args(file: PathBuf) -> CheckArgs {
        CheckArgs {
            file: Some(file),
            offline: true,
            strict: false,
            json: false,
        }
    }

    #[test]
    fn clean_file_exits_0_and_says_so() {
        let d = tmp("clean");
        let p = d.join("c.toml");
        std::fs::write(&p, "listen = \"127.0.0.1:7788\"\n").unwrap();
        let (out, code) = render(&args(p), None).unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            out.lines().last().unwrap().split(": ").nth(1).unwrap(),
            "0 errors, 0 warnings, 0 notes"
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn error_exits_1_and_prints_key_and_fix() {
        let d = tmp("err");
        let p = d.join("c.toml");
        std::fs::write(&p, "[tls]\ncert = \"/c\"\n").unwrap();
        let (out, code) = render(&args(p), None).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("error"), "{out}");
        assert!(out.contains("tls.key"), "{out}");
        assert!(out.contains("        fix: set tls.key"), "{out}");
        assert!(out.contains("1 error, 0 warnings"), "{out}");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn strict_makes_warnings_fail() {
        let d = tmp("strict");
        let p = d.join("c.toml");
        std::fs::write(&p, "state_dir = \"rel\"\n").unwrap();
        let mut a = args(p);
        assert_eq!(render(&a, None).unwrap().1, 0);
        a.strict = true;
        assert_eq!(render(&a, None).unwrap().1, 1);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn json_output_and_global_config_fallback() {
        let d = tmp("json");
        let p = d.join("c.toml");
        std::fs::write(&p, "[tls]\ntrust_proxy_header = true\n").unwrap();
        let mut a = args(p.clone());
        a.file = None;
        a.json = true;
        let (out, code) = render(&a, Some(&p)).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["warnings"], 1);
        assert_eq!(v["findings"][0]["key"], "tls.trust_proxy_header");
        assert_eq!(v["findings"][0]["severity"], "warning");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn missing_file_argument_and_unreadable_file_are_usage_errors() {
        let mut a = args(PathBuf::from("/nonexistent/fluxvm.toml"));
        assert!(render(&a, None).is_err());
        a.file = None;
        assert!(
            render(&a, None)
                .unwrap_err()
                .to_string()
                .contains("no config file")
        );
    }

    #[test]
    fn host_checks_run_unless_offline() {
        let d = tmp("host");
        let p = d.join("c.toml");
        std::fs::write(
            &p,
            "[tls]\ncert = \"/definitely/missing.crt\"\nkey = \"/definitely/missing.key\"\n",
        )
        .unwrap();
        let mut a = args(p);
        assert_eq!(render(&a, None).unwrap().1, 0);
        a.offline = false;
        let (out, code) = render(&a, None).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("does not exist"));
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn shipped_example_configs_have_no_errors_or_unknown_keys() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = vec![root.join("config.example.toml")];
        for e in std::fs::read_dir(root.join("configs")).unwrap() {
            files.push(e.unwrap().path());
        }
        for f in files {
            let raw = std::fs::read_to_string(&f).unwrap();
            let bad: Vec<_> = lint_str(&raw, &LintEnv::OFFLINE)
                .into_iter()
                .filter(|x| x.severity == Severity::Error || x.message.contains("unknown key"))
                .collect();
            assert!(bad.is_empty(), "{}: {bad:#?}", f.display());
        }
    }
}
