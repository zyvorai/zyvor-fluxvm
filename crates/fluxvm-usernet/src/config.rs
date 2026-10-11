// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Gateway configuration and the egress policy.

use crate::wire::Mac;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::time::Instant;

/// An IPv4 CIDR block, `a.b.c.d/n` (a bare address is a /32).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let mask = if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix))
        };
        u32::from(ip) & mask == u32::from(self.addr) & mask
    }
}

impl FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, p.parse::<u8>().map_err(|e| format!("{s:?}: {e}"))?),
            None => (s, 32),
        };
        if p > 32 {
            return Err(format!("{s:?}: prefix > 32"));
        }
        Ok(Cidr {
            addr: a.parse().map_err(|e| format!("{s:?}: {e}"))?,
            prefix: p,
        })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl Serialize for Cidr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Addresses a guest must not reach unless explicitly allowed: loopback, RFC 1918, link-local, CGNAT, "this network",
/// benchmarking, reserved.
pub fn is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (o[1] & 0xf0) == 16)
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
}

/// Never a valid NAT destination, regardless of policy.
pub fn is_non_unicast(ip: Ipv4Addr) -> bool {
    ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip.octets()[0] >= 240
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
}

/// What the guest may reach. Evaluation order, first match wins:
/// 1. `deny_cidrs`, `deny_ports` -> deny.
/// 2. `allow_ports` non-empty and the port is not listed -> deny.
/// 3. The destination is explicitly allowed when it is in `allow_cidrs` or was learned from a DNS answer for an
///    `allow_domains` name.
/// 4. Not explicitly allowed and private/loopback/link-local (and `allow_private` unset) -> deny.
/// 5. Not explicitly allowed and any of `allow_cidrs` / `allow_domains` is configured (an allow-list) -> deny.
/// 6. Allow.
///
/// So the default policy is "the public internet, no private ranges"; adding `allow_cidrs` or `allow_domains` turns
/// it into a strict allow-list.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EgressPolicy {
    pub allow_cidrs: Vec<Cidr>,
    pub deny_cidrs: Vec<Cidr>,
    pub allow_ports: Vec<u16>,
    pub deny_ports: Vec<u16>,
    /// Domain suffixes (`example.com` matches itself and `*.example.com`). Names are resolved through the gateway's
    /// forwarder, which learns the answers as allowed destinations. Also gates DNS itself: with a non-empty list,
    /// other names get NXDOMAIN.
    pub allow_domains: Vec<String>,
    /// Names that always get NXDOMAIN.
    pub deny_domains: Vec<String>,
    /// Let the guest reach private ranges (not multicast/broadcast).
    pub allow_private: bool,
    /// Map the gateway address to the host's 127.0.0.1 (like slirp's 10.0.2.2). Off by default.
    pub allow_host_loopback: bool,
}

pub(crate) fn domain_matches(list: &[String], name: &str) -> bool {
    let n = name.trim_end_matches('.').to_ascii_lowercase();
    list.iter().any(|d| {
        let d = d.trim_start_matches("*.").trim_end_matches('.').to_ascii_lowercase();
        n == d || n.ends_with(&format!(".{d}"))
    })
}

impl EgressPolicy {
    pub fn is_allow_list(&self) -> bool {
        !self.allow_cidrs.is_empty() || !self.allow_domains.is_empty()
    }

    /// `learned` maps DNS-learned addresses to their expiry.
    pub fn check(
        &self,
        ip: Ipv4Addr,
        port: u16,
        learned: &HashMap<Ipv4Addr, Instant>,
        now: Instant,
    ) -> Decision {
        if is_non_unicast(ip) {
            return Decision::Deny("non-unicast");
        }
        if self.deny_cidrs.iter().any(|c| c.contains(ip)) {
            return Decision::Deny("deny-cidr");
        }
        if self.deny_ports.contains(&port) {
            return Decision::Deny("deny-port");
        }
        if !self.allow_ports.is_empty() && !self.allow_ports.contains(&port) {
            return Decision::Deny("port-not-allowed");
        }
        let explicit = self.allow_cidrs.iter().any(|c| c.contains(ip))
            || learned.get(&ip).is_some_and(|exp| *exp > now);
        if explicit {
            return Decision::Allow;
        }
        if is_private(ip) && !self.allow_private {
            return Decision::Deny("private-range");
        }
        if self.is_allow_list() {
            return Decision::Deny("not-in-allow-list");
        }
        Decision::Allow
    }

    /// Port rules only (for the gateway-to-host-loopback alias).
    pub fn check_ports(&self, port: u16) -> Decision {
        if self.deny_ports.contains(&port) {
            return Decision::Deny("deny-port");
        }
        if !self.allow_ports.is_empty() && !self.allow_ports.contains(&port) {
            return Decision::Deny("port-not-allowed");
        }
        Decision::Allow
    }

    /// May the guest resolve `name`?
    pub fn check_domain(&self, name: &str) -> Decision {
        if domain_matches(&self.deny_domains, name) {
            return Decision::Deny("deny-domain");
        }
        if !self.allow_domains.is_empty() && !domain_matches(&self.allow_domains, name) {
            return Decision::Deny("domain-not-allowed");
        }
        Decision::Allow
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

/// Publish a guest port on the host: `host_addr:host_port` -> guest `guest_port`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortForward {
    pub proto: Proto,
    #[serde(default = "default_host_addr")]
    pub host_addr: IpAddr,
    /// 0 picks an ephemeral port; see [`UserNet::forward_addrs`](crate::UserNet::forward_addrs).
    pub host_port: u16,
    pub guest_port: u16,
}

fn default_host_addr() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UserNetConfig {
    pub gateway_ip: Ipv4Addr,
    pub dns_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway_mac: Mac,
    /// Pin the guest's MAC; otherwise the first source MAC seen is learned and others are dropped.
    pub guest_mac: Option<Mac>,
    /// IP MTU (576..=9000). Advertised by DHCP option 26 and used for the TCP MSS.
    pub mtu: u16,
    pub dhcp_lease_secs: u32,
    /// Upstream resolvers. Empty: nameservers from /etc/resolv.conf, then 1.1.1.1.
    pub dns_upstreams: Vec<SocketAddr>,
    pub egress: EgressPolicy,
    pub forwards: Vec<PortForward>,
    pub max_tcp_flows: usize,
    pub max_udp_flows: usize,
    pub udp_idle_secs: u64,
    pub tcp_idle_secs: u64,
    pub connect_timeout_secs: u64,
}

impl Default for UserNetConfig {
    fn default() -> Self {
        Self {
            gateway_ip: Ipv4Addr::new(10, 0, 2, 2),
            dns_ip: Ipv4Addr::new(10, 0, 2, 3),
            guest_ip: Ipv4Addr::new(10, 0, 2, 15),
            prefix_len: 24,
            gateway_mac: [0x52, 0x54, 0x00, 0x12, 0x35, 0x02],
            guest_mac: None,
            mtu: 1500,
            dhcp_lease_secs: 86_400,
            dns_upstreams: Vec::new(),
            egress: EgressPolicy::default(),
            forwards: Vec::new(),
            max_tcp_flows: 4096,
            max_udp_flows: 1024,
            udp_idle_secs: 60,
            tcp_idle_secs: 30 * 60,
            connect_timeout_secs: 10,
        }
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

impl UserNetConfig {
    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - u32::from(self.prefix_len)))
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.guest_ip) | !u32::from(self.netmask()))
    }

    pub fn validate(&self) -> io::Result<()> {
        if !(8..=30).contains(&self.prefix_len) {
            return Err(bad("prefix_len must be 8..=30"));
        }
        if !(576..=9000).contains(&self.mtu) {
            return Err(bad("mtu must be 576..=9000"));
        }
        let subnet = Cidr {
            addr: self.guest_ip,
            prefix: self.prefix_len,
        };
        for (n, ip) in [("gateway_ip", self.gateway_ip), ("dns_ip", self.dns_ip)] {
            if !subnet.contains(ip) {
                return Err(bad(format!("{n} {ip} is outside {subnet}")));
            }
            if ip == self.guest_ip {
                return Err(bad(format!("{n} equals guest_ip")));
            }
        }
        for ip in [self.gateway_ip, self.dns_ip, self.guest_ip] {
            if ip == self.broadcast()
                || u32::from(ip) & !u32::from(self.netmask()) == 0
            {
                return Err(bad(format!("{ip} is the network or broadcast address")));
            }
        }
        if self.gateway_mac[0] & 1 != 0 || self.gateway_mac == [0; 6] {
            return Err(bad("gateway_mac must be a unicast address"));
        }
        let mut seen = std::collections::HashSet::new();
        for f in &self.forwards {
            if f.guest_port == 0 {
                return Err(bad("forward guest_port must be non-zero"));
            }
            if f.host_port != 0 && !seen.insert((f.proto, f.host_addr, f.host_port)) {
                return Err(bad(format!(
                    "duplicate forward {:?} {}:{}",
                    f.proto, f.host_addr, f.host_port
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol() -> EgressPolicy {
        EgressPolicy::default()
    }
    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }
    fn chk(p: &EgressPolicy, a: &str, port: u16) -> Decision {
        p.check(ip(a), port, &HashMap::new(), Instant::now())
    }

    #[test]
    fn default_denies_private_allows_public() {
        let p = pol();
        assert_eq!(chk(&p, "1.1.1.1", 443), Decision::Allow);
        for a in [
            "10.1.2.3",
            "127.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "172.31.255.1",
            "169.254.169.254",
            "100.64.0.1",
        ] {
            assert_eq!(chk(&p, a, 80), Decision::Deny("private-range"), "{a}");
        }
        assert_eq!(chk(&p, "172.32.0.1", 80), Decision::Allow);
        assert_eq!(chk(&p, "224.0.0.251", 5353), Decision::Deny("non-unicast"));
    }

    #[test]
    fn allow_list_and_ports() {
        let mut p = pol();
        p.allow_cidrs = vec!["10.9.0.0/16".parse().unwrap(), "8.8.8.8".parse().unwrap()];
        p.allow_ports = vec![53, 443];
        assert_eq!(chk(&p, "10.9.1.1", 443), Decision::Allow);
        assert_eq!(chk(&p, "8.8.8.8", 53), Decision::Allow);
        assert_eq!(chk(&p, "8.8.8.8", 22), Decision::Deny("port-not-allowed"));
        assert_eq!(chk(&p, "1.1.1.1", 443), Decision::Deny("not-in-allow-list"));
        p.deny_cidrs = vec!["10.9.5.0/24".parse().unwrap()];
        assert_eq!(chk(&p, "10.9.5.5", 443), Decision::Deny("deny-cidr"));
    }

    #[test]
    fn learned_addresses_and_domains() {
        let mut p = pol();
        p.allow_domains = vec!["example.com".into()];
        let now = Instant::now();
        let mut l = HashMap::new();
        assert_eq!(
            p.check(ip("93.184.216.34"), 443, &l, now),
            Decision::Deny("not-in-allow-list")
        );
        l.insert(ip("93.184.216.34"), now + std::time::Duration::from_secs(30));
        assert_eq!(p.check(ip("93.184.216.34"), 443, &l, now), Decision::Allow);
        assert_eq!(p.check_domain("www.Example.com."), Decision::Allow);
        assert_eq!(p.check_domain("evil.com"), Decision::Deny("domain-not-allowed"));
        assert_eq!(p.check_domain("notexample.com"), Decision::Deny("domain-not-allowed"));
    }

    #[test]
    fn config_validation_and_cidr_json() {
        assert!(UserNetConfig::default().validate().is_ok());
        let mut c = UserNetConfig::default();
        c.gateway_ip = ip("10.0.3.2");
        assert!(c.validate().is_err());
        let c: UserNetConfig = serde_json::from_str(
            r#"{"mtu":1400,"egress":{"allow_cidrs":["10.0.0.0/8"],"allow_ports":[443]}}"#,
        )
        .unwrap();
        assert_eq!(c.mtu, 1400);
        assert!(c.egress.allow_cidrs[0].contains(ip("10.2.3.4")));
        assert!("1.2.3.4/33".parse::<Cidr>().is_err());
    }
}
