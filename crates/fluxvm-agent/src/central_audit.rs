// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The activity log: every change made through central (who, what, when, and whether it worked), and the screen
//! proxy that lets a tenant see its VM's display.
//!
//! Events are kept in memory (the newest [`KEEP`]) and appended to `fleet-audit.jsonl` (mode 0600) in the state
//! directory, which is rotated to `fleet-audit.jsonl.1` at [`ROTATE_BYTES`]. Reads (`GET`) and node heartbeats are
//! not recorded. Request bodies are not stored, except a VM's, tenant's or network's `name`/`id`/`node` (never a
//! command's body), so the log never holds a VM spec, a command, a token or a key.

use super::*;
use std::collections::VecDeque;
use std::io::Write as _;

/// Events kept in memory and served by `GET /fleet/events`.
const KEEP: usize = 5000;
/// Size at which `fleet-audit.jsonl` is rotated.
const ROTATE_BYTES: u64 = 16 * 1024 * 1024;
/// Largest request body read to find the target's name; bigger bodies are refused. The same as axum's default `Json`
/// limit, so no route refuses a body it would otherwise have taken.
const MAX_BODY: usize = 2 * 1024 * 1024;
/// Longest target kept; a longer name is cut.
const MAX_TARGET: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct AuditEvent {
    pub time: DateTime<Utc>,
    /// `operator`, or the tenant's id.
    pub actor: String,
    /// The acting tenant, when a tenant acted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub method: String,
    pub path: String,
    /// The name or id from the request body, if it had one (a VM name, a tenant id, a network name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub status: u16,
}

pub(super) struct AuditLog {
    events: std::sync::Mutex<VecDeque<AuditEvent>>,
    path: Option<PathBuf>,
}

impl AuditLog {
    /// An in-memory log (tests).
    #[cfg(test)]
    pub(super) fn memory() -> Self {
        AuditLog {
            events: std::sync::Mutex::new(VecDeque::new()),
            path: None,
        }
    }

    /// A log persisted at `path`, starting with the newest events already there.
    pub(super) fn open(path: PathBuf) -> Self {
        let mut events = VecDeque::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(e) = serde_json::from_str::<AuditEvent>(line) {
                    if events.len() == KEEP {
                        events.pop_front();
                    }
                    events.push_back(e);
                }
            }
        }
        AuditLog {
            events: std::sync::Mutex::new(events),
            path: Some(path),
        }
    }

    /// Appends `e`. The lock is held across the (small, appending) file write so the file keeps the memory order; central
    /// records only changes, which are rare next to reads.
    pub(super) fn record(&self, e: AuditEvent) {
        let mut events = self.events.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(path) = &self.path
            && let Ok(line) = serde_json::to_string(&e)
        {
            if fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_BYTES) {
                let _ = fs::rename(path, path.with_extension("jsonl.1"));
            }
            let mut opts = fs::OpenOptions::new();
            opts.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{line}");
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %path.display(), "cannot write the activity log")
                }
            }
        }
        if events.len() == KEEP {
            events.pop_front();
        }
        events.push_back(e);
    }

    /// Newest first; `tenant` limits to one actor.
    pub(super) fn query(&self, tenant: Option<&str>, limit: usize) -> Vec<AuditEvent> {
        let events = self.events.lock().unwrap_or_else(|p| p.into_inner());
        events
            .iter()
            .rev()
            .filter(|e| tenant.is_none_or(|t| e.tenant.as_deref() == Some(t)))
            .take(limit)
            .cloned()
            .collect()
    }
}

/// Whether a request is worth recording: changes only, and not the node heartbeat.
pub(super) fn audited(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) && path != "/fleet/register"
}

/// An event for `caller`'s `method path` that ended with `status`.
pub(super) fn event(
    caller: &Caller,
    method: String,
    path: String,
    target: Option<String>,
    status: StatusCode,
) -> AuditEvent {
    let (actor, tenant) = match caller {
        Caller::Admin => ("operator".to_owned(), None),
        Caller::Tenant(t) => (t.clone(), Some(t.clone())),
    };
    AuditEvent {
        time: Utc::now(),
        actor,
        tenant,
        method,
        path,
        target,
        status: status.as_u16(),
    }
}

/// Reads a JSON body to find its `name`, `id` or `node`, and returns the request rebuilt with the same body. A command
/// (`.../exec`) body is the guest's input, so it is neither read nor searched.
pub(super) async fn target_of(req: Request) -> Result<(Request, Option<String>), Response> {
    if req.uri().path().ends_with("/exec") {
        return Ok((req, None));
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| {
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "request body over 2 MiB, or not fully sent"})),
        )
            .into_response()
    })?;
    let target = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
        ["name", "id", "node"].iter().find_map(|k| {
            v.as_object()?.get(*k).and_then(Value::as_str).map(|s| {
                s.chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_TARGET)
                    .collect::<String>()
            })
        })
    });
    Ok((
        Request::from_parts(parts, axum::body::Body::from(bytes)),
        target,
    ))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct EventsQuery {
    #[serde(default)]
    limit: Option<usize>,
    /// Operator only: one tenant's actions.
    #[serde(default)]
    tenant: Option<String>,
}

/// `GET /fleet/events[?limit=N&tenant=T]`: the activity log, newest first. A tenant sees only its own actions.
pub(super) async fn events(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    axum::extract::Query(q): axum::extract::Query<EventsQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(200).clamp(1, KEEP);
    let tenant = match &caller {
        Caller::Tenant(t) => Some(t.clone()),
        Caller::Admin => q.tenant,
    };
    Json(json!({ "items": fleet.audit.query(tenant.as_deref(), limit) }))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ScreenQuery {
    #[serde(default)]
    max_width: Option<u32>,
}

/// `GET /fleet/vms/{node}/{id}/screen[?max_width=N]`: the VM's display as an image, from its node's
/// `GET /v1/vms/{id}/screenshot`. A tenant sees only its own VMs.
pub(super) async fn screen(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<ScreenQuery>,
) -> Result<Response, AppError> {
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no VM '{id}' on node '{node}'"),
        ));
    }
    let target = {
        let nodes = fleet.nodes.lock().await;
        nodes.get(&node).cloned().ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("no registered node named '{node}'"),
            )
        })?
    };
    let width = q.max_width.unwrap_or(1280).clamp(160, 3840);
    let resp = fleet
        .http
        .get(format!(
            "{}/v1/vms/{id}/screenshot?max_width={width}",
            target.fluxvm_url
        ))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or(body);
        // 404/409/501 from the node mean "no screen for this VM" (not a vz VM, not running, no display); say so
        // rather than reporting a gateway failure.
        let code = match status.as_u16() {
            404 | 409 | 501 => StatusCode::CONFLICT,
            _ => StatusCode::BAD_GATEWAY,
        };
        return Err(AppError(
            code,
            format!("no screen from node '{node}': {msg}"),
        ));
    }
    let mut headers = axum::http::HeaderMap::new();
    // Only an image type is passed on (the console shows it from a blob: URL), and never sniffed into anything else.
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|t| t.starts_with("image/"))
        .unwrap_or("application/octet-stream");
    if let Ok(v) = axum::http::HeaderValue::from_str(ctype) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    for name in [
        "x-fluxvm-screen-width",
        "x-fluxvm-screen-height",
        "x-fluxvm-screen-blank",
    ] {
        if let Some(v) = resp.headers().get(name)
            && let (Ok(n), Ok(v)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_bytes(v.as_bytes()),
            )
        {
            headers.insert(n, v);
        }
    }
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    let bytes = resp.bytes().await?;
    Ok((StatusCode::OK, headers, bytes).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(actor: &str, tenant: Option<&str>, path: &str) -> AuditEvent {
        AuditEvent {
            time: Utc::now(),
            actor: actor.into(),
            tenant: tenant.map(str::to_owned),
            method: "POST".into(),
            path: path.into(),
            target: None,
            status: 201,
        }
    }

    #[test]
    fn newest_first_and_per_tenant() {
        let log = AuditLog::memory();
        log.record(ev("operator", None, "/fleet/tenants"));
        log.record(ev("acme", Some("acme"), "/fleet/leases"));
        log.record(ev("globex", Some("globex"), "/fleet/leases"));
        log.record(ev("acme", Some("acme"), "/fleet/vms"));
        let all = log.query(None, 10);
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].path, "/fleet/vms");
        let acme = log.query(Some("acme"), 10);
        assert_eq!(acme.len(), 2);
        assert!(acme.iter().all(|e| e.actor == "acme"));
        assert_eq!(log.query(None, 1).len(), 1);
    }

    #[test]
    fn persisted_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-audit.jsonl");
        let log = AuditLog::open(path.clone());
        log.record(ev("operator", None, "/fleet/policy"));
        log.record(ev("acme", Some("acme"), "/fleet/vms"));
        drop(log);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let again = AuditLog::open(path);
        let all = again.query(None, 10);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].actor, "acme");
    }

    #[test]
    fn reads_and_heartbeats_are_not_recorded() {
        use axum::http::Method;
        assert!(!audited(&Method::GET, "/fleet/vms"));
        assert!(!audited(&Method::POST, "/fleet/register"));
        assert!(audited(&Method::POST, "/fleet/vms"));
        assert!(audited(&Method::DELETE, "/fleet/vms/mac-1/abc"));
    }

    #[tokio::test]
    async fn the_target_name_is_read_and_the_body_kept() {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/fleet/vms")
            .body(axum::body::Body::from(r#"{"name":"ios-1","image":"/x"}"#))
            .unwrap();
        let (req, target) = target_of(req).await.unwrap();
        assert_eq!(target.as_deref(), Some("ios-1"));
        let body = axum::body::to_bytes(req.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], br#"{"name":"ios-1","image":"/x"}"#);
    }

    #[tokio::test]
    async fn a_command_body_is_not_read() {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/fleet/vms/mac-1/abc/exec")
            .body(axum::body::Body::from(r#"{"name":"secret","command":"x"}"#))
            .unwrap();
        let (_, target) = target_of(req).await.unwrap();
        assert_eq!(target, None);
    }
}
