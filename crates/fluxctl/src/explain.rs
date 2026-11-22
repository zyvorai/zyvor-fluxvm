// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl explain`: what an API error code means and what to do about it. Offline; the table is
//! `fluxvm_core::error_codes`, which a `fluxvm-api` test keeps in step with the codes the API sends.

use anyhow::Result;
use fluxvm_core::error_codes::{self, ErrorCode, TABLE};
use fluxvm_core::health::{self, HealthCode};
use serde_json::{Value, json};

#[derive(clap::Args, Debug, Clone)]
pub struct ExplainArgs {
    /// An error code from an API error body (`NOT_FOUND`, `rate-limited`), an HTTP status (`429`), or part of a code
    /// (`required`). Omit with `--list` to see them all.
    #[arg(value_name = "CODE")]
    pub code: Option<String>,
    /// List every code with its HTTP status and one-line meaning.
    #[arg(long)]
    pub list: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Prints the answer and returns the exit code: 0 found, 1 unknown, 2 no argument.
pub fn run(args: &ExplainArgs) -> Result<i32> {
    let (out, code) = render(args)?;
    if code == 0 {
        print!("{out}");
    } else {
        eprint!("{out}");
    }
    Ok(code)
}

pub fn render(args: &ExplainArgs) -> Result<(String, i32)> {
    if args.list {
        return Ok((list(args.json)?, 0));
    }
    let Some(query) = args.code.as_deref() else {
        return Ok((
            "give an error code (for example `fluxctl explain NOT_FOUND`) or use --list\n".into(),
            2,
        ));
    };
    let hits = find(query);
    let mac = crate::mac_errors::find(query);
    // MAC_SLOT_LIMIT, SSH_HOSTKEY_MISMATCH, ...: failures of `fluxctl mac`, `mac-fleet` and `kube` on a Mac. A bare status
    // (409, 503) shows the API codes and the Mac ones together.
    if hits.is_empty() && !mac.is_empty() {
        return Ok((mac_out(&mac, args.json)?, 0));
    }
    // The codes `fluxctl cluster status` and `fluxctl fix` print (DAEMON_DOWN, LOW_DISK, ...): not API errors.
    if hits.is_empty() {
        let q = query.trim().to_ascii_uppercase().replace('-', "_");
        let found: Vec<&HealthCode> = match health::code(query) {
            Some(c) => vec![c],
            None => health::CODES
                .iter()
                .filter(|c| c.code.contains(q.as_str()))
                .collect(),
        };
        if !found.is_empty() {
            if args.json {
                let v = if found.len() == 1 {
                    health_entry(found[0])
                } else {
                    Value::Array(found.iter().map(|e| health_entry(e)).collect())
                };
                return Ok((serde_json::to_string_pretty(&v)? + "\n", 0));
            }
            return Ok((
                found
                    .iter()
                    .map(|e| health_card(e))
                    .collect::<Vec<_>>()
                    .join("\n"),
                0,
            ));
        }
    }
    if hits.is_empty() {
        let mut msg = format!("unknown error code {query:?}");
        if let Some(s) = error_codes::suggest(query) {
            msg.push_str(&format!(" (did you mean {s}?)"));
        }
        msg.push_str("\nsee them all with `fluxctl explain --list`\n");
        return Ok((msg, 1));
    }
    if args.json {
        let v = if hits.len() == 1 {
            entry(hits[0])
        } else {
            Value::Array(hits.iter().map(|e| entry(e)).collect())
        };
        return Ok((serde_json::to_string_pretty(&v)? + "\n", 0));
    }
    let mut text: Vec<String> = hits.iter().map(|e| card(e)).collect();
    text.extend(mac.iter().map(|e| crate::mac_errors::card(e)));
    Ok((text.join("\n"), 0))
}

fn mac_out(found: &[&crate::mac_errors::MacCode], json: bool) -> Result<String> {
    if json {
        let v = if found.len() == 1 {
            crate::mac_errors::entry(found[0])
        } else {
            Value::Array(found.iter().map(|e| crate::mac_errors::entry(e)).collect())
        };
        return Ok(serde_json::to_string_pretty(&v)? + "\n");
    }
    Ok(found
        .iter()
        .map(|e| crate::mac_errors::card(e))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// A code name, an HTTP status, or a fragment of a code name.
fn find(query: &str) -> Vec<&'static ErrorCode> {
    if let Some(e) = error_codes::lookup(query) {
        return vec![e];
    }
    if let Ok(status) = query.trim().parse::<u16>() {
        let all: Vec<&ErrorCode> = TABLE.iter().filter(|e| e.status == status).collect();
        if !all.is_empty() {
            return all;
        }
        return error_codes::for_status(status).into_iter().collect();
    }
    error_codes::search(query)
}

fn entry(e: &ErrorCode) -> Value {
    json!({
        "code": e.code,
        "status": e.status,
        "summary": e.summary,
        "causes": e.causes,
        "fixes": e.fixes,
    })
}

fn health_entry(e: &HealthCode) -> Value {
    json!({"code": e.code, "status": null, "summary": e.summary, "causes": e.causes, "fixes": e.fixes})
}

fn health_card(e: &HealthCode) -> String {
    let mut s = format!("{} (cluster health)\n  {}\n\ncauses:\n", e.code, e.summary);
    for c in e.causes {
        s.push_str(&format!("  - {c}\n"));
    }
    s.push_str("\nwhat to do:\n");
    for f in e.fixes {
        s.push_str(&format!("  - {f}\n"));
    }
    s
}

fn card(e: &ErrorCode) -> String {
    let mut s = format!("{} (HTTP {})\n  {}\n", e.code, e.status, e.summary);
    s.push_str("\ncauses:\n");
    for c in e.causes {
        s.push_str(&format!("  - {c}\n"));
    }
    s.push_str("\nwhat to do:\n");
    for f in e.fixes {
        s.push_str(&format!("  - {f}\n"));
    }
    s
}

fn list(json: bool) -> Result<String> {
    let mut rows: Vec<&ErrorCode> = TABLE.iter().collect();
    rows.sort_by(|a, b| (a.status, a.code).cmp(&(b.status, b.code)));
    if json {
        let v = Value::Array(
            rows.iter()
                .map(|e| json!({"code": e.code, "status": e.status, "summary": e.summary}))
                .chain(
                    health::CODES
                        .iter()
                        .map(|e| json!({"code": e.code, "status": null, "summary": e.summary})),
                )
                .collect(),
        );
        return Ok(serde_json::to_string_pretty(&v)? + "\n");
    }
    let mut out = String::new();
    for e in rows {
        out.push_str(&format!("{:<4} {:<22} {}\n", e.status, e.code, e.summary));
    }
    for e in health::CODES {
        out.push_str(&format!("{:<4} {:<22} {}\n", "-", e.code, e.summary));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(code: Option<&str>, list: bool, json: bool) -> ExplainArgs {
        ExplainArgs {
            code: code.map(str::to_string),
            list,
            json,
        }
    }

    #[test]
    fn explains_a_code_in_any_spelling() {
        let (out, code) = render(&args(Some("rate-limited"), false, false)).unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("RATE_LIMITED (HTTP 429)"), "{out}");
        assert!(out.contains("what to do:") && out.contains("Retry-After"));
    }

    #[test]
    fn a_status_finds_every_code_on_it() {
        let (out, code) = render(&args(Some("428"), false, true)).unwrap();
        assert_eq!(code, 0);
        let v: Value = serde_json::from_str(&out).unwrap();
        let mut names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["code"].as_str().unwrap())
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["APPROVAL_REQUIRED", "PRECONDITION_REQUIRED"]);
    }

    #[test]
    fn a_5xx_status_without_its_own_row_is_internal() {
        let (out, code) = render(&args(Some("502"), false, false)).unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("INTERNAL"));
    }

    #[test]
    fn unknown_code_exits_1_with_a_suggestion() {
        let (out, code) = render(&args(Some("NOT_FOND"), false, false)).unwrap();
        assert_eq!(code, 1);
        assert!(out.contains("did you mean NOT_FOUND?"), "{out}");
        let (out, code) = render(&args(Some("zzzz-nothing-like-it"), false, false)).unwrap();
        assert_eq!(code, 1);
        assert!(!out.contains("did you mean"));
    }

    #[test]
    fn no_argument_is_a_usage_error() {
        assert_eq!(render(&args(None, false, false)).unwrap().1, 2);
    }

    #[test]
    fn list_is_sorted_and_json_has_every_code() {
        let (out, _) = render(&args(None, true, true)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), TABLE.len() + health::CODES.len());
        let statuses: Vec<u64> = rows.iter().filter_map(|r| r["status"].as_u64()).collect();
        assert!(statuses.windows(2).all(|w| w[0] <= w[1]));
        let (text, _) = render(&args(None, true, false)).unwrap();
        assert_eq!(text.lines().count(), TABLE.len() + health::CODES.len());
    }

    #[test]
    fn explains_a_cluster_health_code() {
        let (out, code) = render(&args(Some("daemon-down"), false, false)).unwrap();
        assert_eq!(code, 0);
        assert!(out.starts_with("DAEMON_DOWN (cluster health)"), "{out}");
        assert!(out.contains("what to do:") && out.contains("fluxctl fix"));
        let (out, _) = render(&args(Some("LOW_DISK"), false, true)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["code"], "LOW_DISK");
        // a fragment finds the codes that contain it
        let (out, code) = render(&args(Some("backup"), false, false)).unwrap();
        assert_eq!(code, 0);
        assert!(
            out.contains("NO_BACKUP") && out.contains("STALE_BACKUP"),
            "{out}"
        );
    }

    #[test]
    fn every_health_code_the_model_can_print_is_explained() {
        for c in health::CODES {
            let (out, code) = render(&args(Some(c.code), false, false)).unwrap();
            assert_eq!(code, 0, "{}", c.code);
            assert!(out.contains(c.code));
        }
    }
}
