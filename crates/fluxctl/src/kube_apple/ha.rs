// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The Mac side of a multi-server cluster: a small TCP forwarder that gives `127.0.0.1:API_PORT` one stable meaning while
//! the servers behind it come and go, and a kubeconfig with one context per server as a manual fallback.
//!
//! The forwarder does not speak TLS and holds no credentials: it copies bytes to the first healthy backend, where a backend
//! is the local end of an ssh tunnel (host key pinned) to one server. Because `ssh -L` accepts a connection before the
//! far side answers, a backend is probed after connecting: a tunnel whose far side is down closes at once, a healthy API
//! server stays silent until the client speaks.

use anyhow::{Context, Result};
use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE: Duration = Duration::from_millis(150);
/// How long a backend that failed is skipped (unless every backend failed).
const DOWN_FOR: Duration = Duration::from_secs(3);

struct Shared {
    backends: Vec<SocketAddr>,
    next: AtomicUsize,
    down: Mutex<Vec<Option<Instant>>>,
}

impl Shared {
    fn mark_down(&self, i: usize) {
        self.down.lock().unwrap()[i] = Some(Instant::now());
    }
    fn is_down(&self, i: usize) -> bool {
        self.down.lock().unwrap()[i].is_some_and(|t| t.elapsed() < DOWN_FOR)
    }
}

/// Connects to a backend and checks it is alive. Returns the stream and any bytes the probe already read.
async fn try_backend(addr: SocketAddr) -> Option<(TcpStream, Vec<u8>)> {
    let s = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .ok()?
        .ok()?;
    match tokio::time::timeout(PROBE, s.readable()).await {
        Err(_) => Some((s, vec![])), // silent: alive
        Ok(Err(_)) => None,
        Ok(Ok(())) => {
            let mut buf = [0u8; 4096];
            match s.try_read(&mut buf) {
                Ok(0) => None, // closed at once: the tunnel's far side is down
                Ok(n) => Some((s, buf[..n].to_vec())),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Some((s, vec![])),
                Err(_) => None,
            }
        }
    }
}

async fn pick(sh: &Shared) -> Option<(TcpStream, Vec<u8>)> {
    let n = sh.backends.len();
    let start = sh.next.fetch_add(1, Ordering::Relaxed);
    // First pass skips backends that failed a moment ago; the second tries them anyway.
    for pass in 0..2 {
        for k in 0..n {
            let i = (start + k) % n;
            if pass == 0 && sh.is_down(i) {
                continue;
            }
            if pass == 1 && !sh.is_down(i) {
                continue;
            }
            match try_backend(sh.backends[i]).await {
                Some(ok) => return Some(ok),
                None => sh.mark_down(i),
            }
        }
    }
    None
}

async fn handle(mut client: TcpStream, sh: Arc<Shared>) {
    let Some((mut backend, first)) = pick(&sh).await else {
        return; // dropping the client tells it nothing answered
    };
    if !first.is_empty() && client.write_all(&first).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut backend).await;
}

/// Accepts connections forever and forwards each to the first healthy backend, rotating the starting point.
pub async fn serve(listener: TcpListener, backends: Vec<SocketAddr>) -> Result<()> {
    anyhow::ensure!(!backends.is_empty(), "no backends to forward to");
    let sh = Arc::new(Shared {
        down: Mutex::new(vec![None; backends.len()]),
        backends,
        next: AtomicUsize::new(0),
    });
    loop {
        let (client, _) = listener.accept().await.context("accepting a connection")?;
        tokio::spawn(handle(client, sh.clone()));
    }
}

/// The kubeconfig of a multi-server cluster: the usual `apple-NAME` context (through the forwarder) plus one context per
/// server (`apple-NAME-s1`, ...) that goes straight to that server's tunnel, for use when the forwarder is not running.
pub fn multi_kubeconfig(raw: &str, name: &str, port: u16, servers: usize) -> Result<String> {
    use serde_yaml::{Mapping, Value};
    let base = crate::kube_host::rewrite_kubeconfig(raw, &format!("apple-{name}"), port);
    let mut doc: Value = serde_yaml::from_str(&base).context("reading the server's kubeconfig")?;
    let s = |t: &str| Value::String(t.to_owned());
    let cluster0 = doc["clusters"]
        .as_sequence()
        .and_then(|c| c.first())
        .cloned()
        .context("the kubeconfig has no cluster")?;
    let mut clusters = vec![];
    let mut contexts = vec![];
    for i in 0..servers {
        let cname = format!("apple-{name}-s{}", i + 1);
        let mut c = cluster0.clone();
        c["name"] = s(&cname);
        c["cluster"]["server"] = s(&format!("https://127.0.0.1:{}", port + 1 + i as u16));
        clusters.push(c);
        let mut ctx = Mapping::new();
        ctx.insert(s("name"), s(&cname));
        let mut inner = Mapping::new();
        inner.insert(s("cluster"), s(&cname));
        inner.insert(s("user"), s(&format!("apple-{name}")));
        ctx.insert(s("context"), Value::Mapping(inner));
        contexts.push(Value::Mapping(ctx));
    }
    for (key, extra) in [("clusters", clusters), ("contexts", contexts)] {
        let seq = doc[key]
            .as_sequence_mut()
            .with_context(|| format!("the kubeconfig has no {key}"))?;
        seq.extend(extra);
    }
    Ok(serde_yaml::to_string(&doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An echo server; returns its address.
    async fn echo() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut b = [0u8; 64];
                    while let Ok(n) = s.read(&mut b).await {
                        if n == 0 || s.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        a
    }

    /// Accepts and closes at once, like `ssh -L` whose far side is down.
    async fn closer() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let _ = l.accept().await;
            }
        });
        a
    }

    async fn refused() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    }

    async fn proxy_to(backends: Vec<SocketAddr>) -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(serve(l, backends));
        a
    }

    async fn roundtrip(p: SocketAddr) -> bool {
        let Ok(mut c) = TcpStream::connect(p).await else {
            return false;
        };
        if c.write_all(b"ping").await.is_err() {
            return false;
        }
        let mut b = [0u8; 4];
        matches!(
            tokio::time::timeout(Duration::from_secs(3), c.read_exact(&mut b)).await,
            Ok(Ok(_))
        ) && &b == b"ping"
    }

    #[tokio::test]
    async fn forwards_to_a_healthy_backend() {
        let p = proxy_to(vec![echo().await]).await;
        assert!(roundtrip(p).await);
    }

    #[tokio::test]
    async fn skips_a_refusing_backend_and_a_backend_that_closes_at_once() {
        let p = proxy_to(vec![refused().await, closer().await, echo().await]).await;
        for _ in 0..4 {
            assert!(
                roundtrip(p).await,
                "every connection must reach the live backend"
            );
        }
    }

    #[tokio::test]
    async fn closes_the_client_when_no_backend_answers() {
        let p = proxy_to(vec![refused().await, closer().await]).await;
        assert!(!roundtrip(p).await);
    }

    #[tokio::test]
    async fn serve_needs_a_backend() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(serve(l, vec![]).await.is_err());
    }

    const RAW: &str = "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: Q0E=\n    server: https://127.0.0.1:6443\n  name: default\n\
contexts:\n- context:\n    cluster: default\n    user: default\n  name: default\ncurrent-context: default\nkind: Config\nusers:\n- name: default\n  user:\n    client-certificate-data: Q0VSVA==\n";

    #[test]
    fn multi_kubeconfig_has_a_context_per_server_and_one_user() {
        let t = multi_kubeconfig(RAW, "dev", 16443, 3).unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&t).unwrap();
        let servers: Vec<String> = v["clusters"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|c| c["cluster"]["server"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            servers,
            [
                "https://127.0.0.1:16443",
                "https://127.0.0.1:16444",
                "https://127.0.0.1:16445",
                "https://127.0.0.1:16446"
            ]
        );
        assert_eq!(v["current-context"], "apple-dev");
        assert_eq!(v["contexts"].as_sequence().unwrap().len(), 4);
        assert_eq!(v["users"].as_sequence().unwrap().len(), 1);
        assert!(t.contains("certificate-authority-data: Q0E="));
        assert!(t.contains("apple-dev-s3"));
    }
}
