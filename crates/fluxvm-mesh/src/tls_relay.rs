// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Native TLS transport for the opaque FluxMesh relay.
//!
//! The relay still only sees WireGuard ciphertext. TLS protects relay metadata
//! (node ids, framing, traffic shape from passive observers) and allows the
//! hard-NAT fallback to listen directly on TCP :443 without stunnel/nginx.

use crate::relay::{MAX_RELAY_ID, MAX_RELAY_PAYLOAD, RelayFrame, RelayPeerBinding};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{RwLock, mpsc};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const RELAY_QUEUE: usize = 512;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

type Sessions = Arc<RwLock<BTreeMap<String, (u64, mpsc::Sender<RelayFrame>)>>>;

pub async fn serve_tls(
    addr: SocketAddr,
    cert_path: PathBuf,
    key_path: PathBuf,
    credentials: BTreeMap<String, String>,
) -> io::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind(addr).await?;
    let credentials = Arc::new(credentials);
    let sessions: Sessions = Arc::new(RwLock::new(BTreeMap::new()));
    let mut config = load_server_config(&cert_path, &key_path)?;

    loop {
        let (tcp, _) = listener.accept().await?;
        // Certificate/key replacement becomes effective for every new TLS
        // connection. If a partially written rotation is observed, retain the
        // last known-good config and try again on the next accept.
        match load_server_config(&cert_path, &key_path) {
            Ok(next) => config = next,
            Err(e) => eprintln!("fluxmesh-relay: TLS reload rejected, keeping last good cert: {e}"),
        }
        let acceptor = TlsAcceptor::from(Arc::clone(&config));
        let credentials = Arc::clone(&credentials);
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move {
            if let Err(e) = handle_server_connection(tcp, acceptor, credentials, sessions).await {
                eprintln!("fluxmesh-relay: TLS client closed: {e}");
            }
        });
    }
}

async fn handle_server_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    credentials: Arc<BTreeMap<String, String>>,
    sessions: Sessions,
) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    let mut tls = acceptor.accept(tcp).await.map_err(io_other)?;
    let registration = read_line_limited(&mut tls, 512).await?;
    let mut fields = registration.split_whitespace();
    if fields.next() != Some("REGISTER") {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "registration required",
        ));
    }
    let node = fields.next().unwrap_or_default();
    let token = fields.next().unwrap_or_default();
    if fields.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid relay registration",
        ));
    }
    validate_wire_id(node)?;
    let Some(expected) = credentials.get(node) else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay authentication failed",
        ));
    };
    if !constant_time_eq(expected.as_bytes(), token.as_bytes()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay authentication failed",
        ));
    }
    tls.write_all(b"OK\n").await?;
    tls.flush().await?;

    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (mut reader, mut writer) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::channel::<RelayFrame>(RELAY_QUEUE);
    sessions
        .write()
        .await
        .insert(node.to_owned(), (session_id, tx));

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            write_frame_async(&mut writer, &frame).await?;
        }
        Ok::<(), io::Error>(())
    });

    let read_result = loop {
        let frame = match read_frame_async(&mut reader).await {
            Ok(frame) => frame,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                break Ok(());
            }
            Err(e) => break Err(e),
        };
        let destination = sessions
            .read()
            .await
            .get(&frame.peer)
            .map(|(_, sender)| sender.clone());
        if let Some(destination) = destination {
            let outbound = RelayFrame {
                peer: node.to_owned(),
                payload: frame.payload,
            };
            // A slow peer may lose relay packets, matching UDP semantics and
            // preventing one stalled client from backpressuring every session.
            let _ = destination.try_send(outbound);
        }
    };

    {
        let mut guard = sessions.write().await;
        if guard.get(node).is_some_and(|(id, _)| *id == session_id) {
            guard.remove(node);
        }
    }
    match writer_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(e)) if read_result.is_ok() => return Err(e),
        Ok(Err(_)) => {}
    }
    read_result
}

pub async fn run_tls_client(
    relay_addr: SocketAddr,
    server_name: &str,
    ca_path: &Path,
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
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = load_client_config(ca_path)?;
    let connector = TlsConnector::from(config);
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid TLS server name"))?;
    let tcp = TcpStream::connect(relay_addr).await?;
    tcp.set_nodelay(true)?;
    let mut tls = connector.connect(name, tcp).await.map_err(io_other)?;
    tls.write_all(format!("REGISTER {node_id} {token}\n").as_bytes())
        .await?;
    tls.flush().await?;
    if read_line_limited(&mut tls, 32).await? != "OK" {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay registration rejected",
        ));
    }

    let mut sockets = BTreeMap::new();
    for binding in bindings {
        validate_wire_id(&binding.peer_id)?;
        if sockets.contains_key(&binding.peer_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "duplicate relay peer binding",
            ));
        }
        let socket = Arc::new(UdpSocket::bind(binding.bind_addr).await?);
        sockets.insert(binding.peer_id, socket);
    }

    let (mut reader, mut writer) = tokio::io::split(tls);
    let (tx, mut rx) = mpsc::channel::<RelayFrame>(RELAY_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            write_frame_async(&mut writer, &frame).await?;
        }
        Ok::<(), io::Error>(())
    });

    for (peer, socket) in &sockets {
        let peer = peer.clone();
        let socket = Arc::clone(socket);
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_RELAY_PAYLOAD];
            loop {
                let Ok((n, _)) = socket.recv_from(&mut buf).await else {
                    break;
                };
                if tx
                    .send(RelayFrame {
                        peer: peer.clone(),
                        payload: buf[..n].to_vec(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    drop(tx);

    let read_result = loop {
        let frame = match read_frame_async(&mut reader).await {
            Ok(frame) => frame,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                break Ok(());
            }
            Err(e) => break Err(e),
        };
        if let Some(socket) = sockets.get(&frame.peer) {
            socket.send_to(&frame.payload, wireguard_addr).await?;
        }
    };
    writer_task.abort();
    read_result
}

fn load_server_config(cert_path: &Path, key_path: &Path) -> io::Result<Arc<ServerConfig>> {
    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<std::result::Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS certificate file contains no certificates",
        ));
    }
    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> =
        rustls_pemfile::private_key(&mut key_reader)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "TLS key file contains no key")
        })?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io_other)?;
    Ok(Arc::new(config))
}

fn load_client_config(ca_path: &Path) -> io::Result<Arc<ClientConfig>> {
    let mut reader = BufReader::new(File::open(ca_path)?);
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay CA file contains no certificates",
        ));
    }
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(io_other)?;
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

async fn read_line_limited<R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<String> {
    let mut bytes = Vec::new();
    loop {
        let mut one = [0u8; 1];
        reader.read_exact(&mut one).await?;
        if one[0] == b'\n' {
            break;
        }
        if bytes.len() >= limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relay line too long",
            ));
        }
        if one[0] != b'\r' {
            bytes.push(one[0]);
        }
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "relay line is not UTF-8"))
}

async fn read_frame_async<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<RelayFrame> {
    let mut id_len = [0u8; 1];
    reader.read_exact(&mut id_len).await?;
    let id_len = id_len[0] as usize;
    if id_len == 0 || id_len > MAX_RELAY_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid relay peer id length",
        ));
    }
    let mut id = vec![0u8; id_len];
    reader.read_exact(&mut id).await?;
    let peer = String::from_utf8(id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "relay peer id is not UTF-8"))?;
    validate_wire_id(&peer)?;
    let mut payload_len = [0u8; 4];
    reader.read_exact(&mut payload_len).await?;
    let payload_len = u32::from_be_bytes(payload_len) as usize;
    if payload_len > MAX_RELAY_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay payload too large",
        ));
    }
    let mut payload = vec![0u8; payload_len];
    reader.read_exact(&mut payload).await?;
    Ok(RelayFrame { peer, payload })
}

async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &RelayFrame,
) -> io::Result<()> {
    validate_wire_id(&frame.peer)?;
    if frame.payload.len() > MAX_RELAY_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relay payload too large",
        ));
    }
    writer.write_all(&[frame.peer.len() as u8]).await?;
    writer.write_all(frame.peer.as_bytes()).await?;
    writer
        .write_all(&(frame.payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await
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

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&left, &right) in a.iter().zip(b) {
        diff |= left ^ right;
    }
    diff == 0
}

fn io_other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"abcdefghijklmnop", b"abcdefghijklmnop"));
        assert!(!constant_time_eq(b"abcdefghijklmnop", b"abcdefghijklmnoq"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }
}
