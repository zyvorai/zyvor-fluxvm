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

use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "usage: fluxvm-vz-switch --socket PATH [--idle-exit-secs N] [--netobs-socket PATH [--network NAME]]";

fn main() {
    let mut socket: Option<PathBuf> = None;
    let mut idle = Duration::from_secs(30);
    let mut netobs_socket: Option<PathBuf> = None;
    let mut network: Option<String> = None;
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
            _ => usage(),
        }
    }
    let Some(socket) = socket else { usage() };
    if let Some(path) = netobs_socket {
        let network = network.unwrap_or_else(|| {
            socket
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "private".to_string())
        });
        start_export(&path, &network);
    }
    if let Err(e) = fluxvm_vz_switch::serve(&socket, idle) {
        eprintln!("fluxvm-vz-switch: {e:#}");
        std::process::exit(1);
    }
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
