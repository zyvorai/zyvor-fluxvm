// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Pod logs and an interactive shell for the Studio's Kubernetes page.
//!
//! * `GET /v1/kube/{cluster}/pods/{ns}/{pod}/log` returns the tail of a container's log as plain text.
//! * `POST /v1/kube/{cluster}/pods/{ns}/{pod}/exec-ticket` (admin) mints a single-use, 30-second ticket, and
//!   `GET /console/kube/ws/{ticket}` redeems it as a WebSocket carrying a shell in the pod. A browser WebSocket cannot send
//!   `Authorization`, so the ticket stands in for it, exactly as the VM console does. The shell is the cluster's own `exec`
//!   subresource over mutual TLS (`v4.channel.k8s.io`): bytes the browser sends are stdin, bytes it receives are the
//!   terminal's output, and a `{"cols":N,"rows":N}` text frame resizes it.

use crate::kube::{self, API_NAME, Target, err};
use crate::{ApiError, ApiResult, Role, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, Query, State, ws::Message as Axum, ws::WebSocket, ws::WebSocketUpgrade},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use fluxvm_scheduler::VmManager;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::{Message as Upstream, client::IntoClientRequest};

const LOG_MAX_BYTES: usize = 4 * 1024 * 1024;
const LOG_DEFAULT_TAIL: u32 = 500;
const LOG_MAX_TAIL: u32 = 5000;
const TICKET_SECS: u64 = 30;
const MAX_TICKETS: usize = 64;
/// What runs in the pod: the nicest shell it has.
const SHELL: &str = "command -v bash >/dev/null 2>&1 && exec bash -l || exec sh";

/// A namespace, pod or container name: a DNS subdomain. Nothing else is ever placed in a URL.
pub(crate) fn k8s_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 253
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'.')
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
}

fn check_names(ns: &str, pod: &str, container: Option<&str>) -> Result<(), ApiError> {
    if !k8s_name_ok(ns) || !k8s_name_ok(pod) || container.is_some_and(|c| !k8s_name_ok(c)) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "not a namespace, pod or container name",
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct LogQuery {
    container: Option<String>,
    tail: Option<u32>,
    previous: Option<bool>,
}

pub(crate) fn log_path(ns: &str, pod: &str, q: &LogQuery) -> String {
    let tail = q.tail.unwrap_or(LOG_DEFAULT_TAIL).clamp(1, LOG_MAX_TAIL);
    let mut path =
        format!("/api/v1/namespaces/{ns}/pods/{pod}/log?tailLines={tail}&timestamps=true");
    if let Some(c) = &q.container {
        path.push_str(&format!("&container={c}"));
    }
    if q.previous == Some(true) {
        path.push_str("&previous=true");
    }
    path
}

/// The message Kubernetes puts in a `Status` error, such as "container x is waiting to start: ContainerCreating".
fn status_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

/// Keeps the last `max` bytes of `bytes` (the newest log lines), starting on a line boundary when it had to cut.
pub(crate) fn tail_text(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let from = bytes.len() - max;
    let cut = &bytes[from..];
    // The cut fell exactly between two lines: keep everything. Otherwise drop the line it fell inside.
    let start = if bytes[from - 1] == b'\n' {
        0
    } else {
        cut.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)
    };
    String::from_utf8_lossy(&cut[start..]).into_owned()
}

/// `GET /v1/kube/{cluster}/pods/{ns}/{pod}/log?container=&tail=&previous=`
pub(crate) async fn pod_log(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((name, ns, pod)): Path<(String, String, String)>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Response> {
    check_names(&ns, &pod, q.container.as_deref())?;
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = kube::resolve(&m, tenant.as_deref(), &name).await?;
    let path = log_path(&ns, &pod, &q);
    let mut tried = vec![];
    for &addr in &target.addrs {
        let client = kube::http_client(&target.creds, addr)?;
        let url = format!("https://{API_NAME}:{}{path}", addr.port());
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tried.push(format!("{addr}: {}", kube::chain(&e)));
                continue;
            }
        };
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                format!("reading the log: {}", kube::chain(&e)),
            )
        })?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            let message =
                status_message(&text).unwrap_or_else(|| format!("the cluster answered {status}"));
            let code = match status.as_u16() {
                400 => StatusCode::BAD_REQUEST,
                403 => StatusCode::FORBIDDEN,
                404 => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_GATEWAY,
            };
            return Err(err(code, message));
        }
        return Ok((
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            tail_text(&body, LOG_MAX_BYTES),
        )
            .into_response());
    }
    Err(err(StatusCode::BAD_GATEWAY, tried.join("; ")))
}

// ---- exec tickets -------------------------------------------------------------------------------------------------

struct ExecTicket {
    cluster: String,
    ns: String,
    pod: String,
    container: Option<String>,
    tenant: Option<String>,
    expires: Instant,
}

fn tickets() -> &'static Mutex<HashMap<String, ExecTicket>> {
    static T: OnceLock<Mutex<HashMap<String, ExecTicket>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// `None` when too many are outstanding (a flood, not a person).
fn mint(t: ExecTicket) -> Option<String> {
    let mut all = tickets().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    all.retain(|_, v| v.expires > now);
    if all.len() >= MAX_TICKETS {
        return None;
    }
    let ticket = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    all.insert(ticket.clone(), t);
    Some(ticket)
}

fn take(ticket: &str) -> Option<ExecTicket> {
    let t = tickets()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(ticket)?;
    (t.expires > Instant::now()).then_some(t)
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct TicketBody {
    container: Option<String>,
}

/// `POST /v1/kube/{cluster}/pods/{ns}/{pod}/exec-ticket` (admin): a shell in a pod is remote code execution.
pub(crate) async fn exec_ticket(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((name, ns, pod)): Path<(String, String, String)>,
    body: Option<Json<TicketBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let container = body.and_then(|Json(b)| b.container);
    check_names(&ns, &pod, container.as_deref())?;
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    kube::resolve(&m, tenant.as_deref(), &name).await?; // exists, is running, and the caller may see it
    let ticket = mint(ExecTicket {
        cluster: name,
        ns,
        pod,
        container,
        tenant,
        expires: Instant::now() + Duration::from_secs(TICKET_SECS),
    })
    .ok_or_else(|| {
        err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many shell tickets outstanding",
        )
    })?;
    Ok(Json(json!({
        "ticket": ticket,
        "expires_in": TICKET_SECS,
        "path": format!("/console/kube/ws/{ticket}"),
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SizeQuery {
    cols: Option<u16>,
    rows: Option<u16>,
}

/// `GET /console/kube/ws/{ticket}?cols=&rows=`: the ticket is the credential (single use, 30 s, minted by an admin).
pub(crate) async fn exec_ws(
    State(m): State<Arc<VmManager>>,
    Path(ticket): Path<String>,
    Query(size): Query<SizeQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let t = take(&ticket).ok_or_else(|| err(StatusCode::NOT_FOUND, "shell ticket not found"))?;
    let target = kube::resolve(&m, t.tenant.as_deref(), &t.cluster).await?;
    let (cols, rows) = (size.cols.unwrap_or(80), size.rows.unwrap_or(24));
    Ok(ws.on_upgrade(move |socket| relay(socket, target, t, cols, rows)))
}

// ---- the exec protocol --------------------------------------------------------------------------------------------

/// Stdin, as `v4.channel.k8s.io` frames it: channel 0, then the bytes.
pub(crate) fn stdin_frame(data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(data.len() + 1);
    f.push(0);
    f.extend_from_slice(data);
    f
}

/// A resize, channel 4 with the size the API server expects.
pub(crate) fn resize_frame(cols: u16, rows: u16) -> Vec<u8> {
    let mut f = vec![4];
    f.extend_from_slice(
        json!({"Width": cols, "Height": rows})
            .to_string()
            .as_bytes(),
    );
    f
}

/// The browser terminal's `{"cols":N,"rows":N}` text frame.
pub(crate) fn parse_resize(text: &str) -> Option<(u16, u16)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let n = |k: &str| {
        v.get(k)?
            .as_u64()
            .filter(|n| (1..=2000).contains(n))
            .map(|n| n as u16)
    };
    Some((n("cols")?, n("rows")?))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Frame {
    /// Terminal output for the browser.
    Output(Vec<u8>),
    /// The command ended: `None` on success, the cluster's message otherwise.
    Done(Option<String>),
    /// Nothing to forward (the empty frames each channel opens with).
    Nothing,
}

/// A frame from the API server: channel 1 and 2 are output, channel 3 is the final status.
pub(crate) fn from_upstream(frame: &[u8]) -> Frame {
    let Some((&channel, rest)) = frame.split_first() else {
        return Frame::Nothing;
    };
    match channel {
        1 | 2 if !rest.is_empty() => Frame::Output(rest.to_vec()),
        3 if !rest.is_empty() => {
            let v: Value = serde_json::from_slice(rest).unwrap_or(Value::Null);
            if v["status"] == "Success" {
                Frame::Done(None)
            } else {
                Frame::Done(Some(
                    v["message"]
                        .as_str()
                        .unwrap_or("the command ended with an error")
                        .to_owned(),
                ))
            }
        }
        _ => Frame::Nothing,
    }
}

/// The `exec` URL: the shell, a TTY, and the container if one was named.
pub(crate) fn exec_url(port: u16, ns: &str, pod: &str, container: Option<&str>) -> String {
    let mut u = reqwest::Url::parse(&format!(
        "wss://{API_NAME}:{port}/api/v1/namespaces/{ns}/pods/{pod}/exec"
    ))
    .expect("a valid URL: the names are validated");
    {
        let mut q = u.query_pairs_mut();
        for c in ["/bin/sh", "-c", SHELL] {
            q.append_pair("command", c);
        }
        q.append_pair("stdin", "true")
            .append_pair("stdout", "true")
            .append_pair("stderr", "false")
            .append_pair("tty", "true");
        if let Some(c) = container {
            q.append_pair("container", c);
        }
    }
    u.into()
}

fn tls_config(creds: &kube::Credentials) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut creds.ca.as_slice()) {
        roots
            .add(c.map_err(|e| format!("CA: {e}"))?)
            .map_err(|e| format!("CA: {e}"))?;
    }
    let certs = rustls_pemfile::certs(&mut creds.identity.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("client certificate: {e}"))?;
    let key = rustls_pemfile::private_key(&mut creds.identity.as_slice())
        .map_err(|e| format!("client key: {e}"))?
        .ok_or("the kubeconfig has no client key")?;
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("client certificate: {e}"))
}

type Upstream2 =
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;

async fn connect(
    creds: &kube::Credentials,
    addr: SocketAddr,
    t: &ExecTicket,
) -> Result<Upstream2, String> {
    let cfg = tls_config(creds)?;
    let tcp = tokio::time::timeout(Duration::from_secs(4), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(API_NAME).map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS: {e}"))?;
    let mut req = exec_url(addr.port(), &t.ns, &t.pod, t.container.as_deref())
        .into_client_request()
        .map_err(|e| e.to_string())?;
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "v4.channel.k8s.io"
            .parse()
            .map_err(|_| "header".to_owned())?,
    );
    let (ws, _) = tokio_tungstenite::client_async(req, tls)
        .await
        .map_err(|e| format!("exec: {e}"))?;
    Ok(ws)
}

fn notice(text: &str) -> Axum {
    Axum::Binary(format!("\r\n\x1b[31m{text}\x1b[0m\r\n").into_bytes().into())
}

async fn relay(mut client: WebSocket, target: Target, t: ExecTicket, cols: u16, rows: u16) {
    let mut tried = vec![];
    let mut upstream = None;
    for &addr in &target.addrs {
        match connect(&target.creds, addr, &t).await {
            Ok(u) => {
                upstream = Some(u);
                break;
            }
            Err(e) => tried.push(format!("{addr}: {e}")),
        }
    }
    let Some(upstream) = upstream else {
        let _ = client
            .send(notice(&format!(
                "Could not open a shell: {}",
                tried.join("; ")
            )))
            .await;
        let _ = client.close().await;
        return;
    };
    let (mut up_tx, mut up_rx) = upstream.split();
    let _ = up_tx
        .send(Upstream::Binary(resize_frame(cols, rows).into()))
        .await;
    let (mut cl_tx, mut cl_rx) = client.split();
    let to_pod = async {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let out = match msg {
                Axum::Binary(b) => Upstream::Binary(stdin_frame(&b).into()),
                Axum::Text(s) => match parse_resize(&s) {
                    Some((c, r)) => Upstream::Binary(resize_frame(c, r).into()),
                    None => continue,
                },
                Axum::Close(_) => break,
                _ => continue,
            };
            if up_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let to_browser = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let bytes = match msg {
                Upstream::Binary(b) => b,
                Upstream::Close(_) => break,
                _ => continue,
            };
            match from_upstream(&bytes) {
                Frame::Output(o) => {
                    if cl_tx.send(Axum::Binary(o.into())).await.is_err() {
                        break;
                    }
                }
                Frame::Done(None) => break,
                Frame::Done(Some(message)) => {
                    let _ = cl_tx.send(notice(&format!("[{message}]"))).await;
                    break;
                }
                Frame::Nothing => {}
            }
        }
        let _ = cl_tx.close().await;
    };
    tokio::select! {
        _ = to_pod => {}
        _ = to_browser => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_subdomains() {
        for ok in [
            "default",
            "kube-system",
            "coredns-7cfb7bc9c7-hfdf4",
            "a.b-c",
            "x",
        ] {
            assert!(k8s_name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            ".a",
            "a.",
            "A",
            "a_b",
            "a/b",
            "a b",
            "a?x=1",
            "../x",
            &"x".repeat(254),
        ] {
            assert!(!k8s_name_ok(bad), "{bad:?}");
        }
        assert!(check_names("default", "web", Some("app")).is_ok());
        assert!(check_names("default", "web", Some("a/b")).is_err());
    }

    #[test]
    fn log_paths_are_bounded_and_complete() {
        let q = |container: Option<&str>, tail, previous| LogQuery {
            container: container.map(str::to_owned),
            tail,
            previous,
        };
        assert_eq!(
            log_path("default", "web", &q(None, None, None)),
            "/api/v1/namespaces/default/pods/web/log?tailLines=500&timestamps=true"
        );
        assert!(
            log_path("d", "w", &q(Some("app"), Some(50), Some(true)))
                .ends_with("tailLines=50&timestamps=true&container=app&previous=true")
        );
        assert!(log_path("d", "w", &q(None, Some(0), None)).contains("tailLines=1&"));
        assert!(log_path("d", "w", &q(None, Some(10_000_000), None)).contains("tailLines=5000&"));
        assert!(!log_path("d", "w", &q(None, None, Some(false))).contains("previous"));
    }

    #[test]
    fn the_newest_log_bytes_are_kept_from_a_line_start() {
        assert_eq!(tail_text(b"a\nb\nc\n", 100), "a\nb\nc\n");
        assert_eq!(tail_text(b"aaaa\nbbbb\ncccc\n", 10), "bbbb\ncccc\n"); // cut between lines: keep both
        assert_eq!(tail_text(b"aaaa\nbbbb\ncccc\n", 9), "cccc\n"); // cut inside bbbb: drop the partial line
        assert_eq!(tail_text(b"one-very-long-line", 4), "line"); // no newline to start from
        assert_eq!(
            status_message(r#"{"kind":"Status","message":"container \"x\" in pod \"p\" is waiting to start: ContainerCreating"}"#).as_deref(),
            Some("container \"x\" in pod \"p\" is waiting to start: ContainerCreating")
        );
        assert_eq!(status_message("not json"), None);
    }

    #[test]
    fn tickets_are_single_use_and_expire() {
        let t = |secs: u64| ExecTicket {
            cluster: "dev".into(),
            ns: "default".into(),
            pod: "web".into(),
            container: None,
            tenant: Some("t1".into()),
            expires: Instant::now() + Duration::from_secs(secs),
        };
        let ticket = mint(t(30)).unwrap();
        assert_eq!(ticket.len(), 64);
        let got = take(&ticket).unwrap();
        assert_eq!(
            (
                got.cluster.as_str(),
                got.pod.as_str(),
                got.tenant.as_deref()
            ),
            ("dev", "web", Some("t1"))
        );
        assert!(take(&ticket).is_none(), "a ticket works once");
        let stale = mint(ExecTicket {
            expires: Instant::now() - Duration::from_secs(1),
            ..t(0)
        })
        .unwrap();
        assert!(take(&stale).is_none(), "an expired ticket is refused");
        assert!(take("not-a-ticket").is_none());
    }

    #[test]
    fn exec_frames_follow_the_v4_channel_protocol() {
        assert_eq!(stdin_frame(b"ls\r"), b"\0ls\r");
        let r = resize_frame(120, 40);
        assert_eq!(r[0], 4);
        assert_eq!(
            serde_json::from_slice::<Value>(&r[1..]).unwrap(),
            json!({"Width": 120, "Height": 40})
        );
        assert_eq!(parse_resize(r#"{"cols":100,"rows":30}"#), Some((100, 30)));
        for bad in [
            "",
            "x",
            r#"{"cols":0,"rows":5}"#,
            r#"{"cols":5}"#,
            r#"{"cols":99999,"rows":5}"#,
            r#"{"cols":"a","rows":5}"#,
        ] {
            assert_eq!(parse_resize(bad), None, "{bad:?}");
        }
        assert_eq!(
            from_upstream(b"\x01hello"),
            Frame::Output(b"hello".to_vec())
        );
        assert_eq!(from_upstream(b"\x02err"), Frame::Output(b"err".to_vec()));
        assert_eq!(from_upstream(b"\x01"), Frame::Nothing);
        assert_eq!(from_upstream(b""), Frame::Nothing);
        assert_eq!(from_upstream(b"\x09zzz"), Frame::Nothing);
        assert_eq!(
            from_upstream(b"\x03{\"status\":\"Success\"}"),
            Frame::Done(None)
        );
        assert_eq!(
            from_upstream(
                b"\x03{\"status\":\"Failure\",\"message\":\"command terminated with exit code 1\"}"
            ),
            Frame::Done(Some("command terminated with exit code 1".into()))
        );
        assert_eq!(
            from_upstream(b"\x03garbage"),
            Frame::Done(Some("the command ended with an error".into()))
        );
    }

    #[test]
    fn the_exec_url_asks_for_a_tty_shell_in_the_right_container() {
        let u = reqwest::Url::parse(&exec_url(6443, "default", "web", Some("app"))).unwrap();
        assert_eq!(u.scheme(), "wss");
        assert_eq!(u.host_str(), Some(API_NAME));
        assert_eq!(u.port(), Some(6443));
        assert_eq!(u.path(), "/api/v1/namespaces/default/pods/web/exec");
        let q: Vec<(String, String)> = u
            .query_pairs()
            .map(|(a, b)| (a.into_owned(), b.into_owned()))
            .collect();
        assert_eq!(
            q.iter()
                .filter(|(k, _)| k == "command")
                .map(|(_, v)| v.as_str())
                .collect::<Vec<_>>()[..2],
            ["/bin/sh", "-c"]
        );
        for want in [
            ("tty", "true"),
            ("stdin", "true"),
            ("stdout", "true"),
            ("stderr", "false"),
            ("container", "app"),
        ] {
            assert!(
                q.iter().any(|(k, v)| (k.as_str(), v.as_str()) == want),
                "{want:?}"
            );
        }
        assert!(
            !reqwest::Url::parse(&exec_url(6443, "d", "w", None))
                .unwrap()
                .query()
                .unwrap()
                .contains("container")
        );
    }

    #[test]
    fn bad_credentials_are_refused_before_any_connection() {
        // The success path (a real CA, certificate and key) is covered by the live test against a cluster.
        let empty = kube::Credentials {
            ca: vec![],
            identity: vec![],
            server: None,
        };
        assert!(tls_config(&empty).unwrap_err().contains("no client key"));
        let junk = kube::Credentials {
            ca: b"nope".to_vec(),
            identity: b"nope".to_vec(),
            server: None,
        };
        assert!(tls_config(&junk).is_err());
    }

    /// Against a real cluster: `FLUXVM_KUBE_LIVE_KUBECONFIG=file FLUXVM_KUBE_LIVE_ADDR=127.0.0.1:26443
    /// FLUXVM_KUBE_LIVE_POD=default/sh-pod cargo test -p fluxvm-api --lib kube_pod::tests::live -- --ignored --nocapture`.
    /// The pod must have a shell and have logged something.
    #[tokio::test]
    #[ignore = "needs a running cluster and a pod"]
    async fn live_logs_and_a_shell_work_against_a_cluster() {
        let creds = kube::parse_kubeconfig(
            &std::fs::read_to_string(std::env::var("FLUXVM_KUBE_LIVE_KUBECONFIG").unwrap())
                .unwrap(),
        )
        .expect("credentials");
        let addr: SocketAddr = std::env::var("FLUXVM_KUBE_LIVE_ADDR")
            .unwrap()
            .parse()
            .unwrap();
        let pod = std::env::var("FLUXVM_KUBE_LIVE_POD").unwrap();
        let (ns, name) = pod.split_once('/').unwrap();

        // Logs, through the same request the endpoint makes.
        let q = LogQuery {
            container: None,
            tail: Some(50),
            previous: None,
        };
        let client = kube::http_client(&creds, addr).unwrap_or_else(|e| panic!("{}", e.message));
        let url = format!(
            "https://{API_NAME}:{}{}",
            addr.port(),
            log_path(ns, name, &q)
        );
        let resp = client.get(&url).send().await.expect("log request");
        assert!(resp.status().is_success(), "log status {}", resp.status());
        let log = tail_text(&resp.bytes().await.unwrap(), LOG_MAX_BYTES);
        println!("log: {log:?}");
        assert!(!log.trim().is_empty(), "the pod has logged nothing");
        assert!(
            log.lines().next().unwrap().len() > 20 && log.starts_with("20"),
            "timestamps=true prefixes each line"
        );

        // A shell: run arithmetic so the answer is not in the echoed command.
        let ticket = ExecTicket {
            cluster: "live".into(),
            ns: ns.into(),
            pod: name.into(),
            container: None,
            tenant: None,
            expires: Instant::now() + Duration::from_secs(30),
        };
        let mut ws = connect(&creds, addr, &ticket)
            .await
            .expect("exec connection");
        ws.send(Upstream::Binary(resize_frame(100, 30).into()))
            .await
            .unwrap();
        ws.send(Upstream::Binary(
            stdin_frame(b"echo FLUX-$((6*7))\nexit\n").into(),
        ))
        .await
        .unwrap();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let next = tokio::time::timeout(Duration::from_secs(5), ws.next()).await;
            let Ok(Some(Ok(Upstream::Binary(b)))) = next else {
                break;
            };
            match from_upstream(&b) {
                Frame::Output(o) => out.extend(o),
                Frame::Done(d) => {
                    println!("done: {d:?}");
                    break;
                }
                Frame::Nothing => {}
            }
        }
        let out = String::from_utf8_lossy(&out).into_owned();
        println!("shell output: {out:?}");
        assert!(out.contains("FLUX-42"), "the shell answered: {out:?}");
    }
}
