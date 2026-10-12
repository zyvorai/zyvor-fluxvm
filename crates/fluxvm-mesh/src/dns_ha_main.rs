// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use fluxvm_mesh::catalog::{CatalogStore, default_catalog_path};
use fluxvm_mesh::dns::{PrivateDns, serve_udp};
use fluxvm_mesh::dns_ha::elect;
use fluxvm_mesh::{StateStore, default_state_path};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fluxmesh-dns-ha: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || args.len() > 4 {
        eprintln!("usage: fluxmesh-dns-ha BIND_ADDR [REPLICAS] [STATE_PATH] [CATALOG_PATH]");
        std::process::exit(2);
    }
    let bind: SocketAddr = args[0].parse()?;
    let replicas = args
        .get(1)
        .map(|v| v.parse::<usize>())
        .transpose()?
        .unwrap_or(2);
    let state = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(default_state_path);
    let catalog = args
        .get(3)
        .map(PathBuf::from)
        .unwrap_or_else(default_catalog_path);
    let mesh = StateStore::new(state).load()?;
    let placement = elect(&mesh, replicas, 1)?;
    if !placement
        .replicas
        .iter()
        .any(|node| node == &mesh.local_node)
    {
        println!(
            "standby local={} replicas={}",
            mesh.local_node,
            placement.replicas.join(",")
        );
        return Ok(());
    }
    let catalog = CatalogStore::new(catalog).load(&mesh)?;
    let dns = PrivateDns::build(&mesh, &catalog.services, &catalog.dns_suffix)?;
    eprintln!(
        "fluxmesh-dns-ha: serving {} primary={} replicas={}",
        bind,
        placement.primary,
        placement.replicas.join(",")
    );
    serve_udp(bind, dns)?;
    Ok(())
}
