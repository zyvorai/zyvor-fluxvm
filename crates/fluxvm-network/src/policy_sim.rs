// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Offline "what-if" evaluation of a [`VmNetworkPolicy`].
//!
//! Answers "would this connection be allowed, and which rule decides?" and "which connections change if I replace
//! this policy with that one?" without a VM, root or eBPF. The decision order mirrors the TC program
//! (`bpf/fluxvm_tc.bpf.c`) for a **new flow** with the stored (group-merged) policy:
//!
//! 1. `deny_udp` drops UDP and SCTP.
//! 2. `deny_cidrs` drops (always wins).
//! 3. IPv4 only: `allow_icmp` lets ICMP through before any allow list is consulted.
//! 4. No allow list (`allow_cidrs` and `allow_ports` both empty): `default_allow` decides.
//! 5. Otherwise the destination must match `allow_cidrs` (when set) and `proto/port` must match `allow_ports`
//!    (when set).
//! 6. IPv6 only: `allow_icmp` lets ICMPv6 through, but only after step 4 did not deny (the kernel program differs from
//!    IPv4 here; this module follows it).
//! 7. `audit_mode` turns every drop into a forward and records that it would have dropped.
//!
//! Not modelled: conntrack (established flows skip the checks), rate ceilings, the Pod policy, the L7 edge contract,
//! DHCP/NDP exemptions, FQDN resolution (names become `allow_cidrs` entries when the policy is applied) and TCP
//! flag handling. [`simulate`] reports what it cannot judge in [`Decision::caveats`].

use crate::dataplane::VmNetworkPolicy;
use serde::Serialize;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Layer-4 protocol of a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
    Sctp,
    Icmp,
    Icmp6,
}

impl Proto {
    fn parse(raw: &str) -> Result<Proto, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tcp" => Ok(Proto::Tcp),
            "udp" => Ok(Proto::Udp),
            "sctp" => Ok(Proto::Sctp),
            "icmp" => Ok(Proto::Icmp),
            "icmp6" | "icmpv6" => Ok(Proto::Icmp6),
            other => Err(format!(
                "unsupported protocol {other:?}; use tcp, udp, sctp, icmp or icmp6"
            )),
        }
    }

    fn is_icmp(self) -> bool {
        matches!(self, Proto::Icmp | Proto::Icmp6)
    }
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Sctp => "sctp",
            Proto::Icmp => "icmp",
            Proto::Icmp6 => "icmp6",
        })
    }
}

/// One hypothetical outbound connection from the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub proto: Proto,
    pub dst: IpAddr,
    /// Destination port; 0 for ICMP.
    pub port: u16,
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.proto.is_icmp(), self.dst) {
            (true, dst) => write!(f, "{}://{}", self.proto, dst),
            (false, IpAddr::V4(a)) => write!(f, "{}://{}:{}", self.proto, a, self.port),
            (false, IpAddr::V6(a)) => write!(f, "{}://[{}]:{}", self.proto, a, self.port),
        }
    }
}

impl FromStr for Probe {
    type Err = String;

    /// `tcp://10.0.0.5:443`, `udp://[2001:db8::1]:53`, `icmp://10.0.0.5`. Without `://` the protocol is `tcp`.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (proto, rest) = match s.split_once("://") {
            Some((p, r)) => (Proto::parse(p)?, r),
            None => (Proto::Tcp, s),
        };
        if proto.is_icmp() {
            let ip: IpAddr = rest
                .trim_matches(['[', ']'])
                .parse()
                .map_err(|_| format!("invalid address in {s:?}"))?;
            let proto = match (proto, ip) {
                (Proto::Icmp, IpAddr::V6(_)) => {
                    return Err(format!("{s:?}: use icmp6 for an IPv6 address"));
                }
                (Proto::Icmp6, IpAddr::V4(_)) => {
                    return Err(format!("{s:?}: use icmp for an IPv4 address"));
                }
                (p, _) => p,
            };
            return Ok(Probe {
                proto,
                dst: ip,
                port: 0,
            });
        }
        let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
            let (h, p) = v6
                .split_once("]:")
                .ok_or_else(|| format!("{s:?}: expected [addr]:port"))?;
            (h, p)
        } else {
            let (h, p) = rest
                .rsplit_once(':')
                .ok_or_else(|| format!("{s:?}: expected addr:port"))?;
            if h.contains(':') {
                return Err(format!("{s:?}: write an IPv6 address as [addr]:port"));
            }
            (h, p)
        };
        let dst: IpAddr = host
            .parse()
            .map_err(|_| format!("invalid address {host:?} in {s:?}"))?;
        let port: u16 = port
            .parse()
            .map_err(|_| format!("invalid port {port:?} in {s:?}"))?;
        if port == 0 {
            return Err(format!("{s:?}: port must be 1..65535"));
        }
        Ok(Probe { proto, dst, port })
    }
}

/// What the enforcer does with the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Drop,
}

/// The step of the decision order that settled the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    UdpKillSwitch,
    ExplicitCidrDeny,
    IcmpAllowed,
    DefaultDeny,
    DefaultAllow,
    CidrMiss,
    L4Miss,
    AllowListMatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    pub probe: Probe,
    pub verdict: Verdict,
    pub stage: Stage,
    /// One sentence naming the rule that decided.
    pub reason: String,
    /// `audit_mode` forwarded a packet the enforcer would have dropped.
    pub audit_forwarded: bool,
    /// Things this module cannot judge for this probe.
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(raw: &str) -> Result<Cidr, String> {
        let (ip, prefix) = raw
            .trim()
            .split_once('/')
            .ok_or_else(|| format!("CIDR {raw:?} must include /prefix"))?;
        let addr: IpAddr = ip
            .parse()
            .map_err(|_| format!("invalid IP {ip:?} in {raw:?}"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| format!("invalid prefix in {raw:?}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!(
                "prefix {prefix} in {raw:?} exceeds {max} for an IPv{} address",
                if addr.is_ipv4() { 4 } else { 6 }
            ));
        }
        Ok(Cidr { addr, prefix })
    }

    fn bits(addr: IpAddr) -> u128 {
        match addr {
            IpAddr::V4(a) => u128::from(u32::from(a)),
            IpAddr::V6(a) => u128::from_be_bytes(a.octets()),
        }
    }

    fn width(&self) -> u32 {
        if self.addr.is_ipv4() { 32 } else { 128 }
    }

    fn host_mask(&self) -> u128 {
        let host = self.width() - u32::from(self.prefix);
        if host >= 128 {
            u128::MAX
        } else {
            (1u128 << host) - 1
        }
    }

    fn mask(&self) -> u128 {
        (u128::MAX >> (128 - self.width())) & !self.host_mask()
    }

    fn network(&self) -> u128 {
        Self::bits(self.addr) & self.mask()
    }

    fn contains(&self, ip: IpAddr) -> bool {
        self.addr.is_ipv4() == ip.is_ipv4() && Self::bits(ip) & self.mask() == self.network()
    }

    /// `self` covers every address of `inner`.
    fn covers(&self, inner: &Cidr) -> bool {
        self.addr.is_ipv4() == inner.addr.is_ipv4()
            && self.prefix <= inner.prefix
            && inner.network() & self.mask() == self.network()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PortRule {
    proto: Proto,
    port: u16,
}

fn parse_port_rule(raw: &str) -> Result<PortRule, String> {
    let (proto, port) = raw
        .split_once('/')
        .ok_or_else(|| format!("port rule {raw:?} must be proto/PORT"))?;
    let proto = Proto::parse(proto)?;
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| format!("invalid port in {raw:?}"))?;
    if port == 0 && !proto.is_icmp() {
        return Err(format!("port rule {raw:?}: port must be 1..65535"));
    }
    Ok(PortRule { proto, port })
}

struct Parsed {
    allow: Vec<Cidr>,
    deny: Vec<Cidr>,
    ports: Vec<PortRule>,
}

fn parse_policy(p: &VmNetworkPolicy) -> Result<Parsed, Vec<String>> {
    let mut errs = Vec::new();
    let mut cidrs = |list: &[String], field: &str| -> Vec<Cidr> {
        list.iter()
            .filter_map(|c| match Cidr::parse(c) {
                Ok(c) => Some(c),
                Err(e) => {
                    errs.push(format!("{field}: {e}"));
                    None
                }
            })
            .collect()
    };
    let allow = cidrs(&p.allow_cidrs, "allow_cidrs");
    let deny = cidrs(&p.deny_cidrs, "deny_cidrs");
    let ports = p
        .allow_ports
        .iter()
        .filter_map(|r| match parse_port_rule(r) {
            Ok(r) => Some(r),
            Err(e) => {
                errs.push(format!("allow_ports: {e}"));
                None
            }
        })
        .collect();
    if errs.is_empty() {
        Ok(Parsed { allow, deny, ports })
    } else {
        Err(errs)
    }
}

/// Evaluates `probe` against `policy`. Errors list every malformed entry (the real apply step would refuse the policy).
pub fn simulate(policy: &VmNetworkPolicy, probe: &Probe) -> Result<Decision, Vec<String>> {
    let parsed = parse_policy(policy)?;
    Ok(decide(policy, &parsed, probe))
}

fn decide(policy: &VmNetworkPolicy, parsed: &Parsed, probe: &Probe) -> Decision {
    let (stage, allowed, reason) = raw_decision(policy, parsed, probe);
    let audit = !allowed && policy.audit_mode;
    let mut caveats = Vec::new();
    if !policy.allow_fqdns.is_empty() {
        caveats.push(format!(
            "{} allow_fqdns entries are resolved into allow_cidrs when the policy is applied; names are not resolved here",
            policy.allow_fqdns.len()
        ));
    }
    if policy.max_egress_mbps.is_some() || policy.max_egress_pps.is_some() {
        caveats.push("an egress rate ceiling is set; it can still drop an allowed flow".into());
    }
    Decision {
        probe: *probe,
        verdict: if allowed || audit {
            Verdict::Allow
        } else {
            Verdict::Drop
        },
        stage,
        reason: if audit {
            format!("{reason}; audit_mode forwards it (would be dropped)")
        } else {
            reason
        },
        audit_forwarded: audit,
        caveats,
    }
}

fn raw_decision(policy: &VmNetworkPolicy, p: &Parsed, probe: &Probe) -> (Stage, bool, String) {
    let v6 = probe.dst.is_ipv6();
    if policy.deny_udp && matches!(probe.proto, Proto::Udp | Proto::Sctp) {
        return (
            Stage::UdpKillSwitch,
            false,
            format!("deny_udp is set and {} is UDP/SCTP", probe.proto),
        );
    }
    if let Some(c) = p.deny.iter().find(|c| c.contains(probe.dst)) {
        return (
            Stage::ExplicitCidrDeny,
            false,
            format!("destination is inside deny_cidrs entry {}", fmt_cidr(c)),
        );
    }
    if !v6 && policy.allow_icmp && probe.proto == Proto::Icmp {
        return (
            Stage::IcmpAllowed,
            true,
            "allow_icmp lets IPv4 ICMP through before the allow lists".into(),
        );
    }
    let enforce_cidr = !p.allow.is_empty();
    let enforce_l4 = !p.ports.is_empty();
    let has_policy = enforce_cidr || enforce_l4;
    if !has_policy && !policy.default_allow {
        return (
            Stage::DefaultDeny,
            false,
            "no allow list and default_allow is false".into(),
        );
    }
    if v6 && policy.allow_icmp && probe.proto == Proto::Icmp6 {
        return (
            Stage::IcmpAllowed,
            true,
            "allow_icmp lets ICMPv6 through".into(),
        );
    }
    if !has_policy {
        return (
            Stage::DefaultAllow,
            true,
            "no allow list is configured and default_allow is true".into(),
        );
    }
    if enforce_cidr && !p.allow.iter().any(|c| c.contains(probe.dst)) {
        return (
            Stage::CidrMiss,
            false,
            "destination matches no allow_cidrs entry".into(),
        );
    }
    if enforce_l4
        && !p
            .ports
            .iter()
            .any(|r| r.proto == probe.proto && r.port == probe.port)
    {
        return (
            Stage::L4Miss,
            false,
            format!(
                "{}/{} matches no allow_ports entry",
                probe.proto, probe.port
            ),
        );
    }
    let mut parts = Vec::new();
    if enforce_cidr {
        parts.push("allow_cidrs");
    }
    if enforce_l4 {
        parts.push("allow_ports");
    }
    (
        Stage::AllowListMatch,
        true,
        format!("matches {}", parts.join(" and ")),
    )
}

fn fmt_cidr(c: &Cidr) -> String {
    format!("{}/{}", c.addr, c.prefix)
}

/// Verdict of one probe under two policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub probe: Probe,
    pub before: Decision,
    pub after: Decision,
    /// `before.verdict != after.verdict`.
    pub flipped: bool,
}

/// Evaluates every probe under both policies. Only probes whose verdict or deciding stage differs are returned.
pub fn compare(
    before: &VmNetworkPolicy,
    after: &VmNetworkPolicy,
    probes: &[Probe],
) -> Result<Vec<Change>, Vec<String>> {
    let b = parse_policy(before).map_err(|e| prefix_all("current policy", e))?;
    let a = parse_policy(after).map_err(|e| prefix_all("new policy", e))?;
    Ok(probes
        .iter()
        .filter_map(|probe| {
            let db = decide(before, &b, probe);
            let da = decide(after, &a, probe);
            (db.verdict != da.verdict || db.stage != da.stage).then(|| Change {
                probe: *probe,
                flipped: db.verdict != da.verdict,
                before: db,
                after: da,
            })
        })
        .collect())
}

fn prefix_all(who: &str, errs: Vec<String>) -> Vec<String> {
    errs.into_iter().map(|e| format!("{who}: {e}")).collect()
}

/// Probes that sit on the edges of what either policy mentions: the first and last address of every CIDR in
/// `policies`, each tried with every allowed `proto/port` (or tcp/443 and udp/53 when none is listed), an ICMP echo
/// to each, and a few fixed public and private destinations. Duplicates removed; order stable.
pub fn derive_probes(policies: &[&VmNetworkPolicy]) -> Vec<Probe> {
    let mut protoports: Vec<(Proto, u16)> = Vec::new();
    let mut addrs: Vec<IpAddr> = vec![
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
    ];
    for p in policies {
        for raw in &p.allow_ports {
            if let Ok(r) = parse_port_rule(raw)
                && !r.proto.is_icmp()
            {
                protoports.push((r.proto, r.port));
            }
        }
        for raw in p.allow_cidrs.iter().chain(p.deny_cidrs.iter()) {
            if let Ok(c) = Cidr::parse(raw) {
                let first = c.network();
                let last = first | c.host_mask();
                for bits in [first, last] {
                    addrs.push(if c.addr.is_ipv4() {
                        IpAddr::V4(Ipv4Addr::from(bits as u32))
                    } else {
                        IpAddr::V6(Ipv6Addr::from(bits.to_be_bytes()))
                    });
                }
            }
        }
    }
    if protoports.is_empty() {
        protoports = vec![(Proto::Tcp, 443), (Proto::Udp, 53)];
    }
    let mut out: Vec<Probe> = Vec::new();
    let mut push = |p: Probe| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for a in addrs {
        for (proto, port) in &protoports {
            push(Probe {
                proto: *proto,
                dst: a,
                port: *port,
            });
        }
        push(Probe {
            proto: if a.is_ipv4() {
                Proto::Icmp
            } else {
                Proto::Icmp6
            },
            dst: a,
            port: 0,
        });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    pub field: &'static str,
    pub message: String,
}

/// Static lint of a policy: malformed entries (errors), rules that can never matter and settings that do not do
/// what they appear to (warnings), and how an unusual combination behaves (info). Pure; no host access.
pub fn lint(policy: &VmNetworkPolicy) -> Vec<Finding> {
    let mut out = Vec::new();
    let f = |sev, field, message: String| Finding {
        severity: sev,
        field,
        message,
    };
    let parsed = match parse_policy(policy) {
        Ok(p) => p,
        Err(errs) => {
            return errs
                .into_iter()
                .map(|e| {
                    let field = if e.starts_with("deny_cidrs") {
                        "deny_cidrs"
                    } else if e.starts_with("allow_ports") {
                        "allow_ports"
                    } else {
                        "allow_cidrs"
                    };
                    f(Severity::Error, field, e)
                })
                .collect();
        }
    };
    for a in &parsed.allow {
        if let Some(d) = parsed.deny.iter().find(|d| d.covers(a)) {
            out.push(f(
                Severity::Warning,
                "allow_cidrs",
                format!(
                    "{} is entirely inside deny_cidrs entry {}; deny always wins, so it never matches",
                    fmt_cidr(a),
                    fmt_cidr(d)
                ),
            ));
        }
    }
    let has_list = !parsed.allow.is_empty() || !parsed.ports.is_empty();
    if has_list && policy.default_allow {
        out.push(f(
            Severity::Info,
            "default_allow",
            "default_allow = true is ignored once an allow list exists: unmatched traffic is dropped".into(),
        ));
    }
    if !has_list && !policy.default_allow {
        out.push(f(
            Severity::Info,
            "default_allow",
            "no allow list and default_allow = false: the VM can send nothing (except DHCP/NDP)"
                .into(),
        ));
    }
    if policy.deny_udp {
        for r in policy.allow_ports.iter() {
            if let Ok(pr) = parse_port_rule(r)
                && matches!(pr.proto, Proto::Udp | Proto::Sctp)
            {
                out.push(f(
                    Severity::Warning,
                    "allow_ports",
                    format!("{r} can never match: deny_udp drops all UDP and SCTP first"),
                ));
            }
        }
    }
    if policy.audit_mode {
        out.push(f(
            Severity::Warning,
            "audit_mode",
            "audit_mode is on: nothing is dropped, violations are only logged".into(),
        ));
    }
    if policy.allow_icmp && !parsed.allow.is_empty() {
        out.push(f(
            Severity::Info,
            "allow_icmp",
            "allow_icmp lets IPv4 ICMP reach any destination not in deny_cidrs, even outside allow_cidrs".into(),
        ));
    }
    if policy.allow_icmp && !has_list && !policy.default_allow {
        out.push(f(
            Severity::Warning,
            "allow_icmp",
            "IPv4 ICMP is allowed but ICMPv6 is still dropped: with no allow list and default_allow = false the IPv6 path denies first".into(),
        ));
    }
    for name in &policy.allow_fqdns {
        if name.contains('*') {
            out.push(f(
                Severity::Warning,
                "allow_fqdns",
                format!("{name}: wildcard names are skipped when the policy is applied"),
            ));
        }
    }
    if !policy.allow_fqdns.is_empty() && parsed.allow.is_empty() {
        out.push(f(
            Severity::Info,
            "allow_fqdns",
            "FQDNs are resolved to allow_cidrs once, at apply time; re-apply or refresh DNS when the addresses change".into(),
        ));
    }
    let v6 = parsed
        .allow
        .iter()
        .chain(parsed.deny.iter())
        .any(|c| c.addr.is_ipv6());
    if v6 {
        out.push(f(
            Severity::Info,
            "allow_cidrs",
            "IPv6 entries need the native eBPF dataplane; the legacy nftables fallback refuses mixed-family policy".into(),
        ));
    }
    for (field, list) in [
        ("allow_cidrs", &policy.allow_cidrs),
        ("deny_cidrs", &policy.deny_cidrs),
        ("allow_ports", &policy.allow_ports),
    ] {
        let mut seen = std::collections::HashSet::new();
        for e in list.iter() {
            if !seen.insert(e.trim().to_ascii_lowercase()) {
                out.push(f(
                    Severity::Warning,
                    field,
                    format!("{e} is listed more than once"),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(f: impl FnOnce(&mut VmNetworkPolicy)) -> VmNetworkPolicy {
        let mut p = VmNetworkPolicy::default();
        f(&mut p);
        p
    }
    fn probe(s: &str) -> Probe {
        s.parse().unwrap()
    }
    fn run(p: &VmNetworkPolicy, s: &str) -> Decision {
        simulate(p, &probe(s)).unwrap()
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn probe_parsing() {
        assert_eq!(probe("10.0.0.5:443").proto, Proto::Tcp);
        assert_eq!(probe("udp://[2001:db8::1]:53").port, 53);
        assert_eq!(probe("icmp://10.0.0.5").port, 0);
        assert_eq!(probe("icmp6://[::1]").proto, Proto::Icmp6);
        assert!("icmp://::1".parse::<Probe>().is_err());
        assert!("icmp6://1.2.3.4".parse::<Probe>().is_err());
        assert!("tcp://1.2.3.4".parse::<Probe>().is_err());
        assert!("tcp://1.2.3.4:0".parse::<Probe>().is_err());
        assert!("tcp://1.2.3.4:70000".parse::<Probe>().is_err());
        assert!("gre://1.2.3.4:1".parse::<Probe>().is_err());
        assert!("tcp://[::1]:80".parse::<Probe>().is_ok());
        assert!("tcp://::1:80".parse::<Probe>().is_err());
        assert_eq!(
            probe("udp://[2001:db8::1]:53").to_string(),
            "udp://[2001:db8::1]:53"
        );
    }

    #[test]
    fn default_policy_allows_everything() {
        let p = VmNetworkPolicy::default();
        let d = run(&p, "tcp://8.8.8.8:443");
        assert_eq!((d.verdict, d.stage), (Verdict::Allow, Stage::DefaultAllow));
    }

    #[test]
    fn default_deny_without_lists() {
        let p = pol(|p| p.default_allow = false);
        let d = run(&p, "tcp://8.8.8.8:443");
        assert_eq!((d.verdict, d.stage), (Verdict::Drop, Stage::DefaultDeny));
    }

    #[test]
    fn cidr_and_port_dimensions_are_anded() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.0.0.0/8"]);
            p.allow_ports = s(&["tcp/443"]);
        });
        assert_eq!(run(&p, "tcp://10.1.2.3:443").stage, Stage::AllowListMatch);
        assert_eq!(run(&p, "tcp://10.1.2.3:80").stage, Stage::L4Miss);
        assert_eq!(run(&p, "udp://10.1.2.3:443").stage, Stage::L4Miss);
        assert_eq!(run(&p, "tcp://8.8.8.8:443").stage, Stage::CidrMiss);
        assert_eq!(run(&p, "tcp://8.8.8.8:443").verdict, Verdict::Drop);
    }

    #[test]
    fn allow_list_ignores_default_allow() {
        let p = pol(|p| p.allow_ports = s(&["tcp/443"]));
        assert_eq!(p.default_allow, true);
        assert_eq!(run(&p, "tcp://1.1.1.1:80").verdict, Verdict::Drop);
        assert_eq!(run(&p, "tcp://1.1.1.1:443").verdict, Verdict::Allow);
    }

    #[test]
    fn deny_always_wins() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.0.0.0/8"]);
            p.deny_cidrs = s(&["10.9.0.0/16"]);
            p.allow_icmp = true;
        });
        assert_eq!(run(&p, "tcp://10.1.0.1:80").verdict, Verdict::Allow);
        let d = run(&p, "tcp://10.9.4.4:80");
        assert_eq!(
            (d.verdict, d.stage),
            (Verdict::Drop, Stage::ExplicitCidrDeny)
        );
        assert!(d.reason.contains("10.9.0.0/16"));
        assert_eq!(run(&p, "icmp://10.9.4.4").verdict, Verdict::Drop);
    }

    #[test]
    fn deny_only_policy_with_default_allow_true_allows_the_rest() {
        let p = pol(|p| p.deny_cidrs = s(&["192.168.0.0/16"]));
        assert_eq!(run(&p, "tcp://192.168.1.1:22").verdict, Verdict::Drop);
        assert_eq!(run(&p, "tcp://8.8.8.8:22").verdict, Verdict::Allow);
    }

    #[test]
    fn icmp_v4_bypasses_allow_lists_but_not_deny() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.0.0.0/8"]);
            p.allow_icmp = true;
        });
        let d = run(&p, "icmp://8.8.8.8");
        assert_eq!((d.verdict, d.stage), (Verdict::Allow, Stage::IcmpAllowed));
        let q = pol(|p| p.allow_cidrs = s(&["10.0.0.0/8"]));
        assert_eq!(run(&q, "icmp://10.0.0.1").stage, Stage::AllowListMatch);
    }

    #[test]
    fn icmp_v6_is_denied_first_when_default_deny() {
        let p = pol(|p| {
            p.default_allow = false;
            p.allow_icmp = true;
        });
        assert_eq!(run(&p, "icmp://10.0.0.1").verdict, Verdict::Allow);
        assert_eq!(run(&p, "icmp6://[2001:db8::1]").verdict, Verdict::Drop);
        assert!(
            lint(&p)
                .iter()
                .any(|f| f.field == "allow_icmp" && f.severity == Severity::Warning)
        );
    }

    #[test]
    fn icmp_v6_allowed_past_cidr_list_when_allow_icmp() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["2001:db8::/32"]);
            p.allow_icmp = true;
        });
        assert_eq!(run(&p, "icmp6://[2600::1]").verdict, Verdict::Allow);
    }

    #[test]
    fn family_never_crosses() {
        let p = pol(|p| p.allow_cidrs = s(&["0.0.0.0/0"]));
        assert_eq!(run(&p, "tcp://[2001:db8::1]:443").stage, Stage::CidrMiss);
        let q = pol(|p| p.allow_cidrs = s(&["::/0"]));
        assert_eq!(run(&q, "tcp://8.8.8.8:443").stage, Stage::CidrMiss);
        assert_eq!(run(&q, "tcp://[2001:db8::1]:443").verdict, Verdict::Allow);
    }

    #[test]
    fn prefix_edges() {
        let p = pol(|p| p.allow_cidrs = s(&["10.1.2.3/32", "172.16.0.0/12"]));
        assert_eq!(run(&p, "tcp://10.1.2.3:1").verdict, Verdict::Allow);
        assert_eq!(run(&p, "tcp://10.1.2.4:1").verdict, Verdict::Drop);
        assert_eq!(run(&p, "tcp://172.31.255.255:1").verdict, Verdict::Allow);
        assert_eq!(run(&p, "tcp://172.32.0.0:1").verdict, Verdict::Drop);
        // host bits in the CIDR are masked, like the dataplane does
        let q = pol(|p| p.allow_cidrs = s(&["10.1.2.99/24"]));
        assert_eq!(run(&q, "tcp://10.1.2.1:1").verdict, Verdict::Allow);
        let z = pol(|p| p.allow_cidrs = s(&["::1/128"]));
        assert_eq!(run(&z, "tcp://[::1]:1").verdict, Verdict::Allow);
        assert_eq!(run(&z, "tcp://[::2]:1").verdict, Verdict::Drop);
    }

    #[test]
    fn udp_kill_switch() {
        let p = pol(|p| p.deny_udp = true);
        let d = run(&p, "udp://8.8.8.8:53");
        assert_eq!((d.verdict, d.stage), (Verdict::Drop, Stage::UdpKillSwitch));
        assert_eq!(run(&p, "tcp://8.8.8.8:53").verdict, Verdict::Allow);
        let q = pol(|p| {
            p.deny_udp = true;
            p.allow_ports = s(&["udp/53"]);
        });
        assert_eq!(run(&q, "udp://8.8.8.8:53").stage, Stage::UdpKillSwitch);
        assert!(
            lint(&q)
                .iter()
                .any(|f| f.message.contains("udp/53 can never match"))
        );
    }

    #[test]
    fn audit_mode_forwards_and_says_so() {
        let p = pol(|p| {
            p.default_allow = false;
            p.audit_mode = true;
        });
        let d = run(&p, "tcp://8.8.8.8:443");
        assert_eq!((d.verdict, d.audit_forwarded), (Verdict::Allow, true));
        assert_eq!(d.stage, Stage::DefaultDeny);
        let ok = run(&VmNetworkPolicy::default(), "tcp://8.8.8.8:443");
        assert!(!ok.audit_forwarded);
    }

    #[test]
    fn malformed_policy_lists_every_error() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.0.0.0", "10.0.0.0/33"]);
            p.deny_cidrs = s(&["::/129"]);
            p.allow_ports = s(&["tcp/0", "http/80", "tcp/443"]);
        });
        let errs = simulate(&p, &probe("tcp://1.1.1.1:1")).unwrap_err();
        assert_eq!(errs.len(), 5, "{errs:?}");
        let findings = lint(&p);
        assert!(findings.iter().all(|f| f.severity == Severity::Error));
        assert!(findings.iter().any(|f| f.field == "deny_cidrs"));
    }

    #[test]
    fn caveats_for_fqdn_and_rate() {
        let p = pol(|p| {
            p.allow_fqdns = s(&["example.com"]);
            p.max_egress_mbps = Some(10);
        });
        assert_eq!(run(&p, "tcp://8.8.8.8:443").caveats.len(), 2);
    }

    #[test]
    fn compare_reports_only_changes_and_flags_flips() {
        let before = pol(|p| p.allow_cidrs = s(&["10.0.0.0/8"]));
        let after = pol(|p| p.allow_cidrs = s(&["10.0.0.0/16"]));
        let probes = [
            probe("tcp://10.0.1.1:80"),
            probe("tcp://10.1.1.1:80"),
            probe("tcp://8.8.8.8:80"),
        ];
        let ch = compare(&before, &after, &probes).unwrap();
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].probe, probes[1]);
        assert!(ch[0].flipped);
        assert_eq!(ch[0].after.stage, Stage::CidrMiss);
        // same verdict, different deciding stage is reported but not flipped
        let b2 = pol(|p| p.default_allow = false);
        let a2 = pol(|p| p.deny_cidrs = s(&["0.0.0.0/0"]));
        let ch = compare(&b2, &a2, &[probe("tcp://8.8.8.8:80")]).unwrap();
        assert_eq!(ch.len(), 1);
        assert!(!ch[0].flipped);
    }

    #[test]
    fn compare_names_the_bad_policy() {
        let bad = pol(|p| p.allow_cidrs = s(&["nope/8"]));
        let e = compare(&VmNetworkPolicy::default(), &bad, &[]).unwrap_err();
        assert!(e[0].starts_with("new policy: "));
    }

    #[test]
    fn derived_probes_cover_edges_and_are_unique() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.1.0.0/16", "2001:db8::/126"]);
            p.allow_ports = s(&["tcp/8080"]);
        });
        let probes = derive_probes(&[&p]);
        assert!(probes.contains(&probe("tcp://10.1.0.0:8080")));
        assert!(probes.contains(&probe("tcp://10.1.255.255:8080")));
        assert!(probes.contains(&probe("tcp://[2001:db8::3]:8080")));
        assert!(probes.contains(&probe("icmp://10.1.0.0")));
        let mut dedup = probes.clone();
        dedup.dedup();
        for (i, a) in probes.iter().enumerate() {
            assert!(!probes[i + 1..].contains(a));
        }
        assert_eq!(dedup.len(), probes.len());
        // default protocols when no port rule exists
        let d = derive_probes(&[&VmNetworkPolicy::default()]);
        assert!(d.contains(&probe("tcp://8.8.8.8:443")) && d.contains(&probe("udp://8.8.8.8:53")));
    }

    #[test]
    fn derived_probe_edges_for_slash_zero_and_host_routes() {
        let p = pol(|p| p.allow_cidrs = s(&["0.0.0.0/0", "10.0.0.7/32", "::/0"]));
        let probes = derive_probes(&[&p]);
        assert!(probes.contains(&probe("tcp://255.255.255.255:443")));
        assert!(probes.contains(&probe("tcp://10.0.0.7:443")));
        assert!(probes.contains(&probe(
            "tcp://[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:443"
        )));
    }

    #[test]
    fn lint_finds_dead_and_misleading_rules() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.9.1.0/24", "10.9.1.0/24", "10.0.0.0/8"]);
            p.deny_cidrs = s(&["10.9.0.0/16"]);
            p.allow_ports = s(&["tcp/443"]);
            p.audit_mode = true;
            p.allow_fqdns = s(&["*.example.com"]);
        });
        let l = lint(&p);
        let has = |sev: Severity, needle: &str| {
            l.iter()
                .any(|f| f.severity == sev && f.message.contains(needle))
        };
        assert!(has(
            Severity::Warning,
            "10.9.1.0/24 is entirely inside deny_cidrs entry 10.9.0.0/16"
        ));
        assert!(has(Severity::Warning, "listed more than once"));
        assert!(has(Severity::Warning, "audit_mode is on"));
        assert!(has(Severity::Warning, "wildcard"));
        assert!(has(Severity::Info, "default_allow = true is ignored"));
        assert!(
            !l.iter()
                .any(|f| f.message.contains("10.0.0.0/8 is entirely"))
        );
    }

    #[test]
    fn lint_clean_policy_is_quiet() {
        let p = pol(|p| {
            p.allow_cidrs = s(&["10.0.0.0/8"]);
            p.allow_ports = s(&["tcp/443"]);
            p.default_allow = false;
        });
        assert!(lint(&p).is_empty(), "{:?}", lint(&p));
    }

    #[test]
    fn mask_helpers() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert_eq!(c.mask(), 0xff00_0000);
        let c0 = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(c0.contains("1.2.3.4".parse().unwrap()));
        let c6 = Cidr::parse("fe80::/10").unwrap();
        assert!(c6.contains("fe80::1".parse().unwrap()));
        assert!(!c6.contains("fec0::1".parse().unwrap()));
        assert!(
            Cidr::parse("10.0.0.0/8")
                .unwrap()
                .covers(&Cidr::parse("10.2.0.0/16").unwrap())
        );
        assert!(
            !Cidr::parse("10.2.0.0/16")
                .unwrap()
                .covers(&Cidr::parse("10.0.0.0/8").unwrap())
        );
    }
}
