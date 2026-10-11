// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Gateway-feature tests: DNS over TCP, ICMP echo proxy, TCP window scaling / SACK / congestion control,
//! IP fragmentation, IPv6 and the versioned frame tap. Same method as the parent module: crafted frames over a
//! socketpair against real loopback sockets.

use super::*;
use std::collections::BTreeMap;
use std::time::Instant;

fn syn_opts(window: u16) -> TcpOut {
    TcpOut {
        flags: SYN,
        window,
        mss: Some(1460),
        wscale: Some(7),
        sack_ok: true,
        ..Default::default()
    }
}

/// A server that writes `bytes` of a deterministic pattern to each connection, then closes it.
async fn tcp_server_sink(bytes: usize) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut left = bytes;
                let mut off = 0usize;
                let mut buf = vec![0u8; 64 * 1024];
                while left > 0 {
                    let n = left.min(buf.len());
                    for (i, b) in buf[..n].iter_mut().enumerate() {
                        *b = ((off + i) % 251) as u8;
                    }
                    if s.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                    off += n;
                    left -= n;
                }
                let _ = s.shutdown().await;
            });
        }
    });
    port
}

fn pattern_ok(data: &[u8]) -> bool {
    data.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8)
}

/// Receive a UDP datagram from the gateway, reassembling IPv4 fragments.
async fn recv_udp_reassembled(g: &Guest, from_port: u16) -> Vec<u8> {
    let mut re = frag::Reassembler::default();
    let mut buf = vec![0u8; 65536];
    timeout(Duration::from_secs(5), async {
        loop {
            let n = g.s.recv(&mut buf).await.unwrap();
            let Some(e) = parse_eth(&buf[..n]) else {
                continue;
            };
            if e.ethertype != ETH_IPV4 {
                continue;
            }
            let Some(ip) = parse_ipv4(e.payload) else {
                continue;
            };
            if ip.proto != PROTO_UDP {
                continue;
            }
            let packet = if ip.fragment {
                match re.add(&ip, Instant::now()) {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                e.payload[..ip.total_len].to_vec()
            };
            let p = parse_ipv4(&packet).unwrap();
            assert_eq!(
                l4_checksum(p.src, p.dst, PROTO_UDP, p.payload),
                0,
                "bad udp checksum"
            );
            let u = parse_udp(p.payload).unwrap();
            if u.sport == from_port {
                return u.payload.to_vec();
            }
        }
    })
    .await
    .expect("timed out waiting for a (reassembled) UDP datagram")
}

// ----- DNS over TCP -------------------------------------------------------------------------------------------

fn framed_messages(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut b = buf.to_vec();
    let mut v = Vec::new();
    while let Some(m) = dns::tcp_take(&mut b) {
        v.push(m);
    }
    v
}

#[tokio::test]
async fn dns_over_tcp_forwards_and_applies_policy() {
    let (up, count) = fake_dns().await;
    let mut cfg = UserNetConfig::default();
    cfg.dns_upstreams = vec![up];
    cfg.egress.deny_domains = vec!["blocked.test".into()];
    let mut r = start(cfg.clone());
    let g = &r.guest;

    let mut c = Conn::connect(g, cfg.dns_ip, 40100, 53).await;
    // Two pipelined queries in one write, split across two TCP segments mid-message.
    let mut wire = dns::tcp_frame(&dns::test_query(0x1111, "a.test"));
    wire.extend(dns::tcp_frame(&dns::test_query(0x2222, "www.blocked.test")));
    c.send(&wire[..7]).await;
    c.send(&wire[7..]).await;
    while framed_messages(&c.received).len() < 2 {
        c.step().await;
    }
    let msgs = framed_messages(&c.received);
    let by_id = |id: u16| {
        msgs.iter()
            .find(|m| u16::from_be_bytes([m[0], m[1]]) == id)
            .unwrap()
    };
    assert_eq!(
        dns::parse_a_records(by_id(0x1111)),
        vec![(Ipv4Addr::LOCALHOST, 300)]
    );
    assert_eq!(
        by_id(0x2222)[3] & 0xf,
        3,
        "denied name gets NXDOMAIN over TCP too"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "denied query must not reach upstream"
    );
    let q = next_event(&mut r.log, FlowKind::DnsQuery).await;
    assert_eq!((q.proto, q.name.as_deref()), (Proto::Tcp, Some("a.test")));
    let d = next_event(&mut r.log, FlowKind::DnsDeny).await;
    assert_eq!(
        (d.proto, d.reason.as_deref()),
        (Proto::Tcp, Some("deny-domain"))
    );

    // The gateway address serves DNS too, and closes after the guest's FIN.
    c.fin().await;
    c.wait_fin().await;
    let mut c2 = Conn::connect(g, cfg.gateway_ip, 40101, 53).await;
    c2.send(&dns::tcp_frame(&dns::test_query(0x3333, "b.test")))
        .await;
    while framed_messages(&c2.received).is_empty() {
        c2.step().await;
    }
    assert_eq!(&c2.received[2..4], &[0x33, 0x33]);
}

/// Upstream that answers UDP with TC=1 and serves the full 100-record answer over TCP on the same port.
async fn truncating_upstream() -> (SocketAddr, Arc<AtomicUsize>) {
    let u = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = u.local_addr().unwrap();
    let l = TcpListener::bind(addr).await.unwrap();
    let tcp_count = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        let mut b = vec![0u8; 2048];
        while let Ok((n, p)) = u.recv_from(&mut b).await {
            let full = dns::test_big_answer(&b[..n], 100);
            let t = dns::truncate_reply(&full).unwrap();
            let _ = u.send_to(&t, p).await;
        }
    });
    let c = tcp_count.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let c = c.clone();
            tokio::spawn(async move {
                let mut len = [0u8; 2];
                if s.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut q = vec![0u8; usize::from(u16::from_be_bytes(len))];
                if s.read_exact(&mut q).await.is_err() {
                    return;
                }
                c.fetch_add(1, Ordering::SeqCst);
                let _ = s
                    .write_all(&dns::tcp_frame(&dns::test_big_answer(&q, 100)))
                    .await;
            });
        }
    });
    (addr, tcp_count)
}

#[tokio::test]
async fn dns_truncation_uses_tcp_upstream_and_tcp_retry_for_guest() {
    let (up, tcp_count) = truncating_upstream().await;
    let mut cfg = UserNetConfig::default();
    cfg.dns_upstreams = vec![up];
    let r = start(cfg.clone());
    let g = &r.guest;

    // Plain UDP query (512-byte limit): the answer (~1.7 KB) is replaced by a TC reply.
    let q = dns::test_query(0x0a0a, "big.test");
    g.send_udp(cfg.dns_ip, 5400, 53, &q).await;
    let a = recv_udp_reassembled(g, 53).await;
    assert!(
        dns::is_truncated(&a),
        "guest must be told to retry over TCP"
    );
    assert!(a.len() < 100);
    assert_eq!(
        tcp_count.load(Ordering::SeqCst),
        1,
        "upstream UDP truncation was retried over TCP"
    );

    // The guest retries over TCP and gets the whole answer.
    let mut c = Conn::connect(g, cfg.dns_ip, 40110, 53).await;
    c.send(&dns::tcp_frame(&q)).await;
    while framed_messages(&c.received).is_empty() {
        c.step().await;
    }
    let full = &framed_messages(&c.received)[0];
    assert!(!dns::is_truncated(full));
    assert_eq!(dns::parse_a_records(full).len(), 100);
    assert!(full.len() > 1500);

    // With EDNS0 (4096) the same answer goes over UDP, IP-fragmented by the gateway.
    let mut e = dns::test_query(0x0b0b, "big.test");
    e[10..12].copy_from_slice(&1u16.to_be_bytes());
    e.extend_from_slice(&[0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]);
    g.send_udp(cfg.dns_ip, 5401, 53, &e).await;
    let a = recv_udp_reassembled(g, 53).await;
    assert!(!dns::is_truncated(&a));
    assert_eq!(dns::parse_a_records(&a).len(), 100);
}

// ----- ICMP echo ----------------------------------------------------------------------------------------------

fn echo_request(id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut v = vec![8, 0, 0, 0];
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(data);
    let c = checksum(&v);
    v[2..4].copy_from_slice(&c.to_be_bytes());
    v
}

fn as_icmp4(f: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, Vec<u8>)> {
    let e = parse_eth(f)?;
    if e.ethertype != ETH_IPV4 {
        return None;
    }
    let ip = parse_ipv4(e.payload)?;
    (ip.proto == PROTO_ICMP).then(|| (ip.src, ip.dst, ip.payload.to_vec()))
}

#[tokio::test]
async fn icmp_echo_is_proxied_through_a_datagram_socket() {
    let mut r = start(cfg_allow_loopback());
    let g = &r.guest;
    g.send_ip(
        Ipv4Addr::LOCALHOST,
        PROTO_ICMP,
        &echo_request(0x4242, 7, b"hello-flux"),
    )
    .await;
    // Either the flow opens, or this environment forbids unprivileged ICMP sockets (documented fallback).
    let first = timeout(Duration::from_secs(5), async {
        loop {
            let e = r.log.recv().await.unwrap();
            if e.proto == Proto::Icmp {
                return e;
            }
        }
    })
    .await
    .expect("no icmp flow event");
    if first.kind == FlowKind::Deny {
        let why = first.reason.unwrap();
        assert!(
            why.starts_with("icmp-socket-unavailable"),
            "unexpected denial: {why}"
        );
        eprintln!("skipping: {why}");
        return;
    }
    assert_eq!(first.kind, FlowKind::Open);
    assert_eq!(first.remote, SocketAddr::from(([127, 0, 0, 1], 0)));
    let (src, dst, m) = g
        .recv_where("echo reply from 127.0.0.1", |f| {
            as_icmp4(f).filter(|(s, _, _)| *s == Ipv4Addr::LOCALHOST)
        })
        .await;
    assert_eq!((src, dst), (Ipv4Addr::LOCALHOST, g.cfg.guest_ip));
    assert_eq!((m[0], m[1]), (0, 0), "echo reply");
    assert_eq!(checksum(&m), 0);
    assert_eq!(
        u16::from_be_bytes([m[4], m[5]]),
        0x4242,
        "guest identifier restored"
    );
    assert_eq!(u16::from_be_bytes([m[6], m[7]]), 7);
    assert_eq!(&m[8..], b"hello-flux");

    // A second request on the same flow is answered too (sequence mapping).
    g.send_ip(
        Ipv4Addr::LOCALHOST,
        PROTO_ICMP,
        &echo_request(0x4242, 8, b"again"),
    )
    .await;
    let (_, _, m) = g
        .recv_where("second reply", |f| {
            as_icmp4(f).filter(|(s, _, _)| *s == Ipv4Addr::LOCALHOST)
        })
        .await;
    assert_eq!(u16::from_be_bytes([m[6], m[7]]), 8);
}

#[tokio::test]
async fn icmp_echo_policy_and_rate_limit() {
    // Default policy: loopback is private and refused, nothing comes back.
    let mut r = start(UserNetConfig::default());
    let g = &r.guest;
    g.send_ip(Ipv4Addr::LOCALHOST, PROTO_ICMP, &echo_request(1, 1, b"x"))
        .await;
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(
        (ev.proto, ev.reason.as_deref()),
        (Proto::Icmp, Some("private-range"))
    );
    assert!(g.silent(150, |f| as_icmp4(f).is_some()).await);

    // Disabled proxy.
    let mut cfg = cfg_allow_loopback();
    cfg.icmp_proxy = false;
    let mut r = start(cfg);
    r.guest
        .send_ip(Ipv4Addr::LOCALHOST, PROTO_ICMP, &echo_request(1, 1, b"x"))
        .await;
    let ev = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(ev.reason.as_deref(), Some("icmp-disabled"));

    // Rate limit: 1/s with a burst of 2, ten requests at once.
    let mut cfg = cfg_allow_loopback();
    cfg.icmp_rate_per_sec = 1;
    let mut r = start(cfg);
    for seq in 0..10 {
        r.guest
            .send_ip(Ipv4Addr::LOCALHOST, PROTO_ICMP, &echo_request(9, seq, b"x"))
            .await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    // Identical denials are collapsed into one event (the rest are counted in `suppressed` of the next one), so
    // the burst shows up as a single rate-limit event.
    let mut limited = 0;
    while let Ok(e) = r.log.try_recv() {
        if e.kind == FlowKind::Deny && e.reason.as_deref() == Some("rate-limit") {
            limited += 1 + e.suppressed;
        }
    }
    assert!(
        (1..=2).contains(&limited),
        "expected one collapsed rate-limit event, saw {limited}"
    );
}

#[tokio::test]
async fn icmp_to_gateway_still_answered_locally() {
    let r = start(UserNetConfig::default());
    let g = &r.guest;
    g.send_ip(g.cfg.dns_ip, PROTO_ICMP, &echo_request(3, 3, b"gw"))
        .await;
    let (src, _, m) = g.recv_where("reply", as_icmp4).await;
    assert_eq!((src, m[0]), (g.cfg.dns_ip, 0));
}

// ----- TCP: window scaling, SACK, congestion control -----------------------------------------------------------

#[tokio::test]
async fn tcp_negotiates_window_scaling_and_sacks_out_of_order_data() {
    let port = tcp_echo_server().await;
    let r = start(cfg_allow_loopback());
    let g = &r.guest;
    let (sport, isn) = (40200u16, 9000u32);
    g.send_tcp(
        Ipv4Addr::LOCALHOST,
        sport,
        port,
        TcpOut {
            seq: isn,
            ..syn_opts(65535)
        },
    )
    .await;
    let sa = g
        .recv_where("SYN-ACK", |f| {
            as_tcp(f).filter(|s| s.sport == port && s.flags & SYN != 0)
        })
        .await;
    assert_eq!(sa.wscale, Some(3), "window scaling negotiated");
    assert!(sa.sack_ok, "SACK-permitted echoed");
    assert_eq!(sa.window, 65535, "SYN windows are never scaled");
    let gw_isn = sa.seq.wrapping_add(1);
    let mk = |seq: u32, payload: &[u8]| TcpOut {
        seq,
        ack: gw_isn,
        flags: ACK | PSH,
        window: 512,
        payload: payload.to_vec(),
        ..Default::default()
    };
    g.send_tcp(
        Ipv4Addr::LOCALHOST,
        sport,
        port,
        TcpOut {
            seq: isn + 1,
            ack: gw_isn,
            flags: ACK,
            window: 512,
            ..Default::default()
        },
    )
    .await;
    let a: Vec<u8> = (0..1000u32).map(|i| (i % 200) as u8).collect();
    let b: Vec<u8> = (0..1000u32).map(|i| 100 + (i % 100) as u8).collect();

    // The second 1000 bytes first: a duplicate ACK with a SACK block.
    g.send_tcp(Ipv4Addr::LOCALHOST, sport, port, mk(isn + 1001, &b))
        .await;
    let s = g
        .recv_where("dup ack with sack", |f| {
            as_tcp(f).filter(|s| s.sport == port && s.payload.is_empty() && !s.sack.is_empty())
        })
        .await;
    assert_eq!(s.ack, isn + 1);
    assert_eq!(s.sack, vec![(isn + 1001, isn + 2001)]);
    assert!(
        s.window > 1000,
        "scaled receive window advertised: {}",
        s.window
    );

    // The hole fills: cumulative ACK jumps over both, and the echo returns the bytes in order.
    g.send_tcp(Ipv4Addr::LOCALHOST, sport, port, mk(isn + 1, &a))
        .await;
    let s = g
        .recv_where("cumulative ack", |f| {
            as_tcp(f).filter(|s| s.sport == port && s.payload.is_empty() && s.ack == isn + 2001)
        })
        .await;
    assert!(s.sack.is_empty());
    let mut got = Vec::new();
    let mut next = sa.seq.wrapping_add(1);
    while got.len() < 2000 {
        let d = g
            .recv_where("echoed data", |f| {
                as_tcp(f).filter(|s| s.sport == port && !s.payload.is_empty() && s.seq == next)
            })
            .await;
        next = next.wrapping_add(d.payload.len() as u32);
        got.extend_from_slice(&d.payload);
    }
    assert_eq!(&got[..1000], &a[..]);
    assert_eq!(&got[1000..], &b[..]);
}

/// Guest-side bulk receiver with optional loss: acks every segment, re-ordering through a small buffer.
/// Returns (bytes, seconds, time from drop to the retransmission of the dropped segment).
async fn bulk_receive(
    g: &Guest,
    remote: Ipv4Addr,
    sport: u16,
    dport: u16,
    total: usize,
    drop_nth: Option<usize>,
) -> (Vec<u8>, f64, Option<Duration>) {
    let isn = 20_000u32;
    g.send_tcp(
        remote,
        sport,
        dport,
        TcpOut {
            seq: isn,
            ..syn_opts(65535)
        },
    )
    .await;
    let sa = g
        .recv_where("SYN-ACK", |f| {
            as_tcp(f).filter(|s| s.sport == dport && s.dport == sport && s.flags & SYN != 0)
        })
        .await;
    let mut rcv_nxt = sa.seq.wrapping_add(1);
    let ack = |rcv_nxt: u32| TcpOut {
        seq: isn + 1,
        ack: rcv_nxt,
        flags: ACK,
        window: 4096, // << 7 = 512 KiB
        ..Default::default()
    };
    g.send_tcp(remote, sport, dport, ack(rcv_nxt)).await;
    let mut got = Vec::with_capacity(total);
    let mut ooo: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let (mut data_segs, mut dropped_seq, mut dropped_at, mut recovered) =
        (0usize, None, None, None);
    let t0 = Instant::now();
    let mut fin = false;
    let mut buf = vec![0u8; 65536];
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(fin && got.len() >= total) {
        assert!(
            Instant::now() < deadline,
            "bulk transfer stalled at {} of {total}",
            got.len()
        );
        let n = timeout(Duration::from_secs(5), g.s.recv(&mut buf))
            .await
            .expect("bulk transfer: no frame for 5 s")
            .unwrap();
        let Some(s) = as_tcp(&buf[..n]).filter(|s| s.sport == dport && s.dport == sport) else {
            continue;
        };
        if !s.payload.is_empty() {
            data_segs += 1;
            if Some(data_segs) == drop_nth && dropped_seq.is_none() {
                dropped_seq = Some(s.seq);
                dropped_at = Some(Instant::now());
                continue; // lose it
            }
            if Some(s.seq) == dropped_seq && recovered.is_none() {
                recovered = dropped_at.map(|t| t.elapsed());
            }
            if s.seq == rcv_nxt {
                got.extend_from_slice(&s.payload);
                rcv_nxt = rcv_nxt.wrapping_add(s.payload.len() as u32);
                while let Some(d) = ooo.remove(&rcv_nxt) {
                    got.extend_from_slice(&d);
                    rcv_nxt = rcv_nxt.wrapping_add(d.len() as u32);
                }
            } else if (s.seq.wrapping_sub(rcv_nxt) as i32) > 0 {
                ooo.insert(s.seq, s.payload.clone());
            }
            if s.flags & FIN != 0 {
                fin = true;
            }
            g.send_tcp(remote, sport, dport, ack(rcv_nxt)).await;
        } else if s.flags & FIN != 0 && s.seq == rcv_nxt {
            fin = true;
            rcv_nxt = rcv_nxt.wrapping_add(1);
            g.send_tcp(remote, sport, dport, ack(rcv_nxt)).await;
        }
    }
    (got, t0.elapsed().as_secs_f64(), recovered)
}

#[tokio::test]
async fn tcp_fast_retransmit_recovers_a_lost_segment_before_the_rto() {
    let total = 300_000;
    let port = tcp_server_sink(total).await;
    let r = start(cfg_allow_loopback());
    let (data, secs, recovered) =
        bulk_receive(&r.guest, Ipv4Addr::LOCALHOST, 40300, port, total, Some(5)).await;
    assert_eq!(data.len(), total);
    assert!(pattern_ok(&data), "data corrupted across loss recovery");
    let rec = recovered.expect("the dropped segment was retransmitted");
    assert!(
        rec < Duration::from_millis(300),
        "retransmission took {rec:?}: that is the 400 ms RTO, not fast retransmit"
    );
    assert!(secs < 10.0);
}

async fn bulk_transfer(total: usize) {
    let port = tcp_server_sink(total).await;
    let r = start(cfg_allow_loopback());
    let (data, secs, _) =
        bulk_receive(&r.guest, Ipv4Addr::LOCALHOST, 40301, port, total, None).await;
    assert_eq!(data.len(), total);
    assert!(pattern_ok(&data));
    eprintln!(
        "tcp throughput: {} bytes in {secs:.2} s = {:.1} MB/s ({:.0} Mbit/s)",
        total,
        total as f64 / secs / 1e6,
        total as f64 * 8.0 / secs / 1e6
    );
}

/// 20 MB host -> guest through window scaling and congestion control (about 2 s in a debug build); prints bytes/s,
/// visible with `--nocapture`.
#[tokio::test]
async fn tcp_throughput_20mb() {
    bulk_transfer(20 * 1024 * 1024).await;
}

// ----- IP fragmentation ---------------------------------------------------------------------------------------

#[tokio::test]
async fn ipv4_fragments_are_reassembled_and_replies_fragmented() {
    let port = udp_echo_server().await;
    let r = start(cfg_allow_loopback());
    let g = &r.guest;
    let data: Vec<u8> = (0..3000u32).map(|i| (i * 13 % 251) as u8).collect();
    let udp = build_udp(g.cfg.guest_ip, Ipv4Addr::LOCALHOST, 41500, port, &data);
    let ip = build_ipv4_with(
        g.cfg.guest_ip,
        Ipv4Addr::LOCALHOST,
        PROTO_UDP,
        &udp,
        4242,
        false,
    );
    let mut frags = fragment_ipv4(&ip, 1500).unwrap();
    assert_eq!(frags.len(), 3);
    frags.reverse(); // out of order
    for f in &frags {
        g.send_eth(g.cfg.gateway_mac, ETH_IPV4, f).await;
    }
    let echoed = recv_udp_reassembled(g, port).await;
    assert!(
        echoed == data,
        "echoed {} bytes, sent {}",
        echoed.len(),
        data.len()
    );

    // The reply left as fragments (it is larger than the MTU).
    // (recv_udp_reassembled consumed them; send again and count the raw frames instead.)
    for f in &frags {
        g.send_eth(g.cfg.gateway_mac, ETH_IPV4, f).await;
    }
    let mut n_frags = 0;
    let mut buf = vec![0u8; 65536];
    let mut complete = false;
    let mut re = frag::Reassembler::default();
    while !complete {
        let n = timeout(Duration::from_secs(5), g.s.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let e = parse_eth(&buf[..n]).unwrap();
        let ip = parse_ipv4(e.payload).unwrap();
        if ip.proto == PROTO_UDP && ip.fragment {
            assert!(n <= 14 + 1500, "fragment exceeds the MTU: {n}");
            n_frags += 1;
            complete = re.add(&ip, Instant::now()).is_some();
        }
    }
    assert_eq!(n_frags, 3);
}

#[tokio::test]
async fn oversized_df_packet_gets_icmp_fragmentation_needed() {
    let mut cfg = cfg_allow_loopback();
    cfg.mtu = 1000;
    let r = start(cfg.clone());
    let g = &r.guest;
    let udp = build_udp(cfg.guest_ip, Ipv4Addr::LOCALHOST, 41600, 9, &[7u8; 1180]);
    let ip = build_ipv4(cfg.guest_ip, Ipv4Addr::LOCALHOST, PROTO_UDP, &udp); // DF set
    assert!(ip.len() > 1000);
    g.send_eth(cfg.gateway_mac, ETH_IPV4, &ip).await;
    let (src, dst, m) = g.recv_where("icmp frag needed", as_icmp4).await;
    assert_eq!((src, dst), (cfg.gateway_ip, cfg.guest_ip));
    assert_eq!((m[0], m[1]), (3, 4));
    assert_eq!(checksum(&m), 0);
    assert_eq!(u16::from_be_bytes([m[6], m[7]]), 1000, "next-hop MTU");
    assert_eq!(&m[8..28], &ip[..20], "quotes the offending header");
}

// ----- IPv6 ---------------------------------------------------------------------------------------------------

fn cfg6() -> UserNetConfig {
    let mut c = UserNetConfig::default();
    c.ipv6 = true;
    c.egress.allow_cidrs6 = vec!["::1/128".parse().unwrap()];
    c
}

fn ll_of(mac: Mac) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 2, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

impl Guest {
    async fn send_icmp6(
        &self,
        src: Ipv6Addr,
        dst: Ipv6Addr,
        ty: u8,
        body: &[u8],
        hop: u8,
        mac: Mac,
    ) {
        let m = build_icmpv6(src, dst, ty, 0, body);
        let ip = build_ipv6(src, dst, PROTO_ICMPV6, hop, &m);
        self.send_eth(mac, ETH_IPV6, &ip).await;
    }
}

type Icmp6 = (Ipv6Addr, Ipv6Addr, u8, Vec<u8>);

fn icmp6_type(want: u8) -> impl Fn(&[u8]) -> Option<Icmp6> {
    move |f| as_icmp6(f).filter(|(_, _, _, m)| m[0] == want)
}

#[tokio::test]
async fn ipv6_router_advertisement_and_neighbor_discovery() {
    let cfg = cfg6();
    let r = start(cfg.clone());
    let g = &r.guest;
    let guest_ll = ll_of(GMAC);
    let all_routers: Ipv6Addr = "ff02::2".parse().unwrap();

    // Router solicitation -> RA with the prefix, MTU, router lifetime and RDNSS.
    let mut rs = vec![0u8; 4];
    rs.extend_from_slice(&[1, 1]);
    rs.extend_from_slice(&GMAC);
    g.send_icmp6(
        guest_ll,
        all_routers,
        133,
        &rs,
        255,
        [0x33, 0x33, 0, 0, 0, 2],
    )
    .await;
    let (src, _dst, hop, m) = g.recv_where("RA", icmp6_type(134)).await;
    assert_eq!((src, hop), (cfg.gateway_ll6(), 255));
    assert_eq!(u16::from_be_bytes([m[6], m[7]]), 1800, "router lifetime");
    let (mut prefix, mut rdnss, mut mtu, mut slla) = (None, None, None, None);
    let mut o = &m[16..];
    while o.len() >= 2 {
        let l = usize::from(o[1]) * 8;
        assert!(l >= 8 && l <= o.len(), "malformed option");
        match o[0] {
            1 => slla = Some(o[2..8].to_vec()),
            3 => {
                prefix = Some((
                    o[2],
                    o[3],
                    Ipv6Addr::from(<[u8; 16]>::try_from(&o[16..32]).unwrap()),
                ))
            }
            5 => mtu = Some(u32::from_be_bytes([o[4], o[5], o[6], o[7]])),
            25 => rdnss = Some(Ipv6Addr::from(<[u8; 16]>::try_from(&o[8..24]).unwrap())),
            _ => {}
        }
        o = &o[l..];
    }
    assert_eq!(
        prefix,
        Some((64, 0xc0, cfg.ipv6_prefix)),
        "on-link + autonomous /64"
    );
    assert_eq!(rdnss, Some(cfg.dns_ip6()));
    assert_eq!(mtu, Some(1500));
    assert_eq!(slla, Some(cfg.gateway_mac.to_vec()));

    // Neighbour solicitation for the gateway (and DNS) address -> NA with our MAC.
    for target in [cfg.gateway_ip6(), cfg.dns_ip6(), cfg.gateway_ll6()] {
        let mut ns = vec![0u8; 4];
        ns.extend_from_slice(&target.octets());
        ns.extend_from_slice(&[1, 1]);
        ns.extend_from_slice(&GMAC);
        let t = target.octets();
        let snm = Ipv6Addr::new(
            0xff02,
            0,
            0,
            0,
            0,
            1,
            0xff00 | u16::from(t[13]),
            u16::from_be_bytes([t[14], t[15]]),
        );
        g.send_icmp6(g.ip6(), snm, 135, &ns, 255, v6_multicast_mac(snm))
            .await;
        let (nsrc, ndst, hop, m) = g
            .recv_where("NA", |f| {
                icmp6_type(136)(f).filter(|(s, _, _, _)| *s == target)
            })
            .await;
        assert_eq!((ndst, hop), (g.ip6(), 255));
        assert_eq!(&m[8..24], &target.octets());
        assert_eq!(m[4] & 0x60, 0x60, "solicited + override");
        assert_eq!(
            &m[24..32],
            &[
                2,
                1,
                cfg.gateway_mac[0],
                cfg.gateway_mac[1],
                cfg.gateway_mac[2],
                cfg.gateway_mac[3],
                cfg.gateway_mac[4],
                cfg.gateway_mac[5]
            ]
        );
        assert_eq!(nsrc, target);
    }

    // DAD for the guest's own tentative address is not answered; a solicitation with a bad hop limit neither.
    let mut dad = vec![0u8; 4];
    dad.extend_from_slice(&g.ip6().octets());
    let snm: Ipv6Addr = "ff02::1:ff00:1234".parse().unwrap();
    g.send_icmp6(
        Ipv6Addr::UNSPECIFIED,
        snm,
        135,
        &dad,
        255,
        v6_multicast_mac(snm),
    )
    .await;
    let mut bad = vec![0u8; 4];
    bad.extend_from_slice(&cfg.gateway_ip6().octets());
    g.send_icmp6(g.ip6(), snm, 135, &bad, 64, v6_multicast_mac(snm))
        .await;
    assert!(g.silent(200, |f| icmp6_type(136)(f).is_some()).await);

    // ICMPv6 echo to the gateway.
    let mut echo = vec![0x12, 0x34, 0, 9];
    echo.extend_from_slice(b"ping6");
    g.send_icmp6(g.ip6(), cfg.gateway_ip6(), 128, &echo, 64, cfg.gateway_mac)
        .await;
    let (src, dst, _, m) = g.recv_where("echo reply", icmp6_type(129)).await;
    assert_eq!((src, dst), (cfg.gateway_ip6(), g.ip6()));
    assert_eq!(&m[4..], &echo[..]);
}

#[tokio::test]
async fn ipv6_is_ignored_unless_enabled() {
    let r = start(UserNetConfig::default());
    let g = &r.guest;
    let rs = vec![0u8; 4];
    g.send_icmp6(
        ll_of(GMAC),
        "ff02::2".parse().unwrap(),
        133,
        &rs,
        255,
        [0x33, 0x33, 0, 0, 0, 2],
    )
    .await;
    g.send_udp(Ipv6Addr::LOCALHOST, 5000, 5000, b"x").await;
    assert!(
        g.silent(300, |f| parse_eth(f)
            .is_some_and(|e| e.ethertype == ETH_IPV6))
            .await
    );
}

async fn udp_echo_server6() -> u16 {
    let s = UdpSocket::bind("[::1]:0").await.unwrap();
    let port = s.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut b = vec![0u8; 2048];
        while let Ok((n, p)) = s.recv_from(&mut b).await {
            let _ = s.send_to(&b[..n], p).await;
        }
    });
    port
}

async fn tcp_echo_server6() -> u16 {
    let l = TcpListener::bind("[::1]:0").await.unwrap();
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

#[tokio::test]
async fn ipv6_udp_and_tcp_nat() {
    let up = udp_echo_server6().await;
    let tp = tcp_echo_server6().await;
    let mut r = start(cfg6());
    let g = &r.guest;
    g.send_udp(Ipv6Addr::LOCALHOST, 42500, up, b"six").await;
    let (src, dst, sport, dport, p) = g.recv_where("udp6 echo", as_udp6).await;
    assert_eq!(
        (src, dst, sport, dport, p.as_slice()),
        (Ipv6Addr::LOCALHOST, g.ip6(), up, 42500, &b"six"[..])
    );
    let ev = next_event(&mut r.log, FlowKind::Open).await;
    assert_eq!(ev.remote, SocketAddr::new(Ipv6Addr::LOCALHOST.into(), up));
    assert_eq!(ev.guest, SocketAddr::new(g.ip6().into(), 42500));

    let mut c = Conn::connect(g, Ipv6Addr::LOCALHOST, 42501, tp).await;
    let blob: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 253) as u8).collect();
    c.send(&blob).await;
    c.read_until(blob.len()).await;
    assert!(
        c.received == blob,
        "echo mismatch ({} bytes)",
        c.received.len()
    );
    c.fin().await;
    c.wait_fin().await;
    let close = next_event(&mut r.log, FlowKind::Close).await;
    assert_eq!((close.proto, close.bytes_tx), (Proto::Tcp, 50_000));
}

#[tokio::test]
async fn ipv6_egress_policy_denies_local_ranges_by_default() {
    let tp = tcp_echo_server6().await;
    let mut cfg = UserNetConfig::default();
    cfg.ipv6 = true; // no allow lists: "public internet only"
    let mut r = start(cfg.clone());
    let g = &r.guest;
    for (i, dst) in ["::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "fec0::1"]
        .into_iter()
        .enumerate()
    {
        let dst: Ipv6Addr = dst.parse().unwrap();
        g.send_udp(dst, 43000 + i as u16, 9, b"x").await;
        let ev = next_event(&mut r.log, FlowKind::Deny).await;
        assert_eq!(ev.reason.as_deref(), Some("private-range"), "{dst}");
        assert_eq!(ev.remote.ip(), IpAddr::V6(dst));
    }
    // TCP is reset and logged.
    g.send_tcp(
        Ipv6Addr::LOCALHOST,
        43010,
        tp,
        TcpOut {
            seq: 1,
            flags: SYN,
            window: 65535,
            ..Default::default()
        },
    )
    .await;
    let s = g
        .recv_where("RST", |f| as_tcp(f).filter(|s| s.sport == tp))
        .await;
    assert_eq!(s.flags & RST, RST);
    assert_eq!(
        next_event(&mut r.log, FlowKind::Deny)
            .await
            .reason
            .as_deref(),
        Some("private-range")
    );
    // Multicast is never a destination; the gateway alias is off.
    g.send_udp("ff02::fb".parse::<Ipv6Addr>().unwrap(), 5353, 5353, b"x")
        .await;
    g.send_udp(cfg.gateway_ip6(), 43020, 9, b"x").await;
    assert_eq!(
        next_event(&mut r.log, FlowKind::Deny)
            .await
            .reason
            .as_deref(),
        Some("host-loopback")
    );
    // Explicit CIDRs, v6 deny wins.
    let mut cfg = UserNetConfig::default();
    cfg.ipv6 = true;
    cfg.egress.allow_cidrs6 = vec!["::1/128".parse().unwrap()];
    cfg.egress.deny_cidrs6 = vec!["::1/128".parse().unwrap()];
    let mut r = start(cfg);
    r.guest.send_udp(Ipv6Addr::LOCALHOST, 43030, 9, b"x").await;
    assert_eq!(
        next_event(&mut r.log, FlowKind::Deny)
            .await
            .reason
            .as_deref(),
        Some("deny-cidr")
    );
}

/// Upstream that answers every AAAA (type 28) query with ::1 and everything else with an A of 127.0.0.1.
async fn fake_dns_aaaa() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = s.local_addr().unwrap();
    tokio::spawn(async move {
        let mut b = vec![0u8; 2048];
        while let Ok((n, p)) = s.recv_from(&mut b).await {
            let q = &b[..n];
            let pq = dns::parse_query(q).unwrap();
            let qtype = u16::from_be_bytes([q[pq.question_end - 4], q[pq.question_end - 3]]);
            let mut a = dns::build_reply(q, &pq, 0, false);
            a[6..8].copy_from_slice(&1u16.to_be_bytes());
            if qtype == 28 {
                a.extend_from_slice(&[0xc0, 12, 0, 28, 0, 1, 0, 0, 1, 44, 0, 16]);
                a.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
            } else {
                a.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 1, 44, 0, 4, 127, 0, 0, 1]);
            }
            let _ = s.send_to(&a, p).await;
        }
    });
    addr
}

#[tokio::test]
async fn ipv6_dns_aaaa_teaches_the_allow_list_and_tcp_dns_works() {
    let tp = tcp_echo_server6().await;
    let up = fake_dns_aaaa().await;
    let mut cfg = UserNetConfig::default();
    cfg.ipv6 = true;
    cfg.dns_upstreams = vec![up];
    cfg.egress.allow_domains = vec!["example.test".into()];
    let mut r = start(cfg.clone());
    let g = &r.guest;
    let syn = TcpOut {
        seq: 1,
        flags: SYN,
        window: 65535,
        ..Default::default()
    };

    // ::1 is private and not allowed until an AAAA answer for an allowed name teaches it.
    g.send_tcp(Ipv6Addr::LOCALHOST, 44100, tp, syn).await;
    g.recv_where("RST", |f| {
        as_tcp(f).filter(|s| s.sport == tp && s.flags & RST != 0)
    })
    .await;
    assert_eq!(
        next_event(&mut r.log, FlowKind::Deny)
            .await
            .reason
            .as_deref(),
        Some("private-range")
    );

    // AAAA query over UDP to the IPv6 DNS address.
    g.send_udp(
        cfg.dns_ip6(),
        5300,
        53,
        &dns::test_query_type(0x0606, "api.example.test", 28),
    )
    .await;
    let (src, dst, sport, _, a) = g.recv_where("aaaa answer", as_udp6).await;
    assert_eq!((src, dst, sport), (cfg.dns_ip6(), g.ip6(), 53));
    assert_eq!(
        dns::parse_addr_records(&a),
        vec![(IpAddr::V6(Ipv6Addr::LOCALHOST), 300)]
    );

    let mut c = Conn::connect(g, Ipv6Addr::LOCALHOST, 44101, tp).await;
    c.send(b"learned").await;
    c.read_until(7).await;
    assert_eq!(c.received, b"learned");

    // DNS over TCP on the v6 address, same policy (other.test is not allowed).
    let mut d = Conn::connect(g, cfg.dns_ip6(), 44102, 53).await;
    d.send(&dns::tcp_frame(&dns::test_query_type(
        0x0707,
        "other.test",
        28,
    )))
    .await;
    while framed_messages(&d.received).is_empty() {
        d.step().await;
    }
    assert_eq!(framed_messages(&d.received)[0][3] & 0xf, 3);
    let ev = next_event(&mut r.log, FlowKind::DnsDeny).await;
    assert_eq!(
        (ev.proto, ev.reason.as_deref()),
        (Proto::Tcp, Some("domain-not-allowed"))
    );
}

#[tokio::test]
async fn ipv6_spoofed_source_outside_prefix_is_dropped() {
    let up = udp_echo_server6().await;
    let r = start(cfg6());
    let g = &r.guest;
    let u = build_udp(
        "2001:db8::5".parse::<Ipv6Addr>().unwrap(),
        Ipv6Addr::LOCALHOST,
        1,
        up,
        b"x",
    );
    let ip = build_ipv6(
        "2001:db8::5".parse().unwrap(),
        Ipv6Addr::LOCALHOST,
        PROTO_UDP,
        64,
        &u,
    );
    g.send_eth(g.cfg.gateway_mac, ETH_IPV6, &ip).await;
    assert!(g.silent(200, |f| as_udp6(f).is_some()).await);
}

// ----- tap verdicts -------------------------------------------------------------------------------------------

fn start_tapped(cfg: UserNetConfig, version: u8) -> (Running, UnixDatagram) {
    let (a, b) = StdUnixDatagram::pair().unwrap();
    let (tap_tx, tap_rx) = StdUnixDatagram::pair().unwrap();
    setbufs(&a);
    setbufs(&b);
    b.set_nonblocking(true).unwrap();
    let mut un = UserNet::new(cfg.clone(), a).unwrap();
    un.set_tap(tap_tx).unwrap();
    un.set_tap_version(version).unwrap();
    let handle = un.handle();
    let log = handle.subscribe();
    let fwd = un.forward_addrs().to_vec();
    let task = tokio::spawn(un.run());
    tap_rx.set_nonblocking(true).unwrap();
    let tap_rx = UnixDatagram::from_std(tap_rx).unwrap();
    (
        Running {
            guest: Guest {
                s: UnixDatagram::from_std(b).unwrap(),
                cfg,
            },
            log,
            fwd,
            handle,
            task,
        },
        tap_rx,
    )
}

/// Next tap datagram as (dir, verdict, frame) for a version 2 tap.
async fn tap_v2(rx: &UnixDatagram) -> (u8, u8, Vec<u8>) {
    let mut buf = vec![0u8; 70000];
    let n = timeout(Duration::from_secs(3), rx.recv(&mut buf))
        .await
        .expect("tap datagram")
        .unwrap();
    (buf[0], buf[1], buf[2..n].to_vec())
}

#[tokio::test]
async fn tap_version_2_carries_policy_verdicts() {
    let (up, _) = fake_dns().await;
    let mut cfg = UserNetConfig::default();
    cfg.dns_upstreams = vec![up];
    cfg.egress.deny_domains = vec!["blocked.test".into()];
    let (r, tap) = start_tapped(cfg.clone(), TAP_VERSION_2);
    let g = &r.guest;

    // ARP: passes, reply is tapped to the guest with verdict 0.
    g.send_eth(
        BROADCAST,
        ETH_ARP,
        &build_arp(1, GMAC, cfg.guest_ip, [0; 6], cfg.gateway_ip),
    )
    .await;
    g.recv_where("ARP reply", |f| {
        parse_eth(f).filter(|e| e.ethertype == ETH_ARP).map(|_| ())
    })
    .await;
    let (d, v, f) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_PASS));
    assert_eq!(parse_eth(&f).unwrap().ethertype, ETH_ARP);
    let (d, v, _) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_TO_GUEST, TAP_VERDICT_PASS));

    // A UDP flow to a private address is denied by policy: the guest frame is marked, nothing comes back.
    g.send_udp(Ipv4Addr::new(10, 9, 9, 9), 45000, 9, b"nope")
        .await;
    let (d, v, f) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_DENIED));
    let (_, _, _, dport, p) = as_udp(&f).unwrap();
    assert_eq!((dport, p.as_slice()), (9, &b"nope"[..]));

    // A TCP SYN to a private address: the SYN is denied, then the RST goes out (verdict 0, after it).
    g.send_tcp(
        Ipv4Addr::new(10, 9, 9, 9),
        45001,
        80,
        TcpOut {
            seq: 1,
            flags: SYN,
            window: 100,
            ..Default::default()
        },
    )
    .await;
    let (d, v, _) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_DENIED));
    let (d, v, f) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_TO_GUEST, TAP_VERDICT_PASS));
    assert_eq!(as_tcp(&f).unwrap().flags & RST, RST);

    // DNS: an allowed name passes, a denied one is marked and answered with NXDOMAIN.
    g.send_udp(cfg.dns_ip, 45002, 53, &dns::test_query(1, "ok.test"))
        .await;
    let (d, v, _) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_PASS));
    let (d, _, _) = tap_v2(&tap).await;
    assert_eq!(d, TAP_TO_GUEST); // the answer
    g.send_udp(cfg.dns_ip, 45003, 53, &dns::test_query(2, "x.blocked.test"))
        .await;
    let (d, v, _) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_DENIED));
    let (d, v, f) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_TO_GUEST, TAP_VERDICT_PASS));
    assert_eq!(as_udp(&f).unwrap().4[3] & 0xf, 3);

    // A spoofed source address is dropped for a non-policy reason.
    let u = build_udp(Ipv4Addr::new(10, 0, 2, 99), cfg.dns_ip, 1, 9, b"x");
    let ip = build_ipv4(Ipv4Addr::new(10, 0, 2, 99), cfg.dns_ip, PROTO_UDP, &u);
    g.send_eth(cfg.gateway_mac, ETH_IPV4, &ip).await;
    let (d, v, _) = tap_v2(&tap).await;
    assert_eq!((d, v), (TAP_FROM_GUEST, TAP_VERDICT_DROPPED));
}

#[tokio::test]
async fn tap_version_1_is_the_default_and_unknown_versions_are_rejected() {
    // Default framing: [dir][frame], no verdict byte (frame_tap_sees_both_directions covers the full flow).
    let (r, tap) = start_tapped(UserNetConfig::default(), TAP_VERSION_1);
    let g = &r.guest;
    g.send_udp(Ipv4Addr::new(10, 9, 9, 9), 45100, 9, b"x").await;
    let mut buf = vec![0u8; 2048];
    let n = timeout(Duration::from_secs(3), tap.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf[0], TAP_FROM_GUEST);
    assert!(
        as_udp(&buf[1..n]).is_some(),
        "v1 datagram is dir byte + Ethernet frame"
    );

    let (a, _b) = StdUnixDatagram::pair().unwrap();
    let mut un = UserNet::new(UserNetConfig::default(), a).unwrap();
    assert!(un.set_tap_version(3).is_err());
    assert!(un.set_tap_version(0).is_err());
}

// ----- config -------------------------------------------------------------------------------------------------

#[test]
fn ipv6_config_defaults_json_and_validation() {
    let c = UserNetConfig::default();
    assert!(!c.ipv6, "v4-only unless asked");
    assert!(c.icmp_proxy);
    let c: UserNetConfig = serde_json::from_str(
        r#"{"ipv6":true,"ipv6_prefix":"fd00:1::","egress":{"allow_cidrs6":["2001:db8::/32"],"deny_cidrs6":["2001:db8:bad::/48"]}}"#,
    )
    .unwrap();
    c.validate().unwrap();
    assert_eq!(c.gateway_ip6(), "fd00:1::2".parse::<Ipv6Addr>().unwrap());
    assert_eq!(c.dns_ip6(), "fd00:1::3".parse::<Ipv6Addr>().unwrap());
    assert!(c.in_prefix6("fd00:1::abcd:1".parse().unwrap()));
    assert!(!c.in_prefix6("fd00:2::1".parse().unwrap()));
    let now = Instant::now();
    let l = HashMap::new();
    let a = |s: &str| c.egress.check6(s.parse().unwrap(), Some(443), &l, now);
    assert_eq!(a("2001:db8::1"), Decision::Allow);
    assert_eq!(a("2001:db8:bad::1"), Decision::Deny("deny-cidr"));
    assert_eq!(a("2606:4700::1"), Decision::Deny("not-in-allow-list"));
    assert_eq!(a("ff02::1"), Decision::Deny("non-unicast"));
    let mut bad = c.clone();
    bad.ipv6_prefix = "fd00:1::1:0:0:0".parse().unwrap();
    assert!(bad.validate().is_err(), "prefix must be a /64");
    let mut f = UserNetConfig::default();
    f.forwards = vec![PortForward::new(
        Proto::Icmp,
        Ipv4Addr::LOCALHOST.into(),
        1,
        1,
    )];
    assert!(f.validate().is_err());
    // default policy for v6
    let p = EgressPolicy::default();
    for ip in [
        "::1",
        "::",
        "fd12::1",
        "fe80::1",
        "fec0::1",
        "::ffff:8.8.8.8",
        "::8.8.8.8",
    ] {
        let d = p.check6(ip.parse().unwrap(), None, &l, now);
        assert!(matches!(d, Decision::Deny(_)), "{ip}: {d:?}");
    }
    assert_eq!(
        p.check6("2606:4700:4700::1111".parse().unwrap(), Some(443), &l, now),
        Decision::Allow
    );
    assert!("::1/129".parse::<Cidr6>().is_err());
    assert_eq!("fd00::/8".parse::<Cidr6>().unwrap().to_string(), "fd00::/8");
}

// ----- port forwards across address families ------------------------------------------------------------

/// Drive one host -> guest TCP forward end to end: the gateway's SYN must go from `gw` to `guest` on `gport`.
async fn forward_tcp_roundtrip(
    g: &Guest,
    mut host: TcpStream,
    gw: IpAddr,
    guest: IpAddr,
    gport: u16,
) {
    let syn = g
        .recv_where("forward SYN", |f| {
            as_tcp(f).filter(|s| s.flags & (SYN | ACK) == SYN)
        })
        .await;
    assert_eq!((syn.src, syn.dst, syn.dport), (gw, guest, gport));
    g.send_tcp(
        gw,
        gport,
        syn.sport,
        TcpOut {
            seq: 5000,
            ack: syn.seq + 1,
            flags: SYN | ACK,
            window: 65535,
            mss: Some(1200),
            ..Default::default()
        },
    )
    .await;
    host.write_all(b"ping").await.unwrap();
    let data = g
        .recv_where("forwarded data", |f| {
            as_tcp(f).filter(|s| s.dport == gport && !s.payload.is_empty())
        })
        .await;
    assert_eq!(data.payload, b"ping");
    g.send_tcp(
        gw,
        gport,
        syn.sport,
        TcpOut {
            seq: 5001,
            ack: data.seq + 4,
            flags: ACK | PSH,
            window: 65535,
            payload: b"pong".to_vec(),
            ..Default::default()
        },
    )
    .await;
    let mut buf = [0u8; 4];
    timeout(Duration::from_secs(5), host.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"pong");
}

#[tokio::test]
async fn tcp_forward_from_ipv6_host_listener_to_ipv4_guest() {
    let mut cfg = UserNetConfig::default();
    cfg.forwards = vec![PortForward::new(
        Proto::Tcp,
        Ipv6Addr::LOCALHOST.into(),
        0,
        8080,
    )];
    let r = start(cfg.clone());
    assert!(
        r.fwd[0].is_ipv6() && r.fwd[0].port() != 0,
        "bound {}",
        r.fwd[0]
    );
    let host = TcpStream::connect(r.fwd[0]).await.unwrap();
    forward_tcp_roundtrip(
        &r.guest,
        host,
        cfg.gateway_ip.into(),
        cfg.guest_ip.into(),
        8080,
    )
    .await;
}

#[tokio::test]
async fn tcp_forward_to_ipv6_guest_with_explicit_and_learned_address() {
    // Explicit guest_addr: works before the guest has sent anything, from an IPv4 host listener.
    let mut cfg = cfg6();
    let guest6 = {
        let mut s = cfg.ipv6_prefix.segments();
        s[7] = 0x1234; // the test guest's address (Guest::ip6)
        Ipv6Addr::from(s)
    };
    let mut f = PortForward::new(Proto::Tcp, Ipv4Addr::LOCALHOST.into(), 0, 9090);
    f.guest_family = GuestFamily::V6;
    f.guest_addr = Some(guest6.into());
    cfg.forwards = vec![f];
    let r = start(cfg.clone());
    assert_eq!(r.guest.ip6(), guest6);
    let host = TcpStream::connect(r.fwd[0]).await.unwrap();
    forward_tcp_roundtrip(
        &r.guest,
        host,
        cfg.gateway_ip6().into(),
        guest6.into(),
        9090,
    )
    .await;

    // Learned address: refused (and reported) until the guest has used its IPv6 address.
    let mut cfg = cfg6();
    let mut f = PortForward::new(Proto::Tcp, Ipv6Addr::LOCALHOST.into(), 0, 9091);
    f.guest_family = GuestFamily::V6;
    cfg.forwards = vec![f];
    let mut r = start(cfg.clone());
    let mut early = TcpStream::connect(r.fwd[0]).await.unwrap();
    let ev = next_event(&mut r.log, FlowKind::ConnectFailed).await;
    assert_eq!(ev.reason.as_deref(), Some("guest-ipv6-unknown"));
    let mut b = [0u8; 1];
    let n = timeout(Duration::from_secs(5), early.read(&mut b))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "the host connection is closed");
    // Any in-prefix packet teaches the address.
    let up = udp_echo_server6().await;
    r.guest
        .send_udp(Ipv6Addr::LOCALHOST, 42600, up, b"hello")
        .await;
    r.guest.recv_where("udp6 echo", as_udp6).await;
    let host = TcpStream::connect(r.fwd[0]).await.unwrap();
    forward_tcp_roundtrip(
        &r.guest,
        host,
        cfg.gateway_ip6().into(),
        r.guest.ip6().into(),
        9091,
    )
    .await;
}

#[tokio::test]
async fn udp_forward_ipv6_both_ways() {
    // ::1 host listener -> IPv4 guest.
    let mut cfg = UserNetConfig::default();
    cfg.forwards = vec![PortForward::new(
        Proto::Udp,
        Ipv6Addr::LOCALHOST.into(),
        0,
        5000,
    )];
    let r = start(cfg.clone());
    let host = UdpSocket::bind("[::1]:0").await.unwrap();
    host.send_to(b"v6 to v4", r.fwd[0]).await.unwrap();
    let (src, dst, sport, dport, p) = r.guest.recv_where("forwarded udp", as_udp).await;
    assert_eq!(
        (src, dst, dport, p.as_slice()),
        (cfg.gateway_ip, cfg.guest_ip, 5000, &b"v6 to v4"[..])
    );
    r.guest.send_udp(cfg.gateway_ip, 5000, sport, b"back").await;
    let mut buf = [0u8; 16];
    let (n, from) = timeout(Duration::from_secs(5), host.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((&buf[..n], from), (&b"back"[..], r.fwd[0]));

    // IPv4 host listener -> IPv6 guest (explicit address), reply over IPv6.
    let mut cfg = cfg6();
    let mut s = cfg.ipv6_prefix.segments();
    s[7] = 0x1234;
    let guest6 = Ipv6Addr::from(s);
    let mut f = PortForward::new(Proto::Udp, Ipv4Addr::LOCALHOST.into(), 0, 5001);
    f.guest_family = GuestFamily::V6;
    f.guest_addr = Some(guest6.into());
    cfg.forwards = vec![f];
    let r = start(cfg.clone());
    let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    host.send_to(b"v4 to v6", r.fwd[0]).await.unwrap();
    let (src, dst, sport, dport, p) = r.guest.recv_where("forwarded udp6", as_udp6).await;
    assert_eq!(
        (src, dst, dport, p.as_slice()),
        (cfg.gateway_ip6(), guest6, 5001, &b"v4 to v6"[..])
    );
    r.guest
        .send_udp(cfg.gateway_ip6(), 5001, sport, b"back6")
        .await;
    let (n, from) = timeout(Duration::from_secs(5), host.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((&buf[..n], from), (&b"back6"[..], r.fwd[0]));
}

#[test]
fn deny_gate_rate_limits_per_tuple_and_counts_suppressed() {
    let sa = |s: &str| s.parse::<SocketAddr>().unwrap();
    let k1 = (Proto::Udp, sa("10.0.2.15:4000"), sa("10.1.1.1:53"));
    let k2 = (Proto::Udp, sa("10.0.2.15:4001"), sa("10.1.1.1:53"));
    let mut seen = HashMap::new();
    let t0 = Instant::now();
    assert_eq!(deny_gate(&mut seen, k1, t0), Some(0));
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(10)),
        None
    );
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(999)),
        None
    );
    // A different tuple is independent.
    assert_eq!(
        deny_gate(&mut seen, k2, t0 + Duration::from_millis(20)),
        Some(0)
    );
    // After the interval the next event carries the count.
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(1000)),
        Some(2)
    );
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(1500)),
        None
    );
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(2100)),
        Some(1)
    );
    assert_eq!(
        deny_gate(&mut seen, k1, t0 + Duration::from_millis(3200)),
        Some(0)
    );
    // The table stays bounded.
    for i in 0..(MAX_DENY_KEYS as u16 + 50) {
        let k = (
            Proto::Tcp,
            sa("10.0.2.15:1"),
            SocketAddr::new(Ipv4Addr::new(9, 9, 9, 9).into(), i),
        );
        deny_gate(&mut seen, k, t0 + Duration::from_secs(10));
    }
    assert!(seen.len() <= MAX_DENY_KEYS);
}

#[tokio::test]
async fn repeated_denials_are_rate_limited_but_every_frame_is_still_denied() {
    let mut r = start(UserNetConfig::default()); // 10.0.0.0/8 is private: denied
    let g = &r.guest;
    for _ in 0..20 {
        g.send_udp(Ipv4Addr::new(10, 9, 9, 9), 4100, 9, b"x").await;
    }
    let first = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(
        (first.suppressed, first.reason.as_deref()),
        (0, Some("private-range"))
    );
    // No second event inside the interval.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        r.log.try_recv().is_err(),
        "burst must not produce more events"
    );
    // After the interval the next denial reports how many were swallowed.
    tokio::time::sleep(DENY_EVENT_INTERVAL).await;
    g.send_udp(Ipv4Addr::new(10, 9, 9, 9), 4100, 9, b"x").await;
    let next = next_event(&mut r.log, FlowKind::Deny).await;
    assert_eq!(next.suppressed, 19);
    let json = serde_json::to_string(&next).unwrap();
    assert!(json.contains("\"suppressed\":19"));
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("suppressed")
    );
    // A different 5-tuple is reported immediately.
    g.send_udp(Ipv4Addr::new(10, 9, 9, 9), 4101, 9, b"x").await;
    assert_eq!(
        next_event(&mut r.log, FlowKind::Deny).await.guest.port(),
        4101
    );
}

#[tokio::test]
async fn forward_status_file_reports_ephemeral_ports() {
    let mut cfg = UserNetConfig::default();
    cfg.forwards = vec![
        PortForward::new(Proto::Tcp, Ipv4Addr::LOCALHOST.into(), 0, 22),
        PortForward::new(Proto::Udp, Ipv6Addr::LOCALHOST.into(), 0, 53),
    ];
    let (a, _b) = StdUnixDatagram::pair().unwrap();
    let un = UserNet::new(cfg, a).unwrap();
    let dir = std::env::temp_dir().join(format!("fluxvm-usernet-fwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("usernet-forwards.json");
    un.write_forward_status(&path).unwrap();
    let f: ForwardStatusFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(f.pid, std::process::id());
    assert_eq!(f.forwards.len(), 2);
    assert_eq!(
        (
            f.forwards[0].index,
            f.forwards[0].proto,
            f.forwards[0].guest_port
        ),
        (0, Proto::Tcp, 22)
    );
    assert!(f.forwards[0].bound.ip().is_ipv4() && f.forwards[0].bound.port() != 0);
    assert!(f.forwards[1].bound.ip().is_ipv6() && f.forwards[1].bound.port() != 0);
    assert_eq!(f.forwards[0].bound, un.forward_addrs()[0]);
    // The ports it reports are really listening.
    TcpStream::connect(f.forwards[0].bound).await.unwrap();
    assert!(!dir.join("usernet-forwards.json.tmp").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
