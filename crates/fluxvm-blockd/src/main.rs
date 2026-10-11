// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! fluxvm-blockd serve --export name=base[,overlay=path][,ro][,iops=N][,bps=N]...
//!                     [--listen 127.0.0.1:10809] [--watch-pid P | --watch-pid-file F]
//! fluxvm-blockd branch --from overlay --to new-overlay
//! fluxvm-blockd rebase --overlay X --parent Y
//! fluxvm-blockd resize --overlay X --size N

use anyhow::{Context, Result, bail};
use fluxvm_blockd::cow::{is_overlay_file, overlay_info};
use fluxvm_blockd::crypt::KEY_ENV;
use fluxvm_blockd::seams::BlockCipher;
use fluxvm_blockd::{
    BlockBackend, CowBackend, CreateOptions, Export, LazyConfig, LazyHttp, Limiter, OpenOptions,
    RawFile, Server, XtsCipher,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage:
  fluxvm-blockd serve --export SPEC [--export SPEC ...] [--listen 127.0.0.1:10809]
                      [--watch-pid P | --watch-pid-file F]
      SPEC = name=base[,overlay=path][,ro][,iops=N][,bps=N]
             [,encrypt][,keyfile=PATH]
             [,sha256=HEX][,parallel=N][,timeout=SECS][,cache=PATH]
      `base` may be an image file, an overlay file or an http(s) URL (lazy,
      read-through; needs overlay=... or cache=...).
  fluxvm-blockd branch --from <overlay> --to <new-overlay> [--key-file PATH]
  fluxvm-blockd rebase --overlay <overlay> --parent <file|url> [--key-file PATH] [--sha256 HEX]
  fluxvm-blockd resize --overlay <overlay> --size <bytes|NK|NM|NG|NT>

An overlay that does not exist is created on top of `base`. If `base` is
itself an overlay file it is used as the parent (and frozen). Keys: 32 or 64
raw bytes, or 64/128 hex digits, from `keyfile=`/--key-file or the
FLUXVM_BLOCKD_KEY environment variable (hex). Keys are never printed.";

fn load_key(file: Option<&str>) -> Result<Option<Arc<dyn BlockCipher>>> {
    if let Some(f) = file {
        let c = XtsCipher::from_file(Path::new(f)).context("reading key file")?;
        return Ok(Some(Arc::new(c)));
    }
    Ok(XtsCipher::from_env(KEY_ENV)
        .context("reading key from environment")?
        .map(|c| Arc::new(c) as Arc<dyn BlockCipher>))
}

fn parse_hex32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256 must be 64 hex digits");
    }
    let mut o = [0u8; 32];
    for (i, b) in o.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)?;
    }
    Ok(o)
}

fn parse_size(s: &str) -> Result<u64> {
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()?
        .checked_mul(mul)
        .context("size overflow")
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn parse_export(spec: &str) -> Result<Export> {
    let mut parts = spec.split(',');
    let first = parts.next().unwrap_or("");
    let (name, base) = first
        .split_once('=')
        .context("export must start with name=base")?;
    let (mut overlay, mut ro, mut iops, mut bps) = (None, false, None, None);
    let (mut encrypt, mut keyfile, mut cache) = (false, None, None);
    let mut lazy = LazyConfig::default();
    for p in parts {
        match p.split_once('=') {
            Some(("overlay", v)) => overlay = Some(PathBuf::from(v)),
            Some(("iops", v)) => iops = Some(v.parse::<u64>()?),
            Some(("bps", v)) => bps = Some(v.parse::<u64>()?),
            Some(("keyfile", v)) => keyfile = Some(v.to_string()),
            Some(("sha256", v)) => lazy.sha256 = Some(parse_hex32(v)?),
            Some(("parallel", v)) => lazy.parallel = v.parse::<usize>()?.max(1),
            Some(("timeout", v)) => lazy.timeout = Duration::from_secs(v.parse()?),
            Some(("cache", v)) => cache = Some(PathBuf::from(v)),
            None if p == "ro" => ro = true,
            None if p == "encrypt" => encrypt = true,
            _ => bail!("unknown export option `{p}`"),
        }
    }
    let key = load_key(keyfile.as_deref())?;
    let open = OpenOptions {
        key: key.clone(),
        lazy: lazy.clone(),
    };
    let create = CreateOptions {
        size: None,
        cipher: if encrypt { key.clone() } else { None },
    };
    if encrypt && key.is_none() {
        bail!("`encrypt` needs a key (keyfile= or {KEY_ENV})");
    }
    let base_p = PathBuf::from(base);
    let direct_lazy = overlay.is_none() && is_url(base);
    let backend: Arc<dyn BlockBackend> = match overlay {
        Some(o) if o.exists() => CowBackend::open_with(&o, &open)?,
        Some(o) if is_url(base) => CowBackend::create_on_lazy(&o, base, &create, &lazy)?,
        Some(o) if is_overlay_file(&base_p) => {
            // Encrypted parents force an encrypted child.
            let mut c = create.clone();
            if overlay_info(&base_p)?.0 && c.cipher.is_none() {
                c.cipher = key.clone();
            }
            CowBackend::create_on_overlay_opts(&o, &base_p, &c, &open)?
        }
        Some(o) => CowBackend::create_opts(&o, &base_p, &create)?,
        None if is_url(base) => {
            let c = cache.context("lazy export without overlay needs cache=PATH")?;
            LazyHttp::open(base, &c, lazy)?
        }
        None if is_overlay_file(&base_p) => CowBackend::open_with(&base_p, &open)?,
        None => Arc::new(RawFile::open(&base_p, ro)?),
    };
    let mut e = Export::new(name, backend).read_only(ro || direct_lazy);
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
            let mut specs = Vec::new();
            let mut watch_pid: Option<i32> = None;
            let mut watch_file: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--export" => specs.push(args.next().context("--export value")?),
                    "--listen" => listen = args.next().context("--listen value")?,
                    "--watch-pid-file" => {
                        watch_file = Some(PathBuf::from(
                            args.next().context("--watch-pid-file value")?,
                        ))
                    }
                    "--watch-pid" => {
                        watch_pid = Some(args.next().context("--watch-pid value")?.parse::<i32>()?)
                    }
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            if specs.is_empty() {
                bail!("at least one --export is required\n{USAGE}");
            }
            // Backends use blocking IO (and a blocking http client).
            let exports = tokio::task::spawn_blocking(move || {
                specs
                    .iter()
                    .map(|s| parse_export(s))
                    .collect::<Result<Vec<_>>>()
            })
            .await??;
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
            let (mut from, mut to, mut kf) = (None, None, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--from" => from = args.next(),
                    "--to" => to = args.next(),
                    "--key-file" => kf = args.next(),
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            let (from, to) = (from.context("--from")?, to.context("--to")?);
            let key = load_key(kf.as_deref())?;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let enc = overlay_info(Path::new(&from))?.0;
                let open = OpenOptions {
                    key: key.clone(),
                    ..Default::default()
                };
                let create = CreateOptions {
                    size: None,
                    cipher: if enc { key } else { None },
                };
                CowBackend::create_on_overlay_opts(
                    Path::new(&to),
                    Path::new(&from),
                    &create,
                    &open,
                )?;
                println!("created {to} on top of {from} (parent frozen)");
                Ok(())
            })
            .await?
        }
        Some("rebase") => {
            let (mut ov, mut parent, mut kf, mut sha) = (None, None, None, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--overlay" => ov = args.next(),
                    "--parent" => parent = args.next(),
                    "--key-file" => kf = args.next(),
                    "--sha256" => sha = args.next(),
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            let (ov, parent) = (ov.context("--overlay")?, parent.context("--parent")?);
            let mut open = OpenOptions {
                key: load_key(kf.as_deref())?,
                ..Default::default()
            };
            if let Some(s) = sha {
                open.lazy.sha256 = Some(parse_hex32(&s)?);
            }
            tokio::task::spawn_blocking(move || -> Result<()> {
                CowBackend::rebase(Path::new(&ov), &parent, &open)?;
                println!("rebased {ov} onto {parent}");
                Ok(())
            })
            .await?
        }
        Some("resize") => {
            let (mut ov, mut size) = (None, None);
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--overlay" => ov = args.next(),
                    "--size" => size = args.next(),
                    _ => bail!("unknown argument `{a}`\n{USAGE}"),
                }
            }
            let (ov, size) = (
                ov.context("--overlay")?,
                parse_size(&size.context("--size")?)?,
            );
            tokio::task::spawn_blocking(move || -> Result<()> {
                let moved = CowBackend::resize_file(Path::new(&ov), size)?;
                println!(
                    "resized {ov} to {size} bytes ({})",
                    if moved { "rewritten" } else { "in place" }
                );
                Ok(())
            })
            .await?
        }
        _ => bail!("{USAGE}"),
    }
}
