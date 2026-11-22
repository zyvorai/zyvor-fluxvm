// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! One direct HTTP/1.1 request from this host, reported step by step: dns, tcp, tls, http and (optionally) which backend
//! answered. Ported from the probe of Zyvor Setura 0.1.0.
//!
//! * `--address` connects to a numeric IP while the request keeps the URL's hostname in `Host` and in the TLS server name,
//!   so a listener that is only reachable through `kubectl port-forward` is tested with the real hostname.
//! * Proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) are never read: this opens a raw
//!   socket, so nothing between this host and the address can be silently involved.
//! * With `--ca`, the PEM bundle is trusted in addition to the system roots, for this request only.
//! * The body is never read; only the status line and headers matter.
//!
//! Blocking (std sockets and rustls), no async runtime needed.

use rustls::pki_types::{CertificateDer, ServerName};
use serde::Serialize;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The response header a backend uses to say who it is (`--backend` compares against it).
pub const BACKEND_HEADER: &str = "X-Setura-Backend";

const MAX_HEAD: usize = 64 * 1024;

/// One step of the report. `state` is `untested`, `unused`, `overridden`, `ok` or `failed`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Step {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addresses: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Step {
    fn new(state: &str) -> Step {
        Step {
            state: state.to_owned(),
            ..Default::default()
        }
    }

    /// The `key=value` pairs after the state, for the text output.
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![];
        if let Some(a) = &self.addresses {
            v.push(("addresses", a.join(",")));
        }
        if let Some(p) = &self.peer {
            v.push(("peer", p.clone()));
        }
        if let Some(x) = &self.version {
            v.push(("version", x.clone()));
        }
        if let Some(x) = self.status {
            v.push(("status", x.to_string()));
        }
        if let Some(x) = self.expected {
            v.push(("expected", x.to_string()));
        }
        if let Some(x) = &self.identity {
            v.push(("identity", x.clone()));
        }
        if let Some(x) = &self.reason {
            v.push(("reason", x.clone()));
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Steps {
    pub dns: Step,
    pub tcp: Step,
    pub tls: Step,
    pub http: Step,
    pub backend: Step,
}

impl Steps {
    pub fn named(&self) -> [(&'static str, &Step); 5] {
        [
            ("dns", &self.dns),
            ("tcp", &self.tcp),
            ("tls", &self.tls),
            ("http", &self.http),
            ("backend", &self.backend),
        ]
    }
    fn get_mut(&mut self, stage: &str) -> &mut Step {
        match stage {
            "dns" => &mut self.dns,
            "tcp" => &mut self.tcp,
            "tls" => &mut self.tls,
            "http" => &mut self.http,
            _ => &mut self.backend,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Report {
    pub url: String,
    pub ok: bool,
    pub steps: Steps,
    pub elapsed_ms: f64,
}

/// What to probe. `status` is signed so an out-of-range value is a usage error here rather than a parse error.
#[derive(Debug, Clone)]
pub struct ProbeOpts {
    pub url: String,
    pub address: Option<String>,
    pub ca: Option<String>,
    pub status: i64,
    pub backend: Option<String>,
    pub timeout: f64,
}

impl ProbeOpts {
    pub fn new(url: &str) -> ProbeOpts {
        ProbeOpts {
            url: url.to_owned(),
            address: None,
            ca: None,
            status: 200,
            backend: None,
            timeout: 4.0,
        }
    }
}

/// A parsed absolute http(s) URL.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub secure: bool,
    /// Lower-cased, without brackets for an IPv6 literal.
    pub host: String,
    pub port: u16,
    /// The authority as written, sent as the `Host` header.
    pub netloc: String,
    /// `/path?query`, `/` when the URL has neither.
    pub path_query: String,
}

/// An absolute `http` or `https` URL without credentials, fragment, spaces or control characters.
pub fn parse_url(url: &str) -> Result<Target, String> {
    const BAD: &str = "expected an absolute HTTP(S) URL without credentials";
    if url.bytes().any(|b| b < 33 || b == 127) || !url.is_ascii() {
        return Err(
            "encode spaces and omit fragments and control characters (the URL must be plain ASCII)"
                .into(),
        );
    }
    if url.contains('#') {
        return Err("encode spaces and omit fragments and control characters".into());
    }
    let (scheme, rest) = url.split_once("://").ok_or(BAD)?;
    let secure = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return Err(BAD.into()),
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (netloc, tail) = rest.split_at(end);
    if netloc.contains('@') {
        return Err(BAD.into());
    }
    let (host, port_text) = if let Some(inner) = netloc.strip_prefix('[') {
        let (h, after) = inner.split_once(']').ok_or(BAD)?;
        let p = match after {
            "" => None,
            a => Some(a.strip_prefix(':').ok_or(BAD)?),
        };
        (h, p)
    } else {
        match netloc.split_once(':') {
            Some((h, p)) => {
                if p.contains(':') {
                    return Err(BAD.into());
                }
                (h, Some(p))
            }
            None => (netloc, None),
        }
    };
    if host.is_empty() {
        return Err(BAD.into());
    }
    let port = match port_text {
        None | Some("") => {
            if secure {
                443
            } else {
                80
            }
        }
        Some(p) => {
            if !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("invalid port {p:?}"));
            }
            match p.parse::<u32>() {
                Ok(n) if (1..=65535).contains(&n) => n as u16,
                _ => return Err("invalid port or expected HTTP status".into()),
            }
        }
    };
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q),
        None => (tail, ""),
    };
    let mut path_query = if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    };
    if !query.is_empty() {
        path_query.push('?');
        path_query.push_str(query);
    }
    Ok(Target {
        secure,
        host: host.to_ascii_lowercase(),
        port,
        netloc: netloc.to_owned(),
        path_query,
    })
}

/// Name lookup, replaceable in tests.
pub type Resolve<'a> = &'a dyn Fn(&str, u16) -> std::io::Result<Vec<SocketAddr>>;

fn system_resolve(host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
    Ok((host, port).to_socket_addrs()?.collect())
}

/// Probes a URL with the system resolver.
pub fn probe(o: &ProbeOpts) -> Result<Report, String> {
    probe_with(o, &system_resolve)
}

fn tls_config(ca: Option<&str>) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native.certs);
    if let Some(path) = ca {
        let pem = std::fs::read(path).map_err(|e| format!("reading {path}: {e}"))?;
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            let cert: CertificateDer<'static> = cert.map_err(|e| format!("{path}: {e}"))?;
            roots.add(cert).map_err(|e| format!("{path}: {e}"))?;
            added += 1;
        }
        if added == 0 {
            return Err(format!("{path}: no PEM certificate found"));
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Sends the request and reads the status line and headers. The body is not read.
fn exchange<S: Read + Write>(
    s: &mut S,
    request: &str,
) -> Result<(u16, Vec<(String, String)>), String> {
    s.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    s.flush().map_err(|e| e.to_string())?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err("response headers are larger than 64 KiB".into());
        }
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the response headers were complete".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("not an HTTP response: {status_line:?}"));
    }
    let status = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|c| (100..=599).contains(c))
        .ok_or_else(|| format!("bad status line: {status_line:?}"))?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Ok((status, headers))
}

/// [`probe`] with a replaceable resolver. `Err` is a usage error (exit 2); an observed failure is a [`Report`] with `ok`
/// false (exit 1).
pub fn probe_with(o: &ProbeOpts, resolve: Resolve<'_>) -> Result<Report, String> {
    let target = parse_url(&o.url)?;
    if !(100..=599).contains(&o.status) {
        return Err("invalid port or expected HTTP status".into());
    }
    let expected = o.status as u16;
    if !(o.timeout > 0.0 && o.timeout <= 60.0) {
        return Err("timeout must be positive and at most 60 seconds".into());
    }
    let timeout = Duration::from_secs_f64(o.timeout);
    let override_ip: Option<IpAddr> = match &o.address {
        Some(a) => Some(
            a.parse()
                .map_err(|_| format!("--address {a:?} is not a numeric IP address"))?,
        ),
        None => None,
    };
    let mut steps = Steps {
        dns: Step::new("untested"),
        tcp: Step::new("untested"),
        tls: Step::new("untested"),
        http: Step::new("untested"),
        backend: Step::new("untested"),
    };
    if !target.secure {
        steps.tls = Step::new("unused");
    }
    if o.backend.is_none() {
        steps.backend = Step::new("unused");
    }
    let start = Instant::now();
    let mut stage = "dns";
    let result = run_stages(
        o,
        &target,
        override_ip,
        expected,
        timeout,
        resolve,
        &mut steps,
        &mut stage,
    );
    let ok = match result {
        Ok(()) => true,
        Err(reason) => {
            let s = steps.get_mut(stage);
            s.state = "failed".into();
            s.reason = Some(reason);
            false
        }
    };
    let elapsed_ms = (start.elapsed().as_secs_f64() * 10_000.0).round() / 10.0;
    Ok(Report {
        url: o.url.clone(),
        ok,
        steps,
        elapsed_ms,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_stages(
    o: &ProbeOpts,
    t: &Target,
    override_ip: Option<IpAddr>,
    expected: u16,
    timeout: Duration,
    resolve: Resolve<'_>,
    steps: &mut Steps,
    stage: &mut &'static str,
) -> Result<(), String> {
    // dns: with --address the name is never looked up.
    let candidates: Vec<SocketAddr> = match override_ip {
        Some(ip) => vec![SocketAddr::new(ip, t.port)],
        None => resolve(&t.host, t.port).map_err(|e| e.to_string())?,
    };
    let mut seen: Vec<String> = vec![];
    for c in &candidates {
        let ip = c.ip().to_string();
        if !seen.contains(&ip) {
            seen.push(ip);
        }
    }
    steps.dns = Step {
        state: if override_ip.is_some() {
            "overridden"
        } else {
            "ok"
        }
        .into(),
        addresses: Some(seen),
        ..Default::default()
    };

    // tcp: the first address that accepts.
    *stage = "tcp";
    let mut last = String::from("no addresses returned");
    let mut sock: Option<TcpStream> = None;
    for addr in &candidates {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    let mut sock = sock.ok_or(last)?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    sock.set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    steps.tcp = Step {
        state: "ok".into(),
        peer: sock.peer_addr().ok().map(|a| a.ip().to_string()),
        ..Default::default()
    };

    // tls: the server name is the URL's hostname, whatever --address says.
    let mut tls: Option<rustls::ClientConnection> = None;
    if t.secure {
        *stage = "tls";
        let config = tls_config(o.ca.as_deref())?;
        let name = ServerName::try_from(t.host.clone())
            .map_err(|e| format!("invalid server name {:?}: {e}", t.host))?;
        let mut conn = rustls::ClientConnection::new(config, name).map_err(|e| e.to_string())?;
        while conn.is_handshaking() {
            conn.complete_io(&mut sock).map_err(|e| e.to_string())?;
        }
        let version = match conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3".to_owned(),
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2".to_owned(),
            Some(other) => format!("{other:?}"),
            None => "unknown".to_owned(),
        };
        steps.tls = Step {
            state: "ok".into(),
            version: Some(version),
            ..Default::default()
        };
        tls = Some(conn);
    }

    // http
    *stage = "http";
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: fluxctl-gateway\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        t.path_query, t.netloc
    );
    let (status, headers) = match tls.as_mut() {
        Some(conn) => exchange(&mut rustls::Stream::new(conn, &mut sock), &request)?,
        None => exchange(&mut sock, &request)?,
    };
    steps.http = Step {
        state: "ok".into(),
        status: Some(status),
        expected: Some(expected),
        ..Default::default()
    };
    if status != expected {
        return Err(format!("expected HTTP {expected}, received {status}"));
    }

    // backend
    if let Some(want) = &o.backend {
        *stage = "backend";
        let actual = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(BACKEND_HEADER))
            .map(|(_, v)| v.as_str());
        if actual != Some(want.as_str()) {
            return Err(match actual {
                Some(a) => format!("expected {want:?}, received {a:?}"),
                None => format!("expected {want:?}, received no {BACKEND_HEADER} header"),
            });
        }
        steps.backend = Step {
            state: "ok".into(),
            identity: actual.map(str::to_owned),
            ..Default::default()
        };
    }
    Ok(())
}

/// The text form of a report: one line per step.
pub fn render_text(r: &Report) -> String {
    let mut out = String::new();
    for (name, s) in r.steps.named() {
        out.push_str(&format!("{name:8} {:12}", s.state));
        for (k, v) in s.fields() {
            out.push_str(&format!(" {k}={v}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Serves GET requests on a loopback port. Answers 200 with `X-Setura-Backend: orders-v1` only for
    /// `Host: orders.mactra.test:PORT` and `/checkout?q=1`, otherwise 404. Optionally wrapped in TLS.
    struct Server {
        port: u16,
        stop: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start(tls: Option<Arc<rustls::ServerConfig>>) -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            let handle = thread::spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut s, _)) => {
                            let tls = tls.clone();
                            thread::spawn(move || {
                                s.set_nonblocking(false).ok();
                                s.set_read_timeout(Some(Duration::from_secs(5))).ok();
                                match tls {
                                    Some(cfg) => {
                                        let Ok(mut conn) = rustls::ServerConnection::new(cfg)
                                        else {
                                            return;
                                        };
                                        let mut st = rustls::Stream::new(&mut conn, &mut s);
                                        serve_one(&mut st, port);
                                    }
                                    None => serve_one(&mut s, port),
                                }
                            });
                        }
                        Err(_) => thread::sleep(Duration::from_millis(10)),
                    }
                }
            });
            Server {
                port,
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, scheme: &str, host: &str) -> String {
            format!("{scheme}://{host}:{}/checkout?q=1", self.port)
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve_one<S: Read + Write>(s: &mut S, port: u16) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let first = text.lines().next().unwrap_or("");
        let host_ok = text
            .lines()
            .any(|l| l == format!("Host: orders.mactra.test:{port}"));
        let good = host_ok && first == "GET /checkout?q=1 HTTP/1.1";
        let code = if good { "200 OK" } else { "404 Not Found" };
        let _ = s.write_all(format!("HTTP/1.1 {code}\r\nX-Setura-Backend: orders-v1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        let _ = s.flush();
    }

    fn opts(url: &str) -> ProbeOpts {
        ProbeOpts {
            address: Some("127.0.0.1".into()),
            ..ProbeOpts::new(url)
        }
    }

    #[test]
    fn host_path_and_identity_are_preserved() {
        let s = Server::start(None);
        let r = probe(&ProbeOpts {
            backend: Some("orders-v1".into()),
            ..opts(&s.url("http", "orders.mactra.test"))
        })
        .unwrap();
        assert!(r.ok, "{r:?}");
        assert_eq!(r.steps.dns.state, "overridden");
        assert_eq!(r.steps.tls.state, "unused");
        assert_eq!(r.steps.backend.identity.as_deref(), Some("orders-v1"));
    }

    #[test]
    fn a_negative_route_can_be_the_expected_outcome() {
        let s = Server::start(None);
        let url = s
            .url("http", "orders.mactra.test")
            .replace("checkout", "wrong");
        assert!(
            probe(&ProbeOpts {
                status: 404,
                ..opts(&url)
            })
            .unwrap()
            .ok
        );
    }

    #[test]
    fn wrong_backend_fails_at_the_backend_step() {
        let s = Server::start(None);
        let r = probe(&ProbeOpts {
            backend: Some("orders-v2".into()),
            ..opts(&s.url("http", "orders.mactra.test"))
        })
        .unwrap();
        assert!(!r.ok);
        assert_eq!(r.steps.http.state, "ok");
        assert_eq!(r.steps.backend.state, "failed");
    }

    #[test]
    fn http_status_mismatch_leaves_the_backend_untested() {
        let s = Server::start(None);
        let r = probe(&ProbeOpts {
            status: 204,
            backend: Some("orders-v1".into()),
            ..opts(&s.url("http", "orders.mactra.test"))
        })
        .unwrap();
        assert_eq!(r.steps.http.status, Some(200));
        assert_eq!(r.steps.http.state, "failed");
        assert_eq!(r.steps.backend.state, "untested");
    }

    #[test]
    fn dns_failure_stops_before_tcp() {
        let fail = |_: &str, _: u16| -> std::io::Result<Vec<SocketAddr>> {
            Err(std::io::Error::other("no DNS"))
        };
        let r = probe_with(&ProbeOpts::new("https://orders.mactra.test"), &fail).unwrap();
        assert_eq!(r.steps.dns.state, "failed");
        assert_eq!(r.steps.tcp.state, "untested");
        assert_eq!(r.steps.dns.reason.as_deref(), Some("no DNS"));
    }

    #[test]
    fn a_name_without_addresses_is_a_dns_failure() {
        let none = |_: &str, _: u16| -> std::io::Result<Vec<SocketAddr>> { Ok(vec![]) };
        let r = probe_with(&ProbeOpts::new("http://nowhere.test"), &none).unwrap();
        assert_eq!(r.steps.tcp.state, "failed");
        assert_eq!(r.steps.tcp.reason.as_deref(), Some("no addresses returned"));
    }

    #[test]
    fn with_an_address_the_resolver_is_never_asked() {
        let s = Server::start(None);
        let boom = |_: &str, _: u16| -> std::io::Result<Vec<SocketAddr>> {
            panic!("resolver must not run")
        };
        let r = probe_with(&opts(&s.url("http", "orders.mactra.test")), &boom).unwrap();
        assert!(r.ok);
    }

    #[test]
    fn refused_tcp_stops_before_tls() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reserved.local_addr().unwrap().port();
        drop(reserved);
        let r = probe(&ProbeOpts::new(&format!("https://127.0.0.1:{port}"))).unwrap();
        assert_eq!(r.steps.tcp.state, "failed");
        assert_eq!(r.steps.tls.state, "untested");
        assert!(!r.ok);
    }

    #[test]
    fn invalid_input_is_a_usage_error() {
        for url in [
            "file:///tmp/x",
            "http://a@localhost/",
            "http://localhost:0",
            "http://localhost:x/",
            "http://h/p#frag",
            "http://h/a b",
            "ftp://h/",
            "localhost",
            "http:///x",
        ] {
            assert!(probe(&ProbeOpts::new(url)).is_err(), "{url}");
        }
        assert!(
            probe(&ProbeOpts {
                status: 99,
                ..ProbeOpts::new("http://localhost")
            })
            .is_err()
        );
        assert!(
            probe(&ProbeOpts {
                status: 600,
                ..ProbeOpts::new("http://localhost")
            })
            .is_err()
        );
        for t in [0.0, -1.0, 61.0, f64::NAN] {
            assert!(
                probe(&ProbeOpts {
                    timeout: t,
                    ..ProbeOpts::new("http://localhost")
                })
                .is_err(),
                "{t}"
            );
        }
        assert!(
            probe(&ProbeOpts {
                address: Some("localhost".into()),
                ..ProbeOpts::new("http://localhost")
            })
            .is_err()
        );
    }

    #[test]
    fn url_parts() {
        let t = parse_url("https://Orders.Mactra.test/checkout?q=1").unwrap();
        assert_eq!(
            (
                t.secure,
                t.host.as_str(),
                t.port,
                t.netloc.as_str(),
                t.path_query.as_str()
            ),
            (
                true,
                "orders.mactra.test",
                443,
                "Orders.Mactra.test",
                "/checkout?q=1"
            )
        );
        let t = parse_url("http://[::1]:8080").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path_query.as_str()),
            ("::1", 8080, "/")
        );
        assert_eq!(parse_url("http://h?x=1").unwrap().path_query, "/?x=1");
        assert_eq!(parse_url("http://h:/p").unwrap().port, 80);
    }

    #[test]
    fn proxy_environment_is_not_consulted() {
        // The probe opens raw sockets and never reads these variables. Nothing to set here: this documents the property by
        // asserting that the source has no HTTP client in it.
        let src = include_str!("probe.rs");
        let client = ["req", "west"].concat();
        let proxy = ["HTTP", "_PROXY"].concat();
        assert!(!src.split("#[cfg(test)]").next().unwrap().contains(&client));
        assert!(
            !src.split("#[cfg(test)]")
                .next()
                .unwrap()
                .contains(&format!("var(\"{proxy}"))
        );
    }

    // ---- TLS: a throwaway CA and a leaf for orders.mactra.test, made with rcgen.

    struct TestPki {
        ca_pem: String,
        server: Arc<rustls::ServerConfig>,
    }

    fn pki(leaf_name: &str) -> TestPki {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
        };
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "gateway test CA");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);
        let key = KeyPair::generate().unwrap();
        let mut leaf = CertificateParams::new(vec![leaf_name.to_owned()]).unwrap();
        leaf.is_ca = IsCa::NoCa;
        let cert = leaf.signed_by(&key, &issuer).unwrap();
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.der().to_vec())], der)
        .unwrap();
        TestPki {
            ca_pem: ca.pem(),
            server: Arc::new(cfg),
        }
    }

    fn write_ca(pem: &str) -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("ca.pem");
        std::fs::write(&p, pem).unwrap();
        let s = p.display().to_string();
        (d, s)
    }

    #[test]
    fn tls_trust_and_hostname() {
        let pki = pki("orders.mactra.test");
        let (_d, ca) = write_ca(&pki.ca_pem);
        let s = Server::start(Some(pki.server.clone()));
        // trusted CA and matching name: the real hostname is the SNI/verification name although we dial 127.0.0.1
        let ok = probe(&ProbeOpts {
            ca: Some(ca.clone()),
            ..opts(&s.url("https", "orders.mactra.test"))
        })
        .unwrap();
        assert!(ok.ok, "{ok:?}");
        assert_eq!(ok.steps.tls.state, "ok");
        assert!(
            ok.steps
                .tls
                .version
                .as_deref()
                .is_some_and(|v| v.starts_with("TLSv1."))
        );
        // no --ca: the private CA is not in the system roots
        let untrusted = probe(&opts(&s.url("https", "orders.mactra.test"))).unwrap();
        assert_eq!(untrusted.steps.tls.state, "failed");
        assert_eq!(untrusted.steps.http.state, "untested");
        // a different hostname than the certificate's
        let bad = probe(&ProbeOpts {
            ca: Some(ca),
            ..opts(&s.url("https", "other.mactra.test"))
        })
        .unwrap();
        assert_eq!(bad.steps.tls.state, "failed");
    }

    #[test]
    fn an_unreadable_or_empty_ca_is_a_tls_failure() {
        let pki = pki("orders.mactra.test");
        let s = Server::start(Some(pki.server.clone()));
        let r = probe(&ProbeOpts {
            ca: Some("/nonexistent/ca.pem".into()),
            ..opts(&s.url("https", "orders.mactra.test"))
        })
        .unwrap();
        assert_eq!(r.steps.tls.state, "failed");
        let (_d, empty) = write_ca("not a certificate\n");
        let r = probe(&ProbeOpts {
            ca: Some(empty),
            ..opts(&s.url("https", "orders.mactra.test"))
        })
        .unwrap();
        assert_eq!(r.steps.tls.state, "failed");
    }

    #[test]
    fn text_output_lists_every_step() {
        let s = Server::start(None);
        let r = probe(&ProbeOpts {
            backend: Some("orders-v1".into()),
            ..opts(&s.url("http", "orders.mactra.test"))
        })
        .unwrap();
        let text = render_text(&r);
        let names: Vec<&str> = text
            .lines()
            .map(|l| l.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(names, ["dns", "tcp", "tls", "http", "backend"]);
        assert!(text.contains("overridden"));
        assert!(text.contains("identity=orders-v1"));
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.find("\"dns\"").unwrap() < json.find("\"backend\"").unwrap());
    }
}
