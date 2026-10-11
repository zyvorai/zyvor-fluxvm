// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! RED-style roll-ups (rate, errors, duration) per VM, peer, service port and protocol, in fixed time windows, with
//! a retained history that is bounded both by window count and by age. Cardinality is bounded too: past
//! `max_keys` per window, new keys are folded into one overflow row.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RollupKey {
    pub vm: String,
    /// The endpoint that is not the guest.
    pub peer: IpAddr,
    /// The service (server-side) port.
    pub port: u16,
    pub proto: u8,
    /// The guest initiated (egress) when true.
    pub outbound: bool,
}

impl RollupKey {
    fn overflow() -> Self {
        Self { vm: "_other".into(), peer: IpAddr::V4(Ipv4Addr::UNSPECIFIED), port: 0, proto: 0, outbound: true }
    }
}

/// Counters for one key and window. Everything is additive so windows can be merged.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Red {
    pub flows: u64,
    pub packets: u64,
    pub bytes: u64,
    pub resets: u64,
    pub retransmits: u64,
    pub zero_windows: u64,
    pub drops: u64,
    pub rtt_samples: u64,
    pub rtt_sum_us: u64,
    pub rtt_max_us: u64,
    pub dns_queries: u64,
    pub dns_errors: u64,
    pub dns_latency_samples: u64,
    pub dns_latency_sum_us: u64,
    pub http_requests: u64,
    pub http_responses: u64,
    pub http_5xx: u64,
    pub tls_handshakes: u64,
}

impl Red {
    pub fn merge(&mut self, o: &Red) {
        self.flows += o.flows;
        self.packets += o.packets;
        self.bytes += o.bytes;
        self.resets += o.resets;
        self.retransmits += o.retransmits;
        self.zero_windows += o.zero_windows;
        self.drops += o.drops;
        self.rtt_samples += o.rtt_samples;
        self.rtt_sum_us += o.rtt_sum_us;
        self.rtt_max_us = self.rtt_max_us.max(o.rtt_max_us);
        self.dns_queries += o.dns_queries;
        self.dns_errors += o.dns_errors;
        self.dns_latency_samples += o.dns_latency_samples;
        self.dns_latency_sum_us += o.dns_latency_sum_us;
        self.http_requests += o.http_requests;
        self.http_responses += o.http_responses;
        self.http_5xx += o.http_5xx;
        self.tls_handshakes += o.tls_handshakes;
    }

    /// Everything that counts against the "errors" leg of RED.
    pub fn error_events(&self) -> u64 {
        self.resets + self.retransmits + self.zero_windows + self.drops + self.dns_errors + self.http_5xx
    }

    pub fn rtt_mean_us(&self) -> u64 {
        if self.rtt_samples == 0 { 0 } else { self.rtt_sum_us / self.rtt_samples }
    }
}

/// One thing that happened, to be folded into the current window.
#[derive(Debug, Clone, Copy)]
pub enum Sample {
    FlowOpened,
    Packet { bytes: u64 },
    Reset,
    Retransmit,
    ZeroWindow,
    Drop,
    Rtt(u64),
    DnsQuery,
    DnsResponse { error: bool, latency_us: Option<u64> },
    HttpRequest,
    HttpResponse { status: u16 },
    TlsHello,
}

impl Red {
    pub fn apply(&mut self, s: Sample) {
        match s {
            Sample::FlowOpened => self.flows += 1,
            Sample::Packet { bytes } => {
                self.packets += 1;
                self.bytes += bytes;
            }
            Sample::Reset => self.resets += 1,
            Sample::Retransmit => self.retransmits += 1,
            Sample::ZeroWindow => self.zero_windows += 1,
            Sample::Drop => self.drops += 1,
            Sample::Rtt(us) => {
                self.rtt_samples += 1;
                self.rtt_sum_us += us;
                self.rtt_max_us = self.rtt_max_us.max(us);
            }
            Sample::DnsQuery => self.dns_queries += 1,
            Sample::DnsResponse { error, latency_us } => {
                if error {
                    self.dns_errors += 1;
                }
                if let Some(l) = latency_us {
                    self.dns_latency_samples += 1;
                    self.dns_latency_sum_us += l;
                }
            }
            Sample::HttpRequest => self.http_requests += 1,
            Sample::HttpResponse { status } => {
                self.http_responses += 1;
                if status >= 500 {
                    self.http_5xx += 1;
                }
            }
            Sample::TlsHello => self.tls_handshakes += 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Window {
    pub start_us: u64,
    pub end_us: u64,
    pub rows: HashMap<RollupKey, Red>,
}

#[derive(Debug, Clone)]
pub struct RollupConfig {
    pub window_us: u64,
    pub max_windows: usize,
    pub max_age_us: u64,
    pub max_keys: usize,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self { window_us: 30_000_000, max_windows: 240, max_age_us: 3_600_000_000, max_keys: 4096 }
    }
}

pub struct Rollup {
    cfg: RollupConfig,
    current: Window,
    history: VecDeque<Window>,
}

impl Rollup {
    pub fn new(cfg: RollupConfig) -> Self {
        Self {
            current: Window { start_us: 0, end_us: 0, rows: HashMap::new() },
            history: VecDeque::new(),
            cfg,
        }
    }

    pub fn config(&self) -> &RollupConfig {
        &self.cfg
    }

    /// Closes the current window if `now_us` is past its end, and prunes history.
    pub fn advance(&mut self, now_us: u64) {
        let w = self.cfg.window_us.max(1);
        if self.current.end_us == 0 {
            let start = now_us - now_us % w;
            self.current = Window { start_us: start, end_us: start + w, rows: HashMap::new() };
            return;
        }
        if now_us >= self.current.end_us {
            let start = now_us - now_us % w;
            let done = std::mem::replace(
                &mut self.current,
                Window { start_us: start, end_us: start + w, rows: HashMap::new() },
            );
            if !done.rows.is_empty() {
                self.history.push_back(done);
            }
        }
        while self.history.len() > self.cfg.max_windows {
            self.history.pop_front();
        }
        while let Some(f) = self.history.front() {
            if now_us.saturating_sub(f.end_us) > self.cfg.max_age_us {
                self.history.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn record(&mut self, now_us: u64, key: RollupKey, s: Sample) {
        self.advance(now_us);
        let rows = &mut self.current.rows;
        if !rows.contains_key(&key) && rows.len() >= self.cfg.max_keys {
            rows.entry(RollupKey::overflow()).or_default().apply(s);
        } else {
            rows.entry(key).or_default().apply(s);
        }
    }

    pub fn current(&self) -> &Window {
        &self.current
    }

    pub fn history(&self) -> impl Iterator<Item = &Window> {
        self.history.iter()
    }

    /// Merges the current window and all retained history into one table (optionally only windows ending after
    /// `since_us`).
    pub fn merged(&self, since_us: Option<u64>) -> HashMap<RollupKey, Red> {
        let mut out: HashMap<RollupKey, Red> = HashMap::new();
        let cutoff = since_us.unwrap_or(0);
        for w in self.history.iter().chain(std::iter::once(&self.current)) {
            if w.end_us <= cutoff {
                continue;
            }
            for (k, r) in &w.rows {
                out.entry(k.clone()).or_default().merge(r);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(port: u16) -> RollupKey {
        RollupKey { vm: "vm".into(), peer: "10.0.0.2".parse().unwrap(), port, proto: 6, outbound: true }
    }

    #[test]
    fn windows_close_and_merge() {
        let mut r = Rollup::new(RollupConfig { window_us: 100, ..RollupConfig::default() });
        r.record(10, key(80), Sample::Packet { bytes: 100 });
        r.record(50, key(80), Sample::Retransmit);
        r.record(120, key(80), Sample::Packet { bytes: 50 });
        assert_eq!(r.history().count(), 1);
        let m = r.merged(None);
        let red = &m[&key(80)];
        assert_eq!((red.packets, red.bytes, red.retransmits), (2, 150, 1));
        assert_eq!(red.error_events(), 1);
    }

    #[test]
    fn history_is_bounded_by_count_and_age() {
        let mut r = Rollup::new(RollupConfig { window_us: 10, max_windows: 3, max_age_us: 1_000, max_keys: 8 });
        for t in 0..10u64 {
            r.record(t * 10, key(1), Sample::Packet { bytes: 1 });
        }
        assert!(r.history().count() <= 3);
        r.record(5_000, key(1), Sample::Packet { bytes: 1 });
        assert_eq!(r.history().count(), 0, "everything older than max_age is gone");
    }

    #[test]
    fn key_cardinality_is_capped() {
        let mut r = Rollup::new(RollupConfig { max_keys: 2, ..RollupConfig::default() });
        for p in 0..10u16 {
            r.record(1, key(p), Sample::FlowOpened);
        }
        let m = r.merged(None);
        assert_eq!(m.len(), 3, "two real keys plus one overflow row");
        assert_eq!(m[&RollupKey::overflow()].flows, 8);
    }

    #[test]
    fn dns_and_http_samples() {
        let mut red = Red::default();
        red.apply(Sample::DnsResponse { error: true, latency_us: Some(900) });
        red.apply(Sample::DnsResponse { error: false, latency_us: Some(100) });
        red.apply(Sample::HttpResponse { status: 502 });
        red.apply(Sample::HttpResponse { status: 200 });
        assert_eq!((red.dns_errors, red.dns_latency_sum_us, red.http_5xx, red.http_responses), (1, 1000, 1, 2));
    }
}
