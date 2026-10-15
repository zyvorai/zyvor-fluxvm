// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal DHCPv6 Prefix Delegation client (RFC 8415 IA_PD/IAPREFIX).

use crate::{MeshError, Result};
use ring::rand::{SecureRandom, SystemRandom};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

const CLIENTID: u16 = 1;
const SERVERID: u16 = 2;
const IA_PD: u16 = 25;
const IAPREFIX: u16 = 26;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegatedPrefix {
    pub prefix: Ipv6Addr,
    pub prefix_len: u8,
    pub preferred_lifetime: u32,
    pub valid_lifetime: u32,
    pub server_id: Vec<u8>,
}
impl DelegatedPrefix {
    pub fn cidr(&self) -> String {
        format!("{}/{}", self.prefix, self.prefix_len)
    }
}

pub async fn acquire(
    bind: SocketAddr,
    server: SocketAddr,
    scope_id: u32,
    timeout: Duration,
) -> Result<DelegatedPrefix> {
    let server = with_scope(server, scope_id);
    let socket = UdpSocket::bind(bind).await?;
    let mut txid = [0u8; 3];
    SystemRandom::new()
        .fill(&mut txid)
        .map_err(|_| MeshError::InvalidState("DHCPv6 transaction random failed".into()))?;
    let client_id = duid_uuid()?;
    let iaid = iaid();
    let solicit = build_message(
        1,
        txid,
        &[(CLIENTID, client_id.clone()), (IA_PD, ia_pd(iaid, None))],
    );
    socket.send_to(&solicit, server).await?;
    let advertised = recv_matching(&socket, txid, timeout).await?;
    if advertised.first().copied() != Some(2) {
        return Err(MeshError::InvalidState("DHCPv6 expected Advertise".into()));
    }
    let opts = parse_options(&advertised[4..])?;
    let server_id = opt(&opts, SERVERID)
        .ok_or_else(|| MeshError::InvalidState("DHCPv6 Advertise missing ServerID".into()))?
        .to_vec();
    let prefix = parse_ia_pd(
        opt(&opts, IA_PD)
            .ok_or_else(|| MeshError::InvalidState("DHCPv6 Advertise missing IA_PD".into()))?,
        &server_id,
    )?;
    let mut request_txid = [0u8; 3];
    SystemRandom::new()
        .fill(&mut request_txid)
        .map_err(|_| MeshError::InvalidState("DHCPv6 request transaction random failed".into()))?;
    let request = build_message(
        3,
        request_txid,
        &[
            (CLIENTID, client_id),
            (SERVERID, server_id.clone()),
            (IA_PD, ia_pd(iaid, Some(&prefix))),
        ],
    );
    socket.send_to(&request, server).await?;
    let reply = recv_matching(&socket, request_txid, timeout).await?;
    if reply.first().copied() != Some(7) {
        return Err(MeshError::InvalidState("DHCPv6 expected Reply".into()));
    }
    let opts = parse_options(&reply[4..])?;
    parse_ia_pd(
        opt(&opts, IA_PD)
            .ok_or_else(|| MeshError::InvalidState("DHCPv6 Reply missing IA_PD".into()))?,
        &server_id,
    )
}

pub fn route_plan(prefix: &DelegatedPrefix, interface: &str) -> Result<Vec<Vec<String>>> {
    if interface.is_empty()
        || !interface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidState("invalid IPv6 PD interface".into()));
    }
    Ok(vec![
        vec![
            "sysctl".into(),
            "-w".into(),
            "net.ipv6.conf.all.forwarding=1".into(),
        ],
        vec![
            "ip".into(),
            "-6".into(),
            "route".into(),
            "replace".into(),
            prefix.cidr(),
            "dev".into(),
            interface.into(),
        ],
    ])
}

fn build_message(kind: u8, txid: [u8; 3], opts: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![kind, txid[0], txid[1], txid[2]];
    for (code, value) in opts {
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
    }
    out
}
fn parse_options(mut b: &[u8]) -> Result<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < 4 {
            return Err(MeshError::InvalidState("truncated DHCPv6 option".into()));
        }
        let code = u16::from_be_bytes([b[0], b[1]]);
        let n = u16::from_be_bytes([b[2], b[3]]) as usize;
        b = &b[4..];
        if b.len() < n {
            return Err(MeshError::InvalidState(
                "truncated DHCPv6 option value".into(),
            ));
        }
        out.push((code, b[..n].to_vec()));
        b = &b[n..];
    }
    Ok(out)
}
fn opt(opts: &[(u16, Vec<u8>)], code: u16) -> Option<&[u8]> {
    opts.iter()
        .find(|(c, _)| *c == code)
        .map(|(_, v)| v.as_slice())
}
fn ia_pd(iaid: u32, prefix: Option<&DelegatedPrefix>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&iaid.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    if let Some(p) = prefix {
        let mut v = Vec::new();
        v.extend_from_slice(&p.preferred_lifetime.to_be_bytes());
        v.extend_from_slice(&p.valid_lifetime.to_be_bytes());
        v.push(p.prefix_len);
        v.extend_from_slice(&p.prefix.octets());
        out.extend_from_slice(&IAPREFIX.to_be_bytes());
        out.extend_from_slice(&(v.len() as u16).to_be_bytes());
        out.extend_from_slice(&v);
    }
    out
}
fn parse_ia_pd(bytes: &[u8], server_id: &[u8]) -> Result<DelegatedPrefix> {
    if bytes.len() < 12 {
        return Err(MeshError::InvalidState("short IA_PD".into()));
    }
    let opts = parse_options(&bytes[12..])?;
    let p = opt(&opts, IAPREFIX)
        .ok_or_else(|| MeshError::InvalidState("IA_PD has no IAPREFIX".into()))?;
    if p.len() < 25 {
        return Err(MeshError::InvalidState("short IAPREFIX".into()));
    }
    let preferred = u32::from_be_bytes(p[0..4].try_into().unwrap());
    let valid = u32::from_be_bytes(p[4..8].try_into().unwrap());
    let len = p[8];
    if len > 128 {
        return Err(MeshError::InvalidState(
            "invalid delegated prefix length".into(),
        ));
    }
    let mut a = [0u8; 16];
    a.copy_from_slice(&p[9..25]);
    Ok(DelegatedPrefix {
        prefix: Ipv6Addr::from(a),
        prefix_len: len,
        preferred_lifetime: preferred,
        valid_lifetime: valid,
        server_id: server_id.to_vec(),
    })
}
async fn recv_matching(socket: &UdpSocket, txid: [u8; 3], timeout: Duration) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    let (n, _) = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
        .await
        .map_err(|_| MeshError::InvalidState("DHCPv6 timeout".into()))??;
    buf.truncate(n);
    if buf.len() < 4 || buf[1..4] != txid[..] {
        return Err(MeshError::InvalidState(
            "DHCPv6 transaction mismatch".into(),
        ));
    }
    Ok(buf)
}
fn duid_uuid() -> Result<Vec<u8>> {
    let mut id = [0u8; 16];
    SystemRandom::new()
        .fill(&mut id)
        .map_err(|_| MeshError::InvalidState("DUID random failed".into()))?;
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&id);
    Ok(out)
}
fn iaid() -> u32 {
    let mut v = [0u8; 4];
    if SystemRandom::new().fill(&mut v).is_ok() {
        u32::from_be_bytes(v)
    } else {
        0x46584d35
    }
}
fn with_scope(addr: SocketAddr, scope: u32) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(
            *v6.ip(),
            v6.port(),
            v6.flowinfo(),
            if v6.scope_id() == 0 {
                scope
            } else {
                v6.scope_id()
            },
        )),
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_prefix() {
        let server = vec![1, 2];
        let p = DelegatedPrefix {
            prefix: "2001:db8:1::".parse().unwrap(),
            prefix_len: 56,
            preferred_lifetime: 100,
            valid_lifetime: 200,
            server_id: server.clone(),
        };
        let bytes = ia_pd(7, Some(&p));
        let got = parse_ia_pd(&bytes, &server).unwrap();
        assert_eq!(got, p);
    }
    #[test]
    fn plan_is_idempotent_replace() {
        let p = DelegatedPrefix {
            prefix: "2001:db8::".parse().unwrap(),
            prefix_len: 56,
            preferred_lifetime: 1,
            valid_lifetime: 2,
            server_id: vec![],
        };
        assert!(route_plan(&p, "fluxmesh0").unwrap()[1].contains(&"replace".into()));
    }
}
