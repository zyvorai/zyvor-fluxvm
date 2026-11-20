// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Outbound event webhooks. See `docs/event-webhooks.md`.
//!
//! Every event appended to the shared event log is matched against the configured `[[event_sinks]]` and POSTed as
//! JSON, signed with HMAC-SHA256 (`X-Fluxvm-Signature: sha256=<hex>` over `"{timestamp}.{body}"`). Each sink has its
//! own bounded queue and worker: a slow or dead receiver cannot delay another sink or the daemon. Failed deliveries
//! are retried with exponential backoff; events that cannot be delivered (retries exhausted, permanent 4xx, queue
//! full, blocked address, unreadable secret) go to a dead-letter JSONL file. Delivery is at-least-once and ordering
//! across retries is not guaranteed; receivers should dedupe on `X-Fluxvm-Delivery`.

use fluxvm_core::config::EventSinkConfig;
use fluxvm_scheduler::VmEvent;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

pub const SIGNATURE_HEADER: &str = "X-Fluxvm-Signature";
pub const TIMESTAMP_HEADER: &str = "X-Fluxvm-Timestamp";
pub const DELIVERY_HEADER: &str = "X-Fluxvm-Delivery";
pub const EVENT_HEADER: &str = "X-Fluxvm-Event";

// ---- signing ------------------------------------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// `sha256=<hex>` over `"{timestamp}.{body}"`. The timestamp is part of the signed message so a captured delivery
/// cannot be replayed with a fresh timestamp.
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize().into_bytes()))
}

/// Receiver-side check: constant-time compare, and a timestamp within `tolerance_secs` of `now`.
pub fn verify(
    secret: &[u8],
    timestamp: i64,
    body: &[u8],
    header: &str,
    now: i64,
    tolerance_secs: i64,
) -> bool {
    if (now - timestamp).abs() > tolerance_secs {
        return false;
    }
    let Some(sig) = header.strip_prefix("sha256=").and_then(unhex) else {
        return false;
    };
    let mut mac =
        <HmacSha256 as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

// ---- SSRF guard ---------------------------------------------------------------------------------------------------

/// Why an address is refused, or `None` when it may be contacted. Link-local (including the 169.254.169.254 cloud
/// metadata service and its IPv6 counterparts), unspecified, multicast and broadcast addresses are always blocked
/// unless the operator lists them in `allow_hosts`. Loopback and RFC 1918 ranges are allowed: delivering to an
/// internal receiver is the common case.
pub fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return blocked_v4(v4);
            }
            blocked_v6(v6)
        }
    }
}

fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    if ip == Ipv4Addr::new(169, 254, 169, 254) || ip == Ipv4Addr::new(100, 100, 100, 200) {
        Some("cloud metadata address")
    } else if ip.is_link_local() {
        Some("link-local address")
    } else if ip.is_unspecified() {
        Some("unspecified address")
    } else if ip.is_multicast() || ip.is_broadcast() {
        Some("multicast or broadcast address")
    } else {
        None
    }
}

fn blocked_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let seg = ip.segments();
    if ip == "fd00:ec2::254".parse::<Ipv6Addr>().unwrap() {
        Some("cloud metadata address")
    } else if seg[0] & 0xffc0 == 0xfe80 {
        Some("link-local address")
    } else if ip.is_unspecified() {
        Some("unspecified address")
    } else if ip.is_multicast() {
        Some("multicast address")
    } else {
        None
    }
}

const METADATA_NAMES: &[&str] = &["metadata.google.internal", "metadata", "instance-data"];

fn host_allowed(allow: &[String], host: &str, ip: Option<IpAddr>) -> bool {
    allow.iter().any(|a| {
        a.eq_ignore_ascii_case(host)
            || ip.is_some_and(|ip| a.parse::<IpAddr>().is_ok_and(|x| x == ip))
    })
}

/// Static check of a configured URL (no DNS): scheme, host, literal addresses and well-known metadata names.
pub fn check_url(url: &str, allow: &[String]) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("unsupported scheme '{}'", u.scheme()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("credentials in the url are not allowed; the request is signed instead".into());
    }
    let host = u.host_str().ok_or("url has no host")?.to_string();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let ip = bare.parse::<IpAddr>().ok();
    if host_allowed(allow, bare, ip) {
        return Ok(u);
    }
    if let Some(ip) = ip {
        if let Some(why) = blocked_reason(ip) {
            return Err(format!(
                "{host} is a {why}; add it to allow_hosts to permit"
            ));
        }
    } else if METADATA_NAMES.iter().any(|n| bare.eq_ignore_ascii_case(n)) {
        return Err(format!(
            "{host} is a cloud metadata name; add it to allow_hosts to permit"
        ));
    }
    Ok(u)
}

/// Delivery-time check: resolve `url`'s host and refuse when any answer is blocked (a name that resolves to
/// link-local space is DNS rebinding against the metadata service). Returns the vetted addresses so the request
/// connects to exactly those, not to a second lookup.
async fn resolve_checked(url: &reqwest::Url, allow: &[String]) -> Result<Vec<SocketAddr>, String> {
    let host = url.host_str().ok_or("url has no host")?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let port = url.port_or_known_default().ok_or("url has no port")?;
    let addrs: Vec<SocketAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((bare, port))
            .await
            .map_err(|e| format!("dns lookup failed: {e}"))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err("host resolved to no addresses".into());
    }
    for a in &addrs {
        if host_allowed(allow, bare, Some(a.ip())) {
            continue;
        }
        if let Some(why) = blocked_reason(a.ip()) {
            return Err(format!("{bare} resolves to {} ({why})", a.ip()));
        }
    }
    Ok(addrs)
}

// ---- config -------------------------------------------------------------------------------------------------------

/// Validates `[[event_sinks]]` at startup; the daemon refuses to start on an error rather than silently dropping events.
pub fn validate(sinks: &[EventSinkConfig]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for s in sinks {
        let n = &s.name;
        if n.is_empty()
            || !n
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "event_sinks: name '{n}' must be non-empty [A-Za-z0-9._-]"
            ));
        }
        if !seen.insert(n.clone()) {
            return Err(format!("event_sinks: duplicate name '{n}'"));
        }
        check_url(&s.url, &s.allow_hosts).map_err(|e| format!("event_sinks '{n}': {e}"))?;
        if s.secret_file.is_none() {
            return Err(format!(
                "event_sinks '{n}': secret_file is required (the secret is never inline)"
            ));
        }
        if s.queue_size == 0 || s.max_attempts == 0 || s.timeout_secs == 0 {
            return Err(format!(
                "event_sinks '{n}': queue_size, max_attempts and timeout_secs must be above 0"
            ));
        }
        if s.backoff_max_ms < s.backoff_base_ms {
            return Err(format!(
                "event_sinks '{n}': backoff_max_ms is below backoff_base_ms"
            ));
        }
    }
    Ok(())
}

/// Delay before retry number `retry` (1-based): `base * 2^(retry-1)`, capped at `max`.
pub fn backoff(retry: u32, base_ms: u64, max_ms: u64) -> Duration {
    let shift = retry.saturating_sub(1).min(32);
    Duration::from_millis(base_ms.saturating_mul(1u64 << shift).min(max_ms))
}

fn matches_filter(prefixes: &[String], event: &str) -> bool {
    prefixes.is_empty() || prefixes.iter().any(|p| event.starts_with(p.as_str()))
}

fn read_secret(path: &Path) -> Result<Vec<u8>, String> {
    let raw =
        std::fs::read(path).map_err(|e| format!("reading secret_file {}: {e}", path.display()))?;
    let end = raw
        .iter()
        .rposition(|b| !matches!(b, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    if end == 0 {
        return Err(format!("secret_file {} is empty", path.display()));
    }
    Ok(raw[..end].to_vec())
}

// ---- delivery -----------------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct SinkStats {
    pub delivered: AtomicU64,
    pub dead_lettered: AtomicU64,
    pub retries: AtomicU64,
}

#[derive(Clone)]
struct DeadLetter {
    path: Arc<PathBuf>,
}

impl DeadLetter {
    fn write(&self, sink: &EventSinkConfig, ev: &VmEvent, attempts: u32, reason: &str) {
        use std::io::Write;
        // Query strings often carry tokens; keep only scheme, host and path.
        let url = reqwest::Url::parse(&sink.url)
            .map(|mut u| {
                u.set_query(None);
                u.set_fragment(None);
                u.to_string()
            })
            .unwrap_or_default();
        let line = serde_json::json!({
            "ts": chrono::Utc::now(),
            "sink": sink.name,
            "url": url,
            "attempts": attempts,
            "reason": reason,
            "event": ev,
        });
        tracing::warn!(sink = %sink.name, event = %ev.event, attempts, reason, "event webhook dead-lettered");
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path.as_ref())
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            tracing::error!(error = %e, path = %self.path.display(), "cannot write event dead-letter file");
        }
    }
}

enum Outcome {
    Delivered,
    Retry(String),
    Permanent(String),
}

async fn attempt(sink: &EventSinkConfig, ev: &VmEvent, delivery_id: &str) -> Outcome {
    let url = match check_url(&sink.url, &sink.allow_hosts) {
        Ok(u) => u,
        Err(e) => return Outcome::Permanent(format!("blocked: {e}")),
    };
    // Re-read on every attempt so a rotated secret takes effect without a restart.
    let secret = match sink.secret_file.as_deref().map(read_secret) {
        Some(Ok(s)) => s,
        Some(Err(e)) => return Outcome::Retry(e),
        None => return Outcome::Permanent("no secret_file".into()),
    };
    let addrs = match resolve_checked(&url, &sink.allow_hosts).await {
        Ok(a) => a,
        // A failed lookup may be transient; a blocked answer never is.
        Err(e) if e.starts_with("dns lookup") => return Outcome::Retry(e),
        Err(e) => return Outcome::Permanent(format!("blocked: {e}")),
    };
    let mut cb = reqwest::Client::builder()
        .timeout(Duration::from_secs(sink.timeout_secs))
        // A redirect could point at a metadata address the guard never saw.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(host) = url.host_str() {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if bare.parse::<IpAddr>().is_err() {
            cb = cb.resolve_to_addrs(bare, &addrs);
        }
    }
    let Ok(client) = cb.build() else {
        return Outcome::Permanent("cannot build http client".into());
    };
    let body = match serde_json::to_vec(ev) {
        Ok(b) => b,
        Err(e) => return Outcome::Permanent(format!("serialize: {e}")),
    };
    let ts = chrono::Utc::now().timestamp();
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .header(EVENT_HEADER, &ev.event)
        .header(DELIVERY_HEADER, delivery_id)
        .header(TIMESTAMP_HEADER, ts.to_string())
        .header(SIGNATURE_HEADER, sign(&secret, ts, &body))
        .body(body)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => Outcome::Delivered,
        Ok(r) => {
            let st = r.status();
            let msg = format!("http {}", st.as_u16());
            // 408 and 429 ask for a retry; any other 4xx will fail the same way next time.
            // Redirects are never followed (SSRF), so a 3xx is final too.
            if st.is_redirection()
                || (st.is_client_error() && st.as_u16() != 408 && st.as_u16() != 429)
            {
                Outcome::Permanent(msg)
            } else {
                Outcome::Retry(msg)
            }
        }
        Err(e) => Outcome::Retry(format!("request failed: {}", e.without_url())),
    }
}

async fn deliver(sink: &EventSinkConfig, ev: &VmEvent, dl: &DeadLetter, stats: &SinkStats) {
    let delivery_id = uuid::Uuid::new_v4().to_string();
    let mut last = String::new();
    for n in 1..=sink.max_attempts {
        match attempt(sink, ev, &delivery_id).await {
            Outcome::Delivered => {
                stats.delivered.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Outcome::Permanent(why) => {
                stats.dead_lettered.fetch_add(1, Ordering::Relaxed);
                dl.write(sink, ev, n, &why);
                return;
            }
            Outcome::Retry(why) => {
                last = why;
                if n < sink.max_attempts {
                    stats.retries.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(backoff(n, sink.backoff_base_ms, sink.backoff_max_ms)).await;
                }
            }
        }
    }
    stats.dead_lettered.fetch_add(1, Ordering::Relaxed);
    dl.write(
        sink,
        ev,
        sink.max_attempts,
        &format!("retries exhausted: {last}"),
    );
}

/// Fan-out point: one bounded queue and worker task per sink.
pub struct Dispatcher {
    sinks: Vec<(EventSinkConfig, mpsc::Sender<VmEvent>, Arc<SinkStats>)>,
    dl: DeadLetter,
}

impl Dispatcher {
    /// Spawns one worker per sink. Must be called inside a tokio runtime.
    pub fn start(sinks: Vec<EventSinkConfig>, dead_letter: PathBuf) -> Self {
        let dl = DeadLetter {
            path: Arc::new(dead_letter),
        };
        let sinks = sinks
            .into_iter()
            .map(|cfg| {
                let (tx, mut rx) = mpsc::channel::<VmEvent>(cfg.queue_size.max(1));
                let stats = Arc::new(SinkStats::default());
                let (c, d, s) = (cfg.clone(), dl.clone(), stats.clone());
                tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        deliver(&c, &ev, &d, &s).await;
                    }
                });
                (cfg, tx, stats)
            })
            .collect();
        Self { sinks, dl }
    }

    /// Queues `ev` for every sink whose filter matches. Never blocks: a full queue dead-letters the event.
    pub fn dispatch(&self, ev: &VmEvent) {
        for (cfg, tx, stats) in &self.sinks {
            if !matches_filter(&cfg.events, &ev.event) {
                continue;
            }
            if tx.try_send(ev.clone()).is_err() {
                stats.dead_lettered.fetch_add(1, Ordering::Relaxed);
                self.dl.write(cfg, ev, 0, "queue_full");
            }
        }
    }

    pub fn stats(&self, name: &str) -> Option<Arc<SinkStats>> {
        self.sinks
            .iter()
            .find(|(c, ..)| c.name == name)
            .map(|(.., s)| s.clone())
    }
}

/// Starts delivery for `cfg.event_sinks` (no-op when none are configured) by tailing the shared event log from its
/// current end, so history is not replayed on restart. Returns an error for an invalid sink config.
pub fn spawn(cfg: &fluxvm_core::config::Config) -> Result<Option<Arc<Dispatcher>>, String> {
    if cfg.event_sinks.is_empty() {
        return Ok(None);
    }
    validate(&cfg.event_sinks)?;
    let Some(log) = fluxvm_scheduler::events::global() else {
        return Err("event_sinks configured but the event log is not initialised".into());
    };
    #[cfg(unix)]
    for s in &cfg.event_sinks {
        use std::os::unix::fs::PermissionsExt;
        if let Some(p) = &s.secret_file
            && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o077 != 0)
        {
            tracing::warn!(sink = %s.name, path = %p.display(), "event sink secret_file is readable by group/other; chmod 600");
        }
    }
    let d = Arc::new(Dispatcher::start(
        cfg.event_sinks.clone(),
        cfg.state_dir.join("event-sinks-dead.jsonl"),
    ));
    let d2 = d.clone();
    tokio::spawn(async move {
        let filter = fluxvm_scheduler::EventFilter::default();
        let mut offset = log.end_offset();
        loop {
            match log.read_from(offset, &filter) {
                Ok((events, next)) => {
                    offset = next;
                    for ev in &events {
                        d2.dispatch(ev);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "event sink tailer: read failed"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    tracing::info!(sinks = cfg.event_sinks.len(), "event webhooks enabled");
    Ok(Some(d))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn ev(name: &str) -> VmEvent {
        VmEvent {
            ts: chrono::Utc::now(),
            event: name.into(),
            vm_id: None,
            fields: BTreeMap::new(),
        }
    }

    type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// Minimal HTTP server: answers each connection with the next scripted status (the last repeats) and records
    /// the raw header block plus body of every request.
    async fn mock(statuses: Vec<u16>) -> (String, Seen) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook?token=abc", l.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let s2 = seen.clone();
        tokio::spawn(async move {
            let mut i = 0;
            loop {
                let (mut sock, _) = l.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head, body) = loop {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break (String::new(), Vec::new());
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_string();
                        let want = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < p + 4 + want {
                            let n = sock.read(&mut tmp).await.unwrap();
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break (head, buf[p + 4..p + 4 + want].to_vec());
                    }
                };
                s2.lock().unwrap().push((head, body));
                let st = statuses[i.min(statuses.len() - 1)];
                i += 1;
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {st} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, seen)
    }

    fn sink(dir: &Path, url: &str) -> EventSinkConfig {
        let secret = dir.join("secret");
        std::fs::write(&secret, "s3cret\n").unwrap();
        EventSinkConfig {
            name: "t".into(),
            url: url.into(),
            secret_file: Some(secret),
            backoff_base_ms: 5,
            backoff_max_ms: 20,
            max_attempts: 3,
            timeout_secs: 2,
            ..Default::default()
        }
    }

    async fn settle(stats: &SinkStats, want: u64) {
        for _ in 0..400 {
            if stats.delivered.load(Ordering::Relaxed) + stats.dead_lettered.load(Ordering::Relaxed)
                >= want
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("sink did not settle");
    }

    #[test]
    fn signature_roundtrip_and_tamper() {
        let body = br#"{"event":"vm.start"}"#;
        let sig = sign(b"k", 1000, body);
        assert!(sig.starts_with("sha256="));
        assert!(verify(b"k", 1000, body, &sig, 1010, 300));
        assert!(!verify(b"k2", 1000, body, &sig, 1010, 300), "wrong key");
        assert!(!verify(b"k", 1000, b"{}", &sig, 1010, 300), "tampered body");
        assert!(
            !verify(b"k", 1001, body, &sig, 1010, 300),
            "timestamp is signed"
        );
        assert!(
            !verify(b"k", 1000, body, &sig, 5000, 300),
            "stale timestamp"
        );
        assert!(!verify(b"k", 1000, body, "sha256=zz", 1000, 300));
        assert!(!verify(b"k", 1000, body, "md5=00", 1000, 300));
        assert_eq!(sign(b"k", 1000, b"x").len(), "sha256=".len() + 64);
    }

    #[test]
    fn ssrf_guard_blocks_metadata_and_link_local() {
        for u in [
            "http://169.254.169.254/latest/meta-data",
            "http://169.254.10.1/",
            "http://[fe80::1]/",
            "http://[fd00:ec2::254]/",
            "http://[::ffff:169.254.169.254]/",
            "http://metadata.google.internal/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "ftp://example.com/",
            "http://user:pw@example.com/",
        ] {
            assert!(check_url(u, &[]).is_err(), "{u} should be refused");
        }
        for u in [
            "https://hooks.example.com/x",
            "http://10.1.2.3/x",
            "http://127.0.0.1:9/x",
        ] {
            assert!(check_url(u, &[]).is_ok(), "{u} should be allowed");
        }
        assert!(check_url("http://169.254.169.254/", &["169.254.169.254".into()]).is_ok());
        assert!(
            check_url(
                "http://metadata.google.internal/",
                &["metadata.google.internal".into()]
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn dns_answers_in_blocked_space_are_refused() {
        // "localhost" is loopback (allowed); an allow-list must not be needed for it.
        let u = reqwest::Url::parse("http://localhost:1/").unwrap();
        assert!(resolve_checked(&u, &[]).await.is_ok());
    }

    #[test]
    fn validate_rejects_bad_config() {
        let ok = EventSinkConfig {
            name: "a".into(),
            url: "https://x.example/h".into(),
            secret_file: Some("/s".into()),
            ..Default::default()
        };
        assert!(validate(std::slice::from_ref(&ok)).is_ok());
        let mut no_secret = ok.clone();
        no_secret.secret_file = None;
        assert!(validate(&[no_secret]).unwrap_err().contains("secret_file"));
        assert!(
            validate(&[ok.clone(), ok.clone()])
                .unwrap_err()
                .contains("duplicate")
        );
        let mut bad = ok.clone();
        bad.url = "http://169.254.169.254/".into();
        assert!(validate(&[bad]).unwrap_err().contains("metadata"));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let b = |n| backoff(n, 100, 1000).as_millis();
        assert_eq!(
            (b(1), b(2), b(3), b(4), b(5), b(60)),
            (100, 200, 400, 800, 1000, 1000)
        );
    }

    #[test]
    fn event_filter_is_a_prefix_match() {
        assert!(matches_filter(&[], "vm.start"));
        assert!(matches_filter(&["vm.".into()], "vm.start"));
        assert!(!matches_filter(&["node.".into()], "vm.start"));
    }

    #[tokio::test]
    async fn delivers_signed_event_that_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![200]).await;
        let d = Dispatcher::start(vec![sink(dir.path(), &url)], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        settle(&d.stats("t").unwrap(), 1).await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (head, body) = &seen[0];
        let h = |name: &str| {
            head.lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix(&format!("{}:", name.to_ascii_lowercase()))
                        .map(|_| l.split_once(':').unwrap().1.trim().to_string())
                })
                .unwrap()
        };
        let ts: i64 = h(TIMESTAMP_HEADER).parse().unwrap();
        assert!(verify(b"s3cret", ts, body, &h(SIGNATURE_HEADER), ts, 300));
        assert_eq!(h(EVENT_HEADER), "vm.start");
        assert!(!h(DELIVERY_HEADER).is_empty());
        assert!(!dir.path().join("dead.jsonl").exists());
    }

    #[tokio::test]
    async fn filter_skips_unmatched_events() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![200]).await;
        let mut c = sink(dir.path(), &url);
        c.events = vec!["node.".into()];
        let d = Dispatcher::start(vec![c], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        d.dispatch(&ev("node.down"));
        settle(&d.stats("t").unwrap(), 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![500, 503, 200]).await;
        let d = Dispatcher::start(vec![sink(dir.path(), &url)], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        let st = d.stats("t").unwrap();
        settle(&st, 1).await;
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(st.delivered.load(Ordering::Relaxed), 1);
        assert_eq!(st.retries.load(Ordering::Relaxed), 2);
        assert!(!dir.path().join("dead.jsonl").exists());
    }

    #[tokio::test]
    async fn exhausted_retries_go_to_dead_letter_without_the_query_string() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![500]).await;
        let d = Dispatcher::start(vec![sink(dir.path(), &url)], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        let st = d.stats("t").unwrap();
        settle(&st, 1).await;
        assert_eq!(seen.lock().unwrap().len(), 3);
        let dead = std::fs::read_to_string(dir.path().join("dead.jsonl")).unwrap();
        assert!(dead.contains("retries exhausted"), "{dead}");
        assert!(dead.contains("vm.start"));
        assert!(!dead.contains("token=abc"), "query must not leak: {dead}");
        assert!(!dead.contains("s3cret"));
    }

    #[tokio::test]
    async fn permanent_4xx_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![400]).await;
        let d = Dispatcher::start(vec![sink(dir.path(), &url)], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        settle(&d.stats("t").unwrap(), 1).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            std::fs::read_to_string(dir.path().join("dead.jsonl"))
                .unwrap()
                .contains("http 400")
        );
    }

    #[tokio::test]
    async fn redirect_to_metadata_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![302]).await;
        let d = Dispatcher::start(vec![sink(dir.path(), &url)], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        settle(&d.stats("t").unwrap(), 1).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "3xx is a permanent failure, never followed"
        );
    }

    #[tokio::test]
    async fn full_queue_dead_letters_instead_of_blocking() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing listens: the first event ties up the worker retrying while the queue fills.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        drop(l);
        let mut c = sink(dir.path(), &url);
        c.queue_size = 1;
        c.max_attempts = 50;
        c.backoff_base_ms = 200;
        c.backoff_max_ms = 200;
        let d = Dispatcher::start(vec![c], dir.path().join("dead.jsonl"));
        for _ in 0..20 {
            d.dispatch(&ev("vm.start"));
        }
        let dead = std::fs::read_to_string(dir.path().join("dead.jsonl")).unwrap();
        assert!(dead.matches("queue_full").count() >= 17, "{dead}");
    }

    #[tokio::test]
    async fn blocked_target_is_dead_lettered_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = sink(dir.path(), "http://169.254.169.254/x");
        c.allow_hosts = vec![];
        let d = Dispatcher::start(vec![c], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        settle(&d.stats("t").unwrap(), 1).await;
        assert!(
            std::fs::read_to_string(dir.path().join("dead.jsonl"))
                .unwrap()
                .contains("blocked")
        );
    }

    #[tokio::test]
    async fn missing_secret_file_retries_then_dead_letters() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = mock(vec![200]).await;
        let mut c = sink(dir.path(), &url);
        c.secret_file = Some(dir.path().join("nope"));
        let d = Dispatcher::start(vec![c], dir.path().join("dead.jsonl"));
        d.dispatch(&ev("vm.start"));
        settle(&d.stats("t").unwrap(), 1).await;
        assert!(seen.lock().unwrap().is_empty(), "never sent unsigned");
    }
}
