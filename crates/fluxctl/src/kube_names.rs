//! Names, not ports: `http://web.shop.localhost` on 127.0.0.1:80 reaches the published service `web` in namespace `shop`.
//! `*.localhost` resolves to loopback on a Mac, so no `/etc/hosts` edit and no port number. While the cluster sleeps, the
//! service's own port is held by the wake-on-demand listener, so a browser request through a name wakes the cluster too.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::kube_host::NodePortService;

/// host name -> the loopback port its service is published on
pub type Names = Arc<Mutex<BTreeMap<String, u16>>>;

/// `web.shop.localhost` for every service; a service in `default` is also `web.localhost`. A service with several node
/// ports answers on its lowest node port (so an app puts the port a browser wants first).
pub(crate) fn route_table(published: &[NodePortService]) -> BTreeMap<String, u16> {
    let mut t = BTreeMap::new();
    let mut sorted: Vec<&NodePortService> = published.iter().collect();
    sorted.sort_by_key(|s| s.port);
    for s in sorted.into_iter().rev() {
        // inserted highest port first, so the lowest port wins
        t.insert(
            format!("{}.{}.localhost", s.name, s.namespace),
            s.host_port(),
        );
        if s.namespace == "default" {
            t.insert(format!("{}.localhost", s.name), s.host_port());
        }
    }
    t
}

/// The host of an HTTP request head, lower-cased and without a port.
pub(crate) fn parse_host(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            let v = value.trim();
            let host = match v.strip_prefix('[') {
                Some(_) => v, // an IPv6 literal is never one of our names
                None => v.rsplit_once(':').map_or(v, |(h, p)| {
                    if p.bytes().all(|b| b.is_ascii_digit()) {
                        h
                    } else {
                        v
                    }
                }),
            };
            return Some(host.to_ascii_lowercase());
        }
    }
    None
}

fn not_found(host: Option<&str>, known: &[String]) -> String {
    let body = format!(
        "No published service is named {}.\nKnown names:\n{}\n",
        host.unwrap_or("(no Host header)"),
        if known.is_empty() {
            "  (none yet: create a NodePort or LoadBalancer service)".to_owned()
        } else {
            known
                .iter()
                .map(|k| format!("  http://{k}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    );
    format!(
        "HTTP/1.1 404 Not Found\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Adds `x-forwarded-proto: https` after the request line, so an app behind the TLS listener builds https links.
pub fn with_forwarded_proto(head: &[u8]) -> Vec<u8> {
    match head.windows(2).position(|w| w == b"\r\n") {
        Some(i) => {
            let mut out = head[..i].to_vec();
            out.extend_from_slice(b"\r\nx-forwarded-proto: https");
            out.extend_from_slice(&head[i..]);
            out
        }
        None => head.to_vec(),
    }
}

/// Reads one request head from `client`, finds the service its Host names and proxies the connection to it.
async fn handle<S>(mut client: S, names: Names, https: bool)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), async {
        while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match client.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    if read.is_err() || head.is_empty() {
        return;
    }
    let host = parse_host(&head);
    let (port, known) = {
        let t = names.lock().unwrap();
        (
            host.as_ref().and_then(|h| t.get(h)).copied(),
            t.keys().cloned().collect::<Vec<_>>(),
        )
    };
    let Some(port) = port else {
        let _ = client
            .write_all(not_found(host.as_deref(), &known).as_bytes())
            .await;
        return;
    };
    // The service's port is open (published) or held by the wake-on-demand listener, which wakes the cluster. A
    // cluster that is just going to sleep or coming back has neither for a moment, so wait a little before giving up.
    let mut upstream = None;
    for _ in 0..40 {
        if let Ok(u) = TcpStream::connect(("127.0.0.1", port)).await {
            upstream = Some(u);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some(mut upstream) = upstream else {
        let _ = client
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await;
        return;
    };
    let head = if https {
        with_forwarded_proto(&head)
    } else {
        head
    };
    if upstream.write_all(&head).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// Listens on 127.0.0.1:`port` and forwards each request to the service its Host names; with `tls`, the connection is
/// TLS first. Returns at once if the port is taken (something else on the Mac uses it); the names then simply do not work
/// and the node-port URLs still do.
async fn listen(names: Names, port: u16, tls: Option<tokio_rustls::TlsAcceptor>) {
    use tokio::net::TcpListener;
    let scheme = if tls.is_some() { "https" } else { "http" };
    // macOS lets an ordinary user bind a privileged port only on the wildcard address, not on 127.0.0.1. Prefer loopback
    // where it works; otherwise listen widely and drop every connection that does not come from this Mac.
    let (listener, wide) = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => (l, false),
        Err(_) => match TcpListener::bind(("0.0.0.0", port)).await {
            Ok(l) => (l, true),
            Err(e) => {
                return eprintln!(
                    "names: cannot listen on port {port} ({e}); services stay on their node ports"
                );
            }
        },
    };
    eprintln!("names: {scheme}://<service>.<namespace>.localhost on port {port} (this Mac only)");
    loop {
        let Ok((client, peer)) = listener.accept().await else {
            continue;
        };
        if wide && !peer.ip().is_loopback() {
            continue; // another machine on the network: not for us
        }
        let names = names.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                None => handle(client, names, false).await,
                Some(acceptor) => {
                    let handshake =
                        tokio::time::timeout(Duration::from_secs(5), acceptor.accept(client)).await;
                    if let Ok(Ok(stream)) = handshake {
                        handle(stream, names, true).await;
                    }
                }
            }
        });
    }
}

/// The plain-HTTP names listener on port 80.
pub async fn serve(names: Names) {
    listen(names, 80, None).await
}

/// The HTTPS names listener on port 443, with a certificate per name from the local CA.
pub async fn serve_tls(names: Names, acceptor: tokio_rustls::TlsAcceptor) {
    listen(names, 443, Some(acceptor)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(ns: &str, name: &str, port: u16) -> NodePortService {
        NodePortService {
            namespace: ns.into(),
            name: name.into(),
            port,
            local: None,
        }
    }

    #[test]
    fn every_service_gets_a_name_and_default_ones_a_short_one() {
        let t = route_table(&[
            svc("shop", "web", 30081),
            svc("default", "api", 30090),
            svc("shop", "web", 30080),
        ]);
        assert_eq!(t["web.shop.localhost"], 30080, "the lowest node port wins");
        assert_eq!(t["api.default.localhost"], 30090);
        assert_eq!(t["api.localhost"], 30090);
        assert!(!t.contains_key("web.localhost"));
    }

    #[test]
    fn the_host_is_read_from_the_request_head() {
        let req = b"GET /x HTTP/1.1\r\nUser-Agent: curl\r\nHost: Web.Shop.localhost:80\r\nAccept: */*\r\n\r\n";
        assert_eq!(parse_host(req).as_deref(), Some("web.shop.localhost"));
        assert_eq!(
            parse_host(b"GET / HTTP/1.1\r\nhost: a.localhost\r\n\r\n").as_deref(),
            Some("a.localhost")
        );
        assert_eq!(parse_host(b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n"), None);
        assert_eq!(parse_host(b"\xff\xfe"), None);
    }

    #[test]
    fn https_requests_tell_the_app_they_were_https() {
        let out = with_forwarded_proto(b"GET / HTTP/1.1\r\nHost: a.localhost\r\n\r\n");
        assert_eq!(
            out,
            b"GET / HTTP/1.1\r\nx-forwarded-proto: https\r\nHost: a.localhost\r\n\r\n"
        );
        assert_eq!(with_forwarded_proto(b"junk"), b"junk");
    }

    #[test]
    fn an_unknown_name_lists_the_known_ones() {
        let r = not_found(Some("nope.localhost"), &["web.shop.localhost".into()]);
        assert!(
            r.starts_with("HTTP/1.1 404")
                && r.contains("http://web.shop.localhost")
                && r.contains("nope.localhost"),
            "{r}"
        );
        assert!(not_found(None, &[]).contains("none yet"));
    }
}
