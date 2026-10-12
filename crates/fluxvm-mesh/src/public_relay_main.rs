// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::public_relay::{RelayClientConfig, run_client, serve};
use std::path::PathBuf;
#[tokio::main]
async fn main() {
    if let Err(e) = real().await {
        eprintln!("fluxmesh-public-relay: {e}");
        std::process::exit(1)
    }
}
async fn real() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    match a.next().as_deref() {
        Some("server") => {
            let p = PathBuf::from(a.next().ok_or("missing relay server config path")?);
            serve(&p).await?;
        }
        Some("client") => {
            let p = PathBuf::from(a.next().ok_or("missing relay client config path")?);
            let raw = std::fs::read(p)?;
            let c: RelayClientConfig = serde_json::from_slice(&raw)?;
            run_client(c).await?;
        }
        _ => {
            return Err(
                "usage: fluxmesh-public-relay server CONFIG.json | client CONFIG.json".into(),
            );
        }
    }
    Ok(())
}
