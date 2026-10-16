// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Host OpenAI-compatible inference status for `apple.host_gpu` (MLX / llama.cpp / stand-in).

use serde::Serialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Snapshot of whether a model server answers on the configured loopback port.
#[derive(Debug, Clone, Serialize)]
pub struct HostInferStatus {
    /// Configured host loopback port (`[apple].host_infer_port`).
    pub port: u16,
    /// Guest URL when `apple.host_gpu` is set.
    pub guest_base_url: String,
    /// TCP connect to `127.0.0.1:{port}` succeeded.
    pub reachable: bool,
    /// `GET /v1/models` returned 2xx (when reachable).
    pub models_ok: bool,
    /// First model id from `/v1/models`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Short error when not healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Probe `127.0.0.1:{port}` for an OpenAI-compatible `/v1/models` endpoint.
pub async fn probe(port: u16) -> HostInferStatus {
    let guest_base_url = format!("http://127.0.0.1:{}/v1", crate::runner::HOST_INFER_PORT);
    let addr = format!("127.0.0.1:{port}");
    let connect = timeout(Duration::from_secs(2), TcpStream::connect(&addr)).await;
    let mut stream = match connect {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return HostInferStatus {
                port,
                guest_base_url,
                reachable: false,
                models_ok: false,
                model: None,
                error: Some(e.to_string()),
            };
        }
        Err(_) => {
            return HostInferStatus {
                port,
                guest_base_url,
                reachable: false,
                models_ok: false,
                model: None,
                error: Some("connect timed out".into()),
            };
        }
    };
    let req =
        format!("GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if let Err(e) = stream.write_all(req.as_bytes()).await {
        return HostInferStatus {
            port,
            guest_base_url,
            reachable: true,
            models_ok: false,
            model: None,
            error: Some(e.to_string()),
        };
    }
    let mut buf = Vec::new();
    let read = timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    let body = match read {
        Ok(Ok(_)) => String::from_utf8_lossy(&buf).into_owned(),
        Ok(Err(e)) => {
            return HostInferStatus {
                port,
                guest_base_url,
                reachable: true,
                models_ok: false,
                model: None,
                error: Some(e.to_string()),
            };
        }
        Err(_) => {
            return HostInferStatus {
                port,
                guest_base_url,
                reachable: true,
                models_ok: false,
                model: None,
                error: Some("read timed out".into()),
            };
        }
    };
    let status_ok = body
        .lines()
        .next()
        .is_some_and(|l| l.contains(" 200 ") || l.ends_with(" 200"));
    let json_start = body.find('{').unwrap_or(0);
    let model = serde_json::from_str::<serde_json::Value>(&body[json_start..])
        .ok()
        .and_then(|v| {
            v.get("data")
                .and_then(|d| d.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.get("id"))
                .and_then(|id| id.as_str())
                .map(str::to_string)
        });
    HostInferStatus {
        port,
        guest_base_url,
        reachable: true,
        models_ok: status_ok,
        model,
        error: if status_ok {
            None
        } else {
            Some("GET /v1/models did not return 200".into())
        },
    }
}
