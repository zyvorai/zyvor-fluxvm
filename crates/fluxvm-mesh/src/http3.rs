// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Standards-compliant HTTP/3 ingress using h3 + h3-quinn.
//!
//! HTTP/3 is a direct UDP/QUIC edge path. Phase 7 intentionally does not
//! pretend that the TCP public relay can carry browser QUIC. CGNAT clients
//! keep using the blind TLS/TCP relay until a QUIC Initial/SNI demultiplexer
//! or dedicated relay IP/port assignment is available.

use crate::public::{IngressRoute, PublicConfigStore};
use crate::share::{ShareAuthority, token_from_path};
use crate::trace::{self, EdgeSpan};
use crate::{MeshError, Result};
use bytes::{Buf, Bytes};
use h3::server;
use h3_quinn::Connection as H3QuinnConnection;
use http::header::HOST;
use http::{Request, Response, StatusCode};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::fs;
use std::io::{BufReader, Cursor};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static CONNECTIONS: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Http3Counters {
    pub connections: u64,
    pub requests: u64,
    pub failures: u64,
}

pub fn counters() -> Http3Counters {
    Http3Counters {
        connections: CONNECTIONS.load(Ordering::Relaxed),
        requests: REQUESTS.load(Ordering::Relaxed),
        failures: FAILURES.load(Ordering::Relaxed),
    }
}

pub async fn serve_http3(
    listen: SocketAddr,
    public_config: PathBuf,
    share_secret: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
) -> Result<()> {
    let server = quic_server_config(&cert_path, &key_path)?;
    let endpoint = Endpoint::server(server, listen)
        .map_err(|e| MeshError::InvalidState(format!("HTTP/3 bind {listen}: {e}")))?;
    let mut generation = tls_generation(&cert_path, &key_path);
    let mut reload = tokio::time::interval(std::time::Duration::from_secs(30));
    reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { return Ok(()); };
                let cfg = public_config.clone();
                let secret = share_secret.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            CONNECTIONS.fetch_add(1, Ordering::Relaxed);
                            if let Err(e) = handle_connection(conn, cfg, secret).await {
                                FAILURES.fetch_add(1, Ordering::Relaxed);
                                eprintln!("fluxmesh-http3: connection failed: {e}");
                            }
                        }
                        Err(e) => {
                            FAILURES.fetch_add(1, Ordering::Relaxed);
                            eprintln!("fluxmesh-http3: QUIC handshake failed: {e}");
                        }
                    }
                });
            }
            _ = reload.tick() => {
                let next = tls_generation(&cert_path, &key_path);
                if next != generation {
                    match quic_server_config(&cert_path, &key_path) {
                        Ok(server) => {
                            endpoint.set_server_config(Some(server));
                            generation = next;
                        }
                        Err(e) => eprintln!("fluxmesh-http3: keep previous TLS config; reload failed: {e}"),
                    }
                }
            }
        }
    }
}

fn tls_generation(cert: &Path, key: &Path) -> (u64, u64, u64, u64) {
    fn one(path: &Path) -> (u64, u64) {
        let Ok(meta) = fs::metadata(path) else {
            return (0, 0);
        };
        let stamp = meta
            .modified()
            .ok()
            .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|v| v.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        (stamp, meta.len())
    }
    let (cm, cl) = one(cert);
    let (km, kl) = one(key);
    (cm, cl, km, kl)
}

async fn handle_connection(
    conn: quinn::Connection,
    config_path: PathBuf,
    share_secret: PathBuf,
) -> Result<()> {
    let quic = H3QuinnConnection::new(conn);
    let mut h3 = server::builder()
        .build(quic)
        .await
        .map_err(|e| MeshError::InvalidState(format!("HTTP/3 server setup: {e}")))?;

    loop {
        let resolver = match h3.accept().await {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(()),
            Err(e) => return Err(MeshError::InvalidState(format!("HTTP/3 accept: {e}"))),
        };
        let cfg = config_path.clone();
        let secret = share_secret.clone();
        tokio::spawn(async move {
            let result = async {
                let (request, mut stream) = resolver
                    .resolve_request()
                    .await
                    .map_err(|e| MeshError::InvalidState(format!("HTTP/3 request headers: {e}")))?;
                REQUESTS.fetch_add(1, Ordering::Relaxed);
                let mut body = Vec::new();
                while let Some(mut chunk) = stream
                    .recv_data()
                    .await
                    .map_err(|e| MeshError::InvalidState(format!("HTTP/3 request body: {e}")))?
                {
                    while chunk.has_remaining() {
                        let piece = chunk.copy_to_bytes(chunk.remaining());
                        body.extend_from_slice(&piece);
                    }
                }
                let upstream = proxy_request(request, body, &cfg, &secret).await?;
                let (parts, body) = upstream.into_parts();
                let mut response = Response::builder().status(parts.status);
                for (name, value) in &parts.headers {
                    if !is_hop_header(name.as_str()) {
                        response = response.header(name, value);
                    }
                }
                let response = response
                    .body(())
                    .map_err(|e| MeshError::InvalidState(format!("HTTP/3 response build: {e}")))?;
                stream.send_response(response).await.map_err(|e| {
                    MeshError::InvalidState(format!("HTTP/3 response headers: {e}"))
                })?;
                if !body.is_empty() {
                    stream.send_data(body).await.map_err(|e| {
                        MeshError::InvalidState(format!("HTTP/3 response body: {e}"))
                    })?;
                }
                stream
                    .finish()
                    .await
                    .map_err(|e| MeshError::InvalidState(format!("HTTP/3 finish: {e}")))?;
                Ok::<(), MeshError>(())
            }
            .await;
            if let Err(e) = result {
                FAILURES.fetch_add(1, Ordering::Relaxed);
                eprintln!("fluxmesh-http3: request failed: {e}");
            }
        });
    }
}

async fn proxy_request(
    req: Request<()>,
    body: Vec<u8>,
    config_path: &Path,
    share_secret: &Path,
) -> Result<Response<Bytes>> {
    let trace_start = unix_nanos();
    let host = request_host(&req)?;
    let cfg = PublicConfigStore::new(config_path).load()?;
    let route = cfg
        .ingress
        .iter()
        .find(|r| r.hostname.eq_ignore_ascii_case(&host))
        .ok_or_else(|| {
            MeshError::InvalidState(format!("no public ingress route for host {host}"))
        })?;
    let route_name = route.name.clone();
    let route_backend = route.backend.to_string();
    let otlp = cfg.otlp.clone();
    let mut path = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/")
        .to_owned();
    authorize_route(route, &mut path, share_secret)?;

    let url = format!("http://{}{}", route.backend, path);
    let client = reqwest::Client::new();
    let mut upstream = client.request(req.method().clone(), &url);
    for (name, value) in req.headers() {
        if is_hop_header(name.as_str()) || name == HOST {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    let response = upstream
        .body(body)
        .send()
        .await
        .map_err(|e| MeshError::InvalidState(format!("HTTP/3 upstream request: {e}")))?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|e| MeshError::InvalidState(format!("HTTP/3 upstream body: {e}")))?;
    let mut out = Response::builder().status(status);
    if let Some(target) = out.headers_mut() {
        for (name, value) in &headers {
            if !is_hop_header(name.as_str()) {
                target.insert(name.clone(), value.clone());
            }
        }
        target.insert(
            "alt-svc",
            http::HeaderValue::from_static("h3=\":443\"; ma=86400"),
        );
    }
    if let Some(otlp) = otlp {
        let mut attributes = std::collections::BTreeMap::new();
        attributes.insert("http.protocol".into(), "h3".into());
        attributes.insert("fluxmesh.route".into(), route_name);
        attributes.insert("server.address".into(), route_backend);
        attributes.insert(
            "http.response.status_code".into(),
            status.as_u16().to_string(),
        );
        let span = EdgeSpan {
            name: "fluxmesh.ingress.request".into(),
            start_unix_nano: trace_start,
            end_unix_nano: unix_nanos(),
            ok: status.is_success() || status.is_redirection(),
            attributes,
        };
        tokio::spawn(async move {
            let _ = trace::export(&otlp, span).await;
        });
    }
    out.body(body)
        .map_err(|e| MeshError::InvalidState(format!("HTTP/3 response: {e}")))
}

fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn request_host(req: &Request<()>) -> Result<String> {
    if let Some(host) = req.uri().host() {
        return Ok(host.to_owned());
    }
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| MeshError::InvalidState("HTTP/3 authority/Host required".into()))?;
    Ok(strip_host_port(host))
}

fn authorize_route(route: &IngressRoute, path: &mut String, share_secret: &Path) -> Result<()> {
    if !route.share_required {
        return Ok(());
    }
    let (token, suffix) = token_from_path(path)
        .ok_or_else(|| MeshError::InvalidState("share link required".into()))?;
    ShareAuthority::load_or_create(share_secret)?.verify(token, &route.name, now_ms())?;
    if route.strip_share_prefix {
        *path = if suffix.is_empty() {
            "/".into()
        } else {
            format!("/{suffix}")
        };
    }
    Ok(())
}

fn quic_server_config(cert_path: &Path, key_path: &Path) -> Result<ServerConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert_data = fs::read(cert_path)?;
    let mut cert_reader = BufReader::new(Cursor::new(cert_data));
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| MeshError::InvalidState(format!("parse HTTP/3 certificate: {e}")))?;
    let key_data = fs::read(key_path)?;
    let mut key_reader = BufReader::new(Cursor::new(key_data));
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| MeshError::InvalidState(format!("parse HTTP/3 private key: {e}")))?
        .ok_or_else(|| MeshError::InvalidState("HTTP/3 private key contained no key".into()))?;
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| MeshError::InvalidState(format!("build HTTP/3 TLS config: {e}")))?;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = QuicServerConfig::try_from(tls)
        .map_err(|e| MeshError::InvalidState(format!("build HTTP/3 QUIC crypto: {e:?}")))?;
    Ok(ServerConfig::with_crypto(Arc::new(crypto)))
}

fn strip_host_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some((addr, _)) = rest.split_once(']') {
            return addr.to_owned();
        }
    }
    host.split(':').next().unwrap_or(host).to_owned()
}

fn is_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ipv6_authority() {
        assert_eq!(strip_host_port("[fd00::1]:443"), "fd00::1");
        assert_eq!(strip_host_port("cloud.example:443"), "cloud.example");
    }

    #[test]
    fn hop_headers_are_filtered() {
        assert!(is_hop_header("Connection"));
        assert!(!is_hop_header("content-type"));
    }
}
