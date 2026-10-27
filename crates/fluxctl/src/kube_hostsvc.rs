//! `fluxctl kube host ollama 11434`: a program on the Mac becomes a Service in the cluster. Pods then call
//! `ollama:11434` (or `ollama.default.svc`) and reach whatever listens on the Mac's 127.0.0.1:11434: a model server that
//! uses the Mac's GPU, Postgres.app, a dev server with a debugger attached. The VM reaches the Mac at the address of its
//! network gateway, but only a program listening there can be reached, so this opens a relay on that one address (and no
//! other) that forwards to the Mac's loopback, lets in only the VM's own subnet, and closes again with Ctrl-C.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;
use tokio::net::TcpListener;

use crate::kube_host::{self, KubeOptions};
use crate::kube_share;
use crate::run::VmApi;

/// The label that marks what this command made, so it never touches a Service it did not create.
const LABEL: &str = "fluxvm.dev/host-of";

/// A DNS-1123 label.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

#[derive(Debug, PartialEq, Eq)]
pub struct Route {
    /// The Mac, as the VM sees it.
    pub gateway: Ipv4Addr,
    /// The VM's own address on that network.
    pub src: Ipv4Addr,
}

/// Reads `ip -4 route get 1.1.1.1` from the guest: `1.1.1.1 via 192.168.64.1 dev enp0s1 src 192.168.64.178 uid 1000`.
pub fn parse_route(text: &str) -> Option<Route> {
    let mut words = text.split_whitespace();
    let (mut gateway, mut src) = (None, None);
    while let Some(w) = words.next() {
        match w {
            "via" => gateway = words.next().and_then(|v| v.parse().ok()),
            "src" => src = words.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    Some(Route {
        gateway: gateway?,
        src: src?,
    })
}

/// Whether `ifconfig` output lists `ip` as an address of this Mac.
pub fn has_address(ifconfig: &str, ip: Ipv4Addr) -> bool {
    ifconfig.lines().any(|l| {
        let mut w = l.split_whitespace();
        w.next() == Some("inet") && w.next().and_then(|a| a.parse::<Ipv4Addr>().ok()) == Some(ip)
    })
}

/// Whether a peer is on the VM's network: the same /24 as the gateway, which is where the cluster's nodes (and so its
/// pods, whose traffic leaves from the node's address) are. Nothing outside that subnet is let in.
pub fn allowed(peer: IpAddr, gateway: Ipv4Addr) -> bool {
    let v4 = match peer {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4,
            None => return false,
        },
    };
    v4.octets()[..3] == gateway.octets()[..3]
}

/// A Service without a selector, and the slice that points it at the Mac.
pub fn manifest(name: &str, namespace: &str, mac: Ipv4Addr, port: u16) -> String {
    format!(
        "apiVersion: v1\n\
         kind: Service\n\
         metadata:\n  name: {name}\n  namespace: {namespace}\n  labels:\n    {LABEL}: {name}\n\
         spec:\n  ports:\n  - name: tcp\n    port: {port}\n    targetPort: {port}\n    protocol: TCP\n\
         ---\n\
         apiVersion: discovery.k8s.io/v1\n\
         kind: EndpointSlice\n\
         metadata:\n  name: {name}-mac\n  namespace: {namespace}\n  labels:\n    kubernetes.io/service-name: {name}\n    {LABEL}: {name}\n\
         addressType: IPv4\n\
         ports:\n- name: tcp\n  port: {port}\n  protocol: TCP\n\
         endpoints:\n- addresses:\n  - {mac}\n  conditions:\n    ready: true\n"
    )
}

fn check(name: &str, namespace: &str, port: u16) -> Result<()> {
    if !name_ok(name) {
        bail!("{name:?} is not a Kubernetes name (lowercase letters, digits and dashes)");
    }
    if !kube_host::resource_name_ok(namespace) {
        bail!("namespace {namespace:?} is not a Kubernetes name");
    }
    if port == 0 {
        bail!("the port must be between 1 and 65535");
    }
    Ok(())
}

/// Removes what [`run`] made (also after a crash left it behind).
pub async fn remove<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    name: &str,
    namespace: &str,
) -> Result<String> {
    check(name, namespace, 1)?;
    let out = kube_host::guest_sh(
        api,
        o,
        &format!(
            "sudo k3s kubectl -n {namespace} delete svc,endpointslice -l {LABEL}={name} --ignore-not-found"
        ),
    )
    .await?;
    if out.trim().is_empty() {
        bail!("{name} is not a Mac service in {namespace}");
    }
    Ok(out)
}

pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    name: &str,
    port: u16,
    namespace: &str,
) -> Result<()> {
    check(name, namespace, port)?;
    let route = parse_route(
        &kube_host::guest_sh(api, o, "ip -4 route get 1.1.1.1")
            .await
            .context("asking the VM how it reaches the Mac")?,
    )
    .context("the VM did not say which address is the Mac's")?;
    if !route.gateway.is_private() {
        bail!(
            "the VM reaches the network through {}, which is not a private address, so it is not the Mac",
            route.gateway
        );
    }
    let ifconfig = Command::new("ifconfig")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if !has_address(&ifconfig, route.gateway) {
        bail!(
            "this Mac has no address {} (the VM's gateway); is the VM running on this Mac?",
            route.gateway
        );
    }
    let existing = kube_host::guest_sh(
        api,
        o,
        &format!("sudo k3s kubectl -n {namespace} get svc {name} --ignore-not-found -o json"),
    )
    .await?;
    if !existing.trim().is_empty() {
        let svc: Value = serde_json::from_str(&existing)?;
        if svc["metadata"]["labels"][LABEL].as_str() != Some(name) {
            bail!(
                "a Service named {name} already exists in {namespace} and was not made by `kube host`; use another name"
            );
        }
    }
    // Something may already listen on the gateway address (a program bound to all addresses); then the VM can reach it
    // without a relay and this only adds the name.
    let listener = match TcpListener::bind((route.gateway, port)).await {
        Ok(l) => Some(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => None,
        Err(e) => {
            return Err(e).with_context(|| format!("could not listen on {}:{port}", route.gateway));
        }
    };
    kube_host::guest_apply(api, o, &manifest(name, namespace, route.gateway, port)).await?;
    println!(
        "{name}.{namespace}:{port} in the cluster now reaches the Mac: {}:{port} on {}",
        if listener.is_some() {
            "127.0.0.1"
        } else {
            "whatever already listens on"
        },
        route.gateway
    );
    println!(
        "Only the VM's network ({}.0/24) can use it. Ctrl-C removes the Service and closes the relay.",
        route.gateway.octets()[..3]
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    );
    let gateway = route.gateway;
    let stop = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match listener {
        Some(l) => {
            let (served, refused) =
                kube_share::relay_with(l, port, move |ip| allowed(ip, gateway), stop).await;
            println!("closed: {served} connection(s) relayed, {refused} refused");
        }
        None => stop.await,
    }
    match remove(api, o, name, namespace).await {
        Ok(_) => println!("removed Service {name}"),
        Err(e) => eprintln!(
            "could not remove Service {name} ({e}); `fluxctl kube host {name} --remove` does it"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_kubernetes_labels() {
        for ok in ["ollama", "pg-1", "a"] {
            assert!(name_ok(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "Ollama", "a.b", "a_b", &"a".repeat(64)] {
            assert!(!name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_route_gives_the_mac_and_the_vm() {
        let r = parse_route(
            "1.1.1.1 via 192.168.64.1 dev enp0s1 src 192.168.64.178 uid 1000 \n    cache \n",
        )
        .unwrap();
        assert_eq!(r.gateway, Ipv4Addr::new(192, 168, 64, 1));
        assert_eq!(r.src, Ipv4Addr::new(192, 168, 64, 178));
        // No route, or a route without a gateway (a direct one), is not enough to find the Mac.
        assert_eq!(parse_route(""), None);
        assert_eq!(parse_route("1.1.1.1 dev lo src 10.0.0.2 uid 0"), None);
        assert_eq!(parse_route("1.1.1.1 via nonsense dev x src 10.0.0.2"), None);
    }

    #[test]
    fn the_mac_must_own_the_gateway_address() {
        let out = "lo0: flags=8049<UP>\n\tinet 127.0.0.1 netmask 0xff000000\nbridge100: flags=8863<UP>\n\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255\n\tinet6 fe80::1 prefixlen 64\n";
        assert!(has_address(out, Ipv4Addr::new(192, 168, 64, 1)));
        assert!(!has_address(out, Ipv4Addr::new(192, 168, 64, 2)));
        assert!(!has_address("", Ipv4Addr::new(192, 168, 64, 1)));
        // inet6 lines and prefixes of an address are not matches.
        assert!(!has_address(out, Ipv4Addr::new(192, 168, 64, 25)));
    }

    #[test]
    fn only_the_vm_subnet_is_let_in() {
        let gw = Ipv4Addr::new(192, 168, 64, 1);
        for ok in ["192.168.64.178", "192.168.64.2", "::ffff:192.168.64.9"] {
            assert!(allowed(ok.parse().unwrap(), gw), "{ok}");
        }
        for no in [
            "192.168.1.20",
            "192.168.65.5",
            "10.0.0.5",
            "127.0.0.1",
            "8.8.8.8",
            "fe80::1",
            "::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!allowed(no.parse().unwrap(), gw), "{no}");
        }
    }

    #[test]
    fn the_manifest_is_a_selectorless_service_and_a_slice_to_the_mac() {
        let m = manifest("ollama", "ai", Ipv4Addr::new(192, 168, 64, 1), 11434);
        let docs: Vec<Value> = m
            .split("---\n")
            .map(|d| serde_yaml::from_str(d).expect(d))
            .collect();
        assert_eq!(docs.len(), 2);
        let (svc, slice) = (&docs[0], &docs[1]);
        assert_eq!(svc["kind"], "Service");
        assert_eq!(svc["metadata"]["namespace"], "ai");
        assert_eq!(svc["metadata"]["labels"][LABEL], "ollama");
        assert!(svc["spec"].get("selector").is_none());
        assert_eq!(svc["spec"]["ports"][0]["port"], 11434);
        assert_eq!(slice["kind"], "EndpointSlice");
        assert_eq!(
            slice["metadata"]["labels"]["kubernetes.io/service-name"],
            "ollama"
        );
        assert_eq!(slice["metadata"]["labels"][LABEL], "ollama");
        assert_eq!(slice["endpoints"][0]["addresses"][0], "192.168.64.1");
        // The port is named in both, so they match.
        assert_eq!(slice["ports"][0]["name"], svc["spec"]["ports"][0]["name"]);
        assert_eq!(slice["ports"][0]["port"], 11434);
    }

    #[test]
    fn bad_input_is_refused_before_anything_runs() {
        assert!(check("ok", "default", 80).is_ok());
        assert!(check("Bad", "default", 80).is_err());
        assert!(check("ok", "Bad_ns", 80).is_err());
        assert!(check("ok", "default", 0).is_err());
    }

    #[tokio::test]
    async fn the_relay_serves_the_vm_subnet_and_refuses_everyone_else() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_port = service.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = service.accept().await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });
        let shared = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shared_port = shared.local_addr().unwrap().port();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        // The test client connects from 127.0.0.1, so a rule that only lets in 192.168.64.x refuses it ...
        let gw = Ipv4Addr::new(192, 168, 64, 1);
        let task = tokio::spawn(kube_share::relay_with(
            shared,
            service_port,
            move |ip| allowed(ip, gw),
            async {
                let _ = stop_rx.await;
            },
        ));
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", shared_port))
            .await
            .unwrap();
        let _ = c.write_all(b"ping").await;
        let mut reply = [0u8; 4];
        assert!(
            c.read_exact(&mut reply).await.is_err(),
            "a stranger got an answer"
        );
        stop_tx.send(()).unwrap();
        assert_eq!(task.await.unwrap(), (0, 1));
    }
}
