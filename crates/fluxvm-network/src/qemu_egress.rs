// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Sentinel Set 7S egress policy for the QEMU process of a VM.
//!
//! A guest on QEMU user-mode (slirp) networking has no host-visible interface, so the
//! tc dataplane policy cannot govern it: slirp re-originates every guest connection from
//! the QEMU process. `bpf/fluxvm_qemu_egress.bpf.c` is the program that sees that traffic.
//! It used to allow loopback only, which left such guests with no network at all. This
//! module turns a small declarative policy (a mode plus `cidr/proto/port` rules) into the
//! program's maps, and reads back the flows it dropped (or would have dropped, in audit
//! mode) so the control plane can explain a denial and suggest the rule that fixes it.
//!
//! Layout constants must match `bpf/fluxvm_qemu_egress.bpf.c`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::Ipv4Addr,
    path::Path,
    process::Command,
};

use crate::ebpf::{bpftool_map_update, require_bpftool};
pub use fluxvm_core::egress::{EgressMode, EgressProto, EgressRule, QemuEgressPolicy};

/// `MAX_RULES` in the BPF program: capacity of the LPM trie.
pub const MAX_RULES: usize = 256;
/// `MAX_RULE_PORTS`: (proto, port) pairs one prefix can carry after covering rules are folded in.
pub const MAX_RULE_PORTS: usize = 4;

const MODE_LOOPBACK: u32 = 0;
const MODE_ALLOWLIST: u32 = 1;
const MODE_AUDIT: u32 = 2;
const MODE_OPEN: u32 = 3;

fn mode_bpf(mode: EgressMode) -> u32 {
    match mode {
        EgressMode::Loopback => MODE_LOOPBACK,
        EgressMode::Allowlist => MODE_ALLOWLIST,
        EgressMode::Audit => MODE_AUDIT,
        EgressMode::Open => MODE_OPEN,
    }
}

fn proto_number(p: EgressProto) -> u8 {
    match p {
        EgressProto::Any => 0,
        EgressProto::Tcp => 6,
        EgressProto::Udp => 17,
    }
}

fn proto_from_number(n: u8) -> Option<EgressProto> {
    match n {
        6 => Some(EgressProto::Tcp),
        17 => Some(EgressProto::Udp),
        _ => None,
    }
}

/// A rule after its CIDR is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Parsed {
    prefix: u8,
    /// Network address, host byte order, host bits cleared.
    addr: u32,
    proto: EgressProto,
    port: Option<u16>,
}

fn parse_cidr(s: &str) -> Result<(u32, u8)> {
    let (ip, len) = match s.split_once('/') {
        Some((ip, len)) => (
            ip,
            len.parse::<u8>()
                .with_context(|| format!("invalid prefix length in {s:?}"))?,
        ),
        None => (s, 32),
    };
    if len > 32 {
        bail!("prefix length in {s:?} is greater than 32");
    }
    let ip: Ipv4Addr = ip.trim().parse().with_context(|| {
        format!("{s:?} is not an IPv4 CIDR (IPv6 egress rules are not supported yet)")
    })?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Ok((u32::from(ip) & mask, len))
}

fn parse_rules(rules: &[EgressRule]) -> Result<Vec<Parsed>> {
    rules
        .iter()
        .map(|r| {
            let (addr, prefix) = parse_cidr(&r.cidr)?;
            if r.port == Some(0) {
                bail!(
                    "rule {:?}: port 0 is not a port; omit it to allow every port",
                    r.cidr
                );
            }
            if r.port.is_some() && r.proto == EgressProto::Any {
                bail!("rule {:?}: a port needs a protocol (tcp or udp)", r.cidr);
            }
            Ok(Parsed {
                prefix,
                addr,
                proto: r.proto,
                port: r.port,
            })
        })
        .collect()
}

/// Check a policy without touching the kernel. Run on every write path so a bad
/// policy is rejected before it is stored.
pub fn validate(policy: &QemuEgressPolicy) -> Result<()> {
    if policy.rules.len() > MAX_RULES {
        bail!(
            "{} rules is over the limit of {MAX_RULES}; use wider CIDRs",
            policy.rules.len()
        );
    }
    if policy.mode == EgressMode::Allowlist && policy.rules.is_empty() {
        // Allowed on purpose (it equals loopback-only), but almost always a mistake.
        tracing::debug!("allowlist egress policy with no rules");
    }
    compile(&policy.rules).map(|_| ())
}

/// What the rules for one network allow: every port (`true`), or these (proto, port) pairs.
type Ports = (bool, BTreeSet<(u8, u16)>);

/// One LPM entry as it is written to the map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub prefix: u8,
    pub addr: u32,
    /// `None` = any port and protocol; otherwise (proto number, port) pairs.
    pub ports: Option<Vec<(u8, u16)>>,
}

/// Turn rules into LPM entries.
///
/// The kernel returns only the *longest* matching prefix, so an entry that is narrower
/// than another rule must also carry the covering rule's ports (otherwise a narrow rule
/// for `10.1.0.0/16 tcp/22` would hide a broad `10.0.0.0/8 tcp/443`). Folding is done here
/// so the BPF side stays a single lookup.
pub fn compile(rules: &[EgressRule]) -> Result<Vec<Compiled>> {
    let parsed = parse_rules(rules)?;
    let mut own: BTreeMap<(u8, u32), Ports> = BTreeMap::new();
    for p in &parsed {
        let e = own.entry((p.prefix, p.addr)).or_default();
        match p.port {
            None => e.0 = true,
            Some(port) => {
                e.1.insert((proto_number(p.proto), port));
            }
        }
    }
    let covers = |outer: (u8, u32), inner: (u8, u32)| -> bool {
        if outer.0 > inner.0 {
            return false;
        }
        let mask = if outer.0 == 0 {
            0
        } else {
            u32::MAX << (32 - outer.0)
        };
        (inner.1 & mask) == outer.1
    };
    let mut out = Vec::new();
    for &key in own.keys() {
        let mut any = false;
        let mut ports: BTreeSet<(u8, u16)> = BTreeSet::new();
        for (&other, (oany, oports)) in own.iter() {
            if covers(other, key) {
                any |= *oany;
                ports.extend(oports.iter().copied());
            }
        }
        if any {
            out.push(Compiled {
                prefix: key.0,
                addr: key.1,
                ports: None,
            });
            continue;
        }
        if ports.len() > MAX_RULE_PORTS {
            bail!(
                "{}/{} would need {} port rules once rules for covering networks are included; at most {MAX_RULE_PORTS} are supported per network. Allow every port for it, or use fewer distinct ports",
                Ipv4Addr::from(key.1),
                key.0,
                ports.len()
            );
        }
        out.push(Compiled {
            prefix: key.0,
            addr: key.1,
            ports: Some(ports.into_iter().collect()),
        });
    }
    Ok(out)
}

/// `struct lpm_key4`: u32 prefixlen (native), u32 address in network byte order.
fn encode_key(c: &Compiled) -> Vec<u8> {
    let mut k = Vec::with_capacity(8);
    k.extend_from_slice(&(c.prefix as u32).to_ne_bytes());
    k.extend_from_slice(&c.addr.to_be_bytes());
    k
}

/// `struct rule_val`: u8 nports, u8 proto[4], u8 pad[3], u16 port[4] (native).
fn encode_val(c: &Compiled) -> Vec<u8> {
    let mut v = vec![0u8; 16];
    if let Some(ports) = &c.ports {
        v[0] = ports.len() as u8;
        for (i, (proto, port)) in ports.iter().enumerate() {
            v[1 + i] = *proto;
            v[8 + 2 * i..10 + 2 * i].copy_from_slice(&port.to_ne_bytes());
        }
    }
    v
}

/// `struct qemu_cfg`: u32 mode, u32 reserved.
fn encode_cfg(mode: EgressMode) -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.extend_from_slice(&mode_bpf(mode).to_ne_bytes());
    v.extend_from_slice(&[0u8; 4]);
    v
}

fn hex_args(bytes: &[u8]) -> Vec<String> {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn map_dir(pin_dir: &Path) -> std::path::PathBuf {
    pin_dir.join("maps")
}

/// Program a VM's pinned maps with `policy`. Entries that are no longer wanted are removed
/// after the new ones are in place, and the mode is written last, so a policy change never
/// opens a window that is wider than either the old or the new policy.
pub fn apply(pin_dir: &Path, policy: &QemuEgressPolicy) -> Result<()> {
    require_bpftool()?;
    let compiled = compile(&policy.rules)?;
    let maps = map_dir(pin_dir);
    let cfg_map = maps.join("fluxvm_qemu_cfg");
    let allow_map = maps.join("fluxvm_qemu_allow4");
    if !cfg_map.exists() || !allow_map.exists() {
        bail!(
            "this VM's QEMU egress program has no policy maps (started before policy support); restart the VM to enable egress policy"
        );
    }

    let wanted: BTreeSet<Vec<u8>> = compiled.iter().map(encode_key).collect();
    for c in &compiled {
        bpftool_map_update(&allow_map, &encode_key(c), &encode_val(c))?;
    }
    for stale in existing_keys(&allow_map)?.difference(&wanted) {
        let mut args: Vec<String> = vec![
            "map".into(),
            "delete".into(),
            "pinned".into(),
            allow_map.display().to_string(),
            "key".into(),
            "hex".into(),
        ];
        args.extend(hex_args(stale));
        crate::ebpf::run("bpftool", &args)?;
    }
    bpftool_map_update(&cfg_map, &0u32.to_ne_bytes(), &encode_cfg(policy.mode))
}

fn existing_keys(map: &Path) -> Result<BTreeSet<Vec<u8>>> {
    let root = dump(map)?;
    let mut keys = BTreeSet::new();
    if let Some(arr) = root.as_array() {
        for e in arr {
            // BTF maps print named fields; the LPM key is {prefixlen, addr}.
            let prefix = e["key"]["prefixlen"].as_u64();
            let addr = e["key"]["addr"].as_u64();
            if let (Some(p), Some(a)) = (prefix, addr) {
                let mut k = Vec::with_capacity(8);
                k.extend_from_slice(&(p as u32).to_ne_bytes());
                // `addr` is read as a native u32 of network-order bytes.
                k.extend_from_slice(&(a as u32).to_ne_bytes());
                keys.insert(k);
            }
        }
    }
    Ok(keys)
}

fn dump(map: &Path) -> Result<Value> {
    if !map.exists() {
        bail!("eBPF map is not pinned at {}", map.display());
    }
    let out = Command::new("bpftool")
        .args(["-j", "map", "dump", "pinned"])
        .arg(map)
        .output()
        .context("running bpftool map dump")?;
    if !out.status.success() {
        bail!(
            "bpftool map dump {} failed: {}",
            map.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let root: Value = serde_json::from_slice(&out.stdout).context("parsing bpftool JSON")?;
    Ok(normalize_dump(root))
}

/// With `-j`, bpftool prints raw hex under `key`/`value` and the BTF-decoded fields under
/// `formatted`. Callers read named fields, so lift `formatted` over each entry.
fn normalize_dump(root: Value) -> Value {
    match root {
        Value::Array(entries) => Value::Array(
            entries
                .into_iter()
                .map(|mut e| match e.get_mut("formatted").map(Value::take) {
                    Some(f) if f.is_object() => f,
                    _ => e,
                })
                .collect(),
        ),
        other => other,
    }
}

/// A destination the program dropped, or in audit mode would have dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedFlow {
    pub dst: String,
    pub proto: EgressProto,
    /// 0 for non-tcp/udp traffic.
    pub port: u16,
    pub packets: u64,
    pub bytes: u64,
    /// `true` = traffic was allowed (audit mode); `false` = it was dropped.
    pub audit: bool,
    /// Nanoseconds since boot (`CLOCK_MONOTONIC`) of the last packet.
    pub last_ns: u64,
}

/// Flows recorded by the program, busiest first.
pub fn observed(pin_dir: &Path) -> Result<Vec<ObservedFlow>> {
    let root = dump(&map_dir(pin_dir).join("fluxvm_qemu_seen"))?;
    let mut flows = Vec::new();
    for e in root.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let (Some(daddr), Some(port), Some(proto)) = (
            e["key"]["daddr"].as_u64(),
            e["key"]["port"].as_u64(),
            e["key"]["proto"].as_u64(),
        ) else {
            continue;
        };
        flows.push(ObservedFlow {
            dst: Ipv4Addr::from((daddr as u32).to_ne_bytes()).to_string(),
            proto: proto_from_number(proto as u8).unwrap_or(EgressProto::Any),
            port: port as u16,
            packets: e["value"]["packets"].as_u64().unwrap_or(0),
            bytes: e["value"]["bytes"].as_u64().unwrap_or(0),
            audit: e["key"]["audit"].as_u64().unwrap_or(0) == 1,
            last_ns: e["value"]["last_ns"].as_u64().unwrap_or(0),
        });
    }
    flows.sort_by(|a, b| b.packets.cmp(&a.packets).then_with(|| a.dst.cmp(&b.dst)));
    Ok(flows)
}

/// Read the mode and rules currently programmed into the kernel (what is enforced, as
/// opposed to what was last asked for).
pub fn programmed(pin_dir: &Path) -> Result<QemuEgressPolicy> {
    let maps = map_dir(pin_dir);
    let cfg = dump(&maps.join("fluxvm_qemu_cfg"))?;
    let mode = match cfg[0]["value"]["mode"].as_u64().unwrap_or(0) as u32 {
        MODE_ALLOWLIST => EgressMode::Allowlist,
        MODE_AUDIT => EgressMode::Audit,
        MODE_OPEN => EgressMode::Open,
        _ => EgressMode::Loopback,
    };
    let allow = dump(&maps.join("fluxvm_qemu_allow4"))?;
    let mut rules = Vec::new();
    for e in allow.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let (Some(prefix), Some(addr)) =
            (e["key"]["prefixlen"].as_u64(), e["key"]["addr"].as_u64())
        else {
            continue;
        };
        let cidr = format!("{}/{}", Ipv4Addr::from((addr as u32).to_ne_bytes()), prefix);
        let n = e["value"]["nports"].as_u64().unwrap_or(0) as usize;
        if n == 0 {
            rules.push(EgressRule {
                cidr,
                proto: EgressProto::Any,
                port: None,
                note: None,
            });
            continue;
        }
        for i in 0..n.min(MAX_RULE_PORTS) {
            rules.push(EgressRule {
                cidr: cidr.clone(),
                proto: proto_from_number(e["value"]["proto"][i].as_u64().unwrap_or(0) as u8)
                    .unwrap_or(EgressProto::Any),
                port: e["value"]["port"][i].as_u64().map(|p| p as u16),
                note: None,
            });
        }
    }
    Ok(QemuEgressPolicy { mode, rules })
}

/// Turn what a guest tried to reach into rules that would allow it.
///
/// Each distinct (destination, protocol, port) becomes a host rule. When many hosts in one
/// /24 are hit on the same port (a CDN or package mirror), they collapse into the /24 so
/// the suggestion stays short and survives address rotation.
pub fn suggest(flows: &[ObservedFlow]) -> Vec<EgressRule> {
    let mut by_service: BTreeMap<(EgressProto, u16), BTreeMap<u32, BTreeSet<Ipv4Addr>>> =
        BTreeMap::new();
    for f in flows {
        let Ok(ip) = f.dst.parse::<Ipv4Addr>() else {
            continue;
        };
        if f.proto == EgressProto::Any && f.port == 0 {
            // ICMP and other non-port traffic: not expressible as a port rule.
            continue;
        }
        by_service
            .entry((f.proto, f.port))
            .or_default()
            .entry(u32::from(ip) & 0xffff_ff00)
            .or_default()
            .insert(ip);
    }
    let mut rules = Vec::new();
    for ((proto, port), nets) in by_service {
        for (net, hosts) in nets {
            let (cidr, note) = if hosts.len() >= 3 {
                (
                    format!("{}/24", Ipv4Addr::from(net)),
                    format!("{} hosts seen", hosts.len()),
                )
            } else {
                (String::new(), String::new())
            };
            if !cidr.is_empty() {
                rules.push(EgressRule {
                    cidr,
                    proto,
                    port: Some(port),
                    note: Some(note),
                });
            } else {
                for h in hosts {
                    rules.push(EgressRule {
                        cidr: format!("{h}/32"),
                        proto,
                        port: Some(port),
                        note: None,
                    });
                }
            }
        }
    }
    rules
}

// ---- config strings --------------------------------------------------------------------

/// Parse `cidr`, `cidr:tcp/443` or `cidr:udp/53` (the `fluxvm.toml` spelling).
pub fn parse_rule(s: &str) -> Result<EgressRule> {
    let (cidr, l4) = match s.split_once(':') {
        Some((c, l4)) => (c.trim(), Some(l4.trim())),
        None => (s.trim(), None),
    };
    let (proto, port) = match l4 {
        None => (EgressProto::Any, None),
        Some(l4) => {
            let (p, port) = l4.split_once('/').with_context(|| {
                format!("{s:?}: expected cidr:proto/port, e.g. 10.0.0.0/8:tcp/443")
            })?;
            let proto = match p {
                "tcp" => EgressProto::Tcp,
                "udp" => EgressProto::Udp,
                other => bail!("{s:?}: protocol {other:?} is not tcp or udp"),
            };
            (
                proto,
                Some(
                    port.parse::<u16>()
                        .with_context(|| format!("{s:?}: bad port"))?,
                ),
            )
        }
    };
    Ok(EgressRule {
        cidr: cidr.into(),
        proto,
        port,
        note: None,
    })
}

/// The node's configured default (`[sandbox.dataplane] qemu_egress_*`).
pub fn config_default(mode: &str, rules: &[String]) -> Result<QemuEgressPolicy> {
    let p = QemuEgressPolicy {
        mode: mode.parse().map_err(|e: String| anyhow::anyhow!(e))?,
        rules: rules.iter().map(|r| parse_rule(r)).collect::<Result<_>>()?,
    };
    validate(&p)?;
    Ok(p)
}

// ---- persistence ---------------------------------------------------------------------

fn vm_path(state_dir: &Path, id: uuid::Uuid) -> std::path::PathBuf {
    state_dir.join("qemu-egress").join(format!("{id}.json"))
}

fn defaults_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("qemu-egress-defaults.json")
}

fn write_json(path: &Path, policy: &QemuEgressPolicy) -> Result<()> {
    let parent = path.parent().context("egress policy path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(policy)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn read_json(path: &Path) -> Result<Option<QemuEgressPolicy>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let p: QemuEgressPolicy = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            validate(&p)?;
            Ok(Some(p))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save_vm_policy(state_dir: &Path, id: uuid::Uuid, policy: &QemuEgressPolicy) -> Result<()> {
    validate(policy)?;
    write_json(&vm_path(state_dir, id), policy)
}

pub fn load_vm_policy(state_dir: &Path, id: uuid::Uuid) -> Result<Option<QemuEgressPolicy>> {
    read_json(&vm_path(state_dir, id))
}

pub fn delete_vm_policy(state_dir: &Path, id: uuid::Uuid) -> Result<()> {
    match fs::remove_file(vm_path(state_dir, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("deleting VM egress policy"),
    }
}

pub fn save_defaults(state_dir: &Path, policy: &QemuEgressPolicy) -> Result<()> {
    validate(policy)?;
    write_json(&defaults_path(state_dir), policy)
}

pub fn load_defaults(state_dir: &Path) -> Result<Option<QemuEgressPolicy>> {
    read_json(&defaults_path(state_dir))
}

/// What a VM gets: its own policy, else the node default, else loopback-only.
pub fn effective(
    state_dir: &Path,
    id: uuid::Uuid,
    config_default: &QemuEgressPolicy,
) -> Result<(QemuEgressPolicy, PolicySource)> {
    if let Some(p) = load_vm_policy(state_dir, id)? {
        return Ok((p, PolicySource::Vm));
    }
    if let Some(p) = load_defaults(state_dir)? {
        return Ok((p, PolicySource::NodeDefault));
    }
    if config_default.mode != EgressMode::Loopback || !config_default.rules.is_empty() {
        return Ok((config_default.clone(), PolicySource::Config));
    }
    Ok((QemuEgressPolicy::default(), PolicySource::Builtin))
}

/// Where the effective policy came from, so a UI can say "inherited from the node default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    Vm,
    NodeDefault,
    Config,
    Builtin,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(cidr: &str, proto: EgressProto, port: Option<u16>) -> EgressRule {
        EgressRule {
            cidr: cidr.into(),
            proto,
            port,
            note: None,
        }
    }

    #[test]
    fn cidr_parsing_clears_host_bits_and_rejects_garbage() {
        assert_eq!(parse_cidr("10.1.2.3/8").unwrap(), (0x0a00_0000, 8));
        assert_eq!(parse_cidr("192.168.1.5").unwrap(), (0xc0a8_0105, 32));
        assert_eq!(parse_cidr("0.0.0.0/0").unwrap(), (0, 0));
        assert!(parse_cidr("10.0.0.0/33").is_err());
        assert!(parse_cidr("fe80::/10").is_err());
        assert!(parse_cidr("nope").is_err());
    }

    #[test]
    fn a_port_needs_a_protocol_and_zero_is_not_a_port() {
        assert!(
            validate(&QemuEgressPolicy {
                mode: EgressMode::Allowlist,
                rules: vec![rule("10.0.0.0/8", EgressProto::Any, Some(443))],
            })
            .is_err()
        );
        assert!(
            validate(&QemuEgressPolicy {
                mode: EgressMode::Allowlist,
                rules: vec![rule("10.0.0.0/8", EgressProto::Tcp, Some(0))],
            })
            .is_err()
        );
    }

    #[test]
    fn narrow_rules_inherit_the_ports_of_covering_networks() {
        // 10.1.0.0/16 only lists tcp/22, but a packet to 10.1.2.3:443 must still be
        // allowed by the covering 10.0.0.0/8 tcp/443 rule, which the kernel's
        // longest-prefix match would otherwise never consult.
        let c = compile(&[
            rule("10.0.0.0/8", EgressProto::Tcp, Some(443)),
            rule("10.1.0.0/16", EgressProto::Tcp, Some(22)),
        ])
        .unwrap();
        let narrow = c.iter().find(|e| e.prefix == 16).unwrap();
        assert_eq!(narrow.ports, Some(vec![(6, 22), (6, 443)]));
        let broad = c.iter().find(|e| e.prefix == 8).unwrap();
        assert_eq!(broad.ports, Some(vec![(6, 443)]));
    }

    #[test]
    fn an_any_port_rule_on_a_covering_network_wins() {
        let c = compile(&[
            rule("10.0.0.0/8", EgressProto::Any, None),
            rule("10.1.0.0/16", EgressProto::Tcp, Some(22)),
        ])
        .unwrap();
        assert!(c.iter().all(|e| e.ports.is_none()));
    }

    #[test]
    fn a_default_route_rule_applies_everywhere() {
        let c = compile(&[
            rule("0.0.0.0/0", EgressProto::Tcp, Some(443)),
            rule("192.168.0.0/16", EgressProto::Tcp, Some(22)),
        ])
        .unwrap();
        let lan = c.iter().find(|e| e.prefix == 16).unwrap();
        assert_eq!(lan.ports, Some(vec![(6, 22), (6, 443)]));
    }

    #[test]
    fn too_many_ports_for_one_network_is_a_clear_error() {
        let rules: Vec<_> = (1..=5)
            .map(|p| rule("10.0.0.0/8", EgressProto::Tcp, Some(8000 + p)))
            .collect();
        let err = compile(&rules).unwrap_err().to_string();
        assert!(err.contains("at most 4"), "{err}");
    }

    #[test]
    fn duplicate_rules_collapse() {
        let c = compile(&[
            rule("10.0.0.5", EgressProto::Tcp, Some(443)),
            rule("10.0.0.5/32", EgressProto::Tcp, Some(443)),
        ])
        .unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn encoding_matches_the_bpf_struct_layout() {
        let c = Compiled {
            prefix: 8,
            addr: 0x0a00_0000,
            ports: Some(vec![(6, 5151), (17, 53)]),
        };
        let key = encode_key(&c);
        assert_eq!(&key[0..4], &8u32.to_ne_bytes());
        assert_eq!(&key[4..8], &[10, 0, 0, 0]);
        let val = encode_val(&c);
        assert_eq!(val.len(), 16);
        assert_eq!(val[0], 2);
        assert_eq!(&val[1..5], &[6, 17, 0, 0]);
        assert_eq!(&val[8..10], &5151u16.to_ne_bytes());
        assert_eq!(&val[10..12], &53u16.to_ne_bytes());
        let any = encode_val(&Compiled {
            prefix: 0,
            addr: 0,
            ports: None,
        });
        assert_eq!(any, vec![0u8; 16]);
        assert_eq!(encode_cfg(EgressMode::Audit), [2, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn bpftool_formatted_entries_are_lifted() {
        let raw: Value = serde_json::from_str(
            r#"[{"key":["0x00"],"value":["0x02"],"formatted":{"key":0,"value":{"mode":2}}},
                {"key":{"daddr":1},"value":{"packets":3}}]"#,
        )
        .unwrap();
        let n = normalize_dump(raw);
        assert_eq!(n[0]["value"]["mode"], 2);
        assert_eq!(n[1]["value"]["packets"], 3);
    }

    #[test]
    fn config_strings_parse() {
        let p = config_default(
            "allowlist",
            &[
                "10.0.0.0/8".into(),
                "192.168.1.5:tcp/30151".into(),
                "0.0.0.0/0:udp/53".into(),
            ],
        )
        .unwrap();
        assert_eq!(p.mode, EgressMode::Allowlist);
        assert_eq!(p.rules[1].port, Some(30151));
        assert_eq!(p.rules[1].proto, EgressProto::Tcp);
        assert_eq!(p.rules[0].proto, EgressProto::Any);
        assert!(config_default("loose", &[]).is_err());
        assert!(config_default("allowlist", &["10.0.0.0/8:icmp/1".into()]).is_err());
        assert!(config_default("allowlist", &["10.0.0.0/8:tcp".into()]).is_err());
    }

    #[test]
    fn suggest_collapses_busy_subnets_and_keeps_singletons() {
        let flow = |dst: &str, port| ObservedFlow {
            dst: dst.into(),
            proto: EgressProto::Tcp,
            port,
            packets: 3,
            bytes: 180,
            audit: false,
            last_ns: 0,
        };
        let rules = suggest(&[
            flow("91.189.91.82", 80),
            flow("91.189.91.83", 80),
            flow("91.189.91.84", 80),
            flow("10.0.128.254", 5151),
        ]);
        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .any(|r| r.cidr == "91.189.91.0/24" && r.port == Some(80))
        );
        assert!(
            rules
                .iter()
                .any(|r| r.cidr == "10.0.128.254/32" && r.port == Some(5151))
        );
        // Everything suggested must validate.
        validate(&QemuEgressPolicy {
            mode: EgressMode::Allowlist,
            rules,
        })
        .unwrap();
    }

    #[test]
    fn effective_prefers_vm_then_node_default_then_config() {
        let dir = std::env::temp_dir().join(format!("qemu-egress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let id = uuid::Uuid::new_v4();
        let cfg = QemuEgressPolicy::default();
        assert_eq!(effective(&dir, id, &cfg).unwrap().1, PolicySource::Builtin);

        let from_config = QemuEgressPolicy {
            mode: EgressMode::Allowlist,
            rules: vec![rule("10.0.0.0/8", EgressProto::Tcp, Some(443))],
        };
        assert_eq!(
            effective(&dir, id, &from_config).unwrap().1,
            PolicySource::Config
        );

        let node = QemuEgressPolicy {
            mode: EgressMode::Open,
            rules: vec![],
        };
        save_defaults(&dir, &node).unwrap();
        assert_eq!(
            effective(&dir, id, &from_config).unwrap().1,
            PolicySource::NodeDefault
        );

        let vm = QemuEgressPolicy {
            mode: EgressMode::Audit,
            rules: vec![],
        };
        save_vm_policy(&dir, id, &vm).unwrap();
        let (p, src) = effective(&dir, id, &from_config).unwrap();
        assert_eq!((p.mode, src), (EgressMode::Audit, PolicySource::Vm));
        delete_vm_policy(&dir, id).unwrap();
        assert_eq!(
            effective(&dir, id, &from_config).unwrap().1,
            PolicySource::NodeDefault
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
