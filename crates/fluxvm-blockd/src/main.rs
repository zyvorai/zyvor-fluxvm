// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! fluxvm-blockd serve --export name=base[,overlay=path][,ro][,iops=N][,bps=N]
//!                     [--listen 127.0.0.1:10809] [--watch-pid P | --watch-pid-file F]
//! fluxvm-blockd branch --from overlay --to new-overlay

use anyhow::{Context, Result, bail};
use fluxvm_blockd::{BlockBackend, CowBackend, Export, Limiter, RawFile, Server};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "usage:
  fluxvm-blockd serve --export name=base[,overlay=path][,ro][,iops=N][,bps=N] \\
                      [--export ...] [--listen 127.0.0.1:10809] [--watch-pid P | --watch-pid-file F]
  fluxvm-blockd branch --from <overlay> --to <new-overlay>

An overlay that does not exist is created on top of `base`. If `base` is
itself an overlay file it is used as the parent (and frozen).";

fn is_overlay(p: &Path) -> bool {
    use std::io::Read;
    let mut m = [0u8; 8];
    std::fs::File::open(p).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == b"FLXOVL01"
}

fn parse_export(spec: &str) -> Result<Export> {
    let mut parts = spec.split(',');
    let first = parts.next().unwrap_or("");
    let (name, base) = first.split_once('=').context("export must start with name=base")?;
    let base = PathBuf::from(base);
    let (mut overlay, mut ro, mut iops, mut bps) = (None, false, None, None);
    for p in parts {
        match p.split_once('=') {
            Some(("overlay", v)) => overlay = Some(PathBuf::from(v)),
            Some(("iops", v)) => iops = Some(v.parse::<u64>()?),
            Some(("bps", v)) => bps = Some(v.parse::<u64>()?),
            None if p == "ro" => ro = true,
            _ => bail!("unknown export option `{p}`"),
        }
    }
    let backend: Arc<dyn BlockBackend> = match overlay {
        Some(o) if o.exists() => CowBackend::open(&o)?,
        Some(o) if is_overlay(&base) => CowBackend::create_on_overlay(&o, &base)?,
        Some(o) => CowBackend::create(&o, &base, None)?,
        None if is_overlay(&base) => CowBackend::open(&base)?,
        None => Arc::new(RawFile::open(&base, ro)?),
    };
    let mut e = Export::new(name, backend).read_only(ro);
    if iops.is_some() || bps.is_some() {
        e = e.limiter(Limiter::new(bps, iops));
    }
    Ok(e)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => {
            let mut listen = "127.0.0.1:10809".to_string();
            let mut exports = Vec::new();
            let mut watch_pid: Option<i32> = None;
            let mut watch_file: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--export" => exports.push(parse_export(&args.next().context("--export value")?)?),
                    "--listen" => listen = args.next().context("--listen value")?,
                    "--watch-pid-file" => watch_file = Some(PathBuf::from(args.next().context("--watch-pid-file value")?)),
                    "--watch-pid" => watch_pid = Some(args.next().context("--watch-pid value")?.parse::<i32>()?),
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            if exports.is_empty() {
                bail!("at least one --export is required\n{USAGE}");
            }
            let listener = tokio::net::TcpListener::bind(&listen).await?;
            eprintln!("fluxvm-blockd: listening on {}", listener.local_addr()?);
            let server = Server::new(exports);
            tokio::select! {
                r = server.serve(listener) => r?,
                _ = tokio::signal::ctrl_c() => {}
                // Exit once the process this serves (the VM runner) is gone. With a pid file, the pid may appear
                // late (the runner starts after this server); a file that never appears ends the server after 120 s.
                _ = async {
                    if watch_pid.is_none() && watch_file.is_none() {
                        return std::future::pending::<()>().await;
                    }
                    let started = std::time::Instant::now();
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        let pid = watch_pid.or_else(|| {
                            std::fs::read_to_string(watch_file.as_ref()?).ok()?.trim().parse::<i32>().ok()
                        });
                        match pid {
                            Some(pid) => {
                                // SAFETY: signal 0 only probes for existence.
                                let gone = unsafe { libc::kill(pid, 0) } != 0
                                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                                if gone {
                                    break;
                                }
                            }
                            None if started.elapsed() > std::time::Duration::from_secs(120) => break,
                            None => {}
                        }
                    }
                } => {}
            }
            Ok(())
        }
        Some("branch") => {
            let (mut from, mut to) = (None, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--from" => from = args.next(),
                    "--to" => to = args.next(),
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            let (from, to) = (from.context("--from")?, to.context("--to")?);
            CowBackend::create_on_overlay(Path::new(&to), Path::new(&from))?;
            println!("created {to} on top of {from} (parent frozen)");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}
