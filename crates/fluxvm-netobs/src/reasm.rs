// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Bounded reassembly of the first bytes of a byte stream.
//!
//! Application-layer metadata (a TLS hello split over segments, HTTP/2 frames, a QUIC CRYPTO stream) lives in the
//! first few kilobytes of a stream, so that is all this keeps: a contiguous prefix of at most `max_bytes_per_stream`
//! bytes plus a short list of out-of-order segments waiting for the gap in front of them to fill.
//!
//! Rules:
//!
//! * Sequence numbers are relative to the stream's base (the ISN + 1 when the SYN was seen, otherwise the first
//!   sequence number observed). Wrap-around is handled; bytes before the base are treated as old.
//! * Retransmissions and overlaps never rewrite bytes already placed in the contiguous prefix. An overlap whose
//!   bytes differ from what was kept is counted in [`Stream::conflicts`] (an evasion signal) and the first bytes
//!   win. Among out-of-order segments, the one at the lower offset wins an overlap.
//! * Everything is bounded: per-stream bytes, number of pending segments, and a caller-supplied `room` that
//!   represents the observer-wide budget. Anything over a bound is dropped and reported, never queued.

/// Limits for stream reassembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasmConfig {
    /// Master switch. When off only single-segment messages are parsed (the pre-reassembly behaviour).
    pub enabled: bool,
    /// How many bytes from the start of each direction of a stream are kept (default 16 KiB).
    pub max_bytes_per_stream: usize,
    /// Out-of-order segments held per direction.
    pub max_pending_segments: usize,
    /// Observer-wide cap on bytes held by all streams together. Streams over it are abandoned and counted.
    pub max_total_bytes: usize,
}

impl Default for ReasmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes_per_stream: 16 * 1024,
            max_pending_segments: 16,
            max_total_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Why a segment was not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropWhy {
    /// Too many (or too many bytes of) out-of-order segments for this stream.
    PendingFull,
    /// The observer-wide byte budget has no room.
    Budget,
}

/// What [`Stream::push`] did with a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// This many new bytes were appended to the contiguous prefix.
    Appended(usize),
    /// Held out of order, waiting for the gap in front of it.
    Buffered,
    /// Nothing new: a retransmission, bytes before the base, or bytes beyond the kept prefix.
    Ignored,
    Dropped(DropWhy),
}

/// One direction of one connection (or one QUIC CRYPTO stream).
#[derive(Debug, Clone, Default)]
pub struct Stream {
    base: Option<u32>,
    buf: Vec<u8>,
    pending: Vec<(usize, Vec<u8>)>,
    pending_bytes: usize,
    /// Overlaps whose bytes disagreed with what was already kept.
    pub conflicts: u32,
}

impl Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// A stream whose first byte has sequence number `base`.
    pub fn with_base(base: u32) -> Self {
        Self {
            base: Some(base),
            ..Self::default()
        }
    }

    /// Sets the sequence number of the first byte unless one is already known (the SYN's ISN + 1).
    pub fn ensure_base(&mut self, base: u32) {
        self.base.get_or_insert(base);
    }

    /// The contiguous prefix received so far.
    pub fn contiguous(&self) -> &[u8] {
        &self.buf
    }

    /// Bytes of heap this stream holds (what counts against the budget).
    pub fn mem(&self) -> usize {
        self.buf.capacity()
            + self
                .pending
                .iter()
                .map(|(_, d)| d.capacity())
                .sum::<usize>()
    }

    pub fn pending_segments(&self) -> usize {
        self.pending.len()
    }

    /// Frees all buffers (the stream is finished or abandoned).
    pub fn release(&mut self) {
        self.buf = Vec::new();
        self.pending = Vec::new();
        self.pending_bytes = 0;
    }

    /// Offers a segment. `limit` caps the contiguous prefix, `max_segs` the pending list, and `room` is how many
    /// more bytes the caller's budget can take.
    pub fn push(
        &mut self,
        seq: u32,
        data: &[u8],
        limit: usize,
        max_segs: usize,
        room: usize,
    ) -> Push {
        if data.is_empty() || limit == 0 {
            return Push::Ignored;
        }
        let base = *self.base.get_or_insert(seq);
        let rel = seq.wrapping_sub(base) as i32;
        let (off, data) = if rel < 0 {
            let skip = rel.unsigned_abs() as usize;
            if skip >= data.len() {
                return Push::Ignored;
            }
            (0usize, &data[skip..])
        } else {
            (rel as usize, data)
        };
        if off >= limit {
            return Push::Ignored;
        }
        let data = &data[..data.len().min(limit - off)];
        let have = self.buf.len();
        if off <= have {
            let overlap = (have - off).min(data.len());
            if overlap > 0 && self.buf[off..off + overlap] != data[..overlap] {
                self.conflicts = self.conflicts.saturating_add(1);
            }
            let fresh = &data[overlap..];
            if fresh.is_empty() {
                return Push::Ignored;
            }
            if fresh.len() > room {
                return Push::Dropped(DropWhy::Budget);
            }
            self.buf.extend_from_slice(fresh);
            let added = fresh.len() + self.drain_pending(limit);
            if self.buf.len() >= limit {
                self.pending = Vec::new();
                self.pending_bytes = 0;
            }
            return Push::Appended(added);
        }
        // A gap in front of this segment.
        if self
            .pending
            .iter()
            .any(|(o, d)| *o == off && d.len() >= data.len())
        {
            return Push::Ignored;
        }
        if self.pending.len() >= max_segs || self.pending_bytes + data.len() > limit {
            return Push::Dropped(DropWhy::PendingFull);
        }
        if data.len() > room {
            return Push::Dropped(DropWhy::Budget);
        }
        let at = self.pending.partition_point(|(o, _)| *o <= off);
        self.pending_bytes += data.len();
        self.pending.insert(at, (off, data.to_vec()));
        Push::Buffered
    }

    fn drain_pending(&mut self, limit: usize) -> usize {
        let mut added = 0;
        while self
            .pending
            .first()
            .is_some_and(|(o, _)| *o <= self.buf.len())
        {
            let (o, d) = self.pending.remove(0);
            self.pending_bytes -= d.len();
            let skip = self.buf.len() - o;
            if skip < d.len() && self.buf.len() < limit {
                let take = (d.len() - skip).min(limit - self.buf.len());
                self.buf.extend_from_slice(&d[skip..skip + take]);
                added += take;
            }
        }
        added
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM: usize = 1024;
    const ROOM: usize = usize::MAX;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + 3) as u8).collect()
    }

    #[test]
    fn in_order_segments_concatenate() {
        let d = data(300);
        let mut s = Stream::with_base(1000);
        assert_eq!(s.push(1000, &d[..100], LIM, 8, ROOM), Push::Appended(100));
        assert_eq!(
            s.push(1100, &d[100..300], LIM, 8, ROOM),
            Push::Appended(200)
        );
        assert_eq!(s.contiguous(), &d[..]);
    }

    #[test]
    fn out_of_order_fills_when_the_gap_closes() {
        let d = data(300);
        let mut s = Stream::with_base(0);
        assert_eq!(s.push(200, &d[200..300], LIM, 8, ROOM), Push::Buffered);
        assert_eq!(s.push(100, &d[100..200], LIM, 8, ROOM), Push::Buffered);
        assert!(s.contiguous().is_empty());
        assert_eq!(s.push(0, &d[..100], LIM, 8, ROOM), Push::Appended(300));
        assert_eq!(s.contiguous(), &d[..]);
        assert_eq!(s.pending_segments(), 0);
    }

    #[test]
    fn retransmits_and_overlaps_are_deduplicated() {
        let d = data(300);
        let mut s = Stream::with_base(0);
        s.push(0, &d[..150], LIM, 8, ROOM);
        assert_eq!(
            s.push(0, &d[..150], LIM, 8, ROOM),
            Push::Ignored,
            "exact retransmit"
        );
        assert_eq!(
            s.push(50, &d[50..120], LIM, 8, ROOM),
            Push::Ignored,
            "fully covered"
        );
        assert_eq!(
            s.push(100, &d[100..300], LIM, 8, ROOM),
            Push::Appended(150),
            "partial overlap adds the tail"
        );
        assert_eq!(s.contiguous(), &d[..]);
        assert_eq!(s.conflicts, 0);
    }

    #[test]
    fn conflicting_overlap_keeps_first_bytes_and_counts() {
        let mut s = Stream::with_base(0);
        s.push(0, b"AAAAAAAAAA", LIM, 8, ROOM);
        s.push(5, b"BBBBBBBBBB", LIM, 8, ROOM);
        assert_eq!(s.contiguous(), b"AAAAAAAAAABBBBB");
        assert_eq!(s.conflicts, 1);
    }

    #[test]
    fn sequence_wraparound() {
        let d = data(100);
        let base = u32::MAX - 49;
        let mut s = Stream::with_base(base);
        assert_eq!(s.push(base, &d[..50], LIM, 8, ROOM), Push::Appended(50));
        assert_eq!(
            s.push(base.wrapping_add(50), &d[50..], LIM, 8, ROOM),
            Push::Appended(50)
        );
        assert_eq!(s.contiguous(), &d[..]);
        // Old data from before the base.
        assert_eq!(
            s.push(base.wrapping_sub(10), &d[..5], LIM, 8, ROOM),
            Push::Ignored
        );
    }

    #[test]
    fn prefix_is_capped_and_later_bytes_ignored() {
        let d = data(4000);
        let mut s = Stream::with_base(0);
        let mut off = 0;
        while off < d.len() {
            let n = 500.min(d.len() - off);
            s.push(off as u32, &d[off..off + n], LIM, 8, ROOM);
            off += n;
        }
        assert_eq!(s.contiguous().len(), LIM);
        assert_eq!(s.contiguous(), &d[..LIM]);
        assert_eq!(s.push(5000, &d[..10], LIM, 8, ROOM), Push::Ignored);
    }

    #[test]
    fn pending_is_bounded_and_drops_are_reported() {
        let mut s = Stream::with_base(0);
        // Segments beyond the gap at offset 0.
        for i in 0..4u32 {
            assert_eq!(
                s.push(100 + i * 10, &[1u8; 5], LIM, 4, ROOM),
                Push::Buffered
            );
        }
        assert_eq!(
            s.push(300, &[1u8; 5], LIM, 4, ROOM),
            Push::Dropped(DropWhy::PendingFull)
        );
        // Byte-bound as well.
        let mut s = Stream::with_base(0);
        assert_eq!(s.push(10, &[1u8; 1000], LIM, 8, ROOM), Push::Buffered);
        assert_eq!(
            s.push(20, &[1u8; 100], LIM, 8, ROOM),
            Push::Dropped(DropWhy::PendingFull)
        );
    }

    #[test]
    fn budget_is_respected() {
        let mut s = Stream::with_base(0);
        assert_eq!(
            s.push(0, &[1u8; 100], LIM, 8, 50),
            Push::Dropped(DropWhy::Budget)
        );
        assert_eq!(
            s.push(10, &[1u8; 100], LIM, 8, 50),
            Push::Dropped(DropWhy::Budget)
        );
        assert!(s.contiguous().is_empty());
        assert_eq!(s.mem(), 0);
    }

    #[test]
    fn release_frees() {
        let mut s = Stream::with_base(0);
        s.push(0, &[1u8; 400], LIM, 8, ROOM);
        s.push(900, &[1u8; 50], LIM, 8, ROOM);
        assert!(s.mem() >= 450);
        s.release();
        assert_eq!(s.mem(), 0);
    }

    /// Random segmentation, reordering, duplication and overlap of a known stream always reassembles to the
    /// original prefix (with generous bounds).
    #[test]
    fn randomized_reordering_reassembles_exactly() {
        let mut st = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        for round in 0..300 {
            let n = 200 + (next() % 800) as usize;
            let d = data(n);
            let base = next() as u32;
            // Cut into segments, then add duplicates and overlapping variants.
            let mut segs: Vec<(usize, usize)> = Vec::new();
            let mut at = 0;
            while at < n {
                let l = 1 + (next() % 120) as usize;
                let l = l.min(n - at);
                segs.push((at, l));
                if next() % 4 == 0 {
                    segs.push((at, l));
                }
                if next() % 5 == 0 && at > 0 {
                    let back = 1 + (next() % at.min(40) as u64) as usize;
                    segs.push((at - back, (l + back).min(n - (at - back))));
                }
                at += l;
            }
            for i in (1..segs.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                segs.swap(i, j);
            }
            let mut s = Stream::with_base(base);
            for (o, l) in &segs {
                let r = s.push(
                    base.wrapping_add(*o as u32),
                    &d[*o..*o + *l],
                    4096,
                    4096,
                    ROOM,
                );
                assert!(!matches!(r, Push::Dropped(_)), "round {round}");
            }
            assert_eq!(s.contiguous(), &d[..], "round {round}");
            assert_eq!(s.conflicts, 0);
        }
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut st = 77u64;
        let mut next = move || {
            st = st
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (st >> 20) as u32
        };
        let mut s = Stream::new();
        for _ in 0..20_000 {
            let seq = next();
            let len = (next() % 300) as usize;
            let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = s.push(seq, &buf, 2048, 8, 1 << 20);
            assert!(s.contiguous().len() <= 2048);
            assert!(s.pending_segments() <= 8);
        }
    }
}
