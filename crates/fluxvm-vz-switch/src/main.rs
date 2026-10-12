// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxvm-vz-switch --socket PATH [--idle-exit-secs N] [--netobs-socket PATH [--network NAME]]`: serves one
//! private network of `vz` guests (see the library).
//!
//! With `--netobs-socket` (requires the `netobs` feature) every frame a guest sends is also offered to a
//! `fluxvm-netobs` bridge server listening on that unix socket, with the verdict the switch reached. The export
//! never blocks the switch: the connection is made and re-made in the background, and frames that cannot be
//! delivered are dropped and counted. `--network` names the network in the observations (default: the stem of
//! `--socket`).
//!
//! With `--overlay-listen ADDR --overlay-peer ADDR… --overlay-key-file PATH` the network also spans the switches of
//! the same network on other Macs (see the library's `overlay` module): frames travel over UDP, sealed with
//! AES-256-GCM under the 32-byte key in the file (64 hex characters).

use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "usage: fluxvm-vz-switch --socket PATH [--idle-exit-secs N] [--netobs-socket PATH [--network NAME]] \
[--overlay-listen ADDR --overlay-peer ADDR... --overlay-key-file PATH [--overlay-config PATH]]";

fn main() {
    let mut socket: Option<PathBuf> = None;
    let mut idle = Duration::from_secs(30);
    let mut netobs_socket: Option<PathBuf> = None;
    let mut network: Option<String> = None;
    let mut overlay_listen: Option<std::net::SocketAddr> = None;
    let mut overlay_peers: Vec<std::net::SocketAddr> = Vec::new();
    let mut overlay_key: Option<PathBuf> = None;
    let mut overlay_config: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--idle-exit-secs" => {
                idle = Duration::from_secs(args.next().and_then(|v| v.parse().ok()).unwrap_or(30));
            }
            "--netobs-socket" => match args.next() {
                Some(p) => netobs_socket = Some(PathBuf::from(p)),
                None => usage(),
            },
            "--network" => match args.next() {
                Some(n) => network = Some(n),
                None => usage(),
            },
            "--overlay-listen" => match args.next().and_then(|v| v.parse().ok()) {
                Some(a) => overlay_listen = Some(a),
                None => usage(),
            },
            "--overlay-peer" => match args.next().and_then(|v| v.parse().ok()) {
                Some(a) => overlay_peers.push(a),
                None => usage(),
            },
            "--overlay-config" => match args.next() {
                Some(p) => overlay_config = Some(PathBuf::from(p)),
                None => usage(),
            },
            "--overlay-key-file" => match args.next() {
                Some(p) => overlay_key = Some(PathBuf::from(p)),
                None => usage(),
            },
            _ => usage(),
        }
    }
    let Some(socket) = socket else { usage() };
    let network = network.unwrap_or_else(|| {
        socket
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "private".to_string())
    });
    if let Some(path) = netobs_socket {
        start_export(&path, &network);
    }
    let overlay = match (overlay_listen, overlay_key) {
        (None, None) if overlay_peers.is_empty() => None,
        (Some(listen), Some(key_file)) => {
            let ov = std::sync::Arc::new(open_overlay(&network, listen, overlay_peers, &key_file));
            watch_overlay(
                overlay_config,
                key_file.clone(),
                network.clone(),
                ov.clone(),
            );
            Some(ov)
        }
        _ => {
            eprintln!(
                "fluxvm-vz-switch: --overlay-listen and --overlay-key-file go together (and peers need them)"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = fluxvm_vz_switch::serve_with_overlay(&socket, idle, overlay) {
        eprintln!("fluxvm-vz-switch: {e:#}");
        std::process::exit(1);
    }
}

/// The ciphers for a key file's keys, the current first.
fn load_keys(
    key_file: &std::path::Path,
) -> Option<Vec<(u32, Box<dyn fluxvm_vz_switch::overlay::FrameCipher>)>> {
    ciphers(&std::fs::read_to_string(key_file).ok()?)
}

/// The ciphers for a key file's text, the current first.
fn ciphers(text: &str) -> Option<Vec<(u32, Box<dyn fluxvm_vz_switch::overlay::FrameCipher>)>> {
    use fluxvm_vz_switch::overlay::{AesGcm, FrameCipher, parse_keys};
    let keys = parse_keys(text)?;
    Some(
        keys.into_iter()
            .map(|(e, k)| (e, Box::new(AesGcm::new(&k)) as Box<dyn FrameCipher>))
            .collect(),
    )
}

/// Every 10 s: re-reads the key file (a rotation takes effect without a restart) and, with `--overlay-config`, this
/// network's peers from the Mac's `overlay.json` (Macs joining or leaving take effect without restarting the switch
/// and its guests' links).
fn watch_overlay(
    config: Option<PathBuf>,
    key_file: PathBuf,
    network: String,
    ov: std::sync::Arc<fluxvm_vz_switch::overlay::Overlay>,
) {
    std::thread::spawn(move || {
        // Compare the file's text, not just the epochs: a bare key file replaced by hand keeps epoch 0. The first pass
        // reloads unconditionally (same keys, windows kept), so a change right after startup is not missed.
        let mut loaded: Option<String> = None;
        loop {
            std::thread::sleep(Duration::from_secs(10));
            if let Ok(text) = std::fs::read_to_string(&key_file)
                && loaded.as_deref() != Some(text.as_str())
                && let Some(keys) = ciphers(&text)
            {
                let epochs: Vec<u32> = keys.iter().map(|(e, _)| *e).collect();
                if loaded.is_some() || epochs != ov.epochs() {
                    eprintln!("fluxvm-vz-switch: {network} keys reloaded, epochs {epochs:?}");
                }
                ov.set_keys(keys);
                loaded = Some(text);
            }
            let Some(path) = &config else {
                continue;
            };
            let Ok(raw) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(list) = v["networks"][network.as_str()]["peers"].as_array() else {
                // The network left this Mac's file: keep running for the guests still attached, with no peers.
                if ov.peer_count() > 0 {
                    eprintln!("fluxvm-vz-switch: {network} no longer has peers");
                    ov.set_peers(Vec::new());
                }
                continue;
            };
            let peers: Vec<std::net::SocketAddr> = list
                .iter()
                .filter_map(|p| p.as_str()?.parse().ok())
                .collect();
            if peers != ov.peers() {
                eprintln!("fluxvm-vz-switch: {network} peers now {peers:?}");
                ov.set_peers(peers);
            }
        }
    });
}

fn open_overlay(
    network: &str,
    listen: std::net::SocketAddr,
    peers: Vec<std::net::SocketAddr>,
    key_file: &std::path::Path,
) -> fluxvm_vz_switch::overlay::Overlay {
    use fluxvm_vz_switch::overlay::{Overlay, OverlayConfig};
    fn fail(msg: String) -> ! {
        eprintln!("fluxvm-vz-switch: {msg}");
        std::process::exit(1);
    }
    let keys = load_keys(key_file).unwrap_or_else(|| {
        fail(format!(
            "{} must hold a key (64 hex characters) or lines of `<epoch> <key>`",
            key_file.display()
        ))
    });
    let cfg = OverlayConfig {
        network: network.to_string(),
        listen,
        peers,
    };
    Overlay::bind_keyed(cfg, keys)
        .unwrap_or_else(|e| fail(format!("binding the overlay on {listen}: {e}")))
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

#[cfg(feature = "netobs")]
fn start_export(path: &std::path::Path, network: &str) {
    fluxvm_vz_switch::connect_bridge(path, network);
}

#[cfg(not(feature = "netobs"))]
fn start_export(_path: &std::path::Path, _network: &str) {
    eprintln!("fluxvm-vz-switch: --netobs-socket needs a build with the `netobs` feature");
    std::process::exit(2);
}
