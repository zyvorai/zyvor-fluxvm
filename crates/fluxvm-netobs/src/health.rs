// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Per-flow connection health derived from headers only.
//!
//! TCP: handshake state, retransmissions, duplicate ACKs, zero-window episodes, and an RTT estimate from matching
//! data segments to the cumulative ACKs that cover them (Karn's rule: segments that were retransmitted are not
//! sampled). The RTT is measured at the observation point, so it is the distance from the tap to the peer and back,
//! not the guest's full path. ICMP echo gets request/reply pairing. UDP has no health beyond the flow counters.

use crate::packet::{Icmp, TCP_ACK, TCP_FIN, TCP_RST, TCP_SYN, Tcp};
use serde::Serialize;
use std::collections::VecDeque;

/// Which side of the flow sent a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    ClientToServer = 0,
    ServerToClient = 1,
}

impl Dir {
    pub fn rev(self) -> Dir {
        match self {
            Dir::ClientToServer => Dir::ServerToClient,
            Dir::ServerToClient => Dir::ClientToServer,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpState {
    /// SYN seen, no SYN-ACK yet.
    SynSent,
    /// SYN-ACK seen, handshake not yet completed by the client's ACK.
    SynReceived,
    Established,
    /// One side has sent FIN.
    Closing,
    /// Both sides sent FIN.
    Closed,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Fin,
    Reset,
    Idle,
    Active,
    Evicted,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RttStats {
    pub samples: u64,
    pub last_us: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub sum_us: u64,
    /// Smoothed RTT (RFC 6298 style, alpha = 1/8).
    pub srtt_us: u64,
}

impl RttStats {
    pub fn add(&mut self, rtt_us: u64) {
        self.last_us = rtt_us;
        self.min_us = if self.samples == 0 {
            rtt_us
        } else {
            self.min_us.min(rtt_us)
        };
        self.max_us = self.max_us.max(rtt_us);
        self.sum_us = self.sum_us.saturating_add(rtt_us);
        self.srtt_us = if self.samples == 0 {
            rtt_us
        } else {
            (self.srtt_us * 7 + rtt_us) / 8
        };
        self.samples += 1;
    }

    pub fn mean_us(&self) -> u64 {
        if self.samples == 0 {
            0
        } else {
            self.sum_us / self.samples
        }
    }
}

const MAX_PENDING: usize = 64;

#[derive(Debug, Clone, Default)]
struct DirState {
    /// Highest sequence number (exclusive end) sent so far.
    next_seq: Option<u32>,
    /// Data segments awaiting an ACK: (sequence end, send time).
    pending: VecDeque<(u32, u64)>,
    last_ack: Option<u32>,
    last_window: Option<u16>,
    zero_window: bool,
    fin: bool,
}

/// `a` is before `b` in sequence space.
fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
fn seq_le(a: u32, b: u32) -> bool {
    a == b || seq_lt(a, b)
}

/// What one packet changed, for the roll-up and event layers.
#[derive(Debug, Default, Clone, Copy)]
pub struct HealthDelta {
    pub retransmit: bool,
    pub dup_ack: bool,
    pub zero_window: bool,
    pub reset: bool,
    pub rtt_us: Option<u64>,
    pub handshake_rtt_us: Option<u64>,
    pub closed: Option<CloseReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TcpHealthView {
    pub state: TcpState,
    pub midstream: bool,
    pub handshake_rtt_us: Option<u64>,
    pub rtt: RttStats,
    pub retransmits_c2s: u64,
    pub retransmits_s2c: u64,
    pub dup_acks: u64,
    pub zero_windows_c2s: u64,
    pub zero_windows_s2c: u64,
    pub reset_by: Option<&'static str>,
    pub mss: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct TcpHealth {
    pub state: TcpState,
    /// The flow was first seen after its handshake.
    pub midstream: bool,
    syn_ts: Option<u64>,
    syn_seq: Option<u32>,
    pub handshake_rtt_us: Option<u64>,
    pub rtt: RttStats,
    pub retransmits: [u64; 2],
    pub dup_acks: u64,
    pub zero_windows: [u64; 2],
    pub reset_by: Option<Dir>,
    pub mss: Option<u16>,
    dirs: [DirState; 2],
}

impl TcpHealth {
    pub fn new() -> Self {
        Self {
            state: TcpState::Established,
            midstream: true,
            syn_ts: None,
            syn_seq: None,
            handshake_rtt_us: None,
            rtt: RttStats::default(),
            retransmits: [0; 2],
            dup_acks: 0,
            zero_windows: [0; 2],
            reset_by: None,
            mss: None,
            dirs: Default::default(),
        }
    }

    pub fn view(&self) -> TcpHealthView {
        TcpHealthView {
            state: self.state,
            midstream: self.midstream,
            handshake_rtt_us: self.handshake_rtt_us,
            rtt: self.rtt.clone(),
            retransmits_c2s: self.retransmits[0],
            retransmits_s2c: self.retransmits[1],
            dup_acks: self.dup_acks,
            zero_windows_c2s: self.zero_windows[0],
            zero_windows_s2c: self.zero_windows[1],
            reset_by: self.reset_by.map(|d| match d {
                Dir::ClientToServer => "client",
                Dir::ServerToClient => "server",
            }),
            mss: self.mss,
        }
    }

    pub fn on_segment(&mut self, dir: Dir, ts_us: u64, t: &Tcp) -> HealthDelta {
        let mut d = HealthDelta::default();
        let me = dir as usize;
        let peer = dir.rev() as usize;
        let syn = t.has(TCP_SYN);
        let ack = t.has(TCP_ACK);
        let fin = t.has(TCP_FIN);
        if t.mss.is_some() {
            self.mss = t.mss;
        }

        if syn && !ack {
            if self.state == TcpState::Established && self.midstream && self.syn_ts.is_none() {
                self.midstream = false;
                self.state = TcpState::SynSent;
                self.syn_ts = Some(ts_us);
                self.syn_seq = Some(t.seq);
            } else if self.syn_seq == Some(t.seq) && self.state == TcpState::SynSent {
                // SYN retransmission: the first SYN's timestamp stays the handshake start.
                self.retransmits[me] += 1;
                d.retransmit = true;
            }
        } else if syn && ack {
            if self.state == TcpState::SynSent {
                self.state = TcpState::SynReceived;
                if let Some(s) = self.syn_ts {
                    let rtt = ts_us.saturating_sub(s);
                    self.handshake_rtt_us = Some(rtt);
                    d.handshake_rtt_us = Some(rtt);
                    self.rtt.add(rtt);
                }
            } else if self.midstream && self.syn_ts.is_none() && self.state == TcpState::Established
            {
                // The first packet seen is a SYN-ACK (the SYN predates the observation).
                self.midstream = false;
                self.state = TcpState::SynReceived;
            } else {
                // Duplicate SYN-ACK: the server retransmitted it.
                self.retransmits[me] += 1;
                d.retransmit = true;
            }
        } else if self.state == TcpState::SynReceived && ack && dir == Dir::ClientToServer {
            self.state = TcpState::Established;
        }

        // Sequence-space accounting: data plus SYN/FIN occupy sequence numbers.
        let consumed = t.payload_len as u32 + u32::from(syn) + u32::from(fin);
        if consumed > 0 && !(syn && !ack && d.retransmit) && !(syn && ack && d.retransmit) {
            let end = t.seq.wrapping_add(consumed);
            let st = &mut self.dirs[me];
            match st.next_seq {
                Some(hi) if seq_lt(t.seq, hi) && seq_le(end, hi) => {
                    // Entirely below the highest sequence already sent: a retransmission (or reordering).
                    self.retransmits[me] += 1;
                    d.retransmit = true;
                    // Karn: ACKs for retransmitted data are ambiguous.
                    st.pending.clear();
                }
                Some(hi) if seq_lt(hi, end) => {
                    st.next_seq = Some(end);
                    if t.payload_len > 0 {
                        if st.pending.len() >= MAX_PENDING {
                            st.pending.pop_front();
                        }
                        st.pending.push_back((end, ts_us));
                    }
                }
                Some(_) => {}
                None => {
                    st.next_seq = Some(end);
                    if t.payload_len > 0 {
                        st.pending.push_back((end, ts_us));
                    }
                }
            }
        }

        // ACK processing: match against the peer's outstanding data.
        if ack && !t.has(TCP_RST) {
            let a = t.ack;
            let was_dup = {
                let st = &self.dirs[me];
                st.last_ack == Some(a)
                    && consumed == 0
                    && st.last_window == Some(t.window)
                    && !self.dirs[peer].pending.is_empty()
            };
            if was_dup {
                self.dup_acks += 1;
                d.dup_ack = true;
            }
            let pend = &mut self.dirs[peer].pending;
            let mut sample = None;
            while let Some(&(end, sent)) = pend.front() {
                if seq_le(end, a) {
                    // Prefer the segment this ACK covers exactly; otherwise the latest covered one.
                    sample = Some(ts_us.saturating_sub(sent));
                    pend.pop_front();
                } else {
                    break;
                }
            }
            if let Some(rtt) = sample {
                self.rtt.add(rtt);
                d.rtt_us = Some(rtt);
            }
            self.dirs[me].last_ack = Some(a);
        }

        // Zero window: the sender advertises it cannot take more data.
        if !t.has(TCP_RST) && !syn {
            let st = &mut self.dirs[me];
            if t.window == 0 {
                if !st.zero_window {
                    st.zero_window = true;
                    self.zero_windows[me] += 1;
                    d.zero_window = true;
                }
            } else {
                st.zero_window = false;
            }
        }
        self.dirs[me].last_window = Some(t.window);

        if fin {
            self.dirs[me].fin = true;
            if self.dirs[0].fin && self.dirs[1].fin {
                self.state = TcpState::Closed;
                d.closed = Some(CloseReason::Fin);
            } else if !matches!(self.state, TcpState::Closed | TcpState::Reset) {
                self.state = TcpState::Closing;
            }
        }
        if t.has(TCP_RST) && self.state != TcpState::Reset {
            self.state = TcpState::Reset;
            self.reset_by = Some(dir);
            d.reset = true;
            d.closed = Some(CloseReason::Reset);
        }
        d
    }
}

impl Default for TcpHealth {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IcmpHealth {
    pub echo_requests: u64,
    pub echo_replies: u64,
    pub unreachable: u64,
    pub rtt: RttStats,
    #[serde(skip)]
    pending: VecDeque<(u16, u16, u64)>,
}

impl IcmpHealth {
    /// Returns an RTT sample when a reply matches a recorded request.
    pub fn on_message(&mut self, ts_us: u64, m: &Icmp) -> Option<u64> {
        if m.is_echo_request() {
            self.echo_requests += 1;
            if self.pending.len() >= 16 {
                self.pending.pop_front();
            }
            self.pending.push_back((m.id, m.seq, ts_us));
        } else if m.is_echo_reply() {
            self.echo_replies += 1;
            if let Some(i) = self
                .pending
                .iter()
                .position(|(id, seq, _)| *id == m.id && *seq == m.seq)
            {
                let (_, _, sent) = self.pending.remove(i)?;
                let rtt = ts_us.saturating_sub(sent);
                self.rtt.add(rtt);
                return Some(rtt);
            }
        } else if m.is_unreachable() {
            self.unreachable += 1;
        }
        None
    }
}

#[derive(Debug, Clone)]
pub enum Health {
    None,
    Tcp(Box<TcpHealth>),
    Icmp(IcmpHealth),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::TCP_PSH;

    fn seg(seq: u32, ack: u32, flags: u8, window: u16, len: usize) -> Tcp {
        Tcp {
            sport: 1,
            dport: 2,
            seq,
            ack,
            flags,
            window,
            mss: None,
            wscale: None,
            payload_len: len,
        }
    }

    const C: Dir = Dir::ClientToServer;
    const S: Dir = Dir::ServerToClient;

    #[test]
    fn handshake_rtt_and_established() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 1_000, &seg(100, 0, TCP_SYN, 65535, 0));
        assert_eq!(h.state, TcpState::SynSent);
        let d = h.on_segment(
            S,
            1_000 + 12_000,
            &seg(500, 101, TCP_SYN | TCP_ACK, 65535, 0),
        );
        assert_eq!(d.handshake_rtt_us, Some(12_000));
        assert_eq!(h.state, TcpState::SynReceived);
        h.on_segment(C, 13_100, &seg(101, 501, TCP_ACK, 65535, 0));
        assert_eq!(h.state, TcpState::Established);
        assert!(!h.midstream);
    }

    #[test]
    fn data_rtt_from_ack() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 0, &seg(100, 0, TCP_SYN, 100, 0));
        h.on_segment(S, 5_000, &seg(500, 101, TCP_SYN | TCP_ACK, 100, 0));
        h.on_segment(C, 5_100, &seg(101, 501, TCP_ACK, 100, 0));
        h.on_segment(C, 10_000, &seg(101, 501, TCP_ACK | TCP_PSH, 100, 100));
        let d = h.on_segment(S, 17_500, &seg(501, 201, TCP_ACK, 100, 0));
        assert_eq!(d.rtt_us, Some(7_500));
        assert_eq!(h.rtt.samples, 2);
        assert_eq!(h.rtt.max_us, 7_500);
    }

    #[test]
    fn retransmission_detected_and_not_sampled() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 0, &seg(1000, 1, TCP_ACK, 100, 100));
        let d = h.on_segment(C, 200_000, &seg(1000, 1, TCP_ACK, 100, 100));
        assert!(d.retransmit);
        assert_eq!(h.retransmits[0], 1);
        // The ACK after a retransmission is ambiguous: no sample.
        let d = h.on_segment(S, 200_500, &seg(1, 1100, TCP_ACK, 100, 0));
        assert_eq!(d.rtt_us, None);
        // New data afterwards is sampled again.
        h.on_segment(C, 300_000, &seg(1100, 1, TCP_ACK, 100, 50));
        let d = h.on_segment(S, 300_900, &seg(1, 1150, TCP_ACK, 100, 0));
        assert_eq!(d.rtt_us, Some(900));
    }

    #[test]
    fn syn_retransmit_keeps_original_timestamp() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 0, &seg(7, 0, TCP_SYN, 100, 0));
        let d = h.on_segment(C, 1_000_000, &seg(7, 0, TCP_SYN, 100, 0));
        assert!(d.retransmit);
        let d = h.on_segment(S, 1_030_000, &seg(9, 8, TCP_SYN | TCP_ACK, 100, 0));
        // Measured against the first SYN (inflated by the loss), by design.
        assert_eq!(d.handshake_rtt_us, Some(1_030_000));
    }

    #[test]
    fn zero_window_counted_once_per_episode() {
        let mut h = TcpHealth::new();
        assert!(h.on_segment(S, 0, &seg(1, 1, TCP_ACK, 0, 0)).zero_window);
        assert!(!h.on_segment(S, 1, &seg(1, 1, TCP_ACK, 0, 0)).zero_window);
        h.on_segment(S, 2, &seg(1, 1, TCP_ACK, 500, 0));
        assert!(h.on_segment(S, 3, &seg(1, 1, TCP_ACK, 0, 0)).zero_window);
        assert_eq!(h.zero_windows[1], 2);
    }

    #[test]
    fn fin_exchange_and_reset() {
        let mut h = TcpHealth::new();
        let d = h.on_segment(C, 0, &seg(10, 5, TCP_FIN | TCP_ACK, 100, 0));
        assert_eq!(h.state, TcpState::Closing);
        assert!(d.closed.is_none());
        let d = h.on_segment(S, 1, &seg(5, 11, TCP_FIN | TCP_ACK, 100, 0));
        assert_eq!(d.closed, Some(CloseReason::Fin));
        assert_eq!(h.state, TcpState::Closed);

        let mut h = TcpHealth::new();
        let d = h.on_segment(S, 0, &seg(5, 0, TCP_RST, 0, 0));
        assert!(d.reset);
        assert_eq!(h.state, TcpState::Reset);
        assert_eq!(h.reset_by, Some(S));
    }

    #[test]
    fn dup_acks_need_outstanding_data() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 0, &seg(100, 1, TCP_ACK, 100, 100));
        h.on_segment(C, 1, &seg(200, 1, TCP_ACK, 100, 100));
        h.on_segment(S, 2, &seg(1, 200, TCP_ACK, 100, 0));
        // Segment two (end 300) is still outstanding, so repeats of ACK 200 are duplicates.
        assert!(h.on_segment(S, 3, &seg(1, 200, TCP_ACK, 100, 0)).dup_ack);
        assert!(h.on_segment(S, 4, &seg(1, 200, TCP_ACK, 100, 0)).dup_ack);
        assert_eq!(h.dup_acks, 2);
        // Once everything is acknowledged, a repeat is not a duplicate ACK.
        h.on_segment(S, 5, &seg(1, 300, TCP_ACK, 100, 0));
        assert!(!h.on_segment(S, 6, &seg(1, 300, TCP_ACK, 100, 0)).dup_ack);
    }

    #[test]
    fn sequence_wraparound() {
        let mut h = TcpHealth::new();
        h.on_segment(C, 0, &seg(u32::MAX - 49, 1, TCP_ACK, 100, 100));
        let d = h.on_segment(S, 10, &seg(1, 50, TCP_ACK, 100, 0));
        assert_eq!(d.rtt_us, Some(10));
    }

    #[test]
    fn icmp_echo_pairing() {
        let mut h = IcmpHealth::default();
        let req = Icmp {
            v6: false,
            kind: 8,
            code: 0,
            id: 1,
            seq: 1,
        };
        let rep = Icmp {
            v6: false,
            kind: 0,
            code: 0,
            id: 1,
            seq: 1,
        };
        assert_eq!(h.on_message(100, &req), None);
        assert_eq!(h.on_message(1_600, &rep), Some(1_500));
        assert_eq!((h.echo_requests, h.echo_replies), (1, 1));
    }
}
