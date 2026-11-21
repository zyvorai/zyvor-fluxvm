// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl kube apple doctor NAME`: the checks that matter for a long-lived, possibly multi-server cluster, each with the
//! command that fixes it. Facts are gathered through [`Host`]; the verdicts are pure functions of those facts.

use super::host::Host;
use super::lifecycle::{sh, sh_server};
use super::model::*;
use super::script::{self};
use super::script_ha as sha;
use anyhow::Result;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: String,
}

fn f(name: &'static str, level: Level, detail: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding { name, level, detail: detail.into(), fix: fix.into() }
}

#[derive(Serialize, Debug)]
pub struct DoctorReport {
    pub cluster: String,
    pub ok: bool,
    pub findings: Vec<Finding>,
}

/// What was read from the Mac and the guests.
#[derive(Debug, Default)]
pub struct Facts {
    pub cluster: String,
    pub datastore: Option<Datastore>,
    pub servers_wanted: usize,
    pub servers_joined: usize,
    /// Servers that answered the node list as Ready / not Ready; `None` when no server could be asked.
    pub servers_ready: Option<usize>,
    pub vms_not_running: Vec<String>,
    pub flag_drift: Vec<String>,
    pub unreadable_units: Vec<String>,
    pub endpoint: Option<EndpointCfg>,
    /// Servers that lack the kube-vip manifest (kube-vip clusters only).
    pub kube_vip_missing: Vec<String>,
    pub tunnel_up: bool,
    /// `(installed, loaded)` of the tunnel LaunchAgent.
    pub agent: Option<(bool, bool)>,
    pub kubeconfig: bool,
    pub snapshot_policy: Option<SnapshotPolicy>,
    /// Age in seconds of the newest snapshot on any server; `Some(None)` = asked, found none.
    pub snapshot_age: Option<Option<u64>>,
    /// Days since the secrets were rotated; `None` = never.
    pub agent_secret_days: Option<i64>,
    pub server_secret_days: Option<i64>,
}

const HOUR: u64 = 3600;

/// Days from a `YYYYMMDD-HHMMSS` stamp to `now`.
pub fn days_since(stamp: &str, now: &str) -> Option<i64> {
    let p = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M%S").ok();
    Some((p(now)? - p(stamp)?).num_days())
}

pub fn verdicts(x: &Facts) -> Vec<Finding> {
    let mut v = vec![];
    let c = &x.cluster;
    // servers and quorum
    if x.servers_wanted > 1 || x.servers_joined > 1 {
        if x.servers_joined < x.servers_wanted {
            v.push(f(
                "servers",
                Level::Fail,
                format!("{} of {} servers have joined", x.servers_joined, x.servers_wanted),
                format!("fluxctl kube apple create {c} --servers {} --embedded-etcd ... (the same command resumes)", x.servers_wanted),
            ));
        } else {
            v.push(f("servers", Level::Ok, format!("{} servers", x.servers_joined), ""));
        }
        if x.servers_joined % 2 == 0 {
            v.push(f(
                "server-count",
                Level::Warn,
                format!("{} servers: an even number tolerates no more failures than {}", x.servers_joined, x.servers_joined - 1),
                "use 3 or 5 servers",
            ));
        }
        match x.servers_ready {
            None => v.push(f("quorum", Level::Fail, "no server could be asked", format!("fluxctl kube apple start {c}"))),
            Some(r) => {
                let need = sha::majority(x.servers_joined);
                if r < need {
                    v.push(f("quorum", Level::Fail, format!("{r} of {} servers Ready, etcd needs {need}", x.servers_joined), format!("fluxctl kube apple start {c}; fluxctl kube apple logs {c} --node NODE")));
                } else if r < x.servers_joined {
                    v.push(f("quorum", Level::Warn, format!("{r} of {} servers Ready: one more failure loses quorum", x.servers_joined), format!("fluxctl kube apple status {c}")));
                } else {
                    v.push(f("quorum", Level::Ok, format!("all {r} servers Ready"), ""));
                }
            }
        }
    }
    // guests
    if x.vms_not_running.is_empty() {
        v.push(f("vms", Level::Ok, "every node's VM is running", ""));
    } else {
        v.push(f("vms", Level::Fail, format!("not running: {}", x.vms_not_running.join(", ")), format!("fluxctl kube apple start {c}; or `recover {c}` if a VM is gone")));
    }
    // flags
    if !x.unreadable_units.is_empty() {
        v.push(f("server-flags", Level::Warn, format!("could not read the unit of: {}", x.unreadable_units.join(", ")), "fluxctl kube apple status"));
    }
    if x.flag_drift.is_empty() {
        if x.servers_joined > 1 {
            v.push(f("server-flags", Level::Ok, "every server runs the shared flags the journal records", ""));
        }
    } else {
        v.push(f("server-flags", Level::Fail, x.flag_drift.join("; "), "make the unit on that server match, then restart k3s there; k3s needs these flags equal on every server"));
    }
    // endpoint
    if let Some(e) = &x.endpoint {
        match e.kind {
            EndpointKind::Proxy => {
                if x.tunnel_up {
                    v.push(f("endpoint", Level::Ok, "the forwarder answers on 127.0.0.1", ""));
                } else {
                    v.push(f("endpoint", Level::Warn, "nothing answers on the API port: the forwarder is not running", format!("fluxctl kube apple tunnel {c} --install-agent (keeps it up), or `tunnel {c}`")));
                }
            }
            EndpointKind::KubeVip => {
                if x.kube_vip_missing.is_empty() {
                    v.push(f("endpoint", Level::Ok, format!("kube-vip manifest on every server (address {})", e.vip.as_deref().unwrap_or("unknown")), ""));
                } else {
                    v.push(f("endpoint", Level::Fail, format!("no kube-vip manifest on: {}", x.kube_vip_missing.join(", ")), format!("fluxctl kube apple join {c} --role server  (or write the manifest again)")));
                }
            }
        }
    }
    match x.agent {
        Some((true, true)) => v.push(f("tunnel-agent", Level::Ok, "the LaunchAgent keeps the tunnel up", "")),
        Some((true, false)) => v.push(f("tunnel-agent", Level::Fail, "the LaunchAgent is installed but not loaded", format!("fluxctl kube apple tunnel {c} --install-agent"))),
        Some((false, _)) => v.push(f("tunnel-agent", Level::Warn, "no LaunchAgent: the tunnel ends with the terminal or a reboot", format!("fluxctl kube apple tunnel {c} --install-agent"))),
        None => {}
    }
    v.push(if x.kubeconfig {
        f("kubeconfig", Level::Ok, "present", "")
    } else {
        f("kubeconfig", Level::Warn, "the kubeconfig file is missing", format!("fluxctl kube apple kubeconfig {c}"))
    });
    // snapshots (embedded etcd only)
    if x.datastore == Some(Datastore::Etcd) {
        match (&x.snapshot_policy, x.snapshot_age) {
            (None, _) => v.push(f("snapshot-schedule", Level::Warn, "no schedule of your own: k3s's default (every 12 hours, keep 5, local) applies", format!("fluxctl kube apple snapshot schedule {c} --cron '0 */6 * * *' --retention 20"))),
            (Some(p), _) => {
                let dest = p.s3.as_ref().map_or("local only".to_owned(), |s| format!("local and s3://{}", s.bucket));
                v.push(f("snapshot-schedule", Level::Ok, format!("{} keep {} ({dest})", p.cron, p.retention), ""));
                if p.s3.is_none() {
                    v.push(f("snapshot-offsite", Level::Warn, "snapshots stay on the servers' disks: losing the VMs loses them", format!("fluxctl kube apple snapshot schedule {c} ... --s3-bucket B --s3-endpoint E, or `snapshot pull`")));
                }
            }
        }
        match x.snapshot_age {
            None => {}
            Some(None) => v.push(f("snapshot-age", Level::Warn, "no snapshot exists yet", format!("fluxctl kube apple snapshot save {c}"))),
            Some(Some(a)) if a > 7 * 24 * HOUR => v.push(f("snapshot-age", Level::Fail, format!("the newest snapshot is {} days old", a / (24 * HOUR)), format!("fluxctl kube apple snapshot save {c}; check `snapshot ls {c}` and the schedule"))),
            Some(Some(a)) if a > 36 * HOUR => v.push(f("snapshot-age", Level::Warn, format!("the newest snapshot is {} hours old", a / HOUR), format!("fluxctl kube apple snapshot ls {c}"))),
            Some(Some(a)) => v.push(f("snapshot-age", Level::Ok, format!("newest snapshot {} hour(s) old", a / HOUR), "")),
        }
    }
    // secrets
    for (what, days) in [("agent", x.agent_secret_days), ("server", x.server_secret_days)] {
        let name: &'static str = if what == "agent" { "agent-secret" } else { "server-secret" };
        match days {
            None => v.push(f(name, Level::Ok, format!("the {what} token was never rotated (it is the one made at creation)"), format!("fluxctl kube apple token rotate {c} --{what}"))),
            Some(d) if d > 180 => v.push(f(name, Level::Warn, format!("the {what} token was rotated {d} days ago"), format!("fluxctl kube apple token rotate {c} --{what}"))),
            Some(d) => v.push(f(name, Level::Ok, format!("the {what} token was rotated {d} days ago"), "")),
        }
    }
    v
}

pub async fn doctor<H: Host>(h: &H, store: &Store, name: &str) -> Result<DoctorReport> {
    let st = store.require(name)?;
    let mut x = Facts {
        cluster: name.to_owned(),
        datastore: Some(st.datastore),
        servers_wanted: usize::from(st.servers_wanted),
        servers_joined: st.servers().iter().filter(|n| n.joined).count(),
        endpoint: st.endpoint.clone(),
        tunnel_up: h.tunnel_up(&st),
        kubeconfig: h.kubeconfig_exists(&st),
        snapshot_policy: st.snapshots.clone(),
        agent: h
            .tunnel_agent_status(&st)
            .ok()
            .map(|s| (s["installed"].as_bool().unwrap_or(false), s["loaded"].as_bool().unwrap_or(false))),
        ..Default::default()
    };
    let now = h.stamp();
    x.agent_secret_days = st.agent_secret_rotated.as_deref().and_then(|s| days_since(s, &now));
    x.server_secret_days = st.server_secret_rotated.as_deref().and_then(|s| days_since(s, &now));
    for n in &st.nodes {
        match h.find_vm(&n.vm_name).await? {
            Some(v) if v.status == "running" => {}
            _ => x.vms_not_running.push(n.name.clone()),
        }
    }
    let up = |n: &str| !x.vms_not_running.iter().any(|m| m == n);
    if let Ok((_, j)) = sh_server(h, &st, &script::nodes_json_script()).await
        && let Ok(list) = script::parse_nodes(&j)
    {
        x.servers_ready = Some(
            st.servers()
                .iter()
                .filter(|s| list.iter().any(|n| n.name == s.name && n.ready))
                .count(),
        );
    }
    let live: Vec<String> = st.servers().iter().filter(|s| up(&s.name)).map(|s| s.name.clone()).collect();
    for s in &live {
        match sh(h, &st, s, &sha::unit_probe_script(Role::Server)).await.ok().and_then(|t| sha::parse_unit_flags(&t)) {
            Some(u) => x.flag_drift.extend(sha::flags_drift(&st.flags, s, &u.shared)),
            None => x.unreadable_units.push(s.clone()),
        }
        if matches!(st.endpoint.as_ref().map(|e| e.kind), Some(EndpointKind::KubeVip))
            && sh(h, &st, s, &sha::kube_vip_probe_script()).await.ok().and_then(|o| sha::parse_kube_vip_probe(&o)).is_none()
        {
            x.kube_vip_missing.push(s.clone());
        }
        if st.datastore == Datastore::Etcd {
            let age = sh(h, &st, s, &sha::snapshot_age_script()).await.ok().map(|o| sha::parse_snapshot_age(&o));
            x.snapshot_age = match (x.snapshot_age, age) {
                (Some(Some(a)), Some(Some(b))) => Some(Some(a.min(b))),
                (Some(Some(a)), _) | (_, Some(Some(a))) => Some(Some(a)),
                (_, Some(None)) | (Some(None), None) => Some(None),
                (a, None) => a,
            };
        }
    }
    let findings = verdicts(&x);
    Ok(DoctorReport { cluster: name.to_owned(), ok: findings.iter().all(|f| f.level != Level::Fail), findings })
}

/// The text `fluxctl kube apple doctor` prints.
pub fn render(r: &DoctorReport) -> String {
    let mut s = String::new();
    for f in &r.findings {
        let tag = match f.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s.push_str(&format!("{tag}  {:<18} {}\n", f.name, f.detail));
        if !f.fix.is_empty() && f.level != Level::Ok {
            s.push_str(&format!("      fix: {}\n", f.fix));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Facts {
        Facts {
            cluster: "dev".into(),
            datastore: Some(Datastore::Etcd),
            servers_wanted: 3,
            servers_joined: 3,
            servers_ready: Some(3),
            endpoint: Some(EndpointCfg { kind: EndpointKind::Proxy, vip: None }),
            tunnel_up: true,
            agent: Some((true, true)),
            kubeconfig: true,
            snapshot_policy: Some(SnapshotPolicy {
                cron: "0 */6 * * *".into(),
                retention: 10,
                compress: true,
                s3: Some(S3Target { endpoint: "s3.example".into(), bucket: "snaps".into(), region: None, folder: None }),
            }),
            snapshot_age: Some(Some(3 * HOUR)),
            agent_secret_days: Some(10),
            server_secret_days: Some(10),
            ..Default::default()
        }
    }

    fn level_of(v: &[Finding], name: &str) -> Level {
        v.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("{name} missing in {v:#?}")).level
    }

    #[test]
    fn a_healthy_ha_cluster_has_no_warning() {
        let v = verdicts(&healthy());
        assert!(v.iter().all(|f| f.level == Level::Ok), "{v:#?}");
    }

    #[test]
    fn quorum_degraded_and_lost_are_told_apart() {
        let mut x = healthy();
        x.servers_ready = Some(2);
        assert_eq!(level_of(&verdicts(&x), "quorum"), Level::Warn);
        x.servers_ready = Some(1);
        assert_eq!(level_of(&verdicts(&x), "quorum"), Level::Fail);
        x.servers_ready = None;
        assert_eq!(level_of(&verdicts(&x), "quorum"), Level::Fail);
    }

    #[test]
    fn missing_servers_even_counts_and_flag_drift_are_reported() {
        let mut x = healthy();
        x.servers_joined = 2;
        x.servers_ready = Some(2);
        let v = verdicts(&x);
        assert_eq!(level_of(&v, "servers"), Level::Fail);
        assert_eq!(level_of(&v, "server-count"), Level::Warn);
        let mut x = healthy();
        x.flag_drift = vec!["dev-s2: --cluster-cidr is \"x\", the cluster has \"y\"".into()];
        let v = verdicts(&x);
        assert_eq!(level_of(&v, "server-flags"), Level::Fail);
        assert!(v.iter().find(|f| f.name == "server-flags").unwrap().fix.contains("equal"));
    }

    #[test]
    fn the_tunnel_endpoint_and_agent_are_checked() {
        let mut x = healthy();
        x.tunnel_up = false;
        x.agent = Some((false, false));
        let v = verdicts(&x);
        assert_eq!(level_of(&v, "endpoint"), Level::Warn);
        assert_eq!(level_of(&v, "tunnel-agent"), Level::Warn);
        x.agent = Some((true, false));
        assert_eq!(level_of(&verdicts(&x), "tunnel-agent"), Level::Fail);
        x.endpoint = Some(EndpointCfg { kind: EndpointKind::KubeVip, vip: Some("10.89.0.250".into()) });
        x.kube_vip_missing = vec!["dev-s3".into()];
        assert_eq!(level_of(&verdicts(&x), "endpoint"), Level::Fail);
    }

    #[test]
    fn snapshots_age_schedule_and_offsite_are_checked() {
        let mut x = healthy();
        x.snapshot_age = Some(Some(40 * HOUR));
        assert_eq!(level_of(&verdicts(&x), "snapshot-age"), Level::Warn);
        x.snapshot_age = Some(Some(8 * 24 * HOUR));
        assert_eq!(level_of(&verdicts(&x), "snapshot-age"), Level::Fail);
        x.snapshot_age = Some(None);
        assert_eq!(level_of(&verdicts(&x), "snapshot-age"), Level::Warn);
        x.snapshot_policy.as_mut().unwrap().s3 = None;
        assert_eq!(level_of(&verdicts(&x), "snapshot-offsite"), Level::Warn);
        x.snapshot_policy = None;
        assert_eq!(level_of(&verdicts(&x), "snapshot-schedule"), Level::Warn);
        // SQLite clusters have no etcd snapshots to check.
        x.datastore = Some(Datastore::Sqlite);
        assert!(!verdicts(&x).iter().any(|f| f.name.starts_with("snapshot")));
    }

    #[test]
    fn secret_age_is_warned_about_after_half_a_year() {
        let mut x = healthy();
        x.agent_secret_days = Some(200);
        assert_eq!(level_of(&verdicts(&x), "agent-secret"), Level::Warn);
        x.agent_secret_days = None;
        assert_eq!(level_of(&verdicts(&x), "agent-secret"), Level::Ok);
        assert_eq!(days_since("20260101-000000", "20260111-120000"), Some(10));
        assert_eq!(days_since("junk", "20260111-120000"), None);
    }

    #[test]
    fn render_shows_the_fix_only_for_problems() {
        let mut x = healthy();
        x.kubeconfig = false;
        let r = DoctorReport { cluster: "dev".into(), ok: true, findings: verdicts(&x) };
        let t = render(&r);
        assert!(t.contains("warn  kubeconfig"));
        assert_eq!(t.matches("fix:").count(), 1, "{t}");
    }
}
