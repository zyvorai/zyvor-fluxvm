// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Home/SMB router automation: PCP, NAT-PMP, then UPnP IGD fallback.

use crate::public::{PortMapping, RouterConfig, RouterProtocol};
use crate::{MeshError, Result};
use ring::rand::{SecureRandom, SystemRandom};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingResult {
    pub name: String,
    pub protocol: RouterProtocol,
    pub external_port: u16,
    pub lifetime_seconds: u32,
}

pub async fn reconcile(config: &RouterConfig) -> Result<Vec<MappingResult>> {
    let mut out = Vec::new();
    for mapping in &config.mappings {
        let result = match config.protocol {
            RouterProtocol::Pcp => map_pcp(config.gateway, mapping)?,
            RouterProtocol::NatPmp => map_nat_pmp(config.gateway, mapping)?,
            RouterProtocol::Upnp => map_upnp(mapping).await?,
            RouterProtocol::Auto => {
                match map_pcp(config.gateway, mapping) {
                    Ok(v) => v,
                    Err(pcp_err) => match map_nat_pmp(config.gateway, mapping) {
                        Ok(v) => v,
                        Err(pmp_err) => map_upnp(mapping).await.map_err(|upnp_err| {
                            MeshError::InvalidState(format!(
                                "router mapping {} failed: PCP={pcp_err}; NAT-PMP={pmp_err}; UPnP={upnp_err}", mapping.name
                            ))
                        })?,
                    },
                }
            }
        };
        out.push(result);
    }
    Ok(out)
}

fn map_pcp(gateway: IpAddr, mapping: &PortMapping) -> Result<MappingResult> {
    let protocol = ip_protocol(&mapping.protocol)?;
    let gateway = SocketAddr::new(gateway, 5351);
    let bind = match gateway.ip() {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(gateway)?;
    let local = socket.local_addr()?.ip();
    let mut req = vec![0u8; 60];
    req[0] = 2; // PCP version
    req[1] = 1; // MAP opcode
    req[4..8].copy_from_slice(&mapping.lifetime_seconds.to_be_bytes());
    req[8..24].copy_from_slice(&pcp_ip(local));
    SystemRandom::new()
        .fill(&mut req[24..36])
        .map_err(|_| MeshError::InvalidState("PCP nonce generation failed".into()))?;
    req[36] = protocol;
    req[40..42].copy_from_slice(&mapping.internal_port.to_be_bytes());
    req[42..44].copy_from_slice(&mapping.external_port.to_be_bytes());
    socket.send(&req)?;
    let mut response = [0u8; 128];
    let n = socket.recv(&mut response)?;
    if n < 60 || response[0] != 2 || response[1] != 0x81 {
        return Err(MeshError::InvalidState("invalid PCP MAP response".into()));
    }
    if response[3] != 0 {
        return Err(MeshError::InvalidState(format!(
            "PCP MAP failed with result {}",
            response[3]
        )));
    }
    if response[24..36] != req[24..36] {
        return Err(MeshError::InvalidState("PCP MAP nonce mismatch".into()));
    }
    let lifetime = u32::from_be_bytes(response[4..8].try_into().unwrap());
    let external_port = u16::from_be_bytes(response[42..44].try_into().unwrap());
    Ok(MappingResult {
        name: mapping.name.clone(),
        protocol: RouterProtocol::Pcp,
        external_port,
        lifetime_seconds: lifetime,
    })
}

fn map_nat_pmp(gateway: IpAddr, mapping: &PortMapping) -> Result<MappingResult> {
    let gateway = match gateway {
        IpAddr::V4(ip) => SocketAddr::new(IpAddr::V4(ip), 5351),
        IpAddr::V6(_) => {
            return Err(MeshError::InvalidState(
                "NAT-PMP requires an IPv4 gateway".into(),
            ));
        }
    };
    let opcode = match mapping.protocol.to_ascii_lowercase().as_str() {
        "udp" => 1u8,
        "tcp" => 2u8,
        other => {
            return Err(MeshError::InvalidState(format!(
                "unsupported NAT-PMP protocol {other}"
            )));
        }
    };
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(gateway)?;
    let mut req = [0u8; 12];
    req[0] = 0;
    req[1] = opcode;
    req[4..6].copy_from_slice(&mapping.internal_port.to_be_bytes());
    req[6..8].copy_from_slice(&mapping.external_port.to_be_bytes());
    req[8..12].copy_from_slice(&mapping.lifetime_seconds.to_be_bytes());
    socket.send(&req)?;
    let mut response = [0u8; 32];
    let n = socket.recv(&mut response)?;
    if n < 16 || response[0] != 0 || response[1] != opcode + 128 {
        return Err(MeshError::InvalidState(
            "invalid NAT-PMP mapping response".into(),
        ));
    }
    let result = u16::from_be_bytes(response[2..4].try_into().unwrap());
    if result != 0 {
        return Err(MeshError::InvalidState(format!(
            "NAT-PMP failed with result {result}"
        )));
    }
    let external_port = u16::from_be_bytes(response[10..12].try_into().unwrap());
    let lifetime = u32::from_be_bytes(response[12..16].try_into().unwrap());
    Ok(MappingResult {
        name: mapping.name.clone(),
        protocol: RouterProtocol::NatPmp,
        external_port,
        lifetime_seconds: lifetime,
    })
}

async fn map_upnp(mapping: &PortMapping) -> Result<MappingResult> {
    let location = upnp_discover()?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(http_err)?;
    let xml = http
        .get(&location)
        .send()
        .await
        .map_err(http_err)?
        .error_for_status()
        .map_err(http_err)?
        .text()
        .await
        .map_err(http_err)?;
    let (service_type, control_url) = find_wan_control(&xml).ok_or_else(|| {
        MeshError::InvalidState(
            "UPnP device has no WANIPConnection/WANPPPConnection service".into(),
        )
    })?;
    let base = reqwest::Url::parse(&location)
        .map_err(|e| MeshError::InvalidState(format!("UPnP location URL: {e}")))?;
    let control = base
        .join(&control_url)
        .map_err(|e| MeshError::InvalidState(format!("UPnP control URL: {e}")))?;
    let local_ip = local_ipv4_for(
        base.host_str().unwrap_or("192.0.2.1"),
        base.port_or_known_default().unwrap_or(80),
    )?;
    let proto = mapping.protocol.to_ascii_uppercase();
    if proto != "TCP" && proto != "UDP" {
        return Err(MeshError::InvalidState(format!(
            "unsupported UPnP protocol {proto}"
        )));
    }
    let body = format!(
        r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:AddPortMapping xmlns:u="{service_type}"><NewRemoteHost></NewRemoteHost><NewExternalPort>{}</NewExternalPort><NewProtocol>{proto}</NewProtocol><NewInternalPort>{}</NewInternalPort><NewInternalClient>{local_ip}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>FluxMesh {}</NewPortMappingDescription><NewLeaseDuration>{}</NewLeaseDuration></u:AddPortMapping></s:Body></s:Envelope>"#,
        mapping.external_port,
        mapping.internal_port,
        xml_escape(&mapping.name),
        mapping.lifetime_seconds
    );
    let response = http
        .post(control)
        .header("SOAPAction", format!("\"{service_type}#AddPortMapping\""))
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .body(body)
        .send()
        .await
        .map_err(http_err)?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(MeshError::InvalidState(format!(
            "UPnP AddPortMapping returned {status}: {}",
            detail.chars().take(1024).collect::<String>()
        )));
    }
    Ok(MappingResult {
        name: mapping.name.clone(),
        protocol: RouterProtocol::Upnp,
        external_port: mapping.external_port,
        lifetime_seconds: mapping.lifetime_seconds,
    })
}

fn upnp_discover() -> Result<String> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";
    socket.send_to(request, "239.255.255.250:1900")?;
    let mut buf = [0u8; 8192];
    let (n, _) = socket.recv_from(&mut buf)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("location")
                .then(|| value.trim().to_owned())
        })
        .ok_or_else(|| MeshError::InvalidState("UPnP discovery response had no LOCATION".into()))
}

fn find_wan_control(xml: &str) -> Option<(String, String)> {
    for service in [
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
    ] {
        let Some(pos) = xml.find(service) else {
            continue;
        };
        let tail = &xml[pos..];
        let Some(start0) = tail.find("<controlURL>") else {
            continue;
        };
        let start = start0 + "<controlURL>".len();
        let Some(end0) = tail[start..].find("</controlURL>") else {
            continue;
        };
        let end = end0 + start;
        return Some((service.into(), tail[start..end].trim().into()));
    }
    None
}

fn local_ipv4_for(host: &str, port: u16) -> Result<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect((host, port))?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        _ => Err(MeshError::InvalidState(
            "UPnP route did not select an IPv4 source".into(),
        )),
    }
}

fn pcp_ip(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V6(v6) => v6.octets(),
        IpAddr::V4(v4) => {
            let mut out = [0u8; 16];
            out[10] = 0xff;
            out[11] = 0xff;
            out[12..].copy_from_slice(&v4.octets());
            out
        }
    }
}
fn ip_protocol(value: &str) -> Result<u8> {
    match value.to_ascii_lowercase().as_str() {
        "tcp" => Ok(6),
        "udp" => Ok(17),
        other => Err(MeshError::InvalidState(format!(
            "unsupported port mapping protocol {other}"
        ))),
    }
}
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn http_err(e: reqwest::Error) -> MeshError {
    MeshError::InvalidState(format!("UPnP HTTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ipv4_is_encoded_as_pcp_v4_mapped() {
        let got = pcp_ip("192.0.2.9".parse().unwrap());
        assert_eq!(&got[..12], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255]);
        assert_eq!(&got[12..], &[192, 0, 2, 9]);
    }
    #[test]
    fn extracts_upnp_control_url() {
        let xml = "x urn:schemas-upnp-org:service:WANIPConnection:1 y <controlURL>/ctl/IPConn</controlURL>";
        assert_eq!(find_wan_control(xml).unwrap().1, "/ctl/IPConn");
    }
}
