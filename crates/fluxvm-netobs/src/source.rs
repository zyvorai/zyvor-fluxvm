// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! How frames reach an [`Observer`].
//!
//! * [`FrameSource`]: a pull-style source (a pcap replay, a userspace NAT's frame loop, a test). Drive it with
//!   [`drain`] or call [`Observer::ingest`] directly: `ingest` takes `&self`, copies nothing it does not keep and
//!   is the single entry point everything funnels through.
//! * [`NetTap`]: a cheap, cloneable, non-blocking handle for a datapath hot loop. `tap()` copies the (snap-length
//!   bounded) frame into a bounded channel with `try_send`. When the queue is full the frame is **dropped and
//!   counted**, never waited for, so observation can never stall or back-pressure the switch. [`spawn_pump`] runs
//!   the consuming side on its own thread.

use crate::flow::{Direction, FrameMeta, Identity, Verdict};
use crate::observer::Observer;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unix time in microseconds.
pub fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// A pull-style frame source.
pub trait FrameSource {
    /// Reads the next frame into `buf` (cleared first). `Ok(None)` is end of stream.
    fn next_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<FrameMeta>>;
}

/// Feeds every frame of `src` to `obs` until the source ends. Returns the number of frames ingested.
pub fn drain<S: FrameSource + ?Sized>(src: &mut S, obs: &Observer) -> io::Result<u64> {
    let mut buf = Vec::with_capacity(2048);
    let mut n = 0;
    while let Some(meta) = src.next_frame(&mut buf)? {
        obs.ingest(&buf, &meta);
        n += 1;
    }
    Ok(n)
}

/// The push side a datapath calls. Implementations must not block.
pub trait Tap: Send + Sync {
    fn tap(&self, direction: Direction, verdict: Verdict, frame: &[u8]);
}

/// Counters shared by every clone of a [`NetTap`].
#[derive(Debug, Default)]
pub struct TapStats {
    pub enqueued: AtomicU64,
    pub dropped: AtomicU64,
}

impl TapStats {
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Relaxed)
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct TapFrame {
    pub identity: Arc<Identity>,
    pub ts_us: u64,
    pub direction: Direction,
    pub verdict: Verdict,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct NetTap {
    tx: SyncSender<TapFrame>,
    stats: Arc<TapStats>,
    identity: Option<Arc<Identity>>,
    snaplen: usize,
}

/// The consuming end of a [`NetTap`].
pub struct TapReceiver {
    rx: Receiver<TapFrame>,
    stats: Arc<TapStats>,
}

impl TapReceiver {
    pub fn stats(&self) -> Arc<TapStats> {
        self.stats.clone()
    }

    pub fn recv_timeout(&self, d: Duration) -> Result<TapFrame, RecvTimeoutError> {
        self.rx.recv_timeout(d)
    }

    pub fn try_recv(&self) -> Option<TapFrame> {
        self.rx.try_recv().ok()
    }
}

impl NetTap {
    /// A tap with room for `capacity` queued frames, each kept up to `snaplen` bytes.
    pub fn new(capacity: usize, snaplen: usize) -> (NetTap, TapReceiver) {
        let (tx, rx) = sync_channel(capacity.max(1));
        let stats = Arc::new(TapStats::default());
        (
            NetTap { tx, stats: stats.clone(), identity: None, snaplen: snaplen.clamp(64, 65_535) },
            TapReceiver { rx, stats },
        )
    }

    /// A clone of this tap that attributes its frames to `identity` (one per guest port).
    pub fn for_identity(&self, identity: Arc<Identity>) -> NetTap {
        NetTap { identity: Some(identity), ..self.clone() }
    }

    pub fn stats(&self) -> &Arc<TapStats> {
        &self.stats
    }

    /// Queues a frame for `identity`. Never blocks; a full queue (or a gone consumer) drops and counts.
    pub fn tap_as(&self, identity: &Arc<Identity>, direction: Direction, verdict: Verdict, frame: &[u8]) {
        let keep = frame.len().min(self.snaplen);
        let item = TapFrame {
            identity: identity.clone(),
            ts_us: now_us(),
            direction,
            verdict,
            data: frame[..keep].to_vec(),
        };
        match self.tx.try_send(item) {
            Ok(()) => {
                self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Tap for NetTap {
    fn tap(&self, direction: Direction, verdict: Verdict, frame: &[u8]) {
        match &self.identity {
            Some(id) => self.tap_as(id, direction, verdict, frame),
            // A tap with no identity cannot attribute the frame; count it as dropped.
            None => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// A running consumer thread; dropping or [`PumpHandle::stop`] ends it.
pub struct PumpHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PumpHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for PumpHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

static GLOBAL_TAP: std::sync::OnceLock<NetTap> = std::sync::OnceLock::new();

/// Publishes the process-wide tap that in-process datapaths feed (the daemon's pump drains it). False if already set.
pub fn install_global_tap(tap: NetTap) -> bool {
    GLOBAL_TAP.set(tap).is_ok()
}

/// The process-wide tap, if the daemon installed one.
pub fn global_tap() -> Option<NetTap> {
    GLOBAL_TAP.get().cloned()
}

/// Moves frames from `rx` into `obs` on a dedicated thread, and ages flows while the tap is idle.
pub fn spawn_pump(obs: Arc<Observer>, rx: TapReceiver) -> PumpHandle {
    obs.set_tap_stats(rx.stats());
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("fluxvm-netobs".into())
        .spawn(move || {
            let feed = |f: TapFrame| {
                let meta = FrameMeta {
                    identity: f.identity,
                    ts_us: f.ts_us,
                    direction: f.direction,
                    verdict: f.verdict,
                };
                obs.ingest(&f.data, &meta);
            };
            while !flag.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(f) => {
                        feed(f);
                        for _ in 0..255 {
                            match rx.try_recv() {
                                Some(f) => feed(f),
                                None => break,
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => obs.tick(now_us()),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .ok();
    PumpHandle { stop, join }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::ObserverConfig;
    use crate::packet::build;
    use std::net::Ipv4Addr;

    fn id() -> Arc<Identity> {
        Identity::new("vm-1", "net", "eth0", build::GUEST_MAC)
    }

    #[test]
    fn full_queue_drops_and_counts_without_blocking() {
        let (tap, rx) = NetTap::new(4, 2048);
        let tap = tap.for_identity(id());
        let f = build::udp_v4((Ipv4Addr::new(10, 0, 0, 1), 1), (Ipv4Addr::new(10, 0, 0, 2), 2), b"x");
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            tap.tap(Direction::FromGuest, Verdict::Allowed, &f);
        }
        assert!(start.elapsed() < Duration::from_secs(2), "tap must never block");
        let s = rx.stats();
        assert_eq!(s.enqueued(), 4);
        assert_eq!(s.dropped(), 996);
        // The queue still delivers what it held, oldest first.
        let mut got = 0;
        while rx.try_recv().is_some() {
            got += 1;
        }
        assert_eq!(got, 4);
        // After draining there is room again.
        tap.tap(Direction::FromGuest, Verdict::Allowed, &f);
        assert_eq!(rx.stats().enqueued(), 5);
    }

    #[test]
    fn gone_consumer_is_counted_not_fatal() {
        let (tap, rx) = NetTap::new(2, 2048);
        let stats = rx.stats();
        drop(rx);
        let tap = tap.for_identity(id());
        tap.tap(Direction::FromGuest, Verdict::Allowed, &[0u8; 20]);
        assert_eq!(stats.dropped(), 1);
    }

    #[test]
    fn frames_are_cut_to_snaplen() {
        let (tap, rx) = NetTap::new(2, 100);
        tap.tap_as(&id(), Direction::FromGuest, Verdict::Allowed, &[7u8; 500]);
        let f = rx.try_recv().unwrap();
        assert_eq!(f.data.len(), 100);
    }

    #[test]
    fn untagged_tap_cannot_attribute() {
        let (tap, rx) = NetTap::new(2, 2048);
        tap.tap(Direction::FromGuest, Verdict::Allowed, &[0u8; 20]);
        assert_eq!(rx.stats().dropped(), 1);
        assert!(rx.try_recv().is_none());
    }

    #[test]
    fn pump_feeds_the_observer() {
        let obs = Arc::new(Observer::new(ObserverConfig::default()));
        let (tap, rx) = NetTap::new(64, 2048);
        let pump = spawn_pump(obs.clone(), rx);
        let tap = tap.for_identity(id());
        let f = build::udp_v4((Ipv4Addr::new(10, 0, 0, 1), 5000), (Ipv4Addr::new(10, 0, 0, 2), 6000), b"hi");
        for _ in 0..5 {
            tap.tap(Direction::FromGuest, Verdict::Allowed, &f);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while obs.counters().frames < 5 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        pump.stop();
        assert_eq!(obs.counters().frames, 5);
        assert_eq!(obs.summary(now_us()).counters.tap_enqueued, 5);
    }

    struct Replay(Vec<(Vec<u8>, FrameMeta)>);
    impl FrameSource for Replay {
        fn next_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<FrameMeta>> {
            Ok(self.0.pop().map(|(f, m)| {
                buf.clear();
                buf.extend_from_slice(&f);
                m
            }))
        }
    }

    #[test]
    fn frame_source_drains_into_observer() {
        let obs = Observer::new(ObserverConfig::default());
        let f = build::udp_v4((Ipv4Addr::new(10, 0, 0, 1), 5000), (Ipv4Addr::new(10, 0, 0, 2), 6000), b"hi");
        let meta = FrameMeta { identity: id(), ts_us: 1, direction: Direction::FromGuest, verdict: Verdict::Allowed };
        let mut src = Replay(vec![(f.clone(), meta.clone()), (f, meta)]);
        assert_eq!(drain(&mut src, &obs).unwrap(), 2);
        assert_eq!(obs.counters().frames, 2);
    }
}
