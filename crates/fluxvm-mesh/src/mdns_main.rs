// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::mdns::{OVERLAY_MDNS_PORT, run_bridge_dual};
use fluxvm_mesh::{StateStore, default_state_path};
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("fluxmesh-mdns: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut bind: SocketAddr = "0.0.0.0:53530".parse()?;
    let mut state_path: Option<PathBuf> = None;
    let mut peers = Vec::new();
    let mut ipv6_ifindex: Option<u32> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => {
                bind = args.get(i + 1).ok_or("missing --bind value")?.parse()?;
                i += 2;
            }
            "--state" => {
                state_path = Some(PathBuf::from(
                    args.get(i + 1).ok_or("missing --state value")?,
                ));
                i += 2;
            }
            "--ipv6-ifindex" => {
                ipv6_ifindex = Some(
                    args.get(i + 1)
                        .ok_or("missing --ipv6-ifindex value")?
                        .parse()?,
                );
                i += 2;
            }
            value => {
                peers.push(value.parse::<SocketAddr>()?);
                i += 1;
            }
        }
    }
    if let Some(path) =
        state_path.or_else(|| default_state_path().exists().then(default_state_path))
    {
        let mesh = StateStore::new(path).load()?;
        for (id, node) in &mesh.nodes {
            if id != &mesh.local_node {
                peers.push(SocketAddr::new(node.overlay_ip.into(), OVERLAY_MDNS_PORT));
            }
        }
    }
    peers.sort();
    peers.dedup();
    if peers.is_empty() {
        return Err("no mDNS mesh peers; pass --state PATH or PEER:53530".into());
    }
    run_bridge_dual(peers, bind, ipv6_ifindex).await?;
    Ok(())
}
