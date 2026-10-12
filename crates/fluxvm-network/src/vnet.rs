// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Virtual networks: isolated IPv4 networks (one VNI each) routed by
//! `bpf/fluxvm_vnet.bpf.c` and carried between hosts in Geneve.
//!
//! A host holds one copy of the programs and maps under
//! `<pin_root>/vnet/{progs,maps,links}`, one collect_md Geneve device
//! ([`GENEVE_DEV`]) and, per network, the last applied [`VnetConfig`] under
//! `<state_dir>/vnet/nets/<vni>.json`. A VM attaches through a bridge-less tap
//! recorded in `<state_dir>/vnet/ports/<uuid>.json` at prepare time; the
//! dataplane step then programs the maps and attaches `vnet_guest` to the tap.
//! A local attachment always wins over a remote endpoint for the same address,
//! so a migrating VM's FIB entry flips as soon as it lands.
//!
//! See docs/virtual-network.md.

use anyhow::{Context, Result, bail};
use fluxvm_core::{
    config::Config,
    model::{VnetAttach, VnetConfig, VnetStatus, parse_ipv4_cidr, vnet_gateway_mac},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::bpf_map::PinnedHashMap;

/// The host's collect_md Geneve device: every VNI shares it.
pub const GENEVE_DEV: &str = "fvgnv0";
/// IANA Geneve port.
pub const GENEVE_PORT: u16 = 6081;
/// Guest MTU that leaves room for the Geneve, UDP and IPv4 headers on a 1500-byte underlay.
pub const GUEST_MTU: u32 = 1450;

const OBJECT: &str = "fluxvm_vnet.bpf.o";
const PROG_GUEST: &str = "vnet_guest";
const PROG_TUNNEL: &str = "vnet_tunnel";
const PROG_UPLINK: &str = "vnet_uplink";
const MAPS: [&str; 8] = [
    "fluxvm_vnet_port",
    "fluxvm_vnet_net",
    "fluxvm_vnet_fib",
    "fluxvm_vnet_host",
    "fluxvm_vnet_fip",
    "fluxvm_vnet_fipr",
    "fluxvm_vnet_nat",
    "fluxvm_vnet_natf",
];
/// clsact preference for the legacy (non-TCX) attach of every vnet program.
const TC_PREF: &str = "49170";

/// Serializes map programming: the API and VM lifecycle both write the shared maps.
static LOCK: Mutex<()> = Mutex::new(());

// Wire encodings of the maps in bpf/fluxvm_vnet.bpf.c: native-endian u32
// fields, IPv4 addresses in network order. Keep in lockstep with the C structs.
const FIB_LOCAL: u32 = 1;
const FIB_REMOTE: u32 = 2;

/// `struct vnet_port { vni, ip, mac[6], pad[2] }` (16 B).
fn port_value(vni: u32, ip: Ipv4Addr, mac: [u8; 6]) -> [u8; 16] {
    let mut v = [0u8; 16];
    v[0..4].copy_from_slice(&vni.to_ne_bytes());
    v[4..8].copy_from_slice(&ip.octets());
    v[8..14].copy_from_slice(&mac);
    v
}

/// `struct vnet_net { net, mask, gateway, snat_ip, uplink_ifindex, pad }` (24 B).
fn net_value(
    net: Ipv4Addr,
    prefix: u8,
    gateway: Ipv4Addr,
    snat: Option<Ipv4Addr>,
    uplink: u32,
) -> [u8; 24] {
    let mut v = [0u8; 24];
    v[0..4].copy_from_slice(&net.octets());
    v[4..8].copy_from_slice(&mask(prefix).octets());
    v[8..12].copy_from_slice(&gateway.octets());
    v[12..16].copy_from_slice(&snat.unwrap_or(Ipv4Addr::UNSPECIFIED).octets());
    v[16..20].copy_from_slice(&uplink.to_ne_bytes());
    v
}

/// `struct vnet_fib_key { vni, ip }` (8 B); also the `fluxvm_vnet_fipr` key and `vnet_fip` value.
fn fib_key(vni: u32, ip: Ipv4Addr) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&vni.to_ne_bytes());
    k[4..8].copy_from_slice(&ip.octets());
    k
}

/// `struct vnet_fib { kind, ifindex, remote, mac[6], pad[2] }` (20 B).
fn fib_value(kind: u32, ifindex: u32, remote: Ipv4Addr, mac: [u8; 6]) -> [u8; 20] {
    let mut v = [0u8; 20];
    v[0..4].copy_from_slice(&kind.to_ne_bytes());
    v[4..8].copy_from_slice(&ifindex.to_ne_bytes());
    v[8..12].copy_from_slice(&remote.octets());
    v[12..18].copy_from_slice(&mac);
    v
}

/// `struct vnet_host { geneve_ifindex, pad }` (8 B).
fn host_value(geneve_ifindex: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[0..4].copy_from_slice(&geneve_ifindex.to_ne_bytes());
    v
}

fn mask(prefix: u8) -> Ipv4Addr {
    match prefix {
        0 => Ipv4Addr::UNSPECIFIED,
        p => Ipv4Addr::from(u32::MAX << (32 - u32::from(p.min(32)))),
    }
}

fn network_of(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) & u32::from(mask(prefix)))
}

fn ipv4(s: &str, what: &str) -> Result<Ipv4Addr> {
    s.parse()
        .with_context(|| format!("{what} {s:?} is not an IPv4 address"))
}

/// What a VM's vnet NIC needs to be (re)programmed, written at prepare time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortRecord {
    pub attach: VnetAttach,
    pub mac: String,
    pub tap: String,
    /// Tap ifindex last programmed into the maps (0: not yet).
    #[serde(default)]
    pub ifindex: u32,
    /// Pin root the maps were programmed under, so cleanup needs only the state dir.
    #[serde(default)]
    pub pin_root: Option<PathBuf>,
}

fn state_root(state_dir: &Path) -> PathBuf {
    state_dir.join("vnet")
}

fn port_path(state_dir: &Path, id: Uuid) -> PathBuf {
    state_root(state_dir)
        .join("ports")
        .join(format!("{}.json", id.simple()))
}

fn net_path(state_dir: &Path, vni: u32) -> PathBuf {
    state_root(state_dir)
        .join("nets")
        .join(format!("{vni}.json"))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Records that VM `id` attaches to a virtual network. Call from prepare.
pub fn record_port(state_dir: &Path, id: Uuid, rec: &PortRecord) -> Result<()> {
    write_json(&port_path(state_dir, id), rec).context("recording vnet attach")
}

/// The vnet attach recorded for `id`, if it is a vnet VM.
pub fn recorded_port(state_dir: &Path, id: Uuid) -> Option<PortRecord> {
    read_json(&port_path(state_dir, id))
}

fn local_ports(state_dir: &Path) -> Vec<(Uuid, PortRecord)> {
    let Ok(dir) = fs::read_dir(state_root(state_dir).join("ports")) else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = Uuid::parse_str(name.strip_suffix(".json")?).ok()?;
            Some((id, read_json(&e.path())?))
        })
        .collect()
}

/// Local attachments of `vni` (programmed or not), by guest address.
fn local_ips(state_dir: &Path, vni: u32) -> BTreeSet<Ipv4Addr> {
    local_ports(state_dir)
        .into_iter()
        .filter(|(_, r)| r.attach.vni == vni)
        .filter_map(|(_, r)| r.attach.ip.parse().ok())
        .collect()
}

fn applied(state_dir: &Path, vni: u32) -> Option<VnetConfig> {
    read_json(&net_path(state_dir, vni))
}

fn vnet_root(cfg: &Config) -> PathBuf {
    cfg.sandbox.dataplane.pin_root.join("vnet")
}

struct Maps {
    port: PinnedHashMap,
    net: PinnedHashMap,
    fib: PinnedHashMap,
    host: PinnedHashMap,
    fip: PinnedHashMap,
    fipr: PinnedHashMap,
}

impl Maps {
    fn open(root: &Path) -> Result<Self> {
        let open = |name: &str| {
            let p = root.join("maps").join(name);
            PinnedHashMap::open(&p).with_context(|| format!("opening {}", p.display()))
        };
        Ok(Self {
            port: open("fluxvm_vnet_port")?,
            net: open("fluxvm_vnet_net")?,
            fib: open("fluxvm_vnet_fib")?,
            host: open("fluxvm_vnet_host")?,
            fip: open("fluxvm_vnet_fip")?,
            fipr: open("fluxvm_vnet_fipr")?,
        })
    }
}

/// Loads `fluxvm_vnet.bpf.o` once per pin root; the maps outlive the programs
/// so a reload keeps every network's state.
fn ensure_loaded(cfg: &Config) -> Result<PathBuf> {
    let root = vnet_root(cfg);
    let progs = root.join("progs");
    let maps = root.join("maps");
    if [PROG_GUEST, PROG_TUNNEL, PROG_UPLINK]
        .iter()
        .all(|p| progs.join(p).exists())
        && MAPS.iter().all(|m| maps.join(m).exists())
    {
        return Ok(root);
    }
    let obj = cfg.sandbox.dataplane.bpf_object.with_file_name(OBJECT);
    if !obj.exists() {
        bail!(
            "the virtual network eBPF object does not exist at {} (scripts/build-ebpf.sh builds it)",
            obj.display()
        );
    }
    crate::ebpf::require_bpftool()?;
    let _ = crate::ebpf::raise_memlock();
    // A partial earlier load leaves some program pins, and loadall refuses to overwrite them.
    let _ = fs::remove_dir_all(&progs);
    fs::create_dir_all(&maps).with_context(|| format!("creating {}", maps.display()))?;
    fs::create_dir_all(root.join("links")).context("creating vnet link pin dir")?;
    let mut args: Vec<String> = vec![
        "prog".into(),
        "loadall".into(),
        obj.display().to_string(),
        progs.display().to_string(),
        "type".into(),
        "classifier".into(),
    ];
    for m in MAPS {
        let pin = maps.join(m);
        if pin.exists() {
            args.extend([
                "map".into(),
                "name".into(),
                m.into(),
                "pinned".into(),
                pin.display().to_string(),
            ]);
        }
    }
    args.extend(["pinmaps".into(), maps.display().to_string()]);
    crate::ebpf::run("bpftool", &args).context("loading the virtual network programs")?;
    info!(root = %root.display(), "loaded FluxVM virtual network programs");
    Ok(root)
}

/// Attaches `prog_pin` to `iface` ingress: TCX when available (at the head of
/// the chain when `first`), else clsact at [`TC_PREF`].
fn attach_prog(iface: &str, prog_pin: &Path, link_pin: &Path, first: bool) -> Result<()> {
    let pref = crate::tcx::Preference::from_env()?;
    if pref != crate::tcx::Preference::Off {
        let _ = crate::tcx::detach(link_pin);
        let attached = if first {
            crate::tcx::attach_first(iface, prog_pin, link_pin)
        } else {
            crate::tcx::attach(iface, prog_pin, link_pin)
        };
        match attached {
            Ok(_) => return Ok(()),
            Err(e) if pref == crate::tcx::Preference::Required => {
                return Err(e)
                    .with_context(|| format!("attaching required TCX program on {iface}"));
            }
            Err(e) => {
                warn!(%iface, error = %e, "TCX unavailable for a virtual network program; using clsact/tc")
            }
        }
    }
    if first {
        warn!(
            %iface,
            "legacy tc attach on an uplink runs after any TCX program there; one that ends \
             with TCX_PASS (Cilium does) hides floating-IP and SNAT replies from FluxVM"
        );
    }
    crate::ebpf::require_tc()?;
    crate::ebpf::ensure_clsact(iface)?;
    crate::ebpf::run(
        "tc",
        &[
            "filter".into(),
            "replace".into(),
            "dev".into(),
            iface.into(),
            "ingress".into(),
            "pref".into(),
            TC_PREF.into(),
            "handle".into(),
            "1".into(),
            "bpf".into(),
            "da".into(),
            "pinned".into(),
            prog_pin.display().to_string(),
        ],
    )
    .with_context(|| format!("attaching {} to {iface}", prog_pin.display()))
}

fn detach_prog(iface: &str, link_pin: &Path) {
    if link_pin.exists() {
        let _ = crate::tcx::detach(link_pin);
        return;
    }
    let _ = crate::ebpf::run(
        "tc",
        &[
            "filter".into(),
            "del".into(),
            "dev".into(),
            iface.into(),
            "ingress".into(),
            "pref".into(),
            TC_PREF.into(),
            "handle".into(),
            "1".into(),
            "bpf".into(),
        ],
    );
}

fn ip_cmd(args: &[&str]) -> Result<()> {
    let out = crate::netns_scope::command("ip")
        .args(args)
        .output()
        .context("running ip")?;
    if !out.status.success() {
        bail!(
            "ip {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Creates the collect_md Geneve device if needed, attaches `vnet_tunnel` to
/// it and tells the programs its ifindex.
fn ensure_geneve(root: &Path, maps: &Maps) -> Result<()> {
    let link = root.join("links").join("tunnel");
    let created = match crate::ebpf::read_ifindex(GENEVE_DEV) {
        Ok(_) => false,
        Err(_) => {
            let port = GENEVE_PORT.to_string();
            ip_cmd(&[
                "link", "add", GENEVE_DEV, "type", "geneve", "external", "dstport", &port,
            ])
            .context("creating the Geneve device")?;
            true
        }
    };
    ip_cmd(&["link", "set", "dev", GENEVE_DEV, "up"])?;
    let ifindex = crate::ebpf::read_ifindex(GENEVE_DEV)?;
    if created || !link.exists() {
        attach_prog(
            GENEVE_DEV,
            &root.join("progs").join(PROG_TUNNEL),
            &link,
            false,
        )
        .context("attaching the Geneve receive program")?;
    }
    maps.host
        .update(&0u32.to_ne_bytes(), &host_value(ifindex))
        .context("writing fluxvm_vnet_host")?;
    Ok(())
}

fn uplink_link(root: &Path, uplink: &str) -> PathBuf {
    root.join("links").join(format!("uplink-{uplink}"))
}

/// Detaches the uplink program from every interface no applied network
/// names any more, so an uplink does not keep a program once the last
/// network using it is gone.
fn release_uplinks(cfg: &Config, root: &Path) {
    let Ok(links) = fs::read_dir(root.join("links")) else {
        return;
    };
    let in_use: BTreeSet<String> = list(cfg)
        .into_iter()
        .filter_map(|s| s.config.uplink)
        .collect();
    for link in links.flatten() {
        let name = link.file_name().to_string_lossy().into_owned();
        let Some(iface) = name.strip_prefix("uplink-") else {
            continue;
        };
        if !in_use.contains(iface) {
            detach_prog(iface, &link.path());
            info!(
                uplink = iface,
                "detached the virtual network uplink program"
            );
        }
    }
}

/// Applies `c` on this host and returns the resulting status. Endpoints that
/// live on this host (a recorded local attachment) are left to the attach.
pub fn apply_config(cfg: &Config, c: &VnetConfig) -> Result<VnetStatus> {
    c.validate().map_err(|e| anyhow::anyhow!(e))?;
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = ensure_loaded(cfg)?;
    let maps = Maps::open(&root)?;
    ensure_geneve(&root, &maps)?;
    let (net, prefix) = parse_ipv4_cidr(&c.cidr).map_err(|e| anyhow::anyhow!(e))?;
    let gateway = ipv4(&c.gateway, "gateway")?;
    let snat = c
        .snat_address
        .as_deref()
        .map(|s| ipv4(s, "SNAT address"))
        .transpose()?;
    let uplink = match &c.uplink {
        Some(name) => {
            let ifindex = crate::ebpf::read_ifindex(name)
                .with_context(|| format!("resolving uplink {name}"))?;
            let link = uplink_link(&root, name);
            if !link.exists() {
                attach_prog(name, &root.join("progs").join(PROG_UPLINK), &link, true)
                    .with_context(|| format!("attaching the uplink program to {name}"))?;
            }
            ifindex
        }
        None => 0,
    };
    maps.net
        .update(
            &c.vni.to_ne_bytes(),
            &net_value(net, prefix, gateway, snat, uplink),
        )
        .context("writing fluxvm_vnet_net")?;

    let locals = local_ips(&cfg.state_dir, c.vni);
    let prev = applied(&cfg.state_dir, c.vni);
    let mut remote: BTreeMap<Ipv4Addr, (Ipv4Addr, [u8; 6])> = BTreeMap::new();
    for e in &c.endpoints {
        let ip = ipv4(&e.ip, "endpoint")?;
        if locals.contains(&ip) {
            continue;
        }
        let node = ipv4(&e.node_ip, "endpoint node")?;
        remote.insert(ip, (node, crate::direct::parse_mac(&e.mac)?));
    }
    let fips: BTreeMap<Ipv4Addr, Ipv4Addr> = c
        .floating_ips
        .iter()
        .map(|f| {
            Ok((
                ipv4(&f.address, "floating IP")?,
                ipv4(&f.ip, "floating IP target")?,
            ))
        })
        .collect::<Result<_>>()?;

    if let Some(prev) = &prev {
        for e in &prev.endpoints {
            let Ok(ip) = e.ip.parse::<Ipv4Addr>() else {
                continue;
            };
            if !remote.contains_key(&ip) && !locals.contains(&ip) {
                maps.fib.delete(&fib_key(c.vni, ip))?;
            }
        }
        for f in &prev.floating_ips {
            let (Ok(public), Ok(ip)) = (f.address.parse::<Ipv4Addr>(), f.ip.parse::<Ipv4Addr>())
            else {
                continue;
            };
            if fips.get(&public) != Some(&ip) {
                maps.fip.delete(&public.octets())?;
                maps.fipr.delete(&fib_key(c.vni, ip))?;
            }
        }
    }
    for (ip, (node, mac)) in &remote {
        maps.fib
            .update(&fib_key(c.vni, *ip), &fib_value(FIB_REMOTE, 0, *node, *mac))
            .with_context(|| format!("writing the FIB entry for {ip}"))?;
    }
    for (public, ip) in &fips {
        maps.fip
            .update(&public.octets(), &fib_key(c.vni, *ip))
            .with_context(|| format!("writing floating IP {public}"))?;
        maps.fipr
            .update(&fib_key(c.vni, *ip), &public.octets())
            .with_context(|| format!("writing floating IP {public}"))?;
    }
    write_json(&net_path(&cfg.state_dir, c.vni), c)?;
    release_uplinks(cfg, &root);
    info!(
        vni = c.vni,
        remote = remote.len(),
        floating_ips = fips.len(),
        "applied virtual network"
    );
    Ok(status_of(c, &locals, remote.len()))
}

fn status_of(c: &VnetConfig, locals: &BTreeSet<Ipv4Addr>, remote: usize) -> VnetStatus {
    let mac = vnet_gateway_mac(c.vni);
    VnetStatus {
        config: c.clone(),
        local_endpoints: locals.iter().map(Ipv4Addr::to_string).collect(),
        remote_endpoints: remote,
        gateway_mac: mac
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    }
}

/// The applied configuration of `vni` on this host.
pub fn status(cfg: &Config, vni: u32) -> Option<VnetStatus> {
    let c = applied(&cfg.state_dir, vni)?;
    let locals = local_ips(&cfg.state_dir, vni);
    let remote = c
        .endpoints
        .iter()
        .filter(|e| {
            e.ip.parse()
                .map(|ip| !locals.contains(&ip))
                .unwrap_or(false)
        })
        .count();
    Some(status_of(&c, &locals, remote))
}

/// Every virtual network applied on this host, by VNI.
pub fn list(cfg: &Config) -> Vec<VnetStatus> {
    let Ok(dir) = fs::read_dir(state_root(&cfg.state_dir).join("nets")) else {
        return Vec::new();
    };
    let mut vnis: Vec<u32> = dir
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .into_string()
                .ok()?
                .strip_suffix(".json")?
                .parse()
                .ok()
        })
        .collect();
    vnis.sort_unstable();
    vnis.into_iter().filter_map(|v| status(cfg, v)).collect()
}

/// Removes `vni` from this host. Refused while a local VM is attached; `false` when unknown.
pub fn remove_config(cfg: &Config, vni: u32) -> Result<bool> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(prev) = applied(&cfg.state_dir, vni) else {
        return Ok(false);
    };
    let locals = local_ips(&cfg.state_dir, vni);
    if !locals.is_empty() {
        bail!(
            "virtual network {vni} still has {} VM(s) attached on this host",
            locals.len()
        );
    }
    let maps = Maps::open(&vnet_root(cfg))?;
    for e in &prev.endpoints {
        if let Ok(ip) = e.ip.parse::<Ipv4Addr>() {
            maps.fib.delete(&fib_key(vni, ip))?;
        }
    }
    for f in &prev.floating_ips {
        if let (Ok(public), Ok(ip)) = (f.address.parse::<Ipv4Addr>(), f.ip.parse::<Ipv4Addr>()) {
            maps.fip.delete(&public.octets())?;
            maps.fipr.delete(&fib_key(vni, ip))?;
        }
    }
    maps.net.delete(&vni.to_ne_bytes())?;
    fs::remove_file(net_path(&cfg.state_dir, vni)).context("removing vnet state")?;
    release_uplinks(cfg, &vnet_root(cfg));
    info!(vni, "removed virtual network");
    Ok(true)
}

/// Programs the maps for VM `id`'s recorded vnet NIC and attaches the guest
/// program to its tap. A no-op for a VM without a vnet record.
pub fn attach_vm(cfg: &Config, id: Uuid) -> Result<bool> {
    let Some(mut rec) = recorded_port(&cfg.state_dir, id) else {
        return Ok(false);
    };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let a = &rec.attach;
    let ip = ipv4(&a.ip, "guest IP")?;
    let gateway = ipv4(&a.gateway, "gateway")?;
    let mac = crate::direct::parse_mac(&rec.mac)?;
    let root = ensure_loaded(cfg)?;
    let maps = Maps::open(&root)?;
    ensure_geneve(&root, &maps)?;
    let ifindex = crate::ebpf::read_ifindex(&rec.tap)
        .with_context(|| format!("resolving vnet tap {}", rec.tap))?;
    if rec.ifindex != 0 && rec.ifindex != ifindex {
        maps.port.delete(&rec.ifindex.to_ne_bytes())?;
    }
    maps.port
        .update(&ifindex.to_ne_bytes(), &port_value(a.vni, ip, mac))
        .context("writing fluxvm_vnet_port")?;
    maps.fib
        .update(
            &fib_key(a.vni, ip),
            &fib_value(FIB_LOCAL, ifindex, Ipv4Addr::UNSPECIFIED, mac),
        )
        .context("writing the local FIB entry")?;
    // Until the network's configuration arrives the VM can still reach local peers.
    if applied(&cfg.state_dir, a.vni).is_none() {
        maps.net
            .update(
                &a.vni.to_ne_bytes(),
                &net_value(network_of(ip, a.prefix_len), a.prefix_len, gateway, None, 0),
            )
            .context("writing fluxvm_vnet_net")?;
    }
    // Policy reconciles re-enter here; a tap that kept its ifindex and link needs no re-attach.
    let link = vm_link(&root, id);
    if rec.ifindex != ifindex || !link.exists() {
        attach_prog(&rec.tap, &root.join("progs").join(PROG_GUEST), &link, false)
            .context("attaching the virtual network guest program")?;
    }
    rec.ifindex = ifindex;
    rec.pin_root = Some(cfg.sandbox.dataplane.pin_root.clone());
    record_port(&cfg.state_dir, id, &rec)?;
    info!(%id, vni = rec.attach.vni, ip = %ip, tap = %rec.tap, "attached VM to virtual network");
    Ok(true)
}

fn vm_link(root: &Path, id: Uuid) -> PathBuf {
    root.join("links").join(format!("vm-{}", id.simple()))
}

/// Removes VM `id` from its virtual network and forgets the record. Best
/// effort: the tap is usually already gone. When the network still lists the
/// address on another host (a migration source) the remote route comes back.
pub fn detach_vm(state_dir: &Path, id: Uuid) {
    let Some(rec) = recorded_port(state_dir, id) else {
        return;
    };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pin_root) = &rec.pin_root {
        let root = pin_root.join("vnet");
        detach_prog(&rec.tap, &vm_link(&root, id));
        if let (Ok(maps), Ok(ip)) = (Maps::open(&root), rec.attach.ip.parse::<Ipv4Addr>()) {
            let vni = rec.attach.vni;
            if rec.ifindex != 0 {
                let _ = maps.port.delete(&rec.ifindex.to_ne_bytes());
            }
            let key = fib_key(vni, ip);
            let remote = applied(state_dir, vni).and_then(|c| {
                let e = c.endpoints.into_iter().find(|e| e.ip == rec.attach.ip)?;
                Some((
                    e.node_ip.parse::<Ipv4Addr>().ok()?,
                    crate::direct::parse_mac(&e.mac).ok()?,
                ))
            });
            let _ = match remote {
                Some((node, mac)) => maps.fib.update(&key, &fib_value(FIB_REMOTE, 0, node, mac)),
                None => maps.fib.delete(&key),
            };
        }
    }
    let _ = fs::remove_file(port_path(state_dir, id));
    info!(%id, vni = rec.attach.vni, "detached VM from virtual network");
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::model::{VnetEndpoint, VnetFloatingIp};

    #[test]
    fn map_values_match_the_c_struct_layouts() {
        let ip = Ipv4Addr::new(10, 0, 0, 5);
        let mac = [2, 0, 0, 0, 0, 5];
        let p = port_value(0x0001_0203, ip, mac);
        assert_eq!(&p[0..4], &0x0001_0203u32.to_ne_bytes());
        assert_eq!(&p[4..8], &[10, 0, 0, 5]);
        assert_eq!(&p[8..14], &mac);
        assert_eq!(&p[14..], &[0, 0]);

        let n = net_value(
            Ipv4Addr::new(10, 0, 0, 0),
            24,
            Ipv4Addr::new(10, 0, 0, 1),
            Some(Ipv4Addr::new(203, 0, 113, 7)),
            9,
        );
        assert_eq!(&n[0..4], &[10, 0, 0, 0]);
        assert_eq!(&n[4..8], &[255, 255, 255, 0]);
        assert_eq!(&n[8..12], &[10, 0, 0, 1]);
        assert_eq!(&n[12..16], &[203, 0, 113, 7]);
        assert_eq!(&n[16..20], &9u32.to_ne_bytes());
        assert_eq!(&n[20..], &[0; 4]);
        let no_snat = net_value(Ipv4Addr::new(10, 0, 0, 0), 24, ip, None, 0);
        assert_eq!(&no_snat[12..20], &[0; 8], "0 means no SNAT and no uplink");

        assert_eq!(fib_key(7, ip), {
            let mut k = [0u8; 8];
            k[..4].copy_from_slice(&7u32.to_ne_bytes());
            k[4..].copy_from_slice(&[10, 0, 0, 5]);
            k
        });
        let f = fib_value(FIB_REMOTE, 0, Ipv4Addr::new(192, 0, 2, 9), mac);
        assert_eq!(&f[0..4], &2u32.to_ne_bytes());
        assert_eq!(&f[4..8], &[0; 4]);
        assert_eq!(&f[8..12], &[192, 0, 2, 9]);
        assert_eq!(&f[12..18], &mac);
        assert_eq!(&f[18..], &[0, 0]);
        assert_eq!(&host_value(4)[..4], &4u32.to_ne_bytes());
    }

    #[test]
    fn masks_and_networks() {
        assert_eq!(mask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(mask(8), Ipv4Addr::new(255, 0, 0, 0));
        assert_eq!(mask(30), Ipv4Addr::new(255, 255, 255, 252));
        assert_eq!(mask(0), Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            network_of(Ipv4Addr::new(10, 1, 2, 3), 16),
            Ipv4Addr::new(10, 1, 0, 0)
        );
    }

    fn rec(vni: u32, ip: &str) -> PortRecord {
        PortRecord {
            attach: VnetAttach {
                vni,
                ip: ip.into(),
                prefix_len: 24,
                gateway: "10.0.0.1".into(),
            },
            mac: "02:00:00:00:00:05".into(),
            tap: "vn0".into(),
            ifindex: 0,
            pin_root: None,
        }
    }

    #[test]
    fn port_records_round_trip_and_feed_local_ips() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(recorded_port(d.path(), a), None);
        record_port(d.path(), a, &rec(5, "10.0.0.5")).unwrap();
        record_port(d.path(), b, &rec(5, "10.0.0.6")).unwrap();
        record_port(d.path(), c, &rec(6, "10.0.0.5")).unwrap();
        assert_eq!(recorded_port(d.path(), a), Some(rec(5, "10.0.0.5")));
        assert_eq!(
            local_ips(d.path(), 5),
            BTreeSet::from([Ipv4Addr::new(10, 0, 0, 5), Ipv4Addr::new(10, 0, 0, 6)])
        );
        assert_eq!(local_ips(d.path(), 6).len(), 1);
        assert!(local_ips(d.path(), 7).is_empty());
        // Detaching a VM with no programmed maps only forgets the record.
        detach_vm(d.path(), a);
        assert_eq!(recorded_port(d.path(), a), None);
        assert_eq!(local_ips(d.path(), 5).len(), 1);
    }

    #[test]
    fn status_counts_only_remote_endpoints() {
        let c = VnetConfig {
            vni: 5,
            cidr: "10.0.0.0/24".into(),
            gateway: "10.0.0.1".into(),
            uplink: None,
            snat_address: None,
            floating_ips: vec![VnetFloatingIp {
                address: "203.0.113.5".into(),
                ip: "10.0.0.5".into(),
            }],
            endpoints: vec![
                VnetEndpoint {
                    ip: "10.0.0.5".into(),
                    mac: "02:00:00:00:00:05".into(),
                    node_ip: "192.0.2.1".into(),
                },
                VnetEndpoint {
                    ip: "10.0.0.6".into(),
                    mac: "02:00:00:00:00:06".into(),
                    node_ip: "192.0.2.2".into(),
                },
            ],
        };
        let locals = BTreeSet::from([Ipv4Addr::new(10, 0, 0, 5)]);
        let s = status_of(&c, &locals, 1);
        assert_eq!(s.local_endpoints, vec!["10.0.0.5".to_string()]);
        assert_eq!(s.remote_endpoints, 1);
        assert_eq!(s.gateway_mac, "02:66:76:00:00:05");
    }
}
