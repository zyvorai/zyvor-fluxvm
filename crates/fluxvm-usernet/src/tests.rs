// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! In-process tests: crafted Ethernet frames over a SOCK_DGRAM socketpair, exactly what a
//! `VZFileHandleNetworkDeviceAttachment` guest would exchange, against real host sockets on 127.0.0.1.

use super::*;
use std::net::IpAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use wire::*;

const GMAC: Mac = [0x02, 0, 0, 0, 0, 1];

struct Guest {
    s: UnixDatagram,
    cfg: UserNetConfig,
}

struct Running {
    guest: Guest,
    log: broadcast::Receiver<FlowEvent>,
    fwd: Vec<SocketAddr>,
    handle: UserNetHandle,
    task: JoinHandle<io::Result<()>>,
}

fn setbufs(s: &StdUnixDatagram) {
    use std::os::fd::AsRawFd;
    for (opt, v) in [(libc::SO_SNDBUF, 1 << 20), (libc::SO_RCVBUF, 4 << 20)] {
        let v: libc::c_int = v;
        unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&raw const v).cast(),
                std::mem::size_of::<libc::c_int>() as u32,
            );
        }
    }
}

fn start(cfg: UserNetConfig) -> Running {
    let (a, b) = StdUnixDatagram::pair().unwrap();
    setbufs(&a);
    setbufs(&b);
    b.set_nonblocking(true).unwrap();
    let un = UserNet::new(cfg.clone(), a).unwrap();
    let handle = un.handle();
    let log = handle.subscribe();
    let fwd = un.forward_addrs().to_vec();
    let task = tokio::spawn(un.run());
    Running {
        guest: Guest {
            s: UnixDatagram::from_std(b).unwrap(),
            cfg,
        },
        log,
        fwd,
        handle,
        task,
    }
}

fn cfg_allow_loopback() -> UserNetConfig {
    let mut c = UserNetConfig::default();
    c.egress.allow_cidrs = vec!["127.0.0.0/8".parse().unwrap()];
    c
}

#[derive(Debug, Clone)]
struct Seg {
    src: IpAddr,
    dst: IpAddr,
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    mss: Option<u16>,
    wscale: Option<u8>,
    sack_ok: bool,
    sack: Vec<(u32, u32)>,
    payload: Vec<u8>,
}

/// A TCP segment (IPv4 or IPv6) in an Ethernet frame.
fn as_tcp(f: &[u8]) -> Option<Seg> {
    let e = parse_eth(f)?;
    let (src, dst, l4): (IpAddr, IpAddr, &[u8]) = match e.ethertype {
        ETH_IPV4 => {
            let ip = parse_ipv4(e.payload)?;
            if ip.proto != PROTO_TCP || ip.fragment {
                return None;
            }
            (ip.src.into(), ip.dst.into(), ip.payload)
        }
        ETH_IPV6 => {
            let ip = parse_ipv6(e.payload)?;
            if ip.next_header != PROTO_TCP {
                return None;
            }
            (ip.src.into(), ip.dst.into(), ip.payload)
        }
        _ => return None,
    };
    assert_eq!(l4_checksum(src, dst, PROTO_TCP, l4), 0, "bad tcp checksum");
    let t = parse_tcp(l4)?;
    Some(Seg {
        src,
        dst,
        sport: t.sport,
        dport: t.dport,
        seq: t.seq,
        ack: t.ack,
        flags: t.flags,
        window: t.window,
        mss: t.mss,
        wscale: t.wscale,
        sack_ok: t.sack_ok,
        sack: t.sack.clone(),
        payload: t.payload.to_vec(),
    })
}

/// A UDP datagram over IPv6 in an Ethernet frame: (src, dst, sport, dport, payload).
fn as_udp6(f: &[u8]) -> Option<(Ipv6Addr, Ipv6Addr, u16, u16, Vec<u8>)> {
    let e = parse_eth(f)?;
    if e.ethertype != ETH_IPV6 {
        return None;
    }
    let ip = parse_ipv6(e.payload)?;
    if ip.next_header != PROTO_UDP {
        return None;
    }
    assert_eq!(l4_checksum(ip.src, ip.dst, PROTO_UDP, ip.payload), 0, "bad udp6 checksum");
    let u = parse_udp(ip.payload)?;
    Some((ip.src, ip.dst, u.sport, u.dport, u.payload.to_vec()))
}

/// An ICMPv6 message in an Ethernet frame: (src, dst, hop limit, message `[type, code, csum, body]`).
fn as_icmp6(f: &[u8]) -> Option<(Ipv6Addr, Ipv6Addr, u8, Vec<u8>)> {
    let e = parse_eth(f)?;
    if e.ethertype != ETH_IPV6 {
        return None;
    }
    let ip = parse_ipv6(e.payload)?;
    if ip.next_header != PROTO_ICMPV6 {
        return None;
    }
    assert_eq!(l4_checksum(ip.src, ip.dst, PROTO_ICMPV6, ip.payload), 0, "bad icmpv6 checksum");
    Some((ip.src, ip.dst, ip.hop_limit, ip.payload.to_vec()))
}

fn as_udp(f: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u16, u16, Vec<u8>)> {
    let e = parse_eth(f)?;
    if e.ethertype != ETH_IPV4 {
        return None;
    }
    let ip = parse_ipv4(e.payload)?;
    if ip.proto != PROTO_UDP || ip.fragment {
        return None;
    }
    assert_eq!(checksum(&e.payload[..20]), 0, "bad ip checksum");
    assert_eq!(l4_checksum(ip.src, ip.dst, PROTO_UDP, ip.payload), 0, "bad udp checksum");
    let u = parse_udp(ip.payload)?;
    Some((ip.src, ip.dst, u.sport, u.dport, u.payload.to_vec()))
}

impl Guest {
    async fn send_eth(&self, dst: Mac, ethertype: u16, payload: &[u8]) {
        self.s
            .send(&build_eth(dst, GMAC, ethertype, payload))
            .await
            .unwrap();
    }

    /// The guest's SLAAC address (any address in the advertised /64 works; this one is fixed for the tests).
    fn ip6(&self) -> Ipv6Addr {
        let mut s = self.cfg.ipv6_prefix.segments();
        s[7] = 0x1234;
        Ipv6Addr::from(s)
    }

    fn src_for(&self, dst: IpAddr) -> IpAddr {
        match dst {
            IpAddr::V4(_) => self.cfg.guest_ip.into(),
            IpAddr::V6(_) => self.ip6().into(),
        }
    }

    async fn send_ip(&self, dst: impl Into<IpAddr>, proto: u8, l4: &[u8]) {
        match dst.into() {
            IpAddr::V4(d) => {
                let ip = build_ipv4(self.cfg.guest_ip, d, proto, l4);
                self.send_eth(self.cfg.gateway_mac, ETH_IPV4, &ip).await;
            }
            IpAddr::V6(d) => {
                let ip = build_ipv6(self.ip6(), d, proto, 64, l4);
                self.send_eth(self.cfg.gateway_mac, ETH_IPV6, &ip).await;
            }
        }
    }

    async fn send_udp(&self, dst: impl Into<IpAddr>, sport: u16, dport: u16, data: &[u8]) {
        let dst = dst.into();
        let u = build_udp(self.src_for(dst), dst, sport, dport, data);
        self.send_ip(dst, PROTO_UDP, &u).await;
    }

    async fn send_tcp(&self, dst: impl Into<IpAddr>, sport: u16, dport: u16, o: TcpOut) {
        let dst = dst.into();
        let seg = build_tcp(self.src_for(dst), dst, sport, dport, &o);
        self.send_ip(dst, PROTO_TCP, &seg).await;
    }

    async fn recv_where<T>(&self, what: &str, f: impl Fn(&[u8]) -> Option<T>) -> T {
        let mut buf = vec![0u8; 65536];
        let r = timeout(Duration::from_secs(5), async {
            loop {
                let n = self.s.recv(&mut buf).await.unwrap();
                if let Some(v) = f(&buf[..n]) {
                    return v;
                }
            }
        })
        .await;
        r.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    /// True when no frame matching `f` arrives within `ms`.
    async fn silent(&self, ms: u64, f: impl Fn(&[u8]) -> bool) -> bool {
        let mut buf = vec![0u8; 65536];
        timeout(Duration::from_millis(ms), async {
            loop {
                let n = self.s.recv(&mut buf).await.unwrap();
                if f(&buf[..n]) {
                    return;
                }
            }
        })
        .await
        .is_err()
    }
}

/// A minimal guest-side TCP endpoint.
struct Conn<'a> {
    g: &'a Guest,
    remote: IpAddr,
    sport: u16,
    dport: u16,
    snd_nxt: u32,
    snd_una: u32,
    rcv_nxt: u32,
    received: Vec<u8>,
    fin_rcvd: bool,
    peer_mss: Option<u16>,
}

impl<'a> Conn<'a> {
    async fn connect(g: &'a Guest, remote: impl Into<IpAddr>, sport: u16, dport: u16) -> Conn<'a> {
        let remote = remote.into();
        let isn = 7000;
        g.send_tcp(
            remote,
            sport,
            dport,
            TcpOut { seq: isn, flags: SYN, window: 65535, mss: Some(1460), ..Default::default() },
        )
        .await;
        let sa = g
            .recv_where("SYN-ACK", |f| {
                as_tcp(f).filter(|s| s.src == remote && s.sport == dport && s.dport == sport)
            })
            .await;
        assert_eq!(sa.flags & (SYN | ACK), SYN | ACK, "expected SYN-ACK, got {sa:?}");
        assert_eq!(sa.ack, isn + 1);
        let c = Conn {
            g,
            remote,
            sport,
            dport,
            snd_nxt: isn + 1,
            snd_una: isn + 1,
            rcv_nxt: sa.seq.wrapping_add(1),
            received: Vec::new(),
            fin_rcvd: false,
            peer_mss: sa.mss,
        };
        c.ack().await;
        c
    }

    async fn ack(&self) {
        self.g
            .send_tcp(
                self.remote,
                self.sport,
                self.dport,
                TcpOut { seq: self.snd_nxt, ack: self.rcv_nxt, flags: ACK, window: 65535, ..Default::default() },
            )
            .await;
    }

    /// Absorb one incoming segment of this connection.
    async fn step(&mut self) {
        let (remote, sport, dport) = (self.remote, self.sport, self.dport);
        let s = self
            .g
            .recv_where("tcp segment", |f| {
                as_tcp(f).filter(|s| s.src == remote && s.sport == dport && s.dport == sport)
            })
            .await;
        if s.flags & ACK != 0 && (s.ack.wrapping_sub(self.snd_una) as i32) > 0 {
            self.snd_una = s.ack;
        }
        if s.seq == self.rcv_nxt {
            self.rcv_nxt += s.payload.len() as u32;
            self.received.extend_from_slice(&s.payload);
            if s.flags & FIN != 0 {
                self.rcv_nxt += 1;
                self.fin_rcvd = true;
            }
            if !s.payload.is_empty() || s.flags & FIN != 0 {
                self.ack().await;
            }
        }
    }

    async fn send(&mut self, data: &[u8]) {
        for batch in data.chunks(8 * 1000) {
            for ch in batch.chunks(1000) {
                self.g
                    .send_tcp(
                        self.remote,
                        self.sport,
                        self.dport,
                        TcpOut {
                            seq: self.snd_nxt,
                            ack: self.rcv_nxt,
                            flags: ACK | PSH,
                            window: 65535,
                            payload: ch.to_vec(),
                            ..Default::default()
                        },
                    )
                    .await;
                self.snd_nxt += ch.len() as u32;
            }
            while self.snd_una != self.snd_nxt {
                self.step().await;
            }
        }
    }

    async fn fin(&mut self) {
        self.g
            .send_tcp(
                self.remote,
                self.sport,
                self.dport,
                TcpOut { seq: self.snd_nxt, ack: self.rcv_nxt, flags: FIN | ACK, window: 65535, ..Default::default() },
            )
            .await;
        self.snd_nxt += 1;
        while self.snd_una != self.snd_nxt {
            self.step().await;
        }
    }

    async fn read_until(&mut self, n: usize) {
        while self.received.len() < n {
            self.step().await;
        }
    }

    async fn wait_fin(&mut self) {
        while !self.fin_rcvd {
            self.step().await;
        }
    }
}

async fn tcp_echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    port
}

async fn udp_echo_server() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut b = vec![0u8; 65535];
        while let Ok((n, p)) = s.recv_from(&mut b).await {
            let _ = s.send_to(&b[..n], p).await;
        }
    });
    port
}

/// Fake upstream resolver: answers every A query with 127.0.0.1 and counts queries.
async fn fake_dns() -> (SocketAddr, Arc<AtomicUsize>) {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = s.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        let mut b = vec![0u8; 2048];
        while let Ok((n, p)) = s.recv_from(&mut b).await {
            c.fetch_add(1, Ordering::SeqCst);
            let a = dns::test_answer(&b[..n], Ipv4Addr::LOCALHOST, 300);
            let _ = s.send_to(&a, p).await;
        }
    });
    (addr, count)
}

async fn next_event(r: &mut broadcast::Receiver<FlowEvent>, kind: FlowKind) -> FlowEvent {
    timeout(Duration::from_secs(5), async {
        loop {
            let e = r.recv().await.unwrap();
            if e.kind == kind {
                return e;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind:?} event"))
}

// -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn arp_and_icmp_echo() {
    let r = start(UserNetConfig::default());
    let c = r.guest.cfg.clone();
    let req = build_arp(1, GMAC, c.guest_ip, [0; 6], c.gateway_ip);
    r.guest.send_eth(BROADCAST, ETH_ARP, &req).await;
    let (op, smac, sip, _) = r
        .guest
        .recv_where("ARP reply", |f| {
            let e = parse_eth(f)?;
            (e.ethertype == ETH_ARP).then(|| parse_arp(e.payload)).flatten()
        })
        .await;
    assert_eq!((op, smac, sip), (2, c.gateway_mac, c.gateway_ip));

    // An address that is not ours gets no answer.
    let req = build_arp(1, GMAC, c.guest_ip, [0; 6], Ipv4Addr::new(10, 0, 2, 99));
    r.guest.send_eth(BROADCAST, ETH_ARP, &req).await;
    assert!(r.guest.silent(150, |f| parse_eth(f).is_some_and(|e| e.ethertype == ETH_ARP)).await);

    // ICMP echo to the gateway.
    let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'p', b'i', b'n', b'g'];
    let cs = checksum(&icmp);
    icmp[2..4].copy_from_slice(&cs.to_be_bytes());
    r.guest.send_ip(c.gateway_ip, PROTO_ICMP, &icmp).await;
    let reply = r
        .guest
        .recv_where("echo reply", |f| {
            let e = parse_eth(f)?;
            let ip = parse_ipv4(e.payload)?;
            (ip.proto == PROTO_ICMP).then(|| ip.payload.to_vec())
        })
        .await;
    assert_eq!(reply[0], 0);
    assert_eq!(checksum(&reply), 0);
    assert_eq!(&reply[8..], b"ping");
}

#[tokio::test]
async fn dhcp_offer_ack_nak_and_mtu() {
    let mut cfg = UserNetConfig::default();
    cfg.mtu = 1400;
    let r = start(cfg.clone());
    let g = &r.guest;
    let dhcp_reply = |f: &[u8]| {
        let (_, dst, sport, dport, p) = as_udp(f)?;
        (sport == 67 && dport == 68 && dst.is_broadcast()).then_some(p)
    };
    let send = |p: Vec<u8>| async move {
        let u = build_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &p);
        let ip = build_ipv4(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, PROTO_UDP, &u);
        g.send_eth(BROADCAST, ETH_IPV4, &ip).await;
    };

    send(dhcp::test_request(GMAC, dhcp::DISCOVER, [1, 2, 3, 4], None)).await;
    let offer = g.recv_where("offer", dhcp_reply).await;
    assert_eq!(&offer[4..8], &[1, 2, 3, 4]);
    assert_eq!(&offer[16..20], &cfg.guest_ip.octets());
    assert_eq!(dhcp::test_option(&offer, 53), Some(vec![dhcp::OFFER]));
    assert_eq!(dhcp::test_option(&offer, 1), Some(vec![255, 255, 255, 0]));
    assert_eq!(dhcp::test_option(&offer, 3), Some(cfg.gateway_ip.octets().to_vec()));
    assert_eq!(dhcp::test_option(&offer, 6), Some(cfg.dns_ip.octets().to_vec()));
    assert_eq!(dhcp::test_option(&offer, 26), Some(1400u16.to_be_bytes().to_vec()));

    send(dhcp::test_request(GMAC, dhcp::REQUEST, [5, 6, 7, 8], Some(cfg.guest_ip))).await;
    let ack = g.recv_where("ack", dhcp_reply).await;
    assert_eq!(dhcp::test_option(&ack, 53), Some(vec![dhcp::ACK]));

    send(dhcp::test_request(GMAC, dhcp::REQUEST, [9, 9, 9, 9], Some(Ipv4Addr::new(10, 0, 2, 77)))).await;
    let nak = g.recv_where("nak", dhcp_reply).await;
    assert_eq!(dhcp::test_option(&nak, 53), Some(vec![dhcp::NAK]));
}

#[tokio::test]
async fn dns_forwarding_and_deny() {
    let (up, count) = fake_dns().await;
    let mut cfg = UserNetConfig::default();
    cfg.dns_upstreams = vec![up];
    cfg.egress.deny_domains = vec!["blocked.test".into()];
    let mut r = start(cfg.clone());
    let g = &r.guest;
    let dns_reply = |f: &[u8]| {
        let (src, _, sport, _, p) = as_udp(f)?;
        (sport == 53 && src == Ipv4Addr::new(10, 0, 2, 3)).then_some(p)
    };

    let q = dns::test_query(0xabcd, "ok.test");
    g.send_udp(cfg.dns_ip, 5353, 53, &q).await;
    let a = g.recv_where("dns answer", dns_reply).await;
    assert_eq!(dns::parse_a_records(&a), vec![(Ipv4Addr::LOCALHOST, 300)]);
    assert_eq!(&a[..2], &[0xab, 0xcd]);
    let ev = next_event(&mut r.log, FlowKind::DnsQuery).await;
    assert_eq!(ev.name.as_deref(), Some("ok.test"));

    let q = dns::test_query(0x0001, "www.blocked.test");
    g.send_udp(cfg.dns_ip, 5353, 53, &q).await;
    let nx = g.recv_where("nxdomain", dns_reply).await;
    assert_eq!(nx[3] & 0xf, 3);
    assert_eq!(count.load(Ordering::SeqCst), 1, "denied query must not reach upstream");
    let ev = next_event(&mut r.log, FlowKind::DnsDeny).await;
    assert_eq!(ev.reason.as_deref(), Some("deny-domain"));
}

#[tokio::test]
async fn tcp_nat_echo_bulk_and_close() {
    let port = tcp_echo_server().await;
    let mut cfg = cfg_allow_loopback();
    cfg.mtu = 1200;
    let mut r = start(cfg);
    let mut c = Conn::connect(&r.guest, Ipv4Addr::LOCALHOST, 40000, port).await;
    assert_eq!(c.peer_mss, Some(1160), "MSS must follow the configured MTU");
    let open = next_event(&mut r.log, FlowKind::Open).await;
    assert_eq!(open.proto, Proto::Tcp);
    assert_eq!(open.remote, SocketAddr::from(([127, 0, 0, 1], port)));

    c.send(b"hello").await;
    c.read_until(5).await;
    assert_eq!(c.received, b"hello");

    let blob: Vec<u8> = (0..100_000u32).map(|i| (i * 31 % 251) as u8).collect();
    c.send(&blob).await;
    c.read_until(5 + blob.len()).await;
    assert_eq!(&c.received[5..], &blob[..]);

    c.fin().await;
    c.wait_fin().await; // the echo server closes after our FIN
    let close = next_event(&mut r.log, FlowKind::Close).await;
    assert_eq!(close.bytes_tx, 100_005);
    assert_eq!(close.bytes_rx, 100_005);
}

#[tokio::test]
async fn tcp_connect_refused_resets_guest() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = l.local_addr().unwrap().port();
    drop(l);
    let mut r = start(cfg_allow_loopback());
    r.guest
        .send_tcp(
            Ipv4Addr::LOCALHOST,
            40001,
            closed,
            TcpOut { seq: 100, flags: SYN, window: 65535, ..Default::default() },
        )
        .await;
    let s = r
        .guest
        .recv_where("RST", |f| as_tcp(f).filter(|s| s.sport == closed))
        .await;
    assert_eq!(s.flags & RST, RST);
    assert_eq!(s.ack, 101);
    next_event(&mut r.log, FlowKind::ConnectFailed).await;
}

#[tokio::test]
async fn udp_nat_echo() {
    let port = udp_echo_server().await;
    let mut r = start(cfg_allow_loopback());
    let g = &r.guest;
    for msg in [&b"one"[..], &b"two"[..]] {
        g.send_udp(Ipv4Addr::LOCALHOST, 41000, port, msg).await;
        let (src, dst, sport, dport, p) = g.recv_where("udp echo", as_udp).await;
        assert_eq!((src, dst, sport, dport), (Ipv4Addr::LOCALHOST, g.cfg.guest_ip, port, 41000));
        assert_eq!(p, msg);
    }
    next_event(&mut r.log, FlowKind::Open).await;
}

#[tokio::test]
async fn egress_policy_denies_private_by_default() {
    let tcp_port = tcp_echo_server().await;
    let udp_port = udp_echo_server().await;
    let mut r = start(UserNetConfig::default());
    let g = &r.guest;

    g.send_tcp(
        Ipv4Addr::LOCALHOST,
        42000,
        tcp_port,
        TcpOut { seq: 1, flags: SYN, window: 65535, ..Default::default() },
    )
    .await;
    let s = g.recv_where("RST", |f| as_tcp(f).filter(|s| s.sport == tcp_port)).await;
    assert_eq!(s.flags & RST, RST);
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!((ev.proto, ev.reason.as_deref()), (Proto::Tcp, Some("private-range")));

    g.send_udp(Ipv4Addr::LOCALHOST, 42001, udp_port, b"x").await;
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!((ev.proto, ev.reason.as_deref()), (Proto::Udp, Some("private-range")));
    assert!(g.silent(150, |f| as_udp(f).is_some()).await);

    // The gateway alias is off too.
    g.send_tcp(
        g.cfg.gateway_ip,
        42002,
        tcp_port,
        TcpOut { seq: 1, flags: SYN, window: 65535, ..Default::default() },
    )
    .await;
    g.recv_where("RST", |f| as_tcp(f).filter(|s| s.flags & RST != 0)).await;
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(ev.reason.as_deref(), Some("host-loopback"));
}

#[tokio::test]
async fn egress_port_rules_and_loopback_alias() {
    let port = tcp_echo_server().await;
    let mut cfg = UserNetConfig::default();
    cfg.egress.allow_host_loopback = true;
    cfg.egress.deny_ports = vec![port.wrapping_add(1)];
    let r = start(cfg);
    let g = &r.guest;
    // gateway:port reaches host 127.0.0.1:port
    let mut c = Conn::connect(g, g.cfg.gateway_ip, 43000, port).await;
    c.send(b"via gateway").await;
    c.read_until(11).await;
    assert_eq!(c.received, b"via gateway");
    // a denied port is reset
    g.send_tcp(
        g.cfg.gateway_ip,
        43001,
        port.wrapping_add(1),
        TcpOut { seq: 1, flags: SYN, window: 65535, ..Default::default() },
    )
    .await;
    g.recv_where("RST", |f| as_tcp(f).filter(|s| s.flags & RST != 0)).await;
}

#[tokio::test]
async fn dns_answers_teach_the_allow_list() {
    let port = tcp_echo_server().await;
    let (up, _) = fake_dns().await;
    let mut cfg = UserNetConfig::default();
    cfg.dns_upstreams = vec![up];
    cfg.egress.allow_domains = vec!["example.test".into()];
    let mut r = start(cfg.clone());
    let g = &r.guest;
    let syn = TcpOut { seq: 1, flags: SYN, window: 65535, ..Default::default() };

    // 127.0.0.1 is private and not in any CIDR: refused until a DNS answer for an allowed name teaches it.
    g.send_tcp(Ipv4Addr::LOCALHOST, 44000, port, syn.clone()).await;
    let s = g.recv_where("RST", |f| as_tcp(f).filter(|s| s.sport == port)).await;
    assert_eq!(s.flags & RST, RST);
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(ev.reason.as_deref(), Some("private-range"));

    // Names outside the list do not resolve at all.
    g.send_udp(cfg.dns_ip, 5300, 53, &dns::test_query(1, "other.test")).await;
    let ev = next_event(&mut r.log, FlowKind::DnsDeny).await;
    assert_eq!(ev.reason.as_deref(), Some("domain-not-allowed"));

    g.send_udp(cfg.dns_ip, 5300, 53, &dns::test_query(2, "api.example.test")).await;
    g.recv_where("answer", |f| {
        as_udp(f).filter(|u| u.2 == 53 && u.4[..2] == [0, 2]).map(|u| u.4)
    })
    .await;

    let c = Conn::connect(g, Ipv4Addr::LOCALHOST, 44001, port).await;
    assert!(c.received.is_empty());
}

#[tokio::test]
async fn tcp_port_forward_host_to_guest() {
    let mut cfg = UserNetConfig::default();
    cfg.forwards = vec![PortForward {
        proto: Proto::Tcp,
        host_addr: Ipv4Addr::LOCALHOST.into(),
        host_port: 0,
        guest_port: 8080,
    }];
    let mut r = start(cfg.clone());
    let addr = r.fwd[0];
    let g = &r.guest;

    let mut host = TcpStream::connect(addr).await.unwrap();
    // The gateway opens a connection to the guest from gateway_ip:ephemeral.
    let syn = g
        .recv_where("forward SYN", |f| as_tcp(f).filter(|s| s.flags & (SYN | ACK) == SYN))
        .await;
    assert_eq!((syn.src, syn.dst, syn.dport), (IpAddr::from(cfg.gateway_ip), IpAddr::from(cfg.guest_ip), 8080));
    let gisn = 5000u32;
    // Reply as the guest: swap addresses.
    let reply = |o: TcpOut| build_tcp(cfg.guest_ip, cfg.gateway_ip, 8080, syn.sport, &o);
    let send = |seg: Vec<u8>| build_ip_frame(&cfg, seg);
    g.s.send(&send(reply(TcpOut {
        seq: gisn,
        ack: syn.seq + 1,
        flags: SYN | ACK,
        window: 65535,
        mss: Some(1460),
        ..Default::default()
    })))
    .await
    .unwrap();
    // Gateway ACKs the handshake, then relays host data.
    host.write_all(b"ping").await.unwrap();
    let data = g
        .recv_where("forwarded data", |f| {
            as_tcp(f).filter(|s| s.dport == 8080 && !s.payload.is_empty())
        })
        .await;
    assert_eq!(data.payload, b"ping");
    // Guest answers; the host client reads it.
    g.s.send(&send(reply(TcpOut {
        seq: gisn + 1,
        ack: data.seq + 4,
        flags: ACK | PSH,
        window: 65535,
        payload: b"pong".to_vec(),
        ..Default::default()
    })))
    .await
    .unwrap();
    let mut buf = [0u8; 4];
    timeout(Duration::from_secs(5), host.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf, b"pong");
    let ev = next_event(&mut r.log, FlowKind::Open).await;
    assert_eq!((ev.direction, ev.guest.port()), (Direction::Inbound, 8080));
}

fn build_ip_frame(cfg: &UserNetConfig, tcp: Vec<u8>) -> Vec<u8> {
    let ip = build_ipv4(cfg.guest_ip, cfg.gateway_ip, PROTO_TCP, &tcp);
    build_eth(cfg.gateway_mac, GMAC, ETH_IPV4, &ip)
}

#[tokio::test]
async fn udp_port_forward_host_to_guest() {
    let mut cfg = UserNetConfig::default();
    cfg.forwards = vec![PortForward {
        proto: Proto::Udp,
        host_addr: Ipv4Addr::LOCALHOST.into(),
        host_port: 0,
        guest_port: 5000,
    }];
    let r = start(cfg.clone());
    let addr = r.fwd[0];
    let g = &r.guest;
    let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    host.send_to(b"hi guest", addr).await.unwrap();
    let (src, dst, sport, dport, p) = g.recv_where("forwarded udp", as_udp).await;
    assert_eq!((src, dst, dport, p.as_slice()), (cfg.gateway_ip, cfg.guest_ip, 5000, &b"hi guest"[..]));
    g.send_udp(cfg.gateway_ip, 5000, sport, b"hi host").await;
    let mut buf = [0u8; 64];
    let (n, from) = timeout(Duration::from_secs(5), host.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!((&buf[..n], from), (&b"hi host"[..], addr));
}

#[tokio::test]
async fn guest_is_pinned_to_its_lease_and_shutdown_works() {
    let port = udp_echo_server().await;
    let r = start(cfg_allow_loopback());
    let g = &r.guest;
    // Spoofed source address: ignored.
    let u = build_udp(Ipv4Addr::new(10, 0, 2, 99), Ipv4Addr::LOCALHOST, 1, port, b"x");
    let ip = build_ipv4(Ipv4Addr::new(10, 0, 2, 99), Ipv4Addr::LOCALHOST, PROTO_UDP, &u);
    g.send_eth(g.cfg.gateway_mac, ETH_IPV4, &ip).await;
    assert!(g.silent(150, |f| as_udp(f).is_some()).await);
    r.handle.shutdown();
    timeout(Duration::from_secs(2), r.task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn frame_tap_sees_both_directions() {
    let (a, b) = StdUnixDatagram::pair().unwrap();
    let (tap_tx, tap_rx) = StdUnixDatagram::pair().unwrap();
    setbufs(&a);
    setbufs(&b);
    b.set_nonblocking(true).unwrap();
    let cfg = UserNetConfig::default();
    let mut un = UserNet::new(cfg.clone(), a).unwrap();
    un.set_tap(tap_tx).unwrap();
    let task = tokio::spawn(un.run());
    let guest = Guest { s: UnixDatagram::from_std(b).unwrap(), cfg: cfg.clone() };
    let req = build_arp(1, GMAC, cfg.guest_ip, [0; 6], cfg.gateway_ip);
    guest.send_eth(BROADCAST, ETH_ARP, &req).await;
    guest
        .recv_where("ARP reply", |f| {
            let e = parse_eth(f)?;
            (e.ethertype == ETH_ARP).then_some(())
        })
        .await;
    tap_rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut dirs = Vec::new();
    let mut buf = vec![0u8; 2048];
    while dirs.len() < 2 {
        let n = tap_rx.recv(&mut buf).unwrap();
        let e = parse_eth(&buf[1..n]).expect("tapped datagram is an Ethernet frame");
        assert_eq!(e.ethertype, ETH_ARP);
        dirs.push(buf[0]);
    }
    assert_eq!(dirs, vec![TAP_FROM_GUEST, TAP_TO_GUEST]);
    task.abort();
}

mod more;
