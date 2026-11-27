// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Where the adapter listens. The Docker API has no authentication, so only a `0600` unix socket or a loopback
//! TCP address is accepted.

use axum::Router;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl ListenAddr {
    /// `unix:///run/x.sock`, `unix:/run/x.sock`, `/run/x.sock`, `tcp://127.0.0.1:2375` or `127.0.0.1:2375`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty Docker API listen address".into());
        }
        if let Some(p) = raw
            .strip_prefix("unix://")
            .or_else(|| raw.strip_prefix("unix:"))
        {
            return Self::unix(p);
        }
        if raw.starts_with('/') {
            return Self::unix(raw);
        }
        let hostport = raw.strip_prefix("tcp://").unwrap_or(raw);
        let addr: SocketAddr = hostport.parse().map_err(|_| {
            format!("invalid Docker API listen address {raw:?} (want unix:///path or tcp://127.0.0.1:PORT)")
        })?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to serve the unauthenticated Docker API on non-loopback {addr}; use 127.0.0.1 or a unix socket"
            ));
        }
        Ok(Self::Tcp(addr))
    }

    fn unix(p: &str) -> Result<Self, String> {
        let path = Path::new(p);
        if !path.is_absolute() {
            return Err(format!(
                "Docker API unix socket path must be absolute: {p:?}"
            ));
        }
        Ok(Self::Unix(path.to_path_buf()))
    }

    /// The `DOCKER_HOST` value a client uses for this address.
    pub fn docker_host(&self) -> String {
        match self {
            Self::Unix(p) => format!("unix://{}", p.display()),
            Self::Tcp(a) => format!("tcp://{a}"),
        }
    }
}

/// Bind `addr` and serve `app` until the process exits. A stale socket at a unix path is replaced; any other file
/// there is an error.
pub async fn serve(addr: ListenAddr, app: Router) -> std::io::Result<()> {
    match addr {
        ListenAddr::Tcp(a) => {
            let listener = tokio::net::TcpListener::bind(a).await?;
            tracing::info!(addr = %a, "Docker API adapter listening");
            axum::serve(listener, app).await
        }
        #[cfg(unix)]
        ListenAddr::Unix(path) => {
            use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            match tokio::fs::symlink_metadata(&path).await {
                Ok(m) if m.file_type().is_socket() => tokio::fs::remove_file(&path).await?,
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let listener = tokio::net::UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            tracing::info!(path = %path.display(), "Docker API adapter listening");
            axum::serve(listener, app).await
        }
        #[cfg(not(unix))]
        ListenAddr::Unix(path) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "unix sockets are not supported on this platform: {}",
                path.display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_and_loopback_tcp() {
        let sock = ListenAddr::Unix("/run/fluxvm/docker-api.sock".into());
        assert_eq!(
            ListenAddr::parse("unix:///run/fluxvm/docker-api.sock"),
            Ok(sock.clone())
        );
        assert_eq!(
            ListenAddr::parse("unix:/run/fluxvm/docker-api.sock"),
            Ok(sock.clone())
        );
        assert_eq!(
            ListenAddr::parse(" /run/fluxvm/docker-api.sock "),
            Ok(sock.clone())
        );
        assert_eq!(sock.docker_host(), "unix:///run/fluxvm/docker-api.sock");
        let tcp = ListenAddr::parse("tcp://127.0.0.1:2375").unwrap();
        assert_eq!(tcp, ListenAddr::Tcp("127.0.0.1:2375".parse().unwrap()));
        assert_eq!(tcp.docker_host(), "tcp://127.0.0.1:2375");
        assert!(ListenAddr::parse("[::1]:2375").is_ok());
    }

    #[test]
    fn rejects_unsafe_or_malformed_addresses() {
        assert!(ListenAddr::parse("").is_err());
        assert!(ListenAddr::parse("unix://relative.sock").is_err());
        assert!(
            ListenAddr::parse("0.0.0.0:2375")
                .unwrap_err()
                .contains("non-loopback")
        );
        assert!(ListenAddr::parse("tcp://10.0.0.1:2375").is_err());
        assert!(ListenAddr::parse("localhost").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_to_replace_a_regular_file() {
        let dir =
            std::env::temp_dir().join(format!("fluxvm-docker-api-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-socket");
        std::fs::write(&file, b"keep me").unwrap();
        let err = serve(ListenAddr::Unix(file.clone()), Router::new())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&file).unwrap(), b"keep me");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
