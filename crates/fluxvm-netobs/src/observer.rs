// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The observer: one `ingest(frame, meta)` entry point that parses, tracks flows and health, extracts L7
//! metadata (redacted), feeds the roll-ups and the capture ring, and publishes events.
//!
//! State lives behind one mutex; ingestion is a single bounded pass per frame, and every collection it touches
//! has a hard cap (flows, events, DNS ring, closed-flow history, roll-up keys and windows, capture sessions,
//! reassembly bytes).
//!
//! Throughput: the lock is not the bottleneck at the rates this observer is fed. On the micro benchmark
//! (`cargo test -p fluxvm-netobs --release --test ingest_bench -- --ignored --nocapture`) one producer sustains
//! millions of frames per second; several producers contending per frame cost throughput, which
//! [`Observer::ingest_batch`] (one lock acquisition per batch, used by the bridge server) recovers. The flow
//! table is therefore not sharded; if a deployment ever measures otherwise, the table and its LRU are the part to
//! split by flow-key hash.

use crate::capture::{CaptureError, CaptureManager, CaptureRequest, CaptureStatus};
use crate::export::{
    CounterView, DnsRecord, DropRecord, EdnsView, Endpoint, EventKind, FlowView, HttpRecord,
    HttpsView, NetEvent, RedRow, SCHEMA_VERSION, Summary, TlsRecord, TlsServerRecord, VmSummary,
};
use crate::flow::{
    Direction, DropReason, Flow, FlowConfig, FlowKey, FlowL7, FlowTable, FrameMeta, Identity,
    Orientation, Verdict, orient_tcp, parse_proto, proto_name,
};
use crate::h2::H2Message;
use crate::health::{CloseReason, Dir, Health, HealthDelta, IcmpHealth, TcpHealth};
use crate::l7::{self, ClientHello, Http, Redaction, ServerHello};
use crate::packet::{
    self, IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_TCP, L4, Mac, Net, Packet, TCP_ACK, TCP_SYN, Tcp,
};
use crate::quic::QuicFlow;
use crate::reasm::{DropWhy, ReasmConfig};
use crate::rollup::{Red, Rollup, RollupConfig, RollupKey, Sample};
use crate::source::{BridgeLoss, TapStats, now_us};
use crate::streams::{FlowStreams, StreamOut};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

#[derive(Debug, Clone)]
pub struct ObserverConfig {
    pub flow: FlowConfig,
    pub rollup: RollupConfig,
    pub redaction: Redaction,
    /// Parse DNS/TLS/HTTP/QUIC metadata. Flow and health tracking works without it.
    pub l7: bool,
    pub event_ring: usize,
    pub dns_ring: usize,
    /// Closed flows kept for the history view.
    pub closed_flows: usize,
    pub closed_max_age_us: u64,
    pub capture_sessions: usize,
    /// Distinct VMs kept in the lifetime per-VM totals.
    pub max_vms: usize,
    /// Bounded TCP stream reassembly for TLS hellos and HTTP/2 frames.
    pub reasm: ReasmConfig,
    /// Host names (and their subdomains) of DNS-over-HTTPS resolvers, matched against the SNI of a hello. A match
    /// tags the flow `encrypted_dns = "doh"`. Defaults to [`l7::DEFAULT_DOH_HOSTS`].
    pub doh_hosts: Vec<String>,
    /// Decode QUIC v1/v2 client Initials (keys derive from public values) for SNI/ALPN/fingerprint.
    pub quic_initial: bool,
    /// Decode cleartext HTTP/2 (`h2c`) request and response heads.
    pub h2: bool,
    /// A switch-level tap sees each frame once, attributed to the guest that sent it. With this on (the default),
    /// an allowed unicast frame whose destination MAC belongs to another guest of the same network is also
    /// accounted in that guest's view as received, so both VMs of a guest-to-guest connection see both
    /// directions (handshake, byte counts both ways, FIN/RST from either side) with their own orientation.
    /// The mirrored copy does not count in the observer-wide frame and byte counters.
    pub mirror_peer_views: bool,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            flow: FlowConfig::default(),
            rollup: RollupConfig::default(),
            redaction: Redaction::default(),
            l7: true,
            event_ring: 4096,
            dns_ring: 2048,
            closed_flows: 4096,
            closed_max_age_us: 3_600_000_000,
            capture_sessions: 8,
            max_vms: 1024,
            reasm: ReasmConfig::default(),
            doh_hosts: l7::DEFAULT_DOH_HOSTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            quic_initial: true,
            h2: true,
            mirror_peer_views: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub frames: u64,
    pub bytes: u64,
    pub malformed: u64,
    pub truncated: u64,
    pub arp: u64,
    pub non_ip: u64,
    pub flows_created: u64,
    pub flows_evicted: u64,
    pub flows_expired: u64,
    pub drops: [u64; 7],
    /// Segments (or QUIC CRYPTO data) not kept because a reassembly bound was hit.
    pub reasm_segments_dropped: u64,
    /// Streams abandoned because the observer-wide reassembly budget was exhausted.
    pub reasm_streams_abandoned: u64,
    /// Streams that gave up with a message still incomplete after the per-stream prefix budget.
    pub reasm_incomplete: u64,
    /// Overlapping retransmissions whose bytes differed from the first copy.
    pub reasm_conflicts: u64,
    /// Flows tagged DoT/DoH/DoQ.
    pub encrypted_dns_flows: u64,
    /// QUIC ClientHellos recovered from Initial packets.
    pub quic_hellos: u64,
}

/// Filters for [`Observer::flows`].
#[derive(Debug, Clone, Default)]
pub struct FlowQuery {
    pub vm: Option<String>,
    pub proto: Option<String>,
    pub port: Option<u16>,
    pub host: Option<IpAddr>,
    /// "allowed", "denied" or "dropped".
    pub verdict: Option<String>,
    pub include_closed: bool,
    pub limit: usize,
    /// Restrict to these VMs (tenant scoping). `None` means all.
    pub allowed_vms: Option<HashSet<String>>,
}

impl FlowQuery {
    fn matches(&self, v: &FlowView) -> bool {
        if let Some(a) = &self.allowed_vms {
            if !a.contains(&v.vm_id) {
                return false;
            }
        }
        if self.vm.as_ref().is_some_and(|vm| *vm != v.vm_id) {
            return false;
        }
        if let Some(p) = &self.proto {
            if proto_name(parse_proto(p).unwrap_or(255)) != v.proto {
                return false;
            }
        }
        if let Some(port) = self.port {
            if v.client.port != port && v.server.port != port {
                return false;
            }
        }
        if let Some(h) = self.host {
            let h = h.to_string();
            if v.client.ip != h && v.server.ip != h {
                return false;
            }
        }
        if self.verdict.as_ref().is_some_and(|x| x != v.verdict) {
            return false;
        }
        true
    }
}

struct Closed {
    view: FlowView,
    at_us: u64,
}

struct Events {
    ring: VecDeque<NetEvent>,
    next_seq: u64,
    cap: usize,
}

impl Events {
    fn emit(&mut self, ts_us: u64, id: &Identity, kind: EventKind) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.ring.len() >= self.cap.max(1) {
            self.ring.pop_front();
        }
        self.ring.push_back(NetEvent {
            schema: SCHEMA_VERSION,
            seq,
            ts_us,
            vm_id: id.vm_id.to_string(),
            network: id.network.to_string(),
            nic: id.nic.to_string(),
            kind,
        });
    }
}

struct Inner {
    cfg: ObserverConfig,
    flows: FlowTable,
    rollup: Rollup,
    events: Events,
    dns: VecDeque<DnsRecord>,
    closed: VecDeque<Closed>,
    capture: CaptureManager,
    counters: Counters,
    vm_totals: HashMap<String, Red>,
    /// Guests seen sending, by (network, MAC): where a unicast frame's receiving view is found.
    peers: HashMap<(Arc<str>, Mac), Arc<Identity>>,
    /// The identity learned last, so a run of frames from one port skips the table.
    last_peer: Option<Arc<Identity>>,
    /// Bytes held by reassembly and QUIC hello buffers across all flows (see [`ReasmConfig::max_total_bytes`]).
    reasm_bytes: usize,
    clock_us: u64,
    last_tick_us: u64,
}

pub struct Observer {
    inner: Mutex<Inner>,
    tap_stats: Mutex<Option<Arc<TapStats>>>,
    bridges: Mutex<Vec<Arc<BridgeLoss>>>,
}

fn ep(a: (IpAddr, u16)) -> Endpoint {
    Endpoint {
        ip: a.0.to_string(),
        port: a.1,
    }
}

impl Inner {
    fn finish(&mut self, now: u64, flow: Flow, reason: CloseReason) {
        // Whatever the flow still buffered goes back to the reassembly budget.
        self.reasm_bytes = self.reasm_bytes.saturating_sub(flow_mem(&flow.l7));
        let view = FlowView::from_flow(&flow, Some(reason));
        if self.closed.len() >= self.cfg.closed_flows.max(1) {
            self.closed.pop_front();
        }
        self.closed.push_back(Closed {
            view: view.clone(),
            at_us: now,
        });
        self.events.emit(
            now,
            &flow.identity,
            EventKind::FlowClosed {
                reason,
                flow: Box::new(view),
            },
        );
    }

    fn tick(&mut self, now: u64) {
        self.clock_us = self.clock_us.max(now);
        let now = self.clock_us;
        if now.saturating_sub(self.last_tick_us) < 1_000_000 && self.last_tick_us != 0 {
            return;
        }
        self.last_tick_us = now;
        for (flow, reason) in self.flows.expire(now) {
            self.counters.flows_expired += 1;
            self.finish(now, flow, reason);
        }
        let mut snaps = Vec::new();
        self.flows
            .due_active(now, |f| snaps.push(FlowView::from_flow(f, None)));
        for v in snaps {
            let id = Identity {
                vm_id: v.vm_id.as_str().into(),
                network: v.network.as_str().into(),
                nic: v.nic.as_str().into(),
                mac: [0; 6],
            };
            self.events
                .emit(now, &id, EventKind::FlowUpdate { flow: Box::new(v) });
        }
        self.rollup.advance(now);
        let max_age = self.cfg.closed_max_age_us;
        while self
            .closed
            .front()
            .is_some_and(|c| now.saturating_sub(c.at_us) > max_age)
        {
            self.closed.pop_front();
        }
    }

    /// Ingests one observed frame. A frame a guest sent to another guest on the same private network is also
    /// accounted in the receiving guest's own view (see [`ObserverConfig::mirror_peer_views`]).
    fn ingest(&mut self, frame: &[u8], meta: &FrameMeta) {
        if meta.direction == Direction::FromGuest && self.cfg.mirror_peer_views {
            // Learn which guest owns which MAC on which network. The identity's MAC is the port's MAC, which
            // the switch enforces as the frame's source.
            let known = self
                .last_peer
                .as_ref()
                .is_some_and(|p| Arc::ptr_eq(p, &meta.identity));
            if !known {
                let k = (meta.identity.network.clone(), meta.identity.mac);
                if self.peers.len() < self.cfg.max_vms.max(1) || self.peers.contains_key(&k) {
                    self.peers.insert(k, meta.identity.clone());
                }
                self.last_peer = Some(meta.identity.clone());
            }
        }
        self.ingest_view(frame, meta, false);
        if self.cfg.mirror_peer_views
            && meta.direction == Direction::FromGuest
            && meta.verdict.is_allowed()
            && frame.len() >= 6
            && frame[0] & 1 == 0
        {
            let dst: Mac = [frame[0], frame[1], frame[2], frame[3], frame[4], frame[5]];
            if let Some(peer) = self
                .peers
                .get(&(meta.identity.network.clone(), dst))
                .filter(|p| p.vm_id != meta.identity.vm_id)
                .cloned()
            {
                let m = FrameMeta {
                    identity: peer,
                    ts_us: meta.ts_us,
                    direction: Direction::ToGuest,
                    verdict: meta.verdict,
                };
                self.ingest_view(frame, &m, true);
            }
        }
    }

    /// Accounts one frame in the view of `meta.identity`. `mirrored` marks the receiving guest's copy of a frame
    /// that was already counted once as it was sent: it feeds that guest's flows, roll-ups and captures but not
    /// the observer-wide frame, byte, drop and malformed counters.
    fn ingest_view(&mut self, frame: &[u8], meta: &FrameMeta, mirrored: bool) {
        let now = meta.ts_us;
        if !mirrored {
            self.counters.frames += 1;
            self.counters.bytes += frame.len() as u64;
            if let Some(r) = meta.verdict.reason() {
                self.counters.drops[r.index()] += 1;
            }
        }
        self.tick(now);

        let Some(pkt) = packet::parse(frame) else {
            if !mirrored {
                self.counters.malformed += 1;
            }
            return;
        };
        if pkt.truncated && !mirrored {
            self.counters.truncated += 1;
        }
        if self.capture.has_active() {
            self.capture.offer(&meta.identity.vm_id, now, frame, &pkt);
        }
        let ip = match &pkt.net {
            Net::Ip(ip) => ip.clone(),
            Net::Arp(_) => {
                if !mirrored {
                    self.counters.arp += 1;
                }
                return;
            }
            Net::None => {
                if !mirrored {
                    self.counters.non_ip += 1;
                }
                return;
            }
        };
        let (sport, dport) = pkt.ports();
        let src = (ip.src, sport);
        let dst = (ip.dst, dport);
        let key = FlowKey::new(&meta.identity, ip.proto, src, dst);

        // A fresh SYN on a flow that already closed starts a new flow instead of extending the old one.
        if let (Some(f), L4::Tcp(t)) = (self.flows.get(&key), &pkt.l4) {
            if f.closed.is_some() && t.has(TCP_SYN) && !t.has(TCP_ACK) {
                if let Some(old) = self.flows.remove(&key) {
                    let reason = old.closed.unwrap_or(CloseReason::Fin);
                    self.finish(now, old, reason);
                }
            }
        }

        let mut evicted = Vec::new();
        let identity = meta.identity.clone();
        let from_guest = meta.direction == Direction::FromGuest;
        let vlan = pkt.vlan;
        let proto = ip.proto;
        // Who is the client is decided by the segment that tells (SYN, SYN+ACK), not by who spoke first.
        let (client, server, oriented) = match &pkt.l4 {
            L4::Tcp(t) => orient_tcp(t.has(TCP_SYN), t.has(TCP_ACK), src, dst),
            _ => (src, dst, Orientation::FirstPacket),
        };
        let guest_is_client = if client == src {
            from_guest
        } else {
            !from_guest
        };
        let (flow, created) = self.flows.get_or_insert(
            &key,
            |id| Flow {
                id,
                identity,
                proto,
                client,
                server,
                oriented,
                guest_is_client,
                vlan,
                first_seen_us: now,
                last_seen_us: now,
                last_export_us: now,
                packets: [0; 2],
                bytes: [0; 2],
                verdict: Verdict::Allowed,
                dropped_packets: 0,
                health: match proto {
                    IPPROTO_TCP => Health::Tcp(Box::new(TcpHealth::new())),
                    IPPROTO_ICMP | IPPROTO_ICMPV6 => Health::Icmp(IcmpHealth::default()),
                    _ => Health::None,
                },
                l7: FlowL7::default(),
                closed: None,
                closed_at_us: 0,
            },
            &mut evicted,
        );
        let dir = if flow.client == src {
            Dir::ClientToServer
        } else {
            Dir::ServerToClient
        };
        flow.last_seen_us = flow.last_seen_us.max(now);
        flow.packets[dir as usize] += 1;
        flow.bytes[dir as usize] += frame.len() as u64;

        let mut samples: Vec<Sample> = Vec::with_capacity(4);
        let mut events: Vec<EventKind> = Vec::new();
        if created {
            samples.push(Sample::FlowOpened);
            events.push(EventKind::FlowOpened {
                flow_id: flow.id,
                proto: proto_name(flow.proto),
                client: ep(flow.client),
                server: ep(flow.server),
                direction: if flow.guest_is_client {
                    "outbound"
                } else {
                    "inbound"
                },
                oriented: flow.oriented.name(),
            });
        }
        samples.push(Sample::Packet {
            bytes: frame.len() as u64,
        });

        if !meta.verdict.is_allowed() {
            flow.dropped_packets += 1;
            samples.push(Sample::Drop);
            if flow.dropped_packets == 1 || flow.verdict.is_allowed() {
                events.push(EventKind::Drop(DropRecord {
                    flow_id: flow.id,
                    proto: proto_name(flow.proto),
                    src: ep(src),
                    dst: ep(dst),
                    verdict: meta.verdict.kind(),
                    reason: meta
                        .verdict
                        .reason()
                        .map(DropReason::name)
                        .unwrap_or("other"),
                }));
            }
            flow.verdict = meta.verdict;
        }

        // Health.
        let mut delta = HealthDelta::default();
        match (&mut flow.health, &pkt.l4) {
            (Health::Tcp(h), L4::Tcp(t)) => delta = h.on_segment(dir, now, t),
            (Health::Icmp(h), L4::Icmp(m)) => delta.rtt_us = h.on_message(now, m),
            _ => {}
        }
        if delta.retransmit {
            samples.push(Sample::Retransmit);
        }
        if delta.zero_window {
            samples.push(Sample::ZeroWindow);
        }
        if delta.reset {
            samples.push(Sample::Reset);
        }
        if let Some(r) = delta.rtt_us {
            samples.push(Sample::Rtt(r));
        } else if let Some(r) = delta.handshake_rtt_us {
            samples.push(Sample::Rtt(r));
        }
        if let Some(reason) = delta.closed {
            if flow.closed.is_none() {
                flow.closed = Some(reason);
                flow.closed_at_us = now;
            }
        }

        // L7 metadata.
        if self.cfg.l7 {
            if let L4::Tcp(t) = &pkt.l4 {
                if t.has(TCP_SYN) {
                    flow.l7.isn[dir as usize] = Some(t.seq);
                }
                if created && (t.sport == 853 || t.dport == 853) {
                    // DNS-over-TLS (RFC 7858) lives on a dedicated port.
                    flow.l7.encrypted_dns = Some("dot");
                    self.counters.encrypted_dns_flows += 1;
                }
            }
            if !pkt.payload.is_empty() {
                let mut ctx = L7Ctx {
                    red: &self.cfg.redaction,
                    doh_hosts: &self.cfg.doh_hosts,
                    reasm: &self.cfg.reasm,
                    quic: self.cfg.quic_initial,
                    h2: self.cfg.h2,
                    counters: &mut self.counters,
                    reasm_bytes: &mut self.reasm_bytes,
                    dns_ring: &mut self.dns,
                };
                // Retransmissions are not re-parsed, except that reassembly must still see them: health calls
                // reordered segments "retransmits" too, and a late segment may be the one that fills a gap.
                inspect_l7(
                    flow,
                    now,
                    &pkt,
                    dir,
                    delta.retransmit,
                    &mut ctx,
                    &mut samples,
                    &mut events,
                );
            }
        }

        let ctx = RollupCtx::of(flow);
        let id = ctx.identity.clone();
        for s in samples {
            self.record_ctx(now, &ctx, s);
        }
        for e in events {
            self.events.emit(now, &id, e);
        }
        for old in evicted {
            self.counters.flows_evicted += 1;
            self.finish(now, old, CloseReason::Evicted);
        }
        if created {
            self.counters.flows_created += 1;
        }
        while self.dns.len() > self.cfg.dns_ring.max(1) {
            self.dns.pop_front();
        }
    }
}

/// What L7 inspection needs from the observer besides the flow, borrowed field by field so the flow-table borrow
/// can stay alive.
struct L7Ctx<'a> {
    red: &'a Redaction,
    doh_hosts: &'a [String],
    reasm: &'a ReasmConfig,
    quic: bool,
    h2: bool,
    counters: &'a mut Counters,
    reasm_bytes: &'a mut usize,
    dns_ring: &'a mut VecDeque<DnsRecord>,
}

/// Heap held by a flow's reassembly and QUIC state (what counts against the observer-wide budget).
fn flow_mem(l7: &FlowL7) -> usize {
    l7.streams.as_ref().map_or(0, |s| s.mem()) + l7.quic.as_ref().map_or(0, |q| q.mem())
}

fn track_mem(ctx: &mut L7Ctx<'_>, before: usize, after: usize) {
    if after >= before {
        *ctx.reasm_bytes += after - before;
    } else {
        *ctx.reasm_bytes = ctx.reasm_bytes.saturating_sub(before - after);
    }
}

fn method_static(m: &str) -> &'static str {
    [
        "GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "CONNECT", "TRACE",
    ]
    .into_iter()
    .find(|k| *k == m)
    .unwrap_or("OTHER")
}

/// Records a ClientHello (from TCP or QUIC). Returns the TLS record to publish, or `None` when this flow's hello
/// was already recorded. The raw SNI is used for encrypted-DNS recognition before redaction; only the redacted
/// form is stored.
fn apply_client_hello(
    flow: &mut Flow,
    h: &ClientHello,
    transport: char,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
) -> Option<TlsRecord> {
    if flow.l7.ja3.is_some() || flow.l7.sni.is_some() {
        return None;
    }
    let red = ctx.red;
    flow.l7.app = Some(if transport == 'q' { "quic" } else { "tls" });
    if let Some(raw) = &h.sni {
        if flow.l7.encrypted_dns.is_none() && l7::sni_is_doh(raw, ctx.doh_hosts) {
            flow.l7.encrypted_dns = Some("doh");
            ctx.counters.encrypted_dns_flows += 1;
        }
    }
    flow.l7.sni = h.sni.as_deref().and_then(|s| red.apply(red.tls_sni, s));
    if red.fingerprints {
        flow.l7.ja3 = Some(h.ja3());
        flow.l7.ja4 = Some(h.ja4_for(transport));
    } else {
        // Mark the hello as seen so it is not parsed again for every segment.
        flow.l7.ja3 = Some(String::new());
    }
    flow.l7.alpn = h
        .alpn
        .first()
        .and_then(|a| std::str::from_utf8(a).ok())
        .map(str::to_string);
    samples.push(Sample::TlsHello);
    Some(TlsRecord {
        flow_id: flow.id,
        server: ep(flow.server),
        sni: flow.l7.sni.clone(),
        ja3: flow.l7.ja3.clone().filter(|s| !s.is_empty()),
        ja4: flow.l7.ja4.clone(),
        alpn: flow.l7.alpn.clone(),
    })
}

fn apply_server_hello(
    flow: &mut Flow,
    h: &ServerHello,
    ctx: &mut L7Ctx<'_>,
) -> Option<TlsServerRecord> {
    if flow.l7.tls_version.is_some() {
        return None;
    }
    if flow.l7.app.is_none() {
        flow.l7.app = Some("tls");
    }
    flow.l7.tls_version = Some(l7::tls_version_name(h.negotiated_version()));
    flow.l7.tls_cipher = Some(l7::tls_cipher_name(h.cipher));
    if ctx.red.fingerprints {
        flow.l7.ja3s = Some(h.ja3s());
        flow.l7.ja4s = Some(h.ja4s());
    }
    Some(TlsServerRecord {
        flow_id: flow.id,
        server: ep(flow.server),
        version: flow.l7.tls_version.clone().unwrap_or_default(),
        cipher: flow.l7.tls_cipher.clone().unwrap_or_default(),
        hello_retry: h.hello_retry,
        ja3s: flow.l7.ja3s.clone(),
        ja4s: flow.l7.ja4s.clone(),
    })
}

fn apply_h2(
    flow: &mut Flow,
    m: &H2Message,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
) {
    flow.l7.app = Some("h2");
    let h = &m.headers;
    let red = ctx.red;
    if m.from_client {
        let Some(method) = h.method.as_deref() else {
            return;
        };
        flow.l7.http_requests += 1;
        let host = h
            .authority
            .as_deref()
            .and_then(|a| red.apply(red.http_host, a));
        if host.is_some() {
            flow.l7.http_host = host.clone();
        }
        samples.push(Sample::HttpRequest);
        events.push(EventKind::Http(HttpRecord {
            flow_id: flow.id,
            server: ep(flow.server),
            request: true,
            method: Some(method_static(method)),
            host,
            status: None,
            version: "2".to_string(),
            scheme: h.scheme.clone(),
            content_type: None,
        }));
    } else if let Some(status) = h.status {
        if status < 200 {
            return; // interim (1xx) responses are not the answer
        }
        flow.l7.http_responses += 1;
        samples.push(Sample::HttpResponse { status });
        events.push(EventKind::Http(HttpRecord {
            flow_id: flow.id,
            server: ep(flow.server),
            request: false,
            method: None,
            host: None,
            status: Some(status),
            version: "2".to_string(),
            scheme: None,
            content_type: h.content_type.clone(),
        }));
    }
}

fn apply_stream_out(
    flow: &mut Flow,
    out: Vec<StreamOut>,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
) {
    for o in out {
        match o {
            StreamOut::ClientHello(h) => {
                if let Some(r) = apply_client_hello(flow, &h, 't', ctx, samples) {
                    events.push(EventKind::Tls(r));
                }
            }
            StreamOut::ServerHello(h) => {
                if let Some(r) = apply_server_hello(flow, &h, ctx) {
                    events.push(EventKind::TlsServer(r));
                }
            }
            StreamOut::H2(m) => apply_h2(flow, &m, ctx, samples, events),
        }
    }
}

/// Parsing of L7 metadata for one packet; updates the flow's state and appends samples/events.
#[allow(clippy::too_many_arguments)]
fn inspect_l7(
    flow: &mut Flow,
    now: u64,
    pkt: &Packet<'_>,
    dir: Dir,
    retransmit: bool,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
) {
    let payload = pkt.payload;
    let (sport, dport) = pkt.ports();
    let dns = if retransmit {
        None
    } else {
        match &pkt.l4 {
            L4::Udp(_) if sport == 53 || dport == 53 => l7::parse_dns(payload).map(|m| (m, false)),
            L4::Tcp(_) if sport == 53 || dport == 53 => {
                l7::parse_dns_tcp(payload).map(|m| (m, true))
            }
            _ => None,
        }
    };
    if let Some((m, over_tcp)) = dns {
        let red = ctx.red;
        flow.l7.app = Some("dns");
        let qname = red.apply(red.dns_qname, &m.qname);
        flow.l7.dns_last_qname = qname.clone();
        let rcode = m.full_rcode();
        let mut latency = None;
        if m.response {
            flow.l7.dns_responses += 1;
            if let Some(i) = flow.l7.dns_pending.iter().position(|(id, _)| *id == m.id) {
                let (_, sent) = flow.l7.dns_pending.remove(i);
                latency = Some(now.saturating_sub(sent));
            }
            samples.push(Sample::DnsResponse {
                error: rcode != 0,
                latency_us: latency,
            });
        } else {
            flow.l7.dns_queries += 1;
            if flow.l7.dns_pending.len() >= 16 {
                flow.l7.dns_pending.remove(0);
            }
            flow.l7.dns_pending.push((m.id, now));
            samples.push(Sample::DnsQuery);
        }
        let keep = red.dns_answers;
        let name_of = |n: &str| {
            if n == "." {
                Some(".".to_string())
            } else {
                red.apply(red.dns_qname, n)
            }
        };
        let rec = DnsRecord {
            ts_us: now,
            vm_id: flow.identity.vm_id.to_string(),
            flow_id: flow.id,
            client: ep(flow.client),
            server: ep(flow.server),
            tcp: over_tcp,
            id: m.id,
            response: m.response,
            qname,
            qtype: l7::dns_type_name(m.qtype),
            rcode: m.response.then(|| l7::dns_rcode_name_full(rcode)),
            latency_us: latency,
            answers: if keep {
                m.addrs.iter().map(|a| a.to_string()).collect()
            } else {
                Vec::new()
            },
            answer_types: m
                .answer_types
                .iter()
                .map(|t| l7::dns_type_name(*t))
                .collect(),
            cnames: if keep {
                m.cnames.iter().filter_map(|n| name_of(n)).collect()
            } else {
                Vec::new()
            },
            https: if keep {
                m.https
                    .iter()
                    .map(|h| HttpsView {
                        priority: h.priority,
                        target: name_of(&h.target),
                        alpn: h.alpn.clone(),
                        port: h.port,
                        ech: h.ech,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            edns: m.edns.as_ref().map(|e| EdnsView {
                udp_size: e.udp_size,
                version: e.version,
                dnssec_ok: e.dnssec_ok,
            }),
        };
        ctx.dns_ring.push_back(rec.clone());
        events.push(EventKind::Dns(rec));
        return;
    }

    match &pkt.l4 {
        L4::Udp(_) => inspect_quic(flow, payload, dir, ctx, samples, events),
        L4::Tcp(t) => inspect_tcp(flow, t, payload, dir, retransmit, ctx, samples, events),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn inspect_quic(
    flow: &mut Flow,
    payload: &[u8],
    dir: Dir,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
) {
    if payload.len() < 1200 || payload[0] & 0xc0 != 0xc0 {
        return;
    }
    let Some(q) = l7::detect_quic_initial(payload) else {
        return;
    };
    let first = flow.l7.quic_version.is_none();
    if first {
        flow.l7.app = Some("quic");
        flow.l7.quic_version = Some(q.version);
        if flow.server.1 == 853 && flow.l7.encrypted_dns.is_none() {
            flow.l7.encrypted_dns = Some("doq");
            ctx.counters.encrypted_dns_flows += 1;
        }
    }
    let mut hello = None;
    if ctx.quic && dir == Dir::ClientToServer && flow.l7.ja3.is_none() && flow.l7.sni.is_none() {
        let room = ctx.reasm.max_total_bytes.saturating_sub(*ctx.reasm_bytes);
        let before = flow_mem(&flow.l7);
        let qf = flow
            .l7
            .quic
            .get_or_insert_with(|| Box::new(QuicFlow::new()));
        if !qf.is_finished() {
            let (h, dropped) = qf.on_datagram(payload, room);
            if dropped {
                ctx.counters.reasm_segments_dropped += 1;
            }
            hello = h;
        }
        let after = flow_mem(&flow.l7);
        track_mem(ctx, before, after);
    }
    let rec = hello.and_then(|h| apply_client_hello(flow, &h, 'q', ctx, samples));
    if rec.is_some() {
        ctx.counters.quic_hellos += 1;
    }
    if first || rec.is_some() {
        events.push(EventKind::Quic {
            flow_id: flow.id,
            server: ep(flow.server),
            version: q.version,
            dcid_len: q.dcid_len,
            scid_len: Some(q.scid_len),
            sni: rec.as_ref().and_then(|r| r.sni.clone()),
            alpn: rec.as_ref().and_then(|r| r.alpn.clone()),
            ja4: rec.as_ref().and_then(|r| r.ja4.clone()),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn inspect_tcp(
    flow: &mut Flow,
    t: &Tcp,
    payload: &[u8],
    dir: Dir,
    retransmit: bool,
    ctx: &mut L7Ctx<'_>,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
) {
    let d = dir as usize;
    // TLS handshake messages and cleartext HTTP/2 frames, via reassembly of the first bytes of the stream.
    if !flow.l7.streams_done && !flow.l7.dir_done[d] {
        let mut out: Vec<StreamOut> = Vec::new();
        let looks_tls = payload.len() >= 2 && payload[0] == 0x16 && payload[1] == 3;
        let isn = flow.l7.isn[d];
        let ooo_first = !flow.l7.data_seen[d] && isn.is_some_and(|i| t.seq != i.wrapping_add(1));
        // Cleartext HTTP/2 starts with the client preface (possibly cut short by a small first segment).
        let looks_h2 = ctx.h2
            && dir == Dir::ClientToServer
            && (crate::h2::has_preface(payload) || crate::h2::is_preface_prefix(payload));
        if flow.l7.streams.is_none() {
            let mut start_stream = false;
            if looks_tls && !ooo_first && !retransmit {
                // The common case: the whole hello is in this segment. No buffering at all.
                match l7::handshake_message(payload) {
                    l7::Hs::Complete(ty, body) => {
                        match (dir, ty) {
                            (Dir::ClientToServer, 1) => {
                                if let Some(h) = l7::parse_client_hello_body(&body) {
                                    out.push(StreamOut::ClientHello(Box::new(h)));
                                }
                            }
                            (Dir::ServerToClient, 2) => {
                                if let Some(h) = l7::parse_server_hello_body(&body) {
                                    out.push(StreamOut::ServerHello(Box::new(h)));
                                }
                            }
                            _ => {}
                        }
                        flow.l7.dir_done[d] = true;
                    }
                    l7::Hs::Need => start_stream = ctx.reasm.enabled,
                    l7::Hs::Not => flow.l7.dir_done[d] = true,
                }
                if !start_stream && !flow.l7.dir_done[d] {
                    flow.l7.dir_done[d] = true;
                }
            } else if ctx.reasm.enabled && (looks_tls || ooo_first || looks_h2) {
                start_stream = true;
            } else if !looks_h2 {
                flow.l7.dir_done[d] = true;
            }
            if start_stream {
                flow.l7.streams = Some(Box::new(FlowStreams::new()));
            }
        }
        if let Some(streams) = flow.l7.streams.as_mut() {
            let before = streams.mem();
            let room = ctx.reasm.max_total_bytes.saturating_sub(*ctx.reasm_bytes);
            let st = streams.feed(d, t.seq, payload, isn, ctx.reasm, room, &mut out);
            let after = streams.mem();
            // A stream abandoned for lack of budget takes the whole connection's inspection with it: keeping
            // one direction's state alive would only pin memory the budget has said is not there.
            let all_done = streams.all_done() || st.dropped == Some(DropWhy::Budget);
            match st.dropped {
                Some(DropWhy::PendingFull) => ctx.counters.reasm_segments_dropped += 1,
                Some(DropWhy::Budget) => {
                    ctx.counters.reasm_segments_dropped += 1;
                    ctx.counters.reasm_streams_abandoned += 1;
                }
                None => {}
            }
            ctx.counters.reasm_conflicts += u64::from(st.conflicts);
            if st.gave_up {
                ctx.counters.reasm_incomplete += 1;
            }
            if all_done {
                flow.l7.streams = None;
                flow.l7.streams_done = true;
            }
            track_mem(ctx, before, if all_done { 0 } else { after });
        }
        apply_stream_out(flow, out, ctx, samples, events);
    }
    flow.l7.data_seen[d] = true;

    // HTTP/1.x: only the first segment's request line and Host header.
    if retransmit || payload.is_empty() || !payload[0].is_ascii_uppercase() {
        return;
    }
    if let Some(h) = l7::parse_http(payload) {
        let red = ctx.red;
        flow.l7.app = Some("http");
        match h {
            Http::Request {
                method,
                host,
                version,
            } => {
                flow.l7.http_requests += 1;
                let host = host.and_then(|h| red.apply(red.http_host, &h));
                if host.is_some() {
                    flow.l7.http_host = host.clone();
                }
                samples.push(Sample::HttpRequest);
                events.push(EventKind::Http(HttpRecord {
                    flow_id: flow.id,
                    server: ep(flow.server),
                    request: true,
                    method: Some(method),
                    host,
                    status: None,
                    version,
                    scheme: None,
                    content_type: None,
                }));
            }
            Http::Response { status, version } => {
                flow.l7.http_responses += 1;
                samples.push(Sample::HttpResponse { status });
                events.push(EventKind::Http(HttpRecord {
                    flow_id: flow.id,
                    server: ep(flow.server),
                    request: false,
                    method: None,
                    host: None,
                    status: Some(status),
                    version,
                    scheme: None,
                    content_type: None,
                }));
            }
        }
    }
}

/// The few flow fields roll-up keys need, copied out so the flow borrow can end.
struct RollupCtx {
    identity: Arc<Identity>,
    guest_is_client: bool,
    client: IpAddr,
    server: (IpAddr, u16),
    proto: u8,
}

impl RollupCtx {
    fn of(f: &Flow) -> Self {
        Self {
            identity: f.identity.clone(),
            guest_is_client: f.guest_is_client,
            client: f.client.0,
            server: f.server,
            proto: f.proto,
        }
    }
}

impl RollupCtx {
    fn key(&self) -> RollupKey {
        let (peer, outbound) = if self.guest_is_client {
            (self.server.0, true)
        } else {
            (self.client, false)
        };
        RollupKey {
            vm: self.identity.vm_id.to_string(),
            peer,
            port: self.server.1,
            proto: self.proto,
            outbound,
        }
    }
}

impl Inner {
    fn record_ctx(&mut self, now: u64, c: &RollupCtx, s: Sample) {
        let vm = c.identity.vm_id.as_ref();
        if self.vm_totals.contains_key(vm) || self.vm_totals.len() < self.cfg.max_vms {
            self.vm_totals.entry(vm.to_string()).or_default().apply(s);
        }
        self.rollup.record(now, c.key(), s);
    }
}

impl Observer {
    pub fn new(cfg: ObserverConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                flows: FlowTable::new(cfg.flow.clone()),
                rollup: Rollup::new(cfg.rollup.clone()),
                events: Events {
                    ring: VecDeque::new(),
                    next_seq: 1,
                    cap: cfg.event_ring,
                },
                dns: VecDeque::new(),
                closed: VecDeque::new(),
                capture: CaptureManager::new(cfg.capture_sessions),
                counters: Counters::default(),
                vm_totals: HashMap::new(),
                peers: HashMap::new(),
                last_peer: None,
                reasm_bytes: 0,
                clock_us: 0,
                last_tick_us: 0,
                cfg,
            }),
            tap_stats: Mutex::new(None),
            bridges: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock must not take observation (or the API) down with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The single entry point. Never panics on any input bytes.
    pub fn ingest(&self, frame: &[u8], meta: &FrameMeta) {
        self.lock().ingest(frame, meta);
    }

    /// Ingests several frames under one lock acquisition. Semantically the same as calling [`Self::ingest`] for
    /// each in order, but it keeps a multi-producer feed (a bridge with many connections) from bouncing the
    /// mutex once per frame.
    pub fn ingest_batch<'a, I>(&self, items: I)
    where
        I: IntoIterator<Item = (&'a [u8], &'a FrameMeta)>,
    {
        let mut g = self.lock();
        for (frame, meta) in items {
            g.ingest(frame, meta);
        }
    }

    /// Ages flows and rotates windows. Call periodically when traffic may be idle (the tap pump does).
    pub fn tick(&self, now_us: u64) {
        self.lock().tick(now_us);
    }

    pub fn set_tap_stats(&self, s: Arc<TapStats>) {
        *self.tap_stats.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }

    /// Like [`Self::set_tap_stats`] but leaves an existing registration alone. Returns whether `s` was installed.
    pub fn set_tap_stats_if_unset(&self, s: Arc<TapStats>) -> bool {
        let mut g = self.tap_stats.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some() {
            return false;
        }
        *g = Some(s);
        true
    }

    /// Registers a bridge's loss accounting. Unlike the single in-process tap slot, any number of bridges can
    /// be registered, and registering never displaces the tap: the summary adds them up. Registering the same
    /// `Arc` twice is a no-op.
    pub fn add_bridge_loss(&self, b: Arc<BridgeLoss>) {
        let mut g = self.bridges.lock().unwrap_or_else(|e| e.into_inner());
        if !g.iter().any(|x| Arc::ptr_eq(x, &b)) {
            g.push(b);
        }
    }

    pub fn counters(&self) -> Counters {
        self.lock().counters.clone()
    }

    pub fn summary(&self, now_us: u64) -> Summary {
        let tap = self
            .tap_stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let bridges: Vec<Arc<BridgeLoss>> = self
            .bridges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut bridge_by_reason: Vec<(String, u64)> = crate::source::BRIDGE_DROP_REASONS
            .iter()
            .map(|r| (r.to_string(), 0))
            .collect();
        let (mut bridge_frames, mut bridge_dropped, mut bridge_trunc, mut fwd, mut unconf) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        for b in &bridges {
            bridge_frames += b.frames.load(Ordering::Relaxed);
            for (i, (_, n)) in b.dropped_by_reason().into_iter().enumerate() {
                bridge_by_reason[i].1 += n;
                bridge_dropped += n;
            }
            bridge_trunc += b.truncated_to_snaplen.load(Ordering::Relaxed);
            fwd += b.switch_forward_dropped.load(Ordering::Relaxed);
            unconf += b.unconfirmed_total();
        }
        let mut g = self.lock();
        g.tick(now_us);
        let mut active: HashMap<String, u64> = HashMap::new();
        for f in g.flows.iter() {
            *active.entry(f.identity.vm_id.to_string()).or_default() += 1;
        }
        let c = &g.counters;
        let counters = CounterView {
            frames: c.frames,
            bytes: c.bytes,
            malformed: c.malformed,
            truncated: c.truncated,
            arp: c.arp,
            non_ip: c.non_ip,
            flows_created: c.flows_created,
            flows_evicted: c.flows_evicted,
            flows_expired: c.flows_expired,
            drops_by_reason: DropReason::ALL
                .iter()
                .map(|r| (r.name().to_string(), c.drops[r.index()]))
                .collect(),
            // Totals over every source that feeds the observer: the in-process tap and each bridge.
            tap_enqueued: tap
                .as_ref()
                .map_or(0, |t| t.enqueued.load(Ordering::Relaxed))
                + bridge_frames,
            tap_dropped: tap
                .as_ref()
                .map_or(0, |t| t.dropped.load(Ordering::Relaxed))
                + bridge_dropped,
            bridge_frames,
            bridge_dropped,
            bridge_dropped_by_reason: bridge_by_reason,
            bridge_truncated_to_snaplen: bridge_trunc,
            bridge_unconfirmed: unconf,
            switch_forward_dropped: fwd,
            reasm_segments_dropped: c.reasm_segments_dropped,
            reasm_streams_abandoned: c.reasm_streams_abandoned,
            reasm_incomplete: c.reasm_incomplete,
            reasm_conflicts: c.reasm_conflicts,
            reasm_bytes: g.reasm_bytes as u64,
            encrypted_dns_flows: c.encrypted_dns_flows,
            quic_hellos: c.quic_hellos,
        };
        let mut per_vm: Vec<VmSummary> = g
            .vm_totals
            .iter()
            .map(|(vm, red)| VmSummary {
                vm_id: vm.clone(),
                flows_active: active.get(vm).copied().unwrap_or(0),
                red: red.clone(),
            })
            .collect();
        per_vm.sort_by(|a, b| a.vm_id.cmp(&b.vm_id));
        let merged = g.rollup.merged(None);
        let since = g
            .rollup
            .history()
            .next()
            .map_or(g.rollup.current().start_us, |w| w.start_us);
        let mut rows: Vec<RedRow> = merged
            .into_iter()
            .map(|(k, red)| RedRow {
                vm_id: k.vm,
                peer: k.peer.to_string(),
                port: k.port,
                proto: proto_name(k.proto),
                direction: if k.outbound { "outbound" } else { "inbound" },
                red,
            })
            .collect();
        rows.sort_by(|a, b| b.red.bytes.cmp(&a.red.bytes));
        let top_talkers: Vec<RedRow> = rows.iter().take(10).cloned().collect();
        rows.retain(|r| r.red.error_events() > 0);
        rows.sort_by(|a, b| b.red.error_events().cmp(&a.red.error_events()));
        rows.truncate(10);
        let capture = g.capture.status(now_us);
        Summary {
            schema: SCHEMA_VERSION,
            enabled: true,
            generated_us: now_us,
            counters,
            flows_active: g.flows.len() as u64,
            per_vm,
            rows_since_us: since,
            top_talkers,
            top_errors: rows,
            capture,
        }
    }

    /// Live flows (and optionally recently closed ones), newest activity first.
    pub fn flows(&self, q: &FlowQuery) -> Vec<FlowView> {
        let g = self.lock();
        let limit = if q.limit == 0 { 200 } else { q.limit.min(5000) };
        let mut out: Vec<FlowView> = g
            .flows
            .iter()
            .map(|f| FlowView::from_flow(f, f.closed))
            .filter(|v| q.matches(v))
            .collect();
        if q.include_closed {
            out.extend(
                g.closed
                    .iter()
                    .map(|c| c.view.clone())
                    .filter(|v| q.matches(v)),
            );
        }
        out.sort_by(|a, b| b.last_seen_us.cmp(&a.last_seen_us));
        out.truncate(limit);
        out
    }

    /// Recent DNS records, newest first.
    pub fn dns(
        &self,
        vm: Option<&str>,
        allowed: Option<&HashSet<String>>,
        limit: usize,
    ) -> Vec<DnsRecord> {
        let g = self.lock();
        let limit = if limit == 0 { 200 } else { limit.min(5000) };
        g.dns
            .iter()
            .rev()
            .filter(|r| vm.is_none_or(|v| v == r.vm_id))
            .filter(|r| allowed.is_none_or(|a| a.contains(&r.vm_id)))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Events with `seq > since`, oldest first, and the sequence to resume from.
    pub fn events_since(
        &self,
        since: u64,
        allowed: Option<&HashSet<String>>,
        vm: Option<&str>,
        limit: usize,
    ) -> (Vec<NetEvent>, u64) {
        let g = self.lock();
        let mut next = since.max(g.events.ring.front().map_or(0, |e| e.seq.saturating_sub(1)));
        let mut out = Vec::new();
        for e in g.events.ring.iter().filter(|e| e.seq > since) {
            next = e.seq;
            if allowed.is_none_or(|a| a.contains(&e.vm_id)) && vm.is_none_or(|v| v == e.vm_id) {
                out.push(e.clone());
                if out.len() >= limit.max(1) {
                    break;
                }
            }
        }
        (out, next)
    }

    /// The last sequence number issued (0 if none).
    pub fn last_seq(&self) -> u64 {
        self.lock().events.next_seq.saturating_sub(1)
    }

    pub fn capture_enable(&self, vm: &str, req: CaptureRequest) -> Result<(), CaptureError> {
        self.lock().capture.enable(vm, req, now_us())
    }

    pub fn capture_disable(&self, vm: &str) -> bool {
        self.lock().capture.disable(vm)
    }

    pub fn capture_status(&self) -> Vec<CaptureStatus> {
        self.lock().capture.status(now_us())
    }

    pub fn capture_pcap(&self, vm: &str) -> Option<Vec<u8>> {
        self.lock().capture.pcap(vm, now_us())
    }

    /// Retained roll-up windows as `(start_us, end_us, rows)`, oldest first, the open window last.
    pub fn history(&self, allowed: Option<&HashSet<String>>) -> Vec<(u64, u64, Vec<RedRow>)> {
        let g = self.lock();
        let conv = |w: &crate::rollup::Window| -> Vec<RedRow> {
            let mut rows: Vec<RedRow> = w
                .rows
                .iter()
                .filter(|(k, _)| allowed.is_none_or(|a| a.contains(&k.vm)))
                .map(|(k, r)| RedRow {
                    vm_id: k.vm.clone(),
                    peer: k.peer.to_string(),
                    port: k.port,
                    proto: proto_name(k.proto),
                    direction: if k.outbound { "outbound" } else { "inbound" },
                    red: r.clone(),
                })
                .collect();
            rows.sort_by(|a, b| b.red.bytes.cmp(&a.red.bytes));
            rows
        };
        g.rollup
            .history()
            .map(|w| (w.start_us, w.end_us, conv(w)))
            .chain(std::iter::once((
                g.rollup.current().start_us,
                g.rollup.current().end_us,
                conv(g.rollup.current()),
            )))
            .collect()
    }
}

static GLOBAL: OnceLock<Arc<Observer>> = OnceLock::new();

/// Installs the process-wide observer the REST API reads. Returns false if one was already installed.
pub fn install_global(o: Arc<Observer>) -> bool {
    GLOBAL.set(o).is_ok()
}

/// The process-wide observer, if one was installed.
pub fn global() -> Option<Arc<Observer>> {
    GLOBAL.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::CaptureFilter;
    use crate::l7::{Redact, fixtures};
    use crate::packet::build::{self, tcp_v4, udp_v4};
    use crate::packet::{TCP_FIN, TCP_PSH, TCP_RST};
    use std::net::Ipv4Addr;

    const G: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 5);
    const S: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);
    const NS: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

    fn id(vm: &str) -> Arc<Identity> {
        Identity::new(vm, "default", "eth0", build::GUEST_MAC)
    }

    fn meta(id: &Arc<Identity>, ts_us: u64, direction: Direction) -> FrameMeta {
        FrameMeta {
            identity: id.clone(),
            ts_us,
            direction,
            verdict: Verdict::Allowed,
        }
    }

    fn obs() -> Observer {
        Observer::new(ObserverConfig::default())
    }

    #[test]
    fn tcp_session_end_to_end() {
        let o = obs();
        let i = id("vm-a");
        let c = (G, 40000);
        let s = (S, 80);
        let t0 = 1_000_000u64;
        let send = |ts: u64, from_guest: bool, f: Vec<u8>| {
            let d = if from_guest {
                Direction::FromGuest
            } else {
                Direction::ToGuest
            };
            o.ingest(&f, &meta(&i, ts, d));
        };
        send(t0, true, tcp_v4(c, s, 100, 0, TCP_SYN, 65535, b""));
        send(
            t0 + 20_000,
            false,
            tcp_v4(s, c, 900, 101, TCP_SYN | TCP_ACK, 65535, b""),
        );
        send(
            t0 + 20_100,
            true,
            tcp_v4(c, s, 101, 901, TCP_ACK, 65535, b""),
        );
        let req = b"GET /private/path HTTP/1.1\r\nHost: Example.org\r\n\r\n";
        send(
            t0 + 30_000,
            true,
            tcp_v4(c, s, 101, 901, TCP_ACK | TCP_PSH, 65535, req),
        );
        send(
            t0 + 55_000,
            false,
            tcp_v4(s, c, 901, 101 + req.len() as u32, TCP_ACK, 65535, b""),
        );
        // Retransmit of the request.
        send(
            t0 + 300_000,
            true,
            tcp_v4(c, s, 101, 901, TCP_ACK | TCP_PSH, 65535, req),
        );
        send(
            t0 + 310_000,
            false,
            tcp_v4(
                s,
                c,
                901,
                101,
                TCP_ACK | TCP_PSH,
                65535,
                b"HTTP/1.1 503 x\r\n\r\n",
            ),
        );
        send(
            t0 + 320_000,
            true,
            tcp_v4(c, s, 101 + req.len() as u32, 919, TCP_RST, 0, b""),
        );

        let flows = o.flows(&FlowQuery::default());
        assert_eq!(flows.len(), 1);
        let f = &flows[0];
        assert_eq!((f.proto.as_str(), f.direction), ("tcp", "outbound"));
        assert_eq!((f.client.port, f.server.port), (40000, 80));
        assert_eq!(f.packets_c2s, 5);
        assert_eq!(f.packets_s2c, 3);
        let tcp = f.tcp.as_ref().unwrap();
        assert_eq!(tcp.handshake_rtt_us, Some(20_000));
        assert_eq!(tcp.retransmits_c2s, 1);
        assert_eq!(
            tcp.rtt.samples, 2,
            "handshake plus one data RTT; the retransmitted data is not sampled"
        );
        assert_eq!(tcp.rtt.last_us, 25_000);
        assert_eq!(f.http_host.as_deref(), Some("example.org"));
        assert_eq!(f.app, Some("http"));
        assert_eq!(f.close_reason, Some(CloseReason::Reset));

        let s = o.summary(t0 + 400_000);
        let vm = &s.per_vm[0];
        assert_eq!((vm.red.flows, vm.red.retransmits, vm.red.resets), (1, 1, 1));
        assert_eq!((vm.red.http_requests, vm.red.http_5xx), (1, 1));
        assert!(s.top_errors[0].red.error_events() >= 3);
        // No path anywhere in anything exported.
        let json = serde_json::to_string(&(
            o.flows(&FlowQuery::default()),
            o.events_since(0, None, None, 1000).0,
        ))
        .unwrap();
        assert!(!json.contains("private"));
    }

    #[test]
    fn closed_flow_is_exported_after_linger_and_new_syn_restarts() {
        let o = Observer::new(ObserverConfig {
            flow: FlowConfig {
                closed_linger_us: 2_000_000,
                ..FlowConfig::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let c = (G, 40001);
        let s = (S, 443);
        let m = |ts| meta(&i, ts, Direction::FromGuest);
        o.ingest(&tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""), &m(1_000_000));
        o.ingest(
            &tcp_v4(c, s, 2, 0, TCP_FIN | TCP_ACK, 100, b""),
            &m(1_100_000),
        );
        o.ingest(
            &tcp_v4(s, c, 9, 3, TCP_FIN | TCP_ACK, 100, b""),
            &m(1_200_000),
        );
        assert_eq!(o.flows(&FlowQuery::default()).len(), 1);
        // A new SYN on the same 5-tuple is a new flow; the old one moves to history.
        o.ingest(&tcp_v4(c, s, 5000, 0, TCP_SYN, 100, b""), &m(1_300_000));
        let all = o.flows(&FlowQuery {
            include_closed: true,
            ..FlowQuery::default()
        });
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.iter()
                .filter(|f| f.close_reason == Some(CloseReason::Fin))
                .count(),
            1
        );
        // Idle aging finally removes the second flow, via tick.
        o.tick(1_300_000 + 400_000_000);
        assert!(o.flows(&FlowQuery::default()).is_empty());
        let ev = o.events_since(0, None, None, 1000).0;
        assert_eq!(
            ev.iter().filter(|e| e.kind.name() == "flow_closed").count(),
            2
        );
    }

    #[test]
    fn dns_latency_and_redaction() {
        let o = obs();
        let i = id("vm-a");
        let c = (G, 5353);
        let r = (NS, 53);
        let q = fixtures::dns_query(0x1234, "Secret.Internal.Example.com", 1);
        let a = fixtures::dns_response(
            0x1234,
            "secret.internal.example.com",
            1,
            0,
            &[[10, 1, 2, 3]],
        );
        o.ingest(
            &udp_v4(c, r, &q),
            &meta(&i, 10_000_000, Direction::FromGuest),
        );
        o.ingest(&udp_v4(r, c, &a), &meta(&i, 10_004_500, Direction::ToGuest));
        let nx = fixtures::dns_response(0x9999, "nope.example.com", 1, 3, &[]);
        o.ingest(
            &udp_v4(r, c, &nx),
            &meta(&i, 10_005_000, Direction::ToGuest),
        );

        let d = o.dns(None, None, 10);
        assert_eq!(d.len(), 3);
        let resp = d.iter().find(|r| r.response && r.id == 0x1234).unwrap();
        assert_eq!(resp.latency_us, Some(4_500));
        assert_eq!(resp.qname.as_deref(), Some("secret.internal.example.com"));
        assert_eq!(resp.answers, vec!["10.1.2.3"]);
        let s = o.summary(10_006_000);
        assert_eq!(
            (s.per_vm[0].red.dns_queries, s.per_vm[0].red.dns_errors),
            (1, 1)
        );
        assert_eq!(s.per_vm[0].red.dns_latency_sum_us, 4_500);

        // Strict redaction drops names and answers before storage.
        let o = Observer::new(ObserverConfig {
            redaction: Redaction::strict(),
            ..ObserverConfig::default()
        });
        o.ingest(&udp_v4(c, r, &q), &meta(&i, 1, Direction::FromGuest));
        o.ingest(&udp_v4(r, c, &a), &meta(&i, 2, Direction::ToGuest));
        let json = serde_json::to_string(&(o.dns(None, None, 10), o.flows(&FlowQuery::default())))
            .unwrap();
        assert!(!json.to_lowercase().contains("secret"));
        assert!(!json.contains("10.1.2.3"));
        assert_eq!(o.dns(None, None, 10)[0].qname, None);
    }

    #[test]
    fn tls_hello_gives_sni_and_fingerprints() {
        let o = obs();
        let i = id("vm-a");
        let hello = fixtures::client_hello(
            0x0301,
            &[0x1301, 0x1302],
            &[
                fixtures::ext_sni("API.Example.com"),
                fixtures::ext_alpn(&["h2"]),
                fixtures::ext_supported_versions(&[0x0304]),
            ],
        );
        let c = (G, 50000);
        let s = (S, 443);
        o.ingest(
            &tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""),
            &meta(&i, 1, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(c, s, 2, 1, TCP_ACK | TCP_PSH, 100, &hello),
            &meta(&i, 2, Direction::FromGuest),
        );
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(f.sni.as_deref(), Some("api.example.com"));
        assert_eq!(f.app, Some("tls"));
        assert_eq!(f.ja3.as_ref().map(String::len), Some(32));
        assert!(
            f.ja4.as_ref().unwrap().starts_with("t13d0203h2_"),
            "{:?}",
            f.ja4
        );
        assert_eq!(o.summary(3).per_vm[0].red.tls_handshakes, 1);

        // Hashing keeps correlation without the name.
        let o = Observer::new(ObserverConfig {
            redaction: Redaction {
                tls_sni: Redact::Hash,
                ..Redaction::default()
            },
            ..ObserverConfig::default()
        });
        o.ingest(
            &tcp_v4(c, s, 2, 1, TCP_ACK | TCP_PSH, 100, &hello),
            &meta(&i, 2, Direction::FromGuest),
        );
        let sni = o.flows(&FlowQuery::default())[0].sni.clone().unwrap();
        assert!(sni.starts_with("h:") && !sni.contains("example"));
    }

    #[test]
    fn verdicts_and_drop_accounting() {
        let o = obs();
        let i = id("vm-a");
        let f = udp_v4((G, 1000), (S, 9999), b"x");
        let denied = FrameMeta {
            identity: i.clone(),
            ts_us: 5,
            direction: Direction::FromGuest,
            verdict: Verdict::Denied(DropReason::Policy),
        };
        o.ingest(&f, &denied);
        o.ingest(&f, &denied);
        let flows = o.flows(&FlowQuery {
            verdict: Some("denied".into()),
            ..FlowQuery::default()
        });
        assert_eq!(flows.len(), 1);
        assert_eq!(
            (
                flows[0].verdict,
                flows[0].drop_reason,
                flows[0].dropped_packets
            ),
            ("denied", Some("policy"), 2)
        );
        let s = o.summary(6);
        let policy = s
            .counters
            .drops_by_reason
            .iter()
            .find(|(r, _)| r == "policy")
            .unwrap()
            .1;
        assert_eq!(policy, 2);
        let drops = o
            .events_since(0, None, None, 100)
            .0
            .iter()
            .filter(|e| e.kind.name() == "drop")
            .count();
        assert_eq!(drops, 1, "one drop event per flow, not per packet");
    }

    #[test]
    fn flow_table_cap_holds_under_a_scan() {
        let o = Observer::new(ObserverConfig {
            flow: FlowConfig {
                max_flows: 100,
                ..FlowConfig::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        for p in 0..5000u16 {
            let f = tcp_v4((G, 1024 + p), (S, 80), 1, 0, TCP_SYN, 100, b"");
            o.ingest(&f, &meta(&i, 1000 + p as u64, Direction::FromGuest));
        }
        let s = o.summary(10_000);
        assert_eq!(s.flows_active, 100);
        assert_eq!(s.counters.flows_evicted, 4900);
        assert!(
            o.events_since(0, None, None, 100_000).0.len() <= 4096,
            "event ring is bounded"
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_observer() {
        let o = obs();
        let i = id("vm-a");
        let mut s = 0xdead_beef_u64;
        let mut next = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 24) as u32
        };
        let seeds = [
            tcp_v4(
                (G, 1),
                (S, 53),
                1,
                1,
                TCP_ACK,
                1,
                &fixtures::dns_query(1, "a.b", 1),
            ),
            udp_v4(
                (G, 1),
                (NS, 53),
                &fixtures::dns_response(1, "a.b", 1, 0, &[[1, 1, 1, 1]]),
            ),
            tcp_v4(
                (G, 1),
                (S, 443),
                1,
                1,
                TCP_ACK,
                1,
                &fixtures::client_hello(0x0303, &[0x1301], &[fixtures::ext_sni("x")]),
            ),
            tcp_v4(
                (G, 1),
                (S, 80),
                1,
                1,
                TCP_ACK,
                1,
                b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
            ),
        ];
        for n in 0..30_000u64 {
            let mut f = seeds[(next() % 4) as usize].clone();
            for _ in 0..(next() % 6) {
                let at = (next() as usize) % f.len();
                f[at] = next() as u8;
            }
            if next() % 3 == 0 {
                f.truncate((next() as usize) % (f.len() + 1));
            }
            o.ingest(&f, &meta(&i, n, Direction::FromGuest));
        }
        assert_eq!(o.counters().frames, 30_000);
        let _ = o.summary(40_000);
    }

    #[test]
    fn tenant_scoping_and_event_resume() {
        let o = obs();
        for vm in ["vm-a", "vm-b"] {
            let i = id(vm);
            o.ingest(
                &udp_v4((G, 1), (S, 2), b"x"),
                &meta(&i, 1, Direction::FromGuest),
            );
        }
        let only_a: HashSet<String> = ["vm-a".to_string()].into();
        let q = FlowQuery {
            allowed_vms: Some(only_a.clone()),
            ..FlowQuery::default()
        };
        let flows = o.flows(&q);
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].vm_id, "vm-a");

        let (first, next) = o.events_since(0, Some(&only_a), None, 100);
        assert!(!first.is_empty() && first.iter().all(|e| e.vm_id == "vm-a"));
        let (again, _) = o.events_since(next, Some(&only_a), None, 100);
        assert!(again.is_empty(), "resume point skips what was delivered");
    }

    // ---- reassembly, TLS server hello, HTTP/2, QUIC, encrypted DNS: end to end through the observer ----------

    fn big_hello(sni: &str) -> Vec<u8> {
        fixtures::client_hello(
            0x0303,
            &[0x1301, 0x1302],
            &[
                fixtures::ext_sni(sni),
                fixtures::ext_alpn(&["h2", "http/1.1"]),
                fixtures::ext_supported_versions(&[0x0304]),
                fixtures::ext(0x0015, &vec![0u8; 700]),
            ],
        )
    }

    fn tls_events(o: &Observer, kind: &str) -> usize {
        o.events_since(0, None, None, 100_000)
            .0
            .iter()
            .filter(|e| e.kind.name() == kind)
            .count()
    }

    #[test]
    fn client_hello_in_three_reordered_retransmitted_segments() {
        let o = obs();
        let i = id("vm-a");
        let hello = big_hello("Split.Example.com");
        assert!(hello.len() > 700);
        let (c, s) = ((G, 50100), (S, 443));
        let isn = 1000u32;
        let m = |ts| meta(&i, ts, Direction::FromGuest);
        o.ingest(&tcp_v4(c, s, isn, 0, TCP_SYN, 100, b""), &m(1));
        let seg = |n: usize| {
            let at = n * 300;
            (isn + 1 + at as u32, &hello[at..(at + 300).min(hello.len())])
        };
        let parts = hello.len().div_ceil(300);
        assert!(parts >= 3);
        // Tail first, then the head, a retransmit of the head, then the middle.
        let mut order: Vec<usize> = vec![parts - 1, 0, 0];
        order.extend(1..parts - 1);
        for (k, n) in order.iter().enumerate() {
            let (seq, data) = seg(*n);
            o.ingest(
                &tcp_v4(c, s, seq, 1, TCP_ACK | TCP_PSH, 100, data),
                &m(10 + k as u64),
            );
        }
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(f.sni.as_deref(), Some("split.example.com"));
        assert_eq!(f.app, Some("tls"));
        assert_eq!(f.alpn.as_deref(), Some("h2"));
        assert!(
            f.ja4.as_ref().unwrap().starts_with("t13d0204h2_"),
            "{:?}",
            f.ja4
        );
        assert_eq!(
            tls_events(&o, "tls"),
            1,
            "one hello, one event, however many segments"
        );
        assert_eq!(
            o.summary(100).counters.reasm_bytes,
            0,
            "buffers are released once parsed"
        );
        assert_eq!(o.summary(100).per_vm[0].red.tls_handshakes, 1);
    }

    #[test]
    fn server_hello_metadata_and_ja3s_ja4s() {
        let o = obs();
        let i = id("vm-a");
        let (c, s) = ((G, 50200), (S, 443));
        o.ingest(
            &tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""),
            &meta(&i, 1, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(s, c, 9000, 2, TCP_SYN | TCP_ACK, 100, b""),
            &meta(&i, 2, Direction::ToGuest),
        );
        let sh = fixtures::server_hello(
            0x0303,
            0x1301,
            &[
                fixtures::ext(43, &[0x03, 0x04]),
                fixtures::ext(51, &[0x00, 0x1d, 0x00, 0x00]),
            ],
        );
        let mut wire = sh.clone();
        wire.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x20]);
        wire.extend_from_slice(&[0x99; 32]);
        let (a, b) = wire.split_at(20);
        o.ingest(
            &tcp_v4(s, c, 9001, 2, TCP_ACK | TCP_PSH, 100, a),
            &meta(&i, 3, Direction::ToGuest),
        );
        o.ingest(
            &tcp_v4(s, c, 9001 + 20, 2, TCP_ACK | TCP_PSH, 100, b),
            &meta(&i, 4, Direction::ToGuest),
        );
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(f.tls_version.as_deref(), Some("TLS1.3"));
        assert_eq!(f.tls_cipher.as_deref(), Some("TLS_AES_128_GCM_SHA256"));
        assert_eq!(f.ja3s.as_ref().map(String::len), Some(32));
        assert!(
            f.ja4s.as_ref().unwrap().starts_with("t130200_1301_"),
            "{:?}",
            f.ja4s
        );
        assert_eq!(tls_events(&o, "tls_server"), 1);
        // Only metadata: the opaque application-data bytes never show up anywhere.
        let json = serde_json::to_string(&o.events_since(0, None, None, 1000).0).unwrap();
        assert!(json.contains("TLS_AES_128_GCM_SHA256") && !json.contains("153, 153"));
    }

    #[test]
    fn reassembly_budget_is_enforced_and_returned() {
        let o = Observer::new(ObserverConfig {
            reasm: crate::reasm::ReasmConfig {
                max_total_bytes: 8 * 1024,
                ..Default::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let hello = big_hello("budget.example.com");
        // Many connections each stuck after the first 300 bytes of a hello.
        for p in 0..200u16 {
            let (c, s) = ((G, 40000 + p), (S, 443));
            o.ingest(
                &tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""),
                &meta(&i, 10 + p as u64, Direction::FromGuest),
            );
            o.ingest(
                &tcp_v4(c, s, 2, 1, TCP_ACK | TCP_PSH, 100, &hello[..300]),
                &meta(&i, 10 + p as u64, Direction::FromGuest),
            );
        }
        let c = o.summary(1000).counters;
        assert!(c.reasm_bytes <= 8 * 1024, "held {} bytes", c.reasm_bytes);
        assert!(
            c.reasm_streams_abandoned > 0 && c.reasm_segments_dropped > 0,
            "{c:?}"
        );
        // Dropping the flows returns their bytes.
        o.tick(10 + 400_000_000);
        assert_eq!(o.summary(10 + 400_000_000).counters.reasm_bytes, 0);
    }

    #[test]
    fn reassembly_can_be_disabled() {
        let o = Observer::new(ObserverConfig {
            reasm: crate::reasm::ReasmConfig {
                enabled: false,
                ..Default::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let hello = big_hello("off.example.com");
        let (c, s) = ((G, 50300), (S, 443));
        o.ingest(
            &tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""),
            &meta(&i, 1, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(c, s, 2, 1, TCP_ACK, 100, &hello[..300]),
            &meta(&i, 2, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(c, s, 302, 1, TCP_ACK, 100, &hello[300..]),
            &meta(&i, 3, Direction::FromGuest),
        );
        assert_eq!(o.flows(&FlowQuery::default())[0].sni, None);
        assert_eq!(o.summary(5).counters.reasm_bytes, 0);
        // A hello that fits in one segment still works.
        let small =
            fixtures::client_hello(0x0303, &[0x1301], &[fixtures::ext_sni("one.example.com")]);
        o.ingest(
            &tcp_v4((G, 50301), s, 2, 1, TCP_ACK, 100, &small),
            &meta(&i, 4, Direction::FromGuest),
        );
        assert!(
            o.flows(&FlowQuery::default())
                .iter()
                .any(|f| f.sni.as_deref() == Some("one.example.com"))
        );
    }

    #[test]
    fn http2_cleartext_requests_and_responses() {
        use crate::h2::fixtures as h2;
        let o = obs();
        let i = id("vm-a");
        let (c, s) = ((G, 50400), (S, 8080));
        let mut req = h2::client_preface();
        req.extend(h2::headers_frame(
            1,
            true,
            &h2::request_block("POST", "http", "/v1/secret/path?k=v", "Api.Mesh.local:8080"),
        ));
        let mut resp = h2::settings();
        resp.extend(h2::headers_frame(
            1,
            true,
            &h2::response_block(503, Some("application/json; charset=utf-8")),
        ));
        o.ingest(
            &tcp_v4(c, s, 100, 0, TCP_SYN, 100, b""),
            &meta(&i, 1, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(s, c, 500, 101, TCP_SYN | TCP_ACK, 100, b""),
            &meta(&i, 2, Direction::ToGuest),
        );
        let (a, b) = req.split_at(30);
        o.ingest(
            &tcp_v4(c, s, 101, 501, TCP_ACK, 100, a),
            &meta(&i, 3, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(c, s, 131, 501, TCP_ACK, 100, b),
            &meta(&i, 4, Direction::FromGuest),
        );
        o.ingest(
            &tcp_v4(s, c, 501, 101 + req.len() as u32, TCP_ACK, 100, &resp),
            &meta(&i, 5, Direction::ToGuest),
        );
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(f.app, Some("h2"));
        assert_eq!(f.http_host.as_deref(), Some("api.mesh.local:8080"));
        assert_eq!(f.http_requests, 1);
        let sum = o.summary(10);
        assert_eq!(
            (sum.per_vm[0].red.http_requests, sum.per_vm[0].red.http_5xx),
            (1, 1)
        );
        let evs = o.events_since(0, None, None, 1000).0;
        let json = serde_json::to_string(&evs).unwrap();
        assert!(
            json.contains("\"version\":\"2\"")
                && json.contains("application/json")
                && json.contains("\"scheme\":\"http\"")
        );
        assert!(
            !json.contains("secret") && !json.contains("charset"),
            "path and parameters never leave: {json}"
        );
        // An h2 connection keeps its (bounded) prefix while it may still carry frames, and gives it back when the
        // flow ends.
        let held = o.summary(10).counters.reasm_bytes;
        assert!(held > 0 && held <= 2 * 16 * 1024, "held {held}");
        o.tick(10 + 400_000_000);
        assert_eq!(o.summary(10 + 400_000_000).counters.reasm_bytes, 0);
    }

    #[test]
    fn http2_host_redaction_applies() {
        use crate::h2::fixtures as h2;
        let o = Observer::new(ObserverConfig {
            redaction: Redaction {
                http_host: Redact::Drop,
                ..Redaction::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let (c, s) = ((G, 50401), (S, 8080));
        let mut req = h2::client_preface();
        req.extend(h2::headers_frame(
            1,
            true,
            &h2::request_block("GET", "http", "/", "private.mesh.local"),
        ));
        o.ingest(
            &tcp_v4(c, s, 1, 0, TCP_ACK, 100, &req),
            &meta(&i, 1, Direction::FromGuest),
        );
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(
            (f.app, f.http_requests, f.http_host.clone()),
            (Some("h2"), 1, None)
        );
        assert!(
            !serde_json::to_string(&o.events_since(0, None, None, 100).0)
                .unwrap()
                .contains("private.mesh")
        );
    }

    fn quic_vector() -> Vec<u8> {
        use crate::quic::fixtures::*;
        let hello = handshake_client_hello(&[
            fixtures::ext_sni("quic.Example.com"),
            fixtures::ext_alpn(&["h3"]),
            fixtures::ext_supported_versions(&[0x0304]),
        ]);
        client_initial(
            crate::quic::VERSION_1,
            &[0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08],
            &[1, 2],
            0,
            &crypto_frame(0, &hello),
            1252,
        )
    }

    #[test]
    fn quic_initial_yields_sni_alpn_and_ja4() {
        let o = obs();
        let i = id("vm-a");
        o.ingest(
            &udp_v4((G, 55000), (S, 443), &quic_vector()),
            &meta(&i, 1, Direction::FromGuest),
        );
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!((f.app, f.quic_version), (Some("quic"), Some(1)));
        assert_eq!(f.sni.as_deref(), Some("quic.example.com"));
        assert_eq!(f.alpn.as_deref(), Some("h3"));
        assert!(
            f.ja4.as_ref().unwrap().starts_with("q13d0203h3_"),
            "{:?}",
            f.ja4
        );
        let ev = o.events_since(0, None, None, 100).0;
        assert_eq!(ev.iter().filter(|e| e.kind.name() == "quic").count(), 1);
        assert_eq!(o.summary(5).counters.quic_hellos, 1);
        // A second client Initial datagram does not duplicate anything.
        o.ingest(
            &udp_v4((G, 55000), (S, 443), &quic_vector()),
            &meta(&i, 2, Direction::FromGuest),
        );
        assert_eq!(
            o.events_since(0, None, None, 100)
                .0
                .iter()
                .filter(|e| e.kind.name() == "quic")
                .count(),
            1
        );
    }

    #[test]
    fn quic_redaction_and_switch_off() {
        let i = id("vm-a");
        let strict = Observer::new(ObserverConfig {
            redaction: Redaction::strict(),
            ..ObserverConfig::default()
        });
        strict.ingest(
            &udp_v4((G, 55001), (S, 443), &quic_vector()),
            &meta(&i, 1, Direction::FromGuest),
        );
        let f = &strict.flows(&FlowQuery::default())[0];
        assert_eq!((f.sni.clone(), f.ja4.clone()), (None, None));
        assert!(
            !serde_json::to_string(&strict.events_since(0, None, None, 100).0)
                .unwrap()
                .to_lowercase()
                .contains("example")
        );

        let hashed = Observer::new(ObserverConfig {
            redaction: Redaction {
                tls_sni: Redact::Hash,
                ..Redaction::default()
            },
            ..ObserverConfig::default()
        });
        hashed.ingest(
            &udp_v4((G, 55002), (S, 443), &quic_vector()),
            &meta(&i, 1, Direction::FromGuest),
        );
        assert!(
            hashed.flows(&FlowQuery::default())[0]
                .sni
                .as_deref()
                .unwrap()
                .starts_with("h:")
        );

        let off = Observer::new(ObserverConfig {
            quic_initial: false,
            ..ObserverConfig::default()
        });
        off.ingest(
            &udp_v4((G, 55003), (S, 443), &quic_vector()),
            &meta(&i, 1, Direction::FromGuest),
        );
        let f = &off.flows(&FlowQuery::default())[0];
        assert_eq!(
            (f.app, f.sni.clone()),
            (Some("quic"), None),
            "detected, not decrypted"
        );
    }

    #[test]
    fn encrypted_dns_dot_doh_and_doq() {
        let i = id("vm-a");
        let o = obs();
        // DoT: TCP/853.
        o.ingest(
            &tcp_v4((G, 50500), (S, 853), 1, 0, TCP_SYN, 100, b""),
            &meta(&i, 1, Direction::FromGuest),
        );
        // DoH: a hello whose SNI is on the list (also a subdomain).
        for (n, sni) in [
            "dns.google",
            "Resolver.Cloudflare-DNS.com",
            "www.example.com",
        ]
        .into_iter()
        .enumerate()
        {
            let hello = fixtures::client_hello(0x0303, &[0x1301], &[fixtures::ext_sni(sni)]);
            o.ingest(
                &tcp_v4(
                    (G, 50600 + n as u16),
                    (S, 443),
                    2,
                    1,
                    TCP_ACK | TCP_PSH,
                    100,
                    &hello,
                ),
                &meta(&i, 2, Direction::FromGuest),
            );
        }
        // DoQ: QUIC to UDP/853.
        o.ingest(
            &udp_v4((G, 50700), (S, 853), &quic_vector()),
            &meta(&i, 3, Direction::FromGuest),
        );
        let tags: Vec<(u16, Option<&str>)> = {
            let mut v: Vec<_> = o
                .flows(&FlowQuery::default())
                .iter()
                .map(|f| (f.server.port, f.encrypted_dns))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            tags,
            vec![
                (443, None),
                (443, Some("doh")),
                (443, Some("doh")),
                (853, Some("doq")),
                (853, Some("dot")),
            ]
        );
        assert_eq!(o.summary(5).counters.encrypted_dns_flows, 4);

        // The list is configurable; with it emptied nothing matches, and redaction does not affect matching.
        let custom = Observer::new(ObserverConfig {
            doh_hosts: vec!["doh.corp.internal".into()],
            redaction: Redaction::strict(),
            ..ObserverConfig::default()
        });
        for (n, sni) in ["dns.google", "x.doh.corp.internal"]
            .into_iter()
            .enumerate()
        {
            let hello = fixtures::client_hello(0x0303, &[0x1301], &[fixtures::ext_sni(sni)]);
            custom.ingest(
                &tcp_v4((G, 50800 + n as u16), (S, 443), 2, 1, TCP_ACK, 100, &hello),
                &meta(&i, 1, Direction::FromGuest),
            );
        }
        let f = custom.flows(&FlowQuery::default());
        assert_eq!(
            f.iter().filter(|f| f.encrypted_dns == Some("doh")).count(),
            1
        );
        assert!(
            f.iter().all(|f| f.sni.is_none()),
            "the tag does not leak the name under strict redaction"
        );
    }

    #[test]
    fn dns_edns_and_typed_answers() {
        use fixtures::Rr;
        let o = obs();
        let i = id("vm-a");
        let (c, r) = ((G, 5353), (NS, 53));
        let q = fixtures::dns_query_edns(0x4242, "www.example.com", 65, 1232, true);
        let a = fixtures::dns_response_rr(
            0x4242,
            "www.example.com",
            65,
            0,
            &[
                Rr::Cname("edge.cdn.example.net"),
                Rr::A([192, 0, 2, 1]),
                Rr::Aaaa([0x20, 0x01, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
                Rr::Https {
                    priority: 1,
                    target: ".",
                    alpn: &["h3", "h2"],
                    port: Some(8443),
                    ech: true,
                },
            ],
            Some((4096, true, 0)),
        );
        o.ingest(&udp_v4(c, r, &q), &meta(&i, 1_000, Direction::FromGuest));
        o.ingest(&udp_v4(r, c, &a), &meta(&i, 3_000, Direction::ToGuest));
        let d = o.dns(None, None, 10);
        let resp = d.iter().find(|r| r.response).unwrap();
        assert_eq!(resp.qtype, "HTTPS");
        assert_eq!(resp.answer_types, vec!["CNAME", "A", "AAAA", "HTTPS"]);
        assert_eq!(resp.cnames, vec!["edge.cdn.example.net"]);
        assert_eq!(resp.answers, vec!["192.0.2.1", "2001:db8::1"]);
        assert_eq!(resp.https.len(), 1);
        let h = &resp.https[0];
        assert_eq!(
            (h.priority, h.target.as_deref(), h.port, h.ech),
            (1, Some("."), Some(8443), true)
        );
        assert_eq!(h.alpn, vec!["h3", "h2"]);
        assert_eq!(
            resp.edns.as_ref().map(|e| (e.udp_size, e.dnssec_ok)),
            Some((4096, true))
        );
        assert_eq!(resp.latency_us, Some(2_000));
        let qrec = d.iter().find(|r| !r.response).unwrap();
        assert_eq!(
            qrec.edns.as_ref().map(|e| (e.udp_size, e.dnssec_ok)),
            Some((1232, true))
        );

        // Answer detail follows the answer redaction setting; names follow the qname setting.
        let strict = Observer::new(ObserverConfig {
            redaction: Redaction::strict(),
            ..ObserverConfig::default()
        });
        strict.ingest(&udp_v4(c, r, &q), &meta(&i, 1_000, Direction::FromGuest));
        strict.ingest(&udp_v4(r, c, &a), &meta(&i, 3_000, Direction::ToGuest));
        let json = serde_json::to_string(&strict.dns(None, None, 10)).unwrap();
        assert!(
            !json.contains("cdn") && !json.contains("192.0.2.1") && !json.contains("h3"),
            "{json}"
        );
        let r2 = strict.dns(None, None, 10);
        assert_eq!(
            r2.iter().find(|r| r.response).unwrap().answer_types,
            vec!["CNAME", "A", "AAAA", "HTTPS"]
        );

        // Extended rcode.
        let bad = fixtures::dns_response_rr(1, "x.test", 1, 0, &[], Some((512, false, 1)));
        let m = l7::parse_dns(&bad).unwrap();
        assert_eq!(m.full_rcode(), 16);
        assert_eq!(l7::dns_rcode_name_full(m.full_rcode()), "BADVERS");
    }

    #[test]
    fn dns_over_tcp_still_works_and_malformed_additional_is_survivable() {
        let o = obs();
        let i = id("vm-a");
        let a = fixtures::dns_response_rr(
            7,
            "t.test",
            1,
            0,
            &[fixtures::Rr::A([1, 2, 3, 4])],
            Some((512, false, 0)),
        );
        for cut in 0..a.len() {
            let _ = l7::parse_dns(&a[..cut]);
        }
        let mut t = (a.len() as u16).to_be_bytes().to_vec();
        t.extend(&a);
        o.ingest(
            &tcp_v4((NS, 53), (G, 40404), 1, 1, TCP_ACK, 100, &t),
            &meta(&i, 1, Direction::ToGuest),
        );
        let d = o.dns(None, None, 10);
        assert_eq!(d.len(), 1);
        assert!(d[0].tcp && d[0].answers == vec!["1.2.3.4"]);
    }

    #[test]
    fn mixed_hostile_l7_traffic_never_panics_or_overruns_the_budget() {
        let o = Observer::new(ObserverConfig {
            reasm: crate::reasm::ReasmConfig {
                max_total_bytes: 64 * 1024,
                ..Default::default()
            },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let mut s = 0x5eed_u64;
        let mut next = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 24) as u32
        };
        let seeds: Vec<Vec<u8>> = vec![
            big_hello("fuzz.example.com"),
            {
                let mut v = crate::h2::fixtures::client_preface();
                v.extend(crate::h2::fixtures::headers_frame(
                    1,
                    true,
                    &crate::h2::fixtures::request_block("GET", "http", "/", "f.test"),
                ));
                v
            },
            fixtures::server_hello(0x0303, 0x1301, &[fixtures::ext(43, &[3, 4])]),
            quic_vector(),
        ];
        for n in 0..20_000u32 {
            let seed = &seeds[(next() % 4) as usize];
            let port = (next() % 64) as u16;
            let off = (next() as usize) % seed.len();
            let len = ((next() as usize) % 400).max(1).min(seed.len() - off);
            let mut chunk = seed[off..off + len].to_vec();
            if next() % 4 == 0 {
                let at = next() as usize % chunk.len();
                chunk[at] = next() as u8;
            }
            let seq = if next() % 2 == 0 {
                off as u32 + 1
            } else {
                next() % 100_000
            };
            let frame = match next() % 3 {
                0 => udp_v4((G, 40000 + port), (S, 443), seed),
                1 => tcp_v4((G, 40000 + port), (S, 443), seq, 1, TCP_ACK, 100, &chunk),
                _ => tcp_v4((S, 443), (G, 40000 + port), seq, 1, TCP_ACK, 100, &chunk),
            };
            o.ingest(&frame, &meta(&i, n as u64 * 10, Direction::FromGuest));
            if n % 1000 == 0 {
                assert!(o.summary(n as u64 * 10).counters.reasm_bytes <= 64 * 1024);
            }
        }
        assert_eq!(o.counters().frames, 20_000);
    }

    #[test]
    fn capture_through_the_observer() {
        let o = obs();
        let i = id("vm-a");
        assert!(o.capture_pcap("vm-a").is_none());
        assert!(o.capture_enable("vm-a", CaptureRequest::default()).is_err());
        o.capture_enable(
            "vm-a",
            CaptureRequest {
                filter: CaptureFilter {
                    port: Some(53),
                    ..Default::default()
                },
                ..CaptureRequest::default()
            },
        )
        .unwrap();
        let ts = now_us();
        o.ingest(
            &udp_v4((G, 5000), (NS, 53), b"q"),
            &meta(&i, ts, Direction::FromGuest),
        );
        o.ingest(
            &udp_v4((G, 5000), (NS, 80), b"q"),
            &meta(&i, ts, Direction::FromGuest),
        );
        let pcap = o.capture_pcap("vm-a").unwrap();
        assert_eq!(&pcap[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(o.capture_status()[0].packets, 1);
        assert!(o.capture_disable("vm-a"));
    }

    // --- Flow orientation and close, on synthetic traces -------------------------------------------------------

    mod orientation {
        use super::*;
        use crate::packet::Mac;

        const A: Ipv4Addr = Ipv4Addr::new(10, 89, 50, 2);
        const B: Ipv4Addr = Ipv4Addr::new(10, 89, 50, 3);
        const MAC_A: Mac = [2, 0, 0, 0, 0, 0xa];
        const MAC_B: Mac = [2, 0, 0, 0, 0, 0xb];

        /// Rewrites the Ethernet addresses of a builder frame.
        fn l2(mut f: Vec<u8>, dst: Mac, src: Mac) -> Vec<u8> {
            f[..6].copy_from_slice(&dst);
            f[6..12].copy_from_slice(&src);
            f
        }

        fn one(o: &Observer) -> FlowView {
            let f = o.flows(&FlowQuery::default());
            assert_eq!(f.len(), 1, "{f:?}");
            f.into_iter().next().unwrap()
        }

        fn view(o: &Observer, vm: &str) -> FlowView {
            let f: Vec<_> = o
                .flows(&FlowQuery::default())
                .into_iter()
                .filter(|f| f.vm_id == vm)
                .collect();
            assert_eq!(f.len(), 1, "{vm}: {f:?}");
            f.into_iter().next().unwrap()
        }

        #[test]
        fn full_handshake_observed_in_one_view() {
            let o = obs();
            let i = id("vm-a");
            let (c, s) = ((G, 40000), (S, 5201));
            let from = |t: u64, f: Vec<u8>| o.ingest(&f, &meta(&i, t, Direction::FromGuest));
            let to = |t: u64, f: Vec<u8>| o.ingest(&f, &meta(&i, t, Direction::ToGuest));
            from(1, tcp_v4(c, s, 100, 0, TCP_SYN, 65535, b""));
            to(2, tcp_v4(s, c, 900, 101, TCP_SYN | TCP_ACK, 65535, b""));
            from(3, tcp_v4(c, s, 101, 901, TCP_ACK, 65535, b""));
            let f = one(&o);
            assert_eq!((f.client.port, f.server.port), (40000, 5201));
            assert_eq!((f.direction, f.oriented), ("outbound", "observed"));
            assert_eq!(
                f.tcp.as_ref().unwrap().state,
                crate::health::TcpState::Established
            );
            assert!(!f.tcp.as_ref().unwrap().midstream);
        }

        #[test]
        fn first_seen_syn_ack_makes_the_sender_the_server() {
            // The server VM's own view when it is the first to be seen: its SYN-ACK. Before the fix this
            // recorded client=server-side, direction=outbound.
            let o = obs();
            let i = id("vm-b");
            let (server, client) = ((B, 5201), (A, 60856));
            o.ingest(
                &tcp_v4(server, client, 900, 101, TCP_SYN | TCP_ACK, 65535, b""),
                &meta(&i, 1, Direction::FromGuest),
            );
            o.ingest(
                &tcp_v4(client, server, 101, 901, TCP_ACK, 65535, b""),
                &meta(&i, 2, Direction::ToGuest),
            );
            o.ingest(
                &tcp_v4(
                    client,
                    server,
                    101,
                    901,
                    TCP_ACK | TCP_PSH,
                    65535,
                    &[7u8; 100],
                ),
                &meta(&i, 3, Direction::ToGuest),
            );
            o.ingest(
                &tcp_v4(server, client, 901, 201, TCP_ACK, 65535, b""),
                &meta(&i, 4, Direction::FromGuest),
            );
            let f = one(&o);
            assert_eq!((f.client.ip.as_str(), f.client.port), ("10.89.50.2", 60856));
            assert_eq!((f.server.ip.as_str(), f.server.port), ("10.89.50.3", 5201));
            assert_eq!((f.direction, f.oriented), ("inbound", "observed"));
            assert_eq!((f.packets_c2s, f.packets_s2c), (2, 2));
            assert!(f.bytes_c2s > 0 && f.bytes_s2c > 0);
            let t = f.tcp.unwrap();
            assert_eq!(t.state, crate::health::TcpState::Established);
            // Roll-up attribution agrees: this VM served the flow, so it is inbound.
            let s = o.summary(10);
            assert!(s.top_talkers.iter().all(|r| r.direction == "inbound"));
        }

        #[test]
        fn mid_stream_flows_are_guessed_and_marked() {
            // Ephemeral to well-known: the well-known port is the server.
            let o = obs();
            let i = id("vm-a");
            o.ingest(
                &tcp_v4(
                    (G, 51000),
                    (S, 443),
                    5000,
                    1,
                    TCP_ACK | TCP_PSH,
                    100,
                    &[1u8; 10],
                ),
                &meta(&i, 1, Direction::FromGuest),
            );
            let f = one(&o);
            assert_eq!((f.client.port, f.server.port), (51000, 443));
            assert_eq!((f.direction, f.oriented), ("outbound", "guessed"));
            assert!(f.tcp.unwrap().midstream);

            // First seen from the server side (the reply of a flow that began before observation).
            let o = obs();
            o.ingest(
                &tcp_v4(
                    (S, 443),
                    (G, 51000),
                    9000,
                    5010,
                    TCP_ACK | TCP_PSH,
                    100,
                    &[1u8; 10],
                ),
                &meta(&i, 1, Direction::FromGuest),
            );
            let f = one(&o);
            assert_eq!((f.client.port, f.server.port), (51000, 443));
            assert_eq!((f.direction, f.oriented), ("inbound", "guessed"));

            // Neither port well-known: the lower one serves.
            let o = obs();
            o.ingest(
                &tcp_v4((G, 9001), (S, 9000), 1, 1, TCP_ACK, 100, b""),
                &meta(&i, 1, Direction::FromGuest),
            );
            let f = one(&o);
            assert_eq!(
                (f.client.port, f.server.port, f.oriented),
                (9001, 9000, "guessed")
            );

            // Equal ports: the sender is called the client.
            let o = obs();
            o.ingest(
                &tcp_v4((G, 7000), (S, 7000), 1, 1, TCP_ACK, 100, b""),
                &meta(&i, 1, Direction::FromGuest),
            );
            assert_eq!(one(&o).direction, "outbound");

            // UDP has no handshake: first packet decides and says so.
            let o = obs();
            o.ingest(
                &udp_v4((G, 40000), (NS, 53), b"q"),
                &meta(&i, 1, Direction::FromGuest),
            );
            let f = one(&o);
            assert_eq!((f.direction, f.oriented), ("outbound", "first_packet"));
        }

        #[test]
        fn fin_both_ways_closes_and_stops_being_active() {
            let o = obs();
            let i = id("vm-a");
            let (c, s) = ((G, 40000), (S, 5201));
            let from = |t: u64, f: Vec<u8>| o.ingest(&f, &meta(&i, t, Direction::FromGuest));
            let to = |t: u64, f: Vec<u8>| o.ingest(&f, &meta(&i, t, Direction::ToGuest));
            from(1, tcp_v4(c, s, 100, 0, TCP_SYN, 65535, b""));
            to(2, tcp_v4(s, c, 900, 101, TCP_SYN | TCP_ACK, 65535, b""));
            from(3, tcp_v4(c, s, 101, 901, TCP_ACK, 65535, b""));
            from(
                4,
                tcp_v4(c, s, 101, 901, TCP_ACK | TCP_PSH, 65535, &[9u8; 500]),
            );
            to(
                5,
                tcp_v4(s, c, 901, 601, TCP_ACK | TCP_PSH, 65535, &[8u8; 300]),
            );
            from(6, tcp_v4(c, s, 601, 1201, TCP_FIN | TCP_ACK, 65535, b""));
            let mid = one(&o);
            assert!(mid.active, "one FIN is not a close");
            assert_eq!(
                mid.tcp.as_ref().unwrap().state,
                crate::health::TcpState::Closing
            );
            to(7, tcp_v4(s, c, 1201, 602, TCP_FIN | TCP_ACK, 65535, b""));
            from(8, tcp_v4(c, s, 602, 1202, TCP_ACK, 65535, b""));
            let f = one(&o);
            assert!(!f.active, "{f:?}");
            assert_eq!(f.close_reason, Some(CloseReason::Fin));
            assert_eq!(
                f.tcp.as_ref().unwrap().state,
                crate::health::TcpState::Closed
            );
            assert!(f.bytes_s2c > 0 && f.bytes_c2s > f.bytes_s2c - 1);
        }

        #[test]
        fn reset_closes_the_flow() {
            let o = obs();
            let i = id("vm-a");
            let (c, s) = ((G, 40000), (S, 5201));
            o.ingest(
                &tcp_v4(c, s, 100, 0, TCP_SYN, 65535, b""),
                &meta(&i, 1, Direction::FromGuest),
            );
            o.ingest(
                &tcp_v4(s, c, 0, 101, TCP_RST | TCP_ACK, 0, b""),
                &meta(&i, 2, Direction::ToGuest),
            );
            let f = one(&o);
            assert!(!f.active);
            assert_eq!(f.close_reason, Some(CloseReason::Reset));
            assert_eq!(f.tcp.unwrap().reset_by, Some("server"));
        }

        /// A guest-to-guest connection through a switch-level tap: every frame is seen once, attributed to its
        /// sender, and each VM must still get its own complete and correctly oriented view.
        fn guest_to_guest(o: &Observer) -> usize {
            let ia = Identity::new("vm-a", "net1", "private", MAC_A);
            let ib = Identity::new("vm-b", "net1", "private", MAC_B);
            let (ca, sb) = ((A, 60856), (B, 5201));
            let mut n = 0;
            let mut t = 1_000u64;
            let mut send = |from: &Arc<Identity>, f: Vec<u8>| {
                t += 10;
                n += 1;
                o.ingest(&f, &meta(from, t, Direction::FromGuest));
            };
            let a2b = |f: Vec<u8>| l2(f, MAC_B, MAC_A);
            let b2a = |f: Vec<u8>| l2(f, MAC_A, MAC_B);
            send(&ia, a2b(tcp_v4(ca, sb, 100, 0, TCP_SYN, 65535, b"")));
            send(
                &ib,
                b2a(tcp_v4(sb, ca, 900, 101, TCP_SYN | TCP_ACK, 65535, b"")),
            );
            send(&ia, a2b(tcp_v4(ca, sb, 101, 901, TCP_ACK, 65535, b"")));
            send(
                &ia,
                a2b(tcp_v4(
                    ca,
                    sb,
                    101,
                    901,
                    TCP_ACK | TCP_PSH,
                    65535,
                    &[1u8; 1000],
                )),
            );
            send(&ib, b2a(tcp_v4(sb, ca, 901, 1101, TCP_ACK, 65535, b"")));
            send(
                &ia,
                a2b(tcp_v4(ca, sb, 1101, 901, TCP_FIN | TCP_ACK, 65535, b"")),
            );
            send(
                &ib,
                b2a(tcp_v4(sb, ca, 901, 1102, TCP_FIN | TCP_ACK, 65535, b"")),
            );
            send(&ia, a2b(tcp_v4(ca, sb, 1102, 902, TCP_ACK, 65535, b"")));
            n
        }

        #[test]
        fn guest_to_guest_gives_each_vm_its_own_correct_view() {
            let o = obs();
            let n = guest_to_guest(&o);
            let a = view(&o, "vm-a");
            let b = view(&o, "vm-b");
            assert_eq!((a.client.port, a.server.port), (60856, 5201));
            assert_eq!((b.client.port, b.server.port), (60856, 5201));
            assert_eq!((a.direction, a.oriented), ("outbound", "observed"));
            assert_eq!((b.direction, b.oriented), ("inbound", "observed"));
            // vm-b is learned as a sender only with its SYN-ACK, so it never saw the SYN addressed to it: a
            // guest's received frames are attributed once the observer has seen that guest send.
            for (v, c2s) in [(&a, 5), (&b, 4)] {
                assert_eq!((v.packets_c2s, v.packets_s2c), (c2s, 3), "{}", v.vm_id);
                assert!(v.bytes_s2c > 0 && v.bytes_c2s > 1000);
                assert!(!v.active, "{}: both FINs seen, so closed", v.vm_id);
                assert_eq!(v.close_reason, Some(CloseReason::Fin));
            }
            // The observer-wide counters count each frame once.
            assert_eq!(o.counters().frames, n as u64);
            // Per-VM roll-ups: the client VM sees outbound traffic, the server VM inbound.
            let s = o.summary(10_000);
            assert!(
                s.top_talkers
                    .iter()
                    .any(|r| r.vm_id == "vm-a" && r.direction == "outbound")
            );
            assert!(
                s.top_talkers
                    .iter()
                    .any(|r| r.vm_id == "vm-b" && r.direction == "inbound")
            );
            assert!(
                !s.top_talkers
                    .iter()
                    .any(|r| r.vm_id == "vm-b" && r.direction == "outbound")
            );
        }

        #[test]
        fn without_mirroring_each_vm_sees_only_what_it_sent() {
            let o = Observer::new(ObserverConfig {
                mirror_peer_views: false,
                ..ObserverConfig::default()
            });
            guest_to_guest(&o);
            let b = view(&o, "vm-b");
            // Still oriented correctly from its own SYN-ACK, but it only ever sees its own half.
            assert_eq!((b.direction, b.oriented), ("inbound", "observed"));
            assert_eq!(b.packets_c2s, 0);
            assert!(b.active);
        }
    }
}
