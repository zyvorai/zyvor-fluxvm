// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! HTTPS reverse ingress and ACME HTTP-01 challenge serving.

use crate::public::{IngressRoute, PublicConfigStore};
use crate::share::{ShareAuthority, token_from_path};
use crate::trace::{self, EdgeSpan};
use crate::{MeshError, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::HOST;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as HyperBuilder;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::fs;
use std::io::{BufReader, Cursor};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

const MAX_HEADER: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ChallengeStore {
    dir: PathBuf,
}

impl ChallengeStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn put(&self, token: &str, key_authorization: &str) -> Result<()> {
        validate_challenge_token(token)?;
        fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(token);
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, key_authorization.as_bytes())?;
        fs::rename(tmp, path)?;
        Ok(())
    }
    pub fn get(&self, token: &str) -> Result<Option<String>> {
        validate_challenge_token(token)?;
        let path = self.dir.join(token);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(path)?))
    }
    pub fn remove(&self, token: &str) -> Result<()> {
        validate_challenge_token(token)?;
        let path = self.dir.join(token);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

pub fn default_challenge_dir() -> PathBuf {
    crate::mesh_dir().join("acme-http01")
}

pub async fn serve_https(
    listen: SocketAddr,
    public_config: PathBuf,
    share_secret: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
) -> Result<()> {
    let listener = TcpListener::bind(listen).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        // Reload per accepted connection so an ACME renewal becomes active
        // immediately without restarting the edge process.
        let acceptor = match tls_acceptor(&cert_path, &key_path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fluxmesh-ingress: TLS material unavailable: {e}");
                continue;
            }
        };
        let cfg = public_config.clone();
        let secret = share_secret.clone();
        tokio::spawn(async move {
            if let Ok(tls) = acceptor.accept(stream).await {
                let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2");
                if h2 {
                    let _ = proxy_h2_connection(tls, cfg, secret).await;
                } else {
                    let _ = proxy_connection(tls, &cfg, &secret).await;
                }
            }
        });
    }
}

pub async fn serve_http01(listen: SocketAddr, store: ChallengeStore) -> Result<()> {
    let listener = TcpListener::bind(listen).await?;
    loop {
        let (mut stream, _) = listener.accept().await?;
        let store = store.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let text = String::from_utf8_lossy(&buf[..n]);
            let path = text
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let token = path.strip_prefix("/.well-known/acme-challenge/");
            let (status, body) = match token.and_then(|t| store.get(t).ok().flatten()) {
                Some(value) => ("200 OK", value),
                None => ("404 Not Found", "not found\n".into()),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn proxy_connection<S>(mut client: S, config_path: &Path, share_secret: &Path) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (header, tail) = read_header(&mut client).await?;
    let parsed = ParsedRequest::parse(&header)?;
    let cfg = PublicConfigStore::new(config_path).load()?;
    let route = cfg
        .ingress
        .iter()
        .find(|route| route.hostname.eq_ignore_ascii_case(&parsed.host))
        .ok_or_else(|| {
            MeshError::InvalidState(format!("no public ingress route for host {}", parsed.host))
        })?;

    let rewritten = authorize_and_rewrite(route, &parsed, &header, share_secret)?;
    let mut backend = TcpStream::connect(route.backend).await?;
    backend.write_all(&rewritten).await?;
    if !tail.is_empty() {
        backend.write_all(&tail).await?;
    }
    backend.flush().await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await?;
    Ok(())
}

async fn proxy_h2_connection(
    tls: tokio_rustls::server::TlsStream<TcpStream>,
    config_path: PathBuf,
    share_secret: PathBuf,
) -> Result<()> {
    let service = service_fn(move |req| {
        let config_path = config_path.clone();
        let share_secret = share_secret.clone();
        async move { Ok::<_, Infallible>(proxy_h2_request(req, &config_path, &share_secret).await) }
    });
    let builder = HyperBuilder::new(TokioExecutor::new());
    builder
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .await
        .map_err(|e| MeshError::InvalidState(format!("HTTP/2 connection: {e}")))?;
    Ok(())
}

async fn proxy_h2_request(
    req: Request<Incoming>,
    config_path: &Path,
    share_secret: &Path,
) -> Response<Full<Bytes>> {
    match proxy_h2_request_inner(req, config_path, share_secret).await {
        Ok(v) => v,
        Err(e) => simple_response(StatusCode::BAD_GATEWAY, format!("fluxmesh ingress: {e}\n")),
    }
}

async fn proxy_h2_request_inner(
    mut req: Request<Incoming>,
    config_path: &Path,
    share_secret: &Path,
) -> Result<Response<Full<Bytes>>> {
    let trace_start = unix_nanos();
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .map(strip_host_port)
        .ok_or_else(|| MeshError::InvalidState("HTTP Host header required".into()))?;
    let cfg = PublicConfigStore::new(config_path).load()?;
    let route = cfg
        .ingress
        .iter()
        .find(|route| route.hostname.eq_ignore_ascii_case(&host))
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
    if route.share_required {
        let (token, suffix) = token_from_path(&path)
            .ok_or_else(|| MeshError::InvalidState("share link required".into()))?;
        ShareAuthority::load_or_create(share_secret)?.verify(token, &route.name, now_ms())?;
        if route.strip_share_prefix {
            path = if suffix.is_empty() {
                "/".into()
            } else {
                format!("/{suffix}")
            };
        }
    }
    let body = req
        .body_mut()
        .collect()
        .await
        .map_err(|e| MeshError::InvalidState(format!("read HTTP/2 body: {e}")))?
        .to_bytes();
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
        .map_err(|e| MeshError::InvalidState(format!("upstream HTTP: {e}")))?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| MeshError::InvalidState(format!("read upstream body: {e}")))?;
    let mut out = Response::builder().status(status);
    if let Some(h) = out.headers_mut() {
        for (name, value) in &headers {
            if !is_hop_header(name.as_str()) {
                h.insert(name.clone(), value.clone());
            }
        }
        h.insert(
            http::header::HeaderName::from_static("alt-svc"),
            http::HeaderValue::from_static("h3=\":443\"; ma=86400"),
        );
    }
    if let Some(otlp) = otlp {
        let mut attributes = std::collections::BTreeMap::new();
        attributes.insert("http.protocol".into(), "h2".into());
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
    out.body(Full::new(bytes))
        .map_err(|e| MeshError::InvalidState(format!("build HTTP/2 response: {e}")))
}

fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

fn strip_host_port(value: &str) -> String {
    if value.starts_with('[') {
        return value
            .strip_prefix('[')
            .and_then(|v| v.split_once(']').map(|(h, _)| h.to_owned()))
            .unwrap_or_else(|| value.to_owned());
    }
    value.split(':').next().unwrap_or(value).to_owned()
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
fn simple_response(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn authorize_and_rewrite(
    route: &IngressRoute,
    parsed: &ParsedRequest,
    raw_header: &[u8],
    share_secret: &Path,
) -> Result<Vec<u8>> {
    if !route.share_required {
        return Ok(raw_header.to_vec());
    }
    let (token, suffix) = token_from_path(&parsed.path)
        .ok_or_else(|| MeshError::InvalidState("share link required".into()))?;
    ShareAuthority::load_or_create(share_secret)?.verify(token, &route.name, now_ms())?;
    if !route.strip_share_prefix {
        return Ok(raw_header.to_vec());
    }
    let path = if suffix.is_empty() {
        "/".into()
    } else {
        format!("/{suffix}")
    };
    rewrite_request_path(raw_header, &path)
}

fn rewrite_request_path(header: &[u8], new_path: &str) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(header)
        .map_err(|_| MeshError::InvalidState("HTTP header is not UTF-8/ASCII".into()))?;
    let end = text
        .find("\r\n")
        .ok_or_else(|| MeshError::InvalidState("invalid HTTP request line".into()))?;
    let mut parts = text[..end].split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| MeshError::InvalidState("missing HTTP method".into()))?;
    let _old = parts
        .next()
        .ok_or_else(|| MeshError::InvalidState("missing HTTP path".into()))?;
    let version = parts
        .next()
        .ok_or_else(|| MeshError::InvalidState("missing HTTP version".into()))?;
    let mut out = format!("{method} {new_path} {version}{}", &text[end..]).into_bytes();
    if !out.ends_with(b"\r\n\r\n") {
        return Err(MeshError::InvalidState("incomplete HTTP header".into()));
    }
    Ok(std::mem::take(&mut out))
}

struct ParsedRequest {
    host: String,
    path: String,
}
impl ParsedRequest {
    fn parse(header: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(header)
            .map_err(|_| MeshError::InvalidState("HTTP header is not UTF-8/ASCII".into()))?;
        let mut lines = text.split("\r\n");
        let request = lines
            .next()
            .ok_or_else(|| MeshError::InvalidState("missing request line".into()))?;
        let path = request
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| MeshError::InvalidState("missing request path".into()))?
            .to_owned();
        let host = lines
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("host")
                    .then(|| value.trim().to_owned())
            })
            .ok_or_else(|| MeshError::InvalidState("HTTP Host header required".into()))?;
        let host = host
            .strip_prefix('[')
            .and_then(|v| v.split_once(']').map(|(h, _)| h.to_owned()))
            .unwrap_or_else(|| host.split(':').next().unwrap_or(&host).to_owned());
        Ok(Self { host, path })
    }
}

async fn read_header<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(MeshError::InvalidState(
                "connection closed before HTTP header".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEADER {
            return Err(MeshError::InvalidState("HTTP header exceeds 64 KiB".into()));
        }
        if let Some(pos) = find_double_crlf(&buf) {
            let end = pos + 4;
            return Ok((buf[..end].to_vec(), buf[end..].to_vec()));
        }
    }
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

fn tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert_data = fs::read(cert_path)?;
    let mut reader = BufReader::new(Cursor::new(cert_data));
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| MeshError::InvalidState(format!("parse TLS certificate: {e}")))?;
    let key_data = fs::read(key_path)?;
    let mut reader = BufReader::new(Cursor::new(key_data));
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| MeshError::InvalidState(format!("parse TLS private key: {e}")))?
        .ok_or_else(|| MeshError::InvalidState("TLS private key file contained no key".into()))?;
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| MeshError::InvalidState(format!("build TLS server: {e}")))?;
    // HTTP/2 is negotiated when the client supports ALPN h2. HTTP/1.1 stays
    // on the zero-copy/raw proxy path to preserve upgrade/websocket behavior.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn validate_challenge_token(token: &str) -> Result<()> {
    if token.is_empty()
        || token.len() > 256
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(MeshError::InvalidState("invalid ACME HTTP-01 token".into()));
    }
    Ok(())
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
    fn parses_host_and_path() {
        let req = b"GET /x HTTP/1.1\r\nHost: cloud.example:443\r\n\r\n";
        let parsed = ParsedRequest::parse(req).unwrap();
        assert_eq!(parsed.host, "cloud.example");
        assert_eq!(parsed.path, "/x");
    }
    #[test]
    fn rewrites_only_request_target() {
        let req = b"GET /s/token/old HTTP/1.1\r\nHost: x\r\n\r\n";
        let out = String::from_utf8(rewrite_request_path(req, "/old").unwrap()).unwrap();
        assert!(out.starts_with("GET /old HTTP/1.1\r\n"));
        assert!(out.ends_with("\r\n\r\n"));
    }
}
