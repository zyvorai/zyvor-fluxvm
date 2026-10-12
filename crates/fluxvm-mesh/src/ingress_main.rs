// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::acme::{
    AcmeClient, default_account_key_path, default_certificate_key_path, default_certificate_path,
};
use fluxvm_mesh::ingress::{ChallengeStore, default_challenge_dir, serve_http01, serve_https};
use fluxvm_mesh::public::default_public_config_path;
use fluxvm_mesh::share::default_share_secret_path;
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(e) = real().await {
        eprintln!("fluxmesh-ingress: {e}");
        std::process::exit(1)
    }
}
async fn real() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1).collect::<Vec<_>>();
    if a.is_empty() {
        return Err(usage().into());
    }
    match a.remove(0).as_str() {
        "serve" => {
            let mut listen: SocketAddr = "0.0.0.0:443".parse()?;
            let mut cfg = default_public_config_path();
            let mut secret = default_share_secret_path();
            let mut cert = default_certificate_path();
            let mut key = default_certificate_key_path();
            let mut http01: Option<SocketAddr> = None;
            let mut challenge = default_challenge_dir();
            parse_pairs(&a, |k, v| match k {
                "--listen" => listen = v.parse().unwrap(),
                "--config" => cfg = PathBuf::from(v),
                "--share-secret" => secret = PathBuf::from(v),
                "--cert" => cert = PathBuf::from(v),
                "--key" => key = PathBuf::from(v),
                "--http01-listen" => http01 = Some(v.parse().unwrap()),
                "--challenge-dir" => challenge = PathBuf::from(v),
                _ => {}
            })?;
            if let Some(addr) = http01 {
                let store = ChallengeStore::new(challenge);
                tokio::spawn(async move {
                    let _ = serve_http01(addr, store).await;
                });
            }
            serve_https(listen, cfg, secret, cert, key).await?;
        }
        "acme" => {
            let mut directory = "https://acme-v02.api.letsencrypt.org/directory".to_string();
            let mut email = String::new();
            let mut domains = Vec::new();
            let mut account = default_account_key_path();
            let mut challenge = default_challenge_dir();
            let mut cert = default_certificate_path();
            let mut key = default_certificate_key_path();
            let mut http: SocketAddr = "0.0.0.0:80".parse()?;
            let mut i = 0;
            while i < a.len() {
                let k = &a[i];
                if k == "--domain" {
                    domains.push(a.get(i + 1).ok_or("missing --domain value")?.clone());
                    i += 2;
                    continue;
                }
                let v = a.get(i + 1).ok_or("missing option value")?;
                match k.as_str() {
                    "--directory" => directory = v.clone(),
                    "--email" => email = v.clone(),
                    "--account-key" => account = PathBuf::from(v),
                    "--challenge-dir" => challenge = PathBuf::from(v),
                    "--cert" => cert = PathBuf::from(v),
                    "--key" => key = PathBuf::from(v),
                    "--http-listen" => http = v.parse()?,
                    _ => return Err(format!("unknown option {k}").into()),
                }
                i += 2;
            }
            if domains.is_empty() {
                return Err("at least one --domain is required".into());
            }
            let store = ChallengeStore::new(challenge);
            let listener_store = store.clone();
            tokio::spawn(async move {
                let _ = serve_http01(http, listener_store).await;
            });
            let mut client = AcmeClient::connect(&directory, &account).await?;
            let bundle = client
                .obtain_http01(&email, &domains, &store, &cert, &key)
                .await?;
            println!(
                "certificate={} key={} domains={}",
                bundle.cert_path.display(),
                bundle.key_path.display(),
                bundle.domains.join(",")
            );
        }
        _ => return Err(usage().into()),
    }
    Ok(())
}
fn parse_pairs<F: FnMut(&str, &str)>(
    a: &[String],
    mut f: F,
) -> Result<(), Box<dyn std::error::Error>> {
    if a.len() % 2 != 0 {
        return Err("options must be --flag value pairs".into());
    }
    let mut i = 0;
    while i < a.len() {
        if !a[i].starts_with("--") {
            return Err(format!("invalid option {}", a[i]).into());
        }
        f(&a[i], &a[i + 1]);
        i += 2;
    }
    Ok(())
}
fn usage() -> &'static str {
    "usage: fluxmesh-ingress serve [--listen 0.0.0.0:443 ...] | acme --email EMAIL --domain NAME [--domain NAME...]"
}
