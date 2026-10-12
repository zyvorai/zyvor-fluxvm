// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Tunnels: SSH and other TCP ports of a VM, reached through central so no Mac has to be exposed.
//!
//! A client (`fluxctl fleet ssh`, `tunnel-stdio`, `port-forward`) opens a WebSocket to
//! `GET /fleet/vms/{node}/{id}/tunnel?port=22` with its bearer token. Central checks that the caller owns the VM, that
//! the key may make changes, that the port is open to tunnels ([`TunnelPolicy`]) and that the tenant has a tunnel to
//! spare; then it opens a WebSocket of its own to the node's `GET /v1/vms/{id}/tunnel?port=22` (with the fleet token)
//! and relays binary messages both ways. The node connects to the guest's address on that port; central never connects
//! anywhere but a registered node's own API, so a tunnel cannot reach central's or a Mac's local services.
//!
//! Each direction is relayed by its own loop that waits for a message to be written before reading the next, so a slow
//! reader slows the writer down (backpressure) instead of filling memory. A tunnel with no traffic for the policy's idle
//! timeout is closed, as is one closed with `DELETE /fleet/tunnels/{id}`. Opening and closing are written to the
//! activity log (VM, port, bytes each way, how long, and why it ended). Tunnels live in this central's memory: a
//! restart ends them, and clients reconnect.

use super::*;
use crate::tenancy::TunnelPolicy;
use axum::extract::Query;
use axum::extract::ws::{
    CloseFrame, Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{self, Message as NodeMessage, client::IntoClientRequest};

/// Most tunnels one central keeps open, whatever the per-tenant limit.
const MAX_TUNNELS_TOTAL: usize = 4096;
/// Largest WebSocket message accepted from a client (nodes send at most 64 KiB at a time).
const MAX_MESSAGE: usize = 256 * 1024;
/// How long the node has to accept the tunnel.
const NODE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One open tunnel.
pub(super) struct Tunnel {
    id: String,
    tenant: Option<String>,
    key: Option<String>,
    node: String,
    vm_id: String,
    vm_name: String,
    port: u16,
    opened_at: DateTime<Utc>,
    /// Bytes from the client to the VM.
    sent: AtomicU64,
    /// Bytes from the VM to the client.
    received: AtomicU64,
    /// Unix milliseconds of the last byte either way.
    last_active: AtomicI64,
    close: tokio::sync::Notify,
    closed_by: std::sync::Mutex<Option<&'static str>>,
}

impl Tunnel {
    fn touch(&self) {
        self.last_active
            .store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let ms = Utc::now().timestamp_millis() - self.last_active.load(Ordering::Relaxed);
        Duration::from_millis(ms.max(0) as u64)
    }

    fn visible_to(&self, caller: &Caller) -> bool {
        match caller {
            Caller::Admin => true,
            Caller::Tenant(t) => self.tenant.as_deref() == Some(t.as_str()),
        }
    }

    fn public_json(&self) -> Value {
        let last = DateTime::<Utc>::from_timestamp_millis(self.last_active.load(Ordering::Relaxed))
            .unwrap_or(self.opened_at);
        json!({
            "id": self.id,
            "tenant": self.tenant,
            "key": self.key,
            "node": self.node,
            "vm_id": self.vm_id,
            "vm_name": self.vm_name,
            "port": self.port,
            "opened_at": self.opened_at,
            "last_active_at": last,
            "idle_seconds": self.idle_for().as_secs(),
            "bytes_sent": self.sent.load(Ordering::Relaxed),
            "bytes_received": self.received.load(Ordering::Relaxed),
        })
    }
}

/// Every tunnel open through this central.
#[derive(Default)]
pub(super) struct Tunnels {
    open: std::sync::Mutex<HashMap<String, Arc<Tunnel>>>,
}

/// Why a tunnel could not get a slot.
#[derive(Debug, PartialEq, Eq)]
enum SlotError {
    TenantLimit(u32),
    CentralFull,
}

impl Tunnels {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Tunnel>>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Takes a slot for `t` when its tenant is under `max_per_tenant` (counting and adding under one lock, so two
    /// opens at once cannot both take the last slot).
    fn reserve(self: &Arc<Self>, t: Tunnel, max_per_tenant: u32) -> Result<Slot, SlotError> {
        let mut open = self.lock();
        if open.len() >= MAX_TUNNELS_TOTAL {
            return Err(SlotError::CentralFull);
        }
        let mine = open.values().filter(|o| o.tenant == t.tenant).count();
        if mine >= max_per_tenant as usize {
            return Err(SlotError::TenantLimit(max_per_tenant));
        }
        let t = Arc::new(t);
        open.insert(t.id.clone(), t.clone());
        Ok(Slot {
            tunnels: self.clone(),
            tunnel: t,
        })
    }

    fn get(&self, id: &str) -> Option<Arc<Tunnel>> {
        self.lock().get(id).cloned()
    }

    /// The tunnels `caller` may see, newest first.
    fn visible(&self, caller: &Caller) -> Vec<Arc<Tunnel>> {
        let mut out: Vec<Arc<Tunnel>> = self
            .lock()
            .values()
            .filter(|t| t.visible_to(caller))
            .cloned()
            .collect();
        out.sort_by(|a, b| b.opened_at.cmp(&a.opened_at).then(a.id.cmp(&b.id)));
        out
    }
}

/// A tunnel's place in [`Tunnels`]; dropping it (the relay ended, or the open failed half way) frees the slot.
struct Slot {
    tunnels: Arc<Tunnels>,
    tunnel: Arc<Tunnel>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.tunnels.lock().remove(&self.tunnel.id);
    }
}

fn new_tunnel_id() -> String {
    format!("tun_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// `1.2 KiB`-style sizes for the activity log.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// The ports a policy opens, for error messages.
fn ports_text(p: &TunnelPolicy) -> String {
    if p.ports.is_empty() {
        "none".into()
    } else {
        p.ports.join(", ")
    }
}

/// The node's tunnel endpoint for a VM: its API URL with `ws`/`wss`, the id as one escaped path segment.
fn node_tunnel_url(fluxvm_url: &str, vm_id: &str, port: u16) -> Option<String> {
    let base = fluxvm_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return None;
    };
    let id: String = vm_id
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    Some(format!("{ws}/v1/vms/{id}/tunnel?port={port}"))
}

/// Records a tunnel event in the activity log (`OPEN` or `CLOSE` on the tunnel's path).
fn record(
    fleet: &Fleet,
    caller: &Caller,
    key: Option<String>,
    method: &str,
    (node, vm_id): (&str, &str),
    target: String,
    status: StatusCode,
) {
    fleet.audit.record(audit::event(
        caller,
        method.to_owned(),
        format!("/fleet/vms/{node}/{vm_id}/tunnel"),
        Some(target),
        status,
        key,
    ));
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> AppError {
    AppError(status, msg.into())
}

/// `GET /fleet/vms/{node}/{id}/tunnel?port=N` (WebSocket; *tenant*: own VMs, keys that may make changes): one TCP
/// connection to port N (default 22) of the VM, relayed through central and the VM's node.
pub(super) async fn open_tunnel(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key): Extension<KeyUsed>,
    Path((node, id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, AppError> {
    let port = match q.get("port") {
        None => 22,
        Some(p) => p
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| {
                refuse(
                    StatusCode::BAD_REQUEST,
                    "port must be a number from 1 to 65535",
                )
            })?,
    };
    let key = key.0;
    let (policy, vm_name) = {
        let t = fleet.tenancy.lock().await;
        let entry = t.vm(&node, &id);
        let owned = t.owns(&caller, &node, &id)
            && (caller.tenant().is_none() || entry.is_some_and(|v| v.active()));
        if !owned {
            return Err(refuse(
                StatusCode::NOT_FOUND,
                format!("no VM '{id}' on node '{node}'"),
            ));
        }
        // GET passes the read-only check in the middleware, but a tunnel can change anything in the VM.
        if let (Caller::Tenant(tenant), Some(name)) = (&caller, &key) {
            let read_only = t
                .tenants
                .get(tenant)
                .and_then(|x| x.keys.iter().find(|k| &k.name == name))
                .is_none_or(|k| k.read_only);
            if read_only {
                drop(t);
                record(
                    &fleet,
                    &caller,
                    key.clone(),
                    "OPEN",
                    (node.as_str(), id.as_str()),
                    format!("port {port}"),
                    StatusCode::FORBIDDEN,
                );
                return Err(refuse(
                    StatusCode::FORBIDDEN,
                    format!(
                        "API key '{name}' is read-only; a tunnel needs a key that may make changes"
                    ),
                ));
            }
        }
        let name = entry
            .map(|v| v.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.clone());
        (t.policy.tunnels.clone(), name)
    };
    let target = format!("{vm_name} port {port}");
    if !policy.enabled || !policy.allows_port(port) {
        record(
            &fleet,
            &caller,
            key,
            "OPEN",
            (node.as_str(), id.as_str()),
            target,
            StatusCode::FORBIDDEN,
        );
        return Err(refuse(
            StatusCode::FORBIDDEN,
            if policy.enabled {
                format!(
                    "port {port} is not open to tunnels (allowed: {})",
                    ports_text(&policy)
                )
            } else {
                "tunnels are turned off in this cloud's policy".to_owned()
            },
        ));
    }
    let ws = ws.map_err(|_| {
        refuse(
            StatusCode::UPGRADE_REQUIRED,
            "this is a WebSocket endpoint; connect with `fluxctl fleet ssh`, `fluxctl fleet tunnel-stdio` or \
             `fluxctl fleet port-forward`",
        )
    })?;
    let target_node = fleet
        .nodes
        .lock()
        .await
        .get(&node)
        .cloned()
        .ok_or_else(|| {
            refuse(
                StatusCode::NOT_FOUND,
                format!("no registered node named '{node}'"),
            )
        })?;
    let url = node_tunnel_url(&target_node.fluxvm_url, &id, port).ok_or_else(|| {
        refuse(
            StatusCode::BAD_GATEWAY,
            format!("node '{node}' has no http(s) API address"),
        )
    })?;

    let now = Utc::now();
    let slot = fleet
        .tunnels
        .reserve(
            Tunnel {
                id: new_tunnel_id(),
                tenant: caller.tenant().map(str::to_owned),
                key: key.clone(),
                node: node.clone(),
                vm_id: id.clone(),
                vm_name: vm_name.clone(),
                port,
                opened_at: now,
                sent: AtomicU64::new(0),
                received: AtomicU64::new(0),
                last_active: AtomicI64::new(now.timestamp_millis()),
                close: tokio::sync::Notify::new(),
                closed_by: std::sync::Mutex::new(None),
            },
            policy.max_per_tenant,
        )
        .map_err(|e| {
            record(
                &fleet,
                &caller,
                key.clone(),
                "OPEN",
                (node.as_str(), id.as_str()),
                target.clone(),
                StatusCode::TOO_MANY_REQUESTS,
            );
            match e {
                SlotError::TenantLimit(n) => refuse(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!(
                        "{} {n} tunnels open, the most allowed; close one (`fluxctl fleet tunnels`) and try again",
                        if caller.tenant().is_some() {
                            "you already have"
                        } else {
                            "the operator already has"
                        }
                    ),
                ),
                SlotError::CentralFull => refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "central has as many tunnels open as it can hold; try again later",
                ),
            }
        })?;

    let node_ws = connect_node(&fleet, &url, &node, &id, port).await?;
    record(
        &fleet,
        &caller,
        key.clone(),
        "OPEN",
        (node.as_str(), id.as_str()),
        target,
        StatusCode::SWITCHING_PROTOCOLS,
    );
    let idle = policy.idle_timeout();
    Ok(ws
        .max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |client| async move {
            relay(fleet, caller, slot, client, node_ws, idle).await;
        }))
}

/// Opens the node's side of the tunnel, turning the node's refusal into an error the client can act on.
async fn connect_node(
    fleet: &Fleet,
    url: &str,
    node: &str,
    id: &str,
    port: u16,
) -> Result<NodeSocket, AppError> {
    let mut req = url
        .into_client_request()
        .map_err(|e| refuse(StatusCode::BAD_GATEWAY, format!("bad node address: {e}")))?;
    if let Some(t) = &fleet.token {
        let v: tungstenite::http::HeaderValue = format!("Bearer {t}")
            .parse()
            .map_err(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "unusable fleet token"))?;
        req.headers_mut().insert("authorization", v);
    }
    match tokio::time::timeout(NODE_CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Err(_) => Err(refuse(
            StatusCode::GATEWAY_TIMEOUT,
            format!("node '{node}' did not open the tunnel within 15 seconds"),
        )),
        Ok(Ok((ws, _))) => Ok(ws),
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let status = resp.status().as_u16();
            let body = resp.body().as_deref().unwrap_or_default();
            let msg = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| {
                    String::from_utf8_lossy(&body[..body.len().min(512)])
                        .trim()
                        .to_owned()
                });
            Err(match status {
                404 => refuse(
                    StatusCode::NOT_FOUND,
                    format!("node '{node}' has no VM '{id}': {msg}"),
                ),
                409 => refuse(
                    StatusCode::CONFLICT,
                    format!("cannot reach port {port} of the VM: {msg}"),
                ),
                401 | 403 => refuse(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "node '{node}' refused central's tunnel ({status}); give the node's API the fleet token as \
                         an admin token"
                    ),
                ),
                _ => refuse(
                    StatusCode::BAD_GATEWAY,
                    format!("node '{node}' could not open the tunnel ({status}): {msg}"),
                ),
            })
        }
        Ok(Err(e)) => Err(refuse(
            StatusCode::BAD_GATEWAY,
            format!("could not reach node '{node}' for the tunnel: {e}"),
        )),
    }
}

/// Copies messages both ways until either side closes, the tunnel is idle too long, or it is closed by `DELETE`.
async fn relay(
    fleet: Fleet,
    caller: Caller,
    slot: Slot,
    client: WebSocket,
    node: NodeSocket,
    idle: Duration,
) {
    let t = slot.tunnel.clone();
    let (mut c_tx, mut c_rx) = client.split();
    let (mut n_tx, mut n_rx) = node.split();
    let up = async {
        while let Some(Ok(msg)) = c_rx.next().await {
            let data: Vec<u8> = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(s) => s.as_str().as_bytes().to_vec(),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            let n = data.len() as u64;
            if n == 0 {
                continue;
            }
            if n_tx.send(NodeMessage::Binary(data.into())).await.is_err() {
                break;
            }
            t.sent.fetch_add(n, Ordering::Relaxed);
            t.touch();
        }
    };
    let down = async {
        while let Some(Ok(msg)) = n_rx.next().await {
            let data: Vec<u8> = match msg {
                NodeMessage::Binary(b) => b.to_vec(),
                NodeMessage::Text(s) => s.as_str().as_bytes().to_vec(),
                NodeMessage::Close(_) => break,
                _ => continue,
            };
            let n = data.len() as u64;
            if n == 0 {
                continue;
            }
            if c_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
            t.received.fetch_add(n, Ordering::Relaxed);
            t.touch();
        }
    };
    let watchdog = async {
        let every = idle.min(Duration::from_secs(30));
        loop {
            tokio::time::sleep(every).await;
            if t.idle_for() >= idle {
                return "idle timeout";
            }
            if !still_allowed(&fleet, &t).await {
                return "access was revoked (the VM ended, or the tenant or key was disabled)";
            }
        }
    };
    let reason: &'static str = tokio::select! {
        _ = up => "the client disconnected",
        _ = down => "the VM closed the connection",
        why = watchdog => why,
        _ = t.close.notified() => {
            let by: Option<&'static str> = *t.closed_by.lock().unwrap_or_else(|p| p.into_inner());
            by.unwrap_or("closed")
        }
    };
    // Tell both ends, briefly: a peer that stopped reading must not hold the relay open.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = c_tx
            .send(Message::Close(Some(CloseFrame {
                code: 1000,
                reason: reason.into(),
            })))
            .await;
        let _ = n_tx.send(NodeMessage::Close(None)).await;
    })
    .await;
    let lasted = (Utc::now() - t.opened_at).to_std().unwrap_or_default();
    record(
        &fleet,
        &caller,
        t.key.clone(),
        "CLOSE",
        (t.node.as_str(), t.vm_id.as_str()),
        format!(
            "{} port {}: {} sent, {} received in {}; {reason}",
            t.vm_name,
            t.port,
            human_bytes(t.sent.load(Ordering::Relaxed)),
            human_bytes(t.received.load(Ordering::Relaxed)),
            human_duration(lasted)
        ),
        StatusCode::OK,
    );
    drop(slot);
}

/// Whether an open tunnel's tenant may still use it: the tenant exists and is enabled, the key it was opened with still
/// exists, may make changes and has not expired, and the VM is still the tenant's. Checked by the relay's watchdog, so
/// revoking access ends open tunnels within half a minute instead of at the idle timeout.
async fn still_allowed(fleet: &Fleet, t: &Tunnel) -> bool {
    let Some(tenant) = t.tenant.as_deref() else {
        return true;
    };
    let s = fleet.tenancy.lock().await;
    let Some(x) = s.tenants.get(tenant) else {
        return false;
    };
    if x.disabled {
        return false;
    }
    if let Some(name) = &t.key {
        let now = Utc::now();
        let key_ok = x
            .keys
            .iter()
            .any(|k| &k.name == name && !k.read_only && k.expires_at.is_none_or(|e| e > now));
        if !key_ok {
            return false;
        }
    }
    s.vm(&t.node, &t.vm_id)
        .is_some_and(|v| v.active() && v.tenant.as_deref() == Some(tenant))
}

/// `GET /fleet/tunnels` (*tenant*: its own): the open tunnels, newest first, with bytes each way and idle time.
pub(super) async fn list_tunnels(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let policy = fleet.tenancy.lock().await.policy.tunnels.clone();
    let items: Vec<Value> = fleet
        .tunnels
        .visible(&caller)
        .iter()
        .map(|t| t.public_json())
        .collect();
    Json(json!({
        "items": items,
        "max_per_tenant": policy.max_per_tenant,
        "idle_timeout_minutes": policy.idle_timeout_minutes,
        "ports": policy.ports,
        "enabled": policy.enabled,
    }))
}

/// `DELETE /fleet/tunnels/{id}` (*tenant*: its own): closes a tunnel now.
pub(super) async fn close_tunnel(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let t = fleet
        .tunnels
        .get(&id)
        .filter(|t| t.visible_to(&caller))
        .ok_or_else(|| refuse(StatusCode::NOT_FOUND, format!("no open tunnel '{id}'")))?;
    *t.closed_by.lock().unwrap_or_else(|p| p.into_inner()) = Some(match caller {
        Caller::Admin => "closed by the operator",
        Caller::Tenant(_) => "closed by the tenant",
    });
    t.close.notify_one();
    Ok(Json(json!({"closed": true, "id": id})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(tenant: Option<&str>) -> Tunnel {
        Tunnel {
            id: new_tunnel_id(),
            tenant: tenant.map(str::to_owned),
            key: None,
            node: "mac".into(),
            vm_id: "vm".into(),
            vm_name: "vm".into(),
            port: 22,
            opened_at: Utc::now(),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            last_active: AtomicI64::new(Utc::now().timestamp_millis()),
            close: tokio::sync::Notify::new(),
            closed_by: std::sync::Mutex::new(None),
        }
    }

    #[test]
    fn port_policy() {
        let p = TunnelPolicy::default();
        assert!(p.validate().is_ok());
        assert!(p.allows_port(22));
        assert!(p.allows_port(1024) && p.allows_port(65535) && p.allows_port(8080));
        assert!(
            !p.allows_port(0) && !p.allows_port(23) && !p.allows_port(80) && !p.allows_port(1023)
        );
        let only_ssh = TunnelPolicy {
            ports: vec!["22".into()],
            ..TunnelPolicy::default()
        };
        assert!(only_ssh.allows_port(22) && !only_ssh.allows_port(2222));
        for bad in ["0", "70000", "10-5", "x", "1-", ""] {
            let p = TunnelPolicy {
                ports: vec![bad.into()],
                ..TunnelPolicy::default()
            };
            assert!(p.validate().is_err(), "{bad} was accepted");
        }
        for (field, p) in [
            (
                "idle 0",
                TunnelPolicy {
                    idle_timeout_minutes: 0,
                    ..TunnelPolicy::default()
                },
            ),
            (
                "max 0",
                TunnelPolicy {
                    max_per_tenant: 0,
                    ..TunnelPolicy::default()
                },
            ),
            (
                "too many rules",
                TunnelPolicy {
                    ports: vec!["22".into(); 65],
                    ..TunnelPolicy::default()
                },
            ),
        ] {
            assert!(p.validate().is_err(), "{field}");
        }
        assert_eq!(
            TunnelPolicy::default().idle_timeout(),
            Duration::from_secs(3600)
        );
        // A policy saved before tunnels existed loads with the defaults.
        let old: crate::tenancy::Policy = serde_json::from_str("{}").unwrap();
        assert_eq!(old.tunnels, TunnelPolicy::default());
    }

    #[test]
    fn tenant_routes() {
        use axum::http::Method;
        assert!(tenant_route_allowed(
            &Method::GET,
            "/fleet/vms/mac/vm/tunnel"
        ));
        assert!(!tenant_route_allowed(
            &Method::POST,
            "/fleet/vms/mac/vm/tunnel"
        ));
        assert!(tenant_route_allowed(&Method::GET, "/fleet/tunnels"));
        assert!(tenant_route_allowed(
            &Method::DELETE,
            "/fleet/tunnels/tun_1"
        ));
        assert!(!tenant_route_allowed(&Method::DELETE, "/fleet/tunnels"));
    }

    #[test]
    fn slots_are_limited_per_tenant_and_freed_on_drop() {
        let tunnels = Arc::new(Tunnels::default());
        let a1 = tunnels.reserve(tunnel(Some("acme")), 2).unwrap();
        let a2 = tunnels.reserve(tunnel(Some("acme")), 2).unwrap();
        assert_eq!(
            tunnels.reserve(tunnel(Some("acme")), 2).err(),
            Some(SlotError::TenantLimit(2))
        );
        // Another tenant and the operator have their own count.
        let _g = tunnels.reserve(tunnel(Some("globex")), 2).unwrap();
        let _op = tunnels.reserve(tunnel(None), 2).unwrap();
        assert_eq!(tunnels.visible(&Caller::Admin).len(), 4);
        assert_eq!(tunnels.visible(&Caller::Tenant("acme".into())).len(), 2);
        assert!(
            tunnels
                .visible(&Caller::Tenant("acme".into()))
                .iter()
                .all(|t| t.tenant.as_deref() == Some("acme"))
        );
        let id = a1.tunnel.id.clone();
        drop(a1);
        assert!(tunnels.get(&id).is_none());
        assert!(tunnels.reserve(tunnel(Some("acme")), 2).is_ok());
        drop(a2);
    }

    #[test]
    fn node_urls_and_formatting() {
        assert_eq!(
            node_tunnel_url("http://10.0.0.5:7788/", "ab-12", 22).as_deref(),
            Some("ws://10.0.0.5:7788/v1/vms/ab-12/tunnel?port=22")
        );
        assert_eq!(
            node_tunnel_url("https://mac.lan", "x/../y", 8080).as_deref(),
            Some("wss://mac.lan/v1/vms/x%2F%2E%2E%2Fy/tunnel?port=8080")
        );
        assert_eq!(node_tunnel_url("unix:///tmp/s", "x", 22), None);
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_duration(Duration::from_secs(3725)), "1h02m");
        let t = tunnel(Some("acme"));
        t.sent.fetch_add(5, Ordering::Relaxed);
        let v = t.public_json();
        assert_eq!(v["bytes_sent"], 5);
        assert_eq!(v["port"], 22);
        assert!(t.visible_to(&Caller::Tenant("acme".into())));
        assert!(!t.visible_to(&Caller::Tenant("globex".into())));
    }
}
