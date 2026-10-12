// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! CGNAT-safe public ingress relay.
//!
//! The relay never terminates the visitor's TLS session. It peeks only at the
//! ClientHello SNI to select a registered edge, then copies the original TLS
//! bytes through a second authenticated TLS connection opened outbound by the
//! edge. The certificate/private key therefore remain on the personal-cloud
//! edge, not on the relay.

use crate::{MeshError, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use ring::hmac;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Cursor};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const MAX_LINE: usize = 4096;
const MAX_CLIENT_HELLO: usize = 64 * 1024;
const AUTH_WINDOW_SECS: u64 = 300;
const SITE_LEASE_SECS: u64 = 45;
const RELAY_QUIC_ALPN: &[u8] = b"fluxmesh-relay/1";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RelayRouteMode {
    #[default]
    ActiveStandby,
    ActiveActive,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayRoute {
    pub hostname: String,
    /// Sites eligible to serve this hostname.
    pub sites: Vec<String>,
    /// Active-standby chooses the first healthy site. Active-active uses
    /// rendezvous hashing per visitor so existing connections never move.
    #[serde(default)]
    pub mode: RelayRouteMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelaySite {
    pub id: String,
    pub secret_file: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayServerFile {
    pub public_listen: SocketAddr,
    pub tunnel_listen: SocketAddr,
    /// Optional QUIC listener for edge->relay control/data. Visitor traffic
    /// remains blind TLS passthrough on public_listen.
    #[serde(default)]
    pub quic_tunnel_listen: Option<SocketAddr>,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    #[serde(default)]
    pub routes: Vec<RelayRoute>,
    #[serde(default)]
    pub sites: Vec<RelaySite>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RelayTransport {
    #[default]
    Auto,
    Quic,
    TlsTcp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayClientConfig {
    pub relay_addr: SocketAddr,
    #[serde(default)]
    pub quic_relay_addr: Option<SocketAddr>,
    #[serde(default)]
    pub transport: RelayTransport,
    pub server_name: String,
    pub ca_file: PathBuf,
    pub site_id: String,
    pub secret_file: PathBuf,
    #[serde(default = "default_local_ingress")]
    pub local_ingress: SocketAddr,
    #[serde(default = "default_heartbeat_file")]
    pub heartbeat_file: PathBuf,
}

fn default_local_ingress() -> SocketAddr {
    "127.0.0.1:443".parse().unwrap()
}
fn default_heartbeat_file() -> PathBuf {
    crate::mesh_dir().join("public-relay-heartbeat")
}

#[derive(Clone)]
struct ControlEntry {
    tx: mpsc::Sender<String>,
    last_seen: u64,
    session_id: String,
}

#[derive(Clone)]
struct Registry {
    control: Arc<Mutex<HashMap<String, ControlEntry>>>,
    visitors: Arc<Mutex<HashMap<String, TcpStream>>>,
    secrets: Arc<HashMap<String, Vec<u8>>>,
    routes: Arc<Vec<RelayRoute>>,
}

impl Registry {
    fn new(file: &RelayServerFile) -> Result<Self> {
        let mut secrets = HashMap::new();
        for site in &file.sites {
            validate_site_id(&site.id)?;
            let secret = fs::read(&site.secret_file)?;
            if secret.len() < 32 {
                return Err(MeshError::InvalidState(format!(
                    "relay secret for {} must be at least 32 bytes",
                    site.id
                )));
            }
            secrets.insert(site.id.clone(), secret);
        }
        for route in &file.routes {
            if route.hostname.trim().is_empty() || route.sites.is_empty() {
                return Err(MeshError::InvalidState(
                    "relay routes require hostname and at least one site".into(),
                ));
            }
            for site in &route.sites {
                if !secrets.contains_key(site) {
                    return Err(MeshError::InvalidState(format!(
                        "relay route {} references unknown site {site}",
                        route.hostname
                    )));
                }
            }
        }
        Ok(Self {
            control: Arc::new(Mutex::new(HashMap::new())),
            visitors: Arc::new(Mutex::new(HashMap::new())),
            secrets: Arc::new(secrets),
            routes: Arc::new(file.routes.clone()),
        })
    }

    async fn choose_site(&self, hostname: &str, visitor_key: &str) -> Option<String> {
        let route = self
            .routes
            .iter()
            .find(|r| host_matches(&r.hostname, hostname))?;
        let connected = self.control.lock().await;
        let now = now_secs();
        let healthy = route
            .sites
            .iter()
            .filter(|site| {
                connected
                    .get(*site)
                    .is_some_and(|entry| now.saturating_sub(entry.last_seen) <= SITE_LEASE_SECS)
            })
            .cloned()
            .collect::<Vec<_>>();
        match route.mode {
            RelayRouteMode::ActiveStandby => healthy.into_iter().next(),
            RelayRouteMode::ActiveActive => healthy
                .into_iter()
                .max_by_key(|site| rendezvous_score(visitor_key, site)),
        }
    }

    async fn touch_session(&self, site: &str, session_id: &str) {
        if let Some(entry) = self.control.lock().await.get_mut(site) {
            if entry.session_id == session_id {
                entry.last_seen = now_secs();
            }
        }
    }

    async fn remove_session(&self, site: &str, session_id: &str) {
        let mut control = self.control.lock().await;
        if control
            .get(site)
            .is_some_and(|entry| entry.session_id == session_id)
        {
            control.remove(site);
        }
    }
}

pub async fn serve(config_path: &Path) -> Result<()> {
    let raw = fs::read(config_path)?;
    let file: RelayServerFile = serde_json::from_slice(&raw)
        .map_err(|e| MeshError::InvalidState(format!("parse relay config: {e}")))?;
    let registry = Registry::new(&file)?;
    let acceptor = tls_acceptor(&file.tls_cert, &file.tls_key)?;
    let public = TcpListener::bind(file.public_listen).await?;
    let tunnel = TcpListener::bind(file.tunnel_listen).await?;

    if let Some(quic_listen) = file.quic_tunnel_listen {
        let quic_registry = registry.clone();
        let cert = file.tls_cert.clone();
        let key = file.tls_key.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_quic_tunnel(quic_listen, &cert, &key, quic_registry).await {
                eprintln!("fluxmesh-public-relay: QUIC listener stopped: {e}");
            }
        });
    }

    let public_registry = registry.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = public.accept().await else {
                continue;
            };
            let registry = public_registry.clone();
            tokio::spawn(async move {
                let _ = accept_visitor(stream, registry).await;
            });
        }
    });

    loop {
        let (stream, _) = tunnel.accept().await?;
        let acceptor = acceptor.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(stream).await else {
                return;
            };
            let _ = handle_tunnel(tls, registry).await;
        });
    }
}

async fn accept_visitor(stream: TcpStream, registry: Registry) -> Result<()> {
    let sni = peek_sni(&stream)
        .await?
        .ok_or_else(|| MeshError::InvalidState("visitor TLS ClientHello had no SNI".into()))?;
    let id = request_id();
    let site = registry
        .choose_site(&sni, &id)
        .await
        .ok_or_else(|| MeshError::InvalidState(format!("no connected relay edge for {sni}")))?;
    registry.visitors.lock().await.insert(id.clone(), stream);
    let sender = registry
        .control
        .lock()
        .await
        .get(&site)
        .map(|v| v.tx.clone())
        .ok_or_else(|| MeshError::InvalidState(format!("relay site {site} disconnected")))?;
    if sender.send(format!("OPEN\t{id}\t{sni}\n")).await.is_err() {
        registry.visitors.lock().await.remove(&id);
        return Err(MeshError::InvalidState(format!(
            "relay control channel for {site} closed"
        )));
    }
    let visitors = registry.visitors.clone();
    let expire = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        visitors.lock().await.remove(&expire);
    });
    Ok(())
}

async fn handle_tunnel<S>(stream: S, registry: Registry) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = AsyncBufReader::new(stream);
    let mut line = String::new();
    read_line_bounded(&mut reader, &mut line).await?;
    let fields = line.trim_end().split('\t').collect::<Vec<_>>();
    match fields.as_slice() {
        ["REGISTER", site, ts, mac] => {
            authenticate(&registry, site, "register", "", ts, mac)?;
            let site = (*site).to_owned();
            let (tx, mut rx) = mpsc::channel::<String>(64);
            let session_id = request_id();
            registry.control.lock().await.insert(
                site.clone(),
                ControlEntry {
                    tx,
                    last_seen: now_secs(),
                    session_id: session_id.clone(),
                },
            );
            let stream = reader.into_inner();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut input = AsyncBufReader::new(read_half);
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        let Some(msg) = msg else { break; };
                        if write_half.write_all(msg.as_bytes()).await.is_err() { break; }
                        if write_half.flush().await.is_err() { break; }
                    }
                    incoming = async {
                        let mut line = String::new();
                        let n = read_line_bounded(&mut input, &mut line).await?;
                        Ok::<_, MeshError>((n, line))
                    } => {
                        let (n, line) = incoming?;
                        if n == 0 { break; }
                        let fields = line.trim_end().split('\t').collect::<Vec<_>>();
                        if let ["PING", ping_site, ts, mac] = fields.as_slice() {
                            if *ping_site != site.as_str() { return Err(MeshError::InvalidState("relay heartbeat site mismatch".into())); }
                            authenticate(&registry, ping_site, "ping", "", ts, mac)?;
                            registry.touch_session(&site, &session_id).await;
                        }
                    }
                }
            }
            registry.remove_session(&site, &session_id).await;
        }
        ["DATA", site, id, ts, mac] => {
            authenticate(&registry, site, "data", id, ts, mac)?;
            let mut visitor = registry.visitors.lock().await.remove(*id).ok_or_else(|| {
                MeshError::InvalidState(format!("unknown/expired relay request {id}"))
            })?;
            let mut tunnel = reader.into_inner();
            tokio::io::copy_bidirectional(&mut visitor, &mut tunnel).await?;
        }
        _ => {
            return Err(MeshError::InvalidState(
                "invalid relay tunnel greeting".into(),
            ));
        }
    }
    Ok(())
}

fn authenticate(
    registry: &Registry,
    site: &str,
    action: &str,
    id: &str,
    ts: &str,
    mac: &str,
) -> Result<()> {
    validate_site_id(site)?;
    let ts = ts
        .parse::<u64>()
        .map_err(|_| MeshError::InvalidState("invalid relay timestamp".into()))?;
    let now = now_secs();
    if now.abs_diff(ts) > AUTH_WINDOW_SECS {
        return Err(MeshError::InvalidState(
            "relay authentication timestamp outside allowed window".into(),
        ));
    }
    let secret = registry
        .secrets
        .get(site)
        .ok_or_else(|| MeshError::InvalidState(format!("unknown relay site {site}")))?;
    let payload = format!("{action}|{site}|{id}|{ts}");
    verify_mac(secret, payload.as_bytes(), mac)
}

pub async fn run_client(cfg: RelayClientConfig) -> Result<()> {
    validate_site_id(&cfg.site_id)?;
    let secret = fs::read(&cfg.secret_file)?;
    if secret.len() < 32 {
        return Err(MeshError::InvalidState(
            "relay client secret must be at least 32 bytes".into(),
        ));
    }
    let connector = tls_connector(&cfg.ca_file)?;
    loop {
        let mut used_quic = false;
        if cfg.transport != RelayTransport::TlsTcp && cfg.quic_relay_addr.is_some() {
            used_quic = true;
            match connect_quic_control(&cfg, &secret).await {
                Ok(()) => {}
                Err(e) if cfg.transport == RelayTransport::Auto => {
                    eprintln!(
                        "fluxmesh-public-relay: QUIC unavailable, falling back to TLS/TCP: {e}"
                    );
                    used_quic = false;
                }
                Err(e) => eprintln!("fluxmesh-public-relay: QUIC reconnect after error: {e}"),
            }
        }
        if !used_quic {
            if let Err(e) = connect_control(&cfg, &secret, &connector).await {
                eprintln!("fluxmesh-public-relay: TLS/TCP reconnect after error: {e}");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn connect_control(
    cfg: &RelayClientConfig,
    secret: &[u8],
    connector: &TlsConnector,
) -> Result<()> {
    let tcp = TcpStream::connect(cfg.relay_addr).await?;
    let server_name = ServerName::try_from(cfg.server_name.clone())
        .map_err(|_| MeshError::InvalidState("invalid relay TLS server name".into()))?;
    let tls = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay TLS: {e}")))?;
    let (read_half, mut write_half) = tokio::io::split(tls);
    let ts = now_secs().to_string();
    let mac = sign_mac(secret, format!("register|{}||{ts}", cfg.site_id).as_bytes());
    write_half
        .write_all(format!("REGISTER\t{}\t{}\t{}\n", cfg.site_id, ts, mac).as_bytes())
        .await?;
    write_half.flush().await?;
    let mut reader = AsyncBufReader::new(read_half);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    write_heartbeat(&cfg.heartbeat_file)?;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                write_heartbeat(&cfg.heartbeat_file)?;
                let ts = now_secs().to_string();
                let mac = sign_mac(secret, format!("ping|{}||{ts}", cfg.site_id).as_bytes());
                write_half.write_all(format!("PING\t{}\t{}\t{}\n", cfg.site_id, ts, mac).as_bytes()).await?;
                write_half.flush().await?;
            }
            result = async {
                let mut line = String::new();
                let n = read_line_bounded(&mut reader, &mut line).await?;
                Ok::<_,MeshError>((n,line))
            } => {
                let (n,line)=result?;
                if n == 0 { return Ok(()); }
                let fields = line.trim_end().split('\t').collect::<Vec<_>>();
                let ["OPEN", id, _hostname] = fields.as_slice() else { continue; };
                let cfg = cfg.clone();
                let secret = secret.to_vec();
                let connector = connector.clone();
                let id = (*id).to_owned();
                tokio::spawn(async move { let _ = open_data_tunnel(&cfg, &secret, &connector, &id).await; });
            }
        }
    }
}

async fn open_data_tunnel(
    cfg: &RelayClientConfig,
    secret: &[u8],
    connector: &TlsConnector,
    id: &str,
) -> Result<()> {
    let mut local = TcpStream::connect(cfg.local_ingress).await?;
    let tcp = TcpStream::connect(cfg.relay_addr).await?;
    let server_name = ServerName::try_from(cfg.server_name.clone())
        .map_err(|_| MeshError::InvalidState("invalid relay TLS server name".into()))?;
    let mut tunnel = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay TLS: {e}")))?;
    let ts = now_secs().to_string();
    let mac = sign_mac(secret, format!("data|{}|{id}|{ts}", cfg.site_id).as_bytes());
    tunnel
        .write_all(format!("DATA\t{}\t{}\t{}\t{}\n", cfg.site_id, id, ts, mac).as_bytes())
        .await?;
    tunnel.flush().await?;
    tokio::io::copy_bidirectional(&mut local, &mut tunnel).await?;
    Ok(())
}

async fn serve_quic_tunnel(
    listen: SocketAddr,
    cert: &Path,
    key: &Path,
    registry: Registry,
) -> Result<()> {
    let endpoint = quinn::Endpoint::server(quic_server_config(cert, key)?, listen)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC bind {listen}: {e}")))?;
    while let Some(incoming) = endpoint.accept().await {
        let registry = registry.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle_quic_connection(conn, registry).await {
                        eprintln!("fluxmesh-public-relay: QUIC connection failed: {e}");
                    }
                }
                Err(e) => eprintln!("fluxmesh-public-relay: QUIC handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

async fn handle_quic_connection(conn: quinn::Connection, registry: Registry) -> Result<()> {
    let (mut control_send, control_recv) = conn
        .accept_bi()
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC register stream: {e}")))?;
    let mut reader = AsyncBufReader::new(control_recv);
    let mut line = String::new();
    read_line_bounded(&mut reader, &mut line).await?;
    let fields = line.trim_end().split('\t').collect::<Vec<_>>();
    let ["REGISTER", site, ts, mac] = fields.as_slice() else {
        return Err(MeshError::InvalidState(
            "relay QUIC connection must start with REGISTER".into(),
        ));
    };
    authenticate(&registry, site, "register", "", ts, mac)?;
    let site = (*site).to_owned();
    let session_id = request_id();
    let (tx, mut rx) = mpsc::channel::<String>(64);
    registry.control.lock().await.insert(
        site.clone(),
        ControlEntry {
            tx,
            last_seen: now_secs(),
            session_id: session_id.clone(),
        },
    );

    loop {
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { break; };
                if control_send.write_all(msg.as_bytes()).await.is_err() { break; }
                if control_send.flush().await.is_err() { break; }
            }
            incoming = conn.accept_bi() => {
                let Ok((send, recv)) = incoming else { break; };
                let registry = registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_quic_data_stream(send, recv, registry).await {
                        eprintln!("fluxmesh-public-relay: QUIC data stream failed: {e}");
                    }
                });
            }
            incoming = async {
                let mut line = String::new();
                let n = read_line_bounded(&mut reader, &mut line).await?;
                Ok::<_, MeshError>((n, line))
            } => {
                let (n, line) = incoming?;
                if n == 0 { break; }
                let fields = line.trim_end().split('\t').collect::<Vec<_>>();
                if let ["PING", ping_site, ts, mac] = fields.as_slice() {
                    if *ping_site != site.as_str() { return Err(MeshError::InvalidState("relay QUIC heartbeat site mismatch".into())); }
                    authenticate(&registry, ping_site, "ping", "", ts, mac)?;
                    registry.touch_session(&site, &session_id).await;
                }
            }
        }
    }
    registry.remove_session(&site, &session_id).await;
    Ok(())
}

async fn handle_quic_data_stream(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    registry: Registry,
) -> Result<()> {
    let mut reader = AsyncBufReader::new(recv);
    let mut line = String::new();
    read_line_bounded(&mut reader, &mut line).await?;
    let fields = line.trim_end().split('\t').collect::<Vec<_>>();
    let ["DATA", site, id, ts, mac] = fields.as_slice() else {
        return Err(MeshError::InvalidState(
            "invalid relay QUIC DATA greeting".into(),
        ));
    };
    authenticate(&registry, site, "data", id, ts, mac)?;
    let visitor =
        registry.visitors.lock().await.remove(*id).ok_or_else(|| {
            MeshError::InvalidState(format!("unknown/expired relay request {id}"))
        })?;
    pump_tcp_quic(visitor, send, reader.into_inner()).await
}

async fn connect_quic_control(cfg: &RelayClientConfig, secret: &[u8]) -> Result<()> {
    let remote = cfg
        .quic_relay_addr
        .ok_or_else(|| MeshError::InvalidState("QUIC relay address is not configured".into()))?;
    let endpoint = quic_client_endpoint(cfg, remote)?;
    let connecting = endpoint
        .connect(remote, &cfg.server_name)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC connect setup: {e}")))?;
    let conn = connecting
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC connect: {e}")))?;
    let (mut control_send, control_recv) = conn
        .open_bi()
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC control stream: {e}")))?;
    let ts = now_secs().to_string();
    let mac = sign_mac(secret, format!("register|{}||{ts}", cfg.site_id).as_bytes());
    control_send
        .write_all(format!("REGISTER\t{}\t{}\t{}\n", cfg.site_id, ts, mac).as_bytes())
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC register: {e}")))?;
    control_send
        .flush()
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC register flush: {e}")))?;
    let mut reader = AsyncBufReader::new(control_recv);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    write_heartbeat(&cfg.heartbeat_file)?;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                write_heartbeat(&cfg.heartbeat_file)?;
                let ts = now_secs().to_string();
                let mac = sign_mac(secret, format!("ping|{}||{ts}", cfg.site_id).as_bytes());
                control_send.write_all(format!("PING\t{}\t{}\t{}\n", cfg.site_id, ts, mac).as_bytes()).await
                    .map_err(|e| MeshError::InvalidState(format!("relay QUIC heartbeat: {e}")))?;
                control_send.flush().await.map_err(|e| MeshError::InvalidState(format!("relay QUIC heartbeat flush: {e}")))?;
            }
            result = async {
                let mut line = String::new();
                let n = read_line_bounded(&mut reader, &mut line).await?;
                Ok::<_, MeshError>((n, line))
            } => {
                let (n, line) = result?;
                if n == 0 { return Ok(()); }
                let fields = line.trim_end().split('\t').collect::<Vec<_>>();
                let ["OPEN", id, _hostname] = fields.as_slice() else { continue; };
                let cfg = cfg.clone();
                let secret = secret.to_vec();
                let conn = conn.clone();
                let id = (*id).to_owned();
                tokio::spawn(async move {
                    if let Err(e) = open_quic_data(&cfg, &secret, conn, &id).await {
                        eprintln!("fluxmesh-public-relay: QUIC visitor tunnel failed: {e}");
                    }
                });
            }
        }
    }
}

async fn open_quic_data(
    cfg: &RelayClientConfig,
    secret: &[u8],
    conn: quinn::Connection,
    id: &str,
) -> Result<()> {
    let local = TcpStream::connect(cfg.local_ingress).await?;
    let (mut send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC DATA stream: {e}")))?;
    let ts = now_secs().to_string();
    let mac = sign_mac(secret, format!("data|{}|{id}|{ts}", cfg.site_id).as_bytes());
    send.write_all(format!("DATA\t{}\t{}\t{}\t{}\n", cfg.site_id, id, ts, mac).as_bytes())
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC DATA greeting: {e}")))?;
    send.flush()
        .await
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC DATA flush: {e}")))?;
    pump_tcp_quic(local, send, recv).await
}

async fn pump_tcp_quic(
    tcp: TcpStream,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<()> {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let up = async {
        tokio::io::copy(&mut tcp_read, &mut send).await?;
        send.finish()
            .map_err(|e| MeshError::InvalidState(format!("relay QUIC finish: {e}")))?;
        Ok::<(), MeshError>(())
    };
    let down = async {
        tokio::io::copy(&mut recv, &mut tcp_write)
            .await
            .map_err(MeshError::Io)?;
        tcp_write.shutdown().await?;
        Ok::<(), MeshError>(())
    };
    tokio::try_join!(up, down)?;
    Ok(())
}

fn quic_server_config(cert: &Path, key: &Path) -> Result<quinn::ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(load_certs(cert)?, load_key(key)?)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC TLS server config: {e}")))?;
    tls.alpn_protocols = vec![RELAY_QUIC_ALPN.to_vec()];
    let crypto = QuicServerConfig::try_from(tls)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC server crypto: {e:?}")))?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(crypto)))
}

fn quic_client_endpoint(cfg: &RelayClientConfig, remote: SocketAddr) -> Result<quinn::Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    for cert in load_certs(&cfg.ca_file)? {
        roots
            .add(cert)
            .map_err(|e| MeshError::InvalidState(format!("relay QUIC CA: {e}")))?;
    }
    let mut tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![RELAY_QUIC_ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC client crypto: {e:?}")))?;
    let client = quinn::ClientConfig::new(Arc::new(crypto));
    let bind = if remote.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let mut endpoint = quinn::Endpoint::client(bind)
        .map_err(|e| MeshError::InvalidState(format!("relay QUIC client bind: {e}")))?;
    endpoint.set_default_client_config(client);
    Ok(endpoint)
}

async fn peek_sni(stream: &TcpStream) -> Result<Option<String>> {
    let mut cap = 1024usize;
    loop {
        let mut buf = vec![0u8; cap];
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        match parse_client_hello_sni(&buf[..n]) {
            Ok(v) => return Ok(v),
            Err(ParseHello::NeedMore) if cap < MAX_CLIENT_HELLO => {
                cap = (cap * 2).min(MAX_CLIENT_HELLO);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(ParseHello::NeedMore) => {
                return Err(MeshError::InvalidState(
                    "TLS ClientHello exceeds 64 KiB".into(),
                ));
            }
            Err(ParseHello::Invalid) => {
                return Err(MeshError::InvalidState("invalid TLS ClientHello".into()));
            }
        }
    }
}

#[derive(Debug)]
enum ParseHello {
    NeedMore,
    Invalid,
}

fn parse_client_hello_sni(buf: &[u8]) -> std::result::Result<Option<String>, ParseHello> {
    if buf.len() < 5 {
        return Err(ParseHello::NeedMore);
    }
    if buf[0] != 22 {
        return Err(ParseHello::Invalid);
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < 5 + record_len {
        return Err(ParseHello::NeedMore);
    }
    let mut p = 5usize;
    if buf.get(p).copied() != Some(1) {
        return Err(ParseHello::Invalid);
    }
    if p + 4 > buf.len() {
        return Err(ParseHello::NeedMore);
    }
    let hs_len = ((buf[p + 1] as usize) << 16) | ((buf[p + 2] as usize) << 8) | buf[p + 3] as usize;
    p += 4;
    if p + hs_len > buf.len() {
        return Err(ParseHello::NeedMore);
    }
    if p + 34 > buf.len() {
        return Err(ParseHello::Invalid);
    }
    p += 34; // legacy_version + random
    let sid = *buf.get(p).ok_or(ParseHello::Invalid)? as usize;
    p += 1;
    p = p.checked_add(sid).ok_or(ParseHello::Invalid)?;
    if p + 2 > buf.len() {
        return Err(ParseHello::Invalid);
    }
    let suites = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
    p += 2 + suites;
    let comp = *buf.get(p).ok_or(ParseHello::Invalid)? as usize;
    p += 1 + comp;
    if p + 2 > buf.len() {
        return Ok(None);
    }
    let ext_len = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
    p += 2;
    let end = p.checked_add(ext_len).ok_or(ParseHello::Invalid)?;
    if end > buf.len() {
        return Err(ParseHello::NeedMore);
    }
    while p + 4 <= end {
        let kind = u16::from_be_bytes([buf[p], buf[p + 1]]);
        let len = u16::from_be_bytes([buf[p + 2], buf[p + 3]]) as usize;
        p += 4;
        if p + len > end {
            return Err(ParseHello::Invalid);
        }
        if kind == 0 {
            if len < 5 {
                return Err(ParseHello::Invalid);
            }
            let list_len = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
            let mut q = p + 2;
            let list_end = (q + list_len).min(p + len);
            while q + 3 <= list_end {
                let name_type = buf[q];
                let name_len = u16::from_be_bytes([buf[q + 1], buf[q + 2]]) as usize;
                q += 3;
                if q + name_len > list_end {
                    return Err(ParseHello::Invalid);
                }
                if name_type == 0 {
                    let host = std::str::from_utf8(&buf[q..q + name_len])
                        .map_err(|_| ParseHello::Invalid)?;
                    if host.is_empty() || host.len() > 253 {
                        return Err(ParseHello::Invalid);
                    }
                    return Ok(Some(host.to_ascii_lowercase()));
                }
                q += name_len;
            }
        }
        p += len;
    }
    Ok(None)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().to_ascii_lowercase();
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some(suffix) = p.strip_prefix("*.") {
        h != suffix && h.ends_with(&format!(".{suffix}"))
    } else {
        h == p.trim_end_matches('.')
    }
}

fn rendezvous_score(visitor_key: &str, site: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in visitor_key.bytes().chain([0xff]).chain(site.bytes()) {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn validate_site_id(site: &str) -> Result<()> {
    if site.is_empty()
        || site.len() > 64
        || !site
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidState(format!(
            "invalid relay site id {site:?}"
        )));
    }
    Ok(())
}

fn sign_mac(secret: &[u8], payload: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hex(hmac::sign(&key, payload).as_ref())
}
fn verify_mac(secret: &[u8], payload: &[u8], supplied: &str) -> Result<()> {
    let raw = unhex(supplied)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, payload, &raw)
        .map_err(|_| MeshError::InvalidState("relay authentication failed".into()))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(value: &str) -> Result<Vec<u8>> {
    if value.len() != 64 {
        return Err(MeshError::InvalidState("invalid relay MAC".into()));
    }
    (0..value.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&value[i..i + 2], 16)
                .map_err(|_| MeshError::InvalidState("invalid relay MAC".into()))
        })
        .collect()
}
pub fn heartbeat_fresh(path: &Path, max_age: Duration) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(ts) = raw.trim().parse::<u64>() else {
        return false;
    };
    now_secs().saturating_sub(ts) <= max_age.as_secs()
}
fn write_heartbeat(path: &Path) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, now_secs().to_string())?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn request_id() -> String {
    let n = now_secs();
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{n:016x}{t:08x}")
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn read_line_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    line: &mut String,
) -> Result<usize> {
    let n = r.read_line(line).await?;
    if line.len() > MAX_LINE {
        return Err(MeshError::InvalidState(
            "relay protocol line too long".into(),
        ));
    }
    Ok(n)
}

fn tls_acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs = load_certs(cert)?;
    let key = load_key(key)?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| MeshError::InvalidState(format!("relay TLS server config: {e}")))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
fn tls_connector(ca: &Path) -> Result<TlsConnector> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    for cert in load_certs(ca)? {
        roots
            .add(cert)
            .map_err(|e| MeshError::InvalidState(format!("relay CA: {e}")))?;
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let data = fs::read(path)?;
    let mut r = BufReader::new(Cursor::new(data));
    rustls_pemfile::certs(&mut r)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| MeshError::InvalidState(format!("parse certificates: {e}")))
}
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let data = fs::read(path)?;
    let mut r = BufReader::new(Cursor::new(data));
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| MeshError::InvalidState(format!("parse private key: {e}")))?
        .ok_or_else(|| MeshError::InvalidState("private key file contained no key".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcard_route_matches_subdomain_only() {
        assert!(host_matches("*.example.com", "a.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
    }
    #[test]
    fn relay_mac_roundtrip() {
        let s = b"01234567890123456789012345678901";
        let p = b"register|home||1";
        let m = sign_mac(s, p);
        verify_mac(s, p, &m).unwrap();
        assert!(verify_mac(s, b"x", &m).is_err());
    }
    #[test]
    fn invalid_site_rejected() {
        assert!(validate_site_id("a/b").is_err());
    }
    #[test]
    fn rendezvous_is_stable() {
        assert_eq!(
            rendezvous_score("v1", "site-a"),
            rendezvous_score("v1", "site-a")
        );
        assert_ne!(
            rendezvous_score("v1", "site-a"),
            rendezvous_score("v1", "site-b")
        );
    }
    #[test]
    fn route_mode_defaults_to_standby() {
        let r: RelayRoute = serde_json::from_str(r#"{"hostname":"x","sites":["a"]}"#).unwrap();
        assert_eq!(r.mode, RelayRouteMode::ActiveStandby);
    }
}
