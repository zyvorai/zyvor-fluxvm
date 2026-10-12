// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Native TLS zero-touch enrollment transport.
//!
//! The controller issues a one-time HMAC invitation with public_key="*". A
//! joining node generates its WireGuard key locally, sends only the public key
//! plus invitation over verified TLS, and receives a mesh state snapshot whose
//! LOCAL record is rewritten to itself.

use crate::enroll::{EnrollmentAuthority, UsedNonceStore, apply_join};
use crate::{MeshError, Result, StateStore};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub async fn serve(
    addr: SocketAddr,
    cert_path: &Path,
    key_path: &Path,
    state_path: PathBuf,
    secret_path: PathBuf,
    nonce_path: PathBuf,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind(addr).await.map_err(MeshError::Io)?;
    let acceptor = TlsAcceptor::from(load_server_config(cert_path, key_path)?);
    loop {
        let (tcp, _) = listener.accept().await.map_err(MeshError::Io)?;
        let acceptor = acceptor.clone();
        let state_path = state_path.clone();
        let secret_path = secret_path.clone();
        let nonce_path = nonce_path.clone();
        tokio::spawn(async move {
            let result = async {
                let mut stream = acceptor.accept(tcp).await.map_err(io_other)?;
                let line = read_line(&mut stream, 16 * 1024).await?;
                let mut fields = line.split_whitespace();
                if fields.next() != Some("JOIN") {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "JOIN required"));
                }
                let token = fields.next().unwrap_or_default();
                let presented_key = fields.next().unwrap_or_default();
                if fields.next().is_some()
                    || presented_key.is_empty()
                    || presented_key.chars().any(char::is_whitespace)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid JOIN request",
                    ));
                }
                let authority =
                    EnrollmentAuthority::load_or_create(&secret_path).map_err(io_other)?;
                let mut claims = authority.verify(token, now_ms()).map_err(io_other)?;
                if claims.public_key != "*" && claims.public_key != presented_key {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "public key does not match invitation",
                    ));
                }
                claims.public_key = presented_key.to_owned();
                UsedNonceStore::new(&nonce_path)
                    .consume(&claims.nonce)
                    .map_err(io_other)?;
                let store = StateStore::new(&state_path);
                let mut mesh = store.load().map_err(io_other)?;
                apply_join(&mut mesh, &claims).map_err(io_other)?;
                store.save(&mesh).map_err(io_other)?;

                let mut client_state = mesh.clone();
                client_state.local_node = claims.node_id.clone();
                let snapshot =
                    snapshot_bytes(&state_path, &claims.nonce, &client_state).map_err(io_other)?;
                stream
                    .write_all(format!("OK {}\n", snapshot.len()).as_bytes())
                    .await?;
                stream.write_all(&snapshot).await?;
                stream.flush().await?;
                Ok::<(), io::Error>(())
            }
            .await;
            if let Err(e) = result {
                eprintln!("fluxmesh-enroll: client rejected: {e}");
            }
        });
    }
}

pub async fn join(
    controller: SocketAddr,
    server_name: &str,
    ca_path: &Path,
    token: &str,
    public_key: &str,
    local_state_path: &Path,
) -> Result<()> {
    if public_key.is_empty() || public_key.chars().any(char::is_whitespace) {
        return Err(MeshError::InvalidState(
            "invalid WireGuard public key".into(),
        ));
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let connector = TlsConnector::from(load_client_config(ca_path)?);
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| MeshError::InvalidState("invalid enrollment TLS server name".into()))?;
    let tcp = TcpStream::connect(controller)
        .await
        .map_err(MeshError::Io)?;
    let mut stream = connector.connect(name, tcp).await.map_err(io_other)?;
    stream
        .write_all(format!("JOIN {token} {public_key}\n").as_bytes())
        .await
        .map_err(MeshError::Io)?;
    stream.flush().await.map_err(MeshError::Io)?;
    let line = read_line(&mut stream, 256).await.map_err(MeshError::Io)?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some("OK") {
        return Err(MeshError::InvalidState("enrollment rejected".into()));
    }
    let len = fields
        .next()
        .ok_or_else(|| MeshError::InvalidState("missing enrollment snapshot length".into()))?
        .parse::<usize>()
        .map_err(|_| MeshError::InvalidState("invalid enrollment snapshot length".into()))?;
    if fields.next().is_some() || len == 0 || len > 4 * 1024 * 1024 {
        return Err(MeshError::InvalidState(
            "invalid enrollment snapshot length".into(),
        ));
    }
    let mut snapshot = vec![0u8; len];
    stream
        .read_exact(&mut snapshot)
        .await
        .map_err(MeshError::Io)?;
    write_snapshot(local_state_path, &snapshot)?;
    Ok(())
}

fn snapshot_bytes(base: &Path, nonce: &str, state: &crate::MeshState) -> Result<Vec<u8>> {
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let path = parent.join(format!(".enroll-{nonce}.state"));
    let store = StateStore::new(&path);
    store.save(state)?;
    let bytes = fs::read(&path)?;
    fs::remove_file(path).ok();
    Ok(bytes)
}

fn write_snapshot(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("join.tmp");
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    // Validate the complete durable format before replacing a working local
    // state file. A truncated or malicious controller response therefore
    // cannot destroy the previous mesh state.
    StateStore::new(&tmp).load()?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn load_server_config(cert_path: &Path, key_path: &Path) -> Result<Arc<ServerConfig>> {
    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(MeshError::Io)?;
    if certs.is_empty() {
        return Err(MeshError::InvalidState(
            "enrollment TLS certificate file is empty".into(),
        ));
    }
    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(MeshError::Io)?
        .ok_or_else(|| {
            MeshError::InvalidState("enrollment TLS key contains no private key".into())
        })?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| MeshError::InvalidState(format!("enrollment TLS certificate: {e}")))?;
    Ok(Arc::new(config))
}

fn load_client_config(ca_path: &Path) -> Result<Arc<ClientConfig>> {
    let mut reader = BufReader::new(File::open(ca_path)?);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(MeshError::Io)?;
    if certs.is_empty() {
        return Err(MeshError::InvalidState(
            "enrollment CA file is empty".into(),
        ));
    }
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| MeshError::InvalidState(format!("enrollment CA: {e}")))?;
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> io::Result<String> {
    let mut out = Vec::new();
    loop {
        let mut one = [0u8; 1];
        reader.read_exact(&mut one).await?;
        if one[0] == b'\n' {
            break;
        }
        if one[0] != b'\r' {
            out.push(one[0]);
        }
        if out.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
    String::from_utf8(out)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not UTF-8"))
}

fn io_other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
