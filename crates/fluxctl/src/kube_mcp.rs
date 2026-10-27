//! The cluster as MCP tools, so an AI agent (Claude Code, Cursor, Codex) can look at and change a `fluxctl kube` cluster
//! with a safety net: `kube_apply` saves a checkpoint before it changes anything, then reports what changed and how to put
//! it back with `kube_undo`. Read tools are always listed; write tools only when the server allows writes.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

use crate::kube_checkpoint;
use crate::kube_diff;
use crate::kube_doctor;
use crate::kube_host::{self, KubeOptions};
use crate::kube_ps;
use crate::mcp::{CALL_TIMEOUT, Tool, object, pretty, str_arg, timed, tool};
use crate::remote::Remote;

/// A restore waits for the cluster to come back.
const SLOW: Duration = Duration::from_secs(180);
/// The largest manifest `kube_apply` accepts.
pub const MAX_MANIFEST: usize = 256 * 1024;
/// How many `try-` checkpoints `kube_apply` keeps.
const KEEP_TRIES: usize = 5;

/// The cluster named by the `cluster` argument (default `fluxvm-kube`).
pub fn options(args: &Value) -> Result<KubeOptions> {
    let mut o = KubeOptions::default();
    if let Some(name) = str_arg(args, "cluster") {
        if !kube_host::name_ok(name) {
            bail!("cluster {name:?} is not a valid name (lowercase letters, digits and dashes)");
        }
        o.name = name.to_owned();
    }
    Ok(o)
}

/// Rejects an empty or oversized manifest before anything is saved or applied.
pub fn check_manifest(manifest: &str) -> Result<()> {
    if manifest.trim().is_empty() {
        bail!("manifest is empty");
    }
    if manifest.len() > MAX_MANIFEST {
        bail!(
            "manifest is {} bytes; the limit is {MAX_MANIFEST}",
            manifest.len()
        );
    }
    Ok(())
}

/// What `kube_apply` tells the agent.
pub fn apply_report(label: &str, applied: &str, changes: &str) -> String {
    format!(
        "{}\n\nChanged since the checkpoint {label}:\n{changes}\n\nTo put the cluster back as it was, call kube_undo (it restores {label}).",
        applied.trim_end()
    )
}

/// Saves a `try-` checkpoint, applies the manifest, and reports the difference. The checkpoint is kept when the apply
/// fails, so the cluster can still be put back.
pub async fn safe_apply<A: crate::run::VmApi>(
    api: &A,
    o: &KubeOptions,
    manifest: &str,
) -> Result<String> {
    check_manifest(manifest)?;
    let label = kube_checkpoint::try_label(chrono::Utc::now());
    kube_checkpoint::checkpoint(api, o, &label).await?;
    let applied = match kube_host::guest_apply(api, o, manifest).await {
        Ok(out) => out,
        Err(e) => bail!(
            "kubectl apply failed ({e:#}). The cluster was checkpointed first as {label}; kube_undo restores it"
        ),
    };
    // Keep the last few safety checkpoints, as `kube try` does.
    if let Ok(listed) = kube_checkpoint::list(api, o).await {
        let labels: Vec<String> = listed["checkpoints"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c["label"].as_str().map(str::to_owned))
            .collect();
        for old in kube_checkpoint::stale_tries(&labels, KEEP_TRIES) {
            let _ = kube_checkpoint::remove(api, o, &old).await;
        }
    }
    let changes = kube_diff::run(api, o, Some(&label), Some("now"))
        .await
        .unwrap_or_else(|e| format!("(could not compare: {e:#})"));
    Ok(apply_report(&label, &applied, &changes))
}

fn cluster_prop() -> Value {
    json!({"type": "string", "description": "cluster name (default fluxvm-kube)"})
}

pub fn tools(remote: Arc<Remote>) -> Vec<Tool> {
    let label_prop = json!({"type": "string", "description": "checkpoint label: 1 to 40 lowercase letters, digits or dashes"});
    vec![
        tool(
            "kube_status",
            "One screen of a fluxctl kube cluster: pods by namespace with restarts and failures, services and their Mac ports.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let o = options(&args)?;
                    Ok(kube_ps::render(&kube_ps::gather(&*r, &o).await?))
                })
                .await
            },
        ),
        tool(
            "kube_doctor",
            "Check a fluxctl kube cluster for the usual Mac problems (disk, VPN, DNS, clock, kubeconfig, crash-looping or unpullable pods) and print the fix next to each.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let o = options(&args)?;
                    Ok(kube_doctor::render(&kube_doctor::run(&*r, &o).await?))
                })
                .await
            },
        ),
        tool(
            "kube_checkpoints",
            "List the cluster's checkpoints, newest first: label, time, size.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let o = options(&args)?;
                    pretty(&kube_checkpoint::list(&*r, &o).await?)
                })
                .await
            },
        ),
        tool(
            "kube_diff",
            "What changed in the cluster between two checkpoints, or since one: objects added, removed or changed (images, replicas, spec). a defaults to the newest checkpoint, b to \"now\".",
            object(
                json!({"cluster": cluster_prop(), "a": label_prop.clone(), "b": {"type": "string", "description": "a checkpoint label or \"now\""}}),
                &[],
            ),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let o = options(&args)?;
                    kube_diff::run(&*r, &o, str_arg(&args, "a"), str_arg(&args, "b")).await
                })
                .await
            },
        ),
        tool(
            "kube_timeline",
            "Every checkpoint, newest first, with what changed since the one before it.",
            object(json!({"cluster": cluster_prop()}), &[]),
            false,
            &remote,
            |r, args| async move {
                timed(CALL_TIMEOUT, async {
                    let o = options(&args)?;
                    kube_diff::timeline(&*r, &o).await
                })
                .await
            },
        ),
        tool(
            "kube_apply",
            "Apply a Kubernetes manifest (YAML) to the cluster with a safety net: the cluster is checkpointed first, then the manifest is applied, then what changed is reported. kube_undo puts the cluster back as it was.",
            object(
                json!({"cluster": cluster_prop(), "manifest": {"type": "string", "description": "the YAML to apply, one or more documents"}}),
                &["manifest"],
            ),
            true,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let o = options(&args)?;
                    safe_apply(&*r, &o, str_arg(&args, "manifest").unwrap_or_default()).await
                })
                .await
            },
        ),
        tool(
            "kube_checkpoint",
            "Save the running cluster, memory included, as a checkpoint (about 1 s).",
            object(
                json!({"cluster": cluster_prop(), "label": label_prop.clone()}),
                &["label"],
            ),
            true,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let o = options(&args)?;
                    kube_checkpoint::checkpoint(&*r, &o, str_arg(&args, "label").unwrap_or_default()).await
                })
                .await
            },
        ),
        tool(
            "kube_restore",
            "Put the cluster back as a checkpoint saved it (8 to 13 s), discarding everything since.",
            object(
                json!({"cluster": cluster_prop(), "label": label_prop}),
                &["label"],
            ),
            true,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let o = options(&args)?;
                    kube_checkpoint::restore(&*r, &o, str_arg(&args, "label").unwrap_or_default()).await
                })
                .await
            },
        ),
        tool(
            "kube_undo",
            "Put the cluster back as it was before the last kube_apply (restores the newest try- checkpoint).",
            object(json!({"cluster": cluster_prop()}), &[]),
            true,
            &remote,
            |r, args| async move {
                timed(SLOW, async {
                    let o = options(&args)?;
                    kube_checkpoint::undo(&*r, &o).await
                })
                .await
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Tool> {
        tools(Arc::new(Remote::new("127.0.0.1:1", None)))
    }

    #[test]
    fn reads_are_listed_without_write_access_and_changes_need_it() {
        let t = all();
        let write: Vec<&str> = t.iter().filter(|t| t.write).map(|t| t.name).collect();
        let read: Vec<&str> = t.iter().filter(|t| !t.write).map(|t| t.name).collect();
        assert_eq!(
            write,
            ["kube_apply", "kube_checkpoint", "kube_restore", "kube_undo"]
        );
        assert_eq!(
            read,
            [
                "kube_status",
                "kube_doctor",
                "kube_checkpoints",
                "kube_diff",
                "kube_timeline"
            ]
        );
    }

    #[test]
    fn every_tool_has_a_unique_name_a_description_and_an_object_schema() {
        let t = all();
        let mut names: Vec<&str> = t.iter().map(|t| t.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), t.len());
        for tool in &t {
            assert!(tool.name.starts_with("kube_"));
            assert!(tool.description.len() > 20, "{}", tool.name);
            assert_eq!(tool.schema["type"], "object", "{}", tool.name);
            assert!(tool.schema["properties"]["cluster"].is_object(), "{}", tool.name);
        }
        let apply = t.iter().find(|t| t.name == "kube_apply").unwrap();
        assert_eq!(apply.schema["required"], json!(["manifest"]));
    }

    #[test]
    fn the_cluster_defaults_and_a_bad_name_is_refused() {
        assert_eq!(options(&json!({})).unwrap().name, KubeOptions::default().name);
        assert_eq!(options(&json!({"cluster": "dev"})).unwrap().name, "dev");
        assert!(options(&json!({"cluster": "Dev; rm -rf"})).is_err());
        assert!(options(&json!({"cluster": ""})).is_err());
    }

    #[test]
    fn empty_and_oversized_manifests_are_refused_before_anything_is_saved() {
        assert!(check_manifest("kind: Namespace\n").is_ok());
        assert!(check_manifest("").is_err());
        assert!(check_manifest("  \n\t").is_err());
        let big = "a".repeat(MAX_MANIFEST + 1);
        assert!(check_manifest(&big).unwrap_err().to_string().contains("limit"));
    }

    #[test]
    fn the_apply_report_names_the_checkpoint_and_the_way_back() {
        let r = apply_report(
            "try-20261027-100000000",
            "deployment.apps/web configured\n",
            "~ Deployment/shop/web: replicas 2 -> 5\n(+0 -0 ~1)",
        );
        assert!(r.starts_with("deployment.apps/web configured\n\nChanged since the checkpoint try-20261027-100000000:"));
        assert!(r.contains("replicas 2 -> 5"));
        assert!(r.ends_with("call kube_undo (it restores try-20261027-100000000)."));
    }

    #[tokio::test]
    async fn a_write_tool_is_refused_when_the_server_is_read_only() {
        // The server filters on `write`, so the safety net is only reachable with --allow-write.
        let mut s = crate::mcp::Server::new("t", "0", false);
        s.add(all());
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let (mut out, mut buf) = (Vec::new(), String::new());
        s.serve(input.as_bytes(), &mut out).await.unwrap();
        buf.push_str(&String::from_utf8(out).unwrap());
        assert!(buf.contains("kube_status") && buf.contains("kube_diff"), "{buf}");
        assert!(!buf.contains("kube_apply") && !buf.contains("kube_undo"), "{buf}");
    }
}
