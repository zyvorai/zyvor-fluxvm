// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::edge::{EdgeConfig, reconcile_once, run};
use fluxvm_mesh::wan::WanState;
use std::path::PathBuf;
use std::time::Duration;
#[tokio::main]
async fn main() {
    if let Err(e) = real().await {
        eprintln!("fluxmesh-edge: {e}");
        std::process::exit(1)
    }
}
async fn real() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        return Err(usage().into());
    }
    let mode = args.remove(0);
    let cfg = parse(args)?;
    match mode.as_str() {
        "once" => {
            let r = reconcile_once(&cfg, &mut WanState::default()).await?;
            println!(
                "acme={} ddns4={} ddns6={} mappings={} wan={} commands={}",
                r.acme_renewed,
                r.ddns_v4,
                r.ddns_v6,
                r.router_mappings,
                r.active_wan.as_deref().unwrap_or("none"),
                r.wan_commands
            )
        }
        "run" => run(cfg).await?,
        _ => return Err(usage().into()),
    }
    Ok(())
}
fn parse(a: Vec<String>) -> Result<EdgeConfig, Box<dyn std::error::Error>> {
    let mut c = EdgeConfig::default();
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--state" => c.state_path = PathBuf::from(val(&a, i)?),
            "--public-config" => c.public_config_path = PathBuf::from(val(&a, i)?),
            "--share-secret" => c.share_secret = PathBuf::from(val(&a, i)?),
            "--account-key" => c.acme_account_key = PathBuf::from(val(&a, i)?),
            "--acme-marker" => c.acme_marker = PathBuf::from(val(&a, i)?),
            "--cert" => c.cert_path = PathBuf::from(val(&a, i)?),
            "--key" => c.cert_key_path = PathBuf::from(val(&a, i)?),
            "--challenge-dir" => c.challenge_dir = PathBuf::from(val(&a, i)?),
            "--https-listen" => c.https_listen = val(&a, i)?.parse()?,
            "--http3-listen" => c.http3_listen = Some(val(&a, i)?.parse()?),
            "--no-http3" => {
                c.http3_listen = None;
                i += 1;
                continue;
            }
            "--http01-listen" => c.http01_listen = val(&a, i)?.parse()?,
            "--interval" => c.interval = Duration::from_secs(val(&a, i)?.parse()?),
            "--stun-bind" => c.stun_bind = val(&a, i)?.parse()?,
            "--telemetry" => c.telemetry_path = PathBuf::from(val(&a, i)?),
            "--stun" => {
                c.stun_servers.push(val(&a, i)?.parse()?);
                i += 2;
                continue;
            }
            "--dry-run" => {
                c.dry_run = true;
                i += 1;
                continue;
            }
            x => return Err(format!("unknown option {x}").into()),
        }
        i += 2
    }
    Ok(c)
}
fn val(a: &[String], i: usize) -> Result<&str, Box<dyn std::error::Error>> {
    a.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("missing value for {}", a[i]).into())
}
fn usage() -> &'static str {
    "usage: fluxmesh-edge once|run [--state PATH --public-config PATH --http3-listen IP:PORT|--no-http3 --stun IP:PORT ... --dry-run]"
}
