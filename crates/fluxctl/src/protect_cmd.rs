// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl protect NAME...`, `fluxctl unprotect NAME...` and `fluxctl protect --list`: deletion protection, like
//! `limactl protect`. A protected VM carries the label `fluxvm.io/protected=true`; the daemon refuses to delete it
//! (HTTP 409, `fluxctl explain VM_PROTECTED`) until it is unprotected. `fluxctl prune` skips protected VMs and `fluxctl
//! df` does not count them as reclaimable. Only deletion is blocked; stop, snapshot and edit still work.

use crate::cli_errors::coded;
use crate::remote::{Remote, encode_query};
use anyhow::Result;
use fluxvm_core::protect::{LABEL, is_protected};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(clap::Args, Debug, Clone)]
pub struct ProtectArgs {
    /// VMs to protect (name, id or id prefix). With none and no `-l`, use `--list`.
    #[arg(value_parser = crate::output::parse_vm_ref)]
    pub ids: Vec<Uuid>,
    /// Protect every VM matching this label selector (for example `env=prod`).
    #[arg(short = 'l', long)]
    pub label: Option<String>,
    /// Show the protected VMs instead of protecting any.
    #[arg(long, conflicts_with_all = ["ids", "label"])]
    pub list: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct UnprotectArgs {
    /// VMs to unprotect (name, id or id prefix).
    #[arg(value_parser = crate::output::parse_vm_ref)]
    pub ids: Vec<Uuid>,
    /// Unprotect every VM matching this label selector.
    #[arg(short = 'l', long)]
    pub label: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

fn unavailable(e: anyhow::Error) -> anyhow::Error {
    coded(
        format!("could not reach the FluxVM daemon: {e:#}"),
        "DAEMON_REQUIRED",
    )
}

async fn list_vms(r: &Remote, label: Option<&str>) -> Result<Vec<Value>> {
    let path = match label {
        Some(l) => format!("/v1/vms?label={}", encode_query(l)),
        None => "/v1/vms".to_string(),
    };
    let v = r
        .call(Method::GET, &path, None)
        .await
        .map_err(unavailable)?;
    Ok(v["items"].as_array().cloned().unwrap_or_default())
}

fn labels_of(v: &Value) -> BTreeMap<String, String> {
    serde_json::from_value(v["labels"].clone()).unwrap_or_default()
}

/// `(id, name, status)` of the VMs carrying the protection label.
pub fn protected_rows(items: &[Value]) -> Vec<(String, String, String)> {
    let mut rows: Vec<_> = items
        .iter()
        .filter(|v| is_protected(&labels_of(v)))
        .map(|v| {
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            (s("id"), s("name"), s("status"))
        })
        .collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    rows
}

pub fn render_list(rows: &[(String, String, String)]) -> String {
    if rows.is_empty() {
        return "No VM is protected. Protect one with `fluxctl protect NAME`.\n".into();
    }
    let mut out = format!("{:<24} {:<10} {}\n", "NAME", "STATUS", "ID");
    for (id, name, status) in rows {
        out.push_str(&format!("{name:<24} {status:<10} {id}\n"));
    }
    out
}

/// Sets or clears the label on every target. Returns one result per VM; `ok: false` rows carry the daemon's reason.
pub async fn apply(
    r: &Remote,
    ids: &[Uuid],
    label: Option<&str>,
    protect: bool,
) -> Result<Vec<Value>> {
    let mut targets: Vec<(String, String)> =
        ids.iter().map(|i| (i.to_string(), String::new())).collect();
    if let Some(sel) = label {
        for v in list_vms(r, Some(sel)).await? {
            let id = v["id"].as_str().unwrap_or("").to_string();
            if !targets.iter().any(|t| t.0 == id) {
                targets.push((id, v["name"].as_str().unwrap_or("").to_string()));
            }
        }
        if targets.is_empty() {
            return Err(coded(
                format!("no VM matches the selector {sel:?}"),
                "PROTECT_NO_TARGET",
            ));
        }
    }
    if targets.is_empty() {
        return Err(coded(
            "name at least one VM, or use -l SELECTOR".to_string(),
            "PROTECT_NO_TARGET",
        ));
    }
    let body = if protect {
        json!({"labels": {LABEL: "true"}})
    } else {
        json!({"labels": {LABEL: null}})
    };
    let mut out = Vec::new();
    for (id, name) in targets {
        match r
            .call(Method::PATCH, &format!("/v1/vms/{id}"), Some(body.clone()))
            .await
        {
            Ok(v) => out.push(json!({
                "id": id,
                "name": v["name"].as_str().unwrap_or(&name),
                "ok": true,
                "protected": is_protected(&labels_of(&v)),
            })),
            Err(e) => {
                out.push(json!({"id": id, "name": name, "ok": false, "error": format!("{e:#}")}))
            }
        }
    }
    Ok(out)
}

pub fn render_apply(results: &[Value], protect: bool) -> String {
    let mut s = String::new();
    for v in results {
        let name = v["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or(v["id"].as_str().unwrap_or("?"));
        if v["ok"] == true {
            s.push_str(&format!(
                "{} {name}\n",
                if protect { "protected" } else { "unprotected" }
            ));
        } else {
            s.push_str(&format!(
                "failed {name}: {}\n",
                v["error"].as_str().unwrap_or("")
            ));
        }
    }
    s
}

fn finish(results: Vec<Value>, protect: bool, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        print!("{}", render_apply(&results, protect));
    }
    let failed = results.iter().filter(|v| v["ok"] != true).count();
    if failed > 0 {
        anyhow::bail!(
            "{failed} VM(s) could not be {}",
            if protect { "protected" } else { "unprotected" }
        );
    }
    Ok(())
}

pub async fn run_protect(r: &Remote, a: &ProtectArgs) -> Result<()> {
    if a.list {
        let rows = protected_rows(&list_vms(r, None).await?);
        if a.json {
            let v: Vec<Value> = rows
                .iter()
                .map(|(id, name, status)| json!({"id": id, "name": name, "status": status}))
                .collect();
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            print!("{}", render_list(&rows));
        }
        return Ok(());
    }
    finish(
        apply(r, &a.ids, a.label.as_deref(), true).await?,
        true,
        a.json,
    )
}

pub async fn run_unprotect(r: &Remote, a: &UnprotectArgs) -> Result<()> {
    finish(
        apply(r, &a.ids, a.label.as_deref(), false).await?,
        false,
        a.json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fake, spawn, vm};
    use std::sync::Arc;

    fn fake() -> Arc<Fake> {
        let f = Arc::new(Fake::default());
        *f.vms.lock().unwrap() = vec![
            vm(1, "web", "running", 1, &[("env", "prod")]),
            vm(2, "db", "stopped", 1, &[("env", "prod")]),
            vm(3, "scratch", "stopped", 1, &[("env", "dev")]),
        ];
        f
    }

    #[tokio::test]
    async fn protect_blocks_delete_until_unprotect_and_only_that_vm() {
        let f = fake();
        let r = spawn(f.clone()).await;
        let db = Uuid::from_u128(2);
        let res = apply(&r, &[db], None, true).await.unwrap();
        assert_eq!(res[0]["ok"], true);
        assert_eq!(res[0]["protected"], true);
        // The daemon (here: the fake using the real guard) now refuses the delete with a 409 and the fix.
        let e = r
            .call(Method::DELETE, &format!("/v1/vms/{db}"), None)
            .await
            .unwrap_err();
        let m = format!("{e:#}");
        assert!(
            m.contains("409") && m.contains("fluxctl unprotect db"),
            "{m}"
        );
        // A neighbour is unaffected.
        r.call(
            Method::DELETE,
            &format!("/v1/vms/{}", Uuid::from_u128(3)),
            None,
        )
        .await
        .unwrap();
        // Unprotect, and the delete goes through.
        let res = apply(&r, &[db], None, false).await.unwrap();
        assert_eq!(res[0]["protected"], false);
        r.call(Method::DELETE, &format!("/v1/vms/{db}"), None)
            .await
            .unwrap();
        assert_eq!(f.vms.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_selector_protects_every_match_and_nothing_else() {
        let f = fake();
        let r = spawn(f.clone()).await;
        let res = apply(&r, &[], Some("env=prod"), true).await.unwrap();
        assert_eq!(res.len(), 2);
        let rows = protected_rows(&list_vms(&r, None).await.unwrap());
        let names: Vec<_> = rows.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(names, ["db", "web"]);
        let none = apply(&r, &[], Some("env=staging"), true).await.unwrap_err();
        assert!(format!("{none:#}").contains("no VM matches"), "{none:#}");
        let none = apply(&r, &[], None, true).await.unwrap_err();
        assert!(format!("{none:#}").contains("fluxctl explain PROTECT_NO_TARGET"));
    }

    #[tokio::test]
    async fn a_missing_vm_is_reported_per_vm_and_fails_the_command() {
        let r = spawn(fake()).await;
        let res = apply(&r, &[Uuid::from_u128(99), Uuid::from_u128(1)], None, true)
            .await
            .unwrap();
        assert_eq!(res[0]["ok"], false);
        assert_eq!(res[1]["ok"], true);
        assert!(render_apply(&res, true).contains("protected web"));
        assert!(finish(res, true, true).is_err());
    }

    #[test]
    fn the_list_is_plain_and_the_empty_case_says_what_to_do() {
        assert!(render_list(&[]).contains("fluxctl protect NAME"));
        let s = render_list(&[("i".into(), "db".into(), "stopped".into())]);
        assert!(s.starts_with("NAME") && s.contains("db") && s.contains("stopped"));
    }

    #[test]
    fn arguments_parse_and_list_excludes_targets() {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(flatten)]
            a: ProtectArgs,
        }
        let p = P::try_parse_from(["x", "--list", "--json"]).unwrap();
        assert!(p.a.list && p.a.json);
        let id = Uuid::from_u128(5).to_string();
        assert!(P::try_parse_from(["x", "--list", "-l", "a=b"]).is_err());
        assert!(P::try_parse_from(["x", &id, "-l", "a=b"]).is_ok());
    }
}
