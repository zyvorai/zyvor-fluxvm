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
//! * a DNS forwarder over UDP and TCP with allow/deny domain policy; answers for allow-listed names (A and AAAA)
//!   teach the egress policy which addresses are reachable. Upstream queries go over UDP and retry over TCP when
//!   the answer is truncated; a UDP answer that does not fit the client's buffer is replaced by a TC reply so the
//!   client retries over TCP, which the gateway also serves;
//! * TCP and UDP NAT to the host network: guest TCP is terminated in userspace and relayed over ordinary host
//!   sockets, so nothing needs privileges and every flow passes the [`EgressPolicy`]. TCP uses window scaling,
//!   SACK blocks towards the guest and NewReno congestion control (see the `tcp` module docs for the limits);
//! * outbound ICMP echo to allowed public IPv4 hosts through unprivileged ICMP datagram sockets, rate limited;
//!   when such a socket cannot be created the request is denied with reason `icmp-socket-unavailable` and the
//!   guest sees an ordinary timeout;
//! * IPv4 fragment reassembly (bounded, with a timeout) for guest packets, fragmentation of oversized UDP and ICMP
//!   towards the guest when DF is clear, and ICMP "fragmentation needed" for guest packets over the MTU with DF set;
//! * optional IPv6 (`UserNetConfig::ipv6`): NDP, router advertisements with a ULA /64 and RDNSS (SLAAC), ICMPv6
//!   echo to the gateway, TCP/UDP NAT and DNS over the same egress policy (v6 CIDR lists, ULA/link-local/loopback
//!   denied by default). Outbound ICMPv6 echo, v6 fragments and v6 port forwards are not supported;
//! * host -> guest TCP and UDP port forwards (IPv4 guest address);
//! * a [`FlowEvent`] stream (open, close, deny, DNS) on a broadcast channel, and an optional frame tap with
//!   per-frame policy verdicts ([`UserNet::set_tap_version`]).
//!
//! # Wiring
//!
//! Either linked into the daemon (`UserNet::from_fd`) or via the `fluxvm-usernet --fd N --config cfg.json` binary
//! spawned by the runner.

mod config;
mod dhcp;
mod dns;
mod frag;
mod icmp;
mod tcp;
mod v6;
pub mod wire;

pub use config::{
    Cidr, Cidr6, Decision, EgressPolicy, PortForward, Proto, UserNetConfig, is_non_unicast,
    is_non_unicast6, is_private, is_private6,
};

use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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
    tap: Option<StdUnixDatagram>,
    tap_version: u8,
}

/// First byte of a frame-tap datagram: the frame crossed the guest NIC in this direction.
pub const TAP_FROM_GUEST: u8 = 0;
pub const TAP_TO_GUEST: u8 = 1;

/// Tap framing version 1 (the default): `[dir][frame]`.
pub const TAP_VERSION_1: u8 = 1;
/// Tap framing version 2: `[dir][verdict][frame]`, see the `TAP_VERDICT_*` constants.
pub const TAP_VERSION_2: u8 = 2;

/// The frame was processed normally (this is also the verdict of every frame sent to the guest).
pub const TAP_VERDICT_PASS: u8 = 0;
/// The frame was dropped or answered with a refusal by the egress policy (a denied flow, a denied DNS name, a
/// rate- or flow-limit refusal).
pub const TAP_VERDICT_DENIED: u8 = 1;
/// The frame was dropped for a non-policy reason: spoofed source, malformed, over the MTU, fragment timeout.
pub const TAP_VERDICT_DROPPED: u8 = 2;

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
                Proto::Icmp => unreachable!("validate() rejects icmp forwards"),
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
            tap: None,
            tap_version: TAP_VERSION_1,
        })
    }

    /// Copies every frame the guest sends or receives to `tap` (a `SOCK_DGRAM` Unix socket), one datagram each,
    /// prefixed by [`TAP_FROM_GUEST`] / [`TAP_TO_GUEST`] (and, with [`TAP_VERSION_2`], a verdict byte). Sends never
    /// block; a full tap socket drops the copy.
    pub fn set_tap(&mut self, tap: StdUnixDatagram) -> io::Result<()> {
        tap.set_nonblocking(true)?;
        self.tap = Some(tap);
        Ok(())
    }

    /// Choose the tap framing: [`TAP_VERSION_1`] (default) `[dir][frame]`, or [`TAP_VERSION_2`]
    /// `[dir][verdict][frame]` where the verdict says whether the egress policy (or another check) dropped the
    /// frame. Frames from the guest are tapped when their fate is known, but before any reply they provoke.
    pub fn set_tap_version(&mut self, version: u8) -> io::Result<()> {
        if version != TAP_VERSION_1 && version != TAP_VERSION_2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown tap version {version}"),
            ));
        }
        self.tap_version = version;
        Ok(())
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
        let icmp_tokens = f64::from(self.cfg.icmp_rate_per_sec) * 2.0;
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
            learned6: HashMap::new(),
            dns_sem: Arc::new(Semaphore::new(64)),
            upstreams: Arc::new(upstreams),
            next_id: 1,
            next_port: 49152,
            tap: self.tap,
            tap_version: self.tap_version,
            pending_tap: RefCell::new(None),
            ip_id: Cell::new(1),
            reasm: frag::Reassembler::default(),
            icmp: HashMap::new(),
            icmp_bucket: (icmp_tokens, Instant::now()),
            last_ra: None,
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
                .then(|| it.next()?.parse::<IpAddr>().ok())
                .flatten()
        })
        .map(|ip| SocketAddr::new(ip, 53))
        .collect();
    if v.is_empty() {
        v.push(SocketAddr::new(Ipv4Addr::new(1, 1, 1, 1).into(), 53));
    }
    v
}

// ---------------------------------------------------------------------------------------------------------------

const CHUNK: u32 = 16 * 1024;
const BUDGET: usize = 256 * 1024;
const MAX_QUEUED_TO_HOST: usize = 256 * 1024;
const DNS_TCP_IDLE: Duration = Duration::from_secs(30);
const DNS_TCP_MAX_BUF: usize = 2 + 65535;

/// How a DNS answer travels back to the guest.
#[derive(Clone, Copy, Debug)]
enum DnsVia {
    Udp,
    /// Length-prefixed on this TCP flow.
    Tcp(u64),
}

enum Ev {
    TcpConnected { id: u64, stream: TcpStream },
    TcpConnectFailed { id: u64, reason: String },
    TcpData { id: u64, data: Vec<u8> },
    TcpHostEof { id: u64 },
    TcpHostErr { id: u64 },
    ForwardAccepted { idx: usize, stream: TcpStream, peer: SocketAddr },
    UdpReply { key: FKey, data: Vec<u8> },
    UdpFwdIn { idx: usize, peer: SocketAddr, data: Vec<u8> },
    DnsReply {
        via: DnsVia,
        local_ip: IpAddr,
        guest: IpAddr,
        guest_port: u16,
        query: Vec<u8>,
        answer: Option<Vec<u8>>,
    },
    IcmpReply { key: icmp::IcmpKey, data: Vec<u8> },
}

/// Flow identity: guest address and port, remote address and port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FKey {
    g: IpAddr,
    gp: u16,
    r: IpAddr,
    rp: u16,
}

/// Per-connection state of a DNS-over-TCP session terminated at the gateway.
struct DnsTcp {
    buf: Vec<u8>,
    /// Queries sent upstream and not yet answered.
    pending: usize,
    /// The guest closed its side; close ours after the last answer.
    fin: bool,
    local_ip: IpAddr,
}

struct TcpFlow {
    conn: TcpConn,
    key: FKey,
    dir: Direction,
    /// Where the host side really is (after the gateway->loopback alias) or the forward's peer.
    host_peer: SocketAddr,
    opened: bool,
    to_host: Option<mpsc::UnboundedSender<Option<Vec<u8>>>>,
    queued: Arc<AtomicUsize>,
    budget: Arc<Semaphore>,
    tasks: Vec<AbortHandle>,
    dns: Option<DnsTcp>,
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

pub(crate) struct Engine {
    cfg: UserNetConfig,
    sock: UnixDatagram,
    ev_tx: mpsc::Sender<Ev>,
    log: broadcast::Sender<FlowEvent>,
    guest_mac: Option<Mac>,
    flows: HashMap<u64, TcpFlow>,
    keys: HashMap<FKey, u64>,
    udp: HashMap<FKey, UdpFlow>,
    udp_fwd_socks: HashMap<usize, Arc<std::net::UdpSocket>>,
    /// (forward idx, host peer) -> gateway-side port the guest sees.
    udp_sessions: HashMap<(usize, SocketAddr), u16>,
    udp_rev: HashMap<u16, (usize, SocketAddr, Instant)>,
    learned: HashMap<Ipv4Addr, Instant>,
    learned6: HashMap<Ipv6Addr, Instant>,
    dns_sem: Arc<Semaphore>,
    upstreams: Arc<Vec<SocketAddr>>,
    next_id: u64,
    next_port: u16,
    tap: Option<StdUnixDatagram>,
    tap_version: u8,
    /// The guest frame currently being processed, held back until its verdict is known.
    pending_tap: RefCell<Option<Vec<u8>>>,
    ip_id: Cell<u16>,
    reasm: frag::Reassembler,
    icmp: HashMap<icmp::IcmpKey, icmp::IcmpFlow>,
    /// Echo-request token bucket: (tokens, last refill).
    icmp_bucket: (f64, Instant),
    last_ra: Option<Instant>,
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

/// One DNS query over UDP to `up`, retried over TCP to the same server when the answer comes back truncated.
async fn query_upstream(up: SocketAddr, q: &[u8]) -> Option<Vec<u8>> {
    let bind = if up.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let s = UdpSocket::bind(bind).await.ok()?;
    s.connect(up).await.ok()?;
    s.send(q).await.ok()?;
    let mut buf = vec![0u8; 65535];
    let n = tokio::time::timeout(Duration::from_secs(2), s.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    if n < 12 || buf[..2] != q[..2] {
        return None;
    }
    buf.truncate(n);
    if dns::is_truncated(&buf)
        && let Some(full) = query_upstream_tcp(up, q).await
    {
        return Some(full);
    }
    Some(buf)
}

async fn query_upstream_tcp(up: SocketAddr, q: &[u8]) -> Option<Vec<u8>> {
    let r = async {
        let mut s = TcpStream::connect(up).await.ok()?;
        s.write_all(&dns::tcp_frame(q)).await.ok()?;
        let mut len = [0u8; 2];
        s.read_exact(&mut len).await.ok()?;
        let mut buf = vec![0u8; usize::from(u16::from_be_bytes(len))];
        s.read_exact(&mut buf).await.ok()?;
        (buf.len() >= 12 && buf[..2] == q[..2]).then_some(buf)
    };
    tokio::time::timeout(Duration::from_secs(5), r).await.ok().flatten()
}

async fn resolve(upstreams: &[SocketAddr], q: &[u8]) -> Option<Vec<u8>> {
    for up in upstreams {
        if let Some(a) = query_upstream(*up, q).await {
            return Some(a);
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
                        Ok(n) => {
                            if self.tap.is_some() {
                                *self.pending_tap.borrow_mut() = Some(buf[..n].to_vec());
                            }
                            self.on_frame(&buf[..n]);
                            self.tap_commit(TAP_VERDICT_PASS);
                        }
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

    /// Send the held-back guest frame to the tap with `verdict`, if it has not been sent yet.
    fn tap_commit(&self, verdict: u8) {
        let Some(frame) = self.pending_tap.borrow_mut().take() else {
            return;
        };
        self.tap_frame(TAP_FROM_GUEST, verdict, &frame);
    }

    /// The frame being processed is dropped for a reason that is not the egress policy.
    fn mark_dropped(&self) {
        self.tap_commit(TAP_VERDICT_DROPPED);
    }

    fn emit_frame(&self, ethertype: u16, dst: Mac, payload: &[u8]) {
        // The guest frame that provoked this output is tapped first, as a pass.
        self.tap_commit(TAP_VERDICT_PASS);
        let f = wire::build_eth(dst, self.cfg.gateway_mac, ethertype, payload);
        // Datagram semantics: a full socket buffer drops the frame; TCP recovers by retransmission.
        let _ = self.sock.try_send(&f);
        self.tap_frame(TAP_TO_GUEST, TAP_VERDICT_PASS, &f);
    }

    fn tap_frame(&self, dir: u8, verdict: u8, frame: &[u8]) {
        let Some(t) = &self.tap else { return };
        let mut d = Vec::with_capacity(frame.len() + 2);
        d.push(dir);
        if self.tap_version >= TAP_VERSION_2 {
            d.push(verdict);
        }
        d.extend_from_slice(frame);
        let _ = t.send(&d);
    }

    fn next_ip_id(&self) -> u16 {
        let v = self.ip_id.get();
        self.ip_id.set(v.wrapping_add(1));
        v
    }

    fn guest_dst_mac(&self) -> Mac {
        self.guest_mac.unwrap_or(BROADCAST)
    }

    /// Send an IP packet to the guest. IPv4 packets over the MTU are fragmented when `df` is false and dropped
    /// otherwise; IPv6 packets over the MTU are dropped (the gateway never builds them: MSS and the DNS
    /// truncation limit keep them under it).
    fn send_ip(&self, src: IpAddr, dst: IpAddr, proto: u8, l4: &[u8], df: bool) {
        let mtu = usize::from(self.cfg.mtu);
        match (src, dst) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                let ip = wire::build_ipv4_with(s, d, proto, l4, self.next_ip_id(), df);
                let dst_mac = if d.is_broadcast() { BROADCAST } else { self.guest_dst_mac() };
                if ip.len() <= mtu {
                    self.emit_frame(wire::ETH_IPV4, dst_mac, &ip);
                } else if df {
                    tracing::debug!(len = ip.len(), "usernet: dropping oversized DF packet");
                } else if let Some(frags) = wire::fragment_ipv4(&ip, mtu) {
                    for f in frags {
                        self.emit_frame(wire::ETH_IPV4, dst_mac, &f);
                    }
                }
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                let ip = wire::build_ipv6(s, d, proto, 64, l4);
                if ip.len() > mtu {
                    tracing::debug!(len = ip.len(), "usernet: dropping oversized ipv6 packet");
                    return;
                }
                let mac = if d.is_multicast() {
                    wire::v6_multicast_mac(d)
                } else {
                    self.guest_dst_mac()
                };
                self.emit_frame(wire::ETH_IPV6, mac, &ip);
            }
            _ => {}
        }
    }

    fn send_udp(&self, src: IpAddr, sport: u16, dst: IpAddr, dport: u16, data: &[u8]) {
        let u = wire::build_udp(src, dst, sport, dport, data);
        self.send_ip(src, dst, wire::PROTO_UDP, &u, false);
    }

    fn flush_tcp(&self, key: FKey, out: Vec<TcpOut>) {
        for o in out {
            let seg = wire::build_tcp(key.r, key.g, key.rp, key.gp, &o);
            self.send_ip(key.r, key.g, wire::PROTO_TCP, &seg, true);
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

    /// A policy denial: publishes the event and marks the guest frame being processed as denied.
    fn deny_from(&self, proto: Proto, guest: SocketAddr, rip: IpAddr, rport: u16, reason: &str) {
        self.tap_commit(TAP_VERDICT_DENIED);
        let mut e = self.log(
            FlowKind::Deny,
            proto,
            Direction::Outbound,
            guest,
            SocketAddr::new(rip, rport),
        );
        e.reason = Some(reason.to_string());
        self.publish(e);
    }

    fn deny(&self, proto: Proto, gip: IpAddr, gport: u16, rip: IpAddr, rport: u16, reason: &str) {
        self.deny_from(proto, SocketAddr::new(gip, gport), rip, rport, reason);
    }

    fn is_dns_dst(&self, dst: IpAddr) -> bool {
        match dst {
            IpAddr::V4(d) => d == self.cfg.dns_ip || d == self.cfg.gateway_ip,
            IpAddr::V6(d) => {
                self.cfg.ipv6
                    && (d == self.cfg.dns_ip6()
                        || d == self.cfg.gateway_ip6()
                        || d == self.cfg.gateway_ll6())
            }
        }
    }

    // ----- input -------------------------------------------------------------------------------------------

    fn on_frame(&mut self, f: &[u8]) {
        let Some(eth) = wire::parse_eth(f) else {
            return self.mark_dropped();
        };
        if eth.src[0] & 1 != 0 || eth.src == [0; 6] {
            return self.mark_dropped();
        }
        match self.cfg.guest_mac.or(self.guest_mac) {
            Some(m) if m != eth.src => return self.mark_dropped(),
            Some(_) => {}
            None => self.guest_mac = Some(eth.src),
        }
        let v6_multicast = self.cfg.ipv6
            && eth.ethertype == wire::ETH_IPV6
            && eth.dst[0] == 0x33
            && eth.dst[1] == 0x33;
        if eth.dst != self.cfg.gateway_mac && eth.dst != BROADCAST && !v6_multicast {
            return; // multicast and strays are not ours
        }
        match eth.ethertype {
            wire::ETH_ARP => self.on_arp(eth.src, eth.payload),
            wire::ETH_IPV4 => self.on_ipv4(eth.src, eth.payload),
            wire::ETH_IPV6 if self.cfg.ipv6 => self.on_ipv6(eth.src, eth.payload),
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
        let Some(ip) = wire::parse_ipv4(p) else {
            return self.mark_dropped();
        };
        if ip.total_len > usize::from(self.cfg.mtu) {
            if ip.df && !ip.fragment && ip.src == self.cfg.guest_ip {
                self.icmp_frag_needed(&p[..ip.total_len], ip.src);
            }
            tracing::debug!(len = ip.total_len, "usernet: dropping packet over MTU");
            return self.mark_dropped();
        }
        if ip.fragment {
            if ip.src != self.cfg.guest_ip {
                return self.mark_dropped();
            }
            if let Some(packet) = self.reasm.add(&ip, Instant::now()) {
                self.on_ipv4_packet(src_mac, &packet);
            }
            return;
        }
        self.on_ipv4_packet(src_mac, p);
    }

    fn on_ipv4_packet(&mut self, src_mac: Mac, p: &[u8]) {
        let Some(ip) = wire::parse_ipv4(p) else {
            return self.mark_dropped();
        };
        if ip.proto == wire::PROTO_UDP
            && let Some(u) = wire::parse_udp(ip.payload)
            && u.dport == 67
        {
            self.on_dhcp(src_mac, u.payload);
            return;
        }
        if ip.src != self.cfg.guest_ip {
            return self.mark_dropped(); // anti-spoofing: the lease is the only source address
        }
        match ip.proto {
            wire::PROTO_ICMP => self.on_icmp(&ip),
            wire::PROTO_UDP => {
                if let Some(u) = wire::parse_udp(ip.payload) {
                    self.on_udp(ip.src.into(), ip.dst.into(), &u);
                }
            }
            wire::PROTO_TCP => {
                if let Some(t) = wire::parse_tcp(ip.payload) {
                    self.on_tcp(ip.src.into(), ip.dst.into(), &t);
                }
            }
            _ => {}
        }
    }

    /// ICMP "fragmentation needed" (type 3 code 4) back to a guest whose DF packet exceeded our MTU.
    fn icmp_frag_needed(&self, original: &[u8], guest: Ipv4Addr) {
        let ihl = usize::from(original[0] & 0xf) * 4;
        let quote = &original[..original.len().min(ihl + 8)];
        let mut m = vec![3, 4, 0, 0, 0, 0];
        m.extend_from_slice(&self.cfg.mtu.to_be_bytes());
        m.extend_from_slice(quote);
        let c = wire::checksum(&m);
        m[2..4].copy_from_slice(&c.to_be_bytes());
        self.mark_dropped();
        self.send_ip(self.cfg.gateway_ip.into(), guest.into(), wire::PROTO_ICMP, &m, false);
    }

    fn on_icmp(&mut self, ip: &wire::Ipv4<'_>) {
        let p = ip.payload;
        if p.len() < 8 || p[0] != 8 || p[1] != 0 {
            return;
        }
        if ip.dst == self.cfg.gateway_ip || ip.dst == self.cfg.dns_ip {
            let mut r = p.to_vec();
            r[0] = 0;
            r[2] = 0;
            r[3] = 0;
            let c = wire::checksum(&r);
            r[2..4].copy_from_slice(&c.to_be_bytes());
            self.send_ip(ip.dst.into(), ip.src.into(), wire::PROTO_ICMP, &r, false);
            return;
        }
        self.icmp_request(ip.dst, p, Instant::now());
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
            let gw = IpAddr::V4(self.cfg.gateway_ip);
            let used = self.keys.keys().any(|k| k.r == gw && k.rp == p) || self.udp_rev.contains_key(&p);
            if !used {
                return p;
            }
        }
    }

    // ----- UDP ---------------------------------------------------------------------------------------------

    fn on_udp(&mut self, src: IpAddr, dst: IpAddr, u: &wire::Udp<'_>) {
        if u.dport == 53 && self.is_dns_dst(dst) {
            return self.handle_dns(DnsVia::Udp, dst, src, u.sport, u.payload.to_vec());
        }
        if dst == IpAddr::V4(self.cfg.gateway_ip)
            && let Some((idx, peer, last)) = self.udp_rev.get_mut(&u.dport)
        {
            *last = Instant::now();
            if let Some(s) = self.udp_fwd_socks.get(idx) {
                let _ = s.send_to(u.payload, *peer);
            }
            return;
        }
        let non_unicast = match dst {
            IpAddr::V4(d) => d.is_broadcast() || d.is_multicast(),
            IpAddr::V6(d) => d.is_multicast(),
        };
        if non_unicast {
            return;
        }
        let key = FKey { g: src, gp: u.sport, r: dst, rp: u.dport };
        let now = Instant::now();
        if let Some(f) = self.udp.get_mut(&key) {
            f.last = now;
            f.up += u.payload.len() as u64;
            let _ = f.sock.send(u.payload);
            return;
        }
        // New flow: policy first.
        let target = match self.translate(dst, u.dport, now) {
            Ok(t) => t,
            Err(why) => return self.deny(Proto::Udp, src, u.sport, dst, u.dport, why),
        };
        if self.udp.len() >= self.cfg.max_udp_flows {
            return self.deny(Proto::Udp, src, u.sport, dst, u.dport, "flow-limit");
        }
        // Sends go through the std socket (a fresh tokio socket reports WouldBlock to `try_send` until its
        // first readiness event); a clone feeds the receive task.
        let bind = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socks = std::net::UdpSocket::bind(bind).and_then(|s| {
            s.set_nonblocking(true)?;
            s.connect(target)?;
            let r = UdpSocket::from_std(s.try_clone()?)?;
            Ok((s, r))
        });
        let (sock, r) = match socks {
            Ok((s, r)) => (Arc::new(s), r),
            Err(e) => {
                return self.deny(Proto::Udp, src, u.sport, dst, u.dport, &format!("socket: {e}"));
            }
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
        let e = self.log(
            FlowKind::Open,
            Proto::Udp,
            Direction::Outbound,
            SocketAddr::new(src, u.sport),
            SocketAddr::new(dst, u.dport),
        );
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
    fn translate(&self, dst: IpAddr, port: u16, now: Instant) -> Result<SocketAddr, &'static str> {
        let c = &self.cfg;
        match dst {
            IpAddr::V4(dst) => {
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
            IpAddr::V6(dst) => {
                if !c.ipv6 {
                    return Err("ipv6-disabled");
                }
                if dst == c.gateway_ip6() || dst == c.gateway_ll6() {
                    if !c.egress.allow_host_loopback {
                        return Err("host-loopback");
                    }
                    return match c.egress.check_ports(port) {
                        Decision::Allow => Ok(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port)),
                        Decision::Deny(r) => Err(r),
                    };
                }
                if dst == c.dns_ip6() {
                    return Err("gateway-service");
                }
                match c.egress.check6(dst, Some(port), &self.learned6, now) {
                    Decision::Allow => Ok(SocketAddr::new(dst.into(), port)),
                    Decision::Deny(r) => Err(r),
                }
            }
        }
    }

    // ----- DNS ---------------------------------------------------------------------------------------------

    /// Deliver a gateway-generated answer (NXDOMAIN, SERVFAIL) to the guest.
    fn dns_respond(&mut self, via: DnsVia, local_ip: IpAddr, guest: IpAddr, gport: u16, reply: &[u8]) {
        match via {
            DnsVia::Udp => self.send_udp(local_ip, 53, guest, gport, reply),
            DnsVia::Tcp(id) => self.dns_tcp_push(id, reply, false),
        }
    }

    fn handle_dns(&mut self, via: DnsVia, local_ip: IpAddr, guest: IpAddr, gport: u16, msg: Vec<u8>) {
        let Some(q) = dns::parse_query(&msg) else { return };
        let proto = match via {
            DnsVia::Udp => Proto::Udp,
            DnsVia::Tcp(_) => Proto::Tcp,
        };
        let guest_sa = SocketAddr::new(guest, gport);
        let remote = SocketAddr::new(local_ip, 53);
        if let Decision::Deny(why) = self.cfg.egress.check_domain(&q.name) {
            self.tap_commit(TAP_VERDICT_DENIED);
            let mut e = self.log(FlowKind::DnsDeny, proto, Direction::Outbound, guest_sa, remote);
            e.name = Some(q.name.clone());
            e.reason = Some(why.into());
            self.publish(e);
            let r = dns::build_reply(&msg, &q, 3, false);
            return self.dns_respond(via, local_ip, guest, gport, &r);
        }
        let mut e = self.log(FlowKind::DnsQuery, proto, Direction::Outbound, guest_sa, remote);
        e.name = Some(q.name.clone());
        self.publish(e);
        let Ok(permit) = self.dns_sem.clone().try_acquire_owned() else {
            if matches!(via, DnsVia::Tcp(_)) {
                let r = dns::build_reply(&msg, &q, 2, false);
                self.dns_respond(via, local_ip, guest, gport, &r);
            }
            return;
        };
        if let DnsVia::Tcp(id) = via
            && let Some(d) = self.flows.get_mut(&id).and_then(|f| f.dns.as_mut())
        {
            d.pending += 1;
        }
        let ups = self.upstreams.clone();
        let tx = self.ev_tx.clone();
        tokio::spawn(async move {
            let answer = resolve(&ups, &msg).await;
            drop(permit);
            let _ = tx
                .send(Ev::DnsReply {
                    via,
                    local_ip,
                    guest,
                    guest_port: gport,
                    query: msg,
                    answer,
                })
                .await;
        });
    }

    /// Queue a length-prefixed answer on a DNS-over-TCP flow; `was_pending` when it completes an upstream query.
    fn dns_tcp_push(&mut self, id: u64, reply: &[u8], was_pending: bool) {
        let now = Instant::now();
        let Some(f) = self.flows.get_mut(&id) else { return };
        let Some(d) = f.dns.as_mut() else { return };
        if was_pending {
            d.pending = d.pending.saturating_sub(1);
        }
        let mut out = Vec::new();
        f.conn.push_host_data(&dns::tcp_frame(reply), now, &mut out);
        if d.fin && d.pending == 0 {
            f.conn.host_eof(now, &mut out);
        }
        let key = f.key;
        self.flush_tcp(key, out);
    }

    /// Bytes from the guest on a DNS-over-TCP flow (and its FIN).
    fn dns_tcp_input(&mut self, id: u64, data: Option<Vec<u8>>, fin: bool) {
        let mut msgs = Vec::new();
        let (local_ip, guest, gport);
        {
            let Some(f) = self.flows.get_mut(&id) else { return };
            let Some(d) = f.dns.as_mut() else { return };
            if let Some(data) = data {
                d.buf.extend_from_slice(&data);
            }
            while let Some(m) = dns::tcp_take(&mut d.buf) {
                msgs.push(m);
            }
            if d.buf.len() > DNS_TCP_MAX_BUF {
                d.buf.clear();
            }
            if fin {
                d.fin = true;
            }
            local_ip = d.local_ip;
            guest = f.key.g;
            gport = f.key.gp;
        }
        for m in msgs {
            self.handle_dns(DnsVia::Tcp(id), local_ip, guest, gport, m);
        }
        if fin {
            // Nothing in flight: answer the FIN with ours now.
            let now = Instant::now();
            if let Some(f) = self.flows.get_mut(&id)
                && f.dns.as_ref().is_some_and(|d| d.pending == 0)
            {
                let mut out = Vec::new();
                f.conn.host_eof(now, &mut out);
                let key = f.key;
                self.flush_tcp(key, out);
            }
        }
    }

    // ----- TCP ---------------------------------------------------------------------------------------------

    fn mss(&self, v6: bool) -> usize {
        usize::from(self.cfg.mtu) - if v6 { 60 } else { 40 }
    }

    fn rst_for(&self, src: IpAddr, dst: IpAddr, t: &wire::Tcp<'_>) {
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
        let seg = wire::build_tcp(dst, src, t.dport, t.sport, &o);
        self.send_ip(dst, src, wire::PROTO_TCP, &seg, true);
    }

    fn on_tcp(&mut self, src: IpAddr, dst: IpAddr, t: &wire::Tcp<'_>) {
        let key = FKey { g: src, gp: t.sport, r: dst, rp: t.dport };
        let now = Instant::now();
        if let Some(&id) = self.keys.get(&key) {
            let Some(f) = self.flows.get_mut(&id) else { return };
            let is_dns = f.dns.is_some();
            let can = is_dns || (f.to_host.is_some() && f.queued.load(Ordering::Relaxed) < MAX_QUEUED_TO_HOST);
            let mut out = Vec::new();
            let res = f.conn.on_segment(t, can, now, &mut out);
            let mut dns_data = None;
            if is_dns {
                dns_data = res.data;
            } else {
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
            }
            if res.acked > 0 {
                f.budget.add_permits(res.acked);
            }
            let closed = f.conn.state == State::Closed;
            self.flush_tcp(key, out);
            if is_dns && !closed && (dns_data.is_some() || res.guest_fin) {
                self.dns_tcp_input(id, dns_data, res.guest_fin);
            }
            if closed {
                self.remove_flow(id);
            }
            return;
        }
        if t.flags & (SYN | ACK | RST) != SYN {
            return self.rst_for(src, dst, t);
        }
        // New outbound connection.
        if t.dport == 53 && self.is_dns_dst(dst) {
            return self.new_dns_tcp(key, t, now);
        }
        let target = match self.translate(dst, t.dport, now) {
            Ok(a) => a,
            Err(why) => {
                self.deny(Proto::Tcp, src, t.sport, dst, t.dport, why);
                return self.rst_for(src, dst, t);
            }
        };
        if self.flows.len() >= self.cfg.max_tcp_flows {
            self.deny(Proto::Tcp, src, t.sport, dst, t.dport, "flow-limit");
            return self.rst_for(src, dst, t);
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
                conn: TcpConn::new_passive(t, self.mss(dst.is_ipv6()), now),
                key,
                dir: Direction::Outbound,
                host_peer: target,
                opened: false,
                to_host: None,
                queued: Arc::new(AtomicUsize::new(0)),
                budget: Arc::new(Semaphore::new(BUDGET)),
                tasks: vec![task],
                dns: None,
            },
        );
    }

    /// A guest TCP connection to the gateway's DNS service: terminated here, no host socket.
    fn new_dns_tcp(&mut self, key: FKey, syn: &wire::Tcp<'_>, now: Instant) {
        if self.flows.len() >= self.cfg.max_tcp_flows {
            self.deny(Proto::Tcp, key.g, key.gp, key.r, key.rp, "flow-limit");
            return self.rst_for(key.g, key.r, syn);
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut conn = TcpConn::new_passive(syn, self.mss(key.r.is_ipv6()), now);
        let mut out = Vec::new();
        conn.host_connected(now, &mut out);
        self.keys.insert(key, id);
        self.flows.insert(
            id,
            TcpFlow {
                conn,
                key,
                dir: Direction::Outbound,
                host_peer: SocketAddr::new(key.r, key.rp),
                opened: false,
                to_host: None,
                queued: Arc::new(AtomicUsize::new(0)),
                budget: Arc::new(Semaphore::new(BUDGET)),
                tasks: Vec::new(),
                dns: Some(DnsTcp {
                    buf: Vec::new(),
                    pending: 0,
                    fin: false,
                    local_ip: key.r,
                }),
            },
        );
        self.flush_tcp(key, out);
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
            let remote = SocketAddr::new(f.key.r, f.key.rp);
            let mut e = self.log(FlowKind::Close, Proto::Tcp, f.dir, SocketAddr::new(f.key.g, f.key.gp), remote);
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
            SocketAddr::new(f.key.g, f.key.gp),
            match f.dir {
                Direction::Outbound => SocketAddr::new(f.key.r, f.key.rp),
                Direction::Inbound => f.host_peer,
            },
        );
        if f.dir == Direction::Outbound && f.host_peer.ip() != f.key.r {
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
                    SocketAddr::new(key.g, key.gp),
                    SocketAddr::new(key.r, key.rp),
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
                let key = FKey {
                    g: self.cfg.guest_ip.into(),
                    gp: gport,
                    r: self.cfg.gateway_ip.into(),
                    rp: gw_port,
                };
                if self.flows.len() >= self.cfg.max_tcp_flows {
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                let mut out = Vec::new();
                let conn = TcpConn::new_active(self.mss(false), now, &mut out);
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
                        dns: None,
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
                self.send_udp(key.r, key.rp, key.g, key.gp, &data);
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
                        let e = self.log(
                            FlowKind::Open,
                            Proto::Udp,
                            Direction::Inbound,
                            self.guest_addr(gport),
                            peer,
                        );
                        self.publish(e);
                        p
                    }
                };
                self.udp_rev.insert(gw_port, (idx, peer, now));
                self.send_udp(
                    self.cfg.gateway_ip.into(),
                    gw_port,
                    self.cfg.guest_ip.into(),
                    gport,
                    &data,
                );
            }
            Ev::DnsReply { via, local_ip, guest, guest_port, query, answer } => {
                let Some(q) = dns::parse_query(&query) else { return };
                let reply = match answer {
                    None => dns::build_reply(&query, &q, 2, false), // SERVFAIL
                    Some(a) => {
                        if !self.cfg.egress.allow_domains.is_empty() {
                            for (ip, ttl) in dns::parse_addr_records(&a) {
                                let ttl = Duration::from_secs(u64::from(ttl.clamp(60, 3600)));
                                match ip {
                                    IpAddr::V4(ip) => self.learned.insert(ip, now + ttl),
                                    IpAddr::V6(ip) => self.learned6.insert(ip, now + ttl),
                                };
                            }
                        }
                        a
                    }
                };
                match via {
                    DnsVia::Udp => {
                        // Fits the client's buffer (512 or its EDNS size) and, for IPv6 where we cannot
                        // fragment, the MTU; otherwise TC so the client retries over TCP.
                        let mut limit = dns::udp_limit(&query).min(65_000);
                        if local_ip.is_ipv6() {
                            limit = limit.min(usize::from(self.cfg.mtu) - 48);
                        }
                        let reply = if reply.len() > limit {
                            dns::truncate_reply(&reply).unwrap_or(reply)
                        } else {
                            reply
                        };
                        self.send_udp(local_ip, 53, guest, guest_port, &reply);
                    }
                    DnsVia::Tcp(id) => self.dns_tcp_push(id, &reply, true),
                }
            }
            Ev::IcmpReply { key, data } => self.on_icmp_reply(key, &data, now),
        }
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        let idle = Duration::from_secs(self.cfg.tcp_idle_secs);
        let mut work = Vec::new();
        for (id, f) in self.flows.iter_mut() {
            let mut out = Vec::new();
            let idle = if f.dns.is_some() { idle.min(DNS_TCP_IDLE) } else { idle };
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
        let expired: Vec<FKey> = self
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
                    SocketAddr::new(k.g, k.gp),
                    SocketAddr::new(k.r, k.rp),
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
        self.learned6.retain(|_, exp| *exp > now);
        self.reasm.expire(now);
        self.icmp_tick(now);
        self.v6_tick(now);
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
