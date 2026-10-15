// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The per-host half: periodically reports this node's identity, capacity,
//! and current VM count to a central `fluxvm-agent central` registry.
//! Runs alongside a local `fluxctl serve` — it never touches VMs directly,
//! only reads `GET /v1/vms` off the local REST API to count them.

use anyhow::{Context, Result};
use fluxvm_core::security::{HostCapabilities, NodeSecurityCapabilities};
use fluxvm_scheduler::apple_placement::AppleHostCaps;
use serde_json::json;
use std::{collections::HashMap, time::Duration};

pub struct NodeConfig {
    pub name: String,
    pub central_url: String,
    /// Address this agent itself uses to reach the local `fluxctl serve`
    /// for counting VMs — almost always a loopback address
    /// (`http://127.0.0.1:7788`).
    pub fluxvm_url: String,
    /// Address a REMOTE central registry should use to reach this same
    /// `fluxctl serve` — this has to be this host's real, externally
    /// routable address, not `fluxvm_url` unchanged.
    pub advertise_url: String,
    pub interval: Duration,
    /// Bearer token shared with `fluxvm-agent central --token`.
    pub token: Option<String>,
    /// Operator-set labels (`--label key=value`, repeatable) carried on
    /// every heartbeat -- see `central::NodeInfo::labels` for what they're
    /// used for (`nodeSelector`-based placement).
    pub labels: HashMap<String, String>,
    /// Where to write this Mac's `overlay.json` (private networks spanning Macs) after each heartbeat; `None` turns
    /// the sync off.
    pub overlay_config: Option<std::path::PathBuf>,
    /// Report this simulated host instead of the real one (`fluxvm-agent sim-node`).
    pub sim: Option<SimHost>,
}

/// What a simulated Mac reports: its size and its Virtualization.framework capabilities.
#[derive(Debug, Clone)]
pub struct SimHost {
    pub vcpus: u32,
    pub memory_mib: u64,
    pub nested_virtualization: bool,
    pub vmnet: bool,
}

/// Runs forever, heartbeating every `cfg.interval`. Logs and keeps going on
/// a failed beat (the central registry marking this node stale after
/// `HEALTHY_WINDOW_SECS` is the correct outcome of a *sustained* outage —
/// a node agent that gave up after one failed request would make a
/// transient network blip permanently evict an otherwise-fine node).
pub async fn run(cfg: NodeConfig) {
    // No redirects: a standby answers 503 and the next configured central is tried instead, so the bearer token only
    // ever goes to URLs the operator listed.
    // Bounded: a central or a local daemon that accepts the connection and never answers must not
    // hold the heartbeat loop forever (the node would then simply go stale).
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut preferred = 0usize;
    let mut failures = 0u32;
    loop {
        let outcome = beat(&http, &cfg, preferred).await;
        failures = if outcome.is_ok() {
            0
        } else {
            failures.saturating_add(1)
        };
        match outcome {
            Ok(used) => {
                preferred = used;
                tracing::debug!(node = %cfg.name, "heartbeat sent");
                if let Some(path) = &cfg.overlay_config {
                    let central = central_urls(&cfg.central_url)[used].clone();
                    match sync_overlays(&http, &central, cfg.token.as_deref(), &cfg.name, path)
                        .await
                    {
                        Ok(true) => {
                            tracing::info!(path = %path.display(), "overlay config updated")
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(error = %e, "overlay sync failed"),
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, failures, "heartbeat failed, will retry with backoff")
            }
        }
        tokio::time::sleep(retry_delay(cfg.interval, failures, jitter_fraction())).await;
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait before the next heartbeat. After a success: the interval. After `failures`
/// consecutive failures: the interval doubled per failure up to 8x, then scaled into 50-100% by
/// `jitter` (0.0..1.0), so a fleet whose central just came back does not send every node's beat in
/// the same instant. Never longer than the interval times 8, so a node stays well inside the
/// central's stale window.
fn retry_delay(interval: Duration, failures: u32, jitter: f64) -> Duration {
    if failures == 0 {
        return interval;
    }
    let factor = 1u32 << failures.min(3);
    let base = interval.saturating_mul(factor);
    base.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

/// A cheap 0.0..1.0 from the clock; jitter needs spread, not randomness.
fn jitter_fraction() -> f64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (n % 1000) as f64 / 1000.0
}

/// The configured centrals: `--central` is one URL or a comma-separated list (HA).
pub fn central_urls(s: &str) -> Vec<String> {
    s.split(',')
        .map(|u| u.trim().trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
        .collect()
}

/// Sends one heartbeat, starting with central `preferred` (the one that answered last time) and trying the others
/// in order when it is down or a standby. Returns the index of the central that took it.
async fn beat(http: &reqwest::Client, cfg: &NodeConfig, preferred: usize) -> Result<usize> {
    // A daemon that cannot be reached is reported, not hidden: the node still heartbeats (so it
    // is visibly alive and says why it cannot take work) instead of just going stale.
    let (local, daemon) = match local_vm_stats(http, &cfg.fluxvm_url).await {
        Ok(l) => (l, json!({"reachable": true})),
        Err(e) => (
            LocalVmStats::default(),
            json!({"reachable": false, "error": format!("{e:#}")}),
        ),
    };
    let reachable = daemon["reachable"] == json!(true);
    let vm_count = local.count;
    let (vcpus_total, memory_mib_total) = match &cfg.sim {
        Some(h) => (h.vcpus, h.memory_mib),
        None => (
            std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
            total_memory_mib().unwrap_or(0),
        ),
    };
    let (security, density) = if reachable {
        (
            fetch_node_security(http, &cfg.fluxvm_url).await,
            fetch_density(http, &cfg.fluxvm_url).await,
        )
    } else {
        (NodeSecurityCapabilities::standard_only(), None)
    };
    let apple = match &cfg.sim {
        Some(h) => Some(sim_apple_caps(h, local)),
        None => fetch_apple_caps(vcpus_total, memory_mib_total, local).await,
    };
    let allocated = reachable
        .then(|| json!({"vcpus": local.allocated_vcpus, "memory_mib": local.allocated_memory_mib}));

    let body = json!({
        "name": cfg.name,
        "fluxvm_url": cfg.advertise_url,
        "vcpus_total": vcpus_total,
        "memory_mib_total": memory_mib_total,
        "vm_count": vm_count,
        "labels": cfg.labels,
        "security": security,
        "density": density,
        "apple": apple,
        "daemon": daemon,
        "allocated": allocated,
    });
    let urls = central_urls(&cfg.central_url);
    anyhow::ensure!(!urls.is_empty(), "no central URL configured");
    let mut errors = Vec::new();
    for k in 0..urls.len() {
        let i = (preferred + k) % urls.len();
        let mut req = http.post(format!("{}/fleet/register", urls[i]));
        if let Some(t) = &cfg.token {
            req = req.bearer_auth(t);
        }
        match req.json(&body).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(i),
            Ok(resp) => errors.push(format!("{}: {}", urls[i], resp.status())),
            Err(e) => errors.push(format!("{}: {e}", urls[i])),
        }
    }
    anyhow::bail!("no central took the heartbeat ({})", errors.join("; "))
}

/// Writes this Mac's `overlay.json` (and one key file per network, mode 0600, next to it under `overlay-keys/`) from
/// the fleet's allocation. Returns whether anything changed. Key files of networks this Mac left are removed.
pub async fn sync_overlays(
    http: &reqwest::Client,
    central: &str,
    token: Option<&str>,
    node: &str,
    path: &std::path::Path,
) -> Result<bool> {
    let mut req = http.get(format!("{central}/fleet/nodes/{node}/overlays"));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.context("fetching overlays")?;
    anyhow::ensure!(
        resp.status().is_success(),
        "central refused the overlay list: {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.context("parsing overlays")?;
    // File writes and peer name lookups block; keep them off the runtime's workers.
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || apply_overlays(&body, &path))
        .await
        .context("overlay sync task")?
}

/// A peer as the switch takes it: `IP:port`. Central gives a host name when a node advertises one; it is looked up
/// here (IPv4 first, since the switch listens on `0.0.0.0`). `None` when it cannot be resolved.
fn resolve_peer(p: &str) -> Option<String> {
    use std::net::{SocketAddr, ToSocketAddrs};
    if p.parse::<SocketAddr>().is_ok() {
        return Some(p.to_owned());
    }
    let addrs: Vec<SocketAddr> = p.to_socket_addrs().ok()?.collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .map(|a| a.to_string())
}

/// The file-writing half of [`sync_overlays`], separate so it can be tested without a central.
pub fn apply_overlays(body: &serde_json::Value, path: &std::path::Path) -> Result<bool> {
    use fluxvm_apple::vznet::{NetworkOverlay, OverlayFile};
    use std::os::unix::fs::PermissionsExt;
    let dir = path
        .parent()
        .context("overlay config path has no directory")?;
    let keys = dir.join("overlay-keys");
    std::fs::create_dir_all(&keys)?;
    std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700))?;
    let mut changed = false;
    let mut file = OverlayFile::default();
    let empty = serde_json::Map::new();
    let networks = body["networks"].as_object().unwrap_or(&empty);
    for (name, n) in networks {
        anyhow::ensure!(
            fluxvm_apple::vznet::valid_name(name),
            "central sent a bad network name {name:?}"
        );
        let key = n["key"].as_str().context("overlay without a key")?;
        anyhow::ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "central sent a malformed key for {name}"
        );
        let hex64 = |k: &str| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit());
        // `<epoch> <key>` lines, the current first, then the previous one (still accepted while Macs roll over).
        let mut content = match n["epoch"].as_u64() {
            Some(e) => format!("{e} {key}\n"),
            None => format!("{key}\n"),
        };
        if let (Some(e), Some(k)) = (
            n["previous"]["epoch"].as_u64(),
            n["previous"]["key"].as_str(),
        ) && n["epoch"].as_u64().is_some()
        {
            anyhow::ensure!(hex64(k), "central sent a malformed previous key for {name}");
            content.push_str(&format!("{e} {k}\n"));
        }
        let key_file = keys.join(format!("{name}.key"));
        if std::fs::read_to_string(&key_file).ok().as_deref() != Some(content.as_str()) {
            let tmp = key_file.with_extension("key.tmp");
            std::fs::write(&tmp, &content)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp, &key_file)?;
            changed = true;
        }
        let hosts = n["hosts"].as_array().context("overlay without hosts")?;
        let h = |i: usize| -> Result<u8> {
            hosts
                .get(i)
                .and_then(|v| v.as_u64())
                .and_then(|v| u8::try_from(v).ok())
                .context("bad host slice")
        };
        file.networks.insert(
            name.clone(),
            NetworkOverlay {
                subnet: n["subnet"]
                    .as_u64()
                    .and_then(|v| u8::try_from(v).ok())
                    .context("bad subnet")?,
                hosts: [h(0)?, h(1)?],
                listen: n["listen"]
                    .as_str()
                    .context("no listen address")?
                    .to_owned(),
                peers: n["peers"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| {
                                let p = p.as_str()?;
                                let r = resolve_peer(p);
                                if r.is_none() {
                                    tracing::warn!(network = %name, peer = p, "overlay peer does not resolve; left out");
                                }
                                r
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                key_file,
                mtu: n["mtu"].as_u64().and_then(|v| u32::try_from(v).ok()),
            },
        );
    }
    // Forget keys of networks this Mac no longer belongs to.
    for entry in std::fs::read_dir(&keys)? {
        let entry = entry?;
        let stem = entry
            .path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned());
        if let Some(stem) = stem
            && !file.networks.contains_key(&stem)
        {
            std::fs::remove_file(entry.path())?;
            changed = true;
        }
    }
    let json = serde_json::to_string_pretty(&file)?;
    if std::fs::read_to_string(path).ok().as_deref() != Some(json.as_str()) {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod overlay_sync_tests {
    use super::apply_overlays;
    use serde_json::json;

    #[test]
    fn overlays_are_written_with_private_keys_and_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("overlay.json");
        let key = "ab".repeat(32);
        let body = json!({"networks": {"acme-net": {
            "subnet": 128, "hosts": [2, 31], "listen": "0.0.0.0:47100",
            "peers": ["10.0.0.2:47100"], "key": key, "mtu": 1400}}});
        assert!(apply_overlays(&body, &path).unwrap());
        assert!(
            !apply_overlays(&body, &path).unwrap(),
            "nothing changed the second time"
        );
        let file: fluxvm_apple::vznet::OverlayFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let n = &file.networks["acme-net"];
        assert_eq!(n.hosts, [2, 31]);
        assert_eq!(
            std::fs::read_to_string(&n.key_file).unwrap(),
            format!("{key}\n")
        );
        // With epochs (rotation), both keys are written, the current first.
        let old = "cd".repeat(32);
        let rotated = json!({"networks": {"acme-net": {
            "subnet": 128, "hosts": [2, 31], "listen": "0.0.0.0:47100", "peers": ["10.0.0.2:47100"],
            "key": key, "epoch": 2, "previous": {"epoch": 1, "key": old}, "mtu": 1400}}});
        assert!(apply_overlays(&rotated, &path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&n.key_file).unwrap(),
            format!("2 {key}\n1 {old}\n")
        );
        let mode = std::fs::metadata(&n.key_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // The Mac leaves the network: its key goes too.
        assert!(apply_overlays(&json!({"networks": {}}), &path).unwrap());
        assert!(!n.key_file.exists());
        // Hostile input is refused.
        let bad = json!({"networks": {"../x": {"key": key}}});
        assert!(apply_overlays(&bad, &path).is_err());
    }
}

#[cfg(test)]
mod central_list_tests {
    use super::central_urls;

    #[test]
    fn retry_waits_longer_after_failures_with_jitter_and_a_ceiling() {
        let i = std::time::Duration::from_secs(10);
        assert_eq!(super::retry_delay(i, 0, 0.9), i);
        // One failure: 2x, scaled into 50-100%.
        assert_eq!(
            super::retry_delay(i, 1, 0.0),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            super::retry_delay(i, 1, 1.0),
            std::time::Duration::from_secs(20)
        );
        // The ceiling is 8x the interval however long the outage.
        assert_eq!(
            super::retry_delay(i, 50, 1.0),
            std::time::Duration::from_secs(80)
        );
        assert!(super::retry_delay(i, 50, 0.0) >= std::time::Duration::from_secs(40));
        // Out-of-range jitter is clamped, never negative or above the ceiling.
        assert_eq!(
            super::retry_delay(i, 2, 7.0),
            std::time::Duration::from_secs(40)
        );
        assert_eq!(
            super::retry_delay(i, 2, -3.0),
            std::time::Duration::from_secs(20)
        );
    }

    #[test]
    fn allocation_is_summed_from_active_vm_records() {
        let vm = |status: &str, cpus: Option<u64>, mem: Option<u64>| {
            let mut req = serde_json::json!({});
            if let Some(c) = cpus {
                req["vcpus"] = c.into();
            }
            if let Some(m) = mem {
                req["memory_mib"] = m.into();
            }
            serde_json::json!({"status": status, "request": req})
        };
        assert!(super::is_active(&vm("running", None, None)));
        assert!(!super::is_active(&vm("stopped", Some(8), Some(8192))));
        assert_eq!(
            super::vm_size(&vm("running", Some(8), Some(16384))),
            (8, 16384)
        );
        // No recorded size: the assumed one, never zero.
        assert_eq!(
            super::vm_size(&vm("running", None, None)),
            (super::ASSUMED_VM_VCPUS, super::ASSUMED_VM_MEMORY_MIB)
        );
        assert_eq!(
            super::vm_size(&vm("running", Some(0), Some(0))),
            (super::ASSUMED_VM_VCPUS, super::ASSUMED_VM_MEMORY_MIB)
        );
    }

    #[test]
    fn central_lists_are_split_and_trimmed() {
        assert_eq!(central_urls("http://a:7799"), ["http://a:7799"]);
        assert_eq!(
            central_urls(" http://a:7799/, http://b:7799 ,,"),
            ["http://a:7799", "http://b:7799"]
        );
        assert!(central_urls(" , ").is_empty());
    }
}

async fn fetch_node_security(http: &reqwest::Client, fluxvm_url: &str) -> NodeSecurityCapabilities {
    match http
        .get(format!("{fluxvm_url}/v1/security/capabilities"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => match resp.json::<HostCapabilities>().await {
            Ok(caps) => NodeSecurityCapabilities::from(caps),
            Err(_) => NodeSecurityCapabilities::standard_only(),
        },
        _ => NodeSecurityCapabilities::standard_only(),
    }
}

/// Memory pressure, warm slots and sandbox counts for fleet placement; `None` when the daemon does not serve them.
async fn fetch_density(
    http: &reqwest::Client,
    fluxvm_url: &str,
) -> Option<fluxvm_core::agent_density::DensityReport> {
    let resp = http
        .get(format!("{fluxvm_url}/v1/sandboxes/density"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

#[derive(Debug, Clone, Copy, Default)]
struct LocalVmStats {
    count: usize,
    macos_guests: u32,
    /// CPU and memory the active VMs (creating, running, paused) request, summed from their records.
    allocated_vcpus: u32,
    allocated_memory_mib: u64,
}

/// What an active VM without a recorded size is assumed to hold.
const ASSUMED_VM_VCPUS: u32 = 2;
const ASSUMED_VM_MEMORY_MIB: u64 = 2048;

fn is_active(vm: &serde_json::Value) -> bool {
    vm.get("status")
        .or_else(|| vm.get("state"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| matches!(s, "creating" | "running" | "paused"))
}

/// The CPU and memory one VM record requests, or the assumed size when it records none.
fn vm_size(vm: &serde_json::Value) -> (u32, u64) {
    let request = vm.get("request").unwrap_or(vm);
    (
        request
            .get("vcpus")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .filter(|n| *n > 0)
            .unwrap_or(ASSUMED_VM_VCPUS),
        request
            .get("memory_mib")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .unwrap_or(ASSUMED_VM_MEMORY_MIB),
    )
}

async fn local_vm_stats(http: &reqwest::Client, fluxvm_url: &str) -> Result<LocalVmStats> {
    let resp = http
        .get(format!("{fluxvm_url}/v1/vms"))
        .send()
        .await
        .context("GET /v1/vms")?;
    // An error body would parse as "no VMs", which is exactly the wrong thing to report.
    anyhow::ensure!(
        resp.status().is_success(),
        "the local daemon answered GET /v1/vms with {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.context("parsing /v1/vms response")?;
    let items = body
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let macos_guests = count_active_macos_guests(&items);
    let (mut vcpus, mut memory) = (0u32, 0u64);
    for vm in items.iter().filter(|v| is_active(v)) {
        let (c, m) = vm_size(vm);
        vcpus = vcpus.saturating_add(c);
        memory = memory.saturating_add(m);
    }
    Ok(LocalVmStats {
        count: items.len(),
        macos_guests,
        allocated_vcpus: vcpus,
        allocated_memory_mib: memory,
    })
}

/// macOS guests that hold one of Apple's two slots: not stopped or failed. `GET /v1/vms` items are `VmRecord`s, which
/// carry `status` (`creating`, `running`, `paused`, `stopped`, `failed`); `state` is only what the simulator adds.
fn count_active_macos_guests(items: &[serde_json::Value]) -> u32 {
    items
        .iter()
        .filter(|vm| {
            let status = vm
                .get("status")
                .or_else(|| vm.get("state"))
                .and_then(|v| v.as_str());
            let active = status.is_some_and(|s| matches!(s, "creating" | "running" | "paused"));
            let request = vm.get("request").unwrap_or(vm);
            let mac = request
                .get("apple")
                .and_then(|a| a.get("guest_os"))
                .and_then(|v| v.as_str())
                == Some("macos");
            active && mac
        })
        .count() as u32
}

fn sim_apple_caps(h: &SimHost, local: LocalVmStats) -> AppleHostCaps {
    AppleHostCaps {
        free_cpu: h.vcpus.saturating_sub(local.allocated_vcpus),
        free_memory_mib: h.memory_mib.saturating_sub(local.allocated_memory_mib),
        nested_virtualization: h.nested_virtualization,
        vmnet: h.vmnet,
        custom_virtio: true,
        bridged_interfaces: vec!["en0".into()],
        macos_guests: local.macos_guests,
        secure_boot: true,
        rosetta: true,
    }
}

#[cfg(target_os = "macos")]
async fn fetch_apple_caps(
    vcpus_total: u32,
    memory_mib_total: u64,
    local: LocalVmStats,
) -> Option<AppleHostCaps> {
    let caps = tokio::task::spawn_blocking(fluxvm_apple::host_capabilities)
        .await
        .ok()?
        .ok()?;
    Some(AppleHostCaps {
        free_cpu: vcpus_total.saturating_sub(local.allocated_vcpus),
        free_memory_mib: memory_mib_total.saturating_sub(local.allocated_memory_mib),
        nested_virtualization: caps.nested_virtualization,
        vmnet: caps.vmnet_custom_networks && caps.vmnet_serialization,
        custom_virtio: caps.custom_virtio_queue_backend && caps.guest_memory_mapping,
        bridged_interfaces: caps.bridged_interfaces,
        macos_guests: local.macos_guests,
        secure_boot: caps.efi_secure_boot,
        rosetta: caps.rosetta == "installed",
    })
}

#[cfg(not(target_os = "macos"))]
async fn fetch_apple_caps(
    _vcpus_total: u32,
    _memory_mib_total: u64,
    _local: LocalVmStats,
) -> Option<AppleHostCaps> {
    None
}

/// Real total RAM off `/proc/meminfo` (`MemTotal:` is always the first
/// line) — this project is Linux-only throughout, same as every other
/// host-introspection call in it (cgroups, `/proc/uptime`, ...).
#[cfg(target_os = "linux")]
fn total_memory_mib() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = content.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

#[cfg(target_os = "macos")]
fn total_memory_mib() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let name = b"hw.memsize\0";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast(),
            (&mut value as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value / 1_048_576)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn total_memory_mib() -> Option<u64> {
    None
}

#[cfg(test)]
mod macos_guest_count_tests {
    use super::count_active_macos_guests;
    use serde_json::json;

    fn vm(status_key: &str, status: &str, guest: &str) -> serde_json::Value {
        json!({status_key: status, "request": {"apple": {"guest_os": guest}}})
    }

    #[test]
    fn a_real_daemon_record_with_only_status_is_counted() {
        // `GET /v1/vms` items are VmRecords: `status`, no `state`.
        let items = [
            vm("status", "running", "macos"),
            vm("status", "creating", "macos"),
            vm("status", "paused", "macos"),
        ];
        assert_eq!(count_active_macos_guests(&items), 3);
    }

    #[test]
    fn stopped_failed_and_linux_guests_hold_no_slot() {
        let items = [
            vm("status", "stopped", "macos"),
            vm("status", "failed", "macos"),
            vm("status", "running", "linux"),
            json!({"status": "running", "request": {}}),
            vm("status", "running", "macos"),
        ];
        assert_eq!(count_active_macos_guests(&items), 1);
    }

    #[test]
    fn the_simulators_state_key_still_works() {
        assert_eq!(
            count_active_macos_guests(&[vm("state", "running", "macos")]),
            1
        );
    }
}
