// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A small export channel from a datapath process to the process that owns the [`Observer`].
//!
//! `fluxvm-vz-switch` runs as one process per private network, so its frames cannot be handed to the daemon's
//! observer in-process. The switch runs a [`BridgeClient`]; the daemon runs a [`BridgeServer`] on a unix stream
//! socket and ingests what arrives. The client never blocks the datapath: frames go into a bounded queue with
//! `try_send`, and anything that cannot be delivered (queue full, peer gone, write timeout, oversize) is dropped
//! and counted, as with [`crate::NetTap`]. The client reconnects with exponential backoff.
//!
//! # Wire format, version 1 (all integers big-endian)
//!
//! A connection starts with a 16-byte preamble sent by the client:
//!
//! ```text
//! "FXNB" | version u8 (=1) | flags u8 (=0) | reserved u16 (=0) | client_id u64
//! ```
//!
//! then any number of records, each `length u32 | type u8 | body`, where `length` counts the type byte and body
//! (so it is at least 1 and at most [`MAX_RECORD`]):
//!
//! ```text
//! 1 IDENT  id u32 | mac [6] | vm_len u8 | vm | net_len u8 | network | nic_len u8 | nic
//! 2 FRAME  id u32 | ts_us u64 | direction u8 | verdict u8 | reason u8 | frame bytes...
//! 3 STATS  sent u64 | dropped u64 [| queue_full u64 | disconnected u64 | oversize u64 | write_error u64
//!                                  | truncated_to_snaplen u64 | switch_forward_dropped u64]
//!                                            (client totals since it started; lets the server account
//!                                             drops that never reached it. The bracketed part is optional
//!                                             and read when present. It is the first record on every
//!                                             connection, so the server has a baseline for `sent`.)
//! ```
//!
//! `IDENT` binds a connection-local id to an [`Identity`] and precedes the first `FRAME` that uses it; ids are
//! per connection, so a reconnect starts over. `direction`: 0 from guest, 1 to guest. `verdict`: 0 allowed,
//! 1 denied, 2 dropped. `reason`: index into [`DropReason::ALL`] or 0xff. Unknown record types are skipped by
//! length, so v1 can grow; an unknown preamble version closes the connection.
//!
//! Stream sockets only: the framing needs ordered, lossless delivery of whole records, and a datagram socket
//! would force every frame to repeat its identity.
//!
//! # Failure handling
//!
//! * A record longer than [`MAX_RECORD`] is skipped (and counted) if it is shorter than [`SKIP_LIMIT`], otherwise
//!   the connection is closed because framing can no longer be trusted.
//! * A connection that ends mid-record counts one `truncated`; a partial record is never ingested.
//! * Frames that arrive for an id with no `IDENT` are dropped and counted.
//! * The socket file is created with mode 0600.

use crate::flow::{Direction, DropReason, FrameMeta, Identity, Verdict};
use crate::observer::Observer;
use crate::source::{BridgeLoss, REASON_SERVER_REJECTED, REASON_UNSPECIFIED, now_us};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAGIC: [u8; 4] = *b"FXNB";
pub const WIRE_VERSION: u8 = 1;
/// The largest Ethernet frame carried (a jumbo or offloaded frame is cut to this).
pub const MAX_FRAME: usize = 65_535;
/// Largest `length` field accepted in normal operation.
pub const MAX_RECORD: usize = MAX_FRAME + 64;
/// Records longer than [`MAX_RECORD`] but not longer than this are skipped; beyond it the connection is dropped.
pub const SKIP_LIMIT: usize = 4 * 1024 * 1024;
/// Identities one connection may define.
pub const MAX_IDENTS_PER_CONN: usize = 4096;
const PREAMBLE_LEN: usize = 16;

const T_IDENT: u8 = 1;
const T_FRAME: u8 = 2;
const T_STATS: u8 = 3;

const FRAME_FIXED: usize = 1 + 4 + 8 + 3;

fn verdict_code(v: Verdict) -> (u8, u8) {
    match v {
        Verdict::Allowed => (0, 0xff),
        Verdict::Denied(r) => (1, r.index() as u8),
        Verdict::Dropped(r) => (2, r.index() as u8),
    }
}

fn verdict_from(kind: u8, reason: u8) -> Option<Verdict> {
    let r = || {
        DropReason::ALL
            .get(reason as usize)
            .copied()
            .unwrap_or(DropReason::Other)
    };
    match kind {
        0 => Some(Verdict::Allowed),
        1 => Some(Verdict::Denied(r())),
        2 => Some(Verdict::Dropped(r())),
        _ => None,
    }
}

/// Encodes the preamble.
pub fn encode_preamble(client_id: u64) -> [u8; PREAMBLE_LEN] {
    let mut p = [0u8; PREAMBLE_LEN];
    p[..4].copy_from_slice(&MAGIC);
    p[4] = WIRE_VERSION;
    p[8..].copy_from_slice(&client_id.to_be_bytes());
    p
}

/// Appends an IDENT record to `out`. `None` when a name does not fit in 255 bytes.
pub fn encode_ident(out: &mut Vec<u8>, id: u32, identity: &Identity) -> Option<()> {
    let names = [
        identity.vm_id.as_bytes(),
        identity.network.as_bytes(),
        identity.nic.as_bytes(),
    ];
    if names.iter().any(|n| n.len() > 255) {
        return None;
    }
    let len = 1 + 4 + 6 + names.iter().map(|n| 1 + n.len()).sum::<usize>();
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.push(T_IDENT);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&identity.mac);
    for n in names {
        out.push(n.len() as u8);
        out.extend_from_slice(n);
    }
    Some(())
}

/// Appends a FRAME record to `out`. `None` when the frame exceeds [`MAX_FRAME`].
pub fn encode_frame(out: &mut Vec<u8>, id: u32, meta: &FrameMeta, frame: &[u8]) -> Option<()> {
    if frame.len() > MAX_FRAME {
        return None;
    }
    encode_frame_parts(out, id, meta.ts_us, meta.direction, meta.verdict, frame);
    Some(())
}

/// [`encode_frame`] without the [`FrameMeta`]; `frame` must not exceed [`MAX_FRAME`].
fn encode_frame_parts(
    out: &mut Vec<u8>,
    id: u32,
    ts_us: u64,
    direction: Direction,
    verdict: Verdict,
    frame: &[u8],
) {
    let (kind, reason) = verdict_code(verdict);
    out.extend_from_slice(&((FRAME_FIXED + frame.len()) as u32).to_be_bytes());
    out.push(T_FRAME);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&ts_us.to_be_bytes());
    out.push(match direction {
        Direction::FromGuest => 0,
        Direction::ToGuest => 1,
    });
    out.push(kind);
    out.push(reason);
    out.extend_from_slice(frame);
}

/// Appends a STATS record (totals only) to `out`.
pub fn encode_stats(out: &mut Vec<u8>, sent: u64, dropped: u64) {
    out.extend_from_slice(&17u32.to_be_bytes());
    out.push(T_STATS);
    out.extend_from_slice(&sent.to_be_bytes());
    out.extend_from_slice(&dropped.to_be_bytes());
}

/// The part of a STATS record added after the first release of v1: drops split by reason and two more
/// counters. An old server ignores the extra bytes; a new server accepts a record without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsExt {
    /// queue_full, disconnected, oversize, write_error (the order of [`crate::source::BRIDGE_DROP_REASONS`]).
    pub dropped_by_reason: [u64; 4],
    pub truncated_to_snaplen: u64,
    pub switch_forward_dropped: u64,
}

/// Appends a STATS record carrying [`StatsExt`] to `out`.
pub fn encode_stats_ext(out: &mut Vec<u8>, sent: u64, dropped: u64, ext: &StatsExt) {
    out.extend_from_slice(&((1 + 16 + 6 * 8) as u32).to_be_bytes());
    out.push(T_STATS);
    out.extend_from_slice(&sent.to_be_bytes());
    out.extend_from_slice(&dropped.to_be_bytes());
    for v in ext.dropped_by_reason {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out.extend_from_slice(&ext.truncated_to_snaplen.to_be_bytes());
    out.extend_from_slice(&ext.switch_forward_dropped.to_be_bytes());
}

/// A decoded record body (the bytes after the length prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded<'a> {
    Ident {
        id: u32,
        identity: Identity,
    },
    Frame {
        id: u32,
        ts_us: u64,
        direction: Direction,
        verdict: Verdict,
        data: &'a [u8],
    },
    Stats {
        sent: u64,
        dropped: u64,
        ext: Option<StatsExt>,
    },
    /// A record type this version does not know: skipped.
    Unknown(u8),
}

/// Why a record body could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Empty,
    Short,
    BadUtf8,
    BadDirection,
    BadVerdict,
    TrailingBytes,
}

/// Decodes one record body (`type | fields`). Never panics.
pub fn decode_body(b: &[u8]) -> Result<Decoded<'_>, DecodeError> {
    let (&ty, rest) = b.split_first().ok_or(DecodeError::Empty)?;
    match ty {
        T_IDENT => {
            if rest.len() < 4 + 6 {
                return Err(DecodeError::Short);
            }
            let id = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let mut mac = [0u8; 6];
            mac.copy_from_slice(&rest[4..10]);
            let mut p = 10;
            let mut take = || -> Result<&str, DecodeError> {
                let n = *rest.get(p).ok_or(DecodeError::Short)? as usize;
                let s = rest.get(p + 1..p + 1 + n).ok_or(DecodeError::Short)?;
                p += 1 + n;
                std::str::from_utf8(s).map_err(|_| DecodeError::BadUtf8)
            };
            let vm = take()?;
            let network = take()?;
            let nic = take()?;
            if p != rest.len() {
                return Err(DecodeError::TrailingBytes);
            }
            Ok(Decoded::Ident {
                id,
                identity: Identity {
                    vm_id: vm.into(),
                    network: network.into(),
                    nic: nic.into(),
                    mac,
                },
            })
        }
        T_FRAME => {
            if rest.len() < FRAME_FIXED - 1 {
                return Err(DecodeError::Short);
            }
            let id = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
            let ts_us = u64::from_be_bytes([
                rest[4], rest[5], rest[6], rest[7], rest[8], rest[9], rest[10], rest[11],
            ]);
            let direction = match rest[12] {
                0 => Direction::FromGuest,
                1 => Direction::ToGuest,
                _ => return Err(DecodeError::BadDirection),
            };
            let verdict = verdict_from(rest[13], rest[14]).ok_or(DecodeError::BadVerdict)?;
            Ok(Decoded::Frame {
                id,
                ts_us,
                direction,
                verdict,
                data: &rest[15..],
            })
        }
        T_STATS => {
            if rest.len() < 16 {
                return Err(DecodeError::Short);
            }
            let sent = u64::from_be_bytes(rest[..8].try_into().map_err(|_| DecodeError::Short)?);
            let dropped =
                u64::from_be_bytes(rest[8..16].try_into().map_err(|_| DecodeError::Short)?);
            let ext = if rest.len() >= 16 + 6 * 8 {
                let u = |i: usize| {
                    u64::from_be_bytes(rest[16 + 8 * i..24 + 8 * i].try_into().unwrap_or([0; 8]))
                };
                Some(StatsExt {
                    dropped_by_reason: [u(0), u(1), u(2), u(3)],
                    truncated_to_snaplen: u(4),
                    switch_forward_dropped: u(5),
                })
            } else {
                None
            };
            Ok(Decoded::Stats { sent, dropped, ext })
        }
        t => Ok(Decoded::Unknown(t)),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BridgeClientConfig {
    /// Frames that may wait for the writer thread.
    pub queue_capacity: usize,
    /// Encoded bytes that may wait for the writer thread (the queue is full at whichever limit comes first).
    pub queue_bytes: usize,
    /// Frames longer than this are cut (at most [`MAX_FRAME`]).
    pub snaplen: usize,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    /// A write that stalls this long drops the connection (the peer is stuck), so the queue keeps draining.
    pub write_timeout: Duration,
    /// How often the client reports its totals (when they changed) so the server can account for drops.
    pub stats_interval: Duration,
}

impl Default for BridgeClientConfig {
    fn default() -> Self {
        Self {
            // About 100 ms of a 300 kpps guest, and 16 MiB (some 10k MTU-size frames).
            queue_capacity: 32_768,
            queue_bytes: 16 * 1024 * 1024,
            snaplen: 2048,
            reconnect_min: Duration::from_millis(50),
            reconnect_max: Duration::from_secs(2),
            write_timeout: Duration::from_millis(250),
            stats_interval: Duration::from_millis(250),
        }
    }
}

/// Client-side counters. Every frame offered through [`BridgeClient::tap_as`] ends up in exactly one of
/// `sent` (written to the socket) or one of the `dropped_*` counters, or is still queued.
#[derive(Debug, Default)]
pub struct BridgeClientStats {
    /// Frames accepted into the queue.
    pub queued: AtomicU64,
    /// Frames fully written to the socket.
    pub sent: AtomicU64,
    pub truncated_to_snaplen: AtomicU64,
    pub dropped_queue_full: AtomicU64,
    pub dropped_disconnected: AtomicU64,
    pub dropped_oversize: AtomicU64,
    pub dropped_write_error: AtomicU64,
    /// Frames the switch observed but did not deliver to the destination guest (see
    /// [`BridgeClient::note_forward_drop`]). Not part of [`Self::total_dropped`]: the observer saw them.
    pub switch_forward_dropped: AtomicU64,
    pub connects: AtomicU64,
    pub reconnects: AtomicU64,
    pub connect_failures: AtomicU64,
}

impl BridgeClientStats {
    pub fn total_dropped(&self) -> u64 {
        self.dropped_by_reason().iter().sum()
    }

    /// Drops in the order of the first four labels of [`crate::source::BRIDGE_DROP_REASONS`]: queue_full,
    /// disconnected, oversize, write_error.
    pub fn dropped_by_reason(&self) -> [u64; 4] {
        [
            Self::get(&self.dropped_queue_full),
            Self::get(&self.dropped_disconnected),
            Self::get(&self.dropped_oversize),
            Self::get(&self.dropped_write_error),
        ]
    }

    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    fn snapshot(&self) -> (u64, u64, StatsExt) {
        let by = self.dropped_by_reason();
        (
            Self::get(&self.sent),
            by.iter().sum(),
            StatsExt {
                dropped_by_reason: by,
                truncated_to_snaplen: Self::get(&self.truncated_to_snaplen),
                switch_forward_dropped: Self::get(&self.switch_forward_dropped),
            },
        )
    }
}

/// The queue the datapath fills and the writer thread empties: frames already encoded as wire records, so the
/// datapath does one short lock and one copy per frame, and the writer does one `write` per batch.
struct Queue {
    buf: Vec<u8>,
    frames: usize,
    /// Identities in id order (the id is the index). Append-only: a reconnect announces them all again.
    idents: Vec<Arc<Identity>>,
    index: HashMap<Arc<Identity>, u32>,
    /// The identity most recently used, to skip hashing when one port sends a run of frames.
    last: Option<(Arc<Identity>, u32)>,
}

struct Core {
    q: Mutex<Queue>,
    cv: Condvar,
    /// A connection is up. When it is not, frames are counted as dropped without being queued.
    connected: AtomicBool,
    stats: Arc<BridgeClientStats>,
    /// Frames the writer is finished with (written or dropped), for [`BridgeClient::flush`].
    settled: AtomicU64,
    stop: AtomicBool,
    snaplen: usize,
    cap_frames: usize,
    cap_bytes: usize,
}

struct ClientShared {
    core: Arc<Core>,
    stats: Arc<BridgeClientStats>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ClientShared {
    fn drop(&mut self) {
        self.core.stop.store(true, Ordering::Relaxed);
        self.core.cv.notify_all();
        if let Some(j) = self.join.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = j.join();
        }
    }
}

/// The sending side. Cheap to clone; the writer thread stops when the last clone is dropped.
#[derive(Clone)]
pub struct BridgeClient {
    shared: Arc<ClientShared>,
}

impl BridgeClient {
    /// Starts a client that keeps trying to reach the server at `path`. Never blocks and never fails: a server
    /// that is not there yet simply means frames are dropped (and counted) until it is.
    pub fn connect(path: impl Into<PathBuf>, cfg: BridgeClientConfig) -> BridgeClient {
        let path = path.into();
        let stats = Arc::new(BridgeClientStats::default());
        let core = Arc::new(Core {
            q: Mutex::new(Queue {
                buf: Vec::with_capacity(64 * 1024),
                frames: 0,
                idents: Vec::new(),
                index: HashMap::new(),
                last: None,
            }),
            cv: Condvar::new(),
            connected: AtomicBool::new(false),
            stats: stats.clone(),
            settled: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            snaplen: cfg.snaplen.clamp(64, MAX_FRAME),
            cap_frames: cfg.queue_capacity.max(1),
            cap_bytes: cfg.queue_bytes.max(MAX_RECORD + 4),
        });
        let client_id = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash, Hasher};
            (std::process::id(), now_us(), &path).hash(&mut h);
            h.finish()
        };
        let c = core.clone();
        let join = std::thread::Builder::new()
            .name("fluxvm-netobs-bridge-tx".into())
            .spawn(move || client_loop(path, cfg, c, client_id))
            .ok();
        BridgeClient {
            shared: Arc::new(ClientShared {
                core,
                stats,
                join: Mutex::new(join),
            }),
        }
    }

    pub fn stats(&self) -> &Arc<BridgeClientStats> {
        &self.shared.stats
    }

    /// Queues a frame for `identity`. Never blocks beyond a short, uncontended lock.
    pub fn tap_as(
        &self,
        identity: &Arc<Identity>,
        direction: Direction,
        verdict: Verdict,
        frame: &[u8],
    ) {
        let c = &self.shared.core;
        let st = &c.stats;
        let keep = frame.len().min(c.snaplen);
        if keep < frame.len() {
            st.truncated_to_snaplen.fetch_add(1, Ordering::Relaxed);
        }
        if !c.connected.load(Ordering::Relaxed) {
            st.dropped_disconnected.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ts_us = now_us();
        let mut q = c.q.lock().unwrap_or_else(|e| e.into_inner());
        let id = match &q.last {
            Some((l, id)) if Arc::ptr_eq(l, identity) => Some(*id),
            _ => match q.index.get(identity) {
                Some(id) => Some(*id),
                None => {
                    let id = q.idents.len();
                    let encodable = {
                        let mut scratch = Vec::new();
                        encode_ident(&mut scratch, 0, identity).is_some()
                    };
                    if id >= MAX_IDENTS_PER_CONN || !encodable {
                        None
                    } else {
                        q.idents.push(identity.clone());
                        q.index.insert(identity.clone(), id as u32);
                        Some(id as u32)
                    }
                }
            },
        };
        let Some(id) = id else {
            drop(q);
            st.dropped_oversize.fetch_add(1, Ordering::Relaxed);
            return;
        };
        q.last = Some((identity.clone(), id));
        let need = 4 + FRAME_FIXED + keep;
        if q.frames >= c.cap_frames || q.buf.len() + need > c.cap_bytes {
            drop(q);
            st.dropped_queue_full.fetch_add(1, Ordering::Relaxed);
            return;
        }
        encode_frame_parts(&mut q.buf, id, ts_us, direction, verdict, &frame[..keep]);
        q.frames += 1;
        let first = q.frames == 1;
        drop(q);
        st.queued.fetch_add(1, Ordering::Relaxed);
        if first {
            c.cv.notify_one();
        }
    }

    /// A handle bound to `identity` that implements [`crate::Tap`].
    pub fn for_identity(&self, identity: Arc<Identity>) -> BridgeTap {
        BridgeTap {
            client: self.clone(),
            identity,
        }
    }

    /// Records that the switch observed a frame but could not hand it to its destination (a full receive
    /// queue). It is reported to the server as `switch_forward_dropped`; it is not an observation loss.
    pub fn note_forward_drop(&self) {
        self.shared
            .stats
            .switch_forward_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Blocks until everything queued so far has been written or dropped, or `timeout` passes. For tests and
    /// orderly shutdown; the datapath never calls it.
    pub fn flush(&self, timeout: Duration) -> bool {
        let c = &self.shared.core;
        let deadline = Instant::now() + timeout;
        loop {
            if c.settled.load(Ordering::Relaxed) >= BridgeClientStats::get(&c.stats.queued) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A [`BridgeClient`] bound to one guest port's identity.
#[derive(Clone)]
pub struct BridgeTap {
    client: BridgeClient,
    identity: Arc<Identity>,
}

impl crate::source::Tap for BridgeTap {
    fn tap(&self, direction: Direction, verdict: Verdict, frame: &[u8]) {
        self.client
            .tap_as(&self.identity, direction, verdict, frame);
    }
}

/// How many whole records of `data` (all FRAME records) end within the first `written` bytes.
fn complete_records(data: &[u8], written: usize) -> usize {
    let (mut pos, mut n) = (0usize, 0usize);
    while pos + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let end = pos + 4 + len;
        if end > written || end > data.len() {
            break;
        }
        n += 1;
        pos = end;
    }
    n
}

/// Writes all of `data`, returning how many bytes left before an error (equal to the length on success).
fn write_counted(s: &mut UnixStream, data: &[u8]) -> (usize, bool) {
    let mut off = 0;
    while off < data.len() {
        match s.write(&data[off..]) {
            Ok(0) => return (off, false),
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (off, false),
        }
    }
    (off, true)
}

fn client_loop(path: PathBuf, cfg: BridgeClientConfig, core: Arc<Core>, client_id: u64) {
    let stats = core.stats.clone();
    let mut conn: Option<UnixStream> = None;
    let mut ever_connected = false;
    let mut announced = 0usize;
    let mut backoff = cfg.reconnect_min;
    let mut next_attempt = Instant::now();
    let mut spare: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut hdr: Vec<u8> = Vec::new();
    let mut last_stats: Option<(u64, u64, StatsExt)> = None;
    let mut last_stats_at = Instant::now();
    let send_stats = |s: &mut UnixStream, last: &mut Option<(u64, u64, StatsExt)>| -> bool {
        let now = stats.snapshot();
        if last.as_ref() == Some(&now) {
            return true;
        }
        let mut b = Vec::with_capacity(80);
        encode_stats_ext(&mut b, now.0, now.1, &now.2);
        let ok = s.write_all(&b).is_ok();
        if ok {
            *last = Some(now);
        }
        ok
    };
    let mut final_pass = false;
    loop {
        if core.stop.load(Ordering::Relaxed) {
            if final_pass {
                break;
            }
            // One more pass to hand whatever is queued to the socket (or count it) before leaving.
            final_pass = true;
        }
        if conn.is_none() && Instant::now() >= next_attempt && !final_pass {
            match UnixStream::connect(&path) {
                Ok(mut s) => {
                    let _ = s.set_write_timeout(Some(cfg.write_timeout));
                    if s.write_all(&encode_preamble(client_id)).is_ok() {
                        announced = 0;
                        backoff = cfg.reconnect_min;
                        stats.connects.fetch_add(1, Ordering::Relaxed);
                        if ever_connected {
                            stats.reconnects.fetch_add(1, Ordering::Relaxed);
                        }
                        ever_connected = true;
                        // The first record on a connection is a STATS: the server takes `sent` from it as the
                        // baseline for what it should go on to receive.
                        last_stats = None;
                        if send_stats(&mut s, &mut last_stats) {
                            last_stats_at = Instant::now();
                            conn = Some(s);
                            core.connected.store(true, Ordering::Relaxed);
                        } else {
                            stats.connect_failures.fetch_add(1, Ordering::Relaxed);
                            next_attempt = Instant::now() + backoff;
                        }
                    } else {
                        stats.connect_failures.fetch_add(1, Ordering::Relaxed);
                        next_attempt = Instant::now() + backoff;
                    }
                }
                Err(_) => {
                    stats.connect_failures.fetch_add(1, Ordering::Relaxed);
                    next_attempt = Instant::now() + backoff;
                    backoff = (backoff * 2).min(cfg.reconnect_max);
                }
            }
        }

        // Take everything queued in one swap.
        let (n, new_idents) = {
            let mut q = core.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.frames == 0 && !final_pass {
                q = core
                    .cv
                    .wait_timeout(q, Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            if q.frames == 0 {
                (0, Vec::new())
            } else {
                spare.clear();
                std::mem::swap(&mut q.buf, &mut spare);
                let n = q.frames;
                q.frames = 0;
                let from = announced.min(q.idents.len());
                (n, q.idents[from..].to_vec())
            }
        };
        if n > 0 {
            match conn.as_mut() {
                None => {
                    stats
                        .dropped_disconnected
                        .fetch_add(n as u64, Ordering::Relaxed);
                }
                Some(s) => {
                    hdr.clear();
                    for (k, ident) in new_idents.iter().enumerate() {
                        let _ = encode_ident(&mut hdr, (announced + k) as u32, ident);
                    }
                    let (hw, hok) = write_counted(s, &hdr);
                    let (complete, ok) = if hok {
                        let (w, ok) = write_counted(s, &spare);
                        (complete_records(&spare, w), ok)
                    } else {
                        let _ = hw;
                        (0, false)
                    };
                    stats.sent.fetch_add(complete as u64, Ordering::Relaxed);
                    if ok {
                        announced += new_idents.len();
                    } else {
                        stats
                            .dropped_write_error
                            .fetch_add((n - complete) as u64, Ordering::Relaxed);
                        conn = None;
                        core.connected.store(false, Ordering::Relaxed);
                        next_attempt = Instant::now() + cfg.reconnect_min;
                    }
                }
            }
            core.settled.fetch_add(n as u64, Ordering::Relaxed);
        }
        if let Some(s) = conn.as_mut()
            && last_stats_at.elapsed() >= cfg.stats_interval
        {
            if !send_stats(s, &mut last_stats) {
                conn = None;
                core.connected.store(false, Ordering::Relaxed);
                next_attempt = Instant::now() + cfg.reconnect_min;
            }
            last_stats_at = Instant::now();
        }
    }
    core.connected.store(false, Ordering::Relaxed);
    // Report the final totals so the server's account is complete.
    if let Some(s) = conn.as_mut() {
        let _ = send_stats(s, &mut last_stats);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BridgeServerConfig {
    /// Simultaneous client connections served; further ones are closed and counted.
    pub max_connections: usize,
    /// Frames ingested per observer lock acquisition at most.
    pub batch: usize,
    /// A connection that stops in the middle of a record for this long is closed.
    pub stall_timeout: Duration,
}

impl Default for BridgeServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 64,
            batch: 64,
            stall_timeout: Duration::from_secs(10),
        }
    }
}

/// Server-side counters.
#[derive(Debug, Default)]
pub struct BridgeServerStats {
    pub connections: AtomicU64,
    pub connections_rejected: AtomicU64,
    pub active: AtomicU64,
    pub frames: AtomicU64,
    pub idents: AtomicU64,
    pub stats_records: AtomicU64,
    pub unknown_records: AtomicU64,
    pub oversize_records: AtomicU64,
    pub truncated: AtomicU64,
    pub bad_version: AtomicU64,
    pub bad_records: AtomicU64,
    pub unknown_ident: AtomicU64,
    /// Drops the clients reported (frames that never left the switch).
    pub client_reported_dropped: AtomicU64,
}

impl BridgeServerStats {
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }
}

/// Receives frames from [`BridgeClient`]s and ingests them into an [`Observer`].
pub struct BridgeServer {
    listener: UnixListener,
    path: PathBuf,
    cfg: BridgeServerConfig,
    stats: Arc<BridgeServerStats>,
    loss: Arc<BridgeLoss>,
    stop: Arc<AtomicBool>,
}

/// What the server remembers about one client (a switch process) across its connections.
#[derive(Default)]
struct ClientAcct {
    /// Last cumulative drop counters reported, by reason (queue_full, disconnected, oversize, write_error).
    by_reason: [u64; 4],
    /// Last cumulative total, for clients that do not send reasons.
    dropped_total: u64,
    truncated: u64,
    forward: u64,
    /// The client's `sent` when this server first heard from it: only frames after it are expected here.
    sent_base: Option<u64>,
    /// Frames received on connections of this client that have since ended.
    received_closed: u64,
}

type Clients = Arc<Mutex<HashMap<u64, ClientAcct>>>;

/// Adds a connection's received-frame count to its client's account when the connection ends, however it ends.
struct ConnGuard {
    clients: Clients,
    client_id: u64,
    received: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut g = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(self.client_id).or_default().received_closed += self.received;
    }
}

/// Stops a running [`BridgeServer`] from another thread.
#[derive(Clone)]
pub struct BridgeStop(Arc<AtomicBool>);

impl BridgeStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A [`BridgeServer`] running on its own thread.
pub struct BridgeRunning {
    stop: BridgeStop,
    join: Option<JoinHandle<io::Result<()>>>,
    pub stats: Arc<BridgeServerStats>,
    pub loss: Arc<BridgeLoss>,
}

impl BridgeRunning {
    /// Stops the server, waits for it, and removes the socket file.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.stop.stop();
        match self.join.take() {
            Some(j) => j
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("bridge server panicked"))),
            None => Ok(()),
        }
    }
}

impl Drop for BridgeRunning {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

impl BridgeServer {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<BridgeServer> {
        Self::bind_with(path, BridgeServerConfig::default())
    }

    /// Binds a listening socket at `path` (mode 0600). A stale socket file left by a dead server is replaced; a
    /// live one is an `AddrInUse` error.
    pub fn bind_with(path: impl AsRef<Path>, cfg: BridgeServerConfig) -> io::Result<BridgeServer> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "a bridge server is already listening",
                    ));
                }
                Err(_) => std::fs::remove_file(&path)?,
            }
        }
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(BridgeServer {
            listener,
            path,
            cfg,
            stats: Arc::new(BridgeServerStats::default()),
            loss: Arc::new(BridgeLoss::default()),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> Arc<BridgeServerStats> {
        self.stats.clone()
    }

    pub fn stop_handle(&self) -> BridgeStop {
        BridgeStop(self.stop.clone())
    }

    /// Serves until stopped: accepts connections and ingests their frames into `observer`. Also ages the
    /// observer's flows while traffic is idle. The bridge's loss accounting is registered with the observer
    /// (alongside, never instead of, an in-process tap), so frames received and every drop reported by clients
    /// appear in its summary as `bridge_*` and are added into `tap_enqueued` / `tap_dropped`.
    pub fn run(self, observer: Arc<Observer>) -> io::Result<()> {
        observer.add_bridge_loss(self.loss.clone());
        let clients: Clients = Arc::new(Mutex::new(HashMap::new()));
        // Each worker with a second handle to its socket, so stopping can unblock a read that is waiting.
        let mut workers: Vec<(JoinHandle<()>, Option<UnixStream>)> = Vec::new();
        let finish = |workers: Vec<(JoinHandle<()>, Option<UnixStream>)>| {
            for (h, ctl) in workers {
                if let Some(c) = ctl {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
                let _ = h.join();
            }
        };
        let mut last_tick = Instant::now();
        while !self.stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((conn, _)) => {
                    workers.retain(|(w, _)| !w.is_finished());
                    if workers.len() >= self.cfg.max_connections.max(1) {
                        self.stats
                            .connections_rejected
                            .fetch_add(1, Ordering::Relaxed);
                        drop(conn);
                        continue;
                    }
                    self.stats.connections.fetch_add(1, Ordering::Relaxed);
                    let w = Worker {
                        obs: observer.clone(),
                        cfg: self.cfg.clone(),
                        stats: self.stats.clone(),
                        loss: self.loss.clone(),
                        stop: self.stop.clone(),
                        clients: clients.clone(),
                    };
                    let ctl = conn.try_clone().ok();
                    if let Ok(h) = std::thread::Builder::new()
                        .name("fluxvm-netobs-bridge-rx".into())
                        .spawn(move || {
                            w.serve(conn);
                        })
                    {
                        workers.push((h, ctl));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.stop.store(true, Ordering::Relaxed);
                    finish(workers);
                    let _ = std::fs::remove_file(&self.path);
                    return Err(e);
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(250) {
                observer.tick(now_us());
                last_tick = Instant::now();
            }
        }
        finish(workers);
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }

    /// [`Self::run`] on a new thread.
    pub fn spawn(self, observer: Arc<Observer>) -> BridgeRunning {
        let stop = self.stop_handle();
        let stats = self.stats();
        let loss = self.loss.clone();
        let join = std::thread::Builder::new()
            .name("fluxvm-netobs-bridge".into())
            .spawn(move || self.run(observer))
            .ok();
        BridgeRunning {
            stop,
            join,
            stats,
            loss,
        }
    }
}

#[derive(PartialEq, Eq, Debug)]
enum Fill {
    Full,
    /// End of stream before any byte.
    Eof,
    /// End of stream part-way through.
    Truncated,
    Stopped,
    Stalled,
}

/// Reads exactly `buf.len()` bytes, tolerating read timeouts so `stop` is honoured.
fn fill<R: Read>(
    r: &mut R,
    buf: &mut [u8],
    stop: &AtomicBool,
    stall: Duration,
) -> io::Result<Fill> {
    let mut got = 0;
    let mut last = Instant::now();
    while got < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return Ok(Fill::Stopped);
        }
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(if got == 0 { Fill::Eof } else { Fill::Truncated }),
            Ok(n) => {
                got += n;
                last = Instant::now();
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if got > 0 && last.elapsed() > stall {
                    return Ok(Fill::Stalled);
                }
                // Guards against spinning if the socket turned out to be non-blocking.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Fill::Full)
}

struct Worker {
    obs: Arc<Observer>,
    cfg: BridgeServerConfig,
    stats: Arc<BridgeServerStats>,
    loss: Arc<BridgeLoss>,
    stop: Arc<AtomicBool>,
    clients: Clients,
}

impl Worker {
    fn serve(self, conn: UnixStream) {
        self.stats.active.fetch_add(1, Ordering::Relaxed);
        // `run` keeps a second descriptor for unblocking reads at shutdown, so dropping `conn` alone would not
        // hang up on the client; shut the socket down explicitly when this connection is done.
        let hangup = conn.try_clone().ok();
        let _ = self.serve_inner(conn);
        if let Some(h) = hangup {
            let _ = h.shutdown(std::net::Shutdown::Both);
        }
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
    }

    fn flush(&self, arena: &mut Vec<u8>, batch: &mut Vec<(Range<usize>, FrameMeta)>) {
        if batch.is_empty() {
            return;
        }
        self.obs
            .ingest_batch(batch.iter().map(|(r, m)| (&arena[r.clone()], m)));
        self.stats
            .frames
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
        self.loss
            .frames
            .fetch_add(batch.len() as u64, Ordering::Relaxed);
        batch.clear();
        arena.clear();
    }

    fn serve_inner(&self, conn: UnixStream) -> io::Result<()> {
        // Best effort: on some platforms these fail with EINVAL when the peer has already closed, in which case
        // the reads below see its data and EOF straight away. `run` can always unblock a read with `shutdown`.
        let _ = conn.set_nonblocking(false);
        let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
        let mut r = BufReader::with_capacity(256 * 1024, conn);
        let stall = self.cfg.stall_timeout;

        let mut pre = [0u8; PREAMBLE_LEN];
        match fill(&mut r, &mut pre, &self.stop, stall)? {
            Fill::Full => {}
            Fill::Truncated | Fill::Stalled => {
                self.stats.truncated.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            Fill::Eof | Fill::Stopped => return Ok(()),
        }
        if pre[..4] != MAGIC || pre[4] != WIRE_VERSION {
            self.stats.bad_version.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let client_id = u64::from_be_bytes(pre[8..16].try_into().unwrap_or([0; 8]));
        let mut guard = ConnGuard {
            clients: self.clients.clone(),
            client_id,
            received: 0,
        };

        let mut idents: HashMap<u32, Arc<Identity>> = HashMap::new();
        let mut arena: Vec<u8> = Vec::new();
        let mut batch: Vec<(Range<usize>, FrameMeta)> = Vec::new();
        let mut rec: Vec<u8> = Vec::new();
        let mut scratch = [0u8; 4096];
        loop {
            if r.buffer().is_empty() {
                self.flush(&mut arena, &mut batch);
            }
            let mut lenb = [0u8; 4];
            match fill(&mut r, &mut lenb, &self.stop, stall)? {
                Fill::Full => {}
                Fill::Eof | Fill::Stopped => break,
                Fill::Truncated | Fill::Stalled => {
                    self.stats.truncated.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
            let len = u32::from_be_bytes(lenb) as usize;
            if len == 0 {
                self.stats.bad_records.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if len > MAX_RECORD {
                self.stats.oversize_records.fetch_add(1, Ordering::Relaxed);
                if len > SKIP_LIMIT {
                    break; // framing can no longer be trusted
                }
                let mut left = len;
                let mut ok = true;
                while left > 0 {
                    let n = left.min(scratch.len());
                    match fill(&mut r, &mut scratch[..n], &self.stop, stall)? {
                        Fill::Full => left -= n,
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    self.stats.truncated.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                continue;
            }
            rec.resize(len, 0);
            match fill(&mut r, &mut rec, &self.stop, stall)? {
                Fill::Full => {}
                Fill::Stopped => break,
                Fill::Eof | Fill::Truncated | Fill::Stalled => {
                    self.stats.truncated.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
            match decode_body(&rec) {
                Ok(Decoded::Ident { id, identity }) => {
                    if idents.len() >= MAX_IDENTS_PER_CONN && !idents.contains_key(&id) {
                        self.stats.bad_records.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.stats.idents.fetch_add(1, Ordering::Relaxed);
                        idents.insert(id, Arc::new(identity));
                    }
                }
                Ok(Decoded::Frame {
                    id,
                    ts_us,
                    direction,
                    verdict,
                    data,
                }) => match idents.get(&id) {
                    Some(identity) => {
                        guard.received += 1;
                        let start = arena.len();
                        arena.extend_from_slice(data);
                        batch.push((
                            start..arena.len(),
                            FrameMeta {
                                identity: identity.clone(),
                                ts_us,
                                direction,
                                verdict,
                            },
                        ));
                        if batch.len() >= self.cfg.batch.max(1) {
                            self.flush(&mut arena, &mut batch);
                        }
                    }
                    None => {
                        guard.received += 1;
                        self.stats.unknown_ident.fetch_add(1, Ordering::Relaxed);
                        self.loss.dropped[REASON_SERVER_REJECTED].fetch_add(1, Ordering::Relaxed);
                    }
                },
                Ok(Decoded::Stats { sent, dropped, ext }) => {
                    self.stats.stats_records.fetch_add(1, Ordering::Relaxed);
                    let mut delta_total = 0u64;
                    let unconfirmed = {
                        let mut g = self.clients.lock().unwrap_or_else(|e| e.into_inner());
                        if g.len() >= 1024 && !g.contains_key(&client_id) {
                            if let Some(k) = g.keys().next().copied() {
                                g.remove(&k);
                            }
                        }
                        let a = g.entry(client_id).or_default();
                        let base = *a
                            .sent_base
                            .get_or_insert(sent.saturating_sub(guard.received));
                        match ext {
                            Some(e) => {
                                for (i, v) in e.dropped_by_reason.iter().enumerate() {
                                    let d = v.saturating_sub(a.by_reason[i]);
                                    a.by_reason[i] = *v;
                                    if d > 0 {
                                        self.loss.dropped[i].fetch_add(d, Ordering::Relaxed);
                                        delta_total += d;
                                    }
                                }
                                a.dropped_total = dropped;
                                let t = e.truncated_to_snaplen.saturating_sub(a.truncated);
                                a.truncated = e.truncated_to_snaplen;
                                self.loss
                                    .truncated_to_snaplen
                                    .fetch_add(t, Ordering::Relaxed);
                                let f = e.switch_forward_dropped.saturating_sub(a.forward);
                                a.forward = e.switch_forward_dropped;
                                self.loss
                                    .switch_forward_dropped
                                    .fetch_add(f, Ordering::Relaxed);
                            }
                            None => {
                                let d = dropped.saturating_sub(a.dropped_total);
                                a.dropped_total = dropped;
                                if d > 0 {
                                    self.loss.dropped[REASON_UNSPECIFIED]
                                        .fetch_add(d, Ordering::Relaxed);
                                    delta_total += d;
                                }
                            }
                        }
                        (sent.saturating_sub(base))
                            .saturating_sub(a.received_closed + guard.received)
                    };
                    self.loss
                        .unconfirmed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(client_id, unconfirmed);
                    if delta_total > 0 {
                        self.stats
                            .client_reported_dropped
                            .fetch_add(delta_total, Ordering::Relaxed);
                    }
                }
                Ok(Decoded::Unknown(_)) => {
                    self.stats.unknown_records.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.stats.bad_records.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.flush(&mut arena, &mut batch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l7::fixtures as l7f;
    use crate::observer::{FlowQuery, ObserverConfig};
    use crate::packet::build::{self, tcp_v4, udp_v4};
    use crate::packet::{TCP_ACK, TCP_PSH, TCP_SYN};
    use std::net::Ipv4Addr;

    const G: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 5);
    const S: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);

    fn ident(vm: &str) -> Arc<Identity> {
        Identity::new(vm, "net-a", "private", build::GUEST_MAC)
    }

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn sock(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("nb.sock")
    }

    fn meta(id: &Arc<Identity>, ts: u64, d: Direction, v: Verdict) -> FrameMeta {
        FrameMeta {
            identity: id.clone(),
            ts_us: ts,
            direction: d,
            verdict: v,
        }
    }

    // ---- wire format --------------------------------------------------------------------------------------

    #[test]
    fn records_round_trip() {
        let id = ident("vm-ü");
        let mut out = Vec::new();
        encode_ident(&mut out, 7, &id).unwrap();
        let m = meta(
            &id,
            123_456,
            Direction::ToGuest,
            Verdict::Denied(DropReason::Policy),
        );
        encode_frame(&mut out, 7, &m, b"frame-bytes").unwrap();
        encode_stats(&mut out, 5, 2);

        let mut at = 0;
        let mut recs = Vec::new();
        while at < out.len() {
            let len = u32::from_be_bytes(out[at..at + 4].try_into().unwrap()) as usize;
            recs.push(decode_body(&out[at + 4..at + 4 + len]).unwrap());
            at += 4 + len;
        }
        assert_eq!(recs.len(), 3);
        match &recs[0] {
            Decoded::Ident { id: i, identity } => {
                assert_eq!(*i, 7);
                assert_eq!(identity, &*id);
            }
            o => panic!("{o:?}"),
        }
        assert_eq!(
            recs[1],
            Decoded::Frame {
                id: 7,
                ts_us: 123_456,
                direction: Direction::ToGuest,
                verdict: Verdict::Denied(DropReason::Policy),
                data: b"frame-bytes"
            }
        );
        assert_eq!(
            recs[2],
            Decoded::Stats {
                sent: 5,
                dropped: 2,
                ext: None
            }
        );
    }

    #[test]
    fn every_verdict_and_reason_round_trips() {
        for v in std::iter::once(Verdict::Allowed)
            .chain(DropReason::ALL.iter().map(|r| Verdict::Denied(*r)))
            .chain(DropReason::ALL.iter().map(|r| Verdict::Dropped(*r)))
        {
            let (k, r) = verdict_code(v);
            assert_eq!(verdict_from(k, r), Some(v));
        }
        assert_eq!(verdict_from(9, 0), None);
    }

    #[test]
    fn decoder_rejects_garbage_without_panicking() {
        let id = ident("vm");
        let mut good = Vec::new();
        encode_ident(&mut good, 1, &id).unwrap();
        encode_frame(
            &mut good,
            1,
            &meta(&id, 1, Direction::FromGuest, Verdict::Allowed),
            &[1, 2, 3],
        )
        .unwrap();
        // Every truncation of every record body.
        let mut at = 0;
        while at < good.len() {
            let len = u32::from_be_bytes(good[at..at + 4].try_into().unwrap()) as usize;
            let body = &good[at + 4..at + 4 + len];
            for n in 0..body.len() {
                let _ = decode_body(&body[..n]);
            }
            at += 4 + len;
        }
        let mut s = 99u64;
        for _ in 0..20_000 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (s >> 56) as usize;
            let b: Vec<u8> = (0..len).map(|i| (s >> (i % 48)) as u8 ^ i as u8).collect();
            let _ = decode_body(&b);
        }
        assert_eq!(decode_body(&[]), Err(DecodeError::Empty));
        assert!(matches!(decode_body(&[99, 1, 2]), Ok(Decoded::Unknown(99))));
        // Direction 5 is not a direction.
        let mut f = Vec::new();
        encode_frame(
            &mut f,
            0,
            &meta(&id, 1, Direction::FromGuest, Verdict::Allowed),
            b"x",
        )
        .unwrap();
        f[4 + 1 + 4 + 8] = 5;
        assert_eq!(decode_body(&f[4..]), Err(DecodeError::BadDirection));
    }

    #[test]
    fn oversize_frames_are_refused_at_encode() {
        let id = ident("vm");
        let m = meta(&id, 1, Direction::FromGuest, Verdict::Allowed);
        let mut out = Vec::new();
        assert!(encode_frame(&mut out, 0, &m, &vec![0u8; MAX_FRAME + 1]).is_none());
        assert!(out.is_empty());
        assert!(encode_frame(&mut out, 0, &m, &vec![0u8; MAX_FRAME]).is_some());
    }

    // ---- end to end ---------------------------------------------------------------------------------------

    fn start(dir: &tempfile::TempDir) -> (Arc<Observer>, BridgeRunning) {
        let obs = Arc::new(Observer::new(ObserverConfig::default()));
        let server = BridgeServer::bind(sock(dir)).unwrap();
        (obs.clone(), server.spawn(obs))
    }

    #[test]
    fn frames_flow_from_client_to_observer_with_identity_direction_and_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let client = BridgeClient::connect(sock(&dir), BridgeClientConfig::default());
        let a = ident("vm-a");
        let b = Identity::new("vm-b", "net-a", "private", [2, 0, 0, 0, 0, 9]);

        let hello = l7f::client_hello(
            0x0303,
            &[0x1301],
            &[l7f::ext_sni("bridge.example.org"), l7f::ext_alpn(&["h2"])],
        );
        assert!(wait_for(|| {
            client.tap_as(
                &a,
                Direction::FromGuest,
                Verdict::Allowed,
                &udp_v4((G, 4000), (S, 9), b"warmup"),
            );
            client.flush(Duration::from_millis(200));
            BridgeServerStats::get(&server.stats.frames) > 0
        }));
        client.tap_as(
            &a,
            Direction::FromGuest,
            Verdict::Allowed,
            &tcp_v4((G, 5000), (S, 443), 1, 0, TCP_SYN, 100, b""),
        );
        client.tap_as(
            &a,
            Direction::FromGuest,
            Verdict::Allowed,
            &tcp_v4((G, 5000), (S, 443), 2, 1, TCP_ACK | TCP_PSH, 100, &hello),
        );
        client.tap_as(
            &b,
            Direction::ToGuest,
            Verdict::Denied(DropReason::Policy),
            &udp_v4((S, 53), (G, 7000), b"x"),
        );
        assert!(client.flush(Duration::from_secs(5)));

        assert!(wait_for(|| obs.flows(&FlowQuery::default()).len() >= 3));
        let flows = obs.flows(&FlowQuery::default());
        let tls = flows.iter().find(|f| f.server.port == 443).unwrap();
        assert_eq!(tls.vm_id, "vm-a");
        assert_eq!(tls.sni.as_deref(), Some("bridge.example.org"));
        assert_eq!(tls.network, "net-a");
        let denied = flows.iter().find(|f| f.vm_id == "vm-b").unwrap();
        assert_eq!(
            (denied.verdict, denied.drop_reason),
            ("denied", Some("policy"))
        );
        assert_eq!(denied.mac, "02:00:00:00:00:09");
        server.shutdown().unwrap();
        assert!(
            !sock(&dir).exists(),
            "the socket file is removed on shutdown"
        );
    }

    #[test]
    fn stats_records_surface_client_side_drops_in_the_observer_summary() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let client = BridgeClient::connect(
            sock(&dir),
            BridgeClientConfig {
                snaplen: 64,
                ..BridgeClientConfig::default()
            },
        );
        let a = ident("vm-a");
        // One frame larger than the wire allows: dropped and counted client side.
        client.tap_as(&a, Direction::FromGuest, Verdict::Allowed, &[0u8; 100]);
        assert!(wait_for(|| {
            client.tap_as(
                &a,
                Direction::FromGuest,
                Verdict::Allowed,
                &udp_v4((G, 1), (S, 2), b"x"),
            );
            BridgeServerStats::get(&server.stats.frames) > 2
        }));
        assert!(BridgeClientStats::get(&client.stats().truncated_to_snaplen) > 0);
        // Provoke a client-side drop that the server can only learn from STATS.
        client
            .shared
            .stats
            .dropped_disconnected
            .fetch_add(41, Ordering::Relaxed);
        assert!(wait_for(|| {
            client.tap_as(
                &a,
                Direction::FromGuest,
                Verdict::Allowed,
                &udp_v4((G, 1), (S, 2), b"x"),
            );
            BridgeServerStats::get(&server.stats.client_reported_dropped) >= 41
        }));
        let s = obs.summary(now_us());
        assert!(s.counters.tap_dropped >= 41);
        assert!(s.counters.tap_enqueued > 0);
        server.shutdown().unwrap();
    }

    #[test]
    fn stats_ext_round_trips_and_old_stats_decode_without_it() {
        let ext = StatsExt {
            dropped_by_reason: [1, 2, 3, 4],
            truncated_to_snaplen: 5,
            switch_forward_dropped: 6,
        };
        let mut b = Vec::new();
        encode_stats_ext(&mut b, 100, 10, &ext);
        let len = u32::from_be_bytes(b[..4].try_into().unwrap()) as usize;
        assert_eq!(len, b.len() - 4);
        assert_eq!(
            decode_body(&b[4..]),
            Ok(Decoded::Stats {
                sent: 100,
                dropped: 10,
                ext: Some(ext)
            })
        );
        let mut old = Vec::new();
        encode_stats(&mut old, 7, 1);
        assert_eq!(
            decode_body(&old[4..]),
            Ok(Decoded::Stats {
                sent: 7,
                dropped: 1,
                ext: None
            })
        );
    }

    #[test]
    fn partial_writes_count_only_whole_records() {
        let mut data = Vec::new();
        let meta = FrameMeta {
            identity: ident("vm-a"),
            ts_us: 1,
            direction: Direction::FromGuest,
            verdict: Verdict::Allowed,
        };
        for _ in 0..3 {
            encode_frame(&mut data, 0, &meta, &[1u8; 40]).unwrap();
        }
        let rec = data.len() / 3;
        assert_eq!(complete_records(&data, data.len()), 3);
        assert_eq!(complete_records(&data, 2 * rec), 2);
        assert_eq!(complete_records(&data, 2 * rec + rec - 1), 2);
        assert_eq!(complete_records(&data, 0), 0);
    }

    /// Offers `n` frames through a deliberately small queue and checks that every one is either ingested or
    /// counted, that the summary says so by reason, and that nothing is left unconfirmed.
    #[test]
    fn offered_equals_ingested_plus_counted_drops() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        // The daemon registers its in-process tap first; the bridge must be accounted for regardless.
        let (_t, rx) = crate::source::NetTap::new(4, 2048);
        obs.set_tap_stats(rx.stats());
        let client = BridgeClient::connect(
            sock(&dir),
            BridgeClientConfig {
                queue_capacity: 16,
                ..BridgeClientConfig::default()
            },
        );
        let a = ident("vm-a");
        assert!(wait_for(|| {
            client.shared.core.connected.load(Ordering::Relaxed)
        }));
        let f = udp_v4((G, 1), (S, 2), &[7u8; 300]);
        let offered = 50_000u64;
        for _ in 0..offered {
            client.tap_as(&a, Direction::FromGuest, Verdict::Allowed, &f);
        }
        assert!(client.flush(Duration::from_secs(10)));
        let st = client.stats();
        assert!(
            BridgeClientStats::get(&st.dropped_queue_full) > 0,
            "a 16-frame queue must overflow under a burst"
        );
        let dropped = st.total_dropped();
        assert_eq!(BridgeClientStats::get(&st.sent) + dropped, offered);
        assert!(wait_for(|| {
            let s = obs.summary(now_us());
            s.counters.bridge_frames + s.counters.bridge_dropped == offered
                && s.counters.bridge_unconfirmed == 0
        }));
        let s = obs.summary(now_us());
        assert_eq!(s.counters.bridge_frames, BridgeClientStats::get(&st.sent));
        assert_eq!(obs.counters().frames, s.counters.bridge_frames);
        assert_eq!(s.counters.bridge_dropped, dropped);
        assert_eq!(s.counters.tap_dropped, dropped, "totals include bridges");
        let by: std::collections::HashMap<_, _> = s
            .counters
            .bridge_dropped_by_reason
            .iter()
            .cloned()
            .collect();
        assert_eq!(
            by["queue_full"],
            BridgeClientStats::get(&st.dropped_queue_full)
        );
        assert_eq!(by.len(), crate::source::BRIDGE_DROP_REASONS.len());
        server.shutdown().unwrap();
    }

    #[test]
    fn frames_for_unannounced_identities_are_counted_as_server_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = UnixStream::connect(sock(&dir)).unwrap();
        c.write_all(&encode_preamble(9)).unwrap();
        let meta = FrameMeta {
            identity: ident("vm-a"),
            ts_us: 1,
            direction: Direction::FromGuest,
            verdict: Verdict::Allowed,
        };
        let mut b = Vec::new();
        encode_frame(&mut b, 77, &meta, &udp_v4((G, 1), (S, 2), b"x")).unwrap();
        c.write_all(&b).unwrap();
        assert!(wait_for(|| {
            obs.summary(now_us()).counters.bridge_dropped == 1
        }));
        let s = obs.summary(now_us());
        let by: std::collections::HashMap<_, _> = s
            .counters
            .bridge_dropped_by_reason
            .iter()
            .cloned()
            .collect();
        assert_eq!(by["server_rejected"], 1);
        server.shutdown().unwrap();
    }

    #[test]
    fn switch_side_forward_drops_and_snaplen_cuts_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let client = BridgeClient::connect(
            sock(&dir),
            BridgeClientConfig {
                snaplen: 64,
                stats_interval: Duration::from_millis(20),
                ..BridgeClientConfig::default()
            },
        );
        assert!(wait_for(|| {
            client.shared.core.connected.load(Ordering::Relaxed)
        }));
        let a = ident("vm-a");
        client.note_forward_drop();
        client.note_forward_drop();
        client.tap_as(
            &a,
            Direction::FromGuest,
            Verdict::Allowed,
            &udp_v4((G, 1), (S, 2), &[1u8; 400]),
        );
        assert!(wait_for(|| {
            let c = obs.summary(now_us()).counters;
            c.switch_forward_dropped == 2 && c.bridge_truncated_to_snaplen == 1
        }));
        server.shutdown().unwrap();
    }

    #[test]
    fn client_never_blocks_and_drops_when_nobody_listens() {
        let dir = tempfile::tempdir().unwrap();
        let client = BridgeClient::connect(
            sock(&dir),
            BridgeClientConfig {
                queue_capacity: 8,
                ..BridgeClientConfig::default()
            },
        );
        let a = ident("vm-a");
        let f = udp_v4((G, 1), (S, 2), &[7u8; 500]);
        let t = Instant::now();
        for _ in 0..20_000 {
            client.tap_as(&a, Direction::FromGuest, Verdict::Allowed, &f);
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "tap_as must never block"
        );
        let st = client.stats();
        // With nobody listening every frame is refused up front as `disconnected`, and every one is counted.
        assert_eq!(BridgeClientStats::get(&st.dropped_disconnected), 20_000);
        assert_eq!(st.total_dropped(), 20_000);
        assert!(wait_for(|| BridgeClientStats::get(&st.connect_failures) > 0));
        assert_eq!(BridgeClientStats::get(&st.sent), 0);
    }

    #[test]
    fn stuck_server_does_not_stall_the_client() {
        // A listener that accepts and never reads: the socket buffer fills, the write times out, and the client
        // drops the connection and keeps draining its queue.
        let dir = tempfile::tempdir().unwrap();
        let l = UnixListener::bind(sock(&dir)).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let hold = std::thread::spawn(move || {
            let mut keep = Vec::new();
            l.set_nonblocking(true).unwrap();
            while !d2.load(Ordering::Relaxed) {
                if let Ok((c, _)) = l.accept() {
                    keep.push(c);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let client = BridgeClient::connect(
            sock(&dir),
            BridgeClientConfig {
                queue_capacity: 64,
                write_timeout: Duration::from_millis(50),
                snaplen: MAX_FRAME,
                ..BridgeClientConfig::default()
            },
        );
        let a = ident("vm-a");
        let big = vec![0xabu8; 60_000];
        let t = Instant::now();
        let mut errors = 0;
        while t.elapsed() < Duration::from_secs(6) && errors == 0 {
            client.tap_as(&a, Direction::FromGuest, Verdict::Allowed, &big);
            std::thread::sleep(Duration::from_micros(200));
            errors = BridgeClientStats::get(&client.stats().dropped_write_error);
        }
        assert!(
            errors > 0,
            "a write to a peer that never reads must time out and be counted"
        );
        assert!(BridgeClientStats::get(&client.stats().dropped_queue_full) + errors > 0);
        drop(client);
        done.store(true, Ordering::Relaxed);
        hold.join().unwrap();
    }

    fn raw_connect(dir: &tempfile::TempDir) -> UnixStream {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(sock(dir)) {
                Ok(s) => return s,
                Err(e) if Instant::now() > end => panic!("connect: {e}"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn frame_record(id: u32, f: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        encode_frame(
            &mut v,
            id,
            &meta(&ident("x"), 1_000, Direction::FromGuest, Verdict::Allowed),
            f,
        )
        .unwrap();
        v
    }

    #[test]
    fn oversize_record_is_skipped_and_the_stream_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = raw_connect(&dir);
        c.write_all(&encode_preamble(1)).unwrap();
        let mut w = Vec::new();
        encode_ident(&mut w, 0, &ident("vm-raw")).unwrap();
        // A record claiming MAX_RECORD + 1000 bytes, with that much (junk) body.
        let big = MAX_RECORD + 1000;
        w.extend_from_slice(&(big as u32).to_be_bytes());
        w.extend_from_slice(&vec![0xeeu8; big]);
        // Followed by a valid frame.
        w.extend(frame_record(0, &udp_v4((G, 1), (S, 2), b"after")));
        c.write_all(&w).unwrap();
        assert!(wait_for(|| obs.counters().frames == 1));
        assert_eq!(BridgeServerStats::get(&server.stats.oversize_records), 1);
        assert_eq!(BridgeServerStats::get(&server.stats.frames), 1);
        drop(c);
        server.shutdown().unwrap();
    }

    #[test]
    fn absurd_length_closes_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = raw_connect(&dir);
        c.write_all(&encode_preamble(1)).unwrap();
        let mut w = Vec::new();
        encode_ident(&mut w, 0, &ident("vm-raw")).unwrap();
        w.extend_from_slice(&u32::MAX.to_be_bytes());
        c.write_all(&w).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert!(wait_for(|| BridgeServerStats::get(
            &server.stats.oversize_records
        ) == 1));
        // The server hung up: a read sees EOF.
        let mut b = [0u8; 1];
        match c.read(&mut b) {
            Ok(0) => {}
            // Closing a socket that still has unread data may surface as a reset instead of a clean EOF.
            Err(e)
                if !matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            other => panic!("expected the server to hang up, got {other:?}"),
        }
        assert_eq!(obs.counters().frames, 0);
        server.shutdown().unwrap();
    }

    #[test]
    fn truncated_record_is_counted_and_never_ingested() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = raw_connect(&dir);
        c.write_all(&encode_preamble(1)).unwrap();
        let mut w = Vec::new();
        encode_ident(&mut w, 0, &ident("vm-raw")).unwrap();
        w.extend(frame_record(0, &udp_v4((G, 1), (S, 2), b"whole")));
        let partial = frame_record(0, &udp_v4((G, 1), (S, 2), b"cut-off-frame"));
        w.extend_from_slice(&partial[..partial.len() - 5]);
        c.write_all(&w).unwrap();
        drop(c); // EOF in the middle of a record
        assert!(wait_for(
            || BridgeServerStats::get(&server.stats.truncated) == 1
        ));
        assert_eq!(obs.counters().frames, 1, "only the complete frame arrives");
        assert_eq!(BridgeServerStats::get(&server.stats.frames), 1);
        server.shutdown().unwrap();
    }

    #[test]
    fn truncated_preamble_and_bad_version_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = raw_connect(&dir);
        c.write_all(&encode_preamble(1)[..7]).unwrap();
        drop(c);
        assert!(wait_for(
            || BridgeServerStats::get(&server.stats.truncated) == 1
        ));

        let mut c = raw_connect(&dir);
        let mut p = encode_preamble(1);
        p[4] = 99;
        c.write_all(&p).unwrap();
        assert!(wait_for(|| BridgeServerStats::get(
            &server.stats.bad_version
        ) == 1));
        let mut c2 = raw_connect(&dir);
        c2.write_all(b"GET / HTTP/1.1\r\n\r\n....").unwrap();
        assert!(wait_for(|| BridgeServerStats::get(
            &server.stats.bad_version
        ) == 2));
        assert_eq!(obs.counters().frames, 0);
        server.shutdown().unwrap();
    }

    #[test]
    fn unknown_ident_unknown_type_and_garbage_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let mut c = raw_connect(&dir);
        c.write_all(&encode_preamble(1)).unwrap();
        let mut w = Vec::new();
        w.extend(frame_record(42, &udp_v4((G, 1), (S, 2), b"orphan"))); // no IDENT
        w.extend_from_slice(&5u32.to_be_bytes()); // unknown type 200
        w.extend_from_slice(&[200, 1, 2, 3, 4]);
        w.extend_from_slice(&3u32.to_be_bytes()); // IDENT too short
        w.extend_from_slice(&[T_IDENT, 0, 0]);
        w.extend_from_slice(&0u32.to_be_bytes()); // empty record
        encode_ident(&mut w, 0, &ident("vm-raw")).unwrap();
        w.extend(frame_record(0, &udp_v4((G, 1), (S, 2), b"ok")));
        c.write_all(&w).unwrap();
        assert!(wait_for(|| obs.counters().frames == 1));
        let st = &server.stats;
        assert_eq!(BridgeServerStats::get(&st.unknown_ident), 1);
        assert_eq!(BridgeServerStats::get(&st.unknown_records), 1);
        assert_eq!(BridgeServerStats::get(&st.bad_records), 2);
        drop(c);
        server.shutdown().unwrap();
    }

    #[test]
    fn client_reconnects_after_the_server_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let obs = Arc::new(Observer::new(ObserverConfig::default()));
        let client = BridgeClient::connect(sock(&dir), BridgeClientConfig::default());
        let a = ident("vm-a");
        let send = |port: u16| {
            client.tap_as(
                &a,
                Direction::FromGuest,
                Verdict::Allowed,
                &udp_v4((G, port), (S, 9000), b"ping"),
            );
        };
        // No server yet: nothing is delivered, the client does not hang or panic.
        for p in 0..10 {
            send(2000 + p);
        }
        assert!(wait_for(|| BridgeClientStats::get(
            &client.stats().connect_failures
        ) > 0));

        let server = BridgeServer::bind(sock(&dir)).unwrap().spawn(obs.clone());
        assert!(wait_for(|| {
            send(3000);
            BridgeServerStats::get(&server.stats.frames) > 0
        }));
        let first_conn = BridgeClientStats::get(&client.stats().connects);
        server.shutdown().unwrap();
        assert!(!sock(&dir).exists());

        // A new server at the same path: the client finds it again and re-sends the identity.
        let obs2 = Arc::new(Observer::new(ObserverConfig::default()));
        let server2 = BridgeServer::bind(sock(&dir)).unwrap().spawn(obs2.clone());
        assert!(wait_for(|| {
            send(4000);
            obs2.counters().frames > 0
        }));
        assert!(BridgeClientStats::get(&client.stats().connects) > first_conn);
        assert!(BridgeClientStats::get(&client.stats().reconnects) >= 1);
        let flows = obs2.flows(&FlowQuery::default());
        assert!(
            flows
                .iter()
                .all(|f| f.vm_id == "vm-a" && f.network == "net-a"),
            "identity survived the reconnect"
        );
        server2.shutdown().unwrap();
    }

    #[test]
    fn bind_replaces_a_stale_socket_but_refuses_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = sock(&dir);
        // A stale file: a socket whose listener is gone.
        drop(UnixListener::bind(&p).unwrap());
        assert!(p.exists());
        let s1 = BridgeServer::bind(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let err = BridgeServer::bind(&p).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(s1);
    }

    #[test]
    fn many_connections_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let obs = Arc::new(Observer::new(ObserverConfig::default()));
        let server = BridgeServer::bind_with(
            sock(&dir),
            BridgeServerConfig {
                max_connections: 2,
                ..BridgeServerConfig::default()
            },
        )
        .unwrap()
        .spawn(obs);
        let mut keep = Vec::new();
        for i in 0..6 {
            let mut c = raw_connect(&dir);
            c.write_all(&encode_preamble(i)).unwrap();
            keep.push(c);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(wait_for(|| BridgeServerStats::get(
            &server.stats.connections_rejected
        ) >= 1));
        assert!(BridgeServerStats::get(&server.stats.active) <= 2);
        drop(keep);
        server.shutdown().unwrap();
    }

    #[test]
    fn concurrent_clients_all_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let (obs, server) = start(&dir);
        let per = 500u64;
        let clients: Vec<BridgeClient> = (0..4)
            .map(|_| BridgeClient::connect(sock(&dir), BridgeClientConfig::default()))
            .collect();
        // Wait until every client is connected.
        assert!(wait_for(|| clients
            .iter()
            .all(|c| BridgeClientStats::get(&c.stats().connects) >= 1)));
        std::thread::scope(|sc| {
            for (n, c) in clients.iter().enumerate() {
                sc.spawn(move || {
                    let id =
                        Identity::new(&format!("vm-{n}"), "net-a", "private", build::GUEST_MAC);
                    for i in 0..per {
                        let f = udp_v4((G, 10_000 + (i % 50) as u16), (S, 9000), b"concurrent");
                        // Retry on a full queue so the test counts deliveries, not tap drops.
                        loop {
                            let before = BridgeClientStats::get(&c.stats().dropped_queue_full);
                            c.tap_as(&id, Direction::FromGuest, Verdict::Allowed, &f);
                            if BridgeClientStats::get(&c.stats().dropped_queue_full) == before {
                                break;
                            }
                            std::thread::sleep(Duration::from_micros(100));
                        }
                    }
                });
            }
        });
        assert!(wait_for(|| obs.counters().frames == 4 * per));
        assert_eq!(obs.summary(now_us()).per_vm.len(), 4);
        server.shutdown().unwrap();
    }
}
