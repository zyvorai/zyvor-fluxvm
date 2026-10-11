// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The flow table: one record per bidirectional 5-tuple per guest NIC, with packet/byte accounting for both
//! directions, aging, a hard memory bound, and the verdict the datapath gave the traffic.
//!
//! Time is whatever clock the caller stamps frames with (the observer uses unix microseconds); the table never
//! reads a clock itself, which keeps aging deterministic in tests.

use crate::health::{CloseReason, Health};
use crate::packet::{IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_TCP, IPPROTO_UDP, Mac};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Arc;

/// Who a frame belongs to: the VM, the private network or NAT it attaches to, the NIC and its MAC.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    pub vm_id: Arc<str>,
    pub network: Arc<str>,
    pub nic: Arc<str>,
    pub mac: Mac,
}

impl Identity {
    pub fn new(vm_id: &str, network: &str, nic: &str, mac: Mac) -> Arc<Self> {
        Arc::new(Self {
            vm_id: vm_id.into(),
            network: network.into(),
            nic: nic.into(),
            mac,
        })
    }

    pub fn mac_string(&self) -> String {
        let m = self.mac;
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}

/// Which way a frame crossed the guest NIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The guest transmitted it.
    FromGuest,
    /// The guest is about to receive it.
    ToGuest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    Policy,
    SpoofedSource,
    UnknownDestination,
    Runt,
    Oversized,
    QueueFull,
    Other,
}

impl DropReason {
    pub const ALL: [DropReason; 7] = [
        DropReason::Policy,
        DropReason::SpoofedSource,
        DropReason::UnknownDestination,
        DropReason::Runt,
        DropReason::Oversized,
        DropReason::QueueFull,
        DropReason::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            DropReason::Policy => "policy",
            DropReason::SpoofedSource => "spoofed_source",
            DropReason::UnknownDestination => "unknown_destination",
            DropReason::Runt => "runt",
            DropReason::Oversized => "oversized",
            DropReason::QueueFull => "queue_full",
            DropReason::Other => "other",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|r| *r == self)
            .unwrap_or(Self::ALL.len() - 1)
    }
}

/// What the datapath did with a frame. `Denied` is a policy decision; `Dropped` is a datapath failure or a
/// protection (spoofing, no destination).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    #[default]
    Allowed,
    Denied(DropReason),
    Dropped(DropReason),
}

impl Verdict {
    pub fn kind(self) -> &'static str {
        match self {
            Verdict::Allowed => "allowed",
            Verdict::Denied(_) => "denied",
            Verdict::Dropped(_) => "dropped",
        }
    }

    pub fn reason(self) -> Option<DropReason> {
        match self {
            Verdict::Allowed => None,
            Verdict::Denied(r) | Verdict::Dropped(r) => Some(r),
        }
    }

    pub fn is_allowed(self) -> bool {
        self == Verdict::Allowed
    }
}

/// Everything the observer needs to know about a frame besides its bytes.
#[derive(Debug, Clone)]
pub struct FrameMeta {
    pub identity: Arc<Identity>,
    /// Unix time in microseconds.
    pub ts_us: u64,
    pub direction: Direction,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub vm: Arc<str>,
    pub nic: Arc<str>,
    pub proto: u8,
    /// Endpoints in canonical (sorted) order so both directions map to one key.
    pub a: (IpAddr, u16),
    pub b: (IpAddr, u16),
}

impl FlowKey {
    pub fn new(id: &Identity, proto: u8, src: (IpAddr, u16), dst: (IpAddr, u16)) -> Self {
        let (a, b) = if src <= dst { (src, dst) } else { (dst, src) };
        Self {
            vm: id.vm_id.clone(),
            nic: id.nic.clone(),
            proto,
            a,
            b,
        }
    }
}

pub fn proto_name(p: u8) -> String {
    match p {
        IPPROTO_TCP => "tcp".into(),
        IPPROTO_UDP => "udp".into(),
        IPPROTO_ICMP => "icmp".into(),
        IPPROTO_ICMPV6 => "icmpv6".into(),
        n => format!("proto{n}"),
    }
}

pub fn parse_proto(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "tcp" => Some(IPPROTO_TCP),
        "udp" => Some(IPPROTO_UDP),
        "icmp" => Some(IPPROTO_ICMP),
        "icmpv6" | "icmp6" => Some(IPPROTO_ICMPV6),
        other => other
            .strip_prefix("proto")
            .and_then(|n| n.parse().ok())
            .or_else(|| other.parse().ok()),
    }
}

/// Application-layer facts attached to a flow. Names here have already passed through redaction.
#[derive(Debug, Clone, Default)]
pub struct FlowL7 {
    /// Best guess at the application protocol: "dns", "tls", "http", "quic".
    pub app: Option<&'static str>,
    pub sni: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub alpn: Option<String>,
    pub http_host: Option<String>,
    pub http_requests: u64,
    pub http_responses: u64,
    pub dns_queries: u64,
    pub dns_responses: u64,
    pub dns_last_qname: Option<String>,
    pub quic_version: Option<u32>,
    /// Outstanding DNS queries awaiting a response: (transaction id, send time).
    pub dns_pending: Vec<(u16, u64)>,
    /// Negotiated TLS version and cipher from the ServerHello ("TLS1.3", "TLS_AES_128_GCM_SHA256").
    pub tls_version: Option<String>,
    pub tls_cipher: Option<String>,
    pub ja3s: Option<String>,
    pub ja4s: Option<String>,
    /// "dot", "doh" or "doq" when the flow is recognised as encrypted DNS.
    pub encrypted_dns: Option<&'static str>,
    /// SYN sequence numbers per direction (client-to-server, server-to-client), used as reassembly bases.
    pub isn: [Option<u32>; 2],
    /// Payload has been seen in this direction.
    pub data_seen: [bool; 2],
    /// Nothing more to learn from this direction's first bytes (not TLS/h2, or already parsed).
    pub dir_done: [bool; 2],
    /// Reassembly state for the first bytes of a TLS or HTTP/2 connection; present only while needed.
    pub streams: Option<Box<crate::streams::FlowStreams>>,
    /// Both directions finished: do not start reassembly again.
    pub streams_done: bool,
    /// Initial-packet decoding state for a QUIC flow.
    pub quic: Option<Box<crate::quic::QuicFlow>>,
}

/// How sure the observer is about which endpoint of a flow is the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    /// A TCP handshake segment decided it: a SYN without ACK is from the client, a SYN+ACK from the server.
    Observed,
    /// No handshake exists for the protocol (UDP, ICMP): the sender of the first packet seen is the client.
    FirstPacket,
    /// A TCP flow first seen mid-stream: the server is the endpoint with the well-known (else lower) port.
    Guessed,
}

impl Orientation {
    pub fn name(self) -> &'static str {
        match self {
            Orientation::Observed => "observed",
            Orientation::FirstPacket => "first_packet",
            Orientation::Guessed => "guessed",
        }
    }
}

/// Decides client and server for a new TCP flow from the first segment seen (`src` sent it).
///
/// * SYN without ACK: the sender is the client.
/// * SYN with ACK: the sender is the server (the SYN predates the observation, or went the other way).
/// * Anything else (first seen mid-stream): the server is the endpoint with the well-known port (below 1024),
///   else the one outside the ephemeral range (32768 and up), else the lower port; equal ports fall back to the
///   sender as client. This is a guess and is reported as [`Orientation::Guessed`].
///
/// Returns `(client, server, how)`.
pub fn orient_tcp(
    syn: bool,
    ack: bool,
    src: (IpAddr, u16),
    dst: (IpAddr, u16),
) -> ((IpAddr, u16), (IpAddr, u16), Orientation) {
    match (syn, ack) {
        (true, false) => (src, dst, Orientation::Observed),
        (true, true) => (dst, src, Orientation::Observed),
        _ => {
            let src_is_server = if src.1 == dst.1 {
                false
            } else {
                let wk = |p: u16| p < 1024;
                let eph = |p: u16| p >= 32768;
                match (wk(src.1), wk(dst.1)) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => match (eph(src.1), eph(dst.1)) {
                        (false, true) => true,
                        (true, false) => false,
                        _ => src.1 < dst.1,
                    },
                }
            };
            if src_is_server {
                (dst, src, Orientation::Guessed)
            } else {
                (src, dst, Orientation::Guessed)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Flow {
    pub id: u64,
    pub identity: Arc<Identity>,
    pub proto: u8,
    /// The initiating endpoint: the sender of the SYN for TCP when the handshake was seen, otherwise see
    /// [`Self::oriented`].
    pub client: (IpAddr, u16),
    pub server: (IpAddr, u16),
    pub oriented: Orientation,
    /// The guest is the client (the flow is outbound from the VM).
    pub guest_is_client: bool,
    pub vlan: Option<u16>,
    pub first_seen_us: u64,
    pub last_seen_us: u64,
    /// Last time this flow was exported as an active-timeout snapshot.
    pub last_export_us: u64,
    /// Packets and bytes (L2 frame bytes) indexed by [`crate::health::Dir`].
    pub packets: [u64; 2],
    pub bytes: [u64; 2],
    pub verdict: Verdict,
    pub dropped_packets: u64,
    pub health: Health,
    pub l7: FlowL7,
    pub closed: Option<CloseReason>,
    pub closed_at_us: u64,
}

impl Flow {
    pub fn total_packets(&self) -> u64 {
        self.packets[0] + self.packets[1]
    }

    pub fn total_bytes(&self) -> u64 {
        self.bytes[0] + self.bytes[1]
    }
}

#[derive(Debug, Clone)]
pub struct FlowConfig {
    /// Hard cap on live flows; the least-recently-seen flow is evicted past it.
    pub max_flows: usize,
    pub tcp_idle_us: u64,
    pub udp_idle_us: u64,
    pub other_idle_us: u64,
    /// How long a FIN/RST-closed TCP flow lingers to absorb trailing packets before it is exported.
    pub closed_linger_us: u64,
    /// Long-lived flows are exported as a snapshot at this interval while staying in the table.
    pub active_timeout_us: u64,
}

impl Default for FlowConfig {
    fn default() -> Self {
        const S: u64 = 1_000_000;
        Self {
            max_flows: 65_536,
            tcp_idle_us: 300 * S,
            udp_idle_us: 60 * S,
            other_idle_us: 30 * S,
            closed_linger_us: 5 * S,
            active_timeout_us: 300 * S,
        }
    }
}

impl FlowConfig {
    fn idle_for(&self, f: &Flow) -> u64 {
        if f.closed.is_some() {
            return self.closed_linger_us;
        }
        match f.proto {
            IPPROTO_TCP => self.tcp_idle_us,
            IPPROTO_UDP => self.udp_idle_us,
            _ => self.other_idle_us,
        }
    }

    fn min_idle(&self) -> u64 {
        self.tcp_idle_us
            .min(self.udp_idle_us)
            .min(self.other_idle_us)
            .min(self.closed_linger_us)
    }
}

struct Entry {
    flow: Flow,
    tick: u64,
}

/// A bounded flow table with least-recently-seen eviction.
pub struct FlowTable {
    cfg: FlowConfig,
    map: HashMap<FlowKey, Entry>,
    /// tick -> key, oldest first.
    order: BTreeMap<u64, FlowKey>,
    tick: u64,
    next_id: u64,
}

impl FlowTable {
    pub fn new(cfg: FlowConfig) -> Self {
        Self {
            cfg,
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            next_id: 1,
        }
    }

    pub fn config(&self) -> &FlowConfig {
        &self.cfg
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, key: &FlowKey) -> Option<&Flow> {
        self.map.get(key).map(|e| &e.flow)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Flow> {
        self.map.values().map(|e| &e.flow)
    }

    fn touch(&mut self, key: &FlowKey) {
        self.tick += 1;
        if let Some(e) = self.map.get_mut(key) {
            self.order.remove(&e.tick);
            e.tick = self.tick;
            self.order.insert(self.tick, key.clone());
        }
    }

    /// Returns the flow for `key`, creating it with `make` (given a fresh flow id) when absent. Flows evicted to
    /// make room are appended to `evicted`. The bool is true when the flow was created.
    pub fn get_or_insert(
        &mut self,
        key: &FlowKey,
        make: impl FnOnce(u64) -> Flow,
        evicted: &mut Vec<Flow>,
    ) -> (&mut Flow, bool) {
        let created = if self.map.contains_key(key) {
            self.touch(key);
            false
        } else {
            while self.map.len() >= self.cfg.max_flows.max(1) {
                let Some((_, old)) = self.order.pop_first() else {
                    break;
                };
                if let Some(e) = self.map.remove(&old) {
                    evicted.push(e.flow);
                }
            }
            let id = self.next_id;
            self.next_id += 1;
            self.tick += 1;
            self.order.insert(self.tick, key.clone());
            self.map.insert(
                key.clone(),
                Entry {
                    flow: make(id),
                    tick: self.tick,
                },
            );
            true
        };
        let flow = &mut self
            .map
            .get_mut(key)
            .expect("flow was just inserted or found")
            .flow;
        (flow, created)
    }

    pub fn remove(&mut self, key: &FlowKey) -> Option<Flow> {
        let e = self.map.remove(key)?;
        self.order.remove(&e.tick);
        Some(e.flow)
    }

    /// Removes flows idle (or lingering after close) past their timeout and returns them with the reason.
    pub fn expire(&mut self, now_us: u64) -> Vec<(Flow, CloseReason)> {
        let min_idle = self.cfg.min_idle();
        let mut due: Vec<(u64, FlowKey)> = Vec::new();
        for (tick, key) in self.order.iter() {
            let Some(e) = self.map.get(key) else { continue };
            let age = now_us.saturating_sub(e.flow.last_seen_us);
            if age < min_idle {
                // Everything after this was seen more recently still.
                break;
            }
            let reason_closed = e.flow.closed.is_some();
            let timeout = if reason_closed {
                self.cfg.closed_linger_us
            } else {
                self.cfg.idle_for(&e.flow)
            };
            if age >= timeout {
                due.push((*tick, key.clone()));
            }
        }
        let mut out = Vec::with_capacity(due.len());
        for (tick, key) in due {
            self.order.remove(&tick);
            if let Some(e) = self.map.remove(&key) {
                let reason = e.flow.closed.unwrap_or(CloseReason::Idle);
                out.push((e.flow, reason));
            }
        }
        out
    }

    /// Calls `f` for each flow whose last export is older than the active timeout, and stamps it exported.
    pub fn due_active(&mut self, now_us: u64, mut f: impl FnMut(&Flow)) {
        let t = self.cfg.active_timeout_us;
        for e in self.map.values_mut() {
            if now_us.saturating_sub(e.flow.last_export_us.max(e.flow.first_seen_us)) >= t {
                e.flow.last_export_us = now_us;
                f(&e.flow);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::Health;
    use std::net::Ipv4Addr;

    fn id() -> Arc<Identity> {
        Identity::new("vm-1", "default", "eth0", [2, 0, 0, 0, 0, 1])
    }

    fn ep(last: u8, port: u16) -> (IpAddr, u16) {
        (IpAddr::V4(Ipv4Addr::new(10, 0, 0, last)), port)
    }

    fn mk(key_id: &Arc<Identity>, now: u64, proto: u8) -> impl FnOnce(u64) -> Flow {
        let identity = key_id.clone();
        move |id| Flow {
            id,
            identity,
            proto,
            client: ep(1, 1000),
            server: ep(2, 80),
            oriented: Orientation::FirstPacket,
            guest_is_client: true,
            vlan: None,
            first_seen_us: now,
            last_seen_us: now,
            last_export_us: now,
            packets: [0; 2],
            bytes: [0; 2],
            verdict: Verdict::Allowed,
            dropped_packets: 0,
            health: Health::None,
            l7: FlowL7::default(),
            closed: None,
            closed_at_us: 0,
        }
    }

    #[test]
    fn both_directions_share_a_key() {
        let i = id();
        let fwd = FlowKey::new(&i, IPPROTO_TCP, ep(1, 1000), ep(2, 80));
        let rev = FlowKey::new(&i, IPPROTO_TCP, ep(2, 80), ep(1, 1000));
        assert_eq!(fwd, rev);
        let other_proto = FlowKey::new(&i, IPPROTO_UDP, ep(1, 1000), ep(2, 80));
        assert_ne!(fwd, other_proto);
        let other_nic = FlowKey::new(
            &Identity::new("vm-1", "default", "eth1", [2, 0, 0, 0, 0, 3]),
            IPPROTO_TCP,
            ep(1, 1000),
            ep(2, 80),
        );
        assert_ne!(fwd, other_nic);
    }

    #[test]
    fn idle_aging_by_protocol() {
        let mut t = FlowTable::new(FlowConfig {
            udp_idle_us: 10,
            tcp_idle_us: 100,
            ..FlowConfig::default()
        });
        let i = id();
        let mut ev = Vec::new();
        let ku = FlowKey::new(&i, IPPROTO_UDP, ep(1, 1), ep(2, 2));
        let kt = FlowKey::new(&i, IPPROTO_TCP, ep(1, 1), ep(2, 2));
        t.get_or_insert(&ku, mk(&i, 0, IPPROTO_UDP), &mut ev);
        t.get_or_insert(&kt, mk(&i, 0, IPPROTO_TCP), &mut ev);
        assert!(t.expire(5).is_empty());
        let gone = t.expire(10);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].0.proto, IPPROTO_UDP);
        assert_eq!(gone[0].1, CloseReason::Idle);
        assert_eq!(t.len(), 1);
        assert_eq!(t.expire(100).len(), 1);
        assert!(t.is_empty());
    }

    #[test]
    fn lru_cap_evicts_least_recently_seen() {
        let mut t = FlowTable::new(FlowConfig {
            max_flows: 3,
            ..FlowConfig::default()
        });
        let i = id();
        let mut ev = Vec::new();
        let keys: Vec<FlowKey> = (0..4)
            .map(|n| FlowKey::new(&i, IPPROTO_UDP, ep(1, 100 + n), ep(2, 53)))
            .collect();
        for k in &keys[..3] {
            t.get_or_insert(k, mk(&i, 0, IPPROTO_UDP), &mut ev);
        }
        // Touch the first so the second becomes the oldest.
        t.get_or_insert(&keys[0], mk(&i, 0, IPPROTO_UDP), &mut ev);
        assert!(ev.is_empty());
        t.get_or_insert(&keys[3], mk(&i, 0, IPPROTO_UDP), &mut ev);
        assert_eq!(t.len(), 3);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].client, ep(1, 1000));
        assert!(t.get(&keys[1]).is_none());
        assert!(t.get(&keys[0]).is_some());
    }

    #[test]
    fn closed_flows_linger_then_export() {
        let mut t = FlowTable::new(FlowConfig {
            closed_linger_us: 50,
            ..FlowConfig::default()
        });
        let i = id();
        let mut ev = Vec::new();
        let k = FlowKey::new(&i, IPPROTO_TCP, ep(1, 1), ep(2, 2));
        let (f, _) = t.get_or_insert(&k, mk(&i, 1000, IPPROTO_TCP), &mut ev);
        f.closed = Some(CloseReason::Fin);
        assert!(t.expire(1049).is_empty());
        let out = t.expire(1050);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, CloseReason::Fin);
    }

    #[test]
    fn active_timeout_snapshots_without_removing() {
        let mut t = FlowTable::new(FlowConfig {
            active_timeout_us: 100,
            ..FlowConfig::default()
        });
        let i = id();
        let mut ev = Vec::new();
        let k = FlowKey::new(&i, IPPROTO_TCP, ep(1, 1), ep(2, 2));
        t.get_or_insert(&k, mk(&i, 0, IPPROTO_TCP), &mut ev);
        let mut n = 0;
        t.due_active(99, |_| n += 1);
        assert_eq!(n, 0);
        t.due_active(100, |_| n += 1);
        assert_eq!(n, 1);
        t.due_active(150, |_| n += 1);
        assert_eq!(n, 1, "stamped, so not due again until another interval");
        assert_eq!(t.len(), 1);
    }
}
