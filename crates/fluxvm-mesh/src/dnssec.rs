// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal DNS TXT validation with EDNS(0) DO and DNSSEC AD-bit enforcement.
//!
//! This is not a validating resolver. When `require_ad` is true it requires a
//! trusted recursive resolver to return the Authenticated Data bit, so the
//! operator should point it at a resolver whose trust anchor policy they own.

use crate::{MeshError, Result};
use ring::rand::{SecureRandom, SystemRandom};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;

const TYPE_TXT: u16 = 16;
const TYPE_OPT: u16 = 41;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const FLAG_AD: u16 = 0x0020;
const FLAG_RD: u16 = 0x0100;
const DNSSEC_OK: u32 = 0x0000_8000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnssecReport {
    pub authenticated: bool,
    pub found: bool,
    pub txt_values: Vec<String>,
}

pub async fn verify_txt(
    resolver: SocketAddr,
    name: &str,
    expected: &str,
    require_ad: bool,
    timeout: Duration,
) -> Result<DnssecReport> {
    let id = query_id()?;
    let query = build_query(id, name)?;
    let bind: SocketAddr = if resolver.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.send_to(&query, resolver).await?;
    let mut buf = vec![0u8; 4096];
    let (n, _) = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
        .await
        .map_err(|_| {
            MeshError::InvalidState(format!("DNSSEC TXT query to {resolver} timed out"))
        })??;
    let report = parse_response(&buf[..n], id, expected)?;
    if require_ad && !report.authenticated {
        return Err(MeshError::InvalidState(format!(
            "DNS TXT response for {name} was not DNSSEC-authenticated (AD=0)"
        )));
    }
    if !report.found {
        return Err(MeshError::InvalidState(format!(
            "DNS TXT response for {name} did not contain the expected ACME value"
        )));
    }
    Ok(report)
}

fn build_query(id: u16, name: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&FLAG_RD.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    out.extend_from_slice(&0u16.to_be_bytes()); // ancount
    out.extend_from_slice(&0u16.to_be_bytes()); // nscount
    out.extend_from_slice(&1u16.to_be_bytes()); // arcount (OPT)
    encode_name(&mut out, name)?;
    out.extend_from_slice(&TYPE_TXT.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.push(0); // root owner for OPT
    out.extend_from_slice(&TYPE_OPT.to_be_bytes());
    out.extend_from_slice(&1232u16.to_be_bytes()); // UDP payload
    out.extend_from_slice(&DNSSEC_OK.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    Ok(out)
}

fn parse_response(data: &[u8], id: u16, expected: &str) -> Result<DnssecReport> {
    if data.len() < 12 || u16::from_be_bytes([data[0], data[1]]) != id {
        return Err(MeshError::InvalidState(
            "invalid DNS response id/header".into(),
        ));
    }
    let flags = u16::from_be_bytes([data[2], data[3]]);
    if flags & FLAG_QR == 0 || flags & 0x000f != 0 {
        return Err(MeshError::InvalidState(format!(
            "DNS response was not a successful answer (flags=0x{flags:04x})"
        )));
    }
    let qd = u16::from_be_bytes([data[4], data[5]]) as usize;
    let an = u16::from_be_bytes([data[6], data[7]]) as usize;
    let mut pos = 12usize;
    for _ in 0..qd {
        skip_name(data, &mut pos)?;
        pos = pos
            .checked_add(4)
            .ok_or_else(|| MeshError::InvalidState("DNS offset overflow".into()))?;
        if pos > data.len() {
            return Err(MeshError::InvalidState("truncated DNS question".into()));
        }
    }
    let mut values = Vec::new();
    for _ in 0..an {
        skip_name(data, &mut pos)?;
        if pos + 10 > data.len() {
            return Err(MeshError::InvalidState("truncated DNS answer".into()));
        }
        let kind = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let class = u16::from_be_bytes([data[pos + 2], data[pos + 3]]);
        let rdlen = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;
        let end = pos
            .checked_add(rdlen)
            .ok_or_else(|| MeshError::InvalidState("DNS RDATA overflow".into()))?;
        if end > data.len() {
            return Err(MeshError::InvalidState("truncated DNS RDATA".into()));
        }
        if kind == TYPE_TXT && class == CLASS_IN {
            let mut at = pos;
            let mut txt = String::new();
            while at < end {
                let len = data[at] as usize;
                at += 1;
                if at + len > end {
                    return Err(MeshError::InvalidState("malformed DNS TXT RDATA".into()));
                }
                txt.push_str(&String::from_utf8_lossy(&data[at..at + len]));
                at += len;
            }
            values.push(txt);
        }
        pos = end;
    }
    Ok(DnssecReport {
        authenticated: flags & FLAG_AD != 0,
        found: values.iter().any(|v| v == expected),
        txt_values: values,
    })
}

fn encode_name(out: &mut Vec<u8>, name: &str) -> Result<()> {
    let trimmed = name.trim_end_matches('.');
    if trimmed.is_empty() || trimmed.len() > 253 {
        return Err(MeshError::InvalidState(format!(
            "invalid DNS name {name:?}"
        )));
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(MeshError::InvalidState(format!(
                "invalid DNS label in {name:?}"
            )));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

fn skip_name(data: &[u8], pos: &mut usize) -> Result<()> {
    let mut jumps = 0usize;
    loop {
        if *pos >= data.len() {
            return Err(MeshError::InvalidState("truncated DNS name".into()));
        }
        let len = data[*pos];
        if len & 0xc0 == 0xc0 {
            if *pos + 1 >= data.len() {
                return Err(MeshError::InvalidState(
                    "truncated DNS compression pointer".into(),
                ));
            }
            *pos += 2;
            return Ok(());
        }
        *pos += 1;
        if len == 0 {
            return Ok(());
        }
        if len & 0xc0 != 0 || *pos + len as usize > data.len() {
            return Err(MeshError::InvalidState("invalid DNS label".into()));
        }
        *pos += len as usize;
        jumps += 1;
        if jumps > 127 {
            return Err(MeshError::InvalidState(
                "DNS name exceeded label limit".into(),
            ));
        }
    }
}

fn query_id() -> Result<u16> {
    let mut bytes = [0u8; 2];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| MeshError::InvalidState("generate DNS transaction id".into()))?;
    Ok(u16::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_requests_dnssec() {
        let q = build_query(7, "_acme-challenge.example.com").unwrap();
        assert_eq!(&q[0..2], &7u16.to_be_bytes());
        assert!(q.windows(2).any(|v| v == TYPE_OPT.to_be_bytes()));
        assert!(q.windows(4).any(|v| v == DNSSEC_OK.to_be_bytes()));
    }

    #[test]
    fn rejects_long_label() {
        let name = format!("{}.example", "a".repeat(64));
        assert!(build_query(1, &name).is_err());
    }
}
