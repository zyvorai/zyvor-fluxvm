// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The observer: one `ingest(frame, meta)` entry point that parses, tracks flows and health, extracts L7
//! metadata (redacted), feeds the roll-ups and the capture ring, and publishes events.
//!
//! State lives behind one mutex; ingestion is a single bounded pass per frame, and every collection it touches
//! has a hard cap (flows, events, DNS ring, closed-flow history, roll-up keys and windows, capture sessions).

use crate::capture::{CaptureError, CaptureManager, CaptureRequest, CaptureStatus};
use crate::export::{
    CounterView, DnsRecord, DropRecord, Endpoint, EventKind, FlowView, HttpRecord, NetEvent, RedRow, SCHEMA_VERSION,
    Summary, TlsRecord, VmSummary,
};
use crate::flow::{
    Direction, DropReason, Flow, FlowConfig, FlowKey, FlowL7, FlowTable, FrameMeta, Identity, Verdict, parse_proto,
    proto_name,
};
use crate::health::{CloseReason, Dir, Health, HealthDelta, IcmpHealth, TcpHealth};
use crate::l7::{self, Http, Redaction};
use crate::packet::{self, IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_TCP, L4, Net, Packet, TCP_SYN, TCP_ACK};
use crate::rollup::{Red, Rollup, RollupConfig, RollupKey, Sample};
use crate::source::{TapStats, now_us};
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
    clock_us: u64,
    last_tick_us: u64,
}

pub struct Observer {
    inner: Mutex<Inner>,
    tap_stats: Mutex<Option<Arc<TapStats>>>,
}

fn ep(a: (IpAddr, u16)) -> Endpoint {
    Endpoint { ip: a.0.to_string(), port: a.1 }
}

impl Inner {
    fn finish(&mut self, now: u64, flow: Flow, reason: CloseReason) {
        let view = FlowView::from_flow(&flow, Some(reason));
        if self.closed.len() >= self.cfg.closed_flows.max(1) {
            self.closed.pop_front();
        }
        self.closed.push_back(Closed { view: view.clone(), at_us: now });
        self.events.emit(
            now,
            &flow.identity,
            EventKind::FlowClosed { reason, flow: Box::new(view) },
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
        self.flows.due_active(now, |f| snaps.push(FlowView::from_flow(f, None)));
        for v in snaps {
            let id = Identity {
                vm_id: v.vm_id.as_str().into(),
                network: v.network.as_str().into(),
                nic: v.nic.as_str().into(),
                mac: [0; 6],
            };
            self.events.emit(now, &id, EventKind::FlowUpdate { flow: Box::new(v) });
        }
        self.rollup.advance(now);
        let max_age = self.cfg.closed_max_age_us;
        while self.closed.front().is_some_and(|c| now.saturating_sub(c.at_us) > max_age) {
            self.closed.pop_front();
        }
    }

    fn ingest(&mut self, frame: &[u8], meta: &FrameMeta) {
        let now = meta.ts_us;
        self.counters.frames += 1;
        self.counters.bytes += frame.len() as u64;
        if let Some(r) = meta.verdict.reason() {
            self.counters.drops[r.index()] += 1;
        }
        self.tick(now);

        let Some(pkt) = packet::parse(frame) else {
            self.counters.malformed += 1;
            return;
        };
        if pkt.truncated {
            self.counters.truncated += 1;
        }
        if self.capture.has_active() {
            self.capture.offer(&meta.identity.vm_id, now, frame, &pkt);
        }
        let ip = match &pkt.net {
            Net::Ip(ip) => ip.clone(),
            Net::Arp(_) => {
                self.counters.arp += 1;
                return;
            }
            Net::None => {
                self.counters.non_ip += 1;
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
        let (flow, created) = self.flows.get_or_insert(
            &key,
            |id| Flow {
                id,
                identity,
                proto,
                client: src,
                server: dst,
                guest_is_client: from_guest,
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
        let dir = if flow.client == src { Dir::ClientToServer } else { Dir::ServerToClient };
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
                direction: if flow.guest_is_client { "outbound" } else { "inbound" },
            });
        }
        samples.push(Sample::Packet { bytes: frame.len() as u64 });

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
                    reason: meta.verdict.reason().map(DropReason::name).unwrap_or("other"),
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
        if self.cfg.l7 && !pkt.payload.is_empty() && !delta.retransmit {
            let red = &self.cfg.redaction;
            inspect_l7(flow, now, &pkt, red, &mut samples, &mut events, &mut self.dns);
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

/// Parsing of L7 metadata for one packet; updates the flow's state and appends samples/events.
#[allow(clippy::too_many_arguments)]
fn inspect_l7(
    flow: &mut Flow,
    now: u64,
    pkt: &Packet<'_>,
    red: &Redaction,
    samples: &mut Vec<Sample>,
    events: &mut Vec<EventKind>,
    dns_ring: &mut VecDeque<DnsRecord>,
) {
    let payload = pkt.payload;
    let (sport, dport) = pkt.ports();
    let dns = match &pkt.l4 {
        L4::Udp(_) if sport == 53 || dport == 53 => l7::parse_dns(payload).map(|m| (m, false)),
        L4::Tcp(_) if sport == 53 || dport == 53 => l7::parse_dns_tcp(payload).map(|m| (m, true)),
        _ => None,
    };
    if let Some((m, over_tcp)) = dns {
        flow.l7.app = Some("dns");
        let qname = red.apply(red.dns_qname, &m.qname);
        flow.l7.dns_last_qname = qname.clone();
        let mut latency = None;
        if m.response {
            flow.l7.dns_responses += 1;
            if let Some(i) = flow.l7.dns_pending.iter().position(|(id, _)| *id == m.id) {
                let (_, sent) = flow.l7.dns_pending.remove(i);
                latency = Some(now.saturating_sub(sent));
            }
            samples.push(Sample::DnsResponse { error: m.rcode != 0, latency_us: latency });
        } else {
            flow.l7.dns_queries += 1;
            if flow.l7.dns_pending.len() >= 16 {
                flow.l7.dns_pending.remove(0);
            }
            flow.l7.dns_pending.push((m.id, now));
            samples.push(Sample::DnsQuery);
        }
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
            rcode: m.response.then(|| l7::dns_rcode_name(m.rcode)),
            latency_us: latency,
            answers: if red.dns_answers { m.addrs.iter().map(|a| a.to_string()).collect() } else { Vec::new() },
        };
        dns_ring.push_back(rec.clone());
        events.push(EventKind::Dns(rec));
        return;
    }

    match &pkt.l4 {
        L4::Udp(_) => {
            if payload.len() >= 1200 && payload[0] & 0xc0 == 0xc0 {
                if let Some(q) = l7::detect_quic_initial(payload) {
                    if flow.l7.quic_version.is_none() {
                        flow.l7.app = Some("quic");
                        flow.l7.quic_version = Some(q.version);
                        events.push(EventKind::Quic {
                            flow_id: flow.id,
                            server: ep(flow.server),
                            version: q.version,
                            dcid_len: q.dcid_len,
                        });
                    }
                }
            }
        }
        L4::Tcp(_) => {
            if payload.len() >= 6 && payload[0] == 0x16 && payload[5] == 1 {
                if flow.l7.ja3.is_none() && flow.l7.sni.is_none() {
                    if let Some(h) = l7::parse_client_hello(payload) {
                        flow.l7.app = Some("tls");
                        flow.l7.sni = h.sni.as_deref().and_then(|s| red.apply(red.tls_sni, s));
                        if red.fingerprints {
                            flow.l7.ja3 = Some(h.ja3());
                            flow.l7.ja4 = Some(h.ja4());
                        } else {
                            // Mark the hello as seen so it is not re-parsed for every segment.
                            flow.l7.ja3 = Some(String::new());
                        }
                        flow.l7.alpn = h
                            .alpn
                            .first()
                            .and_then(|a| std::str::from_utf8(a).ok())
                            .map(str::to_string);
                        samples.push(Sample::TlsHello);
                        events.push(EventKind::Tls(TlsRecord {
                            flow_id: flow.id,
                            server: ep(flow.server),
                            sni: flow.l7.sni.clone(),
                            ja3: flow.l7.ja3.clone().filter(|s| !s.is_empty()),
                            ja4: flow.l7.ja4.clone(),
                            alpn: flow.l7.alpn.clone(),
                        }));
                    }
                }
            } else if payload[0].is_ascii_uppercase() {
                if let Some(h) = l7::parse_http(payload) {
                    flow.l7.app = Some("http");
                    match h {
                        Http::Request { method, host, version } => {
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
                            }));
                        }
                    }
                }
            }
        }
        _ => {}
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
        let (peer, outbound) = if self.guest_is_client { (self.server.0, true) } else { (self.client, false) };
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
                events: Events { ring: VecDeque::new(), next_seq: 1, cap: cfg.event_ring },
                dns: VecDeque::new(),
                closed: VecDeque::new(),
                capture: CaptureManager::new(cfg.capture_sessions),
                counters: Counters::default(),
                vm_totals: HashMap::new(),
                clock_us: 0,
                last_tick_us: 0,
                cfg,
            }),
            tap_stats: Mutex::new(None),
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

    /// Ages flows and rotates windows. Call periodically when traffic may be idle (the tap pump does).
    pub fn tick(&self, now_us: u64) {
        self.lock().tick(now_us);
    }

    pub fn set_tap_stats(&self, s: Arc<TapStats>) {
        *self.tap_stats.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }

    pub fn counters(&self) -> Counters {
        self.lock().counters.clone()
    }

    pub fn summary(&self, now_us: u64) -> Summary {
        let tap = self.tap_stats.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
            drops_by_reason: DropReason::ALL.iter().map(|r| (r.name().to_string(), c.drops[r.index()])).collect(),
            tap_enqueued: tap.as_ref().map_or(0, |t| t.enqueued.load(Ordering::Relaxed)),
            tap_dropped: tap.as_ref().map_or(0, |t| t.dropped.load(Ordering::Relaxed)),
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
        let since = g.rollup.history().next().map_or(g.rollup.current().start_us, |w| w.start_us);
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
            out.extend(g.closed.iter().map(|c| c.view.clone()).filter(|v| q.matches(v)));
        }
        out.sort_by(|a, b| b.last_seen_us.cmp(&a.last_seen_us));
        out.truncate(limit);
        out
    }

    /// Recent DNS records, newest first.
    pub fn dns(&self, vm: Option<&str>, allowed: Option<&HashSet<String>>, limit: usize) -> Vec<DnsRecord> {
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
            .chain(std::iter::once((g.rollup.current().start_us, g.rollup.current().end_us, conv(g.rollup.current()))))
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
        FrameMeta { identity: id.clone(), ts_us, direction, verdict: Verdict::Allowed }
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
            let d = if from_guest { Direction::FromGuest } else { Direction::ToGuest };
            o.ingest(&f, &meta(&i, ts, d));
        };
        send(t0, true, tcp_v4(c, s, 100, 0, TCP_SYN, 65535, b""));
        send(t0 + 20_000, false, tcp_v4(s, c, 900, 101, TCP_SYN | TCP_ACK, 65535, b""));
        send(t0 + 20_100, true, tcp_v4(c, s, 101, 901, TCP_ACK, 65535, b""));
        let req = b"GET /private/path HTTP/1.1\r\nHost: Example.org\r\n\r\n";
        send(t0 + 30_000, true, tcp_v4(c, s, 101, 901, TCP_ACK | TCP_PSH, 65535, req));
        send(t0 + 55_000, false, tcp_v4(s, c, 901, 101 + req.len() as u32, TCP_ACK, 65535, b""));
        // Retransmit of the request.
        send(t0 + 300_000, true, tcp_v4(c, s, 101, 901, TCP_ACK | TCP_PSH, 65535, req));
        send(t0 + 310_000, false, tcp_v4(s, c, 901, 101, TCP_ACK | TCP_PSH, 65535, b"HTTP/1.1 503 x\r\n\r\n"));
        send(t0 + 320_000, true, tcp_v4(c, s, 101 + req.len() as u32, 919, TCP_RST, 0, b""));

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
        assert_eq!(tcp.rtt.samples, 2, "handshake plus one data RTT; the retransmitted data is not sampled");
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
        let json = serde_json::to_string(&(o.flows(&FlowQuery::default()), o.events_since(0, None, None, 1000).0))
            .unwrap();
        assert!(!json.contains("private"));
    }

    #[test]
    fn closed_flow_is_exported_after_linger_and_new_syn_restarts() {
        let o = Observer::new(ObserverConfig {
            flow: FlowConfig { closed_linger_us: 2_000_000, ..FlowConfig::default() },
            ..ObserverConfig::default()
        });
        let i = id("vm-a");
        let c = (G, 40001);
        let s = (S, 443);
        let m = |ts| meta(&i, ts, Direction::FromGuest);
        o.ingest(&tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""), &m(1_000_000));
        o.ingest(&tcp_v4(c, s, 2, 0, TCP_FIN | TCP_ACK, 100, b""), &m(1_100_000));
        o.ingest(&tcp_v4(s, c, 9, 3, TCP_FIN | TCP_ACK, 100, b""), &m(1_200_000));
        assert_eq!(o.flows(&FlowQuery::default()).len(), 1);
        // A new SYN on the same 5-tuple is a new flow; the old one moves to history.
        o.ingest(&tcp_v4(c, s, 5000, 0, TCP_SYN, 100, b""), &m(1_300_000));
        let all = o.flows(&FlowQuery { include_closed: true, ..FlowQuery::default() });
        assert_eq!(all.len(), 2);
        assert_eq!(all.iter().filter(|f| f.close_reason == Some(CloseReason::Fin)).count(), 1);
        // Idle aging finally removes the second flow, via tick.
        o.tick(1_300_000 + 400_000_000);
        assert!(o.flows(&FlowQuery::default()).is_empty());
        let ev = o.events_since(0, None, None, 1000).0;
        assert_eq!(ev.iter().filter(|e| e.kind.name() == "flow_closed").count(), 2);
    }

    #[test]
    fn dns_latency_and_redaction() {
        let o = obs();
        let i = id("vm-a");
        let c = (G, 5353);
        let r = (NS, 53);
        let q = fixtures::dns_query(0x1234, "Secret.Internal.Example.com", 1);
        let a = fixtures::dns_response(0x1234, "secret.internal.example.com", 1, 0, &[[10, 1, 2, 3]]);
        o.ingest(&udp_v4(c, r, &q), &meta(&i, 10_000_000, Direction::FromGuest));
        o.ingest(&udp_v4(r, c, &a), &meta(&i, 10_004_500, Direction::ToGuest));
        let nx = fixtures::dns_response(0x9999, "nope.example.com", 1, 3, &[]);
        o.ingest(&udp_v4(r, c, &nx), &meta(&i, 10_005_000, Direction::ToGuest));

        let d = o.dns(None, None, 10);
        assert_eq!(d.len(), 3);
        let resp = d.iter().find(|r| r.response && r.id == 0x1234).unwrap();
        assert_eq!(resp.latency_us, Some(4_500));
        assert_eq!(resp.qname.as_deref(), Some("secret.internal.example.com"));
        assert_eq!(resp.answers, vec!["10.1.2.3"]);
        let s = o.summary(10_006_000);
        assert_eq!((s.per_vm[0].red.dns_queries, s.per_vm[0].red.dns_errors), (1, 1));
        assert_eq!(s.per_vm[0].red.dns_latency_sum_us, 4_500);

        // Strict redaction drops names and answers before storage.
        let o = Observer::new(ObserverConfig { redaction: Redaction::strict(), ..ObserverConfig::default() });
        o.ingest(&udp_v4(c, r, &q), &meta(&i, 1, Direction::FromGuest));
        o.ingest(&udp_v4(r, c, &a), &meta(&i, 2, Direction::ToGuest));
        let json = serde_json::to_string(&(o.dns(None, None, 10), o.flows(&FlowQuery::default()))).unwrap();
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
        o.ingest(&tcp_v4(c, s, 1, 0, TCP_SYN, 100, b""), &meta(&i, 1, Direction::FromGuest));
        o.ingest(&tcp_v4(c, s, 2, 1, TCP_ACK | TCP_PSH, 100, &hello), &meta(&i, 2, Direction::FromGuest));
        let f = &o.flows(&FlowQuery::default())[0];
        assert_eq!(f.sni.as_deref(), Some("api.example.com"));
        assert_eq!(f.app, Some("tls"));
        assert_eq!(f.ja3.as_ref().map(String::len), Some(32));
        assert!(f.ja4.as_ref().unwrap().starts_with("t13d0203h2_"), "{:?}", f.ja4);
        assert_eq!(o.summary(3).per_vm[0].red.tls_handshakes, 1);

        // Hashing keeps correlation without the name.
        let o = Observer::new(ObserverConfig {
            redaction: Redaction { tls_sni: Redact::Hash, ..Redaction::default() },
            ..ObserverConfig::default()
        });
        o.ingest(&tcp_v4(c, s, 2, 1, TCP_ACK | TCP_PSH, 100, &hello), &meta(&i, 2, Direction::FromGuest));
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
        let flows = o.flows(&FlowQuery { verdict: Some("denied".into()), ..FlowQuery::default() });
        assert_eq!(flows.len(), 1);
        assert_eq!((flows[0].verdict, flows[0].drop_reason, flows[0].dropped_packets), ("denied", Some("policy"), 2));
        let s = o.summary(6);
        let policy = s.counters.drops_by_reason.iter().find(|(r, _)| r == "policy").unwrap().1;
        assert_eq!(policy, 2);
        let drops = o.events_since(0, None, None, 100).0.iter().filter(|e| e.kind.name() == "drop").count();
        assert_eq!(drops, 1, "one drop event per flow, not per packet");
    }

    #[test]
    fn flow_table_cap_holds_under_a_scan() {
        let o = Observer::new(ObserverConfig {
            flow: FlowConfig { max_flows: 100, ..FlowConfig::default() },
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
        assert!(o.events_since(0, None, None, 100_000).0.len() <= 4096, "event ring is bounded");
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_observer() {
        let o = obs();
        let i = id("vm-a");
        let mut s = 0xdead_beef_u64;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 24) as u32
        };
        let seeds = [
            tcp_v4((G, 1), (S, 53), 1, 1, TCP_ACK, 1, &fixtures::dns_query(1, "a.b", 1)),
            udp_v4((G, 1), (NS, 53), &fixtures::dns_response(1, "a.b", 1, 0, &[[1, 1, 1, 1]])),
            tcp_v4(
                (G, 1),
                (S, 443),
                1,
                1,
                TCP_ACK,
                1,
                &fixtures::client_hello(0x0303, &[0x1301], &[fixtures::ext_sni("x")]),
            ),
            tcp_v4((G, 1), (S, 80), 1, 1, TCP_ACK, 1, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
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
            o.ingest(&udp_v4((G, 1), (S, 2), b"x"), &meta(&i, 1, Direction::FromGuest));
        }
        let only_a: HashSet<String> = ["vm-a".to_string()].into();
        let q = FlowQuery { allowed_vms: Some(only_a.clone()), ..FlowQuery::default() };
        let flows = o.flows(&q);
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].vm_id, "vm-a");

        let (first, next) = o.events_since(0, Some(&only_a), None, 100);
        assert!(!first.is_empty() && first.iter().all(|e| e.vm_id == "vm-a"));
        let (again, _) = o.events_since(next, Some(&only_a), None, 100);
        assert!(again.is_empty(), "resume point skips what was delivered");
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
                filter: CaptureFilter { port: Some(53), ..Default::default() },
                ..CaptureRequest::default()
            },
        )
        .unwrap();
        let ts = now_us();
        o.ingest(&udp_v4((G, 5000), (NS, 53), b"q"), &meta(&i, ts, Direction::FromGuest));
        o.ingest(&udp_v4((G, 5000), (NS, 80), b"q"), &meta(&i, ts, Direction::FromGuest));
        let pcap = o.capture_pcap("vm-a").unwrap();
        assert_eq!(&pcap[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(o.capture_status()[0].packets, 1);
        assert!(o.capture_disable("vm-a"));
    }
}
