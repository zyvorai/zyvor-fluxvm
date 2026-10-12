// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use fluxvm_mesh::cloud::CloudPaths;
use fluxvm_mesh::enroll_tls;
use fluxvm_mesh::reconcile::{rotate_wireguard_key, wireguard_public_key};
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage:\n  fluxmesh-enroll serve ADDR CERT KEY [STATE SECRET NONCES]\n  fluxmesh-enroll join ADDR SERVER_NAME CA TOKEN [KEY STATE]"
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        usage()
    }
    match args.remove(0).as_str() {
        "serve" => {
            if !(3..=6).contains(&args.len()) {
                usage()
            }
            let d = CloudPaths::default();
            enroll_tls::serve(
                args[0].parse::<SocketAddr>()?,
                PathBuf::from(&args[1]).as_path(),
                PathBuf::from(&args[2]).as_path(),
                args.get(3).map(PathBuf::from).unwrap_or(d.state),
                args.get(4).map(PathBuf::from).unwrap_or(d.enroll_secret),
                args.get(5).map(PathBuf::from).unwrap_or(d.used_nonces),
            )
            .await?;
        }
        "join" => {
            if !(4..=6).contains(&args.len()) {
                usage()
            }
            let d = CloudPaths::default();
            let key = args.get(4).map(PathBuf::from).unwrap_or(d.key);
            let state = args.get(5).map(PathBuf::from).unwrap_or(d.state);
            let public = if key.exists() {
                wireguard_public_key(&key)?
            } else {
                rotate_wireguard_key(&key)?
            };
            enroll_tls::join(
                args[0].parse()?,
                &args[1],
                PathBuf::from(&args[2]).as_path(),
                &args[3],
                &public,
                &state,
            )
            .await?;
            println!("joined public_key={public} state={}", state.display());
        }
        _ => usage(),
    }
    Ok(())
}
