// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::pd::{acquire, route_plan};
use std::net::SocketAddr;
use std::time::Duration;

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("fluxmesh-pd: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 2 || args.len() > 4 {
        return Err("usage: fluxmesh-pd BIND_ADDR SERVER_ADDR [SCOPE_ID] [INTERFACE]".into());
    }
    let bind: SocketAddr = args[0].parse()?;
    let server: SocketAddr = args[1].parse()?;
    let scope = args
        .get(2)
        .map(|v| v.parse::<u32>())
        .transpose()?
        .unwrap_or(0);
    let interface = args.get(3).map(String::as_str).unwrap_or("fluxmesh0");
    let lease = acquire(bind, server, scope, Duration::from_secs(5)).await?;
    println!(
        "prefix={} preferred={} valid={}",
        lease.cidr(),
        lease.preferred_lifetime,
        lease.valid_lifetime
    );
    for command in route_plan(&lease, interface)? {
        println!("{}", command.join(" "));
    }
    Ok(())
}
