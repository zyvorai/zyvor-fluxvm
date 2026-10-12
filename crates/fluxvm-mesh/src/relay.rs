// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Opaque relay framing and selection. Relay payloads are intended to be
//! WireGuard UDP packets, so the relay never needs plaintext guest traffic.
//! The TCP runtime can be placed behind a TLS stream proxy on :443.

use crate::{MeshError, MeshState, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

pub const MAX_RELAY_PAYLOAD: usize = 65535;
pub const MAX_RELAY_ID: usize = 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayTransport {
    Udp,
    Tcp443,
}

impl RelayTransport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp443 => "tcp443",
        }
    }
}

impl fmt::Display for RelayTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RelayTransport {
    type Err = MeshError;
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "udp" | "quic" => Ok(Self::Udp),
            "tcp443" | "tls443" | "443" => Ok(Self::Tcp443),
            _ => Err(MeshError::InvalidState(format!(
                "invalid relay transport {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayRecord {
    pub id: String,
    pub endpoint: SocketAddr,
    pub transport: RelayTransport,
    pub priority: u16,
    pub load_percent: u8,
    pub healthy: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayRegistry {
    pub relays: BTreeMap<String, RelayRecord>,
}

impl RelayRegistry {
    pub fn upsert(&mut self, relay: RelayRecord) -> Result<()> {
        validate_id(&relay.id)?;
        if relay.load_percent > 100 {
            return Err(MeshError::InvalidState("relay load must be 0..100".into()));
        }
        self.relays.insert(relay.id.clone(), relay);
        Ok(())
    }

    pub fn select(&self, preferred: RelayTransport) -> Option<&RelayRecord> {
        self.relays
            .values()
            .filter(|r| r.healthy && r.transport == preferred)
            .min_by_key(|r| (r.priority, r.load_percent, r.id.clone()))
            .or_else(|| {
                self.relays
                    .values()
                    .filter(|r| r.healthy)
                    .min_by_key(|r| (r.priority, r.load_percent, r.id.clone()))
            })
    }

    pub fn select_for_peer(
        &self,
        mesh: &MeshState,
        peer: &str,
        preferred: RelayTransport,
    ) -> Result<Option<&RelayRecord>> {
        mesh.node(peer)?;
        Ok(self.select(preferred))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayFrame {
    pub peer: String,
    pub payload: Vec<u8>,
}

impl RelayFrame {
    pub fn write_to(&self, mut w: impl Write) -> io::Result<()> {
        validate_wire_id(&self.peer)?;
        if self.payload.len() > MAX_RELAY_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relay payload too large",
            ));
        }
        w.write_all(&[self.peer.len() as u8])?;
        w.write_all(self.peer.as_bytes())?;
        w.write_all(&(self.payload.len() as u32).to_be_bytes())?;
        w.write_all(&self.payload)
    }

    pub fn read_from(mut r: impl Read) -> io::Result<Self> {
        let mut len = [0u8; 1];
        r.read_exact(&mut len)?;
        let id_len = len[0] as usize;
        if id_len == 0 || id_len > MAX_RELAY_ID {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid relay peer id length",
            ));
        }
        let mut id = vec![0u8; id_len];
        r.read_exact(&mut id)?;
        let peer = String::from_utf8(id).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "relay peer id is not UTF-8")
        })?;
        validate_wire_id(&peer)?;
        let mut payload_len = [0u8; 4];
        r.read_exact(&mut payload_len)?;
        let payload_len = u32::from_be_bytes(payload_len) as usize;
        if payload_len > MAX_RELAY_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relay payload too large",
            ));
        }
        let mut payload = vec![0u8; payload_len];
        r.read_exact(&mut payload)?;
        Ok(Self { peer, payload })
    }
}

fn validate_id(value: &str) -> Result<()> {
    validate_wire_id(value).map_err(MeshError::Io)
}

fn validate_wire_id(value: &str) -> io::Result<()> {
    if value.is_empty()
        || value.len() > MAX_RELAY_ID
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relay node id",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayCredential {
    pub node_id: String,
    pub token: String,
}

pub fn load_credentials(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let node = fields.next().unwrap_or_default();
        let token = fields.next().unwrap_or_default();
        if fields.next().is_some() || token.len() < 16 {
            return Err(MeshError::InvalidState(format!(
                "relay credential line {} must be: NODE TOKEN (token >= 16 chars)",
                line_no + 1
            )));
        }
        validate_id(node)?;
        if out.insert(node.to_owned(), token.to_owned()).is_some() {
            return Err(MeshError::InvalidState(format!(
                "duplicate relay credential for {node}"
            )));
        }
    }
    Ok(out)
}

/// Small blocking TCP relay. Run it on loopback behind a TLS stream proxy for
/// the :443 mode. Registration is authenticated with per-node opaque tokens.
pub fn serve_tcp(addr: SocketAddr, credentials: BTreeMap<String, String>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    let sessions: Arc<Mutex<BTreeMap<String, (u64, Arc<Mutex<TcpStream>>)>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let next_session = Arc::new(AtomicU64::new(1));
    let credentials = Arc::new(credentials);
    for accepted in listener.incoming() {
        let stream = accepted?;
        let sessions = Arc::clone(&sessions);
        let credentials = Arc::clone(&credentials);
        let next_session = Arc::clone(&next_session);
        thread::spawn(move || {
            let _ = handle_client(stream, credentials, sessions, next_session);
        });
    }
    Ok(())
}

fn handle_client(
    mut stream: TcpStream,
    credentials: Arc<BTreeMap<String, String>>,
    sessions: Arc<Mutex<BTreeMap<String, (u64, Arc<Mutex<TcpStream>>)>>>,
    next_session: Arc<AtomicU64>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= 512 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registration too long",
            ));
        }
        line.push(byte[0]);
    }
    let line = String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid registration"))?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("REGISTER") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "registration required",
        ));
    }
    let node = fields.next().unwrap_or_default();
    let token = fields.next().unwrap_or_default();
    if fields.next().is_some() || credentials.get(node).map(String::as_str) != Some(token) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay authentication failed",
        ));
    }
    validate_wire_id(node)?;
    let session_id = next_session.fetch_add(1, Ordering::Relaxed);
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    sessions
        .lock()
        .map_err(|_| io::Error::other("relay session lock poisoned"))?
        .insert(node.to_owned(), (session_id, writer));
    stream.write_all(b"OK\n")?;

    loop {
        let frame = match RelayFrame::read_from(&mut stream) {
            Ok(frame) => frame,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                break;
            }
            Err(e) => return Err(e),
        };
        let destination = {
            let guard = sessions
                .lock()
                .map_err(|_| io::Error::other("relay session lock poisoned"))?;
            guard.get(&frame.peer).map(|(_, stream)| Arc::clone(stream))
        };
        if let Some(destination) = destination {
            let outbound = RelayFrame {
                peer: node.to_owned(),
                payload: frame.payload,
            };
            if let Ok(mut destination) = destination.lock() {
                let _ = outbound.write_to(&mut *destination);
            }
        }
    }

    if let Ok(mut guard) = sessions.lock() {
        if guard.get(node).is_some_and(|(id, _)| *id == session_id) {
            guard.remove(node);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn relay_frame_roundtrip() {
        let frame = RelayFrame {
            peer: "office".into(),
            payload: vec![1, 2, 3, 4, 5],
        };
        let mut bytes = Vec::new();
        frame.write_to(&mut bytes).unwrap();
        assert_eq!(RelayFrame::read_from(Cursor::new(bytes)).unwrap(), frame);
    }

    #[test]
    fn relay_selection_prefers_requested_transport_then_load() {
        let mut registry = RelayRegistry::default();
        registry
            .upsert(RelayRecord {
                id: "a".into(),
                endpoint: "198.51.100.1:443".parse().unwrap(),
                transport: RelayTransport::Tcp443,
                priority: 10,
                load_percent: 70,
                healthy: true,
            })
            .unwrap();
        registry
            .upsert(RelayRecord {
                id: "b".into(),
                endpoint: "198.51.100.2:443".parse().unwrap(),
                transport: RelayTransport::Tcp443,
                priority: 10,
                load_percent: 10,
                healthy: true,
            })
            .unwrap();
        assert_eq!(registry.select(RelayTransport::Tcp443).unwrap().id, "b");
    }

    #[test]
    fn credential_file_is_strict() {
        let creds = load_credentials("home abcdefghijklmnop\noffice qrstuvwxyzABCDEF\n").unwrap();
        assert_eq!(creds.len(), 2);
        assert!(load_credentials("home short\n").is_err());
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayPeerBinding {
    pub peer_id: String,
    /// UDP address presented to WireGuard as this peer's endpoint.
    pub bind_addr: SocketAddr,
}

/// Bridge WireGuard UDP packets to one authenticated relay TCP session.
///
/// Configure each WireGuard peer endpoint to its corresponding `bind_addr`.
/// Outbound packets arriving on that socket are tagged with that peer id.
/// Inbound relay frames are emitted from the same socket to `wireguard_addr`,
/// so WireGuard sees a stable endpoint per peer.
pub fn run_tcp_client(
    relay_addr: SocketAddr,
    node_id: &str,
    token: &str,
    wireguard_addr: SocketAddr,
    bindings: Vec<RelayPeerBinding>,
) -> io::Result<()> {
    validate_wire_id(node_id)?;
    if token.len() < 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relay token must be at least 16 characters",
        ));
    }
    if bindings.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "at least one relay peer binding is required",
        ));
    }

    let mut stream = TcpStream::connect(relay_addr)?;
    stream.set_nodelay(true)?;
    stream.write_all(format!("REGISTER {node_id} {token}\n").as_bytes())?;
    let mut reply = [0u8; 3];
    stream.read_exact(&mut reply)?;
    if &reply != b"OK\n" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay registration rejected",
        ));
    }

    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let mut sockets = BTreeMap::new();
    for binding in &bindings {
        validate_wire_id(&binding.peer_id)?;
        if sockets.contains_key(&binding.peer_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate relay peer binding",
            ));
        }
        let socket = std::net::UdpSocket::bind(binding.bind_addr)?;
        socket.set_read_timeout(None)?;
        sockets.insert(binding.peer_id.clone(), socket);
    }

    for (peer, socket) in &sockets {
        let peer = peer.clone();
        let socket = socket.try_clone()?;
        let writer = Arc::clone(&writer);
        thread::spawn(move || {
            let mut buf = vec![0u8; MAX_RELAY_PAYLOAD];
            loop {
                let (n, _) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let frame = RelayFrame {
                    peer: peer.clone(),
                    payload: buf[..n].to_vec(),
                };
                let Ok(mut writer) = writer.lock() else {
                    break;
                };
                if frame.write_to(&mut *writer).is_err() {
                    break;
                }
            }
        });
    }

    loop {
        let frame = RelayFrame::read_from(&mut stream)?;
        if let Some(socket) = sockets.get(&frame.peer) {
            socket.send_to(&frame.payload, wireguard_addr)?;
        }
    }
}
