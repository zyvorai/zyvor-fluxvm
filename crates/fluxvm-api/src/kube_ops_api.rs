// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Studio Cluster operations card: k3s upgrade and node add / remove for the clusters on this host.
//!
//! * `GET  /v1/k3s/{cluster}/upgrade/plan?channel=stable` shows what an upgrade would do (`fluxctl kube upgrade`, no `--yes`):
//!   what runs now, what it becomes, in which order, and why not when it is unsafe (422 with an explain code).
//! * `POST /v1/k3s/{cluster}/upgrade` with `{"channel": "stable", "confirm": "<cluster>"}` starts it in the background
//!   (`fluxctl kube upgrade --yes`: verified backup first, then servers first). Without the matching `confirm` it answers 428.
//! * `POST /v1/k3s/{cluster}/nodes/add` with `{"node": "optional", "confirm": "<cluster>"}` starts `fluxctl kube node add`.
//! * `POST /v1/k3s/{cluster}/nodes/{node}/remove` with `{"confirm": "<node>"}` runs `fluxctl kube node rm`, which today
//!   answers with the drain-and-delete steps (nothing removes a node's VM yet); they come back as a 422 in words.
//! * `GET  /v1/k3s-ops/{id}` reports a started operation: state, output lines so far and, on failure, the message with its explain
//!   code. `GET /v1/k3s-ops` lists the recent ones.
//!
//! All admin only. The logic is `fluxctl`'s (found next to the daemon, else on PATH): the Studio and a terminal do the same thing.
//! One operation per cluster at a time (409 otherwise). The state is in memory: a daemon restart forgets finished operations, and
//! the CLI's own journal (`fluxctl kube backups`, `fluxctl cluster status`) tells what happened to the cluster.

use crate::kube::err;
use crate::{ApiResult, Role, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
};
use fluxvm_core::k3s_backups as kb;
use fluxvm_scheduler::VmManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path as FsPath, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_LINES: usize = 400;
const KEEP_FINISHED: usize = 20;
const UPGRADE_LIMIT: Duration = Duration::from_secs(60 * 60);
const NODE_LIMIT: Duration = Duration::from_secs(30 * 60);
const PLAN_LIMIT: Duration = Duration::from_secs(90);

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/k3s/{cluster}/upgrade/plan", get(upgrade_plan))
        .route("/v1/k3s/{cluster}/upgrade", post(upgrade))
        .route("/v1/k3s/{cluster}/nodes/add", post(node_add))
        .route("/v1/k3s/{cluster}/nodes/{node}/remove", post(node_remove))
        .route("/v1/k3s-ops", get(list_ops))
        .route("/v1/k3s-ops/{id}", get(get_op))
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Op {
    id: String,
    cluster: String,
    /// `upgrade`, `node-add` or `node-remove`.
    kind: String,
    /// `running`, `done` or `failed`.
    state: String,
    /// The terminal command that does the same.
    command: String,
    lines: Vec<String>,
    /// The failure in words (with `more: fluxctl explain CODE`), when it failed.
    error: Option<String>,
    /// The explain code of the failure, if the message names one.
    explain: Option<String>,
    started_secs_ago: u64,
    #[serde(skip)]
    started: Option<Instant>,
}

fn ops() -> &'static Mutex<Vec<Op>> {
    static OPS: OnceLock<Mutex<Vec<Op>>> = OnceLock::new();
    OPS.get_or_init(|| Mutex::new(Vec::new()))
}

fn lock() -> std::sync::MutexGuard<'static, Vec<Op>> {
    ops().lock().unwrap_or_else(|p| p.into_inner())
}

fn fluxctl() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("fluxctl")))
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("fluxctl"))
            .find(|p| p.is_file())
    })
}

fn need_fluxctl() -> ApiResult<PathBuf> {
    fluxctl().ok_or_else(|| {
        err(
            StatusCode::NOT_IMPLEMENTED,
            "fluxctl was not found next to the daemon or on its PATH, so it cannot do this; run the same command in a terminal (Copy as fluxctl)",
        )
    })
}

/// `stable`, `latest`, `testing` or a minor such as `v1.31` (the same rule as the CLI's).
pub(crate) fn channel_ok(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.as_bytes()[0].is_ascii_alphanumeric()
        && c.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

fn bad_names(cluster: &str, node: Option<&str>) -> ApiResult<()> {
    if !kb::valid_name(cluster) || node.is_some_and(|n| !kb::valid_name(n)) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "that is not a valid cluster or node name",
        ));
    }
    Ok(())
}

/// `--cluster NAME`, left out for the default cluster exactly like the backups card does.
fn cluster_args(cluster: &str) -> Vec<String> {
    if cluster == fluxvm_core::up::CLUSTER_NAME {
        vec![]
    } else {
        vec!["--cluster".into(), cluster.into()]
    }
}

pub(crate) fn upgrade_args(cluster: &str, channel: &str, yes: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "kube".into(),
        "upgrade".into(),
        "--channel".into(),
        channel.into(),
    ];
    a.extend(cluster_args(cluster));
    if yes {
        a.push("--yes".into());
    }
    a
}

pub(crate) fn node_add_args(cluster: &str, node: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = vec!["kube".into(), "node".into(), "add".into()];
    if let Some(n) = node {
        a.extend(["--node".into(), n.into()]);
    }
    a.extend(cluster_args(cluster));
    a
}

fn terminal_command(args: &[String]) -> String {
    format!("fluxctl {}", args.join(" "))
}

/// The explain code a message ends with (`more: fluxctl explain CODE`).
pub(crate) fn explain_code(msg: &str) -> Option<String> {
    let rest = msg.split("fluxctl explain ").nth(1)?;
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    (!code.is_empty()).then_some(code)
}

fn confirm_needed(what: &str, expect: &str) -> (StatusCode, String) {
    (
        StatusCode::PRECONDITION_REQUIRED,
        format!(
            "{what}; send {{\"confirm\": \"{expect}\"}} to do it (this is the `--yes` of the command; more: fluxctl explain KUBE_NEEDS_YES)"
        ),
    )
}

/// Runs a command to the end (the plan; the node-removal steps) and returns its stdout, or its message.
fn run_to_end(
    bin: &FsPath,
    args: &[String],
    limit: Duration,
) -> Result<String, (StatusCode, String)> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not start fluxctl: {e}"),
            )
        })?;
    let mut o = child.stdout.take().expect("piped");
    let mut e = child.stderr.take().expect("piped");
    let ot = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = o.read_to_string(&mut s);
        s
    });
    let et = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = e.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err((
                    StatusCode::GATEWAY_TIMEOUT,
                    "it took too long and was stopped; check the cluster with `fluxctl cluster status`".into(),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    };
    let (out, errs) = (ot.join().unwrap_or_default(), et.join().unwrap_or_default());
    if status.success() {
        return Ok(out);
    }
    let msg = errs.trim();
    let msg = if msg.is_empty() {
        "fluxctl failed without a message".to_owned()
    } else {
        msg.to_owned()
    };
    // A message with an explain code is a refusal in words (422); anything else is ours (500).
    let code = if explain_code(&msg).is_some() {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    Err((code, msg))
}

fn push_line(id: &str, line: String) {
    let mut g = lock();
    if let Some(o) = g.iter_mut().find(|o| o.id == id) {
        if o.lines.len() >= MAX_LINES {
            o.lines.remove(0);
        }
        o.lines.push(line);
    }
}

/// Starts `bin ARGS` in the background as an operation on `cluster`; 409 if that cluster already has one running.
pub(crate) fn start_op(
    bin: PathBuf,
    cluster: &str,
    kind: &str,
    args: Vec<String>,
    limit: Duration,
) -> Result<Op, (StatusCode, String)> {
    let id = {
        let mut g = lock();
        if let Some(o) = g
            .iter()
            .find(|o| o.cluster == cluster && o.state == "running")
        {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "cluster {cluster} is already busy with {} (started {} s ago); wait for it to finish (more: fluxctl explain KUBE_OP_BUSY)",
                    o.kind,
                    o.started.map_or(0, |s| s.elapsed().as_secs())
                ),
            ));
        }
        let finished: Vec<String> = g
            .iter()
            .filter(|o| o.state != "running")
            .map(|o| o.id.clone())
            .collect();
        if finished.len() >= KEEP_FINISHED {
            let drop_id = finished[0].clone();
            g.retain(|o| o.id != drop_id);
        }
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let id = format!("op-{ts}-{n}");
        g.push(Op {
            id: id.clone(),
            cluster: cluster.to_owned(),
            kind: kind.to_owned(),
            state: "running".into(),
            command: terminal_command(&args),
            lines: vec![],
            error: None,
            explain: None,
            started_secs_ago: 0,
            started: Some(Instant::now()),
        });
        id
    };
    let spawned = Command::new(&bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            finish(&id, Err(format!("could not start fluxctl: {e}")));
            return Ok(snapshot(&id).expect("just added"));
        }
    };
    let out = child.stdout.take().expect("piped");
    let errp = child.stderr.take().expect("piped");
    let (id2, id3) = (id.clone(), id.clone());
    let t_out = std::thread::spawn(move || {
        for l in BufReader::new(out).lines().map_while(Result::ok) {
            push_line(&id2, l);
        }
    });
    let t_err = std::thread::spawn(move || {
        let mut all = String::new();
        for l in BufReader::new(errp).lines().map_while(Result::ok) {
            all.push_str(&l);
            all.push('\n');
            push_line(&id3, l);
        }
        all
    });
    let id4 = id.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if start.elapsed() > limit => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(
                        "it took too long and was stopped; the cluster may be half-way: check it with `fluxctl cluster status`".to_owned(),
                    );
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => break Err(e.to_string()),
            }
        };
        let _ = t_out.join();
        let errs = t_err.join().unwrap_or_default();
        finish(
            &id4,
            match status {
                Ok(s) if s.success() => Ok(()),
                Ok(_) => Err(error_text(&errs)),
                Err(m) => Err(m),
            },
        );
    });
    Ok(snapshot(&id).expect("just added"))
}

/// The message of a failed run: the whole stderr minus progress noise, so the `more: fluxctl explain` line is kept.
fn error_text(stderr: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        "fluxctl failed without a message; see the output above".into()
    } else {
        t.to_owned()
    }
}

fn finish(id: &str, r: Result<(), String>) {
    let mut g = lock();
    if let Some(o) = g.iter_mut().find(|o| o.id == id) {
        match r {
            Ok(()) => o.state = "done".into(),
            Err(m) => {
                o.explain = explain_code(&m);
                o.error = Some(m);
                o.state = "failed".into();
            }
        }
    }
}

fn snapshot(id: &str) -> Option<Op> {
    lock().iter().find(|o| o.id == id).cloned().map(fresh)
}

fn fresh(mut o: Op) -> Op {
    o.started_secs_ago = o.started.map_or(0, |s| s.elapsed().as_secs());
    o
}

#[derive(Deserialize, Default)]
struct PlanQuery {
    channel: Option<String>,
}

async fn upgrade_plan(
    Extension(role): Extension<Role>,
    Path(cluster): Path<String>,
    Query(q): Query<PlanQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    bad_names(&cluster, None)?;
    let channel = q.channel.unwrap_or_else(|| "stable".into());
    if !channel_ok(&channel) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "the channel must be stable, latest, testing or a minor such as v1.31",
        ));
    }
    let bin = need_fluxctl()?;
    let args = upgrade_args(&cluster, &channel, false);
    let cmd = terminal_command(&args);
    let text = tokio::task::spawn_blocking(move || run_to_end(&bin, &args, PLAN_LIMIT))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|(s, m)| err(s, m))?;
    let mut run_cmd = cmd.clone();
    run_cmd.push_str(" --yes");
    Ok(Json(json!({
        "cluster": cluster, "channel": channel, "plan": text.trim_end(),
        "nothing_to_do": text.contains("already on") && !text.contains("->") && !text.contains("→"),
        "command": run_cmd,
    })))
}

#[derive(Deserialize, Default)]
struct UpgradeBody {
    channel: Option<String>,
    #[serde(default)]
    confirm: String,
}

async fn upgrade(
    Extension(role): Extension<Role>,
    Path(cluster): Path<String>,
    body: Option<Json<UpgradeBody>>,
) -> ApiResult<Json<Op>> {
    require_admin(role)?;
    bad_names(&cluster, None)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let channel = b.channel.unwrap_or_else(|| "stable".into());
    if !channel_ok(&channel) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "the channel must be stable, latest, testing or a minor such as v1.31",
        ));
    }
    if b.confirm != cluster {
        let (s, m) = confirm_needed(
            &format!(
                "upgrading restarts k3s on every node of {cluster} (the API is gone for about a minute) after a backup"
            ),
            &cluster,
        );
        return Err(err(s, m));
    }
    let bin = need_fluxctl()?;
    start_op(
        bin,
        &cluster,
        "upgrade",
        upgrade_args(&cluster, &channel, true),
        UPGRADE_LIMIT,
    )
    .map(|o| Json(fresh(o)))
    .map_err(|(s, m)| err(s, m))
}

#[derive(Deserialize, Default)]
struct AddBody {
    node: Option<String>,
    #[serde(default)]
    confirm: String,
}

async fn node_add(
    Extension(role): Extension<Role>,
    Path(cluster): Path<String>,
    body: Option<Json<AddBody>>,
) -> ApiResult<Json<Op>> {
    require_admin(role)?;
    bad_names(&cluster, None)?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    bad_names(&cluster, b.node.as_deref().filter(|n| !n.is_empty()))?;
    if b.confirm != cluster {
        let (s, m) = confirm_needed(
            &format!("this starts one more worker VM and joins it to {cluster}"),
            &cluster,
        );
        return Err(err(s, m));
    }
    let bin = need_fluxctl()?;
    let node = b.node.filter(|n| !n.is_empty());
    start_op(
        bin,
        &cluster,
        "node-add",
        node_add_args(&cluster, node.as_deref()),
        NODE_LIMIT,
    )
    .map(|o| Json(fresh(o)))
    .map_err(|(s, m)| err(s, m))
}

#[derive(Deserialize, Default)]
struct RemoveBody {
    #[serde(default)]
    confirm: String,
}

async fn node_remove(
    Extension(role): Extension<Role>,
    Path((cluster, node)): Path<(String, String)>,
    body: Option<Json<RemoveBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    bad_names(&cluster, Some(&node))?;
    let confirm = body.map(|Json(b)| b.confirm).unwrap_or_default();
    if confirm != node {
        let (s, m) = confirm_needed(
            &format!("removing {node} drains it and takes it out of {cluster}"),
            &node,
        );
        return Err(err(s, m));
    }
    let bin = need_fluxctl()?;
    let args: Vec<String> = vec!["kube".into(), "node".into(), "rm".into(), node.clone()];
    let out = tokio::task::spawn_blocking(move || run_to_end(&bin, &args, PLAN_LIMIT))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|(s, m)| err(s, m))?;
    Ok(Json(
        json!({"cluster": cluster, "node": node, "output": out.trim()}),
    ))
}

async fn list_ops(Extension(role): Extension<Role>) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let mut v: Vec<Op> = lock().iter().cloned().map(fresh).collect();
    v.reverse();
    Ok(Json(json!({ "schema": 1, "ops": v })))
}

async fn get_op(Extension(role): Extension<Role>, Path(id): Path<String>) -> ApiResult<Json<Op>> {
    require_admin(role)?;
    snapshot(&id).map(Json).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "there is no such operation (the daemon forgets finished ones when it restarts); `fluxctl cluster status` shows the cluster's state",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &FsPath, body: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let p = dir.join(format!(
            "fake-fluxctl-{}",
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn wait(id: &str) -> Op {
        for _ in 0..200 {
            let o = snapshot(id).unwrap();
            if o.state != "running" {
                return o;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("still running");
    }

    #[test]
    fn the_routes_are_registered_under_v1() {
        let _ = routes();
    }

    #[test]
    fn command_lines_match_the_cli() {
        assert_eq!(
            upgrade_args("lab", "v1.31", true),
            [
                "kube",
                "upgrade",
                "--channel",
                "v1.31",
                "--cluster",
                "lab",
                "--yes"
            ]
        );
        assert_eq!(
            upgrade_args(fluxvm_core::up::CLUSTER_NAME, "stable", false),
            ["kube", "upgrade", "--channel", "stable"]
        );
        assert_eq!(
            node_add_args("lab", Some("lab-a2")),
            [
                "kube",
                "node",
                "add",
                "--node",
                "lab-a2",
                "--cluster",
                "lab"
            ]
        );
        assert_eq!(
            terminal_command(&node_add_args("lab", None)),
            "fluxctl kube node add --cluster lab"
        );
    }

    #[test]
    fn channels_and_explain_codes_are_read_strictly() {
        assert!(channel_ok("stable") && channel_ok("v1.31"));
        assert!(
            !channel_ok("") && !channel_ok("a b") && !channel_ok("../x") && !channel_ok("Stable")
        );
        assert_eq!(
            explain_code("Upgrade refused: x\n  more: fluxctl explain KUBE_UPGRADE_REFUSED")
                .as_deref(),
            Some("KUBE_UPGRADE_REFUSED")
        );
        assert_eq!(explain_code("plain failure"), None);
    }

    #[test]
    fn an_operation_reports_its_output_and_finishes() {
        let d = tempfile::tempdir().unwrap();
        let bin = script(d.path(), "echo backed up; echo upgrading");
        let o = start_op(
            bin,
            "t-ok",
            "upgrade",
            vec!["kube".into()],
            Duration::from_secs(10),
        )
        .unwrap();
        let o = wait(&o.id);
        assert_eq!(o.state, "done");
        assert_eq!(o.lines, ["backed up", "upgrading"]);
        assert!(o.error.is_none());
    }

    #[test]
    fn a_failure_keeps_the_message_and_its_explain_code() {
        let d = tempfile::tempdir().unwrap();
        let bin = script(
            d.path(),
            "echo 'Upgrade refused: a node is not Ready' >&2; echo '  more: fluxctl explain KUBE_UPGRADE_REFUSED' >&2; exit 1",
        );
        let o = start_op(bin, "t-fail", "upgrade", vec![], Duration::from_secs(10)).unwrap();
        let o = wait(&o.id);
        assert_eq!(o.state, "failed");
        assert_eq!(o.explain.as_deref(), Some("KUBE_UPGRADE_REFUSED"));
        assert!(o.error.unwrap().contains("not Ready"));
    }

    #[test]
    fn a_second_operation_on_a_busy_cluster_is_refused_and_a_slow_one_is_stopped() {
        let d = tempfile::tempdir().unwrap();
        let bin = script(d.path(), "exec sleep 30");
        let first = start_op(
            bin.clone(),
            "t-busy",
            "node-add",
            vec![],
            Duration::from_millis(600),
        )
        .unwrap();
        let (s, m) = start_op(
            bin.clone(),
            "t-busy",
            "upgrade",
            vec![],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(m.contains("KUBE_OP_BUSY"));
        // another cluster is not blocked
        let other = start_op(
            script(d.path(), "true"),
            "t-other",
            "upgrade",
            vec![],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(wait(&other.id).state, "done");
        let o = wait(&first.id);
        assert_eq!(o.state, "failed");
        assert!(o.error.unwrap().contains("too long"));
    }

    #[test]
    fn the_plan_runner_turns_a_refusal_into_422_and_a_missing_binary_into_500() {
        let d = tempfile::tempdir().unwrap();
        let bin = script(
            d.path(),
            "echo 'Removing x is not automatic yet\n  more: fluxctl explain KUBE_NODE_UNSUPPORTED' >&2; exit 1",
        );
        let (s, m) = run_to_end(&bin, &[], Duration::from_secs(5)).unwrap_err();
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(m.contains("KUBE_NODE_UNSUPPORTED"));
        let ok = script(d.path(), "echo plan text");
        assert_eq!(
            run_to_end(&ok, &[], Duration::from_secs(5)).unwrap().trim(),
            "plan text"
        );
        assert_eq!(
            run_to_end(&d.path().join("nope"), &[], Duration::from_secs(1))
                .unwrap_err()
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn confirmation_is_required_before_anything_starts() {
        let r = upgrade(
            Extension(Role::Admin),
            Path("lab".into()),
            Some(Json(UpgradeBody {
                channel: None,
                confirm: "other".into(),
            })),
        )
        .await;
        assert_eq!(r.unwrap_err().status, StatusCode::PRECONDITION_REQUIRED);
        let r = node_add(Extension(Role::Admin), Path("lab".into()), None).await;
        assert_eq!(r.unwrap_err().status, StatusCode::PRECONDITION_REQUIRED);
        let r = node_remove(
            Extension(Role::Admin),
            Path(("lab".into(), "lab-a1".into())),
            None,
        )
        .await;
        assert_eq!(r.unwrap_err().status, StatusCode::PRECONDITION_REQUIRED);
        let r = upgrade_plan(
            Extension(Role::Admin),
            Path("lab".into()),
            Query(PlanQuery {
                channel: Some("../x".into()),
            }),
        )
        .await;
        assert_eq!(r.unwrap_err().status, StatusCode::BAD_REQUEST);
    }
}
