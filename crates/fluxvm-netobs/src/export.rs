// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Stable, versioned wire types for everything netobs hands to the outside: flow views, events, the summary, a
//! Prometheus text rendering, SSE framing, and (behind the `otlp` feature) an OTLP/JSON metrics mapping.
//!
//! Compatibility rule: fields are only ever added. `schema` carries [`SCHEMA_VERSION`] on every top-level object;
//! it is bumped only for a breaking change. Times are unix microseconds (`*_us`).

use crate::capture::CaptureStatus;
use crate::flow::{Flow, proto_name};
use crate::health::{CloseReason, Health, IcmpHealth, TcpHealthView};
use crate::rollup::Red;
use serde::Serialize;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Endpoint {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowView {
    pub id: u64,
    pub vm_id: String,
    pub network: String,
    pub nic: String,
    pub mac: String,
    pub proto: String,
    pub client: Endpoint,
    pub server: Endpoint,
    /// "outbound" when the guest opened the flow, "inbound" otherwise.
    pub direction: &'static str,
    pub vlan: Option<u16>,
    pub first_seen_us: u64,
    pub last_seen_us: u64,
    pub duration_us: u64,
    pub packets_c2s: u64,
    pub packets_s2c: u64,
    pub bytes_c2s: u64,
    pub bytes_s2c: u64,
    pub verdict: &'static str,
    pub drop_reason: Option<&'static str>,
    pub dropped_packets: u64,
    pub app: Option<&'static str>,
    pub sni: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub alpn: Option<String>,
    pub http_host: Option<String>,
    pub http_requests: u64,
    pub dns_queries: u64,
    pub dns_last_qname: Option<String>,
    pub quic_version: Option<u32>,
    /// Negotiated TLS version and cipher, and JA3S/JA4S, from the ServerHello.
    pub tls_version: Option<String>,
    pub tls_cipher: Option<String>,
    pub ja3s: Option<String>,
    pub ja4s: Option<String>,
    /// "dot", "doh" or "doq" when the flow is recognised as encrypted DNS.
    pub encrypted_dns: Option<&'static str>,
    pub tcp: Option<TcpHealthView>,
    pub icmp: Option<IcmpHealth>,
    pub close_reason: Option<CloseReason>,
    pub active: bool,
}

impl FlowView {
    pub fn from_flow(f: &Flow, close: Option<CloseReason>) -> Self {
        let (tcp, icmp) = match &f.health {
            Health::Tcp(t) => (Some(t.view()), None),
            Health::Icmp(i) => (None, Some(i.clone())),
            Health::None => (None, None),
        };
        Self {
            id: f.id,
            vm_id: f.identity.vm_id.to_string(),
            network: f.identity.network.to_string(),
            nic: f.identity.nic.to_string(),
            mac: f.identity.mac_string(),
            proto: proto_name(f.proto),
            client: Endpoint { ip: f.client.0.to_string(), port: f.client.1 },
            server: Endpoint { ip: f.server.0.to_string(), port: f.server.1 },
            direction: if f.guest_is_client { "outbound" } else { "inbound" },
            vlan: f.vlan,
            first_seen_us: f.first_seen_us,
            last_seen_us: f.last_seen_us,
            duration_us: f.last_seen_us.saturating_sub(f.first_seen_us),
            packets_c2s: f.packets[0],
            packets_s2c: f.packets[1],
            bytes_c2s: f.bytes[0],
            bytes_s2c: f.bytes[1],
            verdict: f.verdict.kind(),
            drop_reason: f.verdict.reason().map(|r| r.name()),
            dropped_packets: f.dropped_packets,
            app: f.l7.app,
            sni: f.l7.sni.clone(),
            ja3: f.l7.ja3.clone(),
            ja4: f.l7.ja4.clone(),
            alpn: f.l7.alpn.clone(),
            http_host: f.l7.http_host.clone(),
            http_requests: f.l7.http_requests,
            dns_queries: f.l7.dns_queries,
            dns_last_qname: f.l7.dns_last_qname.clone(),
            quic_version: f.l7.quic_version,
            tls_version: f.l7.tls_version.clone(),
            tls_cipher: f.l7.tls_cipher.clone(),
            ja3s: f.l7.ja3s.clone(),
            ja4s: f.l7.ja4s.clone(),
            encrypted_dns: f.l7.encrypted_dns,
            tcp,
            icmp,
            close_reason: close,
            active: close.is_none(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DnsRecord {
    pub ts_us: u64,
    pub vm_id: String,
    pub flow_id: u64,
    pub client: Endpoint,
    pub server: Endpoint,
    pub tcp: bool,
    pub id: u16,
    pub response: bool,
    /// Absent when the name was redacted away.
    pub qname: Option<String>,
    pub qtype: String,
    pub rcode: Option<String>,
    pub latency_us: Option<u64>,
    pub answers: Vec<String>,
    /// Record types present in the answer section (`A`, `AAAA`, `CNAME`, `HTTPS`, ...).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub answer_types: Vec<String>,
    /// CNAME targets (redacted like `qname`; empty when answers are not kept).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cnames: Vec<String>,
    /// HTTPS (type 65) service bindings.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub https: Vec<HttpsView>,
    /// EDNS(0) parameters of the message, if it carried an OPT record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edns: Option<EdnsView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EdnsView {
    pub udp_size: u16,
    pub version: u8,
    pub dnssec_ok: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HttpsView {
    pub priority: u16,
    /// The target name (redacted like `qname`); `"."` means the owner name. Absent when redacted away.
    pub target: Option<String>,
    pub alpn: Vec<String>,
    pub port: Option<u16>,
    /// An Encrypted ClientHello configuration is advertised.
    pub ech: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TlsRecord {
    pub flow_id: u64,
    pub server: Endpoint,
    pub sni: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub alpn: Option<String>,
}

/// What the server's hello said: negotiated version and cipher, and the JA3S/JA4S fingerprints. Certificates and
/// anything after the hello are never read.
#[derive(Debug, Clone, Serialize)]
pub struct TlsServerRecord {
    pub flow_id: u64,
    pub server: Endpoint,
    pub version: String,
    pub cipher: String,
    /// The "ServerHello" was a HelloRetryRequest.
    pub hello_retry: bool,
    pub ja3s: Option<String>,
    pub ja4s: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpRecord {
    pub flow_id: u64,
    pub server: Endpoint,
    pub request: bool,
    pub method: Option<&'static str>,
    pub host: Option<String>,
    pub status: Option<u16>,
    /// "1.0", "1.1" or "2".
    pub version: String,
    /// HTTP/2 `:scheme` (requests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// HTTP/2 response media type (parameters removed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DropRecord {
    pub flow_id: u64,
    pub proto: String,
    pub src: Endpoint,
    pub dst: Endpoint,
    pub verdict: &'static str,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    FlowOpened { flow_id: u64, proto: String, client: Endpoint, server: Endpoint, direction: &'static str },
    FlowUpdate { flow: Box<FlowView> },
    FlowClosed { reason: CloseReason, flow: Box<FlowView> },
    Dns(DnsRecord),
    Tls(TlsRecord),
    TlsServer(TlsServerRecord),
    Http(HttpRecord),
    Quic {
        flow_id: u64,
        server: Endpoint,
        version: u32,
        dcid_len: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        scid_len: Option<u8>,
        /// From the decrypted Initial (redacted like a TLS SNI).
        #[serde(skip_serializing_if = "Option::is_none")]
        sni: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        alpn: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ja4: Option<String>,
    },
    Drop(DropRecord),
}

impl EventKind {
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::FlowOpened { .. } => "flow_opened",
            EventKind::FlowUpdate { .. } => "flow_update",
            EventKind::FlowClosed { .. } => "flow_closed",
            EventKind::Dns(_) => "dns",
            EventKind::Tls(_) => "tls",
            EventKind::TlsServer(_) => "tls_server",
            EventKind::Http(_) => "http",
            EventKind::Quic { .. } => "quic",
            EventKind::Drop(_) => "drop",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NetEvent {
    pub schema: u32,
    pub seq: u64,
    pub ts_us: u64,
    pub vm_id: String,
    pub network: String,
    pub nic: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

impl NetEvent {
    /// One Server-Sent Events frame: `id` is the sequence number, so a client can resume with `?since=<id>`.
    pub fn to_sse(&self) -> String {
        let data = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        format!("id: {}\nevent: netobs.{}\ndata: {}\n\n", self.seq, self.kind.name(), data)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CounterView {
    pub frames: u64,
    pub bytes: u64,
    pub malformed: u64,
    pub truncated: u64,
    pub arp: u64,
    pub non_ip: u64,
    pub flows_created: u64,
    pub flows_evicted: u64,
    pub flows_expired: u64,
    pub drops_by_reason: Vec<(String, u64)>,
    pub tap_enqueued: u64,
    pub tap_dropped: u64,
    /// Reassembly: segments dropped over a bound, streams abandoned on the global budget, messages left
    /// incomplete at the prefix limit, evasion-style overlap conflicts, and bytes currently buffered.
    pub reasm_segments_dropped: u64,
    pub reasm_streams_abandoned: u64,
    pub reasm_incomplete: u64,
    pub reasm_conflicts: u64,
    pub reasm_bytes: u64,
    pub encrypted_dns_flows: u64,
    pub quic_hellos: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VmSummary {
    pub vm_id: String,
    pub flows_active: u64,
    pub red: Red,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedRow {
    pub vm_id: String,
    pub peer: String,
    pub port: u16,
    pub proto: String,
    pub direction: &'static str,
    pub red: Red,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub schema: u32,
    pub enabled: bool,
    pub generated_us: u64,
    pub counters: CounterView,
    pub flows_active: u64,
    pub per_vm: Vec<VmSummary>,
    /// Retained window range the rows below cover.
    pub rows_since_us: u64,
    pub top_talkers: Vec<RedRow>,
    pub top_errors: Vec<RedRow>,
    pub capture: Vec<CaptureStatus>,
}

impl Summary {
    pub fn disabled(now_us: u64) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            enabled: false,
            generated_us: now_us,
            counters: CounterView::default(),
            flows_active: 0,
            per_vm: Vec::new(),
            rows_since_us: 0,
            top_talkers: Vec::new(),
            top_errors: Vec::new(),
            capture: Vec::new(),
        }
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Prometheus text exposition (version 0.0.4). Label cardinality is bounded: global counters and per-VM series
/// only; per-peer rows stay in the JSON API.
pub fn render_prometheus(s: &Summary) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let counter = |o: &mut String, name: &str, help: &str, v: u64| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
    };
    let c = &s.counters;
    counter(&mut o, "fluxvm_netobs_frames_total", "Guest frames observed.", c.frames);
    counter(&mut o, "fluxvm_netobs_bytes_total", "Guest frame bytes observed.", c.bytes);
    counter(&mut o, "fluxvm_netobs_malformed_total", "Frames too short to parse.", c.malformed);
    counter(&mut o, "fluxvm_netobs_truncated_total", "Frames with a header cut short or lying lengths.", c.truncated);
    counter(&mut o, "fluxvm_netobs_flows_created_total", "Flows created.", c.flows_created);
    counter(&mut o, "fluxvm_netobs_flows_evicted_total", "Flows evicted because the table was full.", c.flows_evicted);
    counter(&mut o, "fluxvm_netobs_flows_expired_total", "Flows aged out.", c.flows_expired);
    counter(&mut o, "fluxvm_netobs_tap_enqueued_total", "Frames queued by the datapath tap.", c.tap_enqueued);
    counter(
        &mut o,
        "fluxvm_netobs_tap_dropped_total",
        "Frames the tap discarded because its queue was full.",
        c.tap_dropped,
    );
    counter(
        &mut o,
        "fluxvm_netobs_reasm_segments_dropped_total",
        "TCP segments (or QUIC CRYPTO data) dropped because a reassembly bound was hit.",
        c.reasm_segments_dropped,
    );
    counter(
        &mut o,
        "fluxvm_netobs_reasm_streams_abandoned_total",
        "Streams abandoned because the global reassembly budget was exhausted.",
        c.reasm_streams_abandoned,
    );
    counter(
        &mut o,
        "fluxvm_netobs_reasm_incomplete_total",
        "Streams whose first message was still incomplete at the per-stream byte limit.",
        c.reasm_incomplete,
    );
    counter(
        &mut o,
        "fluxvm_netobs_reasm_conflicts_total",
        "Overlapping retransmissions whose bytes differed from the first copy.",
        c.reasm_conflicts,
    );
    counter(&mut o, "fluxvm_netobs_encrypted_dns_flows_total", "Flows recognised as DoT/DoH/DoQ.", c.encrypted_dns_flows);
    counter(&mut o, "fluxvm_netobs_quic_hellos_total", "QUIC ClientHellos decoded from Initial packets.", c.quic_hellos);
    let _ = writeln!(
        o,
        "# HELP fluxvm_netobs_reasm_bytes Bytes currently buffered for stream reassembly.\n# TYPE fluxvm_netobs_reasm_bytes gauge\nfluxvm_netobs_reasm_bytes {}",
        c.reasm_bytes
    );
    let _ = writeln!(
        o,
        "# HELP fluxvm_netobs_flows_active Flows currently tracked.\n# TYPE fluxvm_netobs_flows_active gauge\nfluxvm_netobs_flows_active {}",
        s.flows_active
    );
    let _ = writeln!(
        o,
        "# HELP fluxvm_netobs_frame_drops_total Frames the datapath dropped or denied, by reason.\n# TYPE fluxvm_netobs_frame_drops_total counter"
    );
    for (reason, n) in &c.drops_by_reason {
        let _ = writeln!(o, "fluxvm_netobs_frame_drops_total{{reason=\"{}\"}} {n}", esc(reason));
    }
    let series: &[(&str, &str, fn(&Red) -> u64)] = &[
        ("packets_total", "Packets per VM.", |r| r.packets),
        ("bytes_total", "Frame bytes per VM.", |r| r.bytes),
        ("flows_total", "Flows opened per VM.", |r| r.flows),
        ("tcp_resets_total", "TCP resets per VM.", |r| r.resets),
        ("tcp_retransmits_total", "TCP retransmissions per VM.", |r| r.retransmits),
        ("tcp_zero_window_total", "TCP zero-window episodes per VM.", |r| r.zero_windows),
        ("drops_total", "Dropped or denied frames attributed to flows, per VM.", |r| r.drops),
        ("dns_queries_total", "DNS queries per VM.", |r| r.dns_queries),
        ("dns_errors_total", "DNS responses with a non-zero rcode per VM.", |r| r.dns_errors),
        ("http_requests_total", "Cleartext HTTP/1 requests per VM.", |r| r.http_requests),
        ("http_5xx_total", "Cleartext HTTP/1 5xx responses per VM.", |r| r.http_5xx),
        ("tls_handshakes_total", "TLS ClientHellos per VM.", |r| r.tls_handshakes),
        ("rtt_microseconds_sum", "Sum of TCP RTT samples per VM.", |r| r.rtt_sum_us),
        ("rtt_samples_total", "TCP RTT samples per VM.", |r| r.rtt_samples),
    ];
    for (name, help, get) in series {
        let full = format!("fluxvm_netobs_vm_{name}");
        let _ = writeln!(o, "# HELP {full} {help}\n# TYPE {full} counter");
        for vm in &s.per_vm {
            let _ = writeln!(o, "{full}{{vm=\"{}\"}} {}", esc(&vm.vm_id), get(&vm.red));
        }
    }
    o
}

/// OTLP/JSON `ExportMetricsServiceRequest` carrying the per-VM counters as cumulative sums. Field names follow
/// the OTLP JSON mapping (camelCase, 64-bit integers as strings).
#[cfg(feature = "otlp")]
pub fn otlp_metrics_json(s: &Summary) -> serde_json::Value {
    use serde_json::json;
    let ts = (s.generated_us.saturating_mul(1000)).to_string();
    let metric = |name: &str, unit: &str, get: fn(&Red) -> u64| {
        let points: Vec<_> = s
            .per_vm
            .iter()
            .map(|vm| {
                json!({
                    "attributes": [{"key": "vm.id", "value": {"stringValue": vm.vm_id}}],
                    "timeUnixNano": ts,
                    "asInt": get(&vm.red).to_string(),
                })
            })
            .collect();
        json!({
            "name": name,
            "unit": unit,
            "sum": {"aggregationTemporality": 2, "isMonotonic": true, "dataPoints": points},
        })
    };
    json!({
        "resourceMetrics": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "fluxvm"}}]},
            "scopeMetrics": [{
                "scope": {"name": "fluxvm-netobs", "version": SCHEMA_VERSION.to_string()},
                "metrics": [
                    metric("fluxvm.netobs.vm.packets", "{packet}", |r| r.packets),
                    metric("fluxvm.netobs.vm.bytes", "By", |r| r.bytes),
                    metric("fluxvm.netobs.vm.tcp.retransmits", "{segment}", |r| r.retransmits),
                    metric("fluxvm.netobs.vm.tcp.resets", "{reset}", |r| r.resets),
                    metric("fluxvm.netobs.vm.dns.queries", "{query}", |r| r.dns_queries),
                    metric("fluxvm.netobs.vm.dns.errors", "{response}", |r| r.dns_errors),
                ],
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> Summary {
        let mut s = Summary::disabled(1_700_000_000_000_000);
        s.enabled = true;
        s.counters.frames = 5;
        s.counters.drops_by_reason = vec![("policy".into(), 2), ("runt".into(), 0)];
        s.per_vm.push(VmSummary {
            vm_id: "vm\"1".into(),
            flows_active: 1,
            red: Red { packets: 9, retransmits: 2, ..Red::default() },
        });
        s
    }

    #[test]
    fn prometheus_text_shape() {
        let t = render_prometheus(&summary());
        assert!(t.contains("fluxvm_netobs_frames_total 5\n"));
        assert!(t.contains("fluxvm_netobs_frame_drops_total{reason=\"policy\"} 2\n"));
        assert!(t.contains("fluxvm_netobs_vm_tcp_retransmits_total{vm=\"vm\\\"1\"} 2\n"));
        // Every sample line is preceded by its HELP/TYPE once.
        assert_eq!(t.matches("# TYPE fluxvm_netobs_vm_packets_total counter").count(), 1);
    }

    #[test]
    fn event_json_is_flat_and_versioned() {
        let ev = NetEvent {
            schema: SCHEMA_VERSION,
            seq: 42,
            ts_us: 7,
            vm_id: "vm".into(),
            network: "n".into(),
            nic: "eth0".into(),
            kind: EventKind::Quic {
                flow_id: 1,
                server: Endpoint { ip: "1.2.3.4".into(), port: 443 },
                version: 1,
                dcid_len: 8,
                scid_len: None,
                sni: None,
                alpn: None,
                ja4: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["type"], "quic");
        assert_eq!(v["seq"], 42);
        assert_eq!(v["server"]["port"], 443);
        let sse = ev.to_sse();
        assert!(sse.starts_with("id: 42\nevent: netobs.quic\ndata: {"));
        assert!(sse.ends_with("\n\n"));
    }

    #[cfg(feature = "otlp")]
    #[test]
    fn otlp_mapping() {
        let v = otlp_metrics_json(&summary());
        let m = &v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        assert_eq!(m[2]["name"], "fluxvm.netobs.vm.tcp.retransmits");
        assert_eq!(m[2]["sum"]["dataPoints"][0]["asInt"], "2");
    }
}
