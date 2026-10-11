// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxvm-usernet --fd N [--config cfg.json]`: serve the userspace gateway on an inherited `SOCK_DGRAM` socket
//! (the end of the NIC socketpair the VM does not hold). Flow events are written to stdout as JSON lines.

use fluxvm_usernet::{UserNet, UserNetConfig};
use std::os::fd::{FromRawFd, OwnedFd};

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fluxvm-usernet: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut fd: Option<i32> = None;
    let mut cfg_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fd" => fd = Some(args.next().ok_or("--fd needs a value")?.parse()?),
            "--config" => cfg_path = Some(args.next().ok_or("--config needs a value")?),
            _ => return Err(format!("usage: fluxvm-usernet --fd N [--config cfg.json] (got {a:?})").into()),
        }
    }
    let fd = fd.ok_or("--fd is required")?;
    let cfg: UserNetConfig = match cfg_path {
        Some(p) => serde_json::from_slice(&std::fs::read(p)?)?,
        None => UserNetConfig::default(),
    };
    // SAFETY: the caller hands this process ownership of the descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let net = UserNet::from_fd(cfg, owned)?;
        let mut log = net.handle().subscribe();
        tokio::spawn(async move {
            while let Ok(e) = log.recv().await {
                if let Ok(s) = serde_json::to_string(&e) {
                    println!("{s}");
                }
            }
        });
        net.run().await?;
        Ok(())
    })
}
