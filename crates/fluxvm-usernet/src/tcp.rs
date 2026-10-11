// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The guest-facing half of one proxied TCP connection. We terminate the guest's TCP: from the guest's view we are
//! the remote host (or, for a host->guest forward, the client). Payload is relayed to a host-side stream by the
//! engine; this state machine does sequence numbers, acknowledgements, windows and retransmission.
//!
//! * Window scaling (RFC 7323) is negotiated when the guest offers it; our receive window is [`RCV_WND`].
//! * Receive side: out-of-order guest data is buffered (bounded by the window) and reported with SACK blocks
//!   (RFC 2018) when the guest sent SACK-permitted.
//! * Send side (host -> guest): NewReno congestion control (RFC 5681 / 6582): slow start from an initial window of
//!   10 segments, congestion avoidance, fast retransmit and fast recovery on three duplicate ACKs, and a
//!   go-back-N restart from a one-segment window on retransmission timeout.
//! * The retransmission timer follows RFC 6298: SRTT/RTTVAR from one timed segment per flight, Karn's rule (a
//!   retransmitted segment is never timed), RTO clamped to `[MIN_RTO, MAX_RTO]` and doubled per timeout. Until the
//!   first sample the RTO is [`INIT_RTO`] (400 ms rather than the RFC's 1 s: the peer is a local VM).
//! * The guest's SACK blocks feed a scoreboard (RFC 2018/6675 style). In fast recovery only the holes the
//!   scoreboard shows (un-SACKed bytes below the highest SACKed byte) are retransmitted, each at most once per
//!   recovery. The scoreboard is cleared on a timeout (the receiver may renege).

use crate::wire::{ACK, FIN, PSH, RST, SYN, Tcp, TcpOut};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Receive window we advertise (and the bound for out-of-order buffering).
pub const RCV_WND: usize = 256 * 1024;
/// Shift we announce when the guest offers window scaling: `RCV_WND >> 3` fits the 16-bit field.
const RCV_SCALE: u8 = 3;
const MAX_RETRIES: u32 = 8;
const MAX_RTO: Duration = Duration::from_secs(60);
const MIN_RTO: Duration = Duration::from_millis(200);
const INIT_RTO: Duration = Duration::from_millis(400);
/// Hole retransmissions allowed per incoming ACK during SACK recovery.
const SACK_REXMIT_PER_ACK: usize = 2;
const MAX_SACK_RANGES: usize = 64;
const MAX_OOO_BLOCKS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Passive open: guest SYN seen, host connect pending.
    Connecting,
    /// Passive open: SYN-ACK sent.
    SynRcvd,
    /// Active open (host->guest forward): SYN sent.
    SynSent,
    Established,
    Closed,
}

#[derive(Default, Debug)]
pub struct SegResult {
    /// In-order payload accepted from the guest, to be written to the host stream.
    pub data: Option<Vec<u8>>,
    /// Guest sent FIN: half-close the host stream's write side.
    pub guest_fin: bool,
    /// Bytes of host data the guest acknowledged (frees host-reader budget).
    pub acked: usize,
    /// Connection reset by the guest.
    pub reset: bool,
}

fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn random_u32() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish() as u32
}

pub struct TcpConn {
    pub state: State,
    our_mss: usize,
    mss: usize,
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// Highest sequence number ever sent (snd_nxt is rewound on a retransmission timeout).
    snd_max: u32,
    rcv_nxt: u32,
    peer_wnd: usize,
    snd_scale: u8,
    rcv_scale: u8,
    ws_ok: bool,
    sack_ok: bool,
    sndbuf: VecDeque<u8>,
    fin_queued: bool,
    fin_sent: bool,
    pub fin_acked: bool,
    pub fin_rcvd: bool,
    /// Sequence number of a guest FIN that arrived ahead of missing data.
    fin_pending: Option<u32>,
    /// Out-of-order guest data: disjoint, non-adjacent `(seq, bytes)` blocks.
    ooo: Vec<(u32, Vec<u8>)>,
    last_ooo: Option<u32>,
    /// Room left in the host-bound queue; the advertised window is bounded by it, so the guest is paced by the
    /// host writer instead of having bytes dropped (and retransmitted) once the queue is full.
    host_room: usize,
    /// Window (bytes) last put on the wire, to decide when a window update is worth sending.
    last_adv: std::cell::Cell<usize>,
    cwnd: usize,
    ssthresh: usize,
    dupacks: u32,
    /// Fast recovery in progress: the highest sequence sent when it began.
    recover: Option<u32>,
    last_tx: Instant,
    pub last_activity: Instant,
    rto: Duration,
    /// RFC 6298 smoothed RTT and variation (`None` until the first sample).
    srtt: Option<Duration>,
    rttvar: Duration,
    /// The one segment being timed: (sequence number just past it, send time). Cleared on any retransmission.
    rtt_probe: Option<(u32, Instant)>,
    /// Guest-SACKed ranges above snd_una: sorted, disjoint, non-adjacent `(start, end)`.
    scoreboard: Vec<(u32, u32)>,
    /// Highest sequence retransmitted in the current recovery (holes below it are not resent).
    rexmit_high: u32,
    retries: u32,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

impl TcpConn {
    fn blank(our_mss: usize, now: Instant) -> Self {
        let iss = random_u32();
        TcpConn {
            state: State::Connecting,
            our_mss,
            mss: 536,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            rcv_nxt: 0,
            peer_wnd: 0,
            snd_scale: 0,
            rcv_scale: 0,
            ws_ok: false,
            sack_ok: false,
            sndbuf: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            fin_acked: false,
            fin_rcvd: false,
            fin_pending: None,
            ooo: Vec::new(),
            last_ooo: None,
            host_room: RCV_WND,
            last_adv: std::cell::Cell::new(RCV_WND),
            cwnd: 10 * 536,
            ssthresh: usize::MAX / 2,
            dupacks: 0,
            recover: None,
            last_tx: now,
            last_activity: now,
            rto: INIT_RTO,
            srtt: None,
            rttvar: Duration::ZERO,
            rtt_probe: None,
            scoreboard: Vec::new(),
            rexmit_high: iss,
            retries: 0,
            bytes_up: 0,
            bytes_down: 0,
        }
    }

    fn set_mss(&mut self, mss: usize) {
        self.mss = mss;
        self.cwnd = 10 * mss;
    }

    /// The guest sent `syn`; the host connection is still being made.
    pub fn new_passive(syn: &Tcp<'_>, our_mss: usize, now: Instant) -> Self {
        let mut c = Self::blank(our_mss, now);
        c.rcv_nxt = syn.seq.wrapping_add(1);
        c.set_mss(usize::from(syn.mss.unwrap_or(536)).clamp(88, our_mss));
        c.peer_wnd = usize::from(syn.window);
        if let Some(s) = syn.wscale {
            c.ws_ok = true;
            c.snd_scale = s;
            c.rcv_scale = RCV_SCALE;
        }
        c.sack_ok = syn.sack_ok;
        c
    }

    /// Host connect succeeded: answer the guest's SYN.
    pub fn host_connected(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        if self.state != State::Connecting {
            return;
        }
        self.state = State::SynRcvd;
        out.push(self.syn_ack());
        self.snd_nxt = self.iss.wrapping_add(1);
        self.snd_max = self.snd_nxt;
        self.last_tx = now;
        self.rtt_probe = Some((self.snd_nxt, now));
    }

    fn syn_ack(&self) -> TcpOut {
        TcpOut {
            seq: self.iss,
            ack: self.rcv_nxt,
            flags: SYN | ACK,
            window: 65535,
            mss: Some(self.our_mss as u16),
            wscale: self.ws_ok.then_some(RCV_SCALE),
            sack_ok: self.sack_ok,
            ..Default::default()
        }
    }

    fn syn(&self) -> TcpOut {
        TcpOut {
            seq: self.iss,
            flags: SYN,
            window: 65535,
            mss: Some(self.our_mss as u16),
            wscale: Some(RCV_SCALE),
            sack_ok: true,
            ..Default::default()
        }
    }

    /// Active open towards the guest (a published port): emits the SYN.
    pub fn new_active(our_mss: usize, now: Instant, out: &mut Vec<TcpOut>) -> Self {
        let mut c = Self::blank(our_mss, now);
        c.state = State::SynSent;
        c.rcv_scale = RCV_SCALE;
        out.push(c.syn());
        c.snd_nxt = c.iss.wrapping_add(1);
        c.snd_max = c.snd_nxt;
        c.rtt_probe = Some((c.snd_nxt, now));
        c
    }

    /// Feed an RTT measurement (RFC 6298 section 2) and recompute the RTO, dropping any backoff.
    fn rtt_sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(srtt) => {
                let delta = srtt.abs_diff(r);
                self.rttvar = self.rttvar * 3 / 4 + delta / 4;
                self.srtt = Some(srtt * 7 / 8 + r / 8);
            }
        }
        let srtt = self.srtt.unwrap_or(r);
        let var = (self.rttvar * 4).max(Duration::from_millis(1));
        self.rto = (srtt + var).clamp(MIN_RTO, MAX_RTO);
    }

    /// `ack` acknowledged new data: complete the timed segment if it is covered (Karn: a retransmission clears
    /// the probe, so an ambiguous ACK is never sampled).
    fn rtt_on_ack(&mut self, ack: u32, now: Instant) {
        if let Some((end, t0)) = self.rtt_probe
            && !seq_lt(ack, end)
        {
            self.rtt_probe = None;
            self.rtt_sample(now.saturating_duration_since(t0));
        }
    }

    /// Record guest SACK blocks (only ranges within `(snd_una, snd_max]` are meaningful).
    fn sack_update(&mut self, blocks: &[(u32, u32)]) {
        for &(s, e) in blocks {
            let s = if seq_lt(s, self.snd_una) {
                self.snd_una
            } else {
                s
            };
            if !seq_lt(s, e) || seq_lt(self.snd_max, e) {
                continue;
            }
            self.scoreboard.push((s, e));
        }
        self.sack_normalize();
    }

    fn sack_normalize(&mut self) {
        let base = self.snd_una;
        let mut v: Vec<(u32, u32)> = std::mem::take(&mut self.scoreboard)
            .into_iter()
            .filter_map(|(s, e)| {
                let s = if seq_lt(s, base) { base } else { s };
                seq_lt(s, e).then_some((s, e))
            })
            .collect();
        v.sort_by_key(|(s, _)| s.wrapping_sub(base));
        let mut out: Vec<(u32, u32)> = Vec::new();
        for (s, e) in v {
            if let Some((_, pe)) = out.last_mut()
                && !seq_lt(*pe, s)
            {
                if seq_lt(*pe, e) {
                    *pe = e;
                }
                continue;
            }
            out.push((s, e));
        }
        out.truncate(MAX_SACK_RANGES);
        self.scoreboard = out;
    }

    /// Bytes the guest reports holding above snd_una.
    fn sacked_bytes(&self) -> usize {
        self.scoreboard
            .iter()
            .map(|(s, e)| e.wrapping_sub(*s) as usize)
            .sum()
    }

    /// The next un-SACKed, not-yet-retransmitted hole below the highest SACKed byte, at most one MSS long.
    fn next_hole(&self) -> Option<(u32, u32)> {
        let mut pos = if seq_lt(self.rexmit_high, self.snd_una) {
            self.snd_una
        } else {
            self.rexmit_high
        };
        for &(s, e) in &self.scoreboard {
            if seq_lt(pos, s) {
                let cap = pos.wrapping_add(self.mss as u32);
                let end = if seq_lt(s, cap) { s } else { cap };
                return Some((pos, end));
            }
            if seq_lt(pos, e) {
                pos = e;
            }
        }
        None
    }

    /// Retransmit `[start, end)` of the send buffer.
    fn retransmit_range(&mut self, start: u32, end: u32, out: &mut Vec<TcpOut>) -> bool {
        let off = start.wrapping_sub(self.snd_una) as usize;
        let want = end.wrapping_sub(start) as usize;
        if off >= self.sndbuf.len() {
            return false;
        }
        let n = want.min(self.sndbuf.len() - off);
        out.push(self.data_seg(start, ACK, self.copy_range(off, n)));
        self.rexmit_high = start.wrapping_add(n as u32);
        self.rtt_probe = None;
        true
    }

    /// SACK-driven retransmission of up to `budget` holes. Returns how many segments were sent.
    fn retransmit_holes(&mut self, budget: usize, out: &mut Vec<TcpOut>) -> usize {
        let mut sent = 0;
        while sent < budget {
            let Some((s, e)) = self.next_hole() else {
                break;
            };
            if !self.retransmit_range(s, e, out) {
                break;
            }
            sent += 1;
        }
        sent
    }

    /// Receive window to put in the 16-bit field of a non-SYN segment.
    fn adv_window(&self) -> u16 {
        let bytes = self.adv_bytes();
        self.last_adv.set(bytes);
        if self.ws_ok {
            (bytes >> self.rcv_scale) as u16
        } else {
            bytes as u16
        }
    }

    /// Receive window in bytes: the host queue's free room, with silly-window avoidance (below one MSS it is
    /// advertised as zero until a window update).
    fn adv_bytes(&self) -> usize {
        let room = self.host_room.min(RCV_WND);
        let room = if room < self.mss { 0 } else { room };
        if self.ws_ok {
            room & !((1usize << self.rcv_scale) - 1)
        } else {
            room.min(65535)
        }
    }

    /// Bytes beyond `rcv_nxt` we are willing to buffer.
    fn rcv_window(&self) -> usize {
        self.host_room.min(RCV_WND)
    }

    /// The host-bound queue has `room` bytes free. Records it and, when the window has opened enough since the
    /// last advertisement (from zero, or by a couple of segments), sends a window update.
    pub fn set_host_room(&mut self, room: usize, out: &mut Vec<TcpOut>) {
        self.host_room = room;
        if self.state != State::Established {
            return;
        }
        let last = self.last_adv.get();
        let now = self.adv_bytes();
        if now > last && (last == 0 || now >= last + 2 * self.mss || now >= RCV_WND) {
            out.push(self.ack_only());
        }
    }

    /// Update the free room without emitting anything (the segment being processed carries the new window).
    pub fn note_host_room(&mut self, room: usize) {
        self.host_room = room;
    }

    /// SACK blocks for the out-of-order data held, the block with the most recent segment first.
    fn sack_blocks(&self) -> Vec<(u32, u32)> {
        if !self.sack_ok || self.ooo.is_empty() {
            return Vec::new();
        }
        let mut v: Vec<(u32, u32)> = self
            .ooo
            .iter()
            .map(|(s, d)| (*s, s.wrapping_add(d.len() as u32)))
            .collect();
        if let Some(l) = self.last_ooo
            && let Some(i) = v.iter().position(|(s, e)| !seq_lt(l, *s) && seq_lt(l, *e))
        {
            v.swap(0, i);
        }
        v.truncate(4);
        v
    }

    fn ack_only(&self) -> TcpOut {
        TcpOut {
            seq: self.snd_nxt,
            ack: self.rcv_nxt,
            flags: ACK,
            window: self.adv_window(),
            sack: self.sack_blocks(),
            ..Default::default()
        }
    }

    fn data_seg(&self, seq: u32, flags: u8, payload: Vec<u8>) -> TcpOut {
        TcpOut {
            seq,
            ack: self.rcv_nxt,
            flags,
            window: self.adv_window(),
            payload,
            ..Default::default()
        }
    }

    pub fn rst(&mut self, out: &mut Vec<TcpOut>) {
        if self.state != State::Closed {
            out.push(TcpOut {
                seq: self.snd_nxt,
                ack: self.rcv_nxt,
                flags: RST | ACK,
                ..Default::default()
            });
        }
        self.state = State::Closed;
    }

    /// Data bytes between snd_una and snd_nxt (the retransmission-rewound send point).
    fn data_in_flight(&self) -> usize {
        (self.snd_nxt.wrapping_sub(self.snd_una) as usize)
            .saturating_sub(usize::from(self.fin_sent))
    }

    /// Data bytes ever sent and not yet acknowledged.
    fn outstanding(&self) -> usize {
        (self.snd_max.wrapping_sub(self.snd_una) as usize).min(self.sndbuf.len())
    }

    #[cfg(test)]
    pub fn rto(&self) -> Duration {
        self.rto
    }

    #[cfg(test)]
    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    #[cfg(test)]
    pub fn cwnd(&self) -> usize {
        self.cwnd
    }

    #[cfg(test)]
    pub fn ssthresh(&self) -> usize {
        self.ssthresh
    }

    #[cfg(test)]
    pub fn in_recovery(&self) -> bool {
        self.recover.is_some()
    }

    #[cfg(test)]
    pub fn sack_enabled(&self) -> bool {
        self.sack_ok
    }

    #[cfg(test)]
    pub fn window_scaling(&self) -> bool {
        self.ws_ok
    }

    /// Host -> guest bytes.
    pub fn push_host_data(&mut self, data: &[u8], now: Instant, out: &mut Vec<TcpOut>) {
        self.sndbuf.extend(data);
        self.bytes_down += data.len() as u64;
        self.pump(now, out);
    }

    pub fn host_eof(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        self.fin_queued = true;
        self.pump(now, out);
    }

    fn copy_range(&self, start: usize, len: usize) -> Vec<u8> {
        let (a, b) = self.sndbuf.as_slices();
        let mut v = Vec::with_capacity(len);
        if start < a.len() {
            let e = (start + len).min(a.len());
            v.extend_from_slice(&a[start..e]);
        }
        let need = len - v.len();
        if need > 0 {
            let s = start.saturating_sub(a.len());
            v.extend_from_slice(&b[s..s + need]);
        }
        v
    }

    fn note_sent(&mut self) {
        if seq_lt(self.snd_max, self.snd_nxt) {
            self.snd_max = self.snd_nxt;
        }
    }

    fn pump(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        if self.state != State::Established {
            return;
        }
        let before = out.len();
        let wnd = self.peer_wnd.min(self.cwnd);
        let mut sent = self.data_in_flight();
        while sent < self.sndbuf.len() && !self.fin_sent {
            let flight = self.data_in_flight();
            if flight >= wnd {
                break;
            }
            let remain = self.sndbuf.len() - sent;
            let n = self.mss.min(remain).min(wnd - flight);
            if n < self.mss.min(remain) && flight > 0 {
                break; // do not dribble sub-MSS segments while data is in flight
            }
            let more = n < remain;
            let seg = self.data_seg(
                self.snd_nxt,
                ACK | if more { 0 } else { PSH },
                self.copy_range(sent, n),
            );
            out.push(seg);
            let is_new = !seq_lt(self.snd_nxt, self.snd_max);
            self.snd_nxt = self.snd_nxt.wrapping_add(n as u32);
            if is_new && self.rtt_probe.is_none() {
                self.rtt_probe = Some((self.snd_nxt, now));
            }
            sent += n;
        }
        if self.fin_queued && !self.fin_sent && sent == self.sndbuf.len() {
            out.push(self.data_seg(self.snd_nxt, FIN | ACK, Vec::new()));
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.fin_sent = true;
        }
        self.note_sent();
        if out.len() > before {
            self.last_tx = now;
        }
    }

    /// Retransmit the first unacknowledged segment (fast retransmit / partial ACK) without moving snd_nxt.
    fn retransmit_one(&mut self, out: &mut Vec<TcpOut>) {
        self.rtt_probe = None;
        if !self.sndbuf.is_empty() {
            let n = self.mss.min(self.sndbuf.len());
            out.push(self.data_seg(self.snd_una, ACK, self.copy_range(0, n)));
            self.rexmit_high = self.snd_una.wrapping_add(n as u32);
        } else if self.fin_sent && !self.fin_acked {
            out.push(self.data_seg(self.snd_una, FIN | ACK, Vec::new()));
        }
    }

    /// Retransmission timeout: collapse the window to one segment and resend from snd_una (go-back-N).
    fn on_rto(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        let flight = self.outstanding();
        self.ssthresh = (flight / 2).max(2 * self.mss);
        self.cwnd = self.mss;
        self.dupacks = 0;
        self.recover = None;
        self.scoreboard.clear();
        self.rexmit_high = self.snd_una;
        self.rtt_probe = None;
        self.snd_nxt = self.snd_una;
        self.fin_sent = false;
        let before = out.len();
        self.pump(now, out);
        if out.len() == before {
            self.last_tx = now;
        }
    }

    fn on_new_ack(&mut self, acked: usize, ack: u32, out: &mut Vec<TcpOut>) {
        self.dupacks = 0;
        if let Some(rec) = self.recover {
            if !seq_lt(ack, rec) {
                // Full acknowledgement: leave recovery with a deflated window (RFC 6582 section 3.2, step 3).
                self.recover = None;
                self.rexmit_high = self.snd_una;
                self.cwnd = self
                    .ssthresh
                    .min(self.outstanding().max(self.mss) + self.mss)
                    .max(self.mss);
            } else {
                // Partial acknowledgement: the next hole is lost too.
                if !seq_lt(self.snd_una, self.rexmit_high) {
                    // snd_una itself is the new hole (not yet resent).
                    self.retransmit_one(out);
                    if !self.scoreboard.is_empty() {
                        self.retransmit_holes(SACK_REXMIT_PER_ACK - 1, out);
                    }
                } else {
                    self.retransmit_holes(SACK_REXMIT_PER_ACK, out);
                }
                self.cwnd = self.cwnd.saturating_sub(acked).max(self.mss) + self.mss;
            }
        } else if self.cwnd < self.ssthresh {
            self.cwnd += acked.min(self.mss);
        } else {
            self.cwnd += (self.mss * self.mss / self.cwnd).max(1);
        }
    }

    fn on_dup_ack(&mut self, out: &mut Vec<TcpOut>) {
        self.dupacks += 1;
        if self.recover.is_some() {
            self.cwnd += self.mss; // each dup ACK means a segment left the network
            self.retransmit_holes(SACK_REXMIT_PER_ACK, out);
        } else if self.dupacks == 3 || self.sacked_bytes() >= 3 * self.mss {
            self.ssthresh = (self.outstanding() / 2).max(2 * self.mss);
            self.recover = Some(self.snd_max);
            self.rexmit_high = self.snd_una;
            self.retransmit_one(out);
            self.retransmit_holes(SACK_REXMIT_PER_ACK - 1, out);
            self.cwnd = self.ssthresh + 3 * self.mss;
        }
    }

    fn insert_ooo(&mut self, seq: u32, data: &[u8]) {
        let off = seq.wrapping_sub(self.rcv_nxt) as usize;
        let wnd = self.rcv_window();
        if off == 0 || off >= wnd || data.is_empty() {
            return;
        }
        let data = &data[..data.len().min(wnd - off)];
        self.ooo.push((seq, data.to_vec()));
        self.last_ooo = Some(seq);
        self.normalize_ooo();
    }

    /// Trim data below rcv_nxt, sort by distance from it and merge overlapping or adjacent blocks.
    fn normalize_ooo(&mut self) {
        let base = self.rcv_nxt;
        let mut items: Vec<(u32, Vec<u8>)> = Vec::with_capacity(self.ooo.len());
        for (s, d) in std::mem::take(&mut self.ooo) {
            let rel = s.wrapping_sub(base) as i32;
            if rel < 0 {
                let skip = rel.unsigned_abs() as usize;
                if skip >= d.len() {
                    continue;
                }
                items.push((base, d[skip..].to_vec()));
            } else {
                items.push((s, d));
            }
        }
        items.sort_by_key(|(s, _)| s.wrapping_sub(base));
        let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
        for (s, d) in items {
            if let Some((ps, pd)) = out.last_mut() {
                let gap = s.wrapping_sub(*ps) as usize;
                if gap <= pd.len() {
                    let covered = pd.len() - gap;
                    if covered < d.len() {
                        pd.extend_from_slice(&d[covered..]);
                    }
                    continue;
                }
            }
            out.push((s, d));
        }
        out.truncate(MAX_OOO_BLOCKS);
        self.ooo = out;
        if self.ooo.is_empty() {
            self.last_ooo = None;
        }
    }

    /// Move now-contiguous out-of-order data into `data`.
    fn drain_ooo(&mut self, data: &mut Vec<u8>) {
        self.normalize_ooo();
        while self.ooo.first().is_some_and(|(s, _)| *s == self.rcv_nxt) {
            let (_, d) = self.ooo.remove(0);
            self.rcv_nxt = self.rcv_nxt.wrapping_add(d.len() as u32);
            self.bytes_up += d.len() as u64;
            data.extend_from_slice(&d);
        }
        if self.ooo.is_empty() {
            self.last_ooo = None;
        }
    }

    /// A segment from the guest. `can_accept` is false when the host-side writer is backed up: payload is then
    /// dropped unacknowledged and the guest retransmits.
    pub fn on_segment(
        &mut self,
        seg: &Tcp<'_>,
        can_accept: bool,
        now: Instant,
        out: &mut Vec<TcpOut>,
    ) -> SegResult {
        let mut res = SegResult::default();
        self.last_activity = now;
        if seg.flags & RST != 0 {
            if self.state != State::Closed {
                self.state = State::Closed;
                res.reset = true;
            }
            return res;
        }
        match self.state {
            State::Closed => return res,
            State::Connecting => return res, // SYN retransmit while the host connect is pending
            State::SynSent => {
                if seg.flags & (SYN | ACK) == (SYN | ACK) && seg.ack == self.iss.wrapping_add(1) {
                    self.rcv_nxt = seg.seq.wrapping_add(1);
                    self.snd_una = seg.ack;
                    self.peer_wnd = usize::from(seg.window);
                    if let Some(m) = seg.mss {
                        self.set_mss(usize::from(m).clamp(88, self.our_mss));
                    }
                    match seg.wscale {
                        Some(s) => {
                            self.ws_ok = true;
                            self.snd_scale = s;
                        }
                        None => {
                            self.ws_ok = false;
                            self.rcv_scale = 0;
                        }
                    }
                    self.sack_ok = seg.sack_ok;
                    self.state = State::Established;
                    self.retries = 0;
                    self.rtt_on_ack(seg.ack, now);
                    out.push(self.ack_only());
                    self.pump(now, out);
                } else if seg.flags & ACK != 0 {
                    out.push(TcpOut {
                        seq: seg.ack,
                        flags: RST,
                        ..Default::default()
                    });
                }
                return res;
            }
            State::SynRcvd => {
                if seg.flags & SYN != 0 {
                    // Retransmitted SYN: resend the SYN-ACK.
                    out.push(self.syn_ack());
                    self.last_tx = now;
                    return res;
                }
                if seg.flags & ACK != 0 && seg.ack == self.iss.wrapping_add(1) {
                    self.snd_una = seg.ack;
                    self.state = State::Established;
                    self.retries = 0;
                    self.rtt_on_ack(seg.ack, now);
                } else {
                    return res;
                }
            }
            State::Established => {}
        }
        // Established from here.
        if seg.flags & SYN != 0 {
            out.push(self.ack_only());
            return res;
        }
        if seg.flags & ACK != 0 {
            let ack = seg.ack;
            let wnd = usize::from(seg.window) << self.snd_scale;
            if seq_lt(self.snd_una, ack) && !seq_lt(self.snd_max, ack) {
                let mut n = ack.wrapping_sub(self.snd_una) as usize;
                self.snd_una = ack;
                if seq_lt(self.snd_nxt, ack) {
                    self.snd_nxt = ack; // an ACK for data sent before a timeout rewind
                }
                if self.fin_queued && n > self.sndbuf.len() {
                    self.fin_acked = true;
                    n = self.sndbuf.len();
                }
                self.sndbuf.drain(..n);
                res.acked = n;
                self.retries = 0;
                self.last_tx = now;
                self.rtt_on_ack(ack, now);
                self.sack_normalize();
                if !seg.sack.is_empty() {
                    self.sack_update(&seg.sack);
                }
                self.on_new_ack(n, ack, out);
            } else if ack == self.snd_una
                && !seg.sack.is_empty()
                && seg.payload.is_empty()
                && seg.flags & FIN == 0
                && self.snd_una != self.snd_max
            {
                // Duplicate ACK carrying SACK information.
                self.sack_update(&seg.sack);
                self.on_dup_ack(out);
            } else if ack == self.snd_una
                && seg.payload.is_empty()
                && seg.flags & FIN == 0
                && self.snd_una != self.snd_max
                && wnd == self.peer_wnd
            {
                self.on_dup_ack(out);
            }
            self.peer_wnd = wnd;
        }
        // Payload and FIN.
        let fin_seq = seg.seq.wrapping_add(seg.payload.len() as u32);
        let mut seg_seq = seg.seq;
        let mut payload = seg.payload;
        if !payload.is_empty() && seq_lt(seg_seq, self.rcv_nxt) {
            let skip = self.rcv_nxt.wrapping_sub(seg_seq) as usize;
            if skip >= payload.len() {
                payload = &[];
            } else {
                payload = &payload[skip..];
            }
            seg_seq = self.rcv_nxt;
        }
        let mut need_ack = false;
        if !payload.is_empty() {
            if seg_seq == self.rcv_nxt {
                if can_accept {
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(payload.len() as u32);
                    self.bytes_up += payload.len() as u64;
                    let mut d = payload.to_vec();
                    self.drain_ooo(&mut d);
                    res.data = Some(d);
                    need_ack = true;
                }
            } else {
                if can_accept {
                    self.insert_ooo(seg_seq, payload);
                }
                need_ack = true; // duplicate ACK (with SACK) asks for the missing bytes
            }
        } else if !seg.payload.is_empty() {
            need_ack = true; // entirely old data
        }
        if seg.flags & FIN != 0 {
            if self.fin_rcvd {
                need_ack = true; // retransmitted FIN
            } else if fin_seq != self.rcv_nxt && seq_lt(self.rcv_nxt, fin_seq) {
                self.fin_pending = Some(fin_seq);
                need_ack = true;
            } else if fin_seq == self.rcv_nxt {
                self.fin_pending = Some(fin_seq);
            }
        }
        if let Some(fs) = self.fin_pending
            && !self.fin_rcvd
            && fs == self.rcv_nxt
        {
            self.fin_rcvd = true;
            self.fin_pending = None;
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            res.guest_fin = true;
            need_ack = true;
        }
        if need_ack {
            out.push(self.ack_only());
            self.last_tx = now;
        }
        self.pump(now, out);
        if self.fin_acked && self.fin_rcvd {
            self.state = State::Closed;
        }
        res
    }

    /// Timer. Returns true when the connection gave up (a RST has been queued).
    pub fn tick(&mut self, now: Instant, idle: Duration, out: &mut Vec<TcpOut>) -> bool {
        match self.state {
            State::Closed | State::Connecting => return false,
            _ => {}
        }
        if now.duration_since(self.last_activity) > idle {
            self.rst(out);
            return true;
        }
        let waiting = match self.state {
            State::SynRcvd | State::SynSent => true,
            State::Established => self.snd_una != self.snd_nxt,
            _ => false,
        };
        if waiting && now.duration_since(self.last_tx) >= self.rto {
            self.retries += 1;
            if self.retries > MAX_RETRIES {
                self.rst(out);
                return true;
            }
            self.rto = (self.rto * 2).min(MAX_RTO);
            self.rtt_probe = None; // Karn: nothing in flight is a valid sample any more
            match self.state {
                State::SynRcvd => {
                    out.push(self.syn_ack());
                    self.last_tx = now;
                }
                State::SynSent => {
                    out.push(self.syn());
                    self.last_tx = now;
                }
                _ => self.on_rto(now, out),
            }
        } else if self.state == State::Established
            && self.snd_una == self.snd_nxt
            && self.peer_wnd == 0
            && !self.sndbuf.is_empty()
            && now.duration_since(self.last_tx) >= Duration::from_secs(1)
        {
            // Zero-window probe: one byte beyond the window.
            let probe = self.data_seg(self.snd_nxt, ACK, self.copy_range(0, 1));
            out.push(probe);
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.note_sent();
            self.last_tx = now;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(seq: u32, ack: u32, flags: u8, payload: &[u8]) -> Tcp<'_> {
        Tcp {
            sport: 1,
            dport: 2,
            seq,
            ack,
            flags,
            window: 4096,
            mss: None,
            wscale: None,
            sack_ok: false,
            sack: Vec::new(),
            payload,
        }
    }

    fn established() -> (TcpConn, Instant) {
        let now = Instant::now();
        let mut c = TcpConn::new_passive(&seg(100, 0, SYN, &[]), 1460, now);
        let mut out = Vec::new();
        c.host_connected(now, &mut out);
        assert_eq!(out[0].flags, SYN | ACK);
        assert_eq!(out[0].ack, 101);
        let iss = out[0].seq;
        out.clear();
        c.on_segment(&seg(101, iss + 1, ACK, &[]), true, now, &mut out);
        assert_eq!(c.state, State::Established);
        (c, now)
    }

    /// Established with a guest that offered window scaling 7, SACK and MSS 1000, advertising `wnd` (scaled).
    fn established_ws(wnd: u16) -> (TcpConn, Instant) {
        let now = Instant::now();
        let mut syn = seg(100, 0, SYN, &[]);
        syn.mss = Some(1000);
        syn.wscale = Some(7);
        syn.sack_ok = true;
        let mut c = TcpConn::new_passive(&syn, 1460, now);
        let mut out = Vec::new();
        c.host_connected(now, &mut out);
        assert_eq!(out[0].wscale, Some(RCV_SCALE));
        assert!(out[0].sack_ok);
        assert_eq!(out[0].window, 65535, "the SYN-ACK window is never scaled");
        let iss = out[0].seq;
        out.clear();
        let mut a = seg(101, iss + 1, ACK, &[]);
        a.window = wnd;
        c.on_segment(&a, true, now, &mut out);
        assert_eq!(c.state, State::Established);
        (c, now)
    }

    #[test]
    fn data_both_ways_and_close() {
        let (mut c, now) = established();
        let mut out = Vec::new();
        let r = c.on_segment(
            &seg(101, c.snd_nxt, ACK | PSH, b"hello"),
            true,
            now,
            &mut out,
        );
        assert_eq!(r.data.as_deref(), Some(&b"hello"[..]));
        assert_eq!(out.last().unwrap().ack, 106);
        out.clear();
        c.push_host_data(&vec![7u8; 3000], now, &mut out);
        assert_eq!(out.len(), 6); // MSS 536 (no option in the SYN) x 5 + 320, window 4096
        assert_eq!(out[0].payload.len(), 536);
        let una = c.snd_una;
        out.clear();
        let r = c.on_segment(&seg(106, una + 3000, ACK, &[]), true, now, &mut out);
        assert_eq!(r.acked, 3000);
        c.host_eof(now, &mut out);
        assert_eq!(out.last().unwrap().flags & FIN, FIN);
        let fin_seq = out.last().unwrap().seq;
        out.clear();
        c.on_segment(&seg(106, fin_seq + 1, ACK | FIN, &[]), true, now, &mut out);
        assert_eq!(c.state, State::Closed);
    }

    #[test]
    fn out_of_order_is_buffered_and_backpressure_is_not_acked() {
        let (mut c, now) = established();
        let mut out = Vec::new();
        let r = c.on_segment(&seg(200, c.snd_nxt, ACK, b"later"), true, now, &mut out);
        assert!(r.data.is_none());
        assert_eq!(out[0].ack, 101); // dup ack
        out.clear();
        let r = c.on_segment(&seg(101, c.snd_nxt, ACK, b"x"), false, now, &mut out);
        assert!(r.data.is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn retransmits_on_timeout() {
        let (mut c, now) = established();
        let mut out = Vec::new();
        c.push_host_data(b"abc", now, &mut out);
        out.clear();
        let later = now + Duration::from_secs(1);
        c.tick(later, Duration::from_secs(60), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, b"abc");
    }

    #[test]
    fn window_scaling_is_negotiated_and_applied() {
        let (mut c, now) = established_ws(1000); // 1000 << 7 = 128000 bytes
        assert!(c.window_scaling() && c.sack_enabled());
        let mut out = Vec::new();
        c.push_host_data(&vec![1u8; 300_000], now, &mut out);
        let sent: usize = out.iter().map(|o| o.payload.len()).sum();
        // limited by the initial congestion window (10 x 1000), not by the unscaled 1000-byte window
        assert_eq!(sent, 10_000);
        assert_eq!(out[0].window, (RCV_WND >> RCV_SCALE) as u16);

        // Without the option the window field is taken literally.
        let (mut c, now) = established();
        assert!(!c.window_scaling());
        let mut out = Vec::new();
        c.push_host_data(&vec![1u8; 300_000], now, &mut out);
        assert_eq!(out.iter().map(|o| o.payload.len()).sum::<usize>(), 7 * 536);
        assert_eq!(out[0].window, 65535);
    }

    /// Regression: a slow host writer used to make the gateway drop guest bursts unacknowledged (the advertised
    /// window stayed at RCV_WND while the queue was capped at the same size), so every burst was retransmitted
    /// (~15% TcpRetransSegs on a 20 MB upload). The window now shrinks with the queue and reopens on drain.
    #[test]
    fn window_follows_host_queue_room_and_reopens_on_drain() {
        let (mut c, now) = established_ws(1000);
        let mss = c.mss;
        let mut out = Vec::new();
        let full = (RCV_WND >> RCV_SCALE) as u16;
        // Host queue holds all but 3 segments' worth: the guest is told so, and the segment is acked.
        c.note_host_room(3 * mss);
        let big = vec![7u8; mss];
        let s = seg(101, c.snd_nxt, ACK, &big);
        let r = c.on_segment(&s, true, now, &mut out);
        assert_eq!(r.data.as_ref().map(Vec::len), Some(mss));
        let w = out.last().unwrap().window;
        assert!(
            usize::from(w) << RCV_SCALE <= 3 * mss && w < full,
            "window shrinks with the queue"
        );
        // Queue full: advertised window is zero, not a silent drop.
        out.clear();
        c.note_host_room(0);
        let s = seg(101 + mss as u32, c.snd_nxt, ACK, &[7u8; 10]);
        c.on_segment(&s, true, now, &mut out);
        assert_eq!(out.last().unwrap().window, 0);
        // Draining reopens it with an unsolicited window update.
        out.clear();
        c.set_host_room(0, &mut out);
        assert!(out.is_empty(), "no update while still closed");
        c.set_host_room(RCV_WND, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].window, full);
        // An out-of-order segment inside the open window is buffered, one beyond a small window is not.
        out.clear();
        let rn = c.rcv_nxt;
        c.note_host_room(2 * mss);
        let far = seg(rn + 10 * mss as u32, c.snd_nxt, ACK, &[1u8; 100]);
        c.on_segment(&far, true, now, &mut out);
        assert!(out.last().unwrap().sack.is_empty(), "beyond the window");
        let near = seg(rn + mss as u32, c.snd_nxt, ACK, &[1u8; 100]);
        c.on_segment(&near, true, now, &mut out);
        assert_eq!(out.last().unwrap().sack.len(), 1);
    }

    #[test]
    fn out_of_order_data_is_reassembled_and_sacked() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        let nxt = c.snd_nxt;
        let b = |n: usize, v: u8| vec![v; n];
        // bytes 101..201 missing; 201..301 and 401..501 arrive
        let r = c.on_segment(&seg(201, nxt, ACK, &b(100, 2)), true, now, &mut out);
        assert!(r.data.is_none());
        assert_eq!(out.last().unwrap().ack, 101);
        assert_eq!(out.last().unwrap().sack, vec![(201, 301)]);
        let r = c.on_segment(&seg(401, nxt, ACK, &b(100, 4)), true, now, &mut out);
        assert!(r.data.is_none());
        // most recent block first
        assert_eq!(out.last().unwrap().sack, vec![(401, 501), (201, 301)]);
        // a segment bridging 301..401 merges the blocks
        c.on_segment(&seg(301, nxt, ACK, &b(100, 3)), true, now, &mut out);
        assert_eq!(out.last().unwrap().sack, vec![(201, 501)]);
        // the hole fills: everything is delivered in order, once
        let r = c.on_segment(&seg(101, nxt, ACK, &b(100, 1)), true, now, &mut out);
        let d = r.data.unwrap();
        assert_eq!(d.len(), 400);
        assert_eq!((d[0], d[100], d[200], d[300]), (1, 2, 3, 4));
        assert_eq!(out.last().unwrap().ack, 501);
        assert!(out.last().unwrap().sack.is_empty());
        assert_eq!(c.bytes_up, 400);
        // duplicate of delivered data only elicits an ACK
        let r = c.on_segment(&seg(201, nxt, ACK, &b(100, 2)), true, now, &mut out);
        assert!(r.data.is_none());
    }

    #[test]
    fn fin_ahead_of_a_hole_is_remembered() {
        let (mut c, now) = established();
        let mut out = Vec::new();
        let nxt = c.snd_nxt;
        let r = c.on_segment(&seg(106, nxt, ACK | FIN, &[]), true, now, &mut out);
        assert!(!r.guest_fin);
        let r = c.on_segment(&seg(101, nxt, ACK, b"hello"), true, now, &mut out);
        assert_eq!(r.data.as_deref(), Some(&b"hello"[..]));
        assert!(r.guest_fin);
        assert_eq!(out.last().unwrap().ack, 107);
    }

    fn ack_at(c: &TcpConn, ack: u32) -> Tcp<'static> {
        let mut s = seg(c.rcv_nxt, ack, ACK, &[]);
        s.window = 1000;
        s
    }

    #[test]
    fn slow_start_then_congestion_avoidance() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![0u8; 500_000], now, &mut out);
        let c0 = c.cwnd();
        assert_eq!(c0, 10_000);
        // ACK one segment: slow start grows cwnd by one MSS
        let una = c.snd_una;
        out.clear();
        c.on_segment(&ack_at(&c, una + 1000), true, now, &mut out);
        assert_eq!(c.cwnd(), c0 + 1000);
        assert!(!out.is_empty(), "the window opening releases new data");
        // Past ssthresh growth is ~ mss^2/cwnd per ACK
        c.ssthresh = c.cwnd;
        let before = c.cwnd();
        let una = c.snd_una;
        out.clear();
        c.on_segment(&ack_at(&c, una + 1000), true, now, &mut out);
        let grew = c.cwnd() - before;
        assert!((1..1000).contains(&grew), "grew by {grew}");
    }

    #[test]
    fn fast_retransmit_and_recovery() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![9u8; 100_000], now, &mut out);
        let una = c.snd_una;
        assert_eq!(out.len(), 10);
        // Segment 0 is lost; the guest sends duplicate ACKs for una.
        for i in 0..3 {
            out.clear();
            let mut s = ack_at(&c, una);
            s.sack = vec![(una + 1000, una + 2000)];
            c.on_segment(&s, true, now, &mut out);
            if i < 2 {
                assert!(out.is_empty(), "no retransmit before the third duplicate");
            }
        }
        assert!(c.in_recovery());
        let r = out
            .iter()
            .find(|o| o.seq == una)
            .expect("fast retransmit of the first segment");
        assert_eq!(r.payload.len(), 1000);
        assert_eq!(c.ssthresh(), 5_000);
        assert_eq!(c.cwnd(), 5_000 + 3_000);
        // More dup ACKs inflate the window.
        out.clear();
        c.on_segment(&ack_at(&c, una), true, now, &mut out);
        assert_eq!(c.cwnd(), 9_000);
        // A full ACK ends recovery with cwnd back near ssthresh.
        let max = c.snd_max;
        out.clear();
        c.on_segment(&ack_at(&c, max), true, now, &mut out);
        assert!(!c.in_recovery());
        assert!(
            c.cwnd() <= 5_000 + 1000 && c.cwnd() >= 1000,
            "cwnd {}",
            c.cwnd()
        );
    }

    #[test]
    fn partial_ack_in_recovery_retransmits_next_hole() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![9u8; 100_000], now, &mut out);
        let una = c.snd_una;
        for _ in 0..3 {
            out.clear();
            c.on_segment(&ack_at(&c, una), true, now, &mut out);
        }
        assert!(c.in_recovery());
        out.clear();
        // ACK covers the first retransmit but not everything: segment 1 is lost as well.
        c.on_segment(&ack_at(&c, una + 1000), true, now, &mut out);
        assert!(c.in_recovery());
        assert!(
            out.iter()
                .any(|o| o.seq == una + 1000 && o.payload.len() == 1000)
        );
    }

    #[test]
    fn rto_collapses_window_and_resends_from_una() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![5u8; 50_000], now, &mut out);
        assert_eq!(out.len(), 10);
        let una = c.snd_una;
        out.clear();
        c.tick(
            now + Duration::from_secs(1),
            Duration::from_secs(60),
            &mut out,
        );
        assert_eq!(out.len(), 1, "one segment after a timeout");
        assert_eq!((out[0].seq, out[0].payload.len()), (una, 1000));
        assert_eq!(c.cwnd(), 1000);
        assert_eq!(c.ssthresh(), 5000);
        // The old flight is acknowledged anyway: snd_nxt jumps forward and slow start continues.
        let max = c.snd_max;
        out.clear();
        c.on_segment(&ack_at(&c, max), true, now, &mut out);
        assert_eq!(
            c.snd_nxt.wrapping_sub(max),
            out.iter().map(|o| o.payload.len() as u32).sum::<u32>()
        );
        assert!(!out.is_empty());
    }

    /// A passive connection whose handshake took `rtt`, established at `t0 + rtt`.
    fn established_after(rtt: Duration) -> (TcpConn, Instant) {
        let t0 = Instant::now();
        let mut syn = seg(100, 0, SYN, &[]);
        syn.mss = Some(1000);
        syn.wscale = Some(7);
        syn.sack_ok = true;
        let mut c = TcpConn::new_passive(&syn, 1460, t0);
        let mut out = Vec::new();
        c.host_connected(t0, &mut out);
        let iss = out[0].seq;
        out.clear();
        let mut a = seg(101, iss + 1, ACK, &[]);
        a.window = 1000;
        c.on_segment(&a, true, t0 + rtt, &mut out);
        assert_eq!(c.state, State::Established);
        (c, t0 + rtt)
    }

    fn near(a: Duration, b: Duration) -> bool {
        let d = a.abs_diff(b);
        d < Duration::from_millis(2)
    }

    #[test]
    fn rto_follows_rfc6298() {
        let (mut c, t) = established_after(Duration::from_millis(100));
        // First sample: SRTT = R, RTTVAR = R/2, RTO = SRTT + 4*RTTVAR.
        assert!(near(c.srtt().unwrap(), Duration::from_millis(100)));
        assert!(near(c.rto(), Duration::from_millis(300)));
        let mut out = Vec::new();
        c.push_host_data(&[1u8; 500], t, &mut out);
        let una = c.snd_una;
        out.clear();
        c.on_segment(
            &ack_at(&c, una + 500),
            true,
            t + Duration::from_millis(200),
            &mut out,
        );
        // RTTVAR = 3/4*50 + 1/4*|100-200| = 62.5, SRTT = 7/8*100 + 1/8*200 = 112.5, RTO = 362.5.
        assert!(near(c.srtt().unwrap(), Duration::from_micros(112_500)));
        assert!(near(c.rto(), Duration::from_micros(362_500)));
    }

    #[test]
    fn rto_has_a_floor_and_slow_paths_do_not_retransmit_spuriously() {
        let (c, _) = established_after(Duration::from_millis(1));
        assert_eq!(c.rto(), MIN_RTO);
        // A 1.5 s path: the old fixed 400 ms RTO would have fired three times by now.
        let (mut c, t) = established_after(Duration::from_millis(1500));
        assert!(c.rto() >= Duration::from_secs(4));
        let mut out = Vec::new();
        c.push_host_data(&[1u8; 500], t, &mut out);
        out.clear();
        c.tick(
            t + Duration::from_millis(1400),
            Duration::from_secs(600),
            &mut out,
        );
        assert!(out.is_empty(), "no spurious retransmit");
    }

    #[test]
    fn karn_no_sample_from_retransmitted_data_and_backoff_is_capped() {
        let (mut c, mut t) = established_after(Duration::from_millis(100));
        let srtt = c.srtt().unwrap();
        let mut out = Vec::new();
        c.push_host_data(&[1u8; 500], t, &mut out);
        let una = c.snd_una;
        // Time out once: the data is retransmitted and the RTO doubles.
        t += c.rto();
        out.clear();
        c.tick(t, Duration::from_secs(3600), &mut out);
        assert_eq!(out.len(), 1);
        assert!(near(c.rto(), Duration::from_millis(600)));
        // The ACK of the retransmission is ambiguous: no sample is taken, the estimate is unchanged.
        t += Duration::from_millis(10);
        out.clear();
        c.on_segment(&ack_at(&c, una + 500), true, t, &mut out);
        assert_eq!(c.srtt().unwrap(), srtt);
        // The backoff survives until a clean sample...
        assert!(near(c.rto(), Duration::from_millis(600)));
        // ...which a fresh segment provides.
        c.push_host_data(&[1u8; 500], t, &mut out);
        let una = c.snd_una;
        out.clear();
        c.on_segment(
            &ack_at(&c, una + 500),
            true,
            t + Duration::from_millis(100),
            &mut out,
        );
        assert!(c.rto() < Duration::from_millis(600));

        // Repeated timeouts double up to MAX_RTO and then give up.
        c.push_host_data(&[1u8; 500], t, &mut out);
        let mut max_seen = Duration::ZERO;
        loop {
            t += c.rto();
            out.clear();
            let gave_up = c.tick(t, Duration::from_secs(36_000), &mut out);
            max_seen = max_seen.max(c.rto());
            if gave_up {
                break;
            }
        }
        assert_eq!(max_seen, MAX_RTO);
    }

    #[test]
    fn sack_scoreboard_retransmits_only_the_holes() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![9u8; 100_000], now, &mut out);
        assert_eq!(out.len(), 10);
        let una = c.snd_una;
        let max = c.snd_max;
        // Segments 0, 3 and 6 are lost; the guest SACKs the rest as it arrives.
        let blocks = [
            vec![(una + 1000, una + 3000)],
            vec![(una + 4000, una + 6000), (una + 1000, una + 3000)],
            vec![
                (una + 7000, una + 10_000),
                (una + 4000, una + 6000),
                (una + 1000, una + 3000),
            ],
        ];
        let mut rexmit: Vec<u32> = Vec::new();
        for b in blocks.iter().cycle().take(6) {
            out.clear();
            let mut s = ack_at(&c, una);
            s.sack = b.clone();
            c.on_segment(&s, true, now, &mut out);
            for o in &out {
                if !o.payload.is_empty() && seq_lt(o.seq, max) {
                    assert_eq!(o.payload.len(), 1000);
                    rexmit.push(o.seq);
                }
            }
        }
        assert!(c.in_recovery());
        rexmit.sort_unstable();
        assert_eq!(
            rexmit,
            vec![una, una + 3000, una + 6000],
            "only holes, each once"
        );
        // The retransmissions fill everything: a cumulative ACK ends recovery.
        out.clear();
        c.on_segment(&ack_at(&c, max), true, now, &mut out);
        assert!(!c.in_recovery());
        assert!(c.scoreboard.is_empty());
    }

    #[test]
    fn sack_blocks_are_clamped_and_merged() {
        let (mut c, now) = established_ws(1000);
        let mut out = Vec::new();
        c.push_host_data(&vec![9u8; 10_000], now, &mut out);
        let una = c.snd_una;
        // Garbage (beyond snd_max, empty, below una) is ignored; overlapping blocks merge.
        c.sack_update(&[
            (una + 20_000, una + 21_000),
            (una + 2000, una + 2000),
            (una.wrapping_sub(500), una),
            (una + 1000, una + 3000),
            (una + 2500, una + 4000),
            (una + 4000, una + 5000),
        ]);
        assert_eq!(c.scoreboard, vec![(una + 1000, una + 5000)]);
        assert_eq!(c.next_hole(), Some((una, una + 1000)));
    }
}
