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
    // Any other `*.localhost` name is an Ingress host if the cluster runs Traefik (`kube up --traefik`): hand it over with
    // its Host header intact and let the Ingress rules decide.
    if let Some(web) = published
        .iter()
        .filter(|s| s.namespace == "kube-system" && s.name == "traefik" && s.service_port == 80)
        .min_by_key(|s| s.port)
    {
        t.insert(INGRESS.to_owned(), web.host_port());
    }
    t
}

/// The route-table key for the ingress controller; not a host name, so it never matches a request or shows in listings.
pub(crate) const INGRESS: &str = "*ingress";

/// The host of an HTTP request head, lower-cased and without a port.
pub(crate) fn parse_host(head: &[u8]) -> Option<String> {
    // Only the header block: what follows may be the start of a binary request body.
    let end = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(head.len(), |i| i + 2);
    let text = String::from_utf8_lossy(&head[..end]);
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
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

/// The port for a request's host: its service, or for any other `*.localhost` name the ingress controller, if there is one.
pub(crate) fn pick_port(table: &BTreeMap<String, u16>, host: Option<&str>) -> Option<u16> {
    let host = host?;
    table.get(host).copied().or_else(|| {
        (host.ends_with(".localhost") && host != "localhost")
            .then(|| table.get(INGRESS).copied())
            .flatten()
    })
}

/// The request head as it is sent on to the service. A client's own `x-forwarded-proto` is dropped and, for https, ours is
/// added. The proxy routes by the first request's Host only, so unless the client is upgrading the connection (a WebSocket)
/// the head also asks for `connection: close`, which makes the next request open a new connection and be routed afresh.
pub fn normalize_head(head: &[u8], https: bool) -> Vec<u8> {
    let Some(split) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
        return head.to_vec(); // not a complete head: send it untouched
    };
    let (block, rest) = head.split_at(split);
    let mut lines = block
        .split(|b| *b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let Some(request_line) = lines.next() else {
        return head.to_vec();
    };
    let name_of = |l: &[u8]| l.iter().position(|b| *b == b':').map(|i| l[..i].to_vec());
    let mut kept: Vec<&[u8]> = vec![];
    let mut upgrading = false;
    for l in lines {
        match name_of(l) {
            Some(n) if n.eq_ignore_ascii_case(b"x-forwarded-proto") => {}
            Some(n) if n.eq_ignore_ascii_case(b"upgrade") => {
                upgrading = true;
                kept.push(l);
            }
            _ => kept.push(l),
        }
    }
    let mut out = request_line.to_vec();
    for l in kept {
        // a Connection header is replaced below, unless the client is upgrading and needs its own
        if !upgrading && name_of(l).is_some_and(|n| n.eq_ignore_ascii_case(b"connection")) {
            continue;
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(l);
    }
    if https {
        out.extend_from_slice(b"\r\nx-forwarded-proto: https");
    }
    if !upgrading {
        out.extend_from_slice(b"\r\nconnection: close");
    }
    out.extend_from_slice(rest);
    out
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
            pick_port(&t, host.as_deref()),
            t.keys()
                .filter(|k| *k != INGRESS)
                .cloned()
                .collect::<Vec<_>>(),
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
    let head = normalize_head(&head, https);
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
    // At most this many connections at once: each can wait up to 20 s for its service, and a loop of local clients must not
    // be able to pile up tasks without end.
    let permits = Arc::new(tokio::sync::Semaphore::new(512));
    loop {
        let Ok((client, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await; // out of descriptors, say: do not spin
            continue;
        };
        if wide && !peer.ip().is_loopback() {
            continue; // another machine on the network: not for us
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let names = names.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
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
            service_port: 0,
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
    fn unknown_localhost_names_go_to_traefik_when_it_is_there() {
        let mut web = svc("kube-system", "traefik", 31080);
        web.service_port = 80;
        let mut tls = svc("kube-system", "traefik", 30443);
        tls.service_port = 443;
        let t = route_table(&[svc("default", "api", 30090), tls, web]);
        assert_eq!(t[INGRESS], 31080, "the service's port 80, not its 443");
        assert_eq!(pick_port(&t, Some("api.localhost")), Some(30090));
        assert_eq!(
            pick_port(&t, Some("shop.localhost")),
            Some(31080),
            "an Ingress host"
        );
        assert_eq!(pick_port(&t, Some("example.com")), None);
        assert_eq!(pick_port(&t, Some("localhost")), None);
        assert_eq!(pick_port(&t, None), None);
        // without Traefik an unknown name is still an unknown name
        let none = route_table(&[svc("default", "api", 30090)]);
        assert_eq!(pick_port(&none, Some("shop.localhost")), None);
    }

    #[test]
    fn the_head_is_normalized_for_the_service() {
        let plain = normalize_head(
            b"GET / HTTP/1.1\r\nHost: a.localhost\r\nConnection: keep-alive\r\n\r\n",
            false,
        );
        assert_eq!(
            plain,
            b"GET / HTTP/1.1\r\nHost: a.localhost\r\nconnection: close\r\n\r\n"
        );
        let tls = normalize_head(
            b"GET / HTTP/1.1\r\nHost: a.localhost\r\nX-Forwarded-Proto: http\r\n\r\nBODY",
            true,
        );
        assert_eq!(
            tls,
            b"GET / HTTP/1.1\r\nHost: a.localhost\r\nx-forwarded-proto: https\r\nconnection: close\r\n\r\nBODY",
            "a client's own value is dropped, and the body is untouched"
        );
        // a WebSocket keeps its Connection: Upgrade and is not told to close
        let ws = normalize_head(b"GET /ws HTTP/1.1\r\nHost: a.localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n", false);
        assert_eq!(
            ws,
            b"GET /ws HTTP/1.1\r\nHost: a.localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
        );
        assert_eq!(
            normalize_head(b"GET / HTTP/1.1\r\nHost: a", false),
            b"GET / HTTP/1.1\r\nHost: a",
            "an incomplete head is untouched"
        );
    }

    #[test]
    fn a_binary_body_or_an_odd_header_line_does_not_hide_the_host() {
        let mut req =
            b"POST /up HTTP/1.1\r\nHost: Web.localhost\r\nContent-Length: 4\r\n\r\n".to_vec();
        req.extend_from_slice(&[0xff, 0xfe, 0x00, 0x80]);
        assert_eq!(parse_host(&req).as_deref(), Some("web.localhost"));
        assert_eq!(
            parse_host(b"GET / HTTP/1.1\r\nbroken header line\r\nHost: a.localhost\r\n\r\n")
                .as_deref(),
            Some("a.localhost")
        );
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
