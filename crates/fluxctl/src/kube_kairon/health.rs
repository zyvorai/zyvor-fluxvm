// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Health checks as pure functions of captured `kubectl ... -o json`, each giving a plain-English line and, when it fails,
//! one fix hint. Tested with fixtures.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: String) -> Self {
        Check {
            name,
            level: Level::Ok,
            detail,
            fix: None,
        }
    }
    fn fail(name: &'static str, detail: String, fix: &str) -> Self {
        Check {
            name,
            level: Level::Fail,
            detail,
            fix: Some(fix.into()),
        }
    }
    fn warn(name: &'static str, detail: String, fix: &str) -> Self {
        Check {
            name,
            level: Level::Warn,
            detail,
            fix: Some(fix.into()),
        }
    }
    fn skip(name: &'static str, detail: &str) -> Self {
        Check {
            name,
            level: Level::Skip,
            detail: detail.into(),
            fix: None,
        }
    }
}

pub fn render(checks: &[Check]) -> String {
    let mut s = String::new();
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
            Level::Skip => "skip",
        };
        s.push_str(&format!("[{tag}] {}: {}\n", c.name, c.detail));
        if let Some(f) = &c.fix {
            s.push_str(&format!("       fix: {f}\n"));
        }
    }
    let bad = checks.iter().filter(|c| c.level == Level::Fail).count();
    if bad == 0 {
        s.push_str("Kairon is healthy.\n");
    } else {
        s.push_str(&format!("{bad} check(s) failed.\n"));
    }
    s
}

pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.level == Level::Fail)
}

fn items(v: &Value) -> Vec<&Value> {
    match v.get("items").and_then(Value::as_array) {
        Some(a) => a.iter().collect(),
        None if v.get("kind").is_some() && v.get("metadata").is_some() => vec![v],
        None => vec![],
    }
}

fn name(v: &Value) -> &str {
    v.pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn cond_true(v: &Value, kind: &str) -> bool {
    v.pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|c| {
            c.iter().any(|x| {
                x.get("type").and_then(Value::as_str) == Some(kind)
                    && x.get("status").and_then(Value::as_str) == Some("True")
            })
        })
}

fn n(v: &Value, p: &str) -> i64 {
    v.pointer(p).and_then(Value::as_i64).unwrap_or(0)
}

/// CRDs that must exist and be Established for anything to work.
pub const CORE_CRDS: [&str; 5] = [
    "machines",
    "machineimages",
    "machinemigrations",
    "machinesnapshots",
    "machineclaims",
];

pub fn crds(json: &str) -> Check {
    const NAME: &str = "CRDs";
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return Check::fail(
            NAME,
            "could not read the CRD list".into(),
            "check the kubeconfig: kubectl get crd",
        );
    };
    let all: Vec<&Value> = items(&v)
        .into_iter()
        .filter(|c| name(c).ends_with(".kairon.zyvor.dev"))
        .collect();
    if all.is_empty() {
        return Check::fail(
            NAME,
            "no kairon.zyvor.dev CRDs are installed".into(),
            "run `fluxctl kube kairon install`",
        );
    }
    let missing: Vec<String> = CORE_CRDS
        .iter()
        .map(|c| format!("{c}.kairon.zyvor.dev"))
        .filter(|full| !all.iter().any(|c| name(c) == full))
        .collect();
    let pending: Vec<&str> = all
        .iter()
        .filter(|c| !cond_true(c, "Established"))
        .map(|c| name(c))
        .collect();
    if !missing.is_empty() {
        return Check::fail(
            NAME,
            format!("missing: {}", missing.join(", ")),
            "run `fluxctl kube kairon upgrade`: it applies the CRDs first (Helm only installs them on the first install)",
        );
    }
    if !pending.is_empty() {
        return Check::fail(
            NAME,
            format!("not Established yet: {}", pending.join(", ")),
            "wait a few seconds and run status again; if it stays, `kubectl describe crd <name>` shows why the API server refused it",
        );
    }
    Check::ok(
        NAME,
        format!("{} Kairon CRDs are installed and Established", all.len()),
    )
}

/// A Deployment that must be fully available. `optional` turns "not found" into a skip.
pub fn deployment(name_: &'static str, json: &str, want: &str, optional: bool) -> Check {
    let found = serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| items(&v).into_iter().find(|d| name(d) == want).cloned());
    let Some(d) = found else {
        return if optional {
            Check::skip(
                name_,
                &format!(
                    "{want} is not installed (turn it on with `fluxctl kube kairon upgrade --ui`)"
                ),
            )
        } else {
            Check::fail(
                name_,
                format!("deployment {want} does not exist"),
                "run `fluxctl kube kairon install`",
            )
        };
    };
    let want_r = n(&d, "/spec/replicas").max(1);
    let ready = n(&d, "/status/readyReplicas");
    if ready >= want_r {
        Check::ok(name_, format!("{want}: {ready} of {want_r} ready"))
    } else {
        Check::fail(
            name_,
            format!("{want}: {ready} of {want_r} ready"),
            &format!(
                "`kubectl -n kairon-system describe deploy {want}` and `kubectl -n kairon-system logs deploy/{want}` say why; image pulls on a new cluster take a minute"
            ),
        )
    }
}

pub fn daemonset(json: &str) -> Check {
    const NAME: &str = "node agent";
    let found = serde_json::from_str::<Value>(json).ok().and_then(|v| {
        items(&v)
            .into_iter()
            .find(|d| name(d) == "kairon-node")
            .cloned()
    });
    let Some(d) = found else {
        return Check::fail(
            NAME,
            "daemonset kairon-node does not exist".into(),
            "run `fluxctl kube kairon install`",
        );
    };
    let want = n(&d, "/status/desiredNumberScheduled");
    let ready = n(&d, "/status/numberReady");
    if want == 0 {
        return Check::fail(
            NAME,
            "kairon-node runs on no node: no node carries kairon.zyvor.dev/capable=true".into(),
            "label the machines that run FluxVM: `fluxctl kube kairon upgrade --node NAME` (a Mac registers itself, see docs/kairon-on-kube.md)",
        );
    }
    if ready >= want {
        Check::ok(NAME, format!("kairon-node: {ready} of {want} ready"))
    } else {
        Check::fail(
            NAME,
            format!("kairon-node: {ready} of {want} ready"),
            "`kubectl -n kairon-system describe pod -l app.kubernetes.io/name=kairon-node`; in the production profile pods wait in ContainerCreating until Secret kairon-migration-tls exists",
        )
    }
}

/// The admission webhook: expected only when the ValidatingWebhookConfiguration exists. `endpoints` is the Endpoints object of
/// Service `kairon-controller-webhook`, `not_after` the Secret's recorded expiry, `now` as RFC 3339.
pub fn webhook(
    vwc: Option<&str>,
    endpoints: Option<&str>,
    not_after: Option<&str>,
    now: &str,
) -> Check {
    const NAME: &str = "admission webhook";
    let Some(vwc) = vwc.and_then(|j| serde_json::from_str::<Value>(j).ok()) else {
        return Check::skip(NAME, "off (evaluation profile)");
    };
    let has_ca = vwc
        .pointer("/webhooks")
        .and_then(Value::as_array)
        .is_some_and(|w| {
            !w.is_empty()
                && w.iter().all(|x| {
                    x.pointer("/clientConfig/caBundle")
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                })
        });
    if !has_ca {
        return Check::fail(
            NAME,
            "the webhook has no CA bundle, so the API server cannot trust it".into(),
            "run `fluxctl kube kairon upgrade --production`",
        );
    }
    let ready = endpoints
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|e| e.get("subsets").and_then(Value::as_array).cloned())
        .map(|s| {
            s.iter().any(|x| {
                x.get("addresses")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
            })
        })
        .unwrap_or(false);
    if !ready {
        return Check::fail(
            NAME,
            "no controller pod is answering behind Service kairon-controller-webhook; with failurePolicy Fail every Machine write is rejected".into(),
            "check the controller: `kubectl -n kairon-system get pods` and that Secret kairon-webhook-tls exists",
        );
    }
    if let Some(na) = not_after {
        // RFC 3339 in UTC compares correctly as text.
        if na < now {
            return Check::fail(
                NAME,
                format!(
                    "the webhook certificate expired on {na}; every Machine write is being rejected"
                ),
                "run `fluxctl kube kairon upgrade --production --rotate-certs`",
            );
        }
        return Check::ok(
            NAME,
            format!(
                "a controller pod answers behind the webhook Service; certificate valid until {na}"
            ),
        );
    }
    Check::warn(
        NAME,
        "a controller pod answers behind the webhook Service; the certificate's expiry is unknown (it was not made by fluxctl)".into(),
        "note its expiry yourself: an expired certificate blocks every Machine write",
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KaironNode {
    pub name: String,
    pub ready: bool,
    pub control_plane: bool,
    pub os: String,
    pub backends: Vec<String>,
}

pub fn kairon_nodes(json: &str) -> Vec<KaironNode> {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return vec![];
    };
    let mut out: Vec<KaironNode> = items(&v)
        .into_iter()
        .filter(|nd| {
            nd.pointer("/metadata/labels")
                .and_then(|l| l.get("kairon.zyvor.dev/capable"))
                .and_then(Value::as_str)
                == Some("true")
        })
        .map(|nd| {
            let labels = nd.pointer("/metadata/labels").and_then(Value::as_object);
            let has = |k: &str| labels.is_some_and(|l| l.contains_key(k));
            KaironNode {
                name: name(nd).into(),
                ready: cond_true(nd, "Ready"),
                control_plane: has("node-role.kubernetes.io/control-plane")
                    || has("node-role.kubernetes.io/master"),
                os: labels
                    .and_then(|l| l.get("kubernetes.io/os"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                backends: labels
                    .map(|l| {
                        let mut b: Vec<String> = l
                            .iter()
                            .filter_map(|(k, v)| {
                                k.strip_prefix("kairon.zyvor.dev/backend.")
                                    .filter(|_| v.as_str() == Some("true"))
                                    .map(str::to_string)
                            })
                            .collect();
                        b.sort();
                        b
                    })
                    .unwrap_or_default(),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn nodes(list: &[KaironNode]) -> Check {
    const NAME: &str = "Kairon nodes";
    if list.is_empty() {
        return Check::fail(
            NAME,
            "no node is a Kairon node yet (label kairon.zyvor.dev/capable=true)".into(),
            "label a FluxVM host: `fluxctl kube kairon upgrade --node NAME`. The cluster's own k3s VM is the control plane; VMs run on FluxVM hosts that joined it, or on a Mac whose kairon-node registers itself",
        );
    }
    let notready: Vec<&str> = list
        .iter()
        .filter(|x| !x.ready)
        .map(|x| x.name.as_str())
        .collect();
    let desc = list
        .iter()
        .map(|x| {
            let mut s = x.name.clone();
            if !x.os.is_empty() {
                s.push_str(&format!(" ({}", x.os));
                if !x.backends.is_empty() {
                    s.push_str(&format!(", {}", x.backends.join("+")));
                }
                s.push(')');
            }
            s
        })
        .collect::<Vec<_>>()
        .join(", ");
    if notready.is_empty() {
        Check::ok(NAME, format!("{} Ready: {desc}", list.len()))
    } else {
        Check::fail(
            NAME,
            format!("not Ready: {}; all: {desc}", notready.join(", ")),
            "on that machine check the kubelet (or `kairon-node` on a Mac) and `kubectl describe node <name>`",
        )
    }
}

/// What `selftest` can do with the registered nodes.
#[derive(Debug, PartialEq, Eq)]
pub enum SelftestPlan {
    Run { hosts: Vec<String> },
    NoNodes,
    OnlyControlPlane { names: Vec<String> },
    NoneReady { names: Vec<String> },
}

/// FluxVM hosts are the Ready Kairon nodes that are not the control plane (a k3s server VM has no FluxVM daemon of its own).
pub fn plan_selftest(list: &[KaironNode]) -> SelftestPlan {
    if list.is_empty() {
        return SelftestPlan::NoNodes;
    }
    let hosts: Vec<String> = list
        .iter()
        .filter(|x| x.ready && !x.control_plane)
        .map(|x| x.name.clone())
        .collect();
    if !hosts.is_empty() {
        return SelftestPlan::Run { hosts };
    }
    if list.iter().all(|x| x.control_plane) {
        return SelftestPlan::OnlyControlPlane {
            names: list.iter().map(|x| x.name.clone()).collect(),
        };
    }
    SelftestPlan::NoneReady {
        names: list.iter().map(|x| x.name.clone()).collect(),
    }
}

pub fn selftest_explanation(p: &SelftestPlan) -> String {
    const HOW: &str = "Kairon runs VMs on FluxVM hosts. Register one: join a Linux/KVM host running the FluxVM daemon to this cluster as a k3s agent \
        (or run `kairon-node` on a Mac, see `fluxctl node-agent install`), label it with `fluxctl kube kairon upgrade --node NAME`, then run selftest again.";
    match p {
        SelftestPlan::Run { hosts } => format!(
            "running the self-test against FluxVM host(s): {}",
            hosts.join(", ")
        ),
        SelftestPlan::NoNodes => {
            format!("no Kairon node is registered, so there is nothing to run a VM on. {HOW}")
        }
        SelftestPlan::OnlyControlPlane { names } => format!(
            "the only Kairon node(s) ({}) are control-plane nodes, which have no FluxVM daemon. {HOW}",
            names.join(", ")
        ),
        SelftestPlan::NoneReady { names } => format!(
            "Kairon node(s) {} exist but none is Ready; fix them first (`fluxctl kube kairon status`)",
            names.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRD_OK: &str = r#"{"items":[
      {"metadata":{"name":"machines.kairon.zyvor.dev"},"status":{"conditions":[{"type":"Established","status":"True"}]}},
      {"metadata":{"name":"machineimages.kairon.zyvor.dev"},"status":{"conditions":[{"type":"Established","status":"True"}]}},
      {"metadata":{"name":"machinemigrations.kairon.zyvor.dev"},"status":{"conditions":[{"type":"Established","status":"True"}]}},
      {"metadata":{"name":"machinesnapshots.kairon.zyvor.dev"},"status":{"conditions":[{"type":"Established","status":"True"}]}},
      {"metadata":{"name":"machineclaims.kairon.zyvor.dev"},"status":{"conditions":[{"type":"Established","status":"True"}]}},
      {"metadata":{"name":"ingressroutes.traefik.io"},"status":{}}]}"#;

    #[test]
    fn crds_ok_ignores_other_groups() {
        let c = crds(CRD_OK);
        assert_eq!(c.level, Level::Ok);
        assert!(c.detail.starts_with("5 Kairon CRDs"));
    }

    #[test]
    fn crds_missing_one_says_upgrade() {
        let j = CRD_OK.replace("machineclaims.kairon.zyvor.dev", "other.kairon.zyvor.dev");
        let c = crds(&j);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("machineclaims.kairon.zyvor.dev"));
        assert!(c.fix.unwrap().contains("upgrade"));
    }

    #[test]
    fn crds_not_established_and_none() {
        let j = CRD_OK.replacen("\"True\"", "\"False\"", 1);
        assert!(
            crds(&j)
                .detail
                .contains("not Established yet: machines.kairon.zyvor.dev")
        );
        assert_eq!(crds(r#"{"items":[]}"#).level, Level::Fail);
        assert_eq!(crds("not json").level, Level::Fail);
    }

    const DEPLOY: &str = r#"{"items":[{"metadata":{"name":"kairon-controller"},"spec":{"replicas":1},"status":{"readyReplicas":1}},
      {"metadata":{"name":"kairon-ui"},"spec":{"replicas":2},"status":{"readyReplicas":1}}]}"#;

    #[test]
    fn deployments_ready_notready_and_optional() {
        assert_eq!(
            deployment("controller", DEPLOY, "kairon-controller", false).level,
            Level::Ok
        );
        let ui = deployment("dashboard", DEPLOY, "kairon-ui", true);
        assert_eq!(ui.level, Level::Fail);
        assert_eq!(ui.detail, "kairon-ui: 1 of 2 ready");
        assert_eq!(
            deployment("dashboard", r#"{"items":[]}"#, "kairon-ui", true).level,
            Level::Skip
        );
        assert_eq!(
            deployment("controller", r#"{"items":[]}"#, "kairon-controller", false).level,
            Level::Fail
        );
    }

    #[test]
    fn daemonset_cases() {
        let ds = |d: i64, r: i64| {
            format!(
                r#"{{"items":[{{"metadata":{{"name":"kairon-node"}},"status":{{"desiredNumberScheduled":{d},"numberReady":{r}}}}}]}}"#
            )
        };
        assert_eq!(daemonset(&ds(2, 2)).level, Level::Ok);
        let c = daemonset(&ds(2, 1));
        assert_eq!(c.level, Level::Fail);
        assert!(c.fix.unwrap().contains("kairon-migration-tls"));
        let none = daemonset(&ds(0, 0));
        assert!(none.detail.contains("no node carries"));
        assert_eq!(daemonset(r#"{"items":[]}"#).level, Level::Fail);
    }

    const VWC: &str = r#"{"webhooks":[{"clientConfig":{"caBundle":"Q0E="}}]}"#;
    const EP: &str = r#"{"subsets":[{"addresses":[{"ip":"10.42.0.9"}]}]}"#;

    #[test]
    fn webhook_cases() {
        let now = "2026-11-22T00:00:00Z";
        assert_eq!(webhook(None, None, None, now).level, Level::Skip);
        assert_eq!(
            webhook(Some(VWC), Some(EP), Some("2027-01-01T00:00:00Z"), now).level,
            Level::Ok
        );
        assert_eq!(webhook(Some(VWC), Some(EP), None, now).level, Level::Warn);
        let expired = webhook(Some(VWC), Some(EP), Some("2026-01-01T00:00:00Z"), now);
        assert_eq!(expired.level, Level::Fail);
        assert!(expired.fix.unwrap().contains("--rotate-certs"));
        assert_eq!(
            webhook(Some(VWC), Some(r#"{"subsets":[]}"#), None, now).level,
            Level::Fail
        );
        assert_eq!(
            webhook(
                Some(r#"{"webhooks":[{"clientConfig":{}}]}"#),
                Some(EP),
                None,
                now
            )
            .level,
            Level::Fail
        );
    }

    const NODES: &str = r#"{"items":[
     {"metadata":{"name":"studio-1","labels":{"kairon.zyvor.dev/capable":"true","kairon.zyvor.dev/backend.vz":"true","kubernetes.io/os":"darwin"}},
      "status":{"conditions":[{"type":"Ready","status":"True"}]}},
     {"metadata":{"name":"fluxvm-kube","labels":{"kairon.zyvor.dev/capable":"true","node-role.kubernetes.io/control-plane":"true","kubernetes.io/os":"linux"}},
      "status":{"conditions":[{"type":"Ready","status":"True"}]}},
     {"metadata":{"name":"plain","labels":{}},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
     {"metadata":{"name":"kvm-1","labels":{"kairon.zyvor.dev/capable":"true","kubernetes.io/os":"linux"}},
      "status":{"conditions":[{"type":"Ready","status":"False"}]}}]}"#;

    #[test]
    fn nodes_parse_and_report() {
        let l = kairon_nodes(NODES);
        assert_eq!(
            l.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            ["fluxvm-kube", "kvm-1", "studio-1"]
        );
        assert!(l[0].control_plane && !l[2].control_plane);
        assert_eq!(l[2].backends, vec!["vz"]);
        let c = nodes(&l);
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.contains("not Ready: kvm-1"));
        assert!(c.detail.contains("studio-1 (darwin, vz)"));
        assert_eq!(nodes(&[]).level, Level::Fail);
    }

    #[test]
    fn selftest_plans() {
        let l = kairon_nodes(NODES);
        assert_eq!(
            plan_selftest(&l),
            SelftestPlan::Run {
                hosts: vec!["studio-1".into()]
            }
        );
        assert_eq!(plan_selftest(&[]), SelftestPlan::NoNodes);
        let cp: Vec<_> = l.iter().filter(|n| n.control_plane).cloned().collect();
        assert!(matches!(
            plan_selftest(&cp),
            SelftestPlan::OnlyControlPlane { .. }
        ));
        let nr: Vec<_> = l.iter().filter(|n| n.name == "kvm-1").cloned().collect();
        assert!(matches!(plan_selftest(&nr), SelftestPlan::NoneReady { .. }));
        let msg = selftest_explanation(&SelftestPlan::NoNodes);
        assert!(msg.contains("FluxVM host") && msg.contains("node-agent install"));
        assert!(!msg.to_lowercase().contains("nested"));
    }

    #[test]
    fn render_golden() {
        let cs = vec![
            Check::ok("CRDs", "5 Kairon CRDs are installed and Established".into()),
            Check::fail(
                "node agent",
                "kairon-node: 0 of 1 ready".into(),
                "look at the pods",
            ),
        ];
        assert_eq!(
            render(&cs),
            "[ok  ] CRDs: 5 Kairon CRDs are installed and Established\n[FAIL] node agent: kairon-node: 0 of 1 ready\n       fix: look at the pods\n1 check(s) failed.\n"
        );
        assert!(failed(&cs));
    }
}
