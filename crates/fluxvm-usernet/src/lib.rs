// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A userspace NAT gateway for one `vz` guest, in the spirit of slirp / gvisor-tap-vsock.
//!
//! A guest NIC backed by `VZFileHandleNetworkDeviceAttachment` exchanges raw Ethernet frames, one per datagram, over
//! one end of a `SOCK_DGRAM` socketpair (see `fluxvm-vz-switch` and `Runner.swift`). [`UserNet`] owns the other end
//! and plays the whole network around the guest:
//!
//! * ARP for the gateway and DNS addresses, ICMP echo to the gateway;
//! * a DHCPv4 server that always hands out the configured /N (default `10.0.2.15/24`, gateway `10.0.2.2`, DNS
//!   `10.0.2.3`, MTU option);
//! * a DNS forwarder (UDP) with allow/deny domain policy; answers for allow-listed names teach the egress policy
//!   which addresses are reachable;
//! * TCP and UDP NAT to the host network: guest TCP is terminated in userspace and relayed over ordinary host
//!   sockets, so nothing needs privileges and every flow passes the [`EgressPolicy`];
//! * host -> guest TCP and UDP port forwards;
//! * a [`FlowEvent`] stream (open, close, deny, DNS) on a broadcast channel.
//!
//! Out of scope (see the README-style notes on [`UserNet`]): IPv6, IP fragments, ICMP beyond gateway echo, DNS over
//! TCP, TCP window scaling/SACK.
//!
//! # Wiring (TODO, not done here)
//!
//! `Runner.swift` already attaches a file-handle NIC for private networks (`socketpair(AF_UNIX, SOCK_DGRAM)`, 1 MiB
//! send / 4 MiB receive buffers). A NIC with `usernet` would do the same and hand the second end to this crate:
//! either linked into the daemon (`UserNet::from_fd`) or via the `fluxvm-usernet --fd N --config cfg.json` binary
//! spawned by the runner. The `apple.usernet` spec field and its `capability.rs` validation are not added yet.

mod config;
mod dhcp;
mod dns;
mod tcp;
pub mod wire;

pub use config::{
    Cidr, Decision, EgressPolicy, PortForward, Proto, UserNetConfig, is_non_unicast, is_private,
};

use serde::Serialize;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram as StdUnixDatagram;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tcp::{State, TcpConn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket, UnixDatagram};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc};
use tokio::task::AbortHandle;
use wire::{ACK, BROADCAST, FIN, Mac, RST, SYN, TcpOut};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Guest-initiated.
    Outbound,
    /// Host-initiated through a port forward.
    Inbound,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Open,
    Close,
    Deny,
    ConnectFailed,
    DnsQuery,
    DnsDeny,
}

/// One entry of the flow log.
#[derive(Clone, Debug, Serialize)]
pub struct FlowEvent {
    pub ts_ms: u64,
    pub kind: FlowKind,
    pub proto: Proto,
    pub direction: Direction,
    pub guest: SocketAddr,
    pub remote: SocketAddr,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// DNS name for `dns_*` events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Bytes guest -> remote / remote -> guest, on `close`.
    pub bytes_tx: u64,
    pub bytes_rx: u64,
}

/// Cloneable control surface for a running [`UserNet`].
#[derive(Clone)]
pub struct UserNetHandle {
    log: broadcast::Sender<FlowEvent>,
    shutdown: Arc<Notify>,
}

impl UserNetHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<FlowEvent> {
        self.log.subscribe()
    }
    /// Make [`UserNet::run`] return and tear down every flow.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }
}

pub struct UserNet {
    cfg: UserNetConfig,
    sock: StdUnixDatagram,
    tcp_fwd: Vec<(usize, std::net::TcpListener)>,
    udp_fwd: Vec<(usize, std::net::UdpSocket)>,
    fwd_addrs: Vec<SocketAddr>,
    handle: UserNetHandle,
}

impl UserNet {
    /// `endpoint` is the gateway's end of the frame socketpair (the guest NIC holds the other). Forward listeners
    /// are bound here, so a busy port fails early.
    pub fn new(cfg: UserNetConfig, endpoint: StdUnixDatagram) -> io::Result<Self> {
        cfg.validate()?;
        endpoint.set_nonblocking(true)?;
        {
            // Apple's guidance for file-handle networking: receive buffer several times the send buffer.
            // Best effort; the runner normally sets these already.
            use std::os::fd::AsRawFd;
            for (opt, v) in [(libc::SO_SNDBUF, 1 << 20), (libc::SO_RCVBUF, 4 << 20)] {
                let v: libc::c_int = v;
                // SAFETY: valid fd and a correctly sized int option value.
                unsafe {
                    libc::setsockopt(
                        endpoint.as_raw_fd(),
                        libc::SOL_SOCKET,
                        opt,
                        (&raw const v).cast(),
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
            }
        }
        let (mut tcp_fwd, mut udp_fwd, mut fwd_addrs) = (Vec::new(), Vec::new(), Vec::new());
        for (i, f) in cfg.forwards.iter().enumerate() {
            let addr = SocketAddr::new(f.host_addr, f.host_port);
            match f.proto {
                Proto::Tcp => {
                    let l = std::net::TcpListener::bind(addr)?;
                    l.set_nonblocking(true)?;
                    fwd_addrs.push(l.local_addr()?);
                    tcp_fwd.push((i, l));
                }
                Proto::Udp => {
                    let s = std::net::UdpSocket::bind(addr)?;
                    s.set_nonblocking(true)?;
                    fwd_addrs.push(s.local_addr()?);
                    udp_fwd.push((i, s));
                }
            }
        }
        // fwd_addrs was filled in config order because each forward pushes exactly one address.
        let (log, _) = broadcast::channel(1024);
        Ok(Self {
            cfg,
            sock: endpoint,
            tcp_fwd,
            udp_fwd,
            fwd_addrs,
            handle: UserNetHandle {
                log,
                shutdown: Arc::new(Notify::new()),
            },
        })
    }

    /// Like [`UserNet::new`] from a raw file descriptor (for example one end of a `socketpair` inherited from the
    /// runner). The fd must be a `SOCK_DGRAM` Unix socket.
    pub fn from_fd(cfg: UserNetConfig, fd: OwnedFd) -> io::Result<Self> {
        Self::new(cfg, StdUnixDatagram::from(fd))
    }

    pub fn handle(&self) -> UserNetHandle {
        self.handle.clone()
    }

    /// The bound host address of each configured forward, in config order (useful with `host_port: 0`).
    pub fn forward_addrs(&self) -> &[SocketAddr] {
        &self.fwd_addrs
    }

    /// Serve until [`UserNetHandle::shutdown`] or a fatal socket error. Must run inside a tokio runtime.
    pub async fn run(self) -> io::Result<()> {
        let sock = UnixDatagram::from_std(self.sock)?;
        let (ev_tx, ev_rx) = mpsc::channel(2048);
        let mut fwd_tasks = Vec::new();
        for (idx, l) in self.tcp_fwd {
            let l = TcpListener::from_std(l)?;
            let tx = ev_tx.clone();
            fwd_tasks.push(
                tokio::spawn(async move {
                    while let Ok((stream, peer)) = l.accept().await {
                        if tx.send(Ev::ForwardAccepted { idx, stream, peer }).await.is_err() {
                            break;
                        }
                    }
                })
                .abort_handle(),
            );
        }
        let mut udp_fwd_socks = HashMap::new();
        for (idx, s) in self.udp_fwd {
            udp_fwd_socks.insert(idx, Arc::new(s.try_clone()?));
            let s = UdpSocket::from_std(s)?;
            let tx = ev_tx.clone();
            fwd_tasks.push(
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65535];
                    while let Ok((n, peer)) = s.recv_from(&mut buf).await {
                        let data = buf[..n].to_vec();
                        if tx.send(Ev::UdpFwdIn { idx, peer, data }).await.is_err() {
                            break;
                        }
                    }
                })
                .abort_handle(),
            );
        }
        let upstreams = if self.cfg.dns_upstreams.is_empty() {
            system_resolvers()
        } else {
            self.cfg.dns_upstreams.clone()
        };
        let mut engine = Engine {
            cfg: self.cfg,
            sock,
            ev_tx,
            log: self.handle.log.clone(),
            guest_mac: None,
            flows: HashMap::new(),
            keys: HashMap::new(),
            udp: HashMap::new(),
            udp_fwd_socks,
            udp_sessions: HashMap::new(),
            udp_rev: HashMap::new(),
            learned: HashMap::new(),
            dns_sem: Arc::new(Semaphore::new(64)),
            upstreams: Arc::new(upstreams),
            next_id: 1,
            next_port: 49152,
        };
        let r = engine.run_loop(ev_rx, self.handle.shutdown.clone()).await;
        for t in fwd_tasks {
            t.abort();
        }
        r
    }
}

fn system_resolvers() -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()? == "nameserver")
                .then(|| it.next()?.parse::<Ipv4Addr>().ok())
                .flatten()
        })
        .map(|ip| SocketAddr::new(ip.into(), 53))
        .collect();
    if v.is_empty() {
        v.push(SocketAddr::new(Ipv4Addr::new(1, 1, 1, 1).into(), 53));
    }
    v
}

// ---------------------------------------------------------------------------------------------------------------

const CHUNK: u32 = 16 * 1024;
const BUDGET: usize = 128 * 1024;
const MAX_QUEUED_TO_HOST: usize = 256 * 1024;

enum Ev {
    TcpConnected { id: u64, stream: TcpStream },
    TcpConnectFailed { id: u64, reason: String },
    TcpData { id: u64, data: Vec<u8> },
    TcpHostEof { id: u64 },
    TcpHostErr { id: u64 },
    ForwardAccepted { idx: usize, stream: TcpStream, peer: SocketAddr },
    UdpReply { key: UKey, data: Vec<u8> },
    UdpFwdIn { idx: usize, peer: SocketAddr, data: Vec<u8> },
    DnsReply { local_ip: Ipv4Addr, guest_port: u16, query: Vec<u8>, answer: Option<Vec<u8>> },
}

type TKey = (u16, Ipv4Addr, u16); // guest port, remote ip, remote port
type UKey = TKey;

struct TcpFlow {
    conn: TcpConn,
    key: TKey,
    dir: Direction,
    /// Where the host side really is (after the gateway->loopback alias) or the forward's peer.
    host_peer: SocketAddr,
    opened: bool,
    to_host: Option<mpsc::UnboundedSender<Option<Vec<u8>>>>,
    queued: Arc<AtomicUsize>,
    budget: Arc<Semaphore>,
    tasks: Vec<AbortHandle>,
}

impl Drop for TcpFlow {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

struct UdpFlow {
    sock: Arc<std::net::UdpSocket>,
    task: AbortHandle,
    last: Instant,
    up: u64,
    down: u64,
}

impl Drop for UdpFlow {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Engine {
    cfg: UserNetConfig,
    sock: UnixDatagram,
    ev_tx: mpsc::Sender<Ev>,
    log: broadcast::Sender<FlowEvent>,
    guest_mac: Option<Mac>,
    flows: HashMap<u64, TcpFlow>,
    keys: HashMap<TKey, u64>,
    udp: HashMap<UKey, UdpFlow>,
    udp_fwd_socks: HashMap<usize, Arc<std::net::UdpSocket>>,
    /// (forward idx, host peer) -> gateway-side port the guest sees.
    udp_sessions: HashMap<(usize, SocketAddr), u16>,
    udp_rev: HashMap<u16, (usize, SocketAddr, Instant)>,
    learned: HashMap<Ipv4Addr, Instant>,
    dns_sem: Arc<Semaphore>,
    upstreams: Arc<Vec<SocketAddr>>,
    next_id: u64,
    next_port: u16,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn tcp_reader(
    id: u64,
    mut r: OwnedReadHalf,
    budget: Arc<Semaphore>,
    ev: mpsc::Sender<Ev>,
) {
    loop {
        let Ok(permit) = budget.acquire_many(CHUNK).await else {
            return;
        };
        let mut buf = vec![0u8; CHUNK as usize];
        match r.read(&mut buf).await {
            Ok(0) => {
                let _ = ev.send(Ev::TcpHostEof { id }).await;
                return;
            }
            Ok(n) => {
                permit.forget();
                budget.add_permits(CHUNK as usize - n);
                buf.truncate(n);
                if ev.send(Ev::TcpData { id, data: buf }).await.is_err() {
                    return;
                }
            }
            Err(_) => {
                let _ = ev.send(Ev::TcpHostErr { id }).await;
                return;
            }
        }
    }
}

async fn tcp_writer(
    id: u64,
    mut w: OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<Option<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    ev: mpsc::Sender<Ev>,
) {
    while let Some(m) = rx.recv().await {
        match m {
            Some(d) => {
                let n = d.len();
                let r = w.write_all(&d).await;
                queued.fetch_sub(n, Ordering::Relaxed);
                if r.is_err() {
                    let _ = ev.send(Ev::TcpHostErr { id }).await;
                    return;
                }
            }
            None => {
                let _ = w.shutdown().await;
            }
        }
    }
}

async fn resolve(upstreams: &[SocketAddr], q: &[u8]) -> Option<Vec<u8>> {
    for up in upstreams {
        let bind = if up.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let Ok(s) = UdpSocket::bind(bind).await else {
            continue;
        };
        if s.connect(up).await.is_err() || s.send(q).await.is_err() {
            continue;
        }
        let mut buf = vec![0u8; 4096];
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), s.recv(&mut buf)).await
            && n >= 12
            && buf[..2] == q[..2]
        {
            buf.truncate(n);
            return Some(buf);
        }
    }
    None
}

impl Engine {
    async fn run_loop(
        &mut self,
        mut ev_rx: mpsc::Receiver<Ev>,
        shutdown: Arc<Notify>,
    ) -> io::Result<()> {
        let mut buf = vec![0u8; 65536];
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.notified() => return Ok(()),
                r = self.sock.recv(&mut buf) => {
                    match r {
                        Ok(0) => {}
                        Ok(n) => self.on_frame(&buf[..n]),
                        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                        Err(e) => return Err(e),
                    }
                }
                Some(ev) = ev_rx.recv() => self.on_event(ev),
                _ = tick.tick() => self.on_tick(),
            }
        }
    }

    // ----- output ------------------------------------------------------------------------------------------

    fn emit_frame(&self, ethertype: u16, dst: Mac, payload: &[u8]) {
        let f = wire::build_eth(dst, self.cfg.gateway_mac, ethertype, payload);
        // Datagram semantics: a full socket buffer drops the frame; TCP recovers by retransmission.
        let _ = self.sock.try_send(&f);
    }

    fn send_ip(&self, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4: &[u8]) {
        let ip = wire::build_ipv4(src, dst, proto, l4);
        let mac = self.guest_mac.unwrap_or(BROADCAST);
        let dst_mac = if dst.is_broadcast() { BROADCAST } else { mac };
        self.emit_frame(wire::ETH_IPV4, dst_mac, &ip);
    }

    fn send_udp(&self, src: Ipv4Addr, sport: u16, dst: Ipv4Addr, dport: u16, data: &[u8]) {
        if data.len() + 28 > usize::from(self.cfg.mtu) {
            tracing::debug!(len = data.len(), "usernet: dropping oversized udp datagram");
            return;
        }
        let u = wire::build_udp(src, dst, sport, dport, data);
        self.send_ip(src, dst, wire::PROTO_UDP, &u);
    }

    fn flush_tcp(&self, key: TKey, out: Vec<TcpOut>) {
        for o in out {
            let seg = wire::build_tcp(key.1, self.cfg.guest_ip, key.2, key.0, &o);
            self.send_ip(key.1, self.cfg.guest_ip, wire::PROTO_TCP, &seg);
        }
    }

    fn log(&self, kind: FlowKind, proto: Proto, dir: Direction, guest: SocketAddr, remote: SocketAddr) -> FlowEvent {
        FlowEvent {
            ts_ms: now_ms(),
            kind,
            proto,
            direction: dir,
            guest,
            remote,
            reason: None,
            name: None,
            bytes_tx: 0,
            bytes_rx: 0,
        }
    }

    fn publish(&self, e: FlowEvent) {
        tracing::debug!(?e, "usernet flow");
        let _ = self.log.send(e);
    }

    fn guest_addr(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.cfg.guest_ip.into(), port)
    }

    fn deny(&self, proto: Proto, gport: u16, rip: Ipv4Addr, rport: u16, reason: &str) {
        let mut e = self.log(
            FlowKind::Deny,
            proto,
            Direction::Outbound,
            self.guest_addr(gport),
            SocketAddr::new(rip.into(), rport),
        );
        e.reason = Some(reason.to_string());
        self.publish(e);
    }

    // ----- input -------------------------------------------------------------------------------------------

    fn on_frame(&mut self, f: &[u8]) {
        let Some(eth) = wire::parse_eth(f) else { return };
        if eth.src[0] & 1 != 0 || eth.src == [0; 6] {
            return;
        }
        match self.cfg.guest_mac.or(self.guest_mac) {
            Some(m) if m != eth.src => return,
            Some(_) => {}
            None => self.guest_mac = Some(eth.src),
        }
        if eth.dst != self.cfg.gateway_mac && eth.dst != BROADCAST {
            return; // multicast and strays are not ours
        }
        match eth.ethertype {
            wire::ETH_ARP => self.on_arp(eth.src, eth.payload),
            wire::ETH_IPV4 => self.on_ipv4(eth.src, eth.payload),
            _ => {}
        }
    }

    fn on_arp(&mut self, src_mac: Mac, p: &[u8]) {
        let Some((op, smac, sip, tip)) = wire::parse_arp(p) else { return };
        if op != 1 || smac != src_mac {
            return;
        }
        if tip == self.cfg.gateway_ip || tip == self.cfg.dns_ip {
            let r = wire::build_arp(2, self.cfg.gateway_mac, tip, smac, sip);
            self.emit_frame(wire::ETH_ARP, smac, &r);
        }
    }

    fn on_ipv4(&mut self, src_mac: Mac, p: &[u8]) {
        let Some(ip) = wire::parse_ipv4(p) else { return };
        if ip.fragment {
            tracing::debug!("usernet: dropping IPv4 fragment");
            return;
        }
        if ip.total_len > usize::from(self.cfg.mtu) {
            tracing::debug!(len = ip.total_len, "usernet: dropping packet over MTU");
            return;
        }
        if ip.proto == wire::PROTO_UDP {
            if let Some(u) = wire::parse_udp(ip.payload)
                && u.dport == 67
            {
                self.on_dhcp(src_mac, u.payload);
                return;
            }
        }
        if ip.src != self.cfg.guest_ip {
            return; // anti-spoofing: the lease is the only source address
        }
        match ip.proto {
            wire::PROTO_ICMP => self.on_icmp(&ip),
            wire::PROTO_UDP => {
                if let Some(u) = wire::parse_udp(ip.payload) {
                    self.on_udp(&ip, &u);
                }
            }
            wire::PROTO_TCP => {
                if let Some(t) = wire::parse_tcp(ip.payload) {
                    self.on_tcp(&ip, &t);
                }
            }
            _ => {}
        }
    }

    fn on_icmp(&mut self, ip: &wire::Ipv4<'_>) {
        let p = ip.payload;
        if p.len() >= 8 && p[0] == 8 && p[1] == 0 && (ip.dst == self.cfg.gateway_ip || ip.dst == self.cfg.dns_ip) {
            let mut r = p.to_vec();
            r[0] = 0;
            r[2] = 0;
            r[3] = 0;
            let c = wire::checksum(&r);
            r[2..4].copy_from_slice(&c.to_be_bytes());
            self.send_ip(ip.dst, ip.src, wire::PROTO_ICMP, &r);
        }
        // Echo to the outside would need raw/ICMP datagram sockets; not supported.
    }

    fn on_dhcp(&mut self, src_mac: Mac, p: &[u8]) {
        let Some(m) = dhcp::parse(p) else { return };
        if m.chaddr != src_mac {
            return;
        }
        let kind = match m.msg_type {
            dhcp::DISCOVER => dhcp::OFFER,
            dhcp::REQUEST => {
                let want = m.requested_ip.unwrap_or(m.ciaddr);
                if want == self.cfg.guest_ip {
                    dhcp::ACK
                } else {
                    dhcp::NAK
                }
            }
            _ => return,
        };
        let r = dhcp::reply(&self.cfg, &m, kind);
        let u = wire::build_udp(self.cfg.gateway_ip, Ipv4Addr::BROADCAST, 67, 68, &r);
        let ip = wire::build_ipv4(self.cfg.gateway_ip, Ipv4Addr::BROADCAST, wire::PROTO_UDP, &u);
        self.emit_frame(wire::ETH_IPV4, BROADCAST, &ip);
    }

    fn alloc_port(&mut self) -> u16 {
        loop {
            let p = self.next_port;
            self.next_port = if p == 65535 { 49152 } else { p + 1 };
            let gw = self.cfg.gateway_ip;
            let used = self.keys.keys().any(|k| k.1 == gw && k.2 == p) || self.udp_rev.contains_key(&p);
            if !used {
                return p;
            }
        }
    }

    // ----- UDP ---------------------------------------------------------------------------------------------

    fn on_udp(&mut self, ip: &wire::Ipv4<'_>, u: &wire::Udp<'_>) {
        let c = &self.cfg;
        if (ip.dst == c.dns_ip || ip.dst == c.gateway_ip) && u.dport == 53 {
            return self.on_dns(ip.dst, u);
        }
        if ip.dst == c.gateway_ip
            && let Some((idx, peer, last)) = self.udp_rev.get_mut(&u.dport)
        {
            *last = Instant::now();
            if let Some(s) = self.udp_fwd_socks.get(idx) {
                let _ = s.send_to(u.payload, *peer);
            }
            return;
        }
        if ip.dst.is_broadcast() || ip.dst.is_multicast() {
            return;
        }
        let key: UKey = (u.sport, ip.dst, u.dport);
        let now = Instant::now();
        if let Some(f) = self.udp.get_mut(&key) {
            f.last = now;
            f.up += u.payload.len() as u64;
            let _ = f.sock.send(u.payload);
            return;
        }
        // New flow: policy first.
        let target = match self.translate(ip.dst, u.dport, now) {
            Ok(t) => t,
            Err(why) => return self.deny(Proto::Udp, u.sport, ip.dst, u.dport, why),
        };
        if self.udp.len() >= self.cfg.max_udp_flows {
            return self.deny(Proto::Udp, u.sport, ip.dst, u.dport, "flow-limit");
        }
        // Sends go through the std socket (a fresh tokio socket reports WouldBlock to `try_send` until its
        // first readiness event); a clone feeds the receive task.
        let socks = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
            s.set_nonblocking(true)?;
            s.connect(target)?;
            let r = UdpSocket::from_std(s.try_clone()?)?;
            Ok((s, r))
        });
        let (sock, r) = match socks {
            Ok((s, r)) => (Arc::new(s), r),
            Err(e) => return self.deny(Proto::Udp, u.sport, ip.dst, u.dport, &format!("socket: {e}")),
        };
        let _ = sock.send(u.payload);
        let tx = self.ev_tx.clone();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            while let Ok(n) = r.recv(&mut buf).await {
                if tx.send(Ev::UdpReply { key, data: buf[..n].to_vec() }).await.is_err() {
                    break;
                }
            }
        })
        .abort_handle();
        let mut e = self.log(
            FlowKind::Open,
            Proto::Udp,
            Direction::Outbound,
            self.guest_addr(u.sport),
            SocketAddr::new(ip.dst.into(), u.dport),
        );
        e.reason = None;
        self.publish(e);
        self.udp.insert(
            key,
            UdpFlow {
                sock,
                task,
                last: now,
                up: u.payload.len() as u64,
                down: 0,
            },
        );
    }

    /// Apply the egress policy and the gateway->loopback alias. Err is the deny reason.
    fn translate(&self, dst: Ipv4Addr, port: u16, now: Instant) -> Result<SocketAddr, &'static str> {
        let c = &self.cfg;
        if dst == c.gateway_ip {
            if !c.egress.allow_host_loopback {
                return Err("host-loopback");
            }
            return match c.egress.check_ports(port) {
                Decision::Allow => Ok(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)),
                Decision::Deny(r) => Err(r),
            };
        }
        if dst == c.dns_ip {
            return Err("gateway-service");
        }
        match c.egress.check(dst, port, &self.learned, now) {
            Decision::Allow => Ok(SocketAddr::new(dst.into(), port)),
            Decision::Deny(r) => Err(r),
        }
    }

    fn on_dns(&mut self, local_ip: Ipv4Addr, u: &wire::Udp<'_>) {
        let Some(q) = dns::parse_query(u.payload) else { return };
        let guest = self.guest_addr(u.sport);
        let remote = SocketAddr::new(local_ip.into(), 53);
        if let Decision::Deny(why) = self.cfg.egress.check_domain(&q.name) {
            let mut e = self.log(FlowKind::DnsDeny, Proto::Udp, Direction::Outbound, guest, remote);
            e.name = Some(q.name.clone());
            e.reason = Some(why.into());
            self.publish(e);
            let r = dns::build_reply(u.payload, &q, 3, false);
            self.send_udp(local_ip, 53, self.cfg.guest_ip, u.sport, &r);
            return;
        }
        let mut e = self.log(FlowKind::DnsQuery, Proto::Udp, Direction::Outbound, guest, remote);
        e.name = Some(q.name.clone());
        self.publish(e);
        let Ok(permit) = self.dns_sem.clone().try_acquire_owned() else {
            return;
        };
        let query = u.payload.to_vec();
        let ups = self.upstreams.clone();
        let tx = self.ev_tx.clone();
        let guest_port = u.sport;
        tokio::spawn(async move {
            let answer = resolve(&ups, &query).await;
            drop(permit);
            let _ = tx
                .send(Ev::DnsReply { local_ip, guest_port, query, answer })
                .await;
        });
    }

    // ----- TCP ---------------------------------------------------------------------------------------------

    fn mss(&self) -> usize {
        usize::from(self.cfg.mtu) - 40
    }

    fn rst_for(&self, ip: &wire::Ipv4<'_>, t: &wire::Tcp<'_>) {
        if t.flags & RST != 0 {
            return;
        }
        let o = if t.flags & ACK != 0 {
            TcpOut { seq: t.ack, flags: RST, ..Default::default() }
        } else {
            let len = t.payload.len() as u32 + u32::from(t.flags & SYN != 0) + u32::from(t.flags & FIN != 0);
            TcpOut {
                seq: 0,
                ack: t.seq.wrapping_add(len),
                flags: RST | ACK,
                ..Default::default()
            }
        };
        let seg = wire::build_tcp(ip.dst, ip.src, t.dport, t.sport, &o);
        self.send_ip(ip.dst, ip.src, wire::PROTO_TCP, &seg);
    }

    fn on_tcp(&mut self, ip: &wire::Ipv4<'_>, t: &wire::Tcp<'_>) {
        let key: TKey = (t.sport, ip.dst, t.dport);
        let now = Instant::now();
        if let Some(&id) = self.keys.get(&key) {
            let Some(f) = self.flows.get_mut(&id) else { return };
            let can = f.to_host.is_some() && f.queued.load(Ordering::Relaxed) < MAX_QUEUED_TO_HOST;
            let mut out = Vec::new();
            let res = f.conn.on_segment(t, can, now, &mut out);
            if let Some(d) = res.data
                && let Some(tx) = &f.to_host
            {
                f.queued.fetch_add(d.len(), Ordering::Relaxed);
                let _ = tx.send(Some(d));
            }
            if res.guest_fin
                && let Some(tx) = &f.to_host
            {
                let _ = tx.send(None);
            }
            if res.acked > 0 {
                f.budget.add_permits(res.acked);
            }
            let closed = f.conn.state == State::Closed;
            self.flush_tcp(key, out);
            if closed {
                self.remove_flow(id);
            }
            return;
        }
        if t.flags & (SYN | ACK | RST) != SYN {
            return self.rst_for(ip, t);
        }
        // New outbound connection.
        if (ip.dst == self.cfg.dns_ip || ip.dst == self.cfg.gateway_ip) && t.dport == 53 {
            self.deny(Proto::Tcp, t.sport, ip.dst, t.dport, "tcp-dns-unsupported");
            return self.rst_for(ip, t);
        }
        let target = match self.translate(ip.dst, t.dport, now) {
            Ok(a) => a,
            Err(why) => {
                self.deny(Proto::Tcp, t.sport, ip.dst, t.dport, why);
                return self.rst_for(ip, t);
            }
        };
        if self.flows.len() >= self.cfg.max_tcp_flows {
            self.deny(Proto::Tcp, t.sport, ip.dst, t.dport, "flow-limit");
            return self.rst_for(ip, t);
        }
        let id = self.next_id;
        self.next_id += 1;
        let ev = self.ev_tx.clone();
        let to = Duration::from_secs(self.cfg.connect_timeout_secs);
        let task = tokio::spawn(async move {
            let r = tokio::time::timeout(to, TcpStream::connect(target)).await;
            let m = match r {
                Ok(Ok(stream)) => Ev::TcpConnected { id, stream },
                Ok(Err(e)) => Ev::TcpConnectFailed { id, reason: e.to_string() },
                Err(_) => Ev::TcpConnectFailed { id, reason: "connect timeout".into() },
            };
            let _ = ev.send(m).await;
        })
        .abort_handle();
        self.keys.insert(key, id);
        self.flows.insert(
            id,
            TcpFlow {
                conn: TcpConn::new_passive(t, self.mss(), now),
                key,
                dir: Direction::Outbound,
                host_peer: target,
                opened: false,
                to_host: None,
                queued: Arc::new(AtomicUsize::new(0)),
                budget: Arc::new(Semaphore::new(BUDGET)),
                tasks: vec![task],
            },
        );
    }

    fn attach_host_stream(&mut self, id: u64, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        let (r, w) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        let Some(f) = self.flows.get_mut(&id) else { return };
        f.to_host = Some(tx);
        f.tasks.push(
            tokio::spawn(tcp_reader(id, r, f.budget.clone(), self.ev_tx.clone())).abort_handle(),
        );
        f.tasks.push(
            tokio::spawn(tcp_writer(id, w, rx, f.queued.clone(), self.ev_tx.clone())).abort_handle(),
        );
    }

    fn remove_flow(&mut self, id: u64) {
        let Some(f) = self.flows.remove(&id) else { return };
        self.keys.remove(&f.key);
        if f.opened {
            let remote = SocketAddr::new(f.key.1.into(), f.key.2);
            let mut e = self.log(FlowKind::Close, Proto::Tcp, f.dir, self.guest_addr(f.key.0), remote);
            e.bytes_tx = f.conn.bytes_up;
            e.bytes_rx = f.conn.bytes_down;
            self.publish(e);
        }
    }

    fn open_event(&self, f: &TcpFlow) {
        let mut e = self.log(
            FlowKind::Open,
            Proto::Tcp,
            f.dir,
            self.guest_addr(f.key.0),
            match f.dir {
                Direction::Outbound => SocketAddr::new(f.key.1.into(), f.key.2),
                Direction::Inbound => f.host_peer,
            },
        );
        if f.dir == Direction::Outbound && f.host_peer.ip() != f.key.1 {
            e.reason = Some(format!("via {}", f.host_peer));
        }
        self.publish(e);
    }

    // ----- host-side events --------------------------------------------------------------------------------

    fn on_event(&mut self, ev: Ev) {
        let now = Instant::now();
        match ev {
            Ev::TcpConnected { id, stream } => {
                if !self.flows.contains_key(&id) {
                    return;
                }
                self.attach_host_stream(id, stream);
                let Some(f) = self.flows.get_mut(&id) else { return };
                f.opened = true;
                let mut out = Vec::new();
                f.conn.host_connected(now, &mut out);
                let key = f.key;
                self.flush_tcp(key, out);
                if let Some(f) = self.flows.get(&id) {
                    self.open_event(f);
                }
            }
            Ev::TcpConnectFailed { id, reason } => {
                let Some(f) = self.flows.get_mut(&id) else { return };
                let key = f.key;
                let mut out = Vec::new();
                f.conn.rst(&mut out);
                // A refused connect resets the guest's SYN: seq 0, ack = its ISN + 1 is what rst() produces.
                let mut e = self.log(
                    FlowKind::ConnectFailed,
                    Proto::Tcp,
                    Direction::Outbound,
                    self.guest_addr(key.0),
                    SocketAddr::new(key.1.into(), key.2),
                );
                e.reason = Some(reason);
                self.publish(e);
                self.flush_tcp(key, out);
                self.remove_flow(id);
            }
            Ev::TcpData { id, data } => {
                let Some(f) = self.flows.get_mut(&id) else { return };
                let mut out = Vec::new();
                f.conn.push_host_data(&data, now, &mut out);
                let key = f.key;
                self.flush_tcp(key, out);
            }
            Ev::TcpHostEof { id } => {
                let Some(f) = self.flows.get_mut(&id) else { return };
                let mut out = Vec::new();
                f.conn.host_eof(now, &mut out);
                let key = f.key;
                self.flush_tcp(key, out);
            }
            Ev::TcpHostErr { id } => {
                let Some(f) = self.flows.get_mut(&id) else { return };
                let mut out = Vec::new();
                f.conn.rst(&mut out);
                let key = f.key;
                self.flush_tcp(key, out);
                self.remove_flow(id);
            }
            Ev::ForwardAccepted { idx, stream, peer } => {
                let fwd = &self.cfg.forwards[idx];
                let gport = fwd.guest_port;
                let gw_port = self.alloc_port();
                let key: TKey = (gport, self.cfg.gateway_ip, gw_port);
                if self.flows.len() >= self.cfg.max_tcp_flows {
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                let mut out = Vec::new();
                let conn = TcpConn::new_active(self.mss(), now, &mut out);
                self.keys.insert(key, id);
                self.flows.insert(
                    id,
                    TcpFlow {
                        conn,
                        key,
                        dir: Direction::Inbound,
                        host_peer: peer,
                        opened: true,
                        to_host: None,
                        queued: Arc::new(AtomicUsize::new(0)),
                        budget: Arc::new(Semaphore::new(BUDGET)),
                        tasks: Vec::new(),
                    },
                );
                self.attach_host_stream(id, stream);
                self.flush_tcp(key, out);
                if let Some(f) = self.flows.get(&id) {
                    self.open_event(f);
                }
            }
            Ev::UdpReply { key, data } => {
                let Some(f) = self.udp.get_mut(&key) else { return };
                f.last = now;
                f.down += data.len() as u64;
                self.send_udp(key.1, key.2, self.cfg.guest_ip, key.0, &data);
            }
            Ev::UdpFwdIn { idx, peer, data } => {
                let gport = self.cfg.forwards[idx].guest_port;
                let gw_port = match self.udp_sessions.get(&(idx, peer)) {
                    Some(&p) => p,
                    None => {
                        if self.udp_sessions.len() >= self.cfg.max_udp_flows {
                            return;
                        }
                        let p = self.alloc_port();
                        self.udp_sessions.insert((idx, peer), p);
                        let mut e = self.log(
                            FlowKind::Open,
                            Proto::Udp,
                            Direction::Inbound,
                            self.guest_addr(gport),
                            peer,
                        );
                        e.reason = None;
                        self.publish(e);
                        p
                    }
                };
                self.udp_rev.insert(gw_port, (idx, peer, now));
                self.send_udp(self.cfg.gateway_ip, gw_port, self.cfg.guest_ip, gport, &data);
            }
            Ev::DnsReply { local_ip, guest_port, query, answer } => {
                let Some(q) = dns::parse_query(&query) else { return };
                let reply = match answer {
                    None => dns::build_reply(&query, &q, 2, false), // SERVFAIL
                    Some(a) => {
                        if !self.cfg.egress.allow_domains.is_empty() {
                            for (ip, ttl) in dns::parse_a_records(&a) {
                                let ttl = Duration::from_secs(u64::from(ttl.clamp(60, 3600)));
                                self.learned.insert(ip, now + ttl);
                            }
                        }
                        if a.len() + 28 > usize::from(self.cfg.mtu) {
                            dns::build_reply(&query, &q, 0, true)
                        } else {
                            a
                        }
                    }
                };
                self.send_udp(local_ip, 53, self.cfg.guest_ip, guest_port, &reply);
            }
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        let idle = Duration::from_secs(self.cfg.tcp_idle_secs);
        let mut work = Vec::new();
        for (id, f) in self.flows.iter_mut() {
            let mut out = Vec::new();
            f.conn.tick(now, idle, &mut out);
            let closed = f.conn.state == State::Closed;
            if !out.is_empty() || closed {
                work.push((*id, f.key, out, closed));
            }
        }
        for (id, key, out, closed) in work {
            self.flush_tcp(key, out);
            if closed {
                self.remove_flow(id);
            }
        }
        let udp_idle = Duration::from_secs(self.cfg.udp_idle_secs);
        let expired: Vec<UKey> = self
            .udp
            .iter()
            .filter(|(_, f)| now.duration_since(f.last) > udp_idle)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            if let Some(f) = self.udp.remove(&k) {
                let mut e = self.log(
                    FlowKind::Close,
                    Proto::Udp,
                    Direction::Outbound,
                    self.guest_addr(k.0),
                    SocketAddr::new(k.1.into(), k.2),
                );
                e.bytes_tx = f.up;
                e.bytes_rx = f.down;
                                self.publish(e);
            }
        }
        self.udp_rev.retain(|_, (_, _, last)| now.duration_since(*last) <= udp_idle);
        let rev = &self.udp_rev;
        self.udp_sessions.retain(|_, p| rev.contains_key(p));
        self.learned.retain(|_, exp| *exp > now);
    }
}

#[cfg(test)]
mod tests;
