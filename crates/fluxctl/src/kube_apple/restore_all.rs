// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Restoring an etcd snapshot on a multi-server cluster: `kube apple snapshot restore NAME SNAPSHOT --rejoin-servers --yes`.
//!
//! The k3s procedure (docs.k3s.io, "Cluster Datastore": restore with several servers) is: stop k3s on every server, run
//! `--cluster-reset --cluster-reset-restore-path` on one, start it, then delete the other servers' etcd data and start them so
//! they rejoin and copy the restored state. This does that, one server at a time, and does it only after checking that every
//! server answers and the snapshot file exists. Data directories are moved aside (`db.pre-restore-STAMP`), not deleted.
//! Not run against real hardware.

use super::model::*;
use super::platform::{Remote, restart_and_wait, wait_up};
use super::script::{self, SERVER_DIR, SNAPSHOT_DIR};
use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct RestoreAllReq {
    pub snapshot: String,
    /// The server whose disk holds the snapshot (default: the first server).
    pub from: Option<String>,
    pub yes: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct RestoreAllReport {
    pub restored_on: String,
    pub wiped_and_rejoined: Vec<String>,
    pub kept_aside: Vec<String>,
}

pub fn stop_script() -> String {
    ": fluxvm-op restore-stop; set -eu; sudo -n systemctl stop k3s.service".into()
}

pub fn snapshot_probe_script(snap: &str) -> String {
    assert!(script::snapshot_name_ok(snap));
    format!(": fluxvm-op restore-probe; set -eu; sudo -n test -f {SNAPSHOT_DIR}/{snap} && echo present")
}

/// Moves the etcd data aside so the server rejoins and copies the restored state. The service stays stopped.
pub fn wipe_script(stamp: &str) -> String {
    assert!(stamp.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    format!(
        ": fluxvm-op restore-wipe; set -eu; sudo -n systemctl stop k3s.service; \
         if sudo -n test -d {SERVER_DIR}/db; then sudo -n mv {SERVER_DIR}/db {SERVER_DIR}/db.pre-restore-{stamp}; fi; \
         echo db.pre-restore-{stamp}"
    )
}

/// What would happen, for the confirmation step.
pub fn plan_text(cluster: &str, servers: &[String], from: &str, snap: &str) -> String {
    let others: Vec<&String> = servers.iter().filter(|s| *s != from).collect();
    format!(
        "restoring {snap} on {cluster} (servers: {}):\n\
         1. stop k3s on every server (the API is down until step 3 finishes)\n\
         2. on {from}: cluster-reset from {SNAPSHOT_DIR}/{snap}, start k3s, wait until ready\n\
         3. on {}: move {SERVER_DIR}/db aside, start k3s, wait until it has rejoined, one at a time\n\
         Everything written to the cluster after the snapshot is lost. Agents are not touched and reconnect by themselves.",
        servers.join(", "),
        others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
    )
}

pub async fn restore_all<R: Remote>(r: &R, st: &State, req: &RestoreAllReq) -> Result<RestoreAllReport> {
    if st.datastore != Datastore::Etcd {
        bail!("snapshots need embedded etcd (create --embedded-etcd); a SQLite cluster uses `restore`");
    }
    if !script::snapshot_name_ok(&req.snapshot) {
        bail!("{:?} is not a snapshot name (letters, digits and - _ . only)", req.snapshot);
    }
    let servers = r.servers();
    if servers.len() < 2 {
        bail!("this cluster has one server; use `snapshot restore NAME SNAPSHOT --yes` without --rejoin-servers");
    }
    let from = match &req.from {
        Some(f) if servers.contains(f) => f.clone(),
        Some(f) => bail!("{f} is not a server of this cluster ({})", servers.join(", ")),
        None => servers[0].clone(),
    };
    // Nothing is stopped until every server answers and the snapshot is where it should be.
    for s in &servers {
        if let Err(e) = r.run(s, ": fluxvm-op restore-ping; true", None).await {
            bail!("{s} does not answer ({e:#}); a server that is down cannot be wiped and rejoined: start it, or remove it with `kube apple remove` first. Nothing was changed");
        }
    }
    match r.run(&from, &snapshot_probe_script(&req.snapshot), None).await {
        Ok(o) if o.trim() == "present" => {}
        _ => bail!("{} is not on {from} ({SNAPSHOT_DIR}); `snapshot ls` shows what each server has, `--from-node` picks the server. Nothing was changed", req.snapshot),
    }
    if !req.yes {
        bail!("{}\nadd --yes to do this", plan_text(&st.name, &servers, &from, &req.snapshot));
    }
    let recover = format!("the cluster may be stopped: `fluxctl kube apple start {}` starts it again (without the restore)", st.name);
    for s in &servers {
        r.run(s, &stop_script(), None).await.map_err(|e| anyhow::anyhow!("{s}: stopping k3s failed: {e:#}; {recover}"))?;
    }
    r.run(&from, &script::snapshot_restore_script(&req.snapshot), None)
        .await
        .map_err(|e| anyhow::anyhow!("{from}: cluster-reset failed: {e:#}; the other servers are stopped and still hold their old data; {recover}"))?;
    // the reset script starts k3s itself
    wait_up(r, &from, Role::Server)
        .await
        .map_err(|e| anyhow::anyhow!("{from} did not come up after the restore: {e:#}; check `fluxctl kube apple logs {} --node {from}`", st.name))?;
    let mut rep = RestoreAllReport { restored_on: from.clone(), ..Default::default() };
    let stamp = r.stamp();
    for s in servers.iter().filter(|s| **s != from) {
        let res = async {
            let aside = r.run(s, &wipe_script(&stamp), None).await?;
            restart_and_wait(r, s, Role::Server).await?;
            anyhow::Ok(aside)
        }
        .await;
        match res {
            Ok(aside) => {
                rep.wiped_and_rejoined.push(s.clone());
                rep.kept_aside.push(format!("{s}:{SERVER_DIR}/{}", aside.trim()));
            }
            Err(e) => bail!(
                "{s}: {e:#}; restored on {from}, rejoined so far: [{}]; the restore does not need repeating: fix {s} and start it (its old data is in {SERVER_DIR}/db.pre-restore-{stamp})",
                rep.wiped_and_rejoined.join(", ")
            ),
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    fn st(ds: Datastore) -> State {
        // a minimal record; only the datastore and name matter here
        serde_json::from_value(serde_json::json!({
            "schema": 1, "name": "c", "owner": "o", "phase": "ready", "user": "u", "api_port": 16443,
            "datastore": if ds == Datastore::Etcd { "etcd" } else { "sqlite" },
            "params": {"cpus": 2, "memory_mib": 4096, "disk_gib": 30, "image": "debian-13"}, "nodes": []
        }))
        .unwrap()
    }

    fn req(yes: bool) -> RestoreAllReq {
        RestoreAllReq { snapshot: "on-demand-c-s1-1".into(), from: None, yes }
    }

    fn fake() -> FakeRemote {
        let r = FakeRemote::cluster(3, 1);
        r.answer("restore-probe", "present");
        r.answer("restore-wipe", "db.pre-restore-20261122-120000");
        r
    }

    #[test]
    fn scripts_golden() {
        assert_eq!(
            wipe_script("20261122-120000"),
            ": fluxvm-op restore-wipe; set -eu; sudo -n systemctl stop k3s.service; \
             if sudo -n test -d /var/lib/rancher/k3s/server/db; then sudo -n mv /var/lib/rancher/k3s/server/db /var/lib/rancher/k3s/server/db.pre-restore-20261122-120000; fi; \
             echo db.pre-restore-20261122-120000"
        );
        assert!(snapshot_probe_script("a-1").contains("test -f /var/lib/rancher/k3s/server/db/snapshots/a-1"));
    }

    #[test]
    fn plan_text_golden() {
        let t = plan_text("c", &["c-s1".into(), "c-s2".into(), "c-s3".into()], "c-s1", "snap");
        assert!(t.starts_with("restoring snap on c (servers: c-s1, c-s2, c-s3):\n1. stop k3s on every server"));
        assert!(t.contains("3. on c-s2, c-s3: move /var/lib/rancher/k3s/server/db aside"));
    }

    #[tokio::test]
    async fn without_yes_it_prints_the_plan_and_changes_nothing() {
        let r = fake();
        let e = restore_all(&r, &st(Datastore::Etcd), &req(false)).await.unwrap_err().to_string();
        assert!(e.contains("add --yes") && e.contains("stop k3s on every server"), "{e}");
        let ops = r.ops();
        assert!(ops.iter().all(|o| o.ends_with("restore-ping") || o.ends_with("restore-probe")), "{ops:?}");
    }

    #[tokio::test]
    async fn stops_all_then_resets_one_then_rejoins_the_rest_one_at_a_time() {
        let r = fake();
        let rep = restore_all(&r, &st(Datastore::Etcd), &req(true)).await.unwrap();
        assert_eq!(rep.restored_on, "c-s1");
        assert_eq!(rep.wiped_and_rejoined, ["c-s2", "c-s3"]);
        let ops: Vec<String> = r.ops().into_iter().filter(|o| !o.contains("restore-ping") && !o.contains("restore-probe")).collect();
        let pos = |needle: &str| ops.iter().position(|o| o == needle).unwrap_or_else(|| panic!("{needle} in {ops:?}"));
        for s in ["c-s1", "c-s2", "c-s3"] {
            assert!(pos(&format!("{s}:restore-stop")) < pos("c-s1:-"), "every server stops before the reset");
        }
        // the reset script is the existing cluster-reset one
        assert!(r.log.borrow().iter().any(|(n, s, _)| n == "c-s1" && s.contains("--cluster-reset-restore-path=/var/lib/rancher/k3s/server/db/snapshots/on-demand-c-s1-1")));
        assert!(pos("c-s1:-") < pos("c-s2:restore-wipe"));
        assert!(pos("c-s2:restore-wipe") < pos("c-s2:platform-restart"));
        assert!(pos("c-s2:platform-restart") < pos("c-s3:restore-wipe"));
        // the agent is not touched
        assert!(!ops.iter().any(|o| o.starts_with("c-a1")));
        assert_eq!(rep.kept_aside[0], "c-s2:/var/lib/rancher/k3s/server/db.pre-restore-20261122-120000");
    }

    #[tokio::test]
    async fn refuses_before_touching_anything() {
        // a server that does not answer
        let r = fake();
        r.fail_on("restore-ping", "no route");
        assert!(restore_all(&r, &st(Datastore::Etcd), &req(true)).await.unwrap_err().to_string().contains("Nothing was changed"));
        assert!(!r.ops().iter().any(|o| o.ends_with("restore-stop")));
        // snapshot missing
        let r = FakeRemote::cluster(3, 0);
        let e = restore_all(&r, &st(Datastore::Etcd), &req(true)).await.unwrap_err().to_string();
        assert!(e.contains("is not on c-s1"), "{e}");
        assert!(!r.ops().iter().any(|o| o.ends_with("restore-stop")));
        // sqlite, one server, unknown --from-node, bad name
        let r = fake();
        assert!(restore_all(&r, &st(Datastore::Sqlite), &req(true)).await.is_err());
        let one = FakeRemote::cluster(1, 0);
        assert!(restore_all(&one, &st(Datastore::Etcd), &req(true)).await.unwrap_err().to_string().contains("one server"));
        let mut q = req(true);
        q.from = Some("c-a1".into());
        assert!(restore_all(&r, &st(Datastore::Etcd), &q).await.is_err());
        q.from = None;
        q.snapshot = "x; rm".into();
        assert!(restore_all(&r, &st(Datastore::Etcd), &q).await.is_err());
    }

    #[tokio::test]
    async fn a_failed_reset_leaves_the_other_servers_data_alone() {
        let r = fake();
        r.fail_on("--cluster-reset", "etcd says no");
        let e = restore_all(&r, &st(Datastore::Etcd), &req(true)).await.unwrap_err().to_string();
        assert!(e.contains("c-s1: cluster-reset failed") && e.contains("still hold their old data"), "{e}");
        assert!(!r.ops().iter().any(|o| o.ends_with("restore-wipe")));
    }
}
