// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! mDNS reflection across the encrypted FluxMesh overlay.
//!
//! Local multicast packets are wrapped in a tiny unicast envelope and copied to
//! selected mesh peers. Received envelopes are emitted back onto local mDNS.
//! A bounded digest cache prevents reflection loops and multicast storms.

use crate::{MeshError, Result};
use ring::digest;
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

pub const OVERLAY_MDNS_PORT: u16 = 53530;
const MAGIC: &[u8; 4] = b"FM5\0";
const MAX_PACKET: usize = 9000;

#[derive(Debug)]
struct SeenCache {
    ttl: Duration,
    cap: usize,
    order: VecDeque<[u8; 16]>,
    seen: HashMap<[u8; 16], Instant>,
}
impl SeenCache {
    fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            ttl,
            cap,
            order: VecDeque::new(),
            seen: HashMap::new(),
        }
    }
    fn insert_new(&mut self, packet: &[u8]) -> bool {
        self.gc();
        let id = packet_id(packet);
        if self.seen.contains_key(&id) {
            return false;
        }
        self.seen.insert(id, Instant::now());
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
    fn gc(&mut self) {
        let now = Instant::now();
        while self.order.front().is_some_and(|id| {
            self.seen
                .get(id)
                .is_some_and(|t| now.duration_since(*t) > self.ttl)
        }) {
            if let Some(id) = self.order.pop_front() {
                self.seen.remove(&id);
            }
        }
    }
}

pub async fn run_bridge(peers: Vec<SocketAddr>, overlay_bind: SocketAddr) -> Result<()> {
    run_bridge_dual(peers, overlay_bind, None).await
}

/// Reflect both IPv4 224.0.0.251 and IPv6 ff02::fb mDNS. IPv6 is enabled
/// only when an explicit interface index is supplied; using ifindex 0 for a
/// link-local multicast group is ambiguous on multi-homed hosts.
pub async fn run_bridge_dual(
    peers: Vec<SocketAddr>,
    overlay_bind: SocketAddr,
    ipv6_ifindex: Option<u32>,
) -> Result<()> {
    let local4 = std::net::UdpSocket::bind("0.0.0.0:5353")?;
    local4.set_nonblocking(true)?;
    local4.join_multicast_v4(&Ipv4Addr::new(224, 0, 0, 251), &Ipv4Addr::UNSPECIFIED)?;
    local4.set_multicast_loop_v4(false)?;
    let local4 = UdpSocket::from_std(local4)?;

    let local6 = if let Some(ifindex) = ipv6_ifindex {
        let sock = std::net::UdpSocket::bind("[::]:5353")?;
        sock.set_nonblocking(true)?;
        sock.join_multicast_v6(&"ff02::fb".parse::<Ipv6Addr>().unwrap(), ifindex)?;
        sock.set_multicast_loop_v6(false)?;
        Some(UdpSocket::from_std(sock)?)
    } else {
        None
    };

    let overlay = UdpSocket::bind(overlay_bind).await?;
    let mut v4_buf = vec![0u8; MAX_PACKET];
    let mut v6_buf = vec![0u8; MAX_PACKET];
    let mut overlay_buf = vec![0u8; MAX_PACKET + 32];
    let mut seen = SeenCache::new(4096, Duration::from_secs(3));
    loop {
        tokio::select! {
            packet = local4.recv_from(&mut v4_buf) => {
                let (n, _) = packet?;
                if n == 0 || !seen.insert_new(&v4_buf[..n]) { continue; }
                let frame = encode_frame(&v4_buf[..n])?;
                for peer in &peers { let _ = overlay.send_to(&frame, peer).await; }
            }
            packet = async {
                match &local6 {
                    Some(sock) => sock.recv_from(&mut v6_buf).await,
                    None => std::future::pending().await,
                }
            } => {
                let (n, _) = packet?;
                if n == 0 || !seen.insert_new(&v6_buf[..n]) { continue; }
                let frame = encode_frame(&v6_buf[..n])?;
                for peer in &peers { let _ = overlay.send_to(&frame, peer).await; }
            }
            packet = overlay.recv_from(&mut overlay_buf) => {
                let (n, _) = packet?;
                let payload = match decode_frame(&overlay_buf[..n]) { Ok(v)=>v, Err(_)=>continue };
                if !seen.insert_new(payload) { continue; }
                let _ = local4.send_to(payload, "224.0.0.251:5353").await;
                if let (Some(sock), Some(ifindex)) = (&local6, ipv6_ifindex) {
                    let target = SocketAddr::V6(SocketAddrV6::new("ff02::fb".parse().unwrap(),5353,0,ifindex));
                    let _ = sock.send_to(payload, target).await;
                }
            }
        }
    }
}

pub fn encode_frame(packet: &[u8]) -> Result<Vec<u8>> {
    if packet.is_empty() || packet.len() > MAX_PACKET {
        return Err(MeshError::InvalidState("mDNS packet size invalid".into()));
    }
    let mut out = Vec::with_capacity(packet.len() + 6);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    out.extend_from_slice(packet);
    Ok(out)
}
pub fn decode_frame(frame: &[u8]) -> Result<&[u8]> {
    if frame.len() < 6 || &frame[..4] != MAGIC {
        return Err(MeshError::InvalidState(
            "invalid FluxMesh mDNS frame".into(),
        ));
    }
    let n = u16::from_be_bytes([frame[4], frame[5]]) as usize;
    if n == 0 || n > MAX_PACKET || frame.len() != n + 6 {
        return Err(MeshError::InvalidState(
            "invalid FluxMesh mDNS frame length".into(),
        ));
    }
    Ok(&frame[6..])
}
fn packet_id(packet: &[u8]) -> [u8; 16] {
    let d = digest::digest(&digest::SHA256, packet);
    let mut out = [0u8; 16];
    out.copy_from_slice(&d.as_ref()[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_roundtrip() {
        let p = b"dns";
        assert_eq!(decode_frame(&encode_frame(p).unwrap()).unwrap(), p);
    }
    #[test]
    fn cache_drops_duplicate() {
        let mut c = SeenCache::new(4, Duration::from_secs(1));
        assert!(c.insert_new(b"x"));
        assert!(!c.insert_new(b"x"));
    }
}
