// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal private DNS for FluxMesh node and service names. Supports A and
//! SRV queries, one question per packet, and intentionally refuses recursion.

use crate::service::ServiceCatalog;
use crate::{MeshError, MeshState, Result};
use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

pub const DEFAULT_DNS_SUFFIX: &str = "flux";
const TYPE_A: u16 = 1;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsLookup {
    A(Ipv4Addr),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivateDns {
    suffix: String,
    a: BTreeMap<String, Ipv4Addr>,
    srv: BTreeMap<String, (u16, u16, u16, String)>,
}

impl PrivateDns {
    pub fn build(mesh: &MeshState, services: &ServiceCatalog, suffix: &str) -> Result<Self> {
        validate_suffix(suffix)?;
        let suffix = suffix.trim_end_matches('.').to_ascii_lowercase();
        let mut a = BTreeMap::new();
        let mut srv = BTreeMap::new();
        for node in mesh.nodes.values() {
            let name = format!("{}.{}", node.id.to_ascii_lowercase(), suffix);
            if a.insert(name.clone(), node.overlay_ip).is_some() {
                return Err(MeshError::InvalidState(format!(
                    "duplicate DNS name {name}"
                )));
            }
        }
        for service in services.services.values() {
            let name = format!("{}.svc.{}", service.name.to_ascii_lowercase(), suffix);
            a.insert(name.clone(), service.vip);
            let srv_name = format!(
                "_{}._{}.svc.{}",
                service.name.to_ascii_lowercase(),
                service.protocol.as_str(),
                suffix
            );
            srv.insert(srv_name, (0, 0, service.port, name));
        }
        Ok(Self { suffix, a, srv })
    }

    pub fn suffix(&self) -> &str {
        &self.suffix
    }

    pub fn lookup(&self, raw_name: &str) -> Option<DnsLookup> {
        let name = normalize_name(raw_name);
        if let Some(ip) = self.a.get(&name) {
            return Some(DnsLookup::A(*ip));
        }
        self.srv
            .get(&name)
            .map(|(priority, weight, port, target)| DnsLookup::Srv {
                priority: *priority,
                weight: *weight,
                port: *port,
                target: target.clone(),
            })
    }

    pub fn names(&self) -> impl Iterator<Item = (&str, Ipv4Addr)> {
        self.a.iter().map(|(name, ip)| (name.as_str(), *ip))
    }

    pub fn answer_packet(&self, packet: &[u8]) -> io::Result<Vec<u8>> {
        if packet.len() < 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short DNS packet",
            ));
        }
        let id = &packet[0..2];
        let qd = u16::from_be_bytes([packet[4], packet[5]]);
        if qd != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "FluxDNS supports one question",
            ));
        }
        let (name, next) = parse_name(packet, 12)?;
        if next + 4 > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated DNS question",
            ));
        }
        let qtype = u16::from_be_bytes([packet[next], packet[next + 1]]);
        let qclass = u16::from_be_bytes([packet[next + 2], packet[next + 3]]);
        let question_end = next + 4;
        let lookup = if qclass == CLASS_IN {
            self.lookup(&name)
        } else {
            None
        };
        let answer = match (qtype, lookup) {
            (TYPE_A, Some(DnsLookup::A(ip))) => Some(Answer::A(ip)),
            (
                TYPE_SRV,
                Some(DnsLookup::Srv {
                    priority,
                    weight,
                    port,
                    target,
                }),
            ) => Some(Answer::Srv {
                priority,
                weight,
                port,
                target,
            }),
            _ => None,
        };

        let mut out = Vec::with_capacity(packet.len() + 64);
        out.extend_from_slice(id);
        // QR=1, AA=1, recursion not available. NXDOMAIN only for names that
        // do not exist; unsupported qtypes return NOERROR/NODATA.
        let name_exists = self.a.contains_key(&normalize_name(&name))
            || self.srv.contains_key(&normalize_name(&name));
        let rcode = if !name_exists { 3u16 } else { 0 };
        let flags = 0x8400u16 | rcode;
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(answer.is_some() as u16).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&packet[12..question_end]);
        if let Some(answer) = answer {
            // Pointer to the qname at offset 12.
            out.extend_from_slice(&0xc00cu16.to_be_bytes());
            match answer {
                Answer::A(ip) => {
                    out.extend_from_slice(&TYPE_A.to_be_bytes());
                    out.extend_from_slice(&CLASS_IN.to_be_bytes());
                    out.extend_from_slice(&30u32.to_be_bytes());
                    out.extend_from_slice(&4u16.to_be_bytes());
                    out.extend_from_slice(&ip.octets());
                }
                Answer::Srv {
                    priority,
                    weight,
                    port,
                    target,
                } => {
                    let target_wire = encode_name(&target)?;
                    out.extend_from_slice(&TYPE_SRV.to_be_bytes());
                    out.extend_from_slice(&CLASS_IN.to_be_bytes());
                    out.extend_from_slice(&30u32.to_be_bytes());
                    out.extend_from_slice(&((6 + target_wire.len()) as u16).to_be_bytes());
                    out.extend_from_slice(&priority.to_be_bytes());
                    out.extend_from_slice(&weight.to_be_bytes());
                    out.extend_from_slice(&port.to_be_bytes());
                    out.extend_from_slice(&target_wire);
                }
            }
        }
        Ok(out)
    }
}

enum Answer {
    A(Ipv4Addr),
    Srv {
        priority: u16,
        weight: u16,
        port: u16,
        target: String,
    },
}

pub fn serve_udp(bind: SocketAddr, dns: PrivateDns) -> io::Result<()> {
    let socket = UdpSocket::bind(bind)?;
    let mut buf = [0u8; 4096];
    loop {
        let (n, peer) = socket.recv_from(&mut buf)?;
        if let Ok(answer) = dns.answer_packet(&buf[..n]) {
            let _ = socket.send_to(&answer, peer);
        }
    }
}

fn normalize_name(value: &str) -> String {
    value.trim_end_matches('.').to_ascii_lowercase()
}

fn validate_suffix(value: &str) -> Result<()> {
    let value = value.trim_end_matches('.');
    if value.is_empty() || value.len() > 63 {
        return Err(MeshError::InvalidState("invalid DNS suffix".into()));
    }
    for label in value.split('.') {
        if label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(MeshError::InvalidState(format!(
                "invalid DNS suffix {value}"
            )));
        }
    }
    Ok(())
}

fn parse_name(packet: &[u8], mut offset: usize) -> io::Result<(String, usize)> {
    let mut labels = Vec::new();
    loop {
        if offset >= packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated DNS name",
            ));
        }
        let len = packet[offset] as usize;
        offset += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "compressed DNS question not supported",
            ));
        }
        if len > 63 || offset + len > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid DNS label",
            ));
        }
        let label = std::str::from_utf8(&packet[offset..offset + len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "DNS label is not UTF-8"))?;
        labels.push(label.to_owned());
        offset += len;
    }
    Ok((labels.join("."), offset))
}

fn encode_name(name: &str) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for label in normalize_name(name).split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DNS name",
            ));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ServiceCatalog, ServiceProtocol};
    use crate::{DEFAULT_POOL, MeshState};

    fn dns() -> PrivateDns {
        let mesh =
            MeshState::new(DEFAULT_POOL.parse().unwrap(), "home", "home.local", "key").unwrap();
        let mut services = ServiceCatalog::default();
        services
            .add_service("photos", 443, ServiceProtocol::Tcp)
            .unwrap();
        PrivateDns::build(&mesh, &services, "flux").unwrap()
    }

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(&encode_name(name).unwrap());
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn node_and_service_names_resolve() {
        let dns = dns();
        assert!(matches!(dns.lookup("home.flux"), Some(DnsLookup::A(_))));
        assert!(matches!(
            dns.lookup("photos.svc.flux"),
            Some(DnsLookup::A(_))
        ));
        assert!(matches!(
            dns.lookup("_photos._tcp.svc.flux"),
            Some(DnsLookup::Srv { port: 443, .. })
        ));
    }

    #[test]
    fn wire_response_is_authoritative_and_has_answer() {
        let answer = dns().answer_packet(&query("home.flux", TYPE_A)).unwrap();
        assert_eq!(&answer[0..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([answer[6], answer[7]]), 1);
        assert_eq!(
            &answer[answer.len() - 4..],
            &dns()
                .lookup("home.flux")
                .and_then(|a| match a {
                    DnsLookup::A(ip) => Some(ip.octets()),
                    _ => None,
                })
                .unwrap()
        );
    }

    #[test]
    fn unknown_name_returns_nxdomain() {
        let answer = dns().answer_packet(&query("missing.flux", TYPE_A)).unwrap();
        let flags = u16::from_be_bytes([answer[2], answer[3]]);
        assert_eq!(flags & 0x000f, 3);
    }
}
