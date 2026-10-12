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
[--overlay-listen ADDR --overlay-peer ADDR... --overlay-key-file PATH]";

fn main() {
    let mut socket: Option<PathBuf> = None;
    let mut idle = Duration::from_secs(30);
    let mut netobs_socket: Option<PathBuf> = None;
    let mut network: Option<String> = None;
    let mut overlay_listen: Option<std::net::SocketAddr> = None;
    let mut overlay_peers: Vec<std::net::SocketAddr> = Vec::new();
    let mut overlay_key: Option<PathBuf> = None;
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
            Some(open_overlay(&network, listen, overlay_peers, &key_file))
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

fn open_overlay(
    network: &str,
    listen: std::net::SocketAddr,
    peers: Vec<std::net::SocketAddr>,
    key_file: &std::path::Path,
) -> fluxvm_vz_switch::overlay::Overlay {
    use fluxvm_vz_switch::overlay::{AesGcm, Overlay, OverlayConfig, parse_key};
    fn fail(msg: String) -> ! {
        eprintln!("fluxvm-vz-switch: {msg}");
        std::process::exit(1);
    }
    let text = std::fs::read_to_string(key_file)
        .unwrap_or_else(|e| fail(format!("reading {}: {e}", key_file.display())));
    let key = parse_key(&text)
        .unwrap_or_else(|| fail(format!("{} is not 64 hex characters", key_file.display())));
    let cfg = OverlayConfig {
        network: network.to_string(),
        listen,
        peers,
    };
    Overlay::bind(cfg, Box::new(AesGcm::new(&key)))
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
