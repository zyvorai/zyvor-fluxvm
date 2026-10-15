// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! FluxMesh: the host-to-host personal-cloud network control plane for FluxVM.
//!
//! This crate deliberately does not own VM-edge policy (Network Fabric) or
//! node-local VIP load balancing (Service Fabric). It owns the missing layer
//! between hosts: stable overlay addressing, node identity, route intent,
//! NAT observations, direct-vs-relay path selection, durable state and a
//! WireGuard command plan that a privileged host service can apply.
//!
//! The durable state model stays intentionally small and portable across Linux
//! and macOS while transport, enrollment and platform backends evolve independently.

pub mod acl;
pub mod acme;
pub mod agent;
pub mod catalog;
pub mod cloud;
pub mod ddns;
pub mod diagnose;
pub mod dns;
pub mod dns01;
pub mod dns_ha;
pub mod dnssec;
pub mod edge;
pub mod enroll;
pub mod enroll_tls;
pub mod fabric;
pub mod gateway;
pub mod http;
pub mod http3;
pub mod ingress;
pub mod ipv6;
pub mod keydist;
pub mod macos;
pub mod mdns;
pub mod multihome;
pub mod pd;
pub mod public;
pub mod public_relay;
pub mod reconcile;
pub mod relay;
pub mod relay_ha;
pub mod router;
pub mod service;
pub mod share;
pub mod stun;
pub mod telemetry;
pub mod tls_relay;
pub mod trace;
pub mod wan;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATE_VERSION: u32 = 1;
pub const DEFAULT_POOL: &str = "100.96.0.0/11";
pub const DEFAULT_WG_PORT: u16 = 51820;

#[derive(Debug)]
pub enum MeshError {
    InvalidNodeId(String),
    InvalidPrefix(String),
    InvalidState(String),
    UnknownNode(String),
    DuplicateRoute(String),
    PoolExhausted,
    Io(io::Error),
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNodeId(v) => write!(f, "invalid node id: {v}"),
            Self::InvalidPrefix(v) => write!(f, "invalid IPv4 prefix: {v}"),
            Self::InvalidState(v) => write!(f, "invalid FluxMesh state: {v}"),
            Self::UnknownNode(v) => write!(f, "unknown FluxMesh node: {v}"),
            Self::DuplicateRoute(v) => write!(f, "route already exists: {v}"),
            Self::PoolExhausted => write!(f, "overlay IPv4 pool exhausted"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MeshError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for MeshError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, MeshError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ipv4Prefix {
    network: u32,
    prefix: u8,
}

impl Ipv4Prefix {
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self> {
        if prefix > 32 {
            return Err(MeshError::InvalidPrefix(format!("{addr}/{prefix}")));
        }
        let mask = prefix_mask(prefix);
        Ok(Self {
            network: u32::from(addr) & mask,
            prefix,
        })
    }

    pub fn prefix_len(self) -> u8 {
        self.prefix
    }

    pub fn network(self) -> Ipv4Addr {
        Ipv4Addr::from(self.network)
    }

    pub fn contains(self, addr: Ipv4Addr) -> bool {
        let mask = prefix_mask(self.prefix);
        (u32::from(addr) & mask) == self.network
    }

    pub fn usable_host_count(self) -> u64 {
        match self.prefix {
            32 => 1,
            31 => 2,
            p => (1u64 << (32 - p)) - 2,
        }
    }

    fn first_usable(self) -> u32 {
        if self.prefix >= 31 {
            self.network
        } else {
            self.network + 1
        }
    }

    fn last_usable(self) -> u32 {
        let size = if self.prefix == 32 {
            1
        } else {
            1u64 << (32 - self.prefix)
        };
        let last = self.network as u64 + size - 1;
        if self.prefix >= 31 {
            last as u32
        } else {
            (last - 1) as u32
        }
    }
}

impl fmt::Display for Ipv4Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network(), self.prefix)
    }
}

impl FromStr for Ipv4Prefix {
    type Err = MeshError;

    fn from_str(value: &str) -> Result<Self> {
        let (addr, prefix) = value
            .split_once('/')
            .ok_or_else(|| MeshError::InvalidPrefix(value.to_owned()))?;
        let addr = addr
            .parse::<Ipv4Addr>()
            .map_err(|_| MeshError::InvalidPrefix(value.to_owned()))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| MeshError::InvalidPrefix(value.to_owned()))?;
        Self::new(addr, prefix)
    }
}

fn prefix_mask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        32 => u32::MAX,
        n => u32::MAX << (32 - n),
    }
}

fn validate_node_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidNodeId(value.to_owned()));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointKind {
    Lan,
    PublicIpv6,
    PublicUdp,
    PeerRelay,
    Relay443,
}

impl EndpointKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::PublicIpv6 => "public_ipv6",
            Self::PublicUdp => "public_udp",
            Self::PeerRelay => "peer_relay",
            Self::Relay443 => "relay443",
        }
    }
}

impl FromStr for EndpointKind {
    type Err = MeshError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "lan" => Ok(Self::Lan),
            "public_ipv6" | "ipv6" => Ok(Self::PublicIpv6),
            "public_udp" | "udp" => Ok(Self::PublicUdp),
            "peer_relay" => Ok(Self::PeerRelay),
            "relay443" | "relay" => Ok(Self::Relay443),
            _ => Err(MeshError::InvalidState(format!(
                "unknown endpoint kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub kind: EndpointKind,
    pub addr: SocketAddr,
    pub priority: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub id: String,
    pub hostname: String,
    pub public_key: String,
    pub overlay_ip: Ipv4Addr,
    pub endpoints: Vec<Endpoint>,
    pub advertised_subnets: Vec<Ipv4Prefix>,
    pub relay_capable: bool,
    pub last_seen_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Host,
    VmSubnet,
    LanSubnet,
    Exit,
}

impl RouteKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::VmSubnet => "vm_subnet",
            Self::LanSubnet => "lan_subnet",
            Self::Exit => "exit",
        }
    }
}

impl FromStr for RouteKind {
    type Err = MeshError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "host" => Ok(Self::Host),
            "vm_subnet" | "vm" => Ok(Self::VmSubnet),
            "lan_subnet" | "lan" => Ok(Self::LanSubnet),
            "exit" => Ok(Self::Exit),
            _ => Err(MeshError::InvalidState(format!(
                "unknown route kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteAdvertisement {
    pub prefix: Ipv4Prefix,
    pub via_node: String,
    pub kind: RouteKind,
    pub metric: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayIpam {
    pub pool: Ipv4Prefix,
    allocations: BTreeMap<String, Ipv4Addr>,
}

impl OverlayIpam {
    pub fn new(pool: Ipv4Prefix) -> Self {
        Self {
            pool,
            allocations: BTreeMap::new(),
        }
    }

    pub fn allocations(&self) -> &BTreeMap<String, Ipv4Addr> {
        &self.allocations
    }

    pub fn allocate(&mut self, node_id: &str) -> Result<Ipv4Addr> {
        validate_node_id(node_id)?;
        if let Some(ip) = self.allocations.get(node_id) {
            return Ok(*ip);
        }

        let count = self.pool.usable_host_count();
        if count == 0 {
            return Err(MeshError::PoolExhausted);
        }
        let first = self.pool.first_usable();
        let last = self.pool.last_usable();
        let used: BTreeSet<u32> = self.allocations.values().copied().map(u32::from).collect();
        if used.len() as u64 >= count {
            return Err(MeshError::PoolExhausted);
        }

        let start = fnv1a64(node_id.as_bytes()) % count;
        for offset in 0..count {
            let candidate = first as u64 + ((start + offset) % count);
            if candidate > u64::from(last) {
                continue;
            }
            let candidate = candidate as u32;
            if !used.contains(&candidate) {
                let ip = Ipv4Addr::from(candidate);
                self.allocations.insert(node_id.to_owned(), ip);
                return Ok(ip);
            }
        }
        Err(MeshError::PoolExhausted)
    }

    fn restore(&mut self, node_id: String, ip: Ipv4Addr) -> Result<()> {
        validate_node_id(&node_id)?;
        if !self.pool.contains(ip) {
            return Err(MeshError::InvalidState(format!(
                "{ip} is outside overlay pool {}",
                self.pool
            )));
        }
        if self
            .allocations
            .iter()
            .any(|(id, existing)| id != &node_id && existing == &ip)
        {
            return Err(MeshError::InvalidState(format!(
                "duplicate overlay address {ip}"
            )));
        }
        self.allocations.insert(node_id, ip);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatType {
    Unknown,
    EndpointIndependent,
    Symmetric,
}

impl fmt::Display for NatType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::EndpointIndependent => write!(f, "endpoint-independent"),
            Self::Symmetric => write!(f, "symmetric"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NatObservation {
    pub observer: String,
    pub mapped_addr: SocketAddr,
}

pub fn classify_nat(observations: &[NatObservation]) -> NatType {
    if observations.len() < 2 {
        return NatType::Unknown;
    }
    let first = observations[0].mapped_addr;
    if observations.iter().all(|o| o.mapped_addr == first) {
        NatType::EndpointIndependent
    } else {
        NatType::Symmetric
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathSignals {
    pub lan_reachable: bool,
    pub public_ipv6_reachable: bool,
    pub direct_udp_reachable: bool,
    pub peer_relay_reachable: bool,
    pub relay443_reachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCandidate {
    pub kind: EndpointKind,
    pub endpoint: Option<SocketAddr>,
    pub score: u16,
    pub reason: &'static str,
}

pub fn select_paths(node: &NodeRecord, signals: &PathSignals) -> Vec<PathCandidate> {
    let mut out = Vec::new();
    let mut add = |kind: EndpointKind, enabled: bool, score: u16, reason: &'static str| {
        if !enabled {
            return;
        }
        let endpoint = node
            .endpoints
            .iter()
            .filter(|e| e.kind == kind)
            .min_by_key(|e| e.priority)
            .map(|e| e.addr);
        if endpoint.is_some() || matches!(kind, EndpointKind::PeerRelay | EndpointKind::Relay443) {
            out.push(PathCandidate {
                kind,
                endpoint,
                score,
                reason,
            });
        }
    };

    add(
        EndpointKind::Lan,
        signals.lan_reachable,
        10,
        "same-LAN direct path",
    );
    add(
        EndpointKind::PublicIpv6,
        signals.public_ipv6_reachable,
        20,
        "public IPv6 direct path",
    );
    add(
        EndpointKind::PublicUdp,
        signals.direct_udp_reachable,
        30,
        "UDP hole-punched direct path",
    );
    add(
        EndpointKind::PeerRelay,
        signals.peer_relay_reachable,
        60,
        "trusted mesh peer relay",
    );
    add(
        EndpointKind::Relay443,
        signals.relay443_reachable,
        90,
        "HTTPS-compatible relay fallback",
    );
    out.sort_by_key(|p| p.score);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshState {
    pub version: u32,
    pub local_node: String,
    pub ipam: OverlayIpam,
    pub nodes: BTreeMap<String, NodeRecord>,
    pub routes: Vec<RouteAdvertisement>,
}

impl MeshState {
    pub fn new(
        pool: Ipv4Prefix,
        local_node: impl Into<String>,
        hostname: impl Into<String>,
        public_key: impl Into<String>,
    ) -> Result<Self> {
        let local_node = local_node.into();
        validate_node_id(&local_node)?;
        let mut ipam = OverlayIpam::new(pool);
        let ip = ipam.allocate(&local_node)?;
        let node = NodeRecord {
            id: local_node.clone(),
            hostname: hostname.into(),
            public_key: public_key.into(),
            overlay_ip: ip,
            endpoints: Vec::new(),
            advertised_subnets: Vec::new(),
            relay_capable: false,
            last_seen_ms: now_ms(),
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(local_node.clone(), node);
        Ok(Self {
            version: STATE_VERSION,
            local_node,
            ipam,
            nodes,
            routes: Vec::new(),
        })
    }

    pub fn add_node(
        &mut self,
        node_id: impl Into<String>,
        hostname: impl Into<String>,
        public_key: impl Into<String>,
        relay_capable: bool,
    ) -> Result<Ipv4Addr> {
        let node_id = node_id.into();
        validate_node_id(&node_id)?;
        if let Some(node) = self.nodes.get_mut(&node_id) {
            node.hostname = hostname.into();
            node.public_key = public_key.into();
            node.relay_capable = relay_capable;
            node.last_seen_ms = now_ms();
            return Ok(node.overlay_ip);
        }
        let ip = self.ipam.allocate(&node_id)?;
        self.nodes.insert(
            node_id.clone(),
            NodeRecord {
                id: node_id,
                hostname: hostname.into(),
                public_key: public_key.into(),
                overlay_ip: ip,
                endpoints: Vec::new(),
                advertised_subnets: Vec::new(),
                relay_capable,
                last_seen_ms: now_ms(),
            },
        );
        Ok(ip)
    }

    pub fn node(&self, node_id: &str) -> Result<&NodeRecord> {
        self.nodes
            .get(node_id)
            .ok_or_else(|| MeshError::UnknownNode(node_id.to_owned()))
    }

    pub fn node_mut(&mut self, node_id: &str) -> Result<&mut NodeRecord> {
        self.nodes
            .get_mut(node_id)
            .ok_or_else(|| MeshError::UnknownNode(node_id.to_owned()))
    }

    pub fn add_endpoint(&mut self, node_id: &str, endpoint: Endpoint) -> Result<()> {
        let node = self.node_mut(node_id)?;
        if let Some(existing) = node
            .endpoints
            .iter_mut()
            .find(|e| e.kind == endpoint.kind && e.addr == endpoint.addr)
        {
            existing.priority = endpoint.priority;
        } else {
            node.endpoints.push(endpoint);
        }
        node.endpoints.sort_by_key(|e| e.priority);
        node.last_seen_ms = now_ms();
        Ok(())
    }

    pub fn advertise_route(&mut self, route: RouteAdvertisement) -> Result<()> {
        self.node(&route.via_node)?;
        if self
            .routes
            .iter()
            .any(|r| r.prefix == route.prefix && r.via_node == route.via_node)
        {
            return Err(MeshError::DuplicateRoute(format!(
                "{} via {}",
                route.prefix, route.via_node
            )));
        }
        if matches!(route.kind, RouteKind::LanSubnet | RouteKind::VmSubnet) {
            let node = self.node_mut(&route.via_node)?;
            if !node.advertised_subnets.contains(&route.prefix) {
                node.advertised_subnets.push(route.prefix);
                node.advertised_subnets
                    .sort_by_key(|p| (std::cmp::Reverse(p.prefix_len()), p.network()));
            }
        }
        self.routes.push(route);
        self.routes.sort_by_key(|r| {
            (
                std::cmp::Reverse(r.prefix.prefix_len()),
                r.metric,
                r.via_node.clone(),
            )
        });
        Ok(())
    }

    pub fn withdraw_route(&mut self, prefix: Ipv4Prefix, via_node: &str) -> bool {
        let before = self.routes.len();
        self.routes
            .retain(|r| !(r.prefix == prefix && r.via_node == via_node));
        if let Some(node) = self.nodes.get_mut(via_node) {
            node.advertised_subnets.retain(|p| *p != prefix);
        }
        before != self.routes.len()
    }

    pub fn route_for(&self, addr: Ipv4Addr) -> Option<&RouteAdvertisement> {
        self.routes
            .iter()
            .filter(|r| r.prefix.contains(addr))
            .min_by_key(|r| (std::cmp::Reverse(r.prefix.prefix_len()), r.metric))
    }

    pub fn best_paths(&self, node_id: &str, signals: &PathSignals) -> Result<Vec<PathCandidate>> {
        Ok(select_paths(self.node(node_id)?, signals))
    }

    pub fn wireguard_plan(
        &self,
        interface: impl Into<String>,
        listen_port: u16,
        private_key_file: impl Into<PathBuf>,
    ) -> Result<WireGuardPlan> {
        let local = self.node(&self.local_node)?;
        let mut peers = Vec::new();
        for (node_id, node) in &self.nodes {
            if node_id == &self.local_node {
                continue;
            }
            let endpoint = node
                .endpoints
                .iter()
                .filter(|e| {
                    matches!(
                        e.kind,
                        EndpointKind::Lan | EndpointKind::PublicIpv6 | EndpointKind::PublicUdp
                    )
                })
                .min_by_key(|e| e.priority)
                .map(|e| e.addr);
            let mut allowed_ips = vec![format!("{}/32", node.overlay_ip)];
            for route in self
                .routes
                .iter()
                .filter(|r| r.via_node.as_str() == node_id.as_str())
            {
                let cidr = route.prefix.to_string();
                if !allowed_ips.contains(&cidr) {
                    allowed_ips.push(cidr);
                }
            }
            peers.push(WireGuardPeerPlan {
                node_id: node_id.clone(),
                public_key: node.public_key.clone(),
                endpoint,
                allowed_ips,
                persistent_keepalive: if endpoint.is_some() { Some(25) } else { None },
            });
        }
        Ok(WireGuardPlan {
            interface: interface.into(),
            local_address: local.overlay_ip,
            local_ipv6: None,
            listen_port,
            private_key_file: private_key_file.into(),
            peers,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireGuardPeerPlan {
    pub node_id: String,
    pub public_key: String,
    pub endpoint: Option<SocketAddr>,
    pub allowed_ips: Vec<String>,
    pub persistent_keepalive: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireGuardPlan {
    pub interface: String,
    pub local_address: Ipv4Addr,
    /// Optional deterministic ULA host address, populated by Phase 4.
    pub local_ipv6: Option<Ipv6Addr>,
    pub listen_port: u16,
    pub private_key_file: PathBuf,
    pub peers: Vec<WireGuardPeerPlan>,
}

impl WireGuardPlan {
    /// Render argv arrays instead of shell strings. A caller can use Command::new(argv[0])
    /// with argv[1..] and never invoke a shell.
    pub fn linux_argv(&self) -> Vec<Vec<String>> {
        let mut commands = vec![
            vec![
                "ip".into(),
                "link".into(),
                "add".into(),
                "dev".into(),
                self.interface.clone(),
                "type".into(),
                "wireguard".into(),
            ],
            vec![
                "ip".into(),
                "address".into(),
                "replace".into(),
                format!("{}/32", self.local_address),
                "dev".into(),
                self.interface.clone(),
            ],
        ];
        if let Some(ipv6) = self.local_ipv6 {
            commands.push(vec![
                "ip".into(),
                "-6".into(),
                "address".into(),
                "replace".into(),
                format!("{ipv6}/128"),
                "dev".into(),
                self.interface.clone(),
            ]);
        }
        commands.push(vec![
            "wg".into(),
            "set".into(),
            self.interface.clone(),
            "listen-port".into(),
            self.listen_port.to_string(),
            "private-key".into(),
            self.private_key_file.display().to_string(),
        ]);

        for peer in &self.peers {
            let mut cmd = vec![
                "wg".into(),
                "set".into(),
                self.interface.clone(),
                "peer".into(),
                peer.public_key.clone(),
                "allowed-ips".into(),
                peer.allowed_ips.join(","),
            ];
            if let Some(endpoint) = peer.endpoint {
                cmd.extend(["endpoint".into(), endpoint.to_string()]);
            }
            if let Some(keepalive) = peer.persistent_keepalive {
                cmd.extend(["persistent-keepalive".into(), keepalive.to_string()]);
            }
            commands.push(cmd);
        }

        commands.push(vec![
            "ip".into(),
            "link".into(),
            "set".into(),
            "up".into(),
            "dev".into(),
            self.interface.clone(),
        ]);
        for peer in &self.peers {
            for prefix in &peer.allowed_ips {
                let mut route = vec!["ip".into()];
                if prefix.contains(':') {
                    route.push("-6".into());
                }
                route.extend([
                    "route".into(),
                    "replace".into(),
                    prefix.clone(),
                    "dev".into(),
                    self.interface.clone(),
                ]);
                commands.push(route);
            }
        }
        commands
    }
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, state: &MeshState) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        body.push_str(&format!("VERSION\t{}\n", state.version));
        body.push_str(&format!("POOL\t{}\n", state.ipam.pool));
        body.push_str(&format!("LOCAL\t{}\n", hex_encode(&state.local_node)));

        for node in state.nodes.values() {
            body.push_str(&format!(
                "NODE\t{}\t{}\t{}\t{}\t{}\t{}\n",
                hex_encode(&node.id),
                node.overlay_ip,
                hex_encode(&node.hostname),
                hex_encode(&node.public_key),
                node.last_seen_ms,
                if node.relay_capable { 1 } else { 0 }
            ));
            for endpoint in &node.endpoints {
                body.push_str(&format!(
                    "ENDPOINT\t{}\t{}\t{}\t{}\n",
                    hex_encode(&node.id),
                    endpoint.kind.as_str(),
                    endpoint.addr,
                    endpoint.priority
                ));
            }
        }
        for route in &state.routes {
            body.push_str(&format!(
                "ROUTE\t{}\t{}\t{}\t{}\n",
                route.prefix,
                hex_encode(&route.via_node),
                route.kind.as_str(),
                route.metric
            ));
        }

        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub fn load(&self) -> Result<MeshState> {
        let text = fs::read_to_string(&self.path)?;
        let mut version = None;
        let mut pool = None;
        let mut local = None;
        let mut node_lines = Vec::new();
        let mut endpoint_lines = Vec::new();
        let mut route_lines = Vec::new();

        for (line_no, line) in text.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.first().copied() {
                Some("VERSION") if fields.len() == 2 => {
                    version = Some(parse_u32(fields[1], line_no)?);
                }
                Some("POOL") if fields.len() == 2 => {
                    pool = Some(fields[1].parse::<Ipv4Prefix>()?);
                }
                Some("LOCAL") if fields.len() == 2 => {
                    local = Some(hex_decode(fields[1])?);
                }
                Some("NODE") if fields.len() == 7 => node_lines.push((line_no, fields)),
                Some("ENDPOINT") if fields.len() == 5 => endpoint_lines.push((line_no, fields)),
                Some("ROUTE") if fields.len() == 5 => route_lines.push((line_no, fields)),
                Some(kind) => {
                    return Err(MeshError::InvalidState(format!(
                        "line {}: invalid {kind} record",
                        line_no + 1
                    )));
                }
                None => {}
            }
        }

        let version = version.ok_or_else(|| MeshError::InvalidState("missing VERSION".into()))?;
        if version != STATE_VERSION {
            return Err(MeshError::InvalidState(format!(
                "unsupported state version {version}"
            )));
        }
        let pool = pool.ok_or_else(|| MeshError::InvalidState("missing POOL".into()))?;
        let local_node = local.ok_or_else(|| MeshError::InvalidState("missing LOCAL".into()))?;
        validate_node_id(&local_node)?;

        let mut ipam = OverlayIpam::new(pool);
        let mut nodes = BTreeMap::new();

        for (line_no, f) in node_lines {
            let id = hex_decode(f[1])?;
            validate_node_id(&id)?;
            let overlay_ip = parse_ipv4(f[2], line_no)?;
            ipam.restore(id.clone(), overlay_ip)?;
            let node = NodeRecord {
                id: id.clone(),
                overlay_ip,
                hostname: hex_decode(f[3])?,
                public_key: hex_decode(f[4])?,
                last_seen_ms: parse_u64(f[5], line_no)?,
                relay_capable: match f[6] {
                    "0" => false,
                    "1" => true,
                    other => {
                        return Err(MeshError::InvalidState(format!(
                            "line {}: invalid relay flag {other}",
                            line_no + 1
                        )));
                    }
                },
                endpoints: Vec::new(),
                advertised_subnets: Vec::new(),
            };
            if nodes.insert(id.clone(), node).is_some() {
                return Err(MeshError::InvalidState(format!("duplicate node {id}")));
            }
        }

        if !nodes.contains_key(&local_node) {
            return Err(MeshError::InvalidState(format!(
                "local node {local_node} is not present"
            )));
        }

        for (line_no, f) in endpoint_lines {
            let id = hex_decode(f[1])?;
            let node = nodes
                .get_mut(&id)
                .ok_or_else(|| MeshError::UnknownNode(id.clone()))?;
            node.endpoints.push(Endpoint {
                kind: f[2].parse()?,
                addr: f[3].parse().map_err(|_| {
                    MeshError::InvalidState(format!(
                        "line {}: invalid socket address {}",
                        line_no + 1,
                        f[3]
                    ))
                })?,
                priority: parse_u16(f[4], line_no)?,
            });
        }

        let mut state = MeshState {
            version,
            local_node,
            ipam,
            nodes,
            routes: Vec::new(),
        };

        for (line_no, f) in route_lines {
            let route = RouteAdvertisement {
                prefix: f[1].parse()?,
                via_node: hex_decode(f[2])?,
                kind: f[3].parse()?,
                metric: parse_u16(f[4], line_no)?,
            };
            state.advertise_route(route)?;
        }
        Ok(state)
    }
}

fn parse_u16(v: &str, line_no: usize) -> Result<u16> {
    v.parse()
        .map_err(|_| MeshError::InvalidState(format!("line {}: invalid u16 {v}", line_no + 1)))
}

fn parse_u32(v: &str, line_no: usize) -> Result<u32> {
    v.parse()
        .map_err(|_| MeshError::InvalidState(format!("line {}: invalid u32 {v}", line_no + 1)))
}

fn parse_u64(v: &str, line_no: usize) -> Result<u64> {
    v.parse()
        .map_err(|_| MeshError::InvalidState(format!("line {}: invalid u64 {v}", line_no + 1)))
}

fn parse_ipv4(v: &str, line_no: usize) -> Result<Ipv4Addr> {
    v.parse().map_err(|_| {
        MeshError::InvalidState(format!("line {}: invalid IPv4 address {v}", line_no + 1))
    })
}

fn hex_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Result<String> {
    if !value.len().is_multiple_of(2) {
        return Err(MeshError::InvalidState("odd-length hex field".into()));
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.as_chunks::<2>().0 {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    String::from_utf8(out).map_err(|_| MeshError::InvalidState("non-UTF8 text field".into()))
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(MeshError::InvalidState("invalid hex field".into())),
    }
}

/// Where FluxMesh keeps its files: `$FLUXMESH_DIR`, else `/var/lib/fluxvm/mesh` on Linux and
/// `~/Library/Application Support/FluxVM/mesh` on macOS (the same split `fluxvm-core` uses for
/// its state directory, since the daemon runs unprivileged there).
pub fn mesh_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FLUXMESH_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/FluxVM/mesh");
    }
    PathBuf::from("/var/lib/fluxvm/mesh")
}

pub fn default_state_path() -> PathBuf {
    if let Ok(path) = std::env::var("FLUXMESH_STATE") {
        return PathBuf::from(path);
    }
    crate::mesh_dir().join("state.v1")
}

pub fn endpoint_addr_family(endpoint: &Endpoint) -> &'static str {
    match endpoint.addr.ip() {
        IpAddr::V4(_) => "ipv4",
        IpAddr::V6(_) => "ipv6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn state() -> MeshState {
        MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "home-mac",
            "macmini.local",
            "pub-local",
        )
        .unwrap()
    }

    #[test]
    fn prefix_masks_and_contains() {
        let p: Ipv4Prefix = "10.20.0.0/16".parse().unwrap();
        assert_eq!(p.network(), Ipv4Addr::new(10, 20, 0, 0));
        assert!(p.contains(Ipv4Addr::new(10, 20, 9, 8)));
        assert!(!p.contains(Ipv4Addr::new(10, 21, 0, 1)));
    }

    #[test]
    fn ipam_is_stable_and_unique() {
        let mut ipam = OverlayIpam::new("100.96.0.0/24".parse().unwrap());
        let a1 = ipam.allocate("a").unwrap();
        let a2 = ipam.allocate("a").unwrap();
        let b = ipam.allocate("b").unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(ipam.pool.contains(a1));
        assert!(ipam.pool.contains(b));
    }

    #[test]
    fn ipam_exhausts_small_pool() {
        let mut ipam = OverlayIpam::new("192.0.2.0/30".parse().unwrap());
        ipam.allocate("a").unwrap();
        ipam.allocate("b").unwrap();
        assert!(matches!(ipam.allocate("c"), Err(MeshError::PoolExhausted)));
    }

    #[test]
    fn nat_classification_does_not_overclaim() {
        let obs = vec![
            NatObservation {
                observer: "a".into(),
                mapped_addr: "198.51.100.10:50000".parse().unwrap(),
            },
            NatObservation {
                observer: "b".into(),
                mapped_addr: "198.51.100.10:50000".parse().unwrap(),
            },
        ];
        assert_eq!(classify_nat(&obs), NatType::EndpointIndependent);

        let mut symmetric = obs.clone();
        symmetric[1].mapped_addr = "198.51.100.10:50001".parse().unwrap();
        assert_eq!(classify_nat(&symmetric), NatType::Symmetric);
        assert_eq!(classify_nat(&obs[..1]), NatType::Unknown);
    }

    #[test]
    fn direct_path_beats_relay() {
        let mut s = state();
        s.add_node("remote", "remote.local", "pub-remote", true)
            .unwrap();
        s.add_endpoint(
            "remote",
            Endpoint {
                kind: EndpointKind::PublicUdp,
                addr: "203.0.113.7:51820".parse().unwrap(),
                priority: 10,
            },
        )
        .unwrap();
        s.add_endpoint(
            "remote",
            Endpoint {
                kind: EndpointKind::Relay443,
                addr: "203.0.113.99:443".parse().unwrap(),
                priority: 10,
            },
        )
        .unwrap();

        let paths = s
            .best_paths(
                "remote",
                &PathSignals {
                    direct_udp_reachable: true,
                    relay443_reachable: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(paths[0].kind, EndpointKind::PublicUdp);
        assert_eq!(paths[1].kind, EndpointKind::Relay443);
    }

    #[test]
    fn longest_prefix_route_wins_then_metric() {
        let mut s = state();
        s.add_node("gateway-a", "a", "key-a", false).unwrap();
        s.add_node("gateway-b", "b", "key-b", false).unwrap();
        s.advertise_route(RouteAdvertisement {
            prefix: "10.0.0.0/8".parse().unwrap(),
            via_node: "gateway-a".into(),
            kind: RouteKind::LanSubnet,
            metric: 50,
        })
        .unwrap();
        s.advertise_route(RouteAdvertisement {
            prefix: "10.20.0.0/16".parse().unwrap(),
            via_node: "gateway-b".into(),
            kind: RouteKind::LanSubnet,
            metric: 100,
        })
        .unwrap();
        assert_eq!(
            s.route_for("10.20.1.5".parse().unwrap()).unwrap().via_node,
            "gateway-b"
        );
        assert_eq!(
            s.route_for("10.30.1.5".parse().unwrap()).unwrap().via_node,
            "gateway-a"
        );
    }

    #[test]
    fn state_roundtrip_is_lossless() {
        let mut s = state();
        s.add_node("office", "office mini", "key with spaces", true)
            .unwrap();
        s.add_endpoint(
            "office",
            Endpoint {
                kind: EndpointKind::Lan,
                addr: "192.168.50.4:51820".parse().unwrap(),
                priority: 5,
            },
        )
        .unwrap();
        s.advertise_route(RouteAdvertisement {
            prefix: "192.168.50.0/24".parse().unwrap(),
            via_node: "office".into(),
            kind: RouteKind::LanSubnet,
            metric: 20,
        })
        .unwrap();

        let path = env::temp_dir().join(format!(
            "fluxmesh-state-{}-{}.txt",
            std::process::id(),
            now_ms()
        ));
        let store = StateStore::new(&path);
        store.save(&s).unwrap();
        let loaded = store.load().unwrap();
        fs::remove_file(path).ok();
        assert_eq!(loaded, s);
    }

    #[test]
    fn wireguard_plan_has_peer_host_and_subnet_routes() {
        let mut s = state();
        let peer_ip = s.add_node("office", "office", "peer-key", false).unwrap();
        s.add_endpoint(
            "office",
            Endpoint {
                kind: EndpointKind::PublicUdp,
                addr: "203.0.113.10:51820".parse().unwrap(),
                priority: 1,
            },
        )
        .unwrap();
        s.advertise_route(RouteAdvertisement {
            prefix: "10.42.0.0/16".parse().unwrap(),
            via_node: "office".into(),
            kind: RouteKind::VmSubnet,
            metric: 10,
        })
        .unwrap();

        let plan = s
            .wireguard_plan("fluxmesh0", 51820, "/etc/fluxvm/mesh.key")
            .unwrap();
        let peer = plan.peers.iter().find(|p| p.node_id == "office").unwrap();
        assert!(peer.allowed_ips.contains(&format!("{peer_ip}/32")));
        assert!(peer.allowed_ips.contains(&"10.42.0.0/16".to_owned()));
        let argv = plan.linux_argv();
        assert!(
            argv.iter()
                .any(|cmd| cmd.first().map(String::as_str) == Some("wg"))
        );
        assert!(
            argv.iter()
                .any(|cmd| cmd.iter().any(|v| v == "10.42.0.0/16"))
        );
    }

    #[test]
    fn node_id_rejects_control_characters() {
        assert!(validate_node_id("good-node_1.example").is_ok());
        assert!(validate_node_id("bad\tnode").is_err());
        assert!(validate_node_id("").is_err());
    }
}
