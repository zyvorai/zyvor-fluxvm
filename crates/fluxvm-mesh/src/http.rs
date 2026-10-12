// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Tiny HTTP/1.1 client used for node-local FluxVM reconciliation.
//!
//! The daemon is normally reached over loopback. HTTPS is intentionally not
//! guessed here: use FluxVM's local HTTP listener for same-host reconciliation
//! or put a mutually authenticated local proxy in front of a remote daemon.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpEndpoint {
    pub host: String,
    pub port: u16,
    pub base_path: String,
}

impl HttpEndpoint {
    pub fn parse(base: &str) -> io::Result<Self> {
        let rest = base.strip_prefix("http://").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "FluxMesh reconciler currently requires an http:// FluxVM API URL",
            )
        })?;
        let (authority, base_path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, String::new()),
        };
        if authority.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty HTTP host",
            ));
        }
        let (host, port) = if authority.starts_with('[') {
            let end = authority.find(']').ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unterminated IPv6 HTTP host")
            })?;
            let host = authority[1..end].to_owned();
            let tail = &authority[end + 1..];
            let port = if tail.is_empty() {
                80
            } else {
                tail.strip_prefix(':')
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid HTTP authority")
                    })?
                    .parse::<u16>()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid HTTP port"))?
            };
            (host, port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            if !host.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                (
                    host.to_owned(),
                    port.parse::<u16>().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid HTTP port")
                    })?,
                )
            } else {
                (authority.to_owned(), 80)
            }
        } else {
            (authority.to_owned(), 80)
        };
        Ok(Self {
            host,
            port,
            base_path: base_path.trim_end_matches('/').to_owned(),
        })
    }

    pub fn request_json(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> io::Result<u16> {
        let addr = resolve_one(&self.host, self.port)?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let path = format!("{}{}", self.base_path, path);
        let body = body.unwrap_or("");
        let host_header = if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nAccept: application/json\r\n"
        );
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if !body.is_empty() {
            request.push_str("Content-Type: application/json\r\n");
        }
        request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(request.as_bytes())?;
        if !body.is_empty() {
            stream.write_all(body.as_bytes())?;
        }
        stream.flush()?;

        let mut response = Vec::new();
        let mut limited = stream.take(256 * 1024);
        limited.read_to_end(&mut response)?;
        let text = String::from_utf8_lossy(&response);
        let status_line = text
            .lines()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty HTTP response"))?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status"))?;
        if !(200..300).contains(&status) {
            let body = text
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or("");
            let detail = body.chars().take(1024).collect::<String>();
            return Err(io::Error::other(format!(
                "FluxVM API {method} {path} returned HTTP {status}: {detail}"
            )));
        }
        Ok(status)
    }
}

fn resolve_one(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "HTTP host did not resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_endpoints() {
        assert_eq!(
            HttpEndpoint::parse("http://127.0.0.1:7788").unwrap(),
            HttpEndpoint {
                host: "127.0.0.1".into(),
                port: 7788,
                base_path: String::new(),
            }
        );
        assert_eq!(
            HttpEndpoint::parse("http://[::1]:7788/api")
                .unwrap()
                .base_path,
            "/api"
        );
        assert!(HttpEndpoint::parse("https://example.com").is_err());
    }
}
