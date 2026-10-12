// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! One private network across several Macs: the switches of the same network on different hosts form a full mesh and
//! carry each other's Ethernet frames over UDP, sealed with AES-256-GCM under the network's key.
//!
//! - **Wire format.** `"FXO2"` ‖ sender id (u32, random per switch start) ‖ counter (u64, from a random start below
//!   2^62) ‖ key epoch (u32) ‖ send time (u32, Unix seconds) ‖ AES-256-GCM ciphertext and tag of the Ethernet frame.
//!   The 12-byte nonce is sender id ‖ counter, so a switch never reuses one, and two switch starts under the same key
//!   would need both a 32-bit sender id and a 62-bit counter range to collide before they could share a nonce; the
//!   associated data is the 24-byte header plus the network name, so a datagram cannot be replayed into another
//!   network that happens to share a key, nor have its epoch or time altered.
//! - **Keys and rotation.** A switch seals with its current key and opens with the current or the previous one (by
//!   epoch). After a rotation it keeps sealing with the old key for [`SEAL_DELAY`] while already opening both, so
//!   every peer has the new key before it is used and the rotation rolls across Macs without dropping traffic. Keys
//!   reload from the key file while running.
//! - **Replay.** Each (epoch, sender) has a 1024-packet sliding window, and a datagram whose send time is more than
//!   [`MAX_SKEW_SECS`] away from the receiver's clock is dropped, so a stream captured earlier cannot be replayed to a
//!   switch that restarted (its windows are empty). Macs need roughly synchronised clocks (NTP).
//! - **Who may send.** Only configured peer IP addresses are accepted, and only datagrams that authenticate.
//! - **Forwarding.** A switch learns which peer each remote MAC sits behind from authenticated frames. Unicast to a
//!   learned MAC goes to that peer; unknown unicast, broadcast and multicast go to every peer. Frames from a peer are
//!   delivered to local guests only, never to other peers (split horizon: full mesh, no loops).
//! - **Anti-spoof.** A peer can never claim a MAC that belongs to a guest on this switch, nor a multicast source.
//!
//! The underlay must carry the overhead: 68 bytes over IPv4 (IP 20 + UDP 8 + header 24 + tag 16), so use an `mtu` of
//! at most 1432 on a 1500-byte underlay (1400 leaves room for IPv6 and tunnels), or a jumbo-frame underlay.

use crate::{MAX_FRAME, Mac};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const MAGIC: &[u8; 4] = b"FXO2";
pub const HEADER: usize = 24;
/// After a rotation, how long a switch keeps sealing with the old key: longer than a node heartbeat (10 s) plus the
/// switch's key reload (10 s), so every peer can open the new key before it is used.
pub const SEAL_DELAY: Duration = Duration::from_secs(60);
/// Largest accepted difference between a datagram's send time and the receiver's clock.
pub const MAX_SKEW_SECS: i64 = 120;
pub const TAG: usize = 16;
/// Largest datagram a peer may send.
pub const MAX_DATAGRAM: usize = HEADER + MAX_FRAME + TAG;
const WINDOW: u64 = 1024;
/// A learned remote MAC is forgotten after this long without a frame from it (it moved, or left).
const LEARN_TTL: Duration = Duration::from_secs(300);

/// Seals and opens frames. [`AesGcm`] is the real one; tests may use another.
pub trait FrameCipher: Send + Sync {
    fn seal(&self, nonce: &[u8; 12], aad: &[u8], frame: &[u8]) -> Option<Vec<u8>>;
    fn open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>>;
}

/// AES-256-GCM under the network's 32-byte key.
pub struct AesGcm(aes_gcm::Aes256Gcm);

impl AesGcm {
    pub fn new(key: &[u8; 32]) -> Self {
        use aes_gcm::aead::KeyInit;
        Self(aes_gcm::Aes256Gcm::new(
            aes_gcm::Key::<aes_gcm::Aes256Gcm>::from_slice(key),
        ))
    }
}

impl FrameCipher for AesGcm {
    fn seal(&self, nonce: &[u8; 12], aad: &[u8], frame: &[u8]) -> Option<Vec<u8>> {
        use aes_gcm::aead::{Aead, Payload};
        self.0
            .encrypt(
                aes_gcm::Nonce::from_slice(nonce),
                Payload { msg: frame, aad },
            )
            .ok()
    }

    fn open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        use aes_gcm::aead::{Aead, Payload};
        self.0
            .decrypt(
                aes_gcm::Nonce::from_slice(nonce),
                Payload { msg: sealed, aad },
            )
            .ok()
    }
}

/// One key as 64 hex characters.
pub fn parse_key(text: &str) -> Option<[u8; 32]> {
    let t = text.trim();
    if t.len() != 64 {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, k) in key.iter_mut().enumerate() {
        *k = u8::from_str_radix(t.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(key)
}

/// Reads a key file: either a single key (epoch 0), or lines of `<epoch> <64 hex>`, the current key first and
/// optionally the previous one second.
pub fn parse_keys(text: &str) -> Option<Vec<(u32, [u8; 32])>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() == 1 && !lines[0].contains(char::is_whitespace) {
        return Some(vec![(0, parse_key(lines[0])?)]);
    }
    if lines.is_empty() || lines.len() > 2 {
        return None;
    }
    let mut out = Vec::new();
    for l in lines {
        let (epoch, key) = l.split_once(char::is_whitespace)?;
        out.push((epoch.parse().ok()?, parse_key(key)?));
    }
    (out.len() < 2 || out[0].0 != out[1].0).then_some(out)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A random u64 from the OS (sender id and counter start).
fn random_u64() -> u64 {
    use std::io::Read;
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // Never expected; fall back to time and pid, still distinct across restarts in practice.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        return t ^ (u64::from(std::process::id()) << 32) ^ u64::from(std::process::id());
    }
    u64::from_be_bytes(b)
}

/// Sliding anti-replay window (RFC 4303 style) for one sender.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReplayWindow {
    top: u64,
    seen: [u64; (WINDOW / 64) as usize],
    started: bool,
}

impl ReplayWindow {
    fn bit(&mut self, n: u64) -> (&mut u64, u64) {
        let i = ((n % WINDOW) / 64) as usize;
        (&mut self.seen[i], 1u64 << (n % 64))
    }

    /// Whether `n` is new; records it if so.
    pub fn accept(&mut self, n: u64) -> bool {
        if !self.started {
            self.started = true;
            self.top = n;
            let (w, m) = self.bit(n);
            *w |= m;
            return true;
        }
        if n > self.top {
            // Clear the slots between the old top and n.
            let gap = (n - self.top).min(WINDOW);
            for k in 1..=gap {
                let (w, m) = self.bit(self.top + k);
                *w &= !m;
            }
            self.top = n;
            let (w, m) = self.bit(n);
            *w |= m;
            return true;
        }
        if self.top - n >= WINDOW {
            return false;
        }
        let (w, m) = self.bit(n);
        if *w & m != 0 {
            return false;
        }
        *w |= m;
        true
    }
}

#[derive(Debug, Clone)]
pub struct OverlayConfig {
    pub network: String,
    pub listen: SocketAddr,
    pub peers: Vec<SocketAddr>,
}

/// What [`Overlay::open`] returns for an accepted datagram.
#[derive(Debug, PartialEq, Eq)]
pub struct Inbound {
    pub frame: Vec<u8>,
    pub from: SocketAddr,
}

/// The overlay half of one switch.
pub struct Overlay {
    cfg: OverlayConfig,
    /// Current peers; starts as `cfg.peers` and can change while the switch runs ([`Overlay::set_peers`]).
    peers: RwLock<Vec<SocketAddr>>,
    /// Keys by epoch, the current (sealing) one first.
    keys: RwLock<Vec<(u32, Arc<dyn FrameCipher>)>>,
    /// After a rotation, keep sealing with this (the old) epoch until the instant, so peers that have not loaded the
    /// new key yet can still open what this switch sends.
    seal_hold: Mutex<Option<(u32, Instant)>>,
    sender: u32,
    counter: AtomicU64,
    windows: Mutex<HashMap<(u32, u32), ReplayWindow>>,
    learned: Mutex<HashMap<Mac, (SocketAddr, Instant)>>,
    socket: Option<UdpSocket>,
    pub dropped: AtomicU64,
}

impl Overlay {
    /// An overlay that only seals and opens (no socket), with one key at epoch 0: for tests and tools.
    pub fn detached(cfg: OverlayConfig, cipher: Box<dyn FrameCipher>) -> Self {
        Self::detached_keyed(cfg, vec![(0, cipher)])
    }

    /// [`Overlay::detached`] with keys by epoch, the current one first.
    pub fn detached_keyed(cfg: OverlayConfig, keys: Vec<(u32, Box<dyn FrameCipher>)>) -> Self {
        Self {
            peers: RwLock::new(cfg.peers.clone()),
            keys: RwLock::new(keys.into_iter().map(|(e, k)| (e, Arc::from(k))).collect()),
            seal_hold: Mutex::new(None),
            cfg,
            sender: random_u64() as u32,
            // A random start (below 2^62, so it never wraps) keeps nonces apart even if two starts draw one sender id.
            counter: AtomicU64::new(random_u64() >> 2),
            windows: Mutex::new(HashMap::new()),
            learned: Mutex::new(HashMap::new()),
            socket: None,
            dropped: AtomicU64::new(0),
        }
    }

    /// Binds the UDP socket (one key at epoch 0).
    pub fn bind(cfg: OverlayConfig, cipher: Box<dyn FrameCipher>) -> std::io::Result<Self> {
        Self::bind_keyed(cfg, vec![(0, cipher)])
    }

    /// Binds the UDP socket, with keys by epoch, the current one first.
    pub fn bind_keyed(
        cfg: OverlayConfig,
        keys: Vec<(u32, Box<dyn FrameCipher>)>,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(cfg.listen)?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut o = Self::detached_keyed(cfg, keys);
        o.socket = Some(socket);
        Ok(o)
    }

    /// Replaces the keys (a rotation): any of them opens at once; the first seals after [`SEAL_DELAY`] if the
    /// previous sealing key is still among them (peers pick the new key up within a heartbeat and a reload), else at
    /// once.
    pub fn set_keys(&self, keys: Vec<(u32, Box<dyn FrameCipher>)>) {
        self.set_keys_with_delay(keys, SEAL_DELAY);
    }

    /// [`Overlay::set_keys`] with a given delay before the new key seals.
    pub fn set_keys_with_delay(&self, keys: Vec<(u32, Box<dyn FrameCipher>)>, delay: Duration) {
        let keys: Vec<(u32, Arc<dyn FrameCipher>)> =
            keys.into_iter().map(|(e, k)| (e, Arc::from(k))).collect();
        let live: Vec<u32> = keys.iter().map(|(e, _)| *e).collect();
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(epoch, _), _| live.contains(epoch));
        let hold = match self.key(None) {
            Some((old, _))
                if live.first() != Some(&old) && live.contains(&old) && !delay.is_zero() =>
            {
                Some((old, Instant::now() + delay))
            }
            _ => None,
        };
        *self.seal_hold.lock().unwrap_or_else(|e| e.into_inner()) = hold;
        *self.keys.write().unwrap_or_else(|e| e.into_inner()) = keys;
    }

    /// Epochs of the keys in use, the current first.
    pub fn epochs(&self) -> Vec<u32> {
        self.keys
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(e, _)| *e)
            .collect()
    }

    /// The key for `epoch`, or with `None` the one to seal with (the held old one right after a rotation).
    fn key(&self, epoch: Option<u32>) -> Option<(u32, Arc<dyn FrameCipher>)> {
        // Lock order: `seal_hold` is never held while taking `keys`.
        let want =
            epoch.or_else(
                || match *self.seal_hold.lock().unwrap_or_else(|e| e.into_inner()) {
                    Some((e, until)) if Instant::now() < until => Some(e),
                    _ => None,
                },
            );
        let keys = self.keys.read().unwrap_or_else(|e| e.into_inner());
        let found = want.and_then(|w| keys.iter().find(|(e, _)| *e == w));
        match epoch {
            Some(_) => found.cloned(),
            None => found.or(keys.first()).cloned(),
        }
    }

    pub fn peer_count(&self) -> usize {
        self.peers().len()
    }

    pub fn peers(&self) -> Vec<SocketAddr> {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Replaces the peers (a Mac joined or left the network). MACs learned behind a removed peer are forgotten.
    pub fn set_peers(&self, peers: Vec<SocketAddr>) {
        self.learned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (p, _)| peers.contains(p));
        *self.peers.write().unwrap_or_else(|e| e.into_inner()) = peers;
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.as_ref().and_then(|s| s.local_addr().ok())
    }

    fn aad(&self, header: &[u8]) -> Vec<u8> {
        let mut aad = header.to_vec();
        aad.extend_from_slice(self.cfg.network.as_bytes());
        aad
    }

    /// Seals one frame into a datagram under the current key.
    pub fn seal(&self, frame: &[u8]) -> Option<Vec<u8>> {
        self.seal_at(frame, now_secs())
    }

    fn seal_at(&self, frame: &[u8], time: i64) -> Option<Vec<u8>> {
        let (epoch, cipher) = self.key(None)?;
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut header = [0u8; HEADER];
        header[..4].copy_from_slice(MAGIC);
        header[4..8].copy_from_slice(&self.sender.to_be_bytes());
        header[8..16].copy_from_slice(&n.to_be_bytes());
        header[16..20].copy_from_slice(&epoch.to_be_bytes());
        header[20..24].copy_from_slice(&(time as u32).to_be_bytes());
        let nonce: [u8; 12] = header[4..16].try_into().ok()?;
        let sealed = cipher.seal(&nonce, &self.aad(&header), frame)?;
        let mut out = header.to_vec();
        out.extend_from_slice(&sealed);
        Some(out)
    }

    /// The configured peer at `ip`, if any. Learning records this address, never the datagram's source port, so a
    /// spoofed or replayed datagram cannot make the switch send to an arbitrary port.
    fn peer_at(&self, ip: IpAddr) -> Option<SocketAddr> {
        self.peers().into_iter().find(|p| p.ip() == ip)
    }

    /// Checks and opens one datagram from `from`. `local` are the MACs of this switch's own guests, which no peer may
    /// claim. On success the source MAC is learned as sitting behind `from`.
    pub fn open(&self, datagram: &[u8], from: SocketAddr, local: &[Mac]) -> Option<Inbound> {
        let reject = || {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            None
        };
        let Some(peer) = self.peer_at(from.ip()) else {
            return reject();
        };
        if datagram.len() < HEADER + TAG + 14
            || datagram.len() > MAX_DATAGRAM
            || &datagram[..4] != MAGIC
        {
            return reject();
        }
        let header = &datagram[..HEADER];
        let sender = u32::from_be_bytes(header[4..8].try_into().ok()?);
        let counter = u64::from_be_bytes(header[8..16].try_into().ok()?);
        let epoch = u32::from_be_bytes(header[16..20].try_into().ok()?);
        let time = i64::from(u32::from_be_bytes(header[20..24].try_into().ok()?));
        if sender == self.sender {
            // Our own datagram reflected back.
            return reject();
        }
        // Unix seconds wrap a u32 in 2106; compare modulo 2^32.
        let now = now_secs() & 0xffff_ffff;
        let skew = (now - time).rem_euclid(1 << 32);
        if skew.min((1i64 << 32) - skew) > MAX_SKEW_SECS {
            return reject();
        }
        let Some((_, cipher)) = self.key(Some(epoch)) else {
            return reject();
        };
        let nonce: [u8; 12] = header[4..16].try_into().ok()?;
        let Some(frame) = cipher.open(&nonce, &self.aad(header), &datagram[HEADER..]) else {
            return reject();
        };
        // Only authenticated datagrams move the replay window.
        let fresh = self
            .windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((epoch, sender))
            .or_default()
            .accept(counter);
        if !fresh || frame.len() < 14 {
            return reject();
        }
        let src: Mac = frame[6..12].try_into().ok()?;
        if src[0] & 1 == 1 || local.contains(&src) {
            return reject();
        }
        self.learned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(src, (peer, Instant::now()));
        Some(Inbound { frame, from })
    }

    /// Peers a locally sent frame goes to: the learned peer for a known unicast destination, otherwise every peer.
    pub fn destinations(&self, frame: &[u8]) -> Vec<SocketAddr> {
        if frame.len() >= 6 && frame[0] & 1 == 0 {
            let dst: Mac = frame[0..6].try_into().unwrap_or([0; 6]);
            let mut learned = self.learned.lock().unwrap_or_else(|e| e.into_inner());
            match learned.get(&dst) {
                Some((peer, at)) if at.elapsed() < LEARN_TTL => return vec![*peer],
                Some(_) => {
                    learned.remove(&dst);
                }
                None => {}
            }
        }
        self.peers()
    }

    /// Seals `frame` and sends it to its peers. Never blocks the switch: a full socket drops the datagram.
    pub fn forward(&self, frame: &[u8]) {
        let Some(sock) = &self.socket else { return };
        let Some(datagram) = self.seal(frame) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for peer in self.destinations(frame) {
            if sock.send_to(&datagram, peer).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Receives one datagram (waits up to the read timeout). `None` on timeout or for anything rejected.
    pub fn recv(&self, local: &[Mac]) -> Option<Inbound> {
        let sock = self.socket.as_ref()?;
        let mut buf = vec![0u8; MAX_DATAGRAM + 1];
        let (n, from) = sock.recv_from(&mut buf).ok()?;
        self.open(&buf[..n], from, local)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in cipher for protocol tests: XOR with a key byte and a 16-byte "tag" that binds nonce and aad.
    pub(crate) struct Toy(pub u8);

    fn tag(nonce: &[u8; 12], aad: &[u8], msg: &[u8], k: u8) -> [u8; 16] {
        let mut t = [k; 16];
        for (i, b) in nonce.iter().chain(aad).chain(msg).enumerate() {
            t[i % 16] = t[i % 16].wrapping_mul(31).wrapping_add(*b);
        }
        t
    }

    impl FrameCipher for Toy {
        fn seal(&self, nonce: &[u8; 12], aad: &[u8], frame: &[u8]) -> Option<Vec<u8>> {
            let mut out: Vec<u8> = frame.iter().map(|b| b ^ self.0).collect();
            out.extend_from_slice(&tag(nonce, aad, frame, self.0));
            Some(out)
        }
        fn open(&self, nonce: &[u8; 12], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
            let (ct, t) = sealed.split_at(sealed.len().checked_sub(16)?);
            let frame: Vec<u8> = ct.iter().map(|b| b ^ self.0).collect();
            (tag(nonce, aad, &frame, self.0) == t).then_some(frame)
        }
    }

    const A: Mac = [2, 0, 0, 0, 0, 0xa];
    const B: Mac = [2, 0, 0, 0, 0, 0xb];

    fn frame(dst: Mac, src: Mac) -> Vec<u8> {
        let mut f = dst.to_vec();
        f.extend_from_slice(&src);
        f.extend_from_slice(&[0x08, 0x00]);
        f.extend_from_slice(b"payload");
        f
    }

    fn pair(key_a: u8, key_b: u8) -> (Overlay, Overlay, SocketAddr, SocketAddr) {
        let a_addr: SocketAddr = "10.0.0.1:47000".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:47000".parse().unwrap();
        let cfg = |listen, peer| OverlayConfig {
            network: "acme".into(),
            listen,
            peers: vec![peer],
        };
        (
            Overlay::detached(cfg(a_addr, b_addr), Box::new(Toy(key_a))),
            Overlay::detached(cfg(b_addr, a_addr), Box::new(Toy(key_b))),
            a_addr,
            b_addr,
        )
    }

    #[test]
    fn frames_round_trip_and_teach_the_receiver() {
        let (a, b, a_addr, _) = pair(7, 7);
        let d = a.seal(&frame(B, A)).unwrap();
        assert_eq!(&d[..4], MAGIC);
        let got = b.open(&d, a_addr, &[B]).unwrap();
        assert_eq!(got.frame, frame(B, A));
        // b learned that A is behind a: a reply to A goes only there.
        assert_eq!(b.destinations(&frame(A, B)), vec![a_addr]);
        // Unknown unicast and broadcast go to every peer.
        assert_eq!(b.destinations(&frame([2, 9, 9, 9, 9, 9], B)), vec![a_addr]);
        assert_eq!(b.destinations(&frame([0xff; 6], B)), vec![a_addr]);
        // A datagram from the peer's IP but another port still teaches the configured address, not that port.
        let d = a.seal(&frame(B, A)).unwrap();
        assert!(b.open(&d, "10.0.0.1:5555".parse().unwrap(), &[B]).is_some());
        assert_eq!(b.destinations(&frame(A, B)), vec![a_addr]);
    }

    #[test]
    fn replays_wrong_keys_strangers_and_spoofs_are_dropped() {
        let (a, b, a_addr, _) = pair(7, 7);
        let d = a.seal(&frame(B, A)).unwrap();
        assert!(b.open(&d, a_addr, &[B]).is_some());
        assert!(b.open(&d, a_addr, &[B]).is_none(), "replay");
        // A stranger's address, even with a valid datagram.
        let d2 = a.seal(&frame(B, A)).unwrap();
        assert!(
            b.open(&d2, "10.9.9.9:47000".parse().unwrap(), &[B])
                .is_none()
        );
        // Tampering.
        let mut d3 = a.seal(&frame(B, A)).unwrap();
        let last = d3.len() - 20;
        d3[last] ^= 1;
        assert!(b.open(&d3, a_addr, &[B]).is_none(), "tampered");
        // A peer claiming one of our own guests' MACs.
        let d4 = a.seal(&frame([0xff; 6], B)).unwrap();
        assert!(b.open(&d4, a_addr, &[B]).is_none(), "spoofed local MAC");
        // Another network's key.
        let (a2, b2, a2_addr, _) = pair(7, 8);
        assert!(
            b2.open(&a2.seal(&frame(B, A)).unwrap(), a2_addr, &[B])
                .is_none()
        );
        // Another network's name with the same key.
        let mut other = Overlay::detached(
            OverlayConfig {
                network: "other".into(),
                listen: a_addr,
                peers: vec![],
            },
            Box::new(Toy(7)),
        );
        other.sender = 99;
        assert!(
            b.open(&other.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_none()
        );
        assert!(b.dropped.load(Ordering::Relaxed) >= 4);
    }

    #[test]
    fn the_replay_window_accepts_reordering_within_it_only() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(w.accept(12));
        assert!(w.accept(11), "late but within the window");
        assert!(!w.accept(11), "duplicate");
        assert!(w.accept(5000));
        assert!(!w.accept(5000 - WINDOW), "too old");
        assert!(w.accept(5000 - WINDOW + 1));
        assert!(!w.accept(12), "far behind");
        // A jump wider than the window clears every slot.
        assert!(w.accept(10_000));
        assert!(w.accept(9_990));
    }

    #[test]
    fn peers_can_change_while_running() {
        let (a, b, a_addr, _) = pair(7, 7);
        assert!(
            b.open(&a.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_some()
        );
        assert_eq!(b.destinations(&frame(A, B)), vec![a_addr]);
        // a leaves the network: its datagrams are refused and what was learned behind it is forgotten.
        let c_addr: SocketAddr = "10.0.0.3:47000".parse().unwrap();
        b.set_peers(vec![c_addr]);
        assert!(
            b.open(&a.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_none()
        );
        assert_eq!(b.destinations(&frame(A, B)), vec![c_addr]);
        assert_eq!(b.peer_count(), 1);
    }

    fn k(epoch: u32, key: u8) -> (u32, Box<dyn FrameCipher>) {
        (epoch, Box::new(Toy(key)))
    }

    #[test]
    fn keys_rotate_without_dropping_traffic() {
        let a_addr: SocketAddr = "10.0.0.1:47000".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:47000".parse().unwrap();
        let cfg = |listen, peer| OverlayConfig {
            network: "acme".into(),
            listen,
            peers: vec![peer],
        };
        let a = Overlay::detached_keyed(cfg(a_addr, b_addr), vec![k(1, 1)]);
        let b = Overlay::detached_keyed(cfg(b_addr, a_addr), vec![k(1, 1)]);
        assert!(
            b.open(&a.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_some()
        );
        // b rotates first: it still opens a's epoch-1 datagrams, and keeps sealing with epoch 1 for a while, so a
        // (not rotated yet) still opens what b sends.
        b.set_keys(vec![k(2, 2), k(1, 1)]);
        assert_eq!(b.epochs(), vec![2, 1]);
        assert!(
            b.open(&a.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_some()
        );
        assert!(
            a.open(&b.seal(&frame(A, B)).unwrap(), b_addr, &[A])
                .is_some()
        );
        // Once b seals with epoch 2, a refuses it until a rotates too.
        b.set_keys_with_delay(vec![k(2, 2), k(1, 1)], Duration::ZERO);
        assert!(
            a.open(&b.seal(&frame(A, B)).unwrap(), b_addr, &[A])
                .is_none()
        );
        a.set_keys_with_delay(vec![k(2, 2), k(1, 1)], Duration::ZERO);
        assert!(
            a.open(&b.seal(&frame(A, B)).unwrap(), b_addr, &[A])
                .is_some()
        );
        // The old key retired everywhere: epoch-1 datagrams are refused.
        let old = Overlay::detached_keyed(cfg(a_addr, b_addr), vec![k(1, 1)]);
        b.set_keys(vec![k(3, 3), k(2, 2)]);
        assert!(
            b.open(&old.seal(&frame(B, A)).unwrap(), a_addr, &[B])
                .is_none()
        );
        // A datagram cannot claim another epoch: the epoch is authenticated.
        let mut d = a.seal(&frame(B, A)).unwrap();
        d[19] = 3;
        assert!(b.open(&d, a_addr, &[B]).is_none());
    }

    #[test]
    fn stale_or_future_datagrams_are_dropped() {
        let (a, b, a_addr, _) = pair(7, 7);
        let now = now_secs();
        assert!(
            b.open(&a.seal_at(&frame(B, A), now - 60).unwrap(), a_addr, &[B])
                .is_some()
        );
        assert!(
            b.open(&a.seal_at(&frame(B, A), now - 600).unwrap(), a_addr, &[B])
                .is_none()
        );
        assert!(
            b.open(&a.seal_at(&frame(B, A), now + 600).unwrap(), a_addr, &[B])
                .is_none()
        );
    }

    #[test]
    fn key_files_parse_with_epochs() {
        let k1 = "11".repeat(32);
        let k2 = "22".repeat(32);
        assert_eq!(parse_keys(&k1).unwrap(), vec![(0, [0x11; 32])]);
        assert_eq!(
            parse_keys(&format!("5 {k2}\n4 {k1}\n")).unwrap(),
            vec![(5, [0x22; 32]), (4, [0x11; 32])]
        );
        assert!(
            parse_keys(&format!("5 {k2}\n5 {k1}")).is_none(),
            "duplicate epoch"
        );
        assert!(parse_keys(&format!("1 {k1}\n2 {k1}\n3 {k1}")).is_none());
        assert!(parse_keys("").is_none());
        assert!(parse_keys("x zz").is_none());
    }

    #[test]
    fn keys_parse_from_hex() {
        let k = "00".repeat(31) + "ff";
        assert_eq!(parse_key(&format!(" {k}\n")).unwrap()[31], 0xff);
        assert!(parse_key("abc").is_none());
        assert!(parse_key(&"zz".repeat(32)).is_none());
    }
}
