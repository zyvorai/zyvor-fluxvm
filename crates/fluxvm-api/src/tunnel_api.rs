// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `GET /v1/vms/{id}/tunnel?port=N`: a WebSocket that carries one TCP connection to port N of a running VM.
//!
//! This is the node's half of fleet tunnels (`fluxctl fleet ssh`): central authenticates the tenant, checks the port
//! policy and connects here with the fleet token; this end only ever connects to the VM itself:
//!
//! - an `apple.usernet` VM through the host listener of its own TCP port forward for that guest port (the guest is on
//!   a private user-mode network the host cannot route to);
//! - a `tap` + `netns` VM from inside its network namespace, to the address its dnsmasq leased;
//! - any other VM to the address the node knows for it (the vz runner's NAT address, a DHCP lease).
//!
//! Loopback, unspecified and multicast guest addresses are refused, so a tunnel never reaches the node's own services.
//! Binary messages carry the bytes, at most 64 KiB each; each direction waits for its write before reading again, so a
//! slow end slows the other instead of filling memory.

use super::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use futures::{SinkExt, StreamExt};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest message read from a guest connection.
const CHUNK: usize = 64 * 1024;
/// Largest message accepted from central.
const MAX_MESSAGE: usize = 256 * 1024;
/// Where the usernet gateway reports its bound forward listeners (`fluxvm_apple::usernet::FORWARDS_FILE`).
const USERNET_FORWARDS_FILE: &str = "usernet-forwards.json";

#[derive(Deserialize)]
pub(crate) struct TunnelQuery {
    port: u16,
}

/// Where a tunnel connects.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// A plain connect from the node.
    Direct(SocketAddr),
    /// A connect from inside the network namespace of process `pid`.
    Netns(u32, SocketAddr),
}

fn conflict(msg: impl Into<String>) -> Response {
    (StatusCode::CONFLICT, Json(json!({"error": msg.into()}))).into_response()
}

/// An address a tunnel may connect to as the guest's: not loopback, not link-local (cloud metadata services live
/// there), not a group. The address can come from the guest itself, so it is never trusted further than this.
#[cfg(test)]
fn usable_guest_ip(ip: IpAddr) -> bool {
    usable_ip(ip, false)
}

/// [`usable_guest_ip`], optionally allowing link-local IPv4. A VM in its own network namespace is given a
/// `169.254.x.y` address there and the connect happens inside that namespace, so only the cloud metadata address
/// stays refused.
fn usable_ip(ip: IpAddr, allow_link_local: bool) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || (v4.is_link_local()
                    && !(allow_link_local && v4 != Ipv4Addr::new(169, 254, 169, 254))))
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local())
        }
    }
}

/// Whether `ip` is an address of one of this host's own interfaces (a guest that announces the Mac's own address
/// must not get a tunnel to the Mac's services). Fails closed: when the interfaces cannot be listed, it says yes.
fn is_host_address(ip: IpAddr) -> bool {
    #[cfg(unix)]
    {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs fills `ifap` with a list that is freed below with freeifaddrs.
        if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
            return true;
        }
        let mut found = false;
        let mut cur = ifap;
        while !cur.is_null() {
            // SAFETY: `cur` is a node of the list getifaddrs returned, which stays valid until freeifaddrs.
            let ifa = unsafe { &*cur };
            if !ifa.ifa_addr.is_null() {
                // SAFETY: `ifa_addr` points to a sockaddr whose family says which sockaddr_* it is.
                let fam = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
                let addr = if fam == libc::AF_INET {
                    let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                        sin.sin_addr.s_addr,
                    ))))
                } else if fam == libc::AF_INET6 {
                    let sin6 = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
                    Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
                } else {
                    None
                };
                if addr == Some(ip) {
                    found = true;
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        // SAFETY: `ifap` came from getifaddrs and is freed once.
        unsafe { libc::freeifaddrs(ifap) };
        found
    }
    #[cfg(not(unix))]
    {
        let _ = ip;
        false
    }
}

/// Whether `ip` is in the subnet of a `bridge*` interface of this Mac: where vmnet puts the guests it attaches. The
/// address of a vz guest is what the guest printed on its serial console, so it is only used when it lies on the
/// guests' own network and never on the Mac's LAN. Fails closed.
fn on_vmnet_bridge(ip: IpAddr) -> bool {
    #[cfg(target_os = "macos")]
    {
        let IpAddr::V4(want) = ip else {
            return false;
        };
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs fills `ifap` with a list that is freed below with freeifaddrs.
        if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
            return false;
        }
        let mut found = false;
        let mut cur = ifap;
        while !cur.is_null() {
            // SAFETY: `cur` is a node of the list getifaddrs returned, which stays valid until freeifaddrs.
            let ifa = unsafe { &*cur };
            if !ifa.ifa_addr.is_null() && !ifa.ifa_netmask.is_null() && !ifa.ifa_name.is_null() {
                // SAFETY: `ifa_name` is a NUL-terminated string and the sockaddrs have the family they declare.
                let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
                let fam = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
                if name.starts_with("bridge") && fam == libc::AF_INET {
                    let addr = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                    let mask = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
                    let addr = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                    let mask = u32::from_be(mask.sin_addr.s_addr);
                    if same_subnet(addr, want, mask) {
                        found = true;
                        break;
                    }
                }
            }
            cur = ifa.ifa_next;
        }
        // SAFETY: `ifap` came from getifaddrs and is freed once.
        unsafe { libc::freeifaddrs(ifap) };
        found
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = ip;
        false
    }
}

/// Whether `a` and `b` are in the same subnet under `mask` (a host mask of 0 would match everything: refused).
fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, mask: u32) -> bool {
    mask != 0 && (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// The host listener of a usernet VM's TCP forward for `port`, from the gateway's report (`[{proto, bound,
/// guest_port, ...}]`). A listener bound to all addresses is reached on loopback.
fn usernet_forward(report: &serde_json::Value, port: u16) -> Option<SocketAddr> {
    report
        .get("forwards")?
        .as_array()?
        .iter()
        .filter(|f| f.get("proto").and_then(|p| p.as_str()) == Some("tcp"))
        .filter(|f| f.get("guest_port").and_then(|p| p.as_u64()) == Some(u64::from(port)))
        .find_map(|f| f.get("bound")?.as_str()?.parse::<SocketAddr>().ok())
        .map(|mut a| {
            if a.ip().is_unspecified() {
                a.set_ip(match a.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                });
            }
            a
        })
}

/// Works out where a tunnel to `port` of `vm` connects, or why it cannot.
fn tunnel_target(vm: &VmRecord, port: u16) -> Result<Target, String> {
    let usernet = vm
        .request
        .apple
        .as_ref()
        .is_some_and(|a| a.usernet.is_some());
    if usernet {
        let report = std::fs::read(vm.workspace.join(USERNET_FORWARDS_FILE))
            .ok()
            .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
            .unwrap_or(serde_json::Value::Null);
        return usernet_forward(&report, port).map(Target::Direct).ok_or_else(|| {
            format!(
                "this VM uses user-mode networking (apple.usernet) and publishes no TCP forward for guest port \
                 {port}; add one to apple.usernet.port_forwards"
            )
        });
    }
    let Some(ip) = vm.guest_ip.as_deref() else {
        return Err(
            "the node does not know the VM's address yet; it does once the guest has booted and taken its address"
                .into(),
        );
    };
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| format!("the VM's address '{ip}' is not an IP address"))?;
    if !usable_ip(ip, vm.netns.is_some()) {
        return Err(format!("the VM's address {ip} cannot be tunnelled to"));
    }
    // A vz guest's address is whatever it printed on its console, so it may only point at the guests' own network:
    // never at the Mac's LAN or at anything outside the vmnet bridges.
    if vm.backend == BackendKind::Vz && vm.netns.is_none() && !on_vmnet_bridge(ip) {
        return Err(format!(
            "the VM reports the address {ip}, which is not on this Mac's VM network, so it cannot be tunnelled to"
        ));
    }
    let addr = SocketAddr::new(ip, port);
    match (vm.netns.is_some(), vm.pid) {
        (true, Some(pid)) => Ok(Target::Netns(pid, addr)),
        (true, None) => Err("the VM has no running process to reach its network through".into()),
        (false, _) if is_host_address(ip) => Err(format!(
            "the VM's address {ip} belongs to this node, so it cannot be tunnelled to"
        )),
        (false, _) => Ok(Target::Direct(addr)),
    }
}

/// `GET /v1/vms/{id}/tunnel?port=N` (admin): see the module docs.
pub(crate) async fn vm_tunnel(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Query(q): Query<TunnelQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(e) = require_admin(role) {
        return e.into_response();
    }
    if q.port == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "port must be 1-65535"})),
        )
            .into_response();
    }
    let vm = match m.get(id).await {
        Ok(vm) => vm,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no VM '{id}'")})),
            )
                .into_response();
        }
    };
    if vm.status != VmStatus::Running {
        return conflict("the VM is not running; start it first");
    }
    let target = match tunnel_target(&vm, q.port) {
        Ok(t) => t,
        Err(e) => return conflict(e),
    };
    let addr = match &target {
        Target::Direct(a) | Target::Netns(_, a) => *a,
    };
    // A guest that announces another VM's address (on a shared network that is another tenant's) gets no tunnel to it.
    if matches!(target, Target::Direct(_)) && vm.guest_ip.is_some() {
        let others = m.list().await;
        if others.iter().any(|o| {
            o.id != vm.id && o.guest_ip.as_deref().map(str::trim) == Some(&addr.ip().to_string())
        }) {
            return conflict(format!(
                "the VM's address {} is also reported by another VM on this node, so it cannot be tunnelled to",
                addr.ip()
            ));
        }
    }
    let connect = async {
        match target {
            Target::Direct(a) => tokio::net::TcpStream::connect(a).await,
            Target::Netns(pid, a) => connect_in_netns(pid, a).await,
        }
    };
    let tcp = match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return conflict(format!(
                "nothing in the VM accepts connections on port {} (connection refused)",
                q.port
            ));
        }
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("connecting to the VM at {addr}: {e}")})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({"error": format!("connecting to the VM at {addr} timed out")})),
            )
                .into_response();
        }
    };
    let _ = tcp.set_nodelay(true);
    tracing::info!(vm = %id, port = q.port, %addr, "tunnel opened");
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move {
            relay(socket, tcp).await;
            tracing::info!(vm = %id, "tunnel closed");
        })
}

/// Binary messages to the guest connection and its bytes back, until either side closes.
async fn relay(socket: WebSocket, tcp: tokio::net::TcpStream) {
    let (mut rd, mut wr) = tcp.into_split();
    let (mut tx, mut rx) = socket.split();
    let up = async {
        while let Some(Ok(msg)) = rx.next().await {
            let ok = match msg {
                Message::Binary(b) => wr.write_all(&b).await.is_ok(),
                Message::Text(t) => wr.write_all(t.as_str().as_bytes()).await.is_ok(),
                Message::Close(_) => false,
                Message::Ping(_) | Message::Pong(_) => true,
            };
            if !ok {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    let down = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netns_guests_may_be_link_local_but_never_the_metadata_address() {
        let ll: IpAddr = "169.254.7.9".parse().unwrap();
        assert!(!usable_guest_ip(ll));
        assert!(usable_ip(ll, true));
        assert!(!usable_ip("169.254.169.254".parse().unwrap(), true));
        assert!(!usable_ip("127.0.0.1".parse().unwrap(), true));
    }

    #[test]
    fn subnet_match_refuses_an_empty_mask() {
        let a: Ipv4Addr = "192.168.64.1".parse().unwrap();
        let m = 0xffff_ff00;
        assert!(same_subnet(a, "192.168.64.77".parse().unwrap(), m));
        assert!(!same_subnet(a, "192.168.65.77".parse().unwrap(), m));
        assert!(!same_subnet(a, "10.0.0.5".parse().unwrap(), m));
        assert!(!same_subnet(a, "10.0.0.5".parse().unwrap(), 0));
    }

    #[test]
    fn guest_addresses() {
        for ok in ["192.168.64.5", "10.230.3.9", "fd00::5"] {
            assert!(usable_guest_ip(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "224.0.0.1",
            "255.255.255.255",
            "169.254.169.254",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!usable_guest_ip(bad.parse().unwrap()), "{bad}");
        }
        // The node's own loopback interface is one of its addresses.
        assert!(is_host_address("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn usernet_forwards() {
        let report = json!({"pid": 7, "forwards": [
            {"index": 0, "proto": "udp", "bound": "127.0.0.1:5353", "guest_port": 22, "guest_family": "v4"},
            {"index": 1, "proto": "tcp", "bound": "0.0.0.0:40022", "guest_port": 22, "guest_family": "v4"},
            {"index": 2, "proto": "tcp", "bound": "127.0.0.1:48080", "guest_port": 80, "guest_family": "v4"},
        ]});
        assert_eq!(
            usernet_forward(&report, 22),
            Some("127.0.0.1:40022".parse().unwrap())
        );
        assert_eq!(
            usernet_forward(&report, 80),
            Some("127.0.0.1:48080".parse().unwrap())
        );
        assert_eq!(usernet_forward(&report, 443), None);
        assert_eq!(usernet_forward(&serde_json::Value::Null, 22), None);
    }

    #[tokio::test]
    async fn relays_bytes_both_ways() {
        // An echo "guest", the node's tunnel endpoint in front of it, and a WebSocket client.
        let guest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let guest_addr = guest.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = guest.accept().await.unwrap();
            let (mut r, mut w) = c.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
        let app = Router::new().route(
            "/t",
            get(move |ws: WebSocketUpgrade| async move {
                let tcp = tokio::net::TcpStream::connect(guest_addr).await.unwrap();
                ws.on_upgrade(move |s| relay(s, tcp))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/t"))
            .await
            .unwrap();
        use tokio_tungstenite::tungstenite::Message as M;
        let big = vec![7u8; 200 * 1024];
        ws.send(M::Binary(big.clone().into())).await.unwrap();
        let mut got = Vec::new();
        while got.len() < big.len() {
            match ws.next().await {
                Some(Ok(M::Binary(b))) => {
                    assert!(b.len() <= CHUNK);
                    got.extend_from_slice(&b);
                }
                Some(Ok(_)) => {}
                other => panic!("ended early: {other:?}"),
            }
        }
        assert_eq!(got, big);
    }
}
