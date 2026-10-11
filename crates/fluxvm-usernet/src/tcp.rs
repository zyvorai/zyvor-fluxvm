// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The guest-facing half of one proxied TCP connection. We terminate the guest's TCP: from the guest's view we are
//! the remote host (or, for a host->guest forward, the client). Payload is relayed to a host-side stream by the
//! engine; this state machine only does sequence numbers, acknowledgements, windows and retransmission.
//!
//! Simplifications: no window scaling or SACK (so <=64 KiB in flight), no congestion control beyond a fixed 64 KiB
//! window, out-of-order guest data is dropped and re-requested by duplicate ACK.

use crate::wire::{ACK, FIN, PSH, RST, SYN, Tcp, TcpOut};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub const WINDOW: u16 = 65535;
const MAX_RETRIES: u32 = 8;
const MAX_RTO: Duration = Duration::from_secs(8);

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
    rcv_nxt: u32,
    peer_wnd: usize,
    sndbuf: VecDeque<u8>,
    fin_queued: bool,
    fin_sent: bool,
    pub fin_acked: bool,
    pub fin_rcvd: bool,
    last_tx: Instant,
    pub last_activity: Instant,
    rto: Duration,
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
            rcv_nxt: 0,
            peer_wnd: 0,
            sndbuf: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            fin_acked: false,
            fin_rcvd: false,
            last_tx: now,
            last_activity: now,
            rto: Duration::from_millis(400),
            retries: 0,
            bytes_up: 0,
            bytes_down: 0,
        }
    }

    /// The guest sent `syn`; the host connection is still being made.
    pub fn new_passive(syn: &Tcp<'_>, our_mss: usize, now: Instant) -> Self {
        let mut c = Self::blank(our_mss, now);
        c.rcv_nxt = syn.seq.wrapping_add(1);
        c.mss = usize::from(syn.mss.unwrap_or(536)).clamp(88, our_mss);
        c.peer_wnd = usize::from(syn.window);
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
        self.last_tx = now;
    }

    fn syn_ack(&self) -> TcpOut {
        TcpOut {
            seq: self.iss,
            ack: self.rcv_nxt,
            flags: SYN | ACK,
            window: WINDOW,
            mss: Some(self.our_mss as u16),
            payload: Vec::new(),
        }
    }

    /// Active open towards the guest (a published port): emits the SYN.
    pub fn new_active(our_mss: usize, now: Instant, out: &mut Vec<TcpOut>) -> Self {
        let mut c = Self::blank(our_mss, now);
        c.state = State::SynSent;
        out.push(TcpOut {
            seq: c.iss,
            flags: SYN,
            window: WINDOW,
            mss: Some(our_mss as u16),
            ..Default::default()
        });
        c.snd_nxt = c.iss.wrapping_add(1);
        c
    }

    fn ack_only(&self) -> TcpOut {
        TcpOut {
            seq: self.snd_nxt,
            ack: self.rcv_nxt,
            flags: ACK,
            window: WINDOW,
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

    fn data_in_flight(&self) -> usize {
        (self.snd_nxt.wrapping_sub(self.snd_una) as usize).saturating_sub(usize::from(self.fin_sent))
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

    fn pump(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        if self.state != State::Established {
            return;
        }
        let before = out.len();
        let wnd = self.peer_wnd.min(usize::from(WINDOW));
        let mut sent = self.data_in_flight();
        while sent < self.sndbuf.len() && !self.fin_sent {
            let flight = self.data_in_flight();
            if flight >= wnd {
                break;
            }
            let n = self.mss.min(self.sndbuf.len() - sent).min(wnd - flight);
            let more = sent + n < self.sndbuf.len();
            out.push(TcpOut {
                seq: self.snd_nxt,
                ack: self.rcv_nxt,
                flags: ACK | if more { 0 } else { PSH },
                window: WINDOW,
                payload: self.copy_range(sent, n),
                ..Default::default()
            });
            self.snd_nxt = self.snd_nxt.wrapping_add(n as u32);
            sent += n;
        }
        if self.fin_queued && !self.fin_sent && sent == self.sndbuf.len() {
            out.push(TcpOut {
                seq: self.snd_nxt,
                ack: self.rcv_nxt,
                flags: FIN | ACK,
                window: WINDOW,
                ..Default::default()
            });
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.fin_sent = true;
        }
        if out.len() > before {
            self.last_tx = now;
        }
    }

    fn retransmit(&mut self, now: Instant, out: &mut Vec<TcpOut>) {
        let flight = self.data_in_flight();
        let mut off = 0;
        while off < flight {
            let n = self.mss.min(flight - off);
            out.push(TcpOut {
                seq: self.snd_una.wrapping_add(off as u32),
                ack: self.rcv_nxt,
                flags: ACK,
                window: WINDOW,
                payload: self.copy_range(off, n),
                ..Default::default()
            });
            off += n;
        }
        if self.fin_sent && !self.fin_acked {
            out.push(TcpOut {
                seq: self.snd_nxt.wrapping_sub(1),
                ack: self.rcv_nxt,
                flags: FIN | ACK,
                window: WINDOW,
                ..Default::default()
            });
        }
        self.last_tx = now;
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
                        self.mss = usize::from(m).clamp(88, self.our_mss);
                    }
                    self.state = State::Established;
                    self.retries = 0;
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
            if seq_lt(self.snd_una, ack) && !seq_lt(self.snd_nxt, ack) {
                let mut n = ack.wrapping_sub(self.snd_una) as usize;
                self.snd_una = ack;
                if self.fin_sent && ack == self.snd_nxt {
                    self.fin_acked = true;
                    n -= 1;
                }
                let n = n.min(self.sndbuf.len());
                self.sndbuf.drain(..n);
                res.acked = n;
                self.retries = 0;
                self.rto = Duration::from_millis(400);
                self.last_tx = now;
            }
            self.peer_wnd = usize::from(seg.window);
        }
        // Payload and FIN, in order only.
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
            if seg_seq == self.rcv_nxt && can_accept {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(payload.len() as u32);
                self.bytes_up += payload.len() as u64;
                res.data = Some(payload.to_vec());
                need_ack = true;
            } else if seg_seq != self.rcv_nxt {
                need_ack = true; // duplicate ACK asks for the missing bytes
            }
        }
        if seg.flags & FIN != 0
            && !self.fin_rcvd
            && seg_seq.wrapping_add(payload.len() as u32) == self.rcv_nxt
            && (payload.is_empty() || res.data.is_some())
        {
            self.fin_rcvd = true;
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            res.guest_fin = true;
            need_ack = true;
        } else if seg.flags & FIN != 0 && self.fin_rcvd {
            need_ack = true; // retransmitted FIN
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
            State::Established => {
                self.snd_una != self.snd_nxt
            }
            _ => false,
        };
        if waiting && now.duration_since(self.last_tx) >= self.rto {
            self.retries += 1;
            if self.retries > MAX_RETRIES {
                self.rst(out);
                return true;
            }
            self.rto = (self.rto * 2).min(MAX_RTO);
            match self.state {
                State::SynRcvd => {
                    out.push(self.syn_ack());
                    self.last_tx = now;
                }
                State::SynSent => {
                    out.push(TcpOut {
                        seq: self.iss,
                        flags: SYN,
                        window: WINDOW,
                        mss: Some(self.our_mss as u16),
                        ..Default::default()
                    });
                    self.last_tx = now;
                }
                _ => self.retransmit(now, out),
            }
        } else if self.state == State::Established
            && self.snd_una == self.snd_nxt
            && self.peer_wnd == 0
            && !self.sndbuf.is_empty()
            && now.duration_since(self.last_tx) >= Duration::from_secs(1)
        {
            // Zero-window probe: one byte beyond the window.
            out.push(TcpOut {
                seq: self.snd_nxt,
                ack: self.rcv_nxt,
                flags: ACK,
                window: WINDOW,
                payload: self.copy_range(0, 1),
                ..Default::default()
            });
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
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

    #[test]
    fn data_both_ways_and_close() {
        let (mut c, now) = established();
        let mut out = Vec::new();
        let r = c.on_segment(&seg(101, c.snd_nxt, ACK | PSH, b"hello"), true, now, &mut out);
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
    fn out_of_order_and_backpressure_are_not_acked_as_data() {
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
}
