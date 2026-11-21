// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Tests for the HA control plane, the snapshot schedule, token rotation, node removal, rebuild and doctor, against the same
//! fake host as the rest of the lifecycle.

use super::*;
use crate::kube_apple::doctor::{self, Level};
use crate::kube_apple::ops::{self, RebuildReq, RemoveReq, RotateReq, ScheduleAction};

fn ha_req(name: &str, n: u8) -> CreateReq {
    let mut r = create_req(name);
    r.servers = n;
    r.datastore = Datastore::Etcd;
    r
}

async fn ha_cluster(f: &Fake, s: &Store) {
    create(f, s, &ha_req("ha", 3)).await.unwrap();
}

fn agent_req(cluster: &str) -> JoinReq {
    join_req(cluster)
}

fn set_ready(f: &Fake, node: &str, ready: bool) {
    for n in f.nodes.borrow_mut().iter_mut().filter(|n| n.name == node) {
        n.ready = ready;
    }
}

fn stop_vm(f: &Fake, vm: &str) {
    f.vms.borrow_mut().iter_mut().find(|v| v.name == vm).unwrap().status = "stopped".into();
}

fn count(f: &Fake, needle: &str) -> usize {
    f.lines().iter().filter(|l| l.contains(needle)).count()
}

fn stdin_of(f: &Fake, node: &str, tag: &str) -> Vec<String> {
    f.stdin_seen
        .borrow()
        .iter()
        .filter(|(n, t, _)| n == node && t == tag)
        .map(|(_, _, d)| String::from_utf8_lossy(d).into_owned())
        .collect()
}

fn no_script_contains(f: &Fake, secret: &str) -> bool {
    !f.scripts.borrow().iter().any(|(_, s)| s.contains(secret))
}

// ---- create --servers --------------------------------------------------------------------------------------------

#[tokio::test]
async fn three_servers_are_created_one_after_another_and_agree_on_flags() {
    let (_d, s, f) = setup();
    let mut req = ha_req("ha", 3);
    req.tls_sans = vec!["cluster.example".into()];
    req.flags = SharedFlags {
        cluster_cidr: Some("10.50.0.0/16".into()),
        disable: vec!["coredns".into()],
        secrets_encryption: true,
        ..Default::default()
    };
    let r = create(&f, &s, &req).await.unwrap();
    assert_eq!(r.servers, ["ha", "ha-s2", "ha-s3"]);
    let st = s.require("ha").unwrap();
    assert_eq!(st.servers_wanted, 3);
    assert!(st.servers().iter().all(|n| n.joined));
    assert_eq!(st.endpoint, Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }));
    assert!(st.uses_proxy());
    let l = f.lines();
    assert!(pos(&l, "create_vm k3s-ha-s2") > pos(&l, "ssh ha unit"));
    assert!(pos(&l, "create_vm k3s-ha-s3") > pos(&l, "ssh ha-s2 unit"));
    let ex = f.exec.borrow();
    for n in ["ha", "ha-s2", "ha-s3"] {
        let e = &ex[n];
        for want in [
            "--disable traefik --disable coredns",
            "--cluster-cidr 10.50.0.0/16",
            "--secrets-encryption",
            "--tls-san cluster.example",
            "--agent-token-file",
        ] {
            assert!(e.contains(want), "{n}: {want} missing in {e}");
        }
    }
    assert!(ex["ha"].contains("--cluster-init") && !ex["ha"].contains("--server"));
    for n in ["ha-s2", "ha-s3"] {
        assert!(ex[n].contains("--server https://10.89.1.2:6443") && !ex[n].contains("--cluster-init"));
    }
    // Joining servers hold both tokens, delivered on stdin; neither is in a script or the journal.
    assert_eq!(stdin_of(&f, "ha-s2", "token"), [TOKEN]);
    assert_eq!(stdin_of(&f, "ha-s2", "agent-token"), [TOKEN]);
    assert!(no_script_contains(&f, TOKEN));
    assert!(!std::fs::read_to_string(s.path("ha")).unwrap().contains(TOKEN));
}

#[tokio::test]
async fn bad_server_counts_flags_and_endpoints_are_refused_before_anything_exists() {
    let (_d, s, f) = setup();
    let mut cases: Vec<(CreateReq, &str)> = vec![];
    cases.push((ha_req("x", 2), "odd"));
    cases.push((ha_req("x", 0), "odd"));
    cases.push((ha_req("x", 9), "odd"));
    let mut sqlite = ha_req("x", 3);
    sqlite.datastore = Datastore::Sqlite;
    cases.push((sqlite, "embedded etcd"));
    let mut vip = ha_req("x", 3);
    vip.endpoint = Some(EndpointKind::KubeVip);
    cases.push((vip, "--vip"));
    let mut vip = ha_req("x", 3);
    vip.endpoint = Some(EndpointKind::KubeVip);
    vip.vip = Some("192.168.1.9".into());
    cases.push((vip, "private network"));
    let mut vip = ha_req("x", 3);
    vip.vip = Some("10.89.0.250".into());
    cases.push((vip, "only for --endpoint kube-vip"));
    let mut single = create_req("x");
    single.endpoint = Some(EndpointKind::KubeVip);
    cases.push((single, "--servers"));
    let mut flags = ha_req("x", 3);
    flags.flags.cluster_cidr = Some("not-a-cidr".into());
    cases.push((flags, "CIDR"));
    let mut port = ha_req("x", 3);
    port.api_port = 65535;
    cases.push((port, "eight free ports"));
    for (req, want) in cases {
        let e = create(&f, &s, &req).await.unwrap_err().to_string();
        assert!(e.contains(want), "{want:?} not in {e:?}");
    }
    assert!(f.lines().is_empty());
    assert!(s.list().unwrap().is_empty());
}

#[tokio::test]
async fn kube_vip_puts_its_manifest_on_every_server_and_its_address_in_the_certificate() {
    let (_d, s, f) = setup();
    let mut req = ha_req("ha", 3);
    req.endpoint = Some(EndpointKind::KubeVip);
    req.vip = Some("10.89.0.250".into());
    create(&f, &s, &req).await.unwrap();
    let st = s.require("ha").unwrap();
    assert_eq!(st.endpoint, Some(EndpointCfg { kind: EndpointKind::KubeVip, vip: Some("10.89.0.250".into()) }));
    assert!(st.tls_sans.contains(&"10.89.0.250".to_owned()));
    assert_eq!(*f.vip.borrow(), ["ha", "ha-s2", "ha-s3"]);
    for n in ["ha", "ha-s2", "ha-s3"] {
        assert!(f.exec.borrow()[n].contains("--tls-san 10.89.0.250"));
        let m = &stdin_of(&f, n, "kube-vip")[0];
        assert!(m.contains("10.89.0.250") && m.contains("cp_enable"));
    }
}

#[tokio::test]
async fn an_interrupted_server_join_is_finished_by_running_create_again_without_a_second_vm() {
    let (_d, s, f) = setup();
    f.fail_on("ha-s2:unit", "unit failed");
    let e = format!("{:#}", create(&f, &s, &ha_req("ha", 3)).await.unwrap_err());
    assert!(e.contains("create ha"), "{e}");
    let st = s.require("ha").unwrap();
    assert_eq!(st.phase, Phase::Ready);
    assert!(!st.node("ha-s2").unwrap().joined);
    assert_eq!(st.servers_wanted, 3);
    f.fail.borrow_mut().clear();
    let r = create(&f, &s, &ha_req("ha", 3)).await.unwrap();
    assert!(r.resumed);
    assert_eq!(r.servers, ["ha", "ha-s2", "ha-s3"]);
    assert_eq!(count(&f, "create_vm k3s-ha-s2"), 1);
    // And now it is complete: a third run refuses like any existing cluster.
    assert!(create(&f, &s, &ha_req("ha", 3)).await.unwrap_err().to_string().contains("already exists"));
}

#[tokio::test]
async fn a_server_added_by_hand_makes_the_cluster_multi_server() {
    let (_d, s, f) = setup();
    etcd_cluster(&f, &s).await;
    let mut r = join_req("dev");
    r.role = Role::Server;
    join(&f, &s, &r).await.unwrap();
    let st = s.require("dev").unwrap();
    assert_eq!(st.servers().len(), 2);
    assert_eq!(st.servers_wanted, 2);
    assert!(st.node("dev-s2").is_some());
    assert!(st.uses_proxy());
    assert!(f.lines().iter().any(|l| l.contains("kubeconfig written")));
    // SQLite clusters still refuse a second server.
    let (_d2, s2, f2) = setup();
    create(&f2, &s2, &create_req("lite")).await.unwrap();
    let mut r = join_req("lite");
    r.role = Role::Server;
    assert!(join(&f2, &s2, &r).await.unwrap_err().to_string().contains("embedded etcd"));
}

// ---- failover and quorum ------------------------------------------------------------------------------------------

#[tokio::test]
async fn commands_fall_over_to_another_server_when_the_first_does_not_answer() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    f.fail_on("ha:kubeconfig", "ha is down");
    f.fail_on("ha:nodes", "ha is down");
    f.clear();
    let p = kubeconfig(&f, &s, "ha", None).await.unwrap();
    assert!(p.is_file());
    assert!(f.lines().iter().any(|l| l == "ssh ha-s2 kubeconfig"));
    let st = s.require("ha").unwrap();
    let nodes = wait_ready(&f, &st, &["ha-s3".to_owned()], None).await.unwrap();
    assert_eq!(nodes.len(), 3);
    assert!(status(&f, &s, "ha").await.unwrap().healthy);
}

#[tokio::test]
async fn status_reports_lost_quorum_and_missing_servers() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    assert!(status(&f, &s, "ha").await.unwrap().healthy);
    set_ready(&f, "ha-s2", false);
    let r = status(&f, &s, "ha").await.unwrap();
    assert!(r.problems.iter().all(|p| !p.contains("quorum")), "{:?}", r.problems);
    set_ready(&f, "ha-s3", false);
    let r = status(&f, &s, "ha").await.unwrap();
    assert!(r.problems.iter().any(|p| p.contains("no quorum: 1 of 3")), "{:?}", r.problems);
    assert!(!r.healthy);
}

#[tokio::test]
async fn start_brings_every_server_up_before_waiting_for_the_first_to_be_ready() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    stop(&f, &s, "ha").await.unwrap();
    f.clear();
    start(&f, &s, "ha", false).await.unwrap();
    let l = f.lines();
    assert!(pos(&l, "power Start k3s-ha-s3") < pos(&l, "ssh ha nodes"), "{l:#?}");
}

// ---- snapshot schedule ---------------------------------------------------------------------------------------------

fn policy(s3: bool) -> SnapshotPolicy {
    SnapshotPolicy {
        cron: "0 */6 * * *".into(),
        retention: 10,
        compress: true,
        s3: s3.then(|| S3Target {
            endpoint: "s3.example:9000".into(),
            bucket: "k3s-snaps".into(),
            region: Some("eu-west-1".into()),
            folder: Some("ha".into()),
        }),
    }
}

const S3_SECRET: &str = "s3cr3t/key+value";

fn s3_secret() -> S3Secret {
    S3Secret::new("AKIAEXAMPLE", S3_SECRET).unwrap()
}

#[tokio::test]
async fn schedule_writes_a_private_dropin_to_each_server_and_restarts_them_one_at_a_time() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    f.clear();
    f.stdin_seen.borrow_mut().clear();
    let r = ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(true)), Some(s3_secret())))
        .await
        .unwrap();
    assert_eq!(r.applied_to, ["ha", "ha-s2", "ha-s3"]);
    let l = f.lines();
    assert!(pos(&l, "ssh ha write-dropin") < pos(&l, "ssh ha restart"));
    assert!(pos(&l, "ssh ha restart") < pos(&l, "ssh ha-s2 write-dropin"));
    assert!(pos(&l, "ssh ha-s2 restart") < pos(&l, "ssh ha-s3 write-dropin"));
    for n in ["ha", "ha-s2", "ha-s3"] {
        let d = &stdin_of(&f, n, "write-dropin")[0];
        assert!(d.contains("etcd-snapshot-retention: 10") && d.contains("etcd-s3-bucket: k3s-snaps"));
        assert!(d.contains(S3_SECRET), "the keys travel on stdin");
    }
    // Nowhere else: not in a script, not in the journal, not in the report.
    assert!(no_script_contains(&f, S3_SECRET) && no_script_contains(&f, "AKIAEXAMPLE"));
    let j = std::fs::read_to_string(s.path("ha")).unwrap();
    assert!(!j.contains(S3_SECRET) && !j.contains("AKIAEXAMPLE"));
    assert_eq!(s.require("ha").unwrap().snapshots, Some(policy(true)));
    assert!(!serde_json::to_string(&r).unwrap().contains(S3_SECRET));
}

#[tokio::test]
async fn schedule_scrubs_the_keys_from_a_failure_and_says_how_far_it_got() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    f.leak_stdin.set(true);
    f.fail_on("ha-s2:write-dropin", "boom");
    let e = format!(
        "{:#}",
        ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(true)), Some(s3_secret())))
            .await
            .unwrap_err()
    );
    assert!(!e.contains(S3_SECRET) && !e.contains("AKIAEXAMPLE"), "{e}");
    assert!(e.contains("configured so far: [ha]"), "{e}");
    assert_eq!(s.require("ha").unwrap().snapshots, None, "the journal records a policy only once every server has it");
}

#[tokio::test]
async fn schedule_validates_before_touching_a_server() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    f.clear();
    let e = |r: Result<ops::ScheduleReport>| r.unwrap_err().to_string();
    assert!(e(ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(true)), None)).await).contains("credentials"));
    assert!(e(ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(false)), Some(s3_secret()))).await).contains("no --s3-bucket"));
    let mut bad = policy(false);
    bad.cron = "every day".into();
    assert!(e(ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(bad), None)).await).contains("cron"));
    assert!(f.lines().is_empty());
    let (_d2, s2, f2) = setup();
    create(&f2, &s2, &create_req("lite")).await.unwrap();
    assert!(e(ops::snapshot_schedule(&f2, &s2, "lite", ScheduleAction::Reset).await).contains("embedded etcd"));
}

#[tokio::test]
async fn schedule_show_and_reset() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(false)), None)).await.unwrap();
    *f.snapshot_config.borrow_mut() = "etcd-snapshot-schedule-cron: 0 */6 * * *\n".into();
    let r = ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Show).await.unwrap();
    assert_eq!(r.policy, Some(policy(false)));
    assert_eq!(r.servers.len(), 3);
    assert_eq!(r.servers[0].newest_age_secs, Some(3600));
    assert!(r.servers[0].config.contains("0 */6"));
    f.clear();
    let r = ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Reset).await.unwrap();
    assert_eq!(r.applied_to.len(), 3);
    assert_eq!(count(&f, "remove-dropin"), 3);
    assert_eq!(s.require("ha").unwrap().snapshots, None);
}

// ---- token rotation -------------------------------------------------------------------------------------------------

async fn ha_with_agent(f: &Fake, s: &Store) {
    ha_cluster(f, s).await;
    join(f, s, &agent_req("ha")).await.unwrap();
    f.clear();
    f.stdin_seen.borrow_mut().clear();
    f.scripts.borrow_mut().clear();
}

#[tokio::test]
async fn rotating_the_agent_token_goes_servers_first_one_at_a_time_then_the_agents() {
    let (_d, s, f) = setup();
    ha_with_agent(&f, &s).await;
    let r = ops::token_rotate(&f, &s, "ha", &RotateReq { agent: true, server: false }).await.unwrap();
    assert!(r.agent_secret_rotated && !r.server_secret_rotated);
    assert_eq!(r.restarted, ["ha", "ha-s2", "ha-s3", "ha-a1"]);
    let l = f.lines();
    for (a, b) in [("ssh ha agent-token", "ssh ha restart"), ("ssh ha restart", "ssh ha-s2 agent-token"), ("ssh ha-s3 restart", "ssh ha-a1 token")] {
        assert!(pos(&l, a) < pos(&l, b), "{a} before {b} in {l:#?}");
    }
    let new = stdin_of(&f, "ha", "agent-token")[0].clone();
    assert_eq!(new.len(), 64);
    assert_ne!(new, TOKEN);
    for n in ["ha-s2", "ha-s3"] {
        assert_eq!(stdin_of(&f, n, "agent-token"), [new.clone()]);
    }
    assert_eq!(stdin_of(&f, "ha-a1", "token"), [new.clone()], "the agent's own token file gets the same value");
    assert!(no_script_contains(&f, &new));
    let j = std::fs::read_to_string(s.path("ha")).unwrap();
    assert!(!j.contains(&new));
    assert_eq!(s.require("ha").unwrap().agent_secret_rotated.as_deref(), Some("20261121-101500"));
    assert_eq!(s.require("ha").unwrap().server_secret_rotated, None);
}

#[tokio::test]
async fn a_server_that_does_not_come_back_stops_the_agent_rotation_before_the_agents() {
    let (_d, s, f) = setup();
    ha_with_agent(&f, &s).await;
    f.fail_on("ha-s3:restart", "boom");
    let e = format!("{:#}", ops::token_rotate(&f, &s, "ha", &RotateReq { agent: true, server: false }).await.unwrap_err());
    assert!(e.contains("run rotate again"), "{e}");
    assert_eq!(count(&f, "ssh ha-a1"), 0, "agents are untouched when a server failed");
    assert_eq!(s.require("ha").unwrap().agent_secret_rotated, None);
}

#[tokio::test]
async fn rotating_the_server_token_uses_stdin_and_the_environment_never_an_argument() {
    let (_d, s, f) = setup();
    ha_with_agent(&f, &s).await;
    let r = ops::token_rotate(&f, &s, "ha", &RotateReq { agent: false, server: true }).await.unwrap();
    assert!(r.server_secret_rotated && !r.agent_secret_rotated);
    let rot = f.rotated.borrow().clone().unwrap();
    let mut it = rot.lines();
    assert_eq!(it.next(), Some(TOKEN), "the old token, read from the server");
    let new = it.next().unwrap().to_owned();
    assert_ne!(new, TOKEN);
    assert!(no_script_contains(&f, TOKEN) && no_script_contains(&f, &new));
    for n in ["ha", "ha-s2", "ha-s3"] {
        assert_eq!(stdin_of(&f, n, "write-server-secret"), [new.clone()]);
    }
    let l = f.lines();
    assert!(pos(&l, "rotate-server-secret") < pos(&l, "ssh ha write-server-secret"));
    assert!(pos(&l, "ssh ha restart") < pos(&l, "ssh ha-s2 write-server-secret"));
    assert_eq!(r.server_secret_confirmed_in_k3s_dir, Some(true));
    assert!(r.notes.iter().any(|n| n.contains("OLD server token")));
    assert!(!std::fs::read_to_string(s.path("ha")).unwrap().contains(&new));
    assert!(s.require("ha").unwrap().server_secret_rotated.is_some());
}

#[tokio::test]
async fn a_failed_server_token_rotation_changes_nothing_and_leaks_nothing() {
    let (_d, s, f) = setup();
    ha_with_agent(&f, &s).await;
    f.leak_stdin.set(true);
    f.fail_on("ha:rotate-server-secret", "k3s said no");
    let e = format!("{:#}", ops::token_rotate(&f, &s, "ha", &RotateReq { agent: false, server: true }).await.unwrap_err());
    assert!(e.contains("nothing was changed") && !e.contains(TOKEN), "{e}");
    assert_eq!(count(&f, "write-server-secret"), 0);
    assert_eq!(s.require("ha").unwrap().server_secret_rotated, None);
    let none = ops::token_rotate(&f, &s, "ha", &RotateReq { agent: false, server: false }).await;
    assert!(none.unwrap_err().to_string().contains("say what to rotate"));
}

// ---- node removal ---------------------------------------------------------------------------------------------------

fn remove_req(node: &str) -> RemoveReq {
    RemoveReq { node: node.into(), force: false, yes: true, no_drain: false }
}

#[tokio::test]
async fn removing_an_agent_cordons_drains_stops_deletes_the_node_then_the_vm() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.clear();
    let r = ops::remove_node(&f, &s, "dev", &remove_req("dev-a1")).await.unwrap();
    let l = f.lines();
    let order = ["ssh dev cordon", "ssh dev drain", "ssh dev-a1 disable-unit", "ssh dev delete-node", "power Stop k3s-dev-a1", "power Delete k3s-dev-a1"];
    for w in order.windows(2) {
        assert!(pos(&l, w[0]) < pos(&l, w[1]), "{} before {} in {l:#?}", w[0], w[1]);
    }
    assert_eq!(r.role, "agent");
    let st = s.require("dev").unwrap();
    assert!(st.node("dev-a1").is_none());
    assert_eq!(st.servers_wanted, 1);
}

#[tokio::test]
async fn removing_a_server_checks_quorum_and_deletes_the_node_so_k3s_drops_the_etcd_member() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    *f.etcd_members.borrow_mut() = vec!["ha".into(), "ha-s2".into()];
    f.member_log.set(true);
    f.clear();
    let r = ops::remove_node(&f, &s, "ha", &remove_req("ha-s3")).await.unwrap();
    let l = f.lines();
    let order = ["ssh ha cordon", "ssh ha drain", "ssh ha-s3 disable-unit", "ssh ha delete-node", "etcd-member-log", "ssh ha etcd-members", "power Delete k3s-ha-s3"];
    for w in order.windows(2) {
        assert!(pos(&l, w[0]) < pos(&l, w[1]), "{} before {} in {l:#?}", w[0], w[1]);
    }
    // Every cluster command went to a server other than the one being removed.
    assert!(!l.iter().any(|x| x.starts_with("ssh ha-s3 ") && !x.contains("disable-unit")));
    assert_eq!(r.servers_left, 2);
    assert!(r.etcd_evidence[0].contains("Removing member"));
    assert!(r.notes.iter().any(|n| n.contains("no longer survives")));
    let st = s.require("ha").unwrap();
    assert_eq!(st.servers_wanted, 2);
    assert!(st.node("ha-s3").is_none());
}

#[tokio::test]
async fn removing_a_server_is_refused_when_etcd_could_not_change_its_membership() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    set_ready(&f, "ha-s2", false);
    set_ready(&f, "ha-s3", false);
    f.clear();
    let e = ops::remove_node(&f, &s, "ha", &remove_req("ha")).await.unwrap_err().to_string();
    assert!(e.contains("etcd needs 2") && e.contains("nothing was changed"), "{e}");
    assert_eq!(count(&f, "power "), 0);
    assert!(s.require("ha").unwrap().node("ha").is_some());
}

#[tokio::test]
async fn the_only_server_is_never_removed_and_confirmation_and_names_are_required() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    let e = |r: Result<ops::RemoveReport>| r.unwrap_err().to_string();
    assert!(e(ops::remove_node(&f, &s, "dev", &remove_req("dev")).await).contains("only server"));
    let mut no = remove_req("dev-a1");
    no.yes = false;
    assert!(e(ops::remove_node(&f, &s, "dev", &no).await).contains("--yes"));
    assert!(e(ops::remove_node(&f, &s, "dev", &remove_req("ghost")).await).contains("no node ghost"));
    assert!(s.require("dev").unwrap().node("dev-a1").is_some());
}

#[tokio::test]
async fn a_failing_drain_stops_the_removal_unless_forced() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    f.fail_on("dev:drain", "pods stuck");
    let e = format!("{:#}", ops::remove_node(&f, &s, "dev", &remove_req("dev-a1")).await.unwrap_err());
    assert!(e.contains("drain failed") && e.contains("uncordon"), "{e}");
    assert_eq!(count(&f, "power Delete"), 0);
    assert!(s.require("dev").unwrap().node("dev-a1").is_some());
    let mut force = remove_req("dev-a1");
    force.force = true;
    let r = ops::remove_node(&f, &s, "dev", &force).await.unwrap();
    assert!(r.steps.iter().any(|x| x.contains("drain failed, ignored")));
    assert!(s.require("dev").unwrap().node("dev-a1").is_none());
}

#[tokio::test]
async fn a_dead_node_is_removed_without_touching_it() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    stop_vm(&f, "k3s-ha-s3");
    set_ready(&f, "ha-s3", false);
    f.clear();
    let r = ops::remove_node(&f, &s, "ha", &remove_req("ha-s3")).await.unwrap();
    assert!(r.steps.iter().any(|x| x.contains("not running: cordon and drain skipped")));
    assert_eq!(count(&f, "ssh ha-s3"), 0);
    assert_eq!(count(&f, "ssh ha delete-node"), 1);
    assert_eq!(count(&f, "power Delete k3s-ha-s3"), 1);
    assert!(s.require("ha").unwrap().node("ha-s3").is_none());
}

#[tokio::test]
async fn a_removal_that_stopped_half_way_is_finished_by_running_it_again() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    // The node object is already gone (an earlier run got that far); the VM and the journal entry are not.
    f.nodes.borrow_mut().retain(|n| n.name != "dev-a1");
    let r = ops::remove_node(&f, &s, "dev", &remove_req("dev-a1")).await.unwrap();
    assert!(r.steps.iter().any(|x| x.contains("already gone")));
    assert!(r.steps.iter().any(|x| x == "VM deleted"));
    assert!(s.require("dev").unwrap().node("dev-a1").is_none());
}

// ---- rebuild of a lost journal -------------------------------------------------------------------------------------

async fn lose_the_journal(f: &Fake, s: &Store, name: &str) -> State {
    let before = s.require(name).unwrap();
    s.remove(name);
    assert!(s.load(name).unwrap().is_none());
    f.clear();
    before
}

fn rebuild_req(accept: bool) -> RebuildReq {
    RebuildReq { user: "mini".into(), api_port: 16443, params: params(), accept_host_keys: accept }
}

#[tokio::test]
async fn rebuild_without_accepting_host_keys_only_says_what_it_would_do() {
    let (_d, s, f) = setup();
    cluster_with_agent(&f, &s).await;
    lose_the_journal(&f, &s, "dev").await;
    let r = ops::rebuild(&f, &s, "dev", &rebuild_req(false)).await.unwrap();
    assert!(!r.written && r.owner_recovered);
    assert_eq!(r.nodes.len(), 2);
    assert!(r.next.contains("--accept-host-keys"));
    assert!(s.load("dev").unwrap().is_none(), "nothing is written without the explicit acceptance");
    assert_eq!(count(&f, "scan "), 0);
    assert!(r.not_recovered.iter().any(|x| x.contains("host keys")));
}

#[tokio::test]
async fn rebuild_restores_what_labels_and_guests_say_and_lists_what_it_cannot() {
    let (_d, s, f) = setup();
    let mut req = ha_req("ha", 3);
    req.tls_sans = vec!["cluster.example".into()];
    req.flags.disable = vec!["servicelb".into()];
    create(&f, &s, &req).await.unwrap();
    join(&f, &s, &agent_req("ha")).await.unwrap();
    s.require("ha").unwrap();
    ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(false)), None)).await.unwrap();
    *f.snapshot_config.borrow_mut() = "etcd-snapshot-schedule-cron: \"0 */6 * * *\"\netcd-snapshot-retention: 10\netcd-snapshot-compress: true\n".into();
    let before = lose_the_journal(&f, &s, "ha").await;
    stop_vm(&f, "k3s-ha-s2");
    let r = ops::rebuild(&f, &s, "ha", &rebuild_req(true)).await.unwrap();
    assert!(r.written);
    let st = s.require("ha").unwrap();
    assert_eq!(st.owner, before.owner, "the owner id comes back from the labels");
    for n in &before.nodes {
        let got = st.node(&n.name).unwrap();
        assert_eq!((got.role, got.id.clone(), got.vm_name.clone()), (n.role, n.id.clone(), n.vm_name.clone()));
        assert_eq!(got.host_key, format!("ssh-ed25519 SCANNED-{}", n.name), "re-pinned from the guest, not recovered");
        assert!(got.joined);
    }
    assert_eq!(st.nodes[0].name, "ha", "the first server stays first");
    assert_eq!(st.datastore, Datastore::Etcd);
    assert_eq!(st.servers_wanted, 3);
    assert_eq!(st.tls_sans, ["cluster.example"]);
    assert_eq!(st.flags.disable, ["servicelb"]);
    assert_eq!(st.endpoint, Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }));
    assert_eq!(st.snapshots, Some(policy(false)));
    assert_eq!(st.k3s_version.as_deref(), Some("v1.31.4+k3s1"));
    assert_eq!(st.phase, Phase::Ready);
    assert_eq!(st.canary, None);
    assert!(f.lines().iter().any(|l| l == "power Start k3s-ha-s2"), "a stopped guest is started to be read");
    for lost in ["host keys", "SHA-256", "VM sizing", "canary", "LaunchAgent", "never adopted"] {
        assert!(r.not_recovered.iter().any(|x| x.contains(lost)), "{lost} missing in {:#?}", r.not_recovered);
    }
    assert!(status(&f, &s, "ha").await.unwrap().healthy);
}

#[tokio::test]
async fn rebuild_never_adopts_unlabelled_vms_and_refuses_ambiguous_owners_or_an_existing_journal() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("dev")).await.unwrap();
    // A VM that merely has a plausible name, and one that is labelled for the cluster by someone else.
    f.add_vm("k3s-dev-a9", "running", None);
    let e = |r: Result<ops::RebuildReport>| r.unwrap_err().to_string();
    assert!(e(ops::rebuild(&f, &s, "dev", &rebuild_req(true)).await).contains("recover"));
    lose_the_journal(&f, &s, "dev").await;
    let plan = ops::rebuild(&f, &s, "dev", &rebuild_req(false)).await.unwrap();
    assert!(!plan.nodes.iter().any(|n| n.contains("dev-a9")));
    f.add_vm("k3s-dev-a2", "running", Some(("dev", Role::Agent, "dev-a2", "someone-else")));
    assert!(e(ops::rebuild(&f, &s, "dev", &rebuild_req(true)).await).contains("different owner"));
    assert!(s.load("dev").unwrap().is_none());
    assert!(e(ops::rebuild(&f, &s, "nothing-here", &rebuild_req(true)).await).contains("nothing to rebuild from"));
}

// ---- doctor ----------------------------------------------------------------------------------------------------------

fn level(r: &doctor::DoctorReport, name: &str) -> Level {
    r.findings.iter().find(|x| x.name == name).unwrap_or_else(|| panic!("{name} in {:#?}", r.findings)).level
}

#[tokio::test]
async fn doctor_passes_a_healthy_multi_server_cluster_and_names_what_to_improve() {
    let (_d, s, f) = setup();
    ha_cluster(&f, &s).await;
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert!(r.ok, "{:#?}", r.findings);
    assert_eq!(level(&r, "quorum"), Level::Ok);
    assert_eq!(level(&r, "server-flags"), Level::Ok);
    assert_eq!(level(&r, "snapshot-schedule"), Level::Warn, "no schedule of its own yet");
    assert_eq!(level(&r, "endpoint"), Level::Warn, "the fake host has no forwarder running");
    ops::snapshot_schedule(&f, &s, "ha", ScheduleAction::Set(Box::new(policy(true)), Some(s3_secret()))).await.unwrap();
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert_eq!(level(&r, "snapshot-schedule"), Level::Ok);
    assert!(r.findings.iter().all(|x| x.name != "snapshot-offsite"));
    assert!(doctor::render(&r).contains("endpoint"));
}

#[tokio::test]
async fn doctor_fails_on_drifted_flags_a_stopped_vm_lost_quorum_and_stale_snapshots() {
    let (_d, s, f) = setup();
    let mut req = ha_req("ha", 3);
    req.flags.cluster_cidr = Some("10.50.0.0/16".into());
    create(&f, &s, &req).await.unwrap();
    // Someone edited one server's unit by hand.
    let e = f.exec.borrow()["ha-s2"].replace("10.50.0.0/16", "10.60.0.0/16");
    f.exec.borrow_mut().insert("ha-s2".into(), e);
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert!(!r.ok);
    assert_eq!(level(&r, "server-flags"), Level::Fail);
    assert!(r.findings.iter().find(|x| x.name == "server-flags").unwrap().detail.contains("ha-s2: --cluster-cidr"));
    let e = f.exec.borrow()["ha"].replace("--cluster-init", "");
    f.exec.borrow_mut().insert("ha-s2".into(), e);
    stop_vm(&f, "k3s-ha-s3");
    set_ready(&f, "ha-s3", false);
    set_ready(&f, "ha-s2", false);
    f.newest_age.set(Some(9 * 24 * 3600));
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert_eq!(level(&r, "vms"), Level::Fail);
    assert_eq!(level(&r, "quorum"), Level::Fail);
    assert_eq!(level(&r, "snapshot-age"), Level::Fail);
    assert!(doctor::render(&r).contains("fix:"));
}

#[tokio::test]
async fn doctor_checks_kube_vip_on_every_server_and_the_tunnel_agent() {
    let (_d, s, f) = setup();
    let mut req = ha_req("ha", 3);
    req.endpoint = Some(EndpointKind::KubeVip);
    req.vip = Some("10.89.0.250".into());
    create(&f, &s, &req).await.unwrap();
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert_eq!(level(&r, "endpoint"), Level::Ok);
    assert_eq!(level(&r, "tunnel-agent"), Level::Ok);
    f.vip.borrow_mut().retain(|n| n != "ha-s3");
    let r = doctor::doctor(&f, &s, "ha").await.unwrap();
    assert_eq!(level(&r, "endpoint"), Level::Fail);
    assert!(!r.ok);
}

#[tokio::test]
async fn doctor_leaves_sqlite_clusters_without_etcd_checks() {
    let (_d, s, f) = setup();
    create(&f, &s, &create_req("lite")).await.unwrap();
    let r = doctor::doctor(&f, &s, "lite").await.unwrap();
    assert!(r.ok);
    assert!(!r.findings.iter().any(|x| x.name.starts_with("snapshot") || x.name == "quorum"));
}
