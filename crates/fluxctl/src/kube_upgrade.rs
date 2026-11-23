// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl kube upgrade` and `fluxctl kube node add|rm`.
//!
//! `upgrade` shows a plan first (what runs now, what it becomes, in which order) and refuses what is unsafe: a downgrade, a jump over
//! a Kubernetes minor version, a node that is not Ready. With `--yes` it takes a verified backup (`kube_backups::take_exclusive`,
//! kind pre-upgrade) and only then starts the rolling upgrade that `kube cluster upgrade --channel` does (servers first). The rollback
//! note says how to go back with that backup. `node add` joins one more node through `kube cluster join`; `node rm` explains the
//! manual steps, because nothing in the cluster lifecycle removes a node's VM and record yet.

use crate::gateway::Target;
use crate::gateway::kube::{Cluster, KubeRun, SystemKubectl};
use crate::kube_app_cmd::{coded, kc, kerr, selection};
use crate::kube_apple::AppleCommand;
use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use fluxvm_core::k3s_backups::Kind;
use serde_json::Value;

const CHANNELS_URL: &str = "https://update.k3s.io/v1-release/channels";

#[derive(Subcommand, Debug)]
pub enum LifecycleCommand {
    /// Upgrade k3s: shows the plan (what runs now, what it becomes, in which order) and, with `--yes`, takes a verified backup and
    /// then upgrades, servers first. One Kubernetes minor version at a time; never a downgrade.
    Upgrade {
        /// The k3s release channel to follow: `stable` (default), `latest`, `testing` or a minor such as `v1.31`.
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Do it; without this only the plan is shown.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Add or remove nodes.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum NodeCommand {
    /// Start one more worker VM and join it to the cluster (a cluster made on this machine with `fluxctl kube cluster`).
    Add {
        /// The node's name (default: CLUSTER-a1, CLUSTER-a2, ...).
        #[arg(long)]
        node: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Remove a node (explains the steps; not automatic yet).
    Rm { node: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    pub name: String,
    pub version: String,
    pub server: bool,
    pub ready: bool,
}

/// The nodes of `kubectl get nodes -o json`, servers first, then by name.
pub fn parse_nodes(v: &Value) -> Vec<NodeInfo> {
    let mut nodes: Vec<NodeInfo> = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let ready = n["status"]["conditions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| c["type"] == "Ready" && c["status"] == "True");
            Some(NodeInfo {
                name: n["metadata"]["name"].as_str()?.to_owned(),
                version: n["status"]["nodeInfo"]["kubeletVersion"]
                    .as_str()?
                    .to_owned(),
                server: n["metadata"]["labels"].as_object().is_some_and(|l| {
                    l.contains_key("node-role.kubernetes.io/control-plane")
                        || l.contains_key("node-role.kubernetes.io/master")
                }),
                ready,
            })
        })
        .collect();
    nodes.sort_by(|a, b| (!a.server, &a.name).cmp(&(!b.server, &b.name)));
    nodes
}

/// The release a channel points at, from `https://update.k3s.io/v1-release/channels`.
pub fn channel_version(json: &str, channel: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    v["data"].as_array()?.iter().find(|c| c["id"] == channel)?["latest"]
        .as_str()
        .filter(|s| crate::kube_apple::model::parse_version(s).is_some())
        .map(str::to_owned)
}

/// `stable`, `latest`, `testing` or a minor channel such as `v1.31`.
fn channel_ok(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.as_bytes()[0].is_ascii_alphanumeric()
        && c.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

fn refused(msg: impl std::fmt::Display) -> anyhow::Error {
    coded("KUBE_UPGRADE_REFUSED", format!("Upgrade refused: {msg}"))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub target: String,
    /// Nodes that change, in order.
    pub steps: Vec<NodeInfo>,
    /// Nodes already on the target.
    pub done: Vec<String>,
}

/// Works out the upgrade of `nodes` to `target`, or why not. Never downgrades, never skips a Kubernetes minor version, and never starts
/// on a cluster with a node that is not Ready.
pub fn plan(nodes: &[NodeInfo], target: &str) -> Result<Plan> {
    use crate::kube_apple::model::parse_version;
    let Some(want) = parse_version(target) else {
        return Err(refused(format!(
            "{target:?} is not a k3s version such as v1.31.4+k3s1"
        )));
    };
    let Some(server) = nodes.iter().find(|n| n.server) else {
        return Err(refused("the cluster reports no server node"));
    };
    let sick: Vec<&str> = nodes
        .iter()
        .filter(|n| !n.ready)
        .map(|n| n.name.as_str())
        .collect();
    if !sick.is_empty() {
        return Err(refused(format!(
            "not every node is Ready ({}). Fix that first: `fluxctl cluster status`",
            sick.join(", ")
        )));
    }
    let Some(have) = parse_version(&server.version) else {
        return Err(refused(format!(
            "the server reports version {:?}, which is not a k3s version",
            server.version
        )));
    };
    if want < have {
        return Err(refused(format!(
            "{target} is older than the {} that runs now, and k3s does not support downgrades",
            server.version
        )));
    }
    if want.0 != have.0 || want.1 > have.1 + 1 {
        return Err(refused(format!(
            "{} to {target} skips a Kubernetes minor version. Go one at a time: `fluxctl kube upgrade --channel v{}.{}`, then this",
            server.version,
            have.0,
            have.1 + 1
        )));
    }
    let (done, steps): (Vec<&NodeInfo>, Vec<&NodeInfo>) =
        nodes.iter().partition(|n| n.version == target);
    Ok(Plan {
        target: target.to_owned(),
        steps: steps.into_iter().cloned().collect(),
        done: done.into_iter().map(|n| n.name.clone()).collect(),
    })
}

pub fn rollback_note() -> &'static str {
    "rollback: k3s cannot step back down by itself. The backup taken first holds the cluster's data; to go back, `fluxctl kube backups` lists it and `fluxctl kube restore BACKUP --yes` puts the cluster back as it was (on the older k3s that the restore reinstalls over, restore the nodes' binary too: `fluxctl kube cluster upgrade NAME --binary ... --allow-downgrade`)\n"
}

pub fn render_plan(p: &Plan, cluster: &str, channel: &str) -> String {
    let mut out = format!("upgrade {cluster} to {} (channel {channel})\n", p.target);
    if p.steps.is_empty() {
        out.push_str("  every node already runs it; nothing to do\n");
        return out;
    }
    out.push_str("  0. a verified backup of the cluster is taken first\n");
    for (i, s) in p.steps.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {} ({}): {} -> {}\n",
            i + 1,
            s.name,
            if s.server { "server" } else { "worker" },
            s.version,
            p.target
        ));
    }
    if !p.done.is_empty() {
        out.push_str(&format!("  already on it: {}\n", p.done.join(", ")));
    }
    out.push_str("\nthe server restarts k3s (the API is gone for about a minute; running pods keep running), then each worker in turn\n");
    out.push_str(rollback_note());
    out
}

fn read_nodes(c: &Cluster) -> Result<Vec<NodeInfo>> {
    let r = kc(c, None, &["get", "nodes", "-o", "json"], None, 30)?;
    if r.code != 0 {
        return Err(kerr("reading the nodes", &r));
    }
    let v: Value = serde_json::from_str(&r.stdout)
        .map_err(|e| anyhow!("kubectl answered something that is not JSON: {e}"))?;
    Ok(parse_nodes(&v))
}

async fn fetch_channels() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    Ok(client
        .get(CHANNELS_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// A `kube cluster ...` command line as the command it names.
fn apple(args: &[&str]) -> Result<AppleCommand> {
    #[derive(Parser)]
    struct W {
        #[command(subcommand)]
        c: AppleCommand,
    }
    let mut argv = vec!["fluxctl"];
    argv.extend(args);
    W::try_parse_from(argv)
        .map(|w| w.c)
        .map_err(|e| anyhow!("{e}"))
}

pub async fn run<A: crate::run::VmApi>(api: &A, cmd: LifecycleCommand) -> Result<()> {
    match cmd {
        LifecycleCommand::Upgrade {
            channel,
            yes,
            target,
        } => upgrade(api, &channel, yes, &target).await,
        LifecycleCommand::Node {
            command: NodeCommand::Add { node, target },
        } => {
            let (sel, _) = selection(&target)?;
            if sel.kube_context.is_some() {
                return Err(coded(
                    "KUBE_NODE_UNSUPPORTED",
                    "Nodes are added to clusters made by `fluxctl kube cluster` on this machine, not to one reached with --kube-context",
                ));
            }
            let store =
                crate::kube_apple::model::Store::new(crate::kube_apple::model::default_root()?);
            let name = crate::kube_backups::pick_cluster(&store, sel.cluster.as_deref())?;
            let mut args = vec!["join", name.as_str()];
            if let Some(n) = node.as_deref() {
                args.extend(["--node", n]);
            }
            crate::kube_apple::run(api, apple(&args)?).await
        }
        LifecycleCommand::Node {
            command: NodeCommand::Rm { node },
        } => Err(rm_refusal(&node)),
    }
}

pub fn rm_refusal(node: &str) -> anyhow::Error {
    coded(
        "KUBE_NODE_UNSUPPORTED",
        format!(
            "Removing {node} is not automatic yet: nothing in the cluster lifecycle deletes a node's VM and its record. By hand: `kubectl drain {node} --ignore-daemonsets --delete-emptydir-data`, then `kubectl delete node {node}`, then stop the machine (`fluxctl cluster status` lists them)"
        ),
    )
}

async fn upgrade<A: crate::run::VmApi>(
    api: &A,
    channel: &str,
    yes: bool,
    target: &Target,
) -> Result<()> {
    if !channel_ok(channel) {
        bail!("--channel {channel:?} must be stable, latest, testing or a minor such as v1.31");
    }
    let (sel, prefix) = selection(target)?;
    let nodes = read_nodes(&Cluster::new(prefix, &SystemKubectl))?;
    let channels = fetch_channels()
        .await
        .map_err(|e| refused(format!("could not look up the {channel} release at update.k3s.io ({e}). Check this machine's internet connection")))?;
    let to = channel_version(&channels, channel).ok_or_else(|| {
        refused(format!(
            "the update service does not know a {channel} channel"
        ))
    })?;
    let p = plan(&nodes, &to)?;
    let label = sel.cluster.clone().unwrap_or_else(|| "the cluster".into());
    print!("{}", render_plan(&p, &label, channel));
    if p.steps.is_empty() {
        return Ok(());
    }
    if !yes {
        println!("\nthis was only the plan. Run the same command with --yes to do it");
        return Ok(());
    }
    if sel.kube_context.is_some() {
        return Err(refused(
            "only clusters made by `fluxctl kube cluster` on this machine can be upgraded from here; use --cluster NAME",
        ));
    }
    let store = crate::kube_apple::model::Store::new(crate::kube_apple::model::default_root()?);
    let name = crate::kube_backups::pick_cluster(&store, sel.cluster.as_deref())?;
    store.require(&name).map_err(|e| refused(format!("{e}")))?;
    let h = crate::kube_apple::host::RealHost::new(api, &store, None)?;
    let t = crate::kube_backups::take_exclusive(&h, &store, &name, Kind::PreUpgrade, None)
        .await
        .map_err(|e| {
            refused(format!(
                "the backup that comes first could not be taken ({e}), so nothing was changed"
            ))
        })?;
    println!("backed up {name} to {} before upgrading", t.file.display());
    // The backup is already taken, so the rolling upgrade does not take a second one.
    let cmd = apple(&[
        "upgrade",
        name.as_str(),
        "--channel",
        channel,
        "--no-backup",
    ])?;
    crate::kube_apple::run(api, cmd).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::kube::fake::FakeKubectl;
    use serde_json::json;

    fn node(name: &str, ver: &str, server: bool, ready: bool) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            version: ver.into(),
            server,
            ready,
        }
    }

    #[test]
    fn nodes_are_read_servers_first() {
        let v = json!({"items": [
            {"metadata": {"name": "w1", "labels": {}}, "status": {"nodeInfo": {"kubeletVersion": "v1.31.3+k3s1"}, "conditions": [{"type": "Ready", "status": "True"}]}},
            {"metadata": {"name": "s", "labels": {"node-role.kubernetes.io/control-plane": "true"}}, "status": {"nodeInfo": {"kubeletVersion": "v1.31.3+k3s1"}, "conditions": [{"type": "Ready", "status": "False"}]}},
        ]});
        let n = parse_nodes(&v);
        assert_eq!(n[0], node("s", "v1.31.3+k3s1", true, false));
        assert!(n[1].ready && !n[1].server);
    }

    #[test]
    fn channel_resolves_a_version() {
        let j = r#"{"data":[{"id":"stable","latest":"v1.31.4+k3s1"},{"id":"v1.32","latest":"v1.32.0+k3s1"},{"id":"bad","latest":"x"}]}"#;
        assert_eq!(channel_version(j, "stable").unwrap(), "v1.31.4+k3s1");
        assert_eq!(channel_version(j, "v1.32").unwrap(), "v1.32.0+k3s1");
        assert!(
            channel_version(j, "bad").is_none()
                && channel_version(j, "nope").is_none()
                && channel_version("{", "stable").is_none()
        );
    }

    #[test]
    fn plan_orders_servers_first_and_skips_done_nodes() {
        let nodes = [
            node("s", "v1.31.3+k3s1", true, true),
            node("w1", "v1.31.4+k3s1", false, true),
            node("w2", "v1.31.3+k3s1", false, true),
        ];
        let p = plan(&nodes, "v1.31.4+k3s1").unwrap();
        assert_eq!(
            p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["s", "w2"]
        );
        assert_eq!(p.done, ["w1"]);
        let text = render_plan(&p, "dev", "stable");
        assert!(
            text.contains("0. a verified backup")
                && text.contains("1. s (server)")
                && text.contains("rollback:"),
            "{text}"
        );
        let same = plan(&[node("s", "v1.31.4+k3s1", true, true)], "v1.31.4+k3s1").unwrap();
        assert!(render_plan(&same, "dev", "stable").contains("nothing to do"));
    }

    #[test]
    fn plan_refuses_downgrades_skipped_minors_and_sick_nodes() {
        let s = |v: &str| [node("s", v, true, true)];
        for (have, to, why) in [
            ("v1.31.4+k3s1", "v1.31.3+k3s1", "downgrades"),
            ("v1.31.4+k3s1", "v1.33.0+k3s1", "skips a Kubernetes minor"),
            ("v1.31.4+k3s1", "v2.0.0+k3s1", "skips a Kubernetes minor"),
            ("v1.31.4+k3s1", "nonsense", "not a k3s version"),
        ] {
            let e = plan(&s(have), to).unwrap_err().to_string();
            assert!(e.contains(why) && e.contains("KUBE_UPGRADE_REFUSED"), "{e}");
        }
        assert!(plan(&s("v1.31.4+k3s1"), "v1.32.0+k3s1").is_ok());
        let e = plan(
            &[
                node("s", "v1.31.4+k3s1", true, true),
                node("w1", "v1.31.4+k3s1", false, false),
            ],
            "v1.32.0+k3s1",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("not every node is Ready (w1)"), "{e}");
    }

    #[test]
    fn nodes_are_read_through_kubectl() {
        let f = FakeKubectl::default().json(json!({"items": []}));
        let c = Cluster::new(vec!["kubectl".into()], &f);
        assert!(read_nodes(&c).unwrap().is_empty());
        assert_eq!(f.argvs()[0], "kubectl get nodes -o json");
    }

    #[test]
    fn kube_cluster_commands_parse() {
        assert!(matches!(
            apple(&["join", "dev", "--node", "dev-a2"]).unwrap(),
            AppleCommand::Join { .. }
        ));
        assert!(matches!(
            apple(&["upgrade", "dev", "--channel", "stable", "--no-backup"]).unwrap(),
            AppleCommand::Upgrade { .. }
        ));
    }

    #[test]
    fn node_rm_is_a_coded_refusal_with_the_manual_steps() {
        let e = rm_refusal("dev-a1").to_string();
        assert!(
            e.contains("KUBE_NODE_UNSUPPORTED") && e.contains("kubectl drain dev-a1"),
            "{e}"
        );
    }
}
