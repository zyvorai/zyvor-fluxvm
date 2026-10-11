// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Conversion from fluxvm-network's per-VM policy model (`dataplane::VmNetworkPolicy`, enforced by nftables/eBPF
//! on Linux) to the userspace gateway's [`EgressPolicy`]. See `docs/egress-policy.md` for the mapping table.
//!
//! `fluxvm-network` is a Linux-first crate (eBPF, nftables, an HTTP proxy stack) and `fluxvm-apple` does not depend
//! on it, so the input is [`NetworkPolicyView`]: a field-for-field serde mirror of the part of `VmNetworkPolicy`
//! that has a meaning for the gateway. The JSON of a `VmNetworkPolicy` deserializes into it unchanged (unknown
//! fields are ignored). Nothing here changes how fluxvm-network behaves.

use fluxvm_usernet::{Cidr, Cidr6, EgressPolicy};
use serde::Deserialize;
use std::net::IpAddr;

/// The subset of `fluxvm_network::dataplane::VmNetworkPolicy` the gateway can act on, with the same field names
/// and the same defaults (`default_allow` is true).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkPolicyView {
    pub default_allow: bool,
    pub allow_cidrs: Vec<String>,
    pub deny_cidrs: Vec<String>,
    /// `tcp/443`, `udp/53`, `icmp/0`, ...
    pub allow_ports: Vec<String>,
    pub allow_icmp: bool,
    pub allow_fqdns: Vec<String>,
    pub entities: Vec<String>,
    pub audit_mode: bool,
    pub deny_udp: bool,
    pub max_egress_mbps: Option<u32>,
    pub max_egress_pps: Option<u32>,
    pub groups: Vec<String>,
    pub labels: Vec<String>,
}

impl Default for NetworkPolicyView {
    fn default() -> Self {
        Self {
            default_allow: true,
            allow_cidrs: Vec::new(),
            deny_cidrs: Vec::new(),
            allow_ports: Vec::new(),
            allow_icmp: false,
            allow_fqdns: Vec::new(),
            entities: Vec::new(),
            audit_mode: false,
            deny_udp: false,
            max_egress_mbps: None,
            max_egress_pps: None,
            groups: Vec::new(),
            labels: Vec::new(),
        }
    }
}

/// Result of [`egress_from_network_policy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    pub egress: EgressPolicy,
    /// Value for `UserNetConfig::icmp_proxy` (ICMP is not part of [`EgressPolicy`]).
    pub icmp_proxy: bool,
    /// Differences that keep the gateway equal or stricter than the source policy, or that widen it only
    /// at protocol granularity (TCP and UDP share one port list in the gateway).
    pub notes: Vec<String>,
    /// Source features the gateway cannot enforce: the converted policy is *weaker* than the source in these
    /// respects. Callers that need strict equivalence should refuse a conversion with a non-empty list.
    pub unsupported: Vec<String>,
}

fn parse_cidr(raw: &str) -> Result<IpCidrAny, String> {
    let (ip, _) = raw
        .split_once('/')
        .ok_or_else(|| format!("CIDR {raw:?} must include /prefix"))?;
    let ip: IpAddr = ip
        .trim()
        .parse()
        .map_err(|_| format!("invalid IP in CIDR {raw:?}"))?;
    match ip {
        IpAddr::V4(_) => raw
            .trim()
            .parse::<Cidr>()
            .map(IpCidrAny::V4)
            .map_err(|e| format!("CIDR {e}")),
        IpAddr::V6(_) => raw
            .trim()
            .parse::<Cidr6>()
            .map(IpCidrAny::V6)
            .map_err(|e| format!("CIDR {e}")),
    }
}

enum IpCidrAny {
    V4(Cidr),
    V6(Cidr6),
}

/// A glob a gateway domain entry can express: `name` or `*.suffix` (the gateway treats `name` as "name and
/// everything under it" either way).
fn domain_entry(pat: &str) -> Option<String> {
    let p = pat.trim().trim_end_matches('.').to_ascii_lowercase();
    let host = p.strip_prefix("*.").unwrap_or(&p);
    let ok = !host.is_empty()
        && !host.contains('*')
        && !host.starts_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    ok.then_some(p)
}

/// Maps a [`NetworkPolicyView`] onto the gateway's egress policy. Errors only on malformed CIDRs or port rules
/// (the same inputs fluxvm-network's `validate_policy` rejects).
pub fn egress_from_network_policy(p: &NetworkPolicyView) -> Result<Converted, String> {
    let mut e = EgressPolicy::default();
    let mut notes = Vec::new();
    let mut unsupported = Vec::new();

    for c in &p.allow_cidrs {
        match parse_cidr(c)? {
            IpCidrAny::V4(c) => e.allow_cidrs.push(c),
            IpCidrAny::V6(c) => e.allow_cidrs6.push(c),
        }
    }
    for c in &p.deny_cidrs {
        match parse_cidr(c)? {
            IpCidrAny::V4(c) => e.deny_cidrs.push(c),
            IpCidrAny::V6(c) => e.deny_cidrs6.push(c),
        }
    }

    // L4 rules: tcp/udp feed the shared port list; icmp rules turn the ICMP proxy on; sctp has no gateway path.
    let (mut tcp, mut udp) = (Vec::new(), Vec::new());
    let mut icmp_rule = false;
    for r in &p.allow_ports {
        let (proto, port) = r
            .split_once('/')
            .ok_or_else(|| format!("port rule {r:?} must be proto/PORT"))?;
        let port: u16 = port
            .trim()
            .parse()
            .map_err(|_| format!("invalid port in {r:?}"))?;
        match proto.trim().to_ascii_lowercase().as_str() {
            "tcp" => tcp.push(port),
            "udp" => udp.push(port),
            "icmp" | "icmp6" | "icmpv6" => icmp_rule = true,
            "sctp" => notes.push(format!("{r}: SCTP is never forwarded by the gateway")),
            other => return Err(format!("unsupported L4 protocol {other:?} in {r:?}")),
        }
        if port == 0 && matches!(proto.trim().to_ascii_lowercase().as_str(), "tcp" | "udp") {
            return Err(format!("port must be 1..65535 in {r:?}"));
        }
    }
    tcp.sort_unstable();
    tcp.dedup();
    udp.sort_unstable();
    udp.dedup();
    if !tcp.is_empty() && !udp.is_empty() && tcp != udp {
        notes.push(
            "tcp and udp port rules differ: the gateway has one port list, so each listed port is allowed for \
             both protocols"
                .into(),
        );
    }
    let mut ports: Vec<u16> = tcp.into_iter().chain(udp).collect();
    ports.sort_unstable();
    ports.dedup();
    e.allow_ports = ports;

    // FQDNs: exact names and `*.suffix` only.
    for f in &p.allow_fqdns {
        match domain_entry(f) {
            Some(d) => e.allow_domains.push(d),
            None => notes.push(format!(
                "allow_fqdns {f:?} is not a name or *.suffix and was dropped (the allow-list is narrower)"
            )),
        }
    }
    if !p.allow_fqdns.is_empty()
        && e.allow_domains.is_empty()
        && e.allow_cidrs.is_empty()
        && e.allow_cidrs6.is_empty()
    {
        // Every FQDN entry was dropped: do not silently fall back to "allow everything public".
        e.deny_cidrs.push("0.0.0.0/0".parse().unwrap());
        e.deny_cidrs6.push("::/0".parse().unwrap());
        notes.push("no usable allow_fqdns entry: converted to deny-all".into());
    }

    // Nothing is private-range filtered by fluxvm-network unless an allow-list excludes the destination.
    e.allow_private = true;

    // default_allow=false with no allow lists is "deny everything".
    let lists = !p.allow_cidrs.is_empty() || !p.allow_ports.is_empty() || !p.allow_fqdns.is_empty();
    if !p.default_allow && !lists && e.deny_cidrs.iter().all(|c| c.prefix != 0) {
        e.deny_cidrs.push("0.0.0.0/0".parse().unwrap());
        e.deny_cidrs6.push("::/0".parse().unwrap());
        notes.push(
            "default_allow=false without allow lists: converted to deny-all via 0/0 and ::/0"
                .into(),
        );
    }

    // ICMP: allowed when the source policy does not enforce, or explicitly.
    let enforcing = lists || !p.default_allow;
    let icmp_proxy = !enforcing || p.allow_icmp || icmp_rule;

    if !p.entities.is_empty() {
        unsupported.push(format!(
            "entities {:?} are not interpreted by the gateway (use CIDRs or FQDNs)",
            p.entities
        ));
    }
    if p.deny_udp {
        unsupported.push(
            "deny_udp has no gateway counterpart (UDP stays allowed per CIDR/port rules)".into(),
        );
    }
    if p.max_egress_mbps.is_some() || p.max_egress_pps.is_some() {
        unsupported.push("max_egress_mbps / max_egress_pps are not enforced by the gateway".into());
    }
    if p.audit_mode {
        notes.push("audit_mode (log-and-allow) is enforced as a real policy by the gateway".into());
    }
    if !p.groups.is_empty() || !p.labels.is_empty() {
        notes.push(
            "groups/labels are merged into the policy by fluxvm-network before this conversion"
                .into(),
        );
    }

    Ok(Converted {
        egress: e,
        icmp_proxy,
        notes,
        unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_usernet::Decision;
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::time::Instant;

    fn view(json: &str) -> NetworkPolicyView {
        serde_json::from_str(json).unwrap()
    }

    fn chk(e: &EgressPolicy, ip: &str, port: u16) -> Decision {
        e.check(
            ip.parse::<Ipv4Addr>().unwrap(),
            port,
            &HashMap::new(),
            Instant::now(),
        )
    }

    #[test]
    fn network_json_roundtrips_into_the_view_with_its_defaults() {
        // A full VmNetworkPolicy document (extra fields ignored, defaults applied).
        let v = view(
            r#"{"allow_cidrs":["10.0.0.0/8","2001:db8::/32"],"sample_rate":4,"compiled_group_ids":[1]}"#,
        );
        assert!(v.default_allow);
        assert_eq!(v.allow_cidrs.len(), 2);
        let c = egress_from_network_policy(&v).unwrap();
        assert_eq!(c.egress.allow_cidrs.len(), 1);
        assert_eq!(c.egress.allow_cidrs6.len(), 1);
        assert!(c.unsupported.is_empty());
    }

    #[test]
    fn no_rules_allows_everything_including_private() {
        let c = egress_from_network_policy(&NetworkPolicyView::default()).unwrap();
        assert!(c.icmp_proxy);
        assert_eq!(chk(&c.egress, "10.1.2.3", 80), Decision::Allow);
        assert_eq!(chk(&c.egress, "8.8.8.8", 53), Decision::Allow);
    }

    #[test]
    fn cidr_and_port_dimensions_are_anded_like_the_cross_product() {
        let c = egress_from_network_policy(&view(
            r#"{"allow_cidrs":["203.0.113.0/24"],"allow_ports":["tcp/443","tcp/80"],"deny_cidrs":["203.0.113.66/32"]}"#,
        ))
        .unwrap();
        let e = &c.egress;
        assert_eq!(chk(e, "203.0.113.5", 443), Decision::Allow);
        assert_eq!(
            chk(e, "203.0.113.5", 22),
            Decision::Deny("port-not-allowed")
        );
        assert_eq!(chk(e, "8.8.8.8", 443), Decision::Deny("not-in-allow-list"));
        assert_eq!(chk(e, "203.0.113.66", 443), Decision::Deny("deny-cidr"));
        assert!(
            !c.icmp_proxy,
            "enforcing policy without allow_icmp drops ICMP"
        );
    }

    #[test]
    fn ports_only_allows_any_destination_on_those_ports() {
        let c = egress_from_network_policy(&view(r#"{"allow_ports":["tcp/443"]}"#)).unwrap();
        assert_eq!(chk(&c.egress, "10.9.9.9", 443), Decision::Allow);
        assert_eq!(
            chk(&c.egress, "10.9.9.9", 80),
            Decision::Deny("port-not-allowed")
        );
    }

    #[test]
    fn mixed_protocol_ports_widen_and_are_reported() {
        let c =
            egress_from_network_policy(&view(r#"{"allow_ports":["tcp/443","udp/53","sctp/9"]}"#))
                .unwrap();
        assert_eq!(c.egress.allow_ports, vec![53, 443]);
        assert!(c.notes.iter().any(|n| n.contains("both protocols")));
        assert!(c.notes.iter().any(|n| n.contains("SCTP")));
    }

    #[test]
    fn default_deny_without_lists_denies_everything() {
        let c = egress_from_network_policy(&view(r#"{"default_allow":false}"#)).unwrap();
        assert_eq!(chk(&c.egress, "8.8.8.8", 443), Decision::Deny("deny-cidr"));
        assert!(!c.icmp_proxy);
        let d = c.egress.check6(
            "2606:4700::1".parse().unwrap(),
            Some(443),
            &HashMap::new(),
            Instant::now(),
        );
        assert_eq!(d, Decision::Deny("deny-cidr"));
    }

    #[test]
    fn fqdns_become_domains_and_unusable_patterns_fail_closed() {
        let c = egress_from_network_policy(&view(
            r#"{"allow_fqdns":["Example.com","*.corp.test"],"allow_icmp":true}"#,
        ))
        .unwrap();
        assert_eq!(c.egress.allow_domains, vec!["example.com", "*.corp.test"]);
        assert!(c.icmp_proxy);
        assert_eq!(c.egress.check_domain("api.corp.test"), Decision::Allow);
        assert!(matches!(
            c.egress.check_domain("evil.test"),
            Decision::Deny(_)
        ));

        let c =
            egress_from_network_policy(&view(r#"{"allow_fqdns":["api-*.example.com"]}"#)).unwrap();
        assert!(c.egress.allow_domains.is_empty());
        assert_eq!(
            chk(&c.egress, "8.8.8.8", 443),
            Decision::Deny("deny-cidr"),
            "fails closed"
        );
    }

    #[test]
    fn unenforceable_features_are_listed() {
        let c = egress_from_network_policy(&view(
            r#"{"deny_udp":true,"max_egress_mbps":10,"entities":["world"],"audit_mode":true}"#,
        ))
        .unwrap();
        assert_eq!(c.unsupported.len(), 3);
        assert!(c.notes.iter().any(|n| n.contains("audit_mode")));
    }

    #[test]
    fn malformed_input_is_rejected() {
        for j in [
            r#"{"allow_cidrs":["10.0.0.0"]}"#,
            r#"{"allow_cidrs":["10.0.0.0/33"]}"#,
            r#"{"deny_cidrs":["nope/8"]}"#,
            r#"{"allow_ports":["443"]}"#,
            r#"{"allow_ports":["tcp/0"]}"#,
            r#"{"allow_ports":["gre/1"]}"#,
        ] {
            assert!(egress_from_network_policy(&view(j)).is_err(), "{j}");
        }
    }
}
