// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Idempotent Linux WireGuard reconciliation and key rotation.

use crate::{MeshError, MeshState, Result, StateStore, WireGuardPlan};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurrentPeer {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    pub keepalive: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileReport {
    pub commands: Vec<Vec<String>>,
    pub applied: usize,
}

pub fn reconcile_linux(plan: &WireGuardPlan, dry_run: bool) -> Result<ReconcileReport> {
    let interface_exists = command_success(&[
        "ip".into(),
        "link".into(),
        "show".into(),
        "dev".into(),
        plan.interface.clone(),
    ]);
    let current_peers = if interface_exists {
        read_current_peers(&plan.interface).unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let current_routes = if interface_exists {
        read_interface_routes(&plan.interface).unwrap_or_default()
    } else {
        BTreeSet::new()
    };
    let commands =
        build_linux_reconcile_commands(plan, interface_exists, &current_peers, &current_routes);
    if dry_run {
        return Ok(ReconcileReport {
            commands,
            applied: 0,
        });
    }
    let mut applied = 0;
    for command in &commands {
        run_argv(command)?;
        applied += 1;
    }
    Ok(ReconcileReport { commands, applied })
}

pub fn build_linux_reconcile_commands(
    plan: &WireGuardPlan,
    interface_exists: bool,
    current_peers: &BTreeMap<String, CurrentPeer>,
    current_routes: &BTreeSet<String>,
) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    if !interface_exists {
        commands.push(vec![
            "ip".into(),
            "link".into(),
            "add".into(),
            "dev".into(),
            plan.interface.clone(),
            "type".into(),
            "wireguard".into(),
        ]);
    }
    commands.push(vec![
        "ip".into(),
        "address".into(),
        "replace".into(),
        format!("{}/32", plan.local_address),
        "dev".into(),
        plan.interface.clone(),
    ]);
    if let Some(ipv6) = plan.local_ipv6 {
        commands.push(vec![
            "ip".into(),
            "-6".into(),
            "address".into(),
            "replace".into(),
            format!("{ipv6}/128"),
            "dev".into(),
            plan.interface.clone(),
        ]);
    }
    commands.push(vec![
        "wg".into(),
        "set".into(),
        plan.interface.clone(),
        "listen-port".into(),
        plan.listen_port.to_string(),
        "private-key".into(),
        plan.private_key_file.display().to_string(),
    ]);

    let desired_keys = plan
        .peers
        .iter()
        .map(|peer| peer.public_key.clone())
        .collect::<BTreeSet<_>>();
    for stale in current_peers
        .keys()
        .filter(|key| !desired_keys.contains(*key))
    {
        commands.push(vec![
            "wg".into(),
            "set".into(),
            plan.interface.clone(),
            "peer".into(),
            stale.clone(),
            "remove".into(),
        ]);
    }

    for peer in &plan.peers {
        let current = current_peers.get(&peer.public_key);
        let desired_endpoint = peer.endpoint.map(|v| v.to_string());
        // `wg set` has no explicit "unset endpoint" verb. Recreate a peer if
        // a direct endpoint disappeared (for example after switching to a
        // separate relay bridge) so stale endpoint state cannot survive.
        if desired_endpoint.is_none() && current.and_then(|p| p.endpoint.as_ref()).is_some() {
            commands.push(vec![
                "wg".into(),
                "set".into(),
                plan.interface.clone(),
                "peer".into(),
                peer.public_key.clone(),
                "remove".into(),
            ]);
        }
        let mut cmd = vec![
            "wg".into(),
            "set".into(),
            plan.interface.clone(),
            "peer".into(),
            peer.public_key.clone(),
            "allowed-ips".into(),
            peer.allowed_ips.join(","),
        ];
        if let Some(endpoint) = desired_endpoint {
            cmd.extend(["endpoint".into(), endpoint]);
        }
        cmd.extend([
            "persistent-keepalive".into(),
            peer.persistent_keepalive.unwrap_or(0).to_string(),
        ]);
        commands.push(cmd);
    }

    commands.push(vec![
        "ip".into(),
        "link".into(),
        "set".into(),
        "up".into(),
        "dev".into(),
        plan.interface.clone(),
    ]);

    let desired_routes = plan
        .peers
        .iter()
        .flat_map(|peer| peer.allowed_ips.iter().cloned())
        .collect::<BTreeSet<_>>();
    for stale in current_routes.difference(&desired_routes) {
        commands.push(route_argv("del", stale, &plan.interface));
    }
    for prefix in &desired_routes {
        commands.push(route_argv("replace", prefix, &plan.interface));
    }
    commands
}

pub fn read_current_peers(interface: &str) -> io::Result<BTreeMap<String, CurrentPeer>> {
    let output = Command::new("wg")
        .args(["show", interface, "dump"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "wg dump is not UTF-8"))?;
    let mut out = BTreeMap::new();
    // First line is interface state. Remaining fields are documented by wg(8):
    // public-key, preshared-key, endpoint, allowed-ips, handshake, rx, tx, keepalive.
    for line in text.lines().skip(1) {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 8 || fields[0].is_empty() {
            continue;
        }
        let endpoint = match fields[2] {
            "(none)" | "" => None,
            value => Some(value.to_owned()),
        };
        let allowed_ips = if fields[3].is_empty() || fields[3] == "(none)" {
            Vec::new()
        } else {
            fields[3].split(',').map(str::to_owned).collect()
        };
        let keepalive = fields[7].parse::<u16>().ok().filter(|value| *value != 0);
        out.insert(
            fields[0].to_owned(),
            CurrentPeer {
                public_key: fields[0].to_owned(),
                endpoint,
                allowed_ips,
                keepalive,
            },
        );
    }
    Ok(out)
}

pub fn read_interface_routes(interface: &str) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for family in ["-4", "-6"] {
        let output = Command::new("ip")
            .args([family, "route", "show", "dev", interface])
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        out.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .filter(|prefix| *prefix != "default" && !prefix.starts_with("fe80:"))
                .map(str::to_owned),
        );
    }
    Ok(out)
}

fn route_argv(action: &str, prefix: &str, interface: &str) -> Vec<String> {
    let mut argv = vec!["ip".into()];
    if prefix.contains(':') {
        argv.push("-6".into());
    }
    argv.extend([
        "route".into(),
        action.into(),
        prefix.into(),
        "dev".into(),
        interface.into(),
    ]);
    argv
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRotationResult {
    pub rotated: bool,
    pub public_key: Option<String>,
}

pub fn key_rotation_due(path: &Path, max_age: Duration, now: SystemTime) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    now.duration_since(modified)
        .map(|age| age >= max_age)
        .unwrap_or(false)
}

pub fn wireguard_public_key(path: &Path) -> Result<String> {
    let private_key = fs::read_to_string(path)?;
    let private_key = private_key.trim();
    if private_key.is_empty() {
        return Err(MeshError::InvalidState(
            "WireGuard private key file is empty".into(),
        ));
    }
    public_key_from_private(private_key)
}

fn public_key_from_private(private_key: &str) -> Result<String> {
    let mut child = Command::new("wg")
        .arg("pubkey")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(MeshError::Io)?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| MeshError::InvalidState("wg pubkey stdin unavailable".into()))?;
        stdin
            .write_all(format!("{private_key}\n").as_bytes())
            .map_err(MeshError::Io)?;
    }
    let public = child.wait_with_output().map_err(MeshError::Io)?;
    if !public.status.success() {
        return Err(MeshError::Io(io::Error::other(
            String::from_utf8_lossy(&public.stderr).trim().to_owned(),
        )));
    }
    let public_key = String::from_utf8(public.stdout)
        .map_err(|_| MeshError::InvalidState("wg pubkey returned non-UTF8 output".into()))?
        .trim()
        .to_owned();
    if public_key.is_empty() {
        return Err(MeshError::InvalidState(
            "wg pubkey returned an empty key".into(),
        ));
    }
    Ok(public_key)
}

pub fn rotate_wireguard_key(path: &Path) -> Result<String> {
    let private = Command::new("wg")
        .arg("genkey")
        .output()
        .map_err(MeshError::Io)?;
    if !private.status.success() {
        return Err(MeshError::Io(io::Error::other(
            String::from_utf8_lossy(&private.stderr).trim().to_owned(),
        )));
    }
    let private_key = String::from_utf8(private.stdout)
        .map_err(|_| MeshError::InvalidState("wg genkey returned non-UTF8 output".into()))?;
    let private_key = private_key.trim().to_owned();
    if private_key.is_empty() {
        return Err(MeshError::InvalidState(
            "wg genkey returned an empty key".into(),
        ));
    }
    let public_key = public_key_from_private(&private_key)?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("new");
    fs::write(&tmp, format!("{private_key}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(tmp, path)?;
    Ok(public_key)
}

pub fn rotate_local_key_if_due(
    state_path: &Path,
    key_path: &Path,
    max_age: Duration,
) -> Result<KeyRotationResult> {
    if !key_rotation_due(key_path, max_age, SystemTime::now()) {
        return Ok(KeyRotationResult {
            rotated: false,
            public_key: None,
        });
    }
    let public_key = rotate_wireguard_key(key_path)?;
    let store = StateStore::new(state_path);
    let mut state = store.load()?;
    let local = state.local_node.clone();
    state.node_mut(&local)?.public_key = public_key.clone();
    store.save(&state)?;
    Ok(KeyRotationResult {
        rotated: true,
        public_key: Some(public_key),
    })
}

pub fn build_plan(
    state: &MeshState,
    interface: &str,
    listen_port: u16,
    private_key_file: impl Into<PathBuf>,
) -> Result<WireGuardPlan> {
    state.wireguard_plan(interface, listen_port, private_key_file)
}

fn command_success(argv: &[String]) -> bool {
    argv.first().is_some_and(|program| {
        Command::new(program)
            .args(&argv[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

fn run_argv(argv: &[String]) -> Result<()> {
    let Some(program) = argv.first() else {
        return Ok(());
    };
    let output = Command::new(program)
        .args(&argv[1..])
        .output()
        .map_err(MeshError::Io)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(MeshError::Io(io::Error::other(format!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn reconcile_removes_stale_peer_and_stale_route() {
        let mut mesh = MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "home",
            "home.local",
            "home-key",
        )
        .unwrap();
        mesh.add_node("office", "office.local", "office-key", false)
            .unwrap();
        let plan = mesh.wireguard_plan("fluxmesh0", 51820, "/tmp/key").unwrap();
        let current = BTreeMap::from([(
            "stale-key".into(),
            CurrentPeer {
                public_key: "stale-key".into(),
                ..CurrentPeer::default()
            },
        )]);
        let routes = BTreeSet::from(["10.9.0.0/16".into()]);
        let commands = build_linux_reconcile_commands(&plan, true, &current, &routes);
        let joined = commands
            .iter()
            .map(|v| v.join(" "))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("peer stale-key remove"));
        assert!(joined.contains("route del 10.9.0.0/16"));
        assert!(joined.contains("peer office-key"));
    }

    #[test]
    fn missing_key_is_due_for_rotation() {
        let path = std::env::temp_dir().join("fluxmesh-no-such-key-for-test");
        fs::remove_file(&path).ok();
        assert!(key_rotation_due(
            &path,
            Duration::from_secs(60),
            SystemTime::now()
        ));
    }
}
