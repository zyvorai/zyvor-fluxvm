// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal RFC 5389 STUN binding client and UDP hole-punch helper.
//!
//! FluxMesh only needs the mapped public endpoint. This module deliberately
//! avoids TURN/ICE policy so the mesh control plane can decide whether a
//! direct WireGuard path is viable or a relay should be used.

use crate::{MeshError, NatObservation, NatType, Result, classify_nat};
use std::io;
#[cfg(unix)]
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER_LEN: usize = 20;
static TX_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunObservation {
    pub server: SocketAddr,
    pub mapped_addr: SocketAddr,
    pub rtt_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunReport {
    pub nat_type: NatType,
    pub observations: Vec<StunObservation>,
}

impl StunReport {
    pub fn public_endpoint(&self) -> Option<SocketAddr> {
        self.observations.first().map(|o| o.mapped_addr)
    }
}

pub fn probe(
    server: SocketAddr,
    bind: SocketAddr,
    timeout: Duration,
) -> io::Result<StunObservation> {
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;

    let txid = transaction_id();
    let mut request = [0u8; HEADER_LEN];
    request[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    request[2..4].copy_from_slice(&0u16.to_be_bytes());
    request[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    request[8..20].copy_from_slice(&txid);

    let started = Instant::now();
    socket.send_to(&request, server)?;
    let mut buf = [0u8; 2048];
    let (n, _) = socket.recv_from(&mut buf)?;
    let mapped_addr = parse_binding_response(&buf[..n], txid)?;
    Ok(StunObservation {
        server,
        mapped_addr,
        rtt_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

pub fn probe_many(
    servers: &[SocketAddr],
    bind: SocketAddr,
    timeout: Duration,
) -> Result<StunReport> {
    if servers.is_empty() {
        return Err(MeshError::InvalidState(
            "at least one STUN server is required".into(),
        ));
    }
    let mut observations = Vec::new();
    let mut last_error = None;
    for server in servers {
        match probe(*server, bind, timeout) {
            Ok(obs) => observations.push(obs),
            Err(e) => last_error = Some(e),
        }
    }
    if observations.is_empty() {
        return Err(MeshError::Io(last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "no STUN server answered")
        })));
    }
    let nat_observations = observations
        .iter()
        .enumerate()
        .map(|(index, obs)| NatObservation {
            observer: format!("stun-{index}"),
            mapped_addr: obs.mapped_addr,
        })
        .collect::<Vec<_>>();
    Ok(StunReport {
        nat_type: classify_nat(&nat_observations),
        observations,
    })
}

/// Send UDP packets that create/refresh a NAT mapping before the encrypted
/// WireGuard handshake starts. The payload has no authorization meaning; it is
/// only a NAT-opening datagram and can safely be ignored by the peer.
pub fn punch_udp(
    bind: SocketAddr,
    peer: SocketAddr,
    attempts: u16,
    interval: Duration,
) -> io::Result<SocketAddr> {
    if attempts == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hole-punch attempts must be greater than zero",
        ));
    }
    let socket = UdpSocket::bind(bind)?;
    let local = socket.local_addr()?;
    const PUNCH: &[u8] = b"FLUXMESH-PUNCH-v1";
    for n in 0..attempts {
        socket.send_to(PUNCH, peer)?;
        if n + 1 < attempts {
            std::thread::sleep(interval);
        }
    }
    Ok(local)
}

fn parse_binding_response(packet: &[u8], expected_txid: [u8; 12]) -> io::Result<SocketAddr> {
    if packet.len() < HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short STUN response",
        ));
    }
    let kind = u16::from_be_bytes([packet[0], packet[1]]);
    if kind != BINDING_SUCCESS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected STUN message type 0x{kind:04x}"),
        ));
    }
    let body_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if packet.len() < HEADER_LEN + body_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated STUN response",
        ));
    }
    if u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]) != MAGIC_COOKIE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid STUN magic cookie",
        ));
    }
    if packet[8..20] != expected_txid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "STUN transaction id mismatch",
        ));
    }

    let end = HEADER_LEN + body_len;
    let mut offset = HEADER_LEN;
    let mut mapped_fallback = None;
    while offset + 4 <= end {
        let attr = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        offset += 4;
        if offset + len > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated STUN attribute",
            ));
        }
        let value = &packet[offset..offset + len];
        if attr == ATTR_XOR_MAPPED_ADDRESS {
            return parse_address(value, true, expected_txid);
        }
        if attr == ATTR_MAPPED_ADDRESS {
            mapped_fallback = Some(parse_address(value, false, expected_txid)?);
        }
        offset += (len + 3) & !3;
    }
    mapped_fallback.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "STUN response has no mapped address",
        )
    })
}

fn parse_address(value: &[u8], xor: bool, txid: [u8; 12]) -> io::Result<SocketAddr> {
    if value.len() < 4 || value[0] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid STUN address attribute",
        ));
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    match value[1] {
        0x01 if value.len() >= 8 => {
            let mut octets = [value[4], value[5], value[6], value[7]];
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for (byte, mask) in octets.iter_mut().zip(cookie) {
                    *byte ^= mask;
                }
            }
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x02 if value.len() >= 20 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            if xor {
                let cookie = MAGIC_COOKIE.to_be_bytes();
                for i in 0..4 {
                    octets[i] ^= cookie[i];
                }
                for i in 0..12 {
                    octets[i + 4] ^= txid[i];
                }
            }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        family => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported STUN address family {family}"),
        )),
    }
}

fn transaction_id() -> [u8; 12] {
    #[cfg(unix)]
    {
        let mut out = [0u8; 12];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut out))
            .is_ok()
        {
            return out;
        }
    }
    // Non-Unix or entropy-source failure fallback. Transaction ids are not
    // credentials, but uniqueness still matters for matching responses.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TX_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let mixed = now ^ (counter << 37) ^ ((std::process::id() as u128) << 71);
    let bytes = mixed.to_be_bytes();
    let mut out = [0u8; 12];
    out.copy_from_slice(&bytes[4..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_ipv4() {
        let txid = [7u8; 12];
        let addr: SocketAddr = "203.0.113.9:51820".parse().unwrap();
        let mut packet = Vec::new();
        packet.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        packet.extend_from_slice(&12u16.to_be_bytes());
        packet.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        packet.extend_from_slice(&txid);
        packet.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        packet.extend_from_slice(&8u16.to_be_bytes());
        packet.extend_from_slice(&[0, 1]);
        packet.extend_from_slice(&(addr.port() ^ ((MAGIC_COOKIE >> 16) as u16)).to_be_bytes());
        let cookie = MAGIC_COOKIE.to_be_bytes();
        let IpAddr::V4(ip) = addr.ip() else {
            unreachable!()
        };
        let octets = ip.octets();
        for i in 0..4 {
            packet.push(octets[i] ^ cookie[i]);
        }
        assert_eq!(parse_binding_response(&packet, txid).unwrap(), addr);
    }

    #[test]
    fn transaction_ids_change() {
        assert_ne!(transaction_id(), transaction_id());
    }
}
