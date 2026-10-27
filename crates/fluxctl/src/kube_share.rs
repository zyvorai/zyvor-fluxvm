//! `fluxctl kube share web`: open a service from a phone or another computer on the same Wi-Fi. A published service
//! answers only on 127.0.0.1; this opens one extra port on the Mac's network address that relays to it, prints the
//! address with a QR code to scan, and closes again on Ctrl-C or after a time limit. Only peers on a private or
//! link-local network are let in, and at most 64 connections at once.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

const MAX_CONNECTIONS: usize = 64;
/// The longest a share may stay open.
pub const MAX_MINUTES: u64 = 480;

#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub namespace: String,
    pub name: String,
    /// The service's port on 127.0.0.1.
    pub port: u16,
}

/// Finds the published service the user named: `web`, or `shop/web` when the name exists in several namespaces.
/// `items` are the entries of `kube services`.
pub fn pick(items: &[Value], query: &str) -> Result<Target> {
    let (want_ns, want_name) = match query.split_once('/') {
        Some((ns, name)) => (Some(ns), name),
        None => (None, query),
    };
    let found: Vec<&Value> = items
        .iter()
        .filter(|s| s["name"].as_str() == Some(want_name))
        .filter(|s| want_ns.is_none_or(|ns| s["namespace"].as_str() == Some(ns)))
        .collect();
    let describe = |s: &Value| {
        format!(
            "{}/{}",
            s["namespace"].as_str().unwrap_or("?"),
            s["name"].as_str().unwrap_or("?")
        )
    };
    match found.as_slice() {
        [] => {
            let known: Vec<String> = items.iter().map(describe).collect();
            if known.is_empty() {
                bail!(
                    "no service is open on this Mac yet; expose one with `fluxctl kube forward NAME` or `fluxctl kube add APP`"
                );
            }
            bail!("no service named {query}; there are: {}", known.join(", "))
        }
        [one] => {
            if one["published"].as_bool() == Some(false) {
                bail!(
                    "{} is not open on this Mac yet (is `fluxctl kube supervise` running?)",
                    describe(one)
                );
            }
            let port = one["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0)
                .with_context(|| format!("{} has no port on this Mac", describe(one)))?;
            Ok(Target {
                namespace: one["namespace"].as_str().unwrap_or("default").into(),
                name: one["name"].as_str().unwrap_or(want_name).into(),
                port,
            })
        }
        many => bail!(
            "{query} is in several namespaces ({}); say which, for example {}",
            many.iter().map(|s| describe(s)).collect::<Vec<_>>().join(", "),
            describe(many[0])
        ),
    }
}

/// Whether a peer may use the share: this Mac, or a private or link-local network address. A public address is never
/// let in, whatever the router forwards.
pub fn lan_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4_allowed(v4),
            None => v6_allowed(v6),
        },
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
}

pub fn share_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

/// The Mac's Bonjour name (`Minis-Mac-mini.local`), from `scutil --get LocalHostName`.
pub fn bonjour_name(local_host_name: &str) -> Option<String> {
    let n = local_host_name.trim();
    (!n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
    .then(|| format!("{n}.local"))
}

fn mac_sh(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// The Mac's addresses a phone can use: the Bonjour name first, then the Wi-Fi or Ethernet IPv4 address.
fn addresses() -> Vec<String> {
    let mut out = vec![];
    if let Some(n) = bonjour_name(&mac_sh("scutil", &["--get", "LocalHostName"])) {
        out.push(n);
    }
    for iface in ["en0", "en1"] {
        let ip = mac_sh("ipconfig", &["getifaddr", iface]);
        if ip.parse::<Ipv4Addr>().is_ok() && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// The QR code for `text` drawn with half-block characters (two modules per text row), with the quiet zone around it.
pub fn qr(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).context("the address is too long for a QR code")?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build())
}

/// Relays every allowed connection on `listener` to 127.0.0.1:`to` until `stop` finishes. Returns how many
/// connections were relayed and how many were refused.
pub async fn relay(
    listener: TcpListener,
    to: u16,
    stop: impl std::future::Future<Output = ()>,
) -> (u64, u64) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let (mut served, mut refused) = (0u64, 0u64);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                let Ok((mut inbound, peer)) = accepted else { continue };
                let permit = slots.clone().try_acquire_owned();
                if !lan_allowed(peer.ip()) || permit.is_err() {
                    refused += 1;
                    continue;
                }
                served += 1;
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Ok(mut outbound) = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], to))).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
            }
        }
    }
    (served, refused)
}

pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    service: &str,
    port: Option<u16>,
    minutes: u64,
) -> Result<()> {
    if minutes == 0 || minutes > MAX_MINUTES {
        bail!("--minutes must be between 1 and {MAX_MINUTES}");
    }
    let listed = kube_host::services(api, o).await?;
    let items = listed["services"].as_array().cloned().unwrap_or_default();
    let target = pick(&items, service)?;
    let listener = TcpListener::bind(("0.0.0.0", port.unwrap_or(0)))
        .await
        .context("could not open a port on the network")?;
    let shared = listener.local_addr()?.port();
    let hosts = addresses();
    if hosts.is_empty() {
        bail!("this Mac has no network address to share on (is Wi-Fi on?)");
    }
    let urls: Vec<String> = hosts.iter().map(|h| share_url(h, shared)).collect();
    println!(
        "{}/{} is open to this network for {minutes} min (Ctrl-C to stop):",
        target.namespace, target.name
    );
    for u in &urls {
        println!("  {u}");
    }
    println!("\n{}", qr(&urls[0])?);
    println!(
        "Anyone on this Wi-Fi who knows the address can reach the service while this runs; only private-network addresses are let in."
    );
    let stop = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = tokio::time::sleep(Duration::from_secs(minutes * 60)) => {}
        }
    };
    let (served, refused) = relay(listener, target.port, stop).await;
    println!("closed: {served} connection(s) relayed, {refused} refused");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(ns: &str, name: &str, port: u16, published: bool) -> Value {
        json!({"namespace": ns, "name": name, "port": port, "published": published})
    }

    #[test]
    fn a_service_is_found_by_name_or_namespace_and_name() {
        let items = vec![
            item("default", "web", 30080, true),
            item("shop", "web", 30081, true),
            item("default", "db", 30432, true),
        ];
        assert_eq!(pick(&items, "db").unwrap().port, 30432);
        assert_eq!(pick(&items, "shop/web").unwrap().port, 30081);
        let err = pick(&items, "web").unwrap_err().to_string();
        assert!(err.contains("several namespaces") && err.contains("default/web"));
        let err = pick(&items, "nope").unwrap_err().to_string();
        assert!(err.contains("default/db"));
    }

    #[test]
    fn nothing_open_and_unpublished_say_what_to_do() {
        let err = pick(&[], "web").unwrap_err().to_string();
        assert!(err.contains("kube forward"));
        let err = pick(&[item("default", "web", 30080, false)], "web")
            .unwrap_err()
            .to_string();
        assert!(err.contains("supervise"));
    }

    #[test]
    fn only_this_mac_and_private_networks_are_let_in() {
        for ok in [
            "127.0.0.1",
            "192.168.1.20",
            "10.0.0.5",
            "172.16.4.1",
            "169.254.3.3",
            "::1",
            "fe80::1",
            "fd12:3456::1",
            "::ffff:192.168.1.9",
        ] {
            assert!(lan_allowed(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!lan_allowed(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn the_bonjour_name_is_made_only_from_a_plain_host_name() {
        assert_eq!(
            bonjour_name("Minis-Mac-mini\n").as_deref(),
            Some("Minis-Mac-mini.local")
        );
        assert_eq!(bonjour_name(""), None);
        assert_eq!(bonjour_name("a b"), None);
        assert_eq!(share_url("x.local", 51234), "http://x.local:51234");
    }

    #[test]
    fn the_qr_code_is_drawn_in_text() {
        let q = qr("http://x.local:51234").unwrap();
        assert!(q.lines().count() > 10);
        assert!(q.chars().any(|c| c == '█' || c == '▀' || c == '▄'));
    }

    #[tokio::test]
    async fn connections_are_relayed_to_the_service_and_stop_ends_the_share() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_port = service.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = service.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"pong").await.unwrap();
        });
        let shared = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let shared_port = shared.local_addr().unwrap().port();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(relay(shared, service_port, async {
            let _ = stop_rx.await;
        }));
        let mut c = TcpStream::connect(("127.0.0.1", shared_port)).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut reply = [0u8; 4];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        stop_tx.send(()).unwrap();
        let (served, refused) = task.await.unwrap();
        assert_eq!((served, refused), (1, 0));
    }
}
