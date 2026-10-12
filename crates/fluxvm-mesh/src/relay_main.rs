// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use fluxvm_mesh::relay::{RelayPeerBinding, load_credentials, run_tcp_client, serve_tcp};
use fluxvm_mesh::tls_relay::{run_tls_client, serve_tls};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "fluxmesh-relay - opaque WireGuard relay for FluxMesh\n\n\
USAGE:\n\
  # trusted/private transport retained for labs\n\
  fluxmesh-relay serve LISTEN_ADDR CREDENTIAL_FILE\n\
  fluxmesh-relay client RELAY_ADDR NODE TOKEN WG_ADDR PEER=LOCAL_UDP_ADDR...\n\n\
  # production hard-NAT fallback: native TLS, no stunnel required\n\
  fluxmesh-relay serve-tls LISTEN_ADDR CERT_PEM KEY_PEM CREDENTIAL_FILE\n\
  fluxmesh-relay client-tls RELAY_ADDR SERVER_NAME CA_PEM NODE TOKEN WG_ADDR PEER=LOCAL_UDP_ADDR...\n\n\
Each PEER binding is the UDP endpoint configured for that WireGuard peer.\n\
The TLS server reloads replaced certificate/key files for new connections."
    );
    std::process::exit(2);
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fluxmesh-relay: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args.remove(0).as_str() {
        "serve" => {
            if args.len() != 2 {
                usage();
            }
            let listen: SocketAddr = args[0].parse()?;
            let credentials = load_credentials(&fs::read_to_string(&args[1])?)?;
            serve_tcp(listen, credentials)?;
        }
        "client" => {
            if args.len() < 5 {
                usage();
            }
            let relay: SocketAddr = args.remove(0).parse()?;
            let node = args.remove(0);
            let token = args.remove(0);
            let wg_addr: SocketAddr = args.remove(0).parse()?;
            let bindings = parse_bindings(args)?;
            run_tcp_client(relay, &node, &token, wg_addr, bindings)?;
        }
        "serve-tls" => {
            if args.len() != 4 {
                usage();
            }
            let listen: SocketAddr = args[0].parse()?;
            let cert = PathBuf::from(&args[1]);
            let key = PathBuf::from(&args[2]);
            let credentials = load_credentials(&fs::read_to_string(&args[3])?)?;
            runtime()?.block_on(serve_tls(listen, cert, key, credentials))?;
        }
        "client-tls" => {
            if args.len() < 7 {
                usage();
            }
            let relay: SocketAddr = args.remove(0).parse()?;
            let server_name = args.remove(0);
            let ca = PathBuf::from(args.remove(0));
            let node = args.remove(0);
            let token = args.remove(0);
            let wg_addr: SocketAddr = args.remove(0).parse()?;
            let bindings = parse_bindings(args)?;
            runtime()?.block_on(run_tls_client(
                relay,
                &server_name,
                &ca,
                &node,
                &token,
                wg_addr,
                bindings,
            ))?;
        }
        _ => usage(),
    }
    Ok(())
}

fn parse_bindings(args: Vec<String>) -> Result<Vec<RelayPeerBinding>, Box<dyn std::error::Error>> {
    args.into_iter()
        .map(|raw| {
            let (peer_id, addr) = raw.split_once('=').ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid peer binding {raw}"),
                )
            })?;
            let bind_addr = addr.parse::<SocketAddr>()?;
            Ok(RelayPeerBinding {
                peer_id: peer_id.to_owned(),
                bind_addr,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
}
