//! `fluxctl kube forward web`: a port-forward that stays. It gives an existing Service a port on 127.0.0.1 (and a
//! `.localhost` name) by adding a companion NodePort Service next to it, which the supervisor then opens. Unlike
//! `kubectl port-forward` it does not die when the pod restarts and needs no terminal kept open.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

/// The first node port tried for a companion Service; `add` and the examples use 30000 to 30999.
const NODE_FIRST: u16 = 30100;
const NODE_LAST: u16 = 32000;

pub fn companion(name: &str) -> String {
    format!("{name}-local")
}

/// A DNS-1123 label short enough to take the `-local` suffix.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 56
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

#[derive(Debug, PartialEq)]
pub struct Plan {
    pub manifest: String,
    pub node_port: u16,
    pub local: u16,
    pub service_port: u16,
}

/// Works out the companion Service for `svc` (the JSON of `kubectl get svc NAME`). `used` are the node ports other Services
/// hold; `reuse` is this companion's own node port from an earlier run, so running it again changes nothing needlessly.
pub fn plan(
    svc: &Value,
    service_port: Option<u16>,
    local: Option<u16>,
    used: &[u16],
    reuse: Option<u16>,
    busy: impl Fn(u16) -> bool,
) -> Result<Plan> {
    let name = svc["metadata"]["name"].as_str().unwrap_or("");
    let ns = svc["metadata"]["namespace"].as_str().unwrap_or("default");
    let selector = svc["spec"]["selector"]
        .as_object()
        .filter(|s| !s.is_empty());
    let Some(selector) = selector else {
        bail!(
            "service {name} has no selector (headless or external), so there are no pods to forward to"
        );
    };
    let ports = svc["spec"]["ports"].as_array().cloned().unwrap_or_default();
    let port = match service_port {
        Some(want) => ports
            .iter()
            .find(|p| p["port"].as_u64() == Some(u64::from(want)))
            .with_context(|| format!("service {name} has no port {want}"))?,
        None if ports.len() == 1 => &ports[0],
        None => bail!(
            "service {name} has several ports ({}); pick one with --service-port",
            ports
                .iter()
                .filter_map(|p| p["port"].as_u64())
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let sp = port["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .context("the service port is not a number")?;
    // A Mac will not let an ordinary user listen below 1024, so 80 becomes 8080 unless told otherwise.
    let local = match local {
        Some(l) if l < 1024 => bail!("--local {l}: this Mac does not let you listen below 1024"),
        Some(l) if busy(l) => bail!("--local {l} is already in use on this Mac"),
        Some(l) => l,
        None => {
            let mut l = if sp < 1024 { sp + 8000 } else { sp };
            let start = l;
            while busy(l) && l < start.saturating_add(50) {
                l += 1;
            }
            if busy(l) {
                bail!("no free port near {start} on this Mac; pass --local");
            }
            l
        }
    };
    let node_port = match reuse {
        Some(n) => n,
        None => (NODE_FIRST..=NODE_LAST)
            .find(|n| !used.contains(n))
            .context("no free node port left")?,
    };
    let manifest = json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": companion(name),
            "namespace": ns,
            "labels": {
                "app.kubernetes.io/managed-by": "fluxvm",
                "fluxvm.dev/forward-of": name,
            },
            "annotations": {"fluxvm.dev/local-ports": format!("{node_port}={local}")},
        },
        "spec": {
            "type": "NodePort",
            "selector": selector,
            "ports": [{
                "name": "fwd",
                "protocol": "TCP",
                "port": sp,
                "targetPort": if port["targetPort"].is_null() { json!(sp) } else { port["targetPort"].clone() },
                "nodePort": node_port,
            }],
        },
    });
    Ok(Plan {
        manifest: serde_json::to_string_pretty(&manifest)?,
        node_port,
        local,
        service_port: sp,
    })
}

/// Every node port any Service in the list holds.
pub fn used_node_ports(list: &Value) -> Vec<u16> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|i| i["spec"]["ports"].as_array().cloned().unwrap_or_default())
        .filter_map(|p| p["nodePort"].as_u64().and_then(|n| u16::try_from(n).ok()))
        .collect()
}

/// The Mac ports that Services have asked for with `fluxvm.dev/local-ports`, except those of the Service `skip` (namespace,
/// name), which is the companion being refreshed.
pub fn claimed_local_ports(list: &Value, skip: (&str, &str)) -> Vec<u16> {
    list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            (
                i["metadata"]["namespace"].as_str().unwrap_or(""),
                i["metadata"]["name"].as_str().unwrap_or(""),
            ) != skip
        })
        .flat_map(|i| {
            kube_host::parse_local_ports(
                i["metadata"]["annotations"]["fluxvm.dev/local-ports"]
                    .as_str()
                    .unwrap_or(""),
            )
        })
        .map(|(_, local)| local)
        .collect()
}

fn check(namespace: &str, service: &str) -> Result<()> {
    if !kube_host::resource_name_ok(namespace) {
        bail!("namespace {namespace:?} is not a Kubernetes name");
    }
    if !name_ok(service) {
        bail!("service {service:?} is not a Kubernetes name (at most 56 characters)");
    }
    Ok(())
}

pub async fn forward<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    service: &str,
    namespace: &str,
    service_port: Option<u16>,
    local: Option<u16>,
) -> Result<String> {
    check(namespace, service)?;
    let svc: Value = serde_json::from_str(
        &kube_host::guest_sh(
            api,
            o,
            &format!("sudo k3s kubectl -n {namespace} get svc {service} -o json"),
        )
        .await
        .with_context(|| format!("looking up service {service} in {namespace}"))?,
    )?;
    let all: Value = serde_json::from_str(
        &kube_host::guest_sh(api, o, "sudo k3s kubectl get svc -A -o json").await?,
    )?;
    let own = companion(service);
    let existing = all["items"].as_array().into_iter().flatten().find(|i| {
        i["metadata"]["name"].as_str() == Some(own.as_str())
            && i["metadata"]["namespace"].as_str() == Some(namespace)
    });
    if let Some(e) = existing
        && e["metadata"]["labels"]["fluxvm.dev/forward-of"].as_str() != Some(service)
    {
        bail!(
            "a Service named {own} already exists in {namespace} and was not made by `kube forward`; rename it or forward another way"
        );
    }
    let reuse = existing
        .and_then(|i| i["spec"]["ports"][0]["nodePort"].as_u64())
        .and_then(|n| u16::try_from(n).ok());
    // This companion's own Mac port is fine to keep; every other port that is open, or promised to another Service, is not.
    let own_local = existing
        .and_then(|i| i["metadata"]["annotations"]["fluxvm.dev/local-ports"].as_str())
        .and_then(|a| kube_host::parse_local_ports(a).first().map(|(_, l)| *l));
    let claimed = claimed_local_ports(&all, (namespace, &own));
    let p = plan(
        &svc,
        service_port,
        local,
        &used_node_ports(&all),
        reuse,
        |port| Some(port) != own_local && (kube_host::port_in_use(port) || claimed.contains(&port)),
    )?;
    kube_host::guest_apply(api, o, &p.manifest).await?;
    kube_host::publish_now(api, o).await?;
    let ready = crate::kube_apps::wait_serving(p.local, std::time::Duration::from_secs(30)).await;
    let name = if namespace == "default" {
        format!("{service}-local.localhost")
    } else {
        format!("{own}.{namespace}.localhost")
    };
    Ok(format!(
        "{service}:{} is on 127.0.0.1:{}{}\n  http://{name} while `fluxctl kube supervise` runs\n  stop it with: fluxctl kube forward {service} --remove",
        p.service_port,
        p.local,
        if ready {
            ""
        } else {
            " (nothing answers yet: its pods may still be starting)"
        }
    ))
}

pub async fn remove<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    service: &str,
    namespace: &str,
) -> Result<String> {
    check(namespace, service)?;
    let out = kube_host::guest_sh(
        api,
        o,
        &format!(
            "sudo k3s kubectl -n {namespace} delete svc -l fluxvm.dev/forward-of={service} --ignore-not-found"
        ),
    )
    .await?;
    if out.is_empty() {
        bail!("{service} is not forwarded");
    }
    Ok(format!("{out}\n(the Mac port closes within a few seconds)"))
}

pub async fn list<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    let all: Value = serde_json::from_str(
        &kube_host::guest_sh(
            api,
            o,
            "sudo k3s kubectl get svc -A -l fluxvm.dev/forward-of -o json",
        )
        .await?,
    )?;
    Ok(render(&all))
}

/// One line per forward: `web (default) 80 -> 127.0.0.1:8080`.
pub fn render(list: &Value) -> String {
    let mut out = String::new();
    for i in list["items"].as_array().into_iter().flatten() {
        let ann = i["metadata"]["annotations"]["fluxvm.dev/local-ports"]
            .as_str()
            .unwrap_or("");
        let local = kube_host::parse_local_ports(ann)
            .first()
            .map_or("?".to_owned(), |(_, l)| l.to_string());
        out.push_str(&format!(
            "{} ({}) {} -> 127.0.0.1:{local}\n",
            i["metadata"]["labels"]["fluxvm.dev/forward-of"]
                .as_str()
                .unwrap_or("?"),
            i["metadata"]["namespace"].as_str().unwrap_or("?"),
            i["spec"]["ports"][0]["port"],
        ));
    }
    if out.is_empty() {
        out.push_str("nothing is forwarded; try: fluxctl kube forward SERVICE\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> Value {
        json!({"metadata": {"name": "web", "namespace": "shop"},
               "spec": {"selector": {"app": "web"},
                        "ports": [{"port": 80, "targetPort": 8080}]}})
    }

    #[test]
    fn a_service_on_80_gets_a_mac_port_above_1024_and_a_free_node_port() {
        let p = plan(&web(), None, None, &[30100, 30101], None, |_| false).unwrap();
        assert_eq!((p.local, p.node_port, p.service_port), (8080, 30102, 80));
        let m: Value = serde_json::from_str(&p.manifest).unwrap();
        assert_eq!(m["metadata"]["name"], "web-local");
        assert_eq!(m["metadata"]["namespace"], "shop");
        assert_eq!(m["spec"]["selector"]["app"], "web");
        assert_eq!(m["spec"]["ports"][0]["targetPort"], 8080);
        assert_eq!(m["spec"]["ports"][0]["nodePort"], 30102);
        assert_eq!(
            m["metadata"]["annotations"]["fluxvm.dev/local-ports"],
            "30102=8080"
        );
        assert_eq!(m["metadata"]["labels"]["fluxvm.dev/forward-of"], "web");
    }

    #[test]
    fn a_busy_mac_port_moves_on_and_an_explicit_one_is_respected_or_refused() {
        let p = plan(&web(), None, None, &[], None, |l| l == 8080).unwrap();
        assert_eq!(p.local, 8081);
        let p = plan(&web(), None, Some(9000), &[], None, |_| false).unwrap();
        assert_eq!(p.local, 9000);
        assert!(plan(&web(), None, Some(9000), &[], None, |_| true).is_err());
        assert!(plan(&web(), None, Some(80), &[], None, |_| false).is_err());
    }

    #[test]
    fn running_it_again_keeps_the_node_port() {
        let p = plan(&web(), None, None, &[30100], Some(30100), |_| false).unwrap();
        assert_eq!(p.node_port, 30100);
    }

    #[test]
    fn several_ports_need_a_choice_and_headless_services_are_refused() {
        let mut two = web();
        two["spec"]["ports"] = json!([{"port": 80}, {"port": 9090, "targetPort": "metrics"}]);
        let err = plan(&two, None, None, &[], None, |_| false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("80, 9090") && err.contains("--service-port"),
            "{err}"
        );
        let p = plan(&two, Some(9090), None, &[], None, |_| false).unwrap();
        assert_eq!(p.local, 9090);
        let m: Value = serde_json::from_str(&p.manifest).unwrap();
        assert_eq!(
            m["spec"]["ports"][0]["targetPort"], "metrics",
            "a named target port is kept"
        );
        assert!(plan(&two, Some(1), None, &[], None, |_| false).is_err());
        let mut headless = web();
        headless["spec"]["selector"] = json!({});
        assert!(plan(&headless, None, None, &[], None, |_| false).is_err());
    }

    #[test]
    fn names_leave_room_for_the_suffix() {
        assert!(name_ok("web") && name_ok(&"a".repeat(56)));
        assert!(!name_ok(&"a".repeat(57)) && !name_ok("-a") && !name_ok("A") && !name_ok("a;b"));
        assert!(check("default", "web").is_ok() && check("Bad", "web").is_err());
    }

    #[test]
    fn node_ports_in_use_are_read_from_every_service() {
        let list = json!({"items": [
            {"spec": {"ports": [{"nodePort": 30100}, {"port": 1}]}},
            {"spec": {"ports": [{"nodePort": 30101}]}}]});
        assert_eq!(used_node_ports(&list), vec![30100, 30101]);
    }

    #[test]
    fn ports_promised_to_other_services_are_claimed_but_not_our_own() {
        let list = json!({"items": [
            {"metadata": {"namespace": "default", "name": "web-local",
                "annotations": {"fluxvm.dev/local-ports": "30100=8080"}}},
            {"metadata": {"namespace": "default", "name": "postgres",
                "annotations": {"fluxvm.dev/local-ports": "30432=5432"}}},
            {"metadata": {"namespace": "default", "name": "plain"}}]});
        assert_eq!(claimed_local_ports(&list, ("x", "y")), vec![8080, 5432]);
        assert_eq!(
            claimed_local_ports(&list, ("default", "web-local")),
            vec![5432]
        );
        // a second forward of another Service on port 80 therefore does not land on 8080 again
        let api = json!({"metadata": {"name": "api", "namespace": "default"},
            "spec": {"selector": {"app": "api"}, "ports": [{"port": 80}]}});
        let claimed = claimed_local_ports(&list, ("default", "api-local"));
        let p = plan(&api, None, None, &[], None, |l| claimed.contains(&l)).unwrap();
        assert_eq!(p.local, 8081);
    }

    #[test]
    fn the_list_shows_each_forward_or_says_there_are_none() {
        let list = json!({"items": [{"metadata": {"namespace": "shop",
            "labels": {"fluxvm.dev/forward-of": "web"},
            "annotations": {"fluxvm.dev/local-ports": "30102=8080"}},
            "spec": {"ports": [{"port": 80}]}}]});
        assert_eq!(render(&list), "web (shop) 80 -> 127.0.0.1:8080\n");
        assert!(render(&json!({"items": []})).contains("nothing is forwarded"));
    }
}
