// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxvm-usernet --fd N [--config cfg.json]`: serve the userspace gateway on an inherited `SOCK_DGRAM` socket
//! (the end of the NIC socketpair the VM does not hold). Flow events are written to stdout as JSON lines.
//! `--watch-pid P` makes it exit once process P is gone (the runner whose NIC it serves).
//! `--tap-fd M` additionally copies every frame to that `SOCK_DGRAM` socket (see `UserNet::set_tap`); with
//! `--tap-version 2` each datagram is `[dir][verdict][frame]` (verdict 0 pass, 1 denied by policy, 2 dropped),
//! the default version 1 is `[dir][frame]`.

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
    let mut tap_fd: Option<i32> = None;
    let mut watch_pid: Option<i32> = None;
    let mut tap_version: u8 = fluxvm_usernet::TAP_VERSION_1;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fd" => fd = Some(args.next().ok_or("--fd needs a value")?.parse()?),
            "--tap-fd" => tap_fd = Some(args.next().ok_or("--tap-fd needs a value")?.parse()?),
            "--tap-version" => tap_version = args.next().ok_or("--tap-version needs a value")?.parse()?,
            "--watch-pid" => watch_pid = Some(args.next().ok_or("--watch-pid needs a value")?.parse()?),
            "--config" => cfg_path = Some(args.next().ok_or("--config needs a value")?),
            _ => return Err(format!("usage: fluxvm-usernet --fd N [--tap-fd M [--tap-version 1|2]] [--watch-pid P] [--config cfg.json] (got {a:?})").into()),
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
        let mut net = UserNet::from_fd(cfg, owned)?;
        if let Some(t) = tap_fd {
            // SAFETY: the caller hands this process ownership of the descriptor.
            let t = unsafe { OwnedFd::from_raw_fd(t) };
            net.set_tap(t.into())?;
            net.set_tap_version(tap_version)?;
        }
        let mut log = net.handle().subscribe();
        tokio::spawn(async move {
            while let Ok(e) = log.recv().await {
                if let Ok(s) = serde_json::to_string(&e) {
                    println!("{s}");
                }
            }
        });
        if let Some(pid) = watch_pid {
            let h = net.handle();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    // SAFETY: signal 0 only probes for existence.
                    let gone = unsafe { libc::kill(pid, 0) } != 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                    if gone {
                        h.shutdown();
                        break;
                    }
                }
            });
        }
        net.run().await?;
        Ok(())
    })
}
