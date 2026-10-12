// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use fluxvm_mesh::agent::{AgentConfig, reconcile_once, run_forever};
use fluxvm_mesh::catalog::{CatalogStore, default_catalog_path};
use fluxvm_mesh::fabric::compile_network_fabric_cnp;
use fluxvm_mesh::macos::MacTunnelPlan;
use fluxvm_mesh::reconcile::{reconcile_linux, rotate_local_key_if_due};
use fluxvm_mesh::stun::{probe_many, punch_udp};
use fluxvm_mesh::{StateStore, default_state_path};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "fluxmesh-agent - runtime reconciliation for FluxMesh\n\n\
USAGE:\n\
  fluxmesh-agent once [OPTIONS]\n\
  fluxmesh-agent run [OPTIONS]\n\
  fluxmesh-agent stun BIND_ADDR STUN_SERVER...\n\
  fluxmesh-agent punch BIND_ADDR PEER_ADDR [COUNT]\n\
  fluxmesh-agent wg-rotate STATE_PATH KEY_PATH MAX_AGE_DAYS\n\
  fluxmesh-agent cnp-plan [STATE_PATH] [CATALOG_PATH] [SOURCE_NODE]\n\
  fluxmesh-agent mac-plan [STATE_PATH] [CATALOG_PATH]\n\n\
OPTIONS (once/run):\n\
  --state PATH              FluxMesh connectivity state\n\
  --catalog PATH            service/DNS/ACL catalog\n\
  --ledger PATH             last-applied Service/CNP ledger\n\
  --api URL                 local FluxVM HTTP API, e.g. http://127.0.0.1:7788\n\
  --token TOKEN             FluxVM bearer token\n\
  --interface NAME          WireGuard interface (default fluxmesh0)\n\
  --listen-port PORT        WireGuard port (default 51820)\n\
  --key PATH                WireGuard private key file\n\
  --keydist PATH            two-phase public-key distribution state\n\
  --interval SECONDS        run-loop interval (default 15)\n\
  --key-max-age-days DAYS   opt-in local WG key rotation (0 disables)\n\
  --stun-bind ADDR          STUN source address; use :51820 only before WG owns it\n\
  --stun SERVER             repeatable STUN server IP:port\n\
  --stun-timeout-ms MS      per-server timeout\n\
  --macos-plan PATH         NetworkExtension desired-state handoff\n\
  --relay-binding PEER=ADDR local UDP endpoint owned by native TLS relay client; repeatable\n\
  --dry-run                 calculate actions without modifying host/Fabric"
    );
    std::process::exit(2);
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fluxmesh-agent: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args.remove(0).as_str() {
        "once" => {
            let cfg = parse_agent_config(args)?;
            let report = reconcile_once(&cfg)?;
            println!(
                "wg_commands={} services={} cnp={} stun={} stun_error={} direct_endpoint={} key_rotated={} macos_plan={}",
                report.wireguard_commands,
                report.fabric_services,
                report.fabric_cnps,
                report.stun.is_some(),
                report.stun_error.as_deref().unwrap_or("none"),
                report.direct_endpoint_persisted,
                report.key_rotated,
                report.macos_plan_written,
            );
        }
        "run" => {
            let cfg = parse_agent_config(args)?;
            run_forever(cfg);
        }
        "stun" => {
            if args.len() < 2 {
                usage();
            }
            let bind: SocketAddr = args.remove(0).parse()?;
            let servers = args
                .iter()
                .map(|value| value.parse::<SocketAddr>())
                .collect::<Result<Vec<_>, _>>()?;
            let report = probe_many(&servers, bind, Duration::from_secs(2))?;
            println!("nat={}", report.nat_type);
            for observation in report.observations {
                println!(
                    "server={} mapped={} rtt_ms={}",
                    observation.server, observation.mapped_addr, observation.rtt_ms
                );
            }
        }
        "punch" => {
            if !(2..=3).contains(&args.len()) {
                usage();
            }
            let bind: SocketAddr = args[0].parse()?;
            let peer: SocketAddr = args[1].parse()?;
            let count = args
                .get(2)
                .map(|v| v.parse::<u16>())
                .transpose()?
                .unwrap_or(4);
            let local = punch_udp(bind, peer, count, Duration::from_millis(150))?;
            println!("punched peer={peer} from={local} attempts={count}");
        }
        "wg-rotate" => {
            if args.len() != 3 {
                usage();
            }
            let days: u64 = args[2].parse()?;
            let result = rotate_local_key_if_due(
                PathBuf::from(&args[0]).as_path(),
                PathBuf::from(&args[1]).as_path(),
                Duration::from_secs(days.saturating_mul(86_400)),
            )?;
            println!(
                "rotated={} public_key={}",
                result.rotated,
                result.public_key.as_deref().unwrap_or("unchanged")
            );
        }
        "cnp-plan" => {
            if args.len() > 3 {
                usage();
            }
            let state_path = args
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(default_state_path);
            let catalog_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(default_catalog_path);
            let mesh = StateStore::new(&state_path).load()?;
            let catalog = CatalogStore::new(&catalog_path).load(&mesh)?;
            let source = args.get(2).map(String::as_str).unwrap_or(&mesh.local_node);
            println!(
                "{}",
                compile_network_fabric_cnp(&mesh, &catalog, source)?.json
            );
        }
        "mac-plan" => {
            if args.len() > 2 {
                usage();
            }
            let state_path = args
                .first()
                .map(PathBuf::from)
                .unwrap_or_else(default_state_path);
            let catalog_path = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(default_catalog_path);
            let mesh = StateStore::new(&state_path).load()?;
            let catalog = CatalogStore::new(&catalog_path).load(&mesh)?;
            println!(
                "{}",
                MacTunnelPlan::from_mesh(&mesh, 51820, catalog.dns_suffix)?.to_json()
            );
        }
        _ => usage(),
    }
    Ok(())
}

fn parse_agent_config(args: Vec<String>) -> Result<AgentConfig, Box<dyn std::error::Error>> {
    let mut cfg = AgentConfig::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: usize| -> Result<&str, Box<dyn std::error::Error>> {
            args.get(i + 1).map(String::as_str).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("missing value for {}", args[i]),
                )
                .into()
            })
        };
        match flag {
            "--state" => {
                cfg.state_path = PathBuf::from(value(i)?);
                i += 2;
            }
            "--catalog" => {
                cfg.catalog_path = PathBuf::from(value(i)?);
                i += 2;
            }
            "--ledger" => {
                cfg.ledger_path = PathBuf::from(value(i)?);
                i += 2;
            }
            "--api" => {
                cfg.api_url = Some(value(i)?.to_owned());
                i += 2;
            }
            "--token" => {
                cfg.api_token = Some(value(i)?.to_owned());
                i += 2;
            }
            "--interface" => {
                cfg.interface = value(i)?.to_owned();
                i += 2;
            }
            "--listen-port" => {
                cfg.listen_port = value(i)?.parse()?;
                i += 2;
            }
            "--key" => {
                cfg.private_key_file = PathBuf::from(value(i)?);
                i += 2;
            }
            "--keydist" => {
                cfg.keydist_path = PathBuf::from(value(i)?);
                i += 2;
            }
            "--interval" => {
                cfg.interval = Duration::from_secs(value(i)?.parse()?);
                i += 2;
            }
            "--key-max-age-days" => {
                let days: u64 = value(i)?.parse()?;
                cfg.key_max_age = if days == 0 {
                    None
                } else {
                    Some(Duration::from_secs(days.saturating_mul(86_400)))
                };
                i += 2;
            }
            "--stun-bind" => {
                cfg.stun_bind = value(i)?.parse()?;
                i += 2;
            }
            "--stun" => {
                cfg.stun_servers.push(value(i)?.parse()?);
                i += 2;
            }
            "--stun-timeout-ms" => {
                cfg.stun_timeout = Duration::from_millis(value(i)?.parse()?);
                i += 2;
            }
            "--macos-plan" => {
                cfg.macos_plan_path = PathBuf::from(value(i)?);
                i += 2;
            }
            "--relay-binding" => {
                let raw = value(i)?;
                let (peer, endpoint) = raw.split_once('=').ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid --relay-binding {raw}; expected PEER=IP:PORT"),
                    )
                })?;
                if peer.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "relay binding peer may not be empty",
                    )
                    .into());
                }
                cfg.relay_bindings
                    .insert(peer.to_owned(), endpoint.parse()?);
                i += 2;
            }
            "--dry-run" => {
                cfg.dry_run = true;
                i += 1;
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("unknown option {flag}"),
                )
                .into());
            }
        }
    }
    Ok(cfg)
}

#[allow(dead_code)]
fn _print_wg_dry_run(cfg: &AgentConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mesh = StateStore::new(&cfg.state_path).load()?;
    let mut plan = mesh.wireguard_plan(&cfg.interface, cfg.listen_port, &cfg.private_key_file)?;
    fluxvm_mesh::ipv6::augment_wireguard_plan(&mesh, &mut plan)?;
    let report = reconcile_linux(&plan, true)?;
    for argv in report.commands {
        println!("{}", argv.join(" "));
    }
    Ok(())
}
