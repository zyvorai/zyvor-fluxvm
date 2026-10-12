// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Notifications: what happened that someone should know about, shown in the console and sent to webhooks.
//!
//! Events come from two places: changes made through central (a VM launched or deleted, a lease taken or released),
//! recorded by the auth middleware next to the activity log; and the reconciler, which looks for things that happen
//! with time (a lease ending soon or ended, a budget near or over, a Mac that stopped reporting, a drain that
//! finished, a Mac waiting for a scrub). Time-based events are sent once; the "already sent" set lives in memory, so a
//! restarted central may send one of them again.
//!
//! A webhook gets each event as JSON, signed with HMAC-SHA256 in `X-FluxVM-Signature: sha256=<hex>`, with up to three
//! attempts. A tenant's webhook must be HTTPS to a public address: the host is resolved at every delivery, private,
//! loopback and link-local addresses are refused, and the connection is pinned to the address that was checked.

use super::*;
use crate::tenancy::Webhook;
use std::collections::{HashSet, VecDeque};
use std::net::IpAddr;

/// Notifications kept for `GET /fleet/notifications`.
const KEEP: usize = 1000;
/// Deliveries kept per webhook.
const KEEP_DELIVERIES: usize = 20;
/// Webhooks per tenant (and for the operator).
const MAX_HOOKS: usize = 10;
/// Deliveries in flight at once (each with its retries); past this, an event is logged as dropped for that hook.
const MAX_IN_FLIGHT: usize = 256;

/// Every kind of event; `operator` ones never reach a tenant.
pub(super) const KINDS: &[(&str, bool)] = &[
    ("vm.created", false),
    ("vm.deleted", false),
    ("vm.snapshot", false),
    ("vm.restored", false),
    ("vm.cloned", false),
    ("lease.created", false),
    ("lease.released", false),
    ("lease.ending", false),
    ("lease.ended", false),
    ("budget.near", false),
    ("budget.over", false),
    ("node.down", true),
    ("node.scrub", true),
    ("drain.done", true),
    ("test", false),
    // ---- volumes ----
    ("volume.created", false),
    ("volume.deleted", false),
    // ---- signup ----
    ("billing.payment_failed", false),
    ("billing.suspended", false),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Notification {
    pub id: String,
    pub time: DateTime<Utc>,
    #[serde(rename = "type")]
    pub kind: String,
    /// The tenant it concerns; `None` for fleet-wide (operator) events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// A console route, such as `#/macs/studio-1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Delivery {
    pub notification: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub time: DateTime<Utc>,
    pub attempt: u32,
    /// The HTTP status, or `None` when no response came back.
    pub status: Option<u16>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct Notifier {
    items: std::sync::Mutex<VecDeque<Notification>>,
    deliveries: std::sync::Mutex<HashMap<String, VecDeque<Delivery>>>,
    /// Keys of time-based events already sent.
    sent: std::sync::Mutex<HashSet<String>>,
    /// Delivery tasks running now.
    in_flight: std::sync::atomic::AtomicUsize,
    /// Deliveries recorded since start, by outcome (`fluxvm_central_webhook_deliveries_total`).
    delivered_ok: std::sync::atomic::AtomicU64,
    delivered_failed: std::sync::atomic::AtomicU64,
}

/// Counts a delivery task as running until dropped.
struct InFlight(Arc<Notifier>);

impl InFlight {
    fn start(n: &Arc<Notifier>) -> Option<Self> {
        use std::sync::atomic::Ordering;
        if n.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
            n.in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(InFlight(n.clone()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0
            .in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Notifier {
    fn push(&self, n: Notification) {
        let mut items = lock(&self.items);
        if items.len() == KEEP {
            items.pop_front();
        }
        items.push_back(n);
    }

    /// Newest first; a tenant sees only its own.
    pub(super) fn list(&self, tenant: Option<&str>, limit: usize) -> Vec<Notification> {
        lock(&self.items)
            .iter()
            .rev()
            .filter(|n| tenant.is_none_or(|t| n.tenant.as_deref() == Some(t)))
            .take(limit)
            .cloned()
            .collect()
    }

    fn record_delivery(&self, hook: &str, d: Delivery) {
        let counter = if d.ok {
            &self.delivered_ok
        } else {
            &self.delivered_failed
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut all = lock(&self.deliveries);
        let q = all.entry(hook.to_owned()).or_default();
        if q.len() == KEEP_DELIVERIES {
            q.pop_front();
        }
        q.push_back(d);
    }

    fn deliveries(&self, hook: &str) -> Vec<Delivery> {
        lock(&self.deliveries)
            .get(hook)
            .map(|q| q.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// True the first time `key` is seen.
    fn first(&self, key: String) -> bool {
        lock(&self.sent).insert(key)
    }

    /// Lets a condition that has cleared fire again later (a Mac scrubbed, then needing a scrub again).
    fn clear(&self, key: &str) {
        lock(&self.sent).remove(key);
    }

    /// Forgets "already sent" keys that can no longer fire, so the set does not grow forever.
    fn prune(&self, keep: impl Fn(&str) -> bool) {
        lock(&self.sent).retain(|k| keep(k.as_str()));
    }

    /// Webhook deliveries so far: (succeeded, failed).
    pub(super) fn delivery_counts(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.delivered_ok.load(Relaxed),
            self.delivered_failed.load(Relaxed),
        )
    }

    /// Drops the delivery logs of webhooks that no longer exist.
    pub(super) fn keep_deliveries(&self, hooks: &[Webhook]) {
        lock(&self.deliveries).retain(|id, _| hooks.iter().any(|h| &h.id == id));
    }

    /// Drops a deleted tenant's notifications, so a new tenant with the same id never sees them.
    pub(super) fn forget_tenant(&self, tenant: &str) {
        lock(&self.items).retain(|n| n.tenant.as_deref() != Some(tenant));
    }
}

fn operator_only(kind: &str) -> bool {
    KINDS.iter().any(|(k, op)| *k == kind && *op)
}

fn wants(hook: &Webhook, n: &Notification) -> bool {
    let audience = match &hook.owner {
        None => true,
        Some(t) => n.tenant.as_deref() == Some(t.as_str()) && !operator_only(&n.kind),
    };
    audience && (hook.events.is_empty() || hook.events.iter().any(|e| *e == n.kind))
}

/// Records a notification and sends it to every webhook that wants it.
pub(super) async fn emit(
    fleet: &Fleet,
    kind: &str,
    tenant: Option<String>,
    title: String,
    detail: Option<String>,
    link: Option<String>,
) {
    let n = Notification {
        id: format!("ntf_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        time: Utc::now(),
        kind: kind.to_owned(),
        tenant,
        title,
        detail,
        link,
    };
    fleet.notify.push(n.clone());
    let hooks: Vec<Webhook> = fleet
        .tenancy
        .lock()
        .await
        .webhooks
        .iter()
        .filter(|h| wants(h, &n))
        .cloned()
        .collect();
    if hooks.is_empty() || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    for hook in hooks {
        let Some(running) = InFlight::start(&fleet.notify) else {
            fleet.notify.record_delivery(
                &hook.id,
                Delivery {
                    notification: n.id.clone(),
                    kind: n.kind.clone(),
                    time: Utc::now(),
                    attempt: 0,
                    status: None,
                    ok: false,
                    error: Some("dropped: too many deliveries in flight".into()),
                },
            );
            continue;
        };
        let notifier = fleet.notify.clone();
        let n = n.clone();
        tokio::spawn(async move {
            let _running = running;
            for (attempt, wait) in [(1u32, 0u64), (2, 2), (3, 10)] {
                if wait > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                }
                let d = deliver(&hook, &n, attempt).await;
                let ok = d.ok;
                notifier.record_delivery(&hook.id, d);
                if ok {
                    break;
                }
            }
        });
    }
}

/// The body a webhook receives.
pub(super) fn payload(n: &Notification) -> Vec<u8> {
    serde_json::to_vec(n).unwrap_or_default()
}

pub(super) fn signature(secret: &str, body: &[u8]) -> String {
    let mac = crate::gha::hmac_sha256(secret.as_bytes(), body);
    format!(
        "sha256={}",
        mac.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Whether an address is on the public internet (not loopback, private, link-local, shared, documentation...).
pub(super) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.is_documentation()
                || v.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
                || o[0] >= 240)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v.segments();
            let v4 = |hi: u16, lo: u16| {
                IpAddr::V4(std::net::Ipv4Addr::new(
                    (hi >> 8) as u8,
                    hi as u8,
                    (lo >> 8) as u8,
                    lo as u8,
                ))
            };
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) reach the IPv4 address inside them.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
                return is_public(v4(s[6], s[7]));
            }
            if s[0] == 0x2002 {
                return is_public(v4(s[1], s[2]));
            }
            !(s[..6] == [0; 6] // ::/96: unspecified, loopback, IPv4-compatible
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local
                || (s[0] == 0x64 && s[1] == 0xff9b) // local-use NAT64 (64:ff9b:1::/48)
                || (s[0] == 0x100 && s[1..4] == [0; 3]) // discard
                || (s[0] == 0x2001 && s[1] == 0) // Teredo
                || (s[0] == 0x2001 && s[1] == 0x0db8)
                || (s[0] == 0x3fff && s[1] < 0x1000))
        }
    }
}

/// Checks a webhook URL when it is registered. Tenants need HTTPS and a host that is not a non-public IP literal;
/// the real check is at delivery, when the host is resolved.
pub(super) fn check_url(url: &str, tenant: bool) -> Result<reqwest::Url, String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("not a URL: {e}"))?;
    match (u.scheme(), tenant) {
        ("https", _) | ("http", false) => {}
        _ => return Err("a webhook URL must use https".into()),
    }
    let host = u.host_str().ok_or("a webhook URL needs a host")?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err("put credentials in the secret, not the URL".into());
    }
    if tenant {
        if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
            && !is_public(ip)
        {
            return Err("a webhook must go to a public address".into());
        }
        if host.eq_ignore_ascii_case("localhost")
            || host.ends_with(".localhost")
            || host.ends_with(".internal")
        {
            return Err("a webhook must go to a public address".into());
        }
    }
    if url.len() > 2048 {
        return Err("a webhook URL is at most 2048 characters".into());
    }
    Ok(u)
}

async fn deliver(hook: &Webhook, n: &Notification, attempt: u32) -> Delivery {
    let mut d = Delivery {
        notification: n.id.clone(),
        kind: n.kind.clone(),
        time: Utc::now(),
        attempt,
        status: None,
        ok: false,
        error: None,
    };
    match send(hook, n).await {
        Ok(status) => {
            d.status = Some(status);
            d.ok = (200..300).contains(&status);
            if !d.ok {
                d.error = Some(format!("the endpoint answered {status}"));
            }
        }
        Err(e) => d.error = Some(e),
    }
    d
}

async fn send(hook: &Webhook, n: &Notification) -> Result<u16, String> {
    let url = check_url(&hook.url, hook.owner.is_some())?;
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10));
    // An IP literal was checked by `check_url`; a name is resolved here.
    let named = url
        .host_str()
        .is_some_and(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().is_err());
    // An operator webhook to this machine or an internal address goes direct: a proxy from the environment (HTTP_PROXY)
    // could not reach it anyway.
    let direct = url.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost")
            || h.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| !is_public(ip))
    });
    if direct {
        builder = builder.no_proxy();
    }
    if hook.owner.is_some() {
        // No proxy from the environment: a proxy would resolve the host itself, past the check below.
        builder = builder.no_proxy();
    }
    if hook.owner.is_some() && named {
        // Resolve now, refuse non-public addresses, and pin the connection to the address that was checked, so a DNS
        // answer that changes between check and connect cannot reach the internal network.
        let host = url.host_str().unwrap_or_default().to_owned();
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| format!("cannot resolve {host}: {e}"))?
            .collect();
        let addr = addrs
            .iter()
            .copied()
            .find(|a| is_public(a.ip()))
            .ok_or_else(|| format!("{host} does not resolve to a public address"))?;
        if addrs.iter().any(|a| !is_public(a.ip())) {
            return Err(format!("{host} resolves to a non-public address"));
        }
        builder = builder.resolve(&host, addr);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let body = payload(n);
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .header("user-agent", "fluxvm-central")
        .header("x-fluxvm-event", n.kind.as_str())
        .header("x-fluxvm-delivery", n.id.as_str())
        .header("x-fluxvm-signature", signature(&hook.secret, &body))
        .body(body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timed out after 10 seconds".to_owned()
            } else {
                format!("could not deliver: {e}")
            }
        })?;
    Ok(resp.status().as_u16())
}

// ---- events from changes ------------------------------------------------------------------------------------------

/// The notification for a successful change recorded in the activity log, if it is one people want to hear about.
pub(super) async fn from_change(fleet: &Fleet, e: &super::audit::AuditEvent) {
    if e.status >= 400 {
        return;
    }
    let seg: Vec<&str> = e.path.trim_matches('/').split('/').collect();
    let who = match &e.tenant {
        Some(t) => e
            .key
            .as_ref()
            .map_or(t.clone(), |k| format!("{t} (key {k})")),
        None => "the operator".to_owned(),
    };
    let target = e.target.clone().unwrap_or_default();
    let (kind, title, link) = match (e.method.as_str(), seg.as_slice()) {
        ("POST", ["fleet", "vms"]) => (
            "vm.created",
            format!("VM {target} launched"),
            Some("#/vms".to_owned()),
        ),
        ("DELETE", ["fleet", "vms", node, _]) => (
            "vm.deleted",
            format!("A VM on {node} was deleted"),
            Some("#/vms".to_owned()),
        ),
        ("POST", ["fleet", "leases"]) => (
            "lease.created",
            "A Mac was leased".to_owned(),
            Some("#/macs".to_owned()),
        ),
        ("POST", ["fleet", "leases", _, "release"]) => (
            "lease.released",
            "A lease was released".to_owned(),
            Some("#/macs".to_owned()),
        ),
        // ---- snapshots ----
        ("POST", ["fleet", "vms", _, _, "snapshots" | "clone"])
        | ("POST", ["fleet", "vms", _, _, "snapshots", _, "restore"]) => {
            match super::snapshots::notice(fleet, &seg, &target).await {
                Some(n) => n,
                None => return,
            }
        }
        // ---- volumes ----
        ("POST", ["fleet", "volumes"]) => (
            "volume.created",
            format!("Volume {target} created"),
            Some("#/volumes".to_owned()),
        ),
        ("DELETE", ["fleet", "volumes", name]) => (
            "volume.deleted",
            format!("Volume {name} deleted"),
            Some("#/volumes".to_owned()),
        ),
        _ => return,
    };
    // An operator's (forced) release is the tenant's news too: the lease says whose it is.
    let tenant = match (&e.tenant, kind, seg.as_slice()) {
        (None, "lease.released", ["fleet", "leases", id, _]) => fleet
            .tenancy
            .lock()
            .await
            .leases
            .iter()
            .find(|l| l.id == *id)
            .map(|l| l.tenant.clone()),
        _ => e.tenant.clone(),
    };
    emit(fleet, kind, tenant, title, Some(format!("By {who}.")), link).await;
}

// ---- events from time ---------------------------------------------------------------------------------------------

/// Looks for time-based events; called by the reconciler. `ended` are the leases it just ended.
pub(super) async fn scan(fleet: &Fleet, ended: &[crate::tenancy::Lease]) {
    let now = Utc::now();
    let mut out: Vec<(
        String,
        &'static str,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    for l in ended {
        out.push((
            format!("ended:{}", l.id),
            "lease.ended",
            Some(l.tenant.clone()),
            format!("Your lease of {} ended", l.node),
            Some("Its VMs were deleted. The Mac is scrubbed before anyone else uses it.".into()),
            Some("#/macs".into()),
        ));
    }
    let (from, to) = crate::billing::month_of(now);
    let month = from.format("%Y-%m").to_string();
    let (budgets, leases, scrub, draining) = {
        let t = fleet.tenancy.lock().await;
        let budgets: Vec<(String, String, f64, f64, String)> = t
            .tenants
            .values()
            .filter(|x| x.monthly_budget.is_some())
            .filter_map(|x| {
                let st = crate::billing::statement(&t, &x.id, from, to, now);
                st.budget.map(|b| {
                    (
                        x.id.clone(),
                        b.state.to_owned(),
                        b.monthly,
                        st.total,
                        st.currency,
                    )
                })
            })
            .map(|(id, state, monthly, total, cur)| {
                (id, format!("{month}:{state}"), monthly, total, cur)
            })
            .collect();
        let leases: Vec<crate::tenancy::Lease> = t
            .leases
            .iter()
            .filter(|l| l.state == crate::tenancy::LeaseState::Active && l.ends_at.is_some())
            .cloned()
            .collect();
        (budgets, leases, t.scrub_pending.clone(), t.draining.clone())
    };
    for (tenant, key, monthly, total, cur) in budgets {
        let state = key.rsplit(':').next().unwrap_or("ok").to_owned();
        let kind = match state.as_str() {
            "near" => "budget.near",
            "over" => "budget.over",
            _ => continue,
        };
        out.push((
            format!("budget:{tenant}:{key}"),
            kind,
            Some(tenant),
            if kind == "budget.over" {
                format!("Over this month's budget of {monthly:.2} {cur}")
            } else {
                format!("80% of this month's budget of {monthly:.2} {cur} used")
            },
            Some(format!("{total:.2} {cur} so far.")),
            Some("#/billing".into()),
        ));
    }
    let active: HashSet<String> = leases.iter().map(|l| l.id.clone()).collect();
    for l in leases {
        let Some(end) = l.ends_at else { continue };
        let left = (end - now).num_minutes();
        let (key, when) = if left <= 60 {
            (format!("ending1:{}", l.id), "in less than an hour")
        } else if left <= 24 * 60 {
            (format!("ending24:{}", l.id), "within 24 hours")
        } else {
            continue;
        };
        out.push((
            key,
            "lease.ending",
            Some(l.tenant.clone()),
            format!("Your lease of {} ends {when}", l.node),
            Some(format!(
                "At {}. Its VMs are deleted then.",
                end.format("%Y-%m-%d %H:%M UTC")
            )),
            Some(format!("#/macs/{}", l.node)),
        ));
    }
    let nodes: Vec<NodeInfo> = fleet.nodes.lock().await.values().cloned().collect();
    for n in &nodes {
        let down = format!("down:{}", n.name);
        if n.healthy() {
            fleet.notify.clear(&down);
        } else {
            out.push((
                down,
                "node.down",
                None,
                format!("{} stopped reporting", n.name),
                Some(format!(
                    "Last heard from at {}.",
                    n.last_seen.format("%Y-%m-%d %H:%M UTC")
                )),
                Some(format!("#/macs/{}", n.name)),
            ));
        }
        let sk = format!("scrub:{}", n.name);
        if scrub.contains(&n.name) {
            out.push((
                sk,
                "node.scrub",
                None,
                format!("{} needs a scrub", n.name),
                Some("Its lease ended; scrub it before it can be leased again.".into()),
                Some(format!("#/macs/{}", n.name)),
            ));
        } else {
            fleet.notify.clear(&sk);
        }
        let dk = format!("drained:{}", n.name);
        if draining.contains(&n.name) && n.vm_count == 0 {
            out.push((
                dk,
                "drain.done",
                None,
                format!("{} has finished draining", n.name),
                Some("Uncordon it when maintenance is done.".into()),
                Some(format!("#/macs/{}", n.name)),
            ));
        } else {
            fleet.notify.clear(&dk);
        }
    }
    for (key, kind, tenant, title, detail, link) in out {
        if fleet.notify.first(key) {
            emit(fleet, kind, tenant, title, detail, link).await;
        }
    }
    // Keys for leases no longer active, past months, Macs no longer registered, and ended leases (reported once, by
    // the reconciler that ended them) can never fire again.
    let this_month = format!(":{month}:");
    fleet.notify.prune(|k| {
        let (kind, rest) = k.split_once(':').unwrap_or((k, ""));
        match kind {
            "ending1" | "ending24" => active.contains(rest),
            "budget" => rest.contains(&this_month),
            "down" | "scrub" | "drained" => nodes.iter().any(|n| n.name == rest),
            _ => false,
        }
    });
}

// ---- API ----------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct ListQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /fleet/notifications[?limit=N]`, newest first. A tenant sees its own.
pub(super) async fn notifications(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(50).clamp(1, KEEP);
    Json(json!({ "items": fleet.notify.list(caller.tenant(), limit) }))
}

fn hook_view(fleet: &Fleet, h: &Webhook) -> Value {
    let mut v = h.public_json();
    v["deliveries"] = json!(fleet.notify.deliveries(&h.id));
    v
}

/// `GET /fleet/webhooks`: a tenant's own, or (operator) every webhook, with recent deliveries. Never the secret.
pub(super) async fn list_webhooks(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let items: Vec<Value> = t
        .webhooks
        .iter()
        .filter(|h| caller.tenant().is_none() || h.owner.as_deref() == caller.tenant())
        .map(|h| hook_view(&fleet, h))
        .collect();
    Json(
        json!({ "items": items, "kinds": KINDS.iter().filter(|(_, op)| caller.tenant().is_none() || !op).map(|(k, _)| *k).collect::<Vec<_>>() }),
    )
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateHookBody {
    url: String,
    #[serde(default)]
    events: Vec<String>,
}

/// `POST /fleet/webhooks {url, events?}`: the secret is in the answer, once.
pub(super) async fn create_webhook(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(b): Json<CreateHookBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let owner = caller.tenant().map(str::to_owned);
    check_url(&b.url, owner.is_some()).map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    for e in &b.events {
        match KINDS.iter().find(|(k, _)| *k == e.as_str()) {
            Some((_, true)) if owner.is_some() => {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("'{e}' is an operator event"),
                ));
            }
            Some(_) => {}
            None => {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("unknown event '{e}'"),
                ));
            }
        }
    }
    let mut t = fleet.tenancy.lock().await;
    if t.webhooks.iter().filter(|h| h.owner == owner).count() >= MAX_HOOKS {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_HOOKS} webhooks; delete one first"),
        ));
    }
    let hook = Webhook {
        id: format!("whk_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        owner,
        url: b.url,
        events: b.events,
        secret: format!("whsec_{}", uuid::Uuid::new_v4().simple()),
        created_at: Utc::now(),
    };
    t.webhooks.push(hook.clone());
    fleet.persist_tenancy(&t);
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "webhook": hook.public_json(),
            "secret": hook.secret,
            "note": "store this secret now; it signs every delivery (X-FluxVM-Signature) and is not shown again",
        })),
    ))
}

fn own_hook(t: &TenancyState, caller: &Caller, id: &str) -> Result<Webhook, AppError> {
    t.webhooks
        .iter()
        .find(|h| {
            h.id == id && (caller.tenant().is_none() || h.owner.as_deref() == caller.tenant())
        })
        .cloned()
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no webhook '{id}'")))
}

/// `DELETE /fleet/webhooks/{id}`.
pub(super) async fn delete_webhook(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut t = fleet.tenancy.lock().await;
    own_hook(&t, &caller, &id)?;
    t.webhooks.retain(|h| h.id != id);
    fleet.persist_tenancy(&t);
    fleet.notify.keep_deliveries(&t.webhooks);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /fleet/webhooks/{id}/test`: one delivery of a `test` event, answered with its result.
pub(super) async fn test_webhook(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let hook = own_hook(&*fleet.tenancy.lock().await, &caller, &id)?;
    let n = Notification {
        id: format!("ntf_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        time: Utc::now(),
        kind: "test".into(),
        tenant: hook.owner.clone(),
        title: "A test from FluxVM Cloud".into(),
        detail: Some("If you can read this, the webhook works.".into()),
        link: None,
    };
    let d = deliver(&hook, &n, 1).await;
    fleet.notify.record_delivery(&hook.id, d.clone());
    Ok(Json(json!({ "delivery": d })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(owner: Option<&str>, events: &[&str]) -> Webhook {
        Webhook {
            id: "whk_1".into(),
            owner: owner.map(str::to_owned),
            url: "https://example.com/hook".into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            secret: "s".into(),
            created_at: Utc::now(),
        }
    }

    fn n(kind: &str, tenant: Option<&str>) -> Notification {
        Notification {
            id: "ntf_1".into(),
            time: Utc::now(),
            kind: kind.into(),
            tenant: tenant.map(str::to_owned),
            title: "t".into(),
            detail: None,
            link: None,
        }
    }

    /// An operator's force-release carries no tenant of its own; the notification must still be the lease's tenant's.
    #[tokio::test]
    async fn forced_release_by_the_operator_notifies_the_leases_tenant() {
        use crate::tenancy::{Lease, LeaseState};
        let fleet = Fleet {
            nodes: Default::default(),
            http: reqwest::Client::new(),
            persist_path: None,
            lock_path: None,
            token: None,
            tenancy: Default::default(),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(super::audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::default(),
        };
        let now = Utc::now();
        fleet.tenancy.lock().await.leases.push(Lease {
            id: "lease-1".into(),
            tenant: "acme".into(),
            node: "mac-1".into(),
            started_at: now,
            min_end: now,
            ends_at: None,
            released_at: None,
            state: LeaseState::Active,
            rate_micros: None,
            price_class: None,
        });
        let ev = |tenant: Option<&str>, path: &str, status: u16| super::audit::AuditEvent {
            time: now,
            actor: tenant.unwrap_or("operator").into(),
            tenant: tenant.map(str::to_owned),
            method: "POST".into(),
            path: path.into(),
            target: None,
            status,
            key: None,
            user: None,
        };
        // Refused releases are not news.
        from_change(&fleet, &ev(None, "/fleet/leases/lease-1/release", 409)).await;
        assert!(fleet.notify.list(None, 10).is_empty());
        // The operator forces it: acme hears, with the operator named.
        from_change(&fleet, &ev(None, "/fleet/leases/lease-1/release", 200)).await;
        let acme = fleet.notify.list(Some("acme"), 10);
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0].kind, "lease.released");
        assert_eq!(acme[0].detail.as_deref(), Some("By the operator."));
        // A lease nobody owns is not attributed to anyone; another tenant never sees it.
        from_change(&fleet, &ev(None, "/fleet/leases/nope/release", 200)).await;
        assert_eq!(fleet.notify.list(Some("globex"), 10).len(), 0);
        // A tenant releasing its own lease still names itself.
        from_change(
            &fleet,
            &ev(Some("acme"), "/fleet/leases/lease-1/release", 200),
        )
        .await;
        assert_eq!(
            fleet.notify.list(Some("acme"), 10)[0].detail.as_deref(),
            Some("By acme.")
        );
    }

    #[test]
    fn who_gets_what() {
        // A tenant's hook: its own events, never operator ones or another tenant's.
        let h = hook(Some("acme"), &[]);
        assert!(wants(&h, &n("vm.created", Some("acme"))));
        assert!(!wants(&h, &n("vm.created", Some("globex"))));
        assert!(!wants(&h, &n("node.down", None)));
        // The operator's: everything. An event filter narrows it.
        assert!(wants(&hook(None, &[]), &n("node.down", None)));
        assert!(wants(&hook(None, &[]), &n("vm.created", Some("acme"))));
        let only = hook(Some("acme"), &["lease.ending"]);
        assert!(!wants(&only, &n("vm.created", Some("acme"))));
        assert!(wants(&only, &n("lease.ending", Some("acme"))));
    }

    #[test]
    fn tenant_urls_are_public_https() {
        assert!(check_url("https://hooks.slack.com/services/x", true).is_ok());
        assert!(check_url("http://hooks.example.com/x", true).is_err());
        assert!(
            check_url("http://10.0.0.5/x", false).is_ok(),
            "the operator may use internal URLs"
        );
        for bad in [
            "https://127.0.0.1/x",
            "https://10.1.2.3/x",
            "https://169.254.169.254/latest",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://localhost/x",
            "https://user:pw@example.com/x",
            "not a url",
        ] {
            assert!(check_url(bad, true).is_err(), "{bad}");
        }
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(!is_public("100.64.1.1".parse().unwrap()));
        assert!(!is_public("::ffff:192.168.1.1".parse().unwrap()));
        for ip in [
            "0.0.0.0",
            "::",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:c0a8:101::1",
            "2001::1",
            "fec0::1",
            "224.0.0.1",
            "ff02::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public("64:ff9b::808:808".parse().unwrap()));
        assert!(is_public("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn signatures_match_github_style_verification() {
        let body = payload(&n("test", None));
        let sig = signature("whsec_x", &body);
        assert!(crate::gha::verify_signature(b"whsec_x", &body, &sig));
        assert!(!crate::gha::verify_signature(b"other", &body, &sig));
    }

    #[test]
    fn time_events_fire_once_until_cleared() {
        let x = Notifier::default();
        assert!(x.first("down:mac".into()));
        assert!(!x.first("down:mac".into()));
        x.clear("down:mac");
        assert!(x.first("down:mac".into()));
        x.push(n("vm.created", Some("acme")));
        x.push(n("node.down", None));
        assert_eq!(x.list(Some("acme"), 10).len(), 1);
        assert_eq!(x.list(None, 10).len(), 2);
    }
}
