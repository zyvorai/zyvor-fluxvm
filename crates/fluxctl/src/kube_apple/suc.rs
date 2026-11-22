// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `kube apple upgrade NAME --channel stable`: upgrades the cluster through Rancher's system-upgrade-controller (SUC) instead
//! of replacing the binary from the Mac. The controller and its CRDs come from a pinned, checksum-verified manifest
//! ([`super::pins`]); two `Plan`s (servers one at a time, then agents) follow a k3s release channel.
//!
//! Differences from `upgrade --binary`: the new binary is fetched by the cluster from the `rancher/k3s-upgrade` image, not
//! verified on this Mac, and the journal's recorded k3s version and checksum are not updated (`status` shows the real one).
//! A downgrade or a jump over a minor release is refused before anything is installed. Not run against real hardware.

use super::model::*;
use super::pins;
use super::platform::Remote;
use super::recipes::{PinSrc, apply_pin};
use super::script::{self, K3S_BIN};
use anyhow::{Result, bail};
use serde::Serialize;

pub const NAMESPACE: &str = "system-upgrade";
const CHANNELS_URL: &str = "https://update.k3s.io/v1-release/channels";

/// `stable`, `latest`, `testing`, or a minor channel such as `v1.31`.
pub fn channel_ok(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.as_bytes()[0].is_ascii_alphanumeric()
        && c.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

pub fn channel_url(c: &str) -> String {
    assert!(channel_ok(c));
    format!("{CHANNELS_URL}/{c}")
}

/// Asks the update service, from a server, which release the channel points at now (it answers with a redirect).
pub fn resolve_script(channel: &str) -> String {
    format!(
        ": fluxvm-op suc-resolve; set -eu; curl -fsS --proto '=https' --tlsv1.2 --max-time 20 -o /dev/null -w '%{{redirect_url}}' '{}'",
        channel_url(channel)
    )
}

/// `https://github.com/k3s-io/k3s/releases/tag/v1.31.5%2Bk3s1` -> `v1.31.5+k3s1`.
pub fn parse_resolved(redirect: &str) -> Option<String> {
    let tag = redirect.trim().rsplit('/').next()?.replace("%2B", "+").replace("%2b", "+");
    parse_version(&tag).map(|_| tag)
}

/// The two Plans, as one multi-document YAML. The agent plan waits for the server plan (`prepare`).
pub fn plans_yaml(channel: &str, drain_agents: bool) -> String {
    let url = channel_url(channel);
    let agent_mode = if drain_agents {
        "  drain:\n    force: true\n    skipWaitForDeleteTimeout: 60\n"
    } else {
        "  cordon: true\n"
    };
    format!(
        "apiVersion: upgrade.cattle.io/v1\n\
         kind: Plan\n\
         metadata:\n  name: k3s-server\n  namespace: {NAMESPACE}\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\n\
         spec:\n  concurrency: 1\n  cordon: true\n  nodeSelector:\n    matchExpressions:\n    \
         - key: node-role.kubernetes.io/control-plane\n      operator: In\n      values:\n      - \"true\"\n  \
         serviceAccountName: system-upgrade\n  upgrade:\n    image: rancher/k3s-upgrade\n  channel: {url}\n\
         ---\n\
         apiVersion: upgrade.cattle.io/v1\n\
         kind: Plan\n\
         metadata:\n  name: k3s-agent\n  namespace: {NAMESPACE}\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\n\
         spec:\n  concurrency: 1\n{agent_mode}  nodeSelector:\n    matchExpressions:\n    \
         - key: node-role.kubernetes.io/control-plane\n      operator: NotIn\n      values:\n      - \"true\"\n  \
         serviceAccountName: system-upgrade\n  prepare:\n    image: rancher/k3s-upgrade\n    args:\n    - prepare\n    - k3s-server\n  \
         upgrade:\n    image: rancher/k3s-upgrade\n  channel: {url}\n"
    )
}

#[derive(Debug, Clone)]
pub struct ChannelReq {
    pub channel: String,
    pub allow_downgrade: bool,
    pub no_backup: bool,
    pub drain: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct ChannelReport {
    pub channel: String,
    pub from: String,
    pub to: String,
    pub up_to_date: bool,
    pub backup: Option<String>,
    pub controller: String,
    pub watch: String,
    pub note: String,
}

pub async fn upgrade_channel<R: Remote>(r: &R, st: &State, req: &ChannelReq) -> Result<ChannelReport> {
    if st.external_url.is_some() {
        bail!("upgrade the nodes of a cluster that another Mac owns from that Mac");
    }
    if !channel_ok(&req.channel) {
        bail!("--channel {:?} must be stable, latest, testing or a minor such as v1.31", req.channel);
    }
    let backup_name = format!("pre-suc-{}", r.stamp());
    if st.datastore == Datastore::Sqlite && !req.no_backup {
        bail!("this cluster uses SQLite, which cannot be copied while running: take `fluxctl kube apple backup {}` first, then repeat with --no-backup", st.name);
    }
    // health first: do not start a rolling upgrade on a cluster that is already unwell
    let nodes = script::parse_nodes(&r.run_server(&script::nodes_json_script(), None).await?)?;
    let sick: Vec<&str> = nodes.iter().filter(|n| !n.ready).map(|n| n.name.as_str()).collect();
    if !sick.is_empty() {
        bail!("not every node is Ready ({}); fix that before upgrading", sick.join(", "));
    }
    let server = r.servers().first().cloned().ok_or_else(|| anyhow::anyhow!("no server in this record"))?;
    let from = script::parse_version_line(&r.run(&server, &script::version_script(K3S_BIN), None).await?)
        .ok_or_else(|| anyhow::anyhow!("cannot read the server's k3s version"))?;
    let to = parse_resolved(&r.run(&server, &resolve_script(&req.channel), None).await?)
        .ok_or_else(|| anyhow::anyhow!("the update service did not say which release {:?} points at", req.channel))?;
    let mut rep = ChannelReport { channel: req.channel.clone(), from: from.clone(), to: to.clone(), ..Default::default() };
    if parse_version(&to) == parse_version(&from) {
        rep.up_to_date = true;
        rep.note = "the channel's release is the version already running; nothing was changed".into();
        return Ok(rep);
    }
    upgrade_allowed(&from, &to, req.allow_downgrade)?;
    if st.datastore == Datastore::Etcd && !req.no_backup {
        r.run(&server, &script::snapshot_save_script(&backup_name), None).await?;
        rep.backup = Some(backup_name);
    }
    apply_pin(r, &pins::SUC_CRD, PinSrc::Download).await?;
    r.run_server(
        &format!(": fluxvm-op suc-wait-crd; set -eu; sudo -n {K3S_BIN} kubectl wait --for=condition=Established crd/plans.upgrade.cattle.io --timeout=90s"),
        None,
    )
    .await?;
    apply_pin(r, &pins::SUC, PinSrc::Download).await?;
    r.run_server(
        &format!(": fluxvm-op suc-rollout; set -eu; sudo -n {K3S_BIN} kubectl -n {NAMESPACE} rollout status deployment/system-upgrade-controller --timeout=180s"),
        None,
    )
    .await?;
    rep.controller = format!("system-upgrade-controller {}", pins::SUC.version);
    r.run_server(
        &format!(": fluxvm-op suc-plans; set -eu; sudo -n {K3S_BIN} kubectl apply -f -"),
        Some(plans_yaml(&req.channel, req.drain).as_bytes()),
    )
    .await?;
    rep.watch = format!("fluxctl kube apple status {}   (or kubectl -n {NAMESPACE} get plans,jobs; the controller upgrades servers one at a time, then agents)", st.name);
    rep.note = "the upgrade now runs inside the cluster and takes a while; the journal's recorded k3s version is not updated".into();
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    fn st(ds: &str) -> State {
        serde_json::from_value(serde_json::json!({
            "schema": 1, "name": "c", "owner": "o", "phase": "ready", "user": "u", "api_port": 16443, "datastore": ds,
            "params": {"cpus": 2, "memory_mib": 4096, "disk_gib": 30, "image": "debian-13"}, "nodes": []
        }))
        .unwrap()
    }

    fn req() -> ChannelReq {
        ChannelReq { channel: "stable".into(), allow_downgrade: false, no_backup: false, drain: false }
    }

    const NODES: &str = r#"{"items":[{"metadata":{"name":"c-s1"},"status":{"conditions":[{"type":"Ready","status":"True"}],"nodeInfo":{"kubeletVersion":"v1.31.4+k3s1"}}}]}"#;

    fn fake(from: &str, to_url: &str) -> FakeRemote {
        let r = FakeRemote::cluster(1, 0);
        r.answer("get nodes -o json", NODES);
        r.answer("--version", &format!("k3s version {from} (abc)"));
        r.answer("suc-resolve", to_url);
        r
    }

    #[test]
    fn channels() {
        for ok in ["stable", "latest", "testing", "v1.31"] {
            assert!(channel_ok(ok), "{ok}");
        }
        for bad in ["", "Stable", "a b", "../x", "x;y", "-x", &"a".repeat(40)] {
            assert!(!channel_ok(bad), "{bad}");
        }
        assert_eq!(channel_url("v1.31"), "https://update.k3s.io/v1-release/channels/v1.31");
    }

    #[test]
    fn resolves_the_redirect() {
        assert_eq!(parse_resolved("https://github.com/k3s-io/k3s/releases/tag/v1.31.5%2Bk3s1\n").as_deref(), Some("v1.31.5+k3s1"));
        assert_eq!(parse_resolved("https://x/tag/v1.31.5+k3s1").as_deref(), Some("v1.31.5+k3s1"));
        assert!(parse_resolved("").is_none());
        assert!(parse_resolved("https://x/tag/nonsense").is_none());
    }

    #[test]
    fn plans_golden() {
        let want = "apiVersion: upgrade.cattle.io/v1\n\
kind: Plan\n\
metadata:\n  name: k3s-server\n  namespace: system-upgrade\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\n\
spec:\n  concurrency: 1\n  cordon: true\n  nodeSelector:\n    matchExpressions:\n    - key: node-role.kubernetes.io/control-plane\n      operator: In\n      values:\n      - \"true\"\n  serviceAccountName: system-upgrade\n  upgrade:\n    image: rancher/k3s-upgrade\n  channel: https://update.k3s.io/v1-release/channels/stable\n\
---\n\
apiVersion: upgrade.cattle.io/v1\n\
kind: Plan\n\
metadata:\n  name: k3s-agent\n  namespace: system-upgrade\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\n\
spec:\n  concurrency: 1\n  cordon: true\n  nodeSelector:\n    matchExpressions:\n    - key: node-role.kubernetes.io/control-plane\n      operator: NotIn\n      values:\n      - \"true\"\n  serviceAccountName: system-upgrade\n  prepare:\n    image: rancher/k3s-upgrade\n    args:\n    - prepare\n    - k3s-server\n  upgrade:\n    image: rancher/k3s-upgrade\n  channel: https://update.k3s.io/v1-release/channels/stable\n";
        assert_eq!(plans_yaml("stable", false), want);
        for y in [plans_yaml("v1.31", true), want.to_owned()] {
            for doc in serde_yaml::Deserializer::from_str(&y) {
                let v: serde_yaml::Value = serde::Deserialize::deserialize(doc).unwrap();
                assert_eq!(v["kind"], "Plan");
            }
        }
        assert!(plans_yaml("stable", true).contains("  drain:\n    force: true"));
    }

    #[tokio::test]
    async fn installs_the_pinned_controller_then_the_plans_after_a_snapshot() {
        let r = fake("v1.31.4+k3s1", "https://github.com/k3s-io/k3s/releases/tag/v1.32.0%2Bk3s1");
        let rep = upgrade_channel(&r, &st("etcd"), &req()).await.unwrap();
        assert_eq!((rep.from.as_str(), rep.to.as_str()), ("v1.31.4+k3s1", "v1.32.0+k3s1"));
        assert_eq!(rep.backup.as_deref(), Some("pre-suc-20261122-120000"));
        let log = r.log.borrow();
        let find = |needle: &str| log.iter().position(|(_, s, _)| s.contains(needle)).unwrap_or_else(|| panic!("{needle}"));
        let snap = find("etcd-snapshot save --name pre-suc-20261122-120000");
        let crd = find(pins::SUC_CRD.sha256);
        let ctl = find(pins::SUC.sha256);
        let plans = find("suc-plans");
        assert!(snap < crd && crd < find("suc-wait-crd") && find("suc-wait-crd") < ctl && ctl < find("suc-rollout") && find("suc-rollout") < plans);
        assert!(log[plans].2.as_deref().unwrap().contains("channels/stable"));
    }

    #[tokio::test]
    async fn refuses_downgrades_skips_and_unwell_clusters_before_installing() {
        let r = fake("v1.32.0+k3s1", "https://x/tag/v1.31.5%2Bk3s1");
        assert!(upgrade_channel(&r, &st("etcd"), &req()).await.unwrap_err().to_string().contains("older than"));
        let r = fake("v1.30.1+k3s1", "https://x/tag/v1.32.0%2Bk3s1");
        assert!(upgrade_channel(&r, &st("etcd"), &req()).await.unwrap_err().to_string().contains("skips a minor"));
        for r in [fake("v1.32.0+k3s1", "https://x/tag/v1.31.5%2Bk3s1"), fake("v1.30.1+k3s1", "https://x/tag/v1.32.0%2Bk3s1")] {
            let _ = upgrade_channel(&r, &st("etcd"), &req()).await;
            assert!(!r.log.borrow().iter().any(|(_, s, _)| s.contains("kubectl apply")), "nothing installed");
            assert!(!r.log.borrow().iter().any(|(_, s, _)| s.contains("etcd-snapshot save")), "no snapshot for a refused upgrade");
        }
        let r2 = FakeRemote::cluster(1, 0);
        r2.answer("get nodes -o json", &NODES.replace("\"True\"", "\"False\""));
        assert!(upgrade_channel(&r2, &st("etcd"), &req()).await.unwrap_err().to_string().contains("not every node is Ready"));
    }

    #[tokio::test]
    async fn same_version_changes_nothing_and_sqlite_needs_an_acknowledged_backup() {
        let r = fake("v1.31.4+k3s1", "https://x/tag/v1.31.4%2Bk3s1");
        let rep = upgrade_channel(&r, &st("etcd"), &req()).await.unwrap();
        assert!(rep.up_to_date);
        assert!(!r.log.borrow().iter().any(|(_, s, _)| s.contains("kubectl apply")));
        let r = fake("v1.31.4+k3s1", "https://x/tag/v1.32.0%2Bk3s1");
        assert!(upgrade_channel(&r, &st("sqlite"), &req()).await.unwrap_err().to_string().contains("--no-backup"));
        assert!(r.log.borrow().is_empty());
        let mut q = req();
        q.no_backup = true;
        let rep = upgrade_channel(&r, &st("sqlite"), &q).await.unwrap();
        assert!(rep.backup.is_none());
    }
}
