// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use fluxvm_mesh::telemetry::{
    TelemetryStore, default_telemetry_path, evaluate_slo, render_prometheus,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
#[tokio::main]
async fn main() {
    if let Err(e) = real().await {
        eprintln!("fluxmesh-metrics: {e}");
        std::process::exit(1)
    }
}
async fn real() -> Result<(), Box<dyn std::error::Error>> {
    let a = std::env::args().skip(1).collect::<Vec<_>>();
    let mut listen: std::net::SocketAddr = "127.0.0.1:9468".parse()?;
    let mut path = default_telemetry_path();
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--listen" => listen = a.get(i + 1).ok_or("missing --listen")?.parse()?,
            "--telemetry" => path = a.get(i + 1).ok_or("missing --telemetry")?.into(),
            x => return Err(format!("unknown option {x}").into()),
        }
        i += 2
    }
    let l = TcpListener::bind(listen).await?;
    loop {
        let (mut s, _) = l.accept().await?;
        let store = TelemetryStore::new(path.clone());
        tokio::spawn(async move {
            let mut b = [0u8; 2048];
            let n = s.read(&mut b).await.unwrap_or(0);
            let line = String::from_utf8_lossy(&b[..n]);
            let target = line.split_whitespace().nth(1).unwrap_or("/");
            let v = store.load().unwrap_or_default();
            let (body, status, ctype) = if target == "/metrics" {
                (render_prometheus(&v), "200 OK", "text/plain; version=0.0.4")
            } else if target == "/healthz" {
                let r = evaluate_slo(&v, 120_000, 0.20);
                if r.healthy {
                    ("ok\n".into(), "200 OK", "text/plain")
                } else {
                    (
                        format!("{}\n", r.reasons.join("; ")),
                        "503 Service Unavailable",
                        "text/plain",
                    )
                }
            } else {
                ("not found\n".into(), "404 Not Found", "text/plain")
            };
            let h = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(h.as_bytes()).await;
            let _ = s.write_all(body.as_bytes()).await;
        });
    }
}
