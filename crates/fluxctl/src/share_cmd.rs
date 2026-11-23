// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl share NAME` and `fluxctl import SOURCE`: hand a stopped macOS instance to someone else as one file.
//!
//! `share` writes the same archive `mac export` writes (the disk, the hardware and auxiliary files, a manifest, and a
//! `.sha256` beside it) and prints the one `fluxctl import` line that restores it. With `--serve` it also offers the file once
//! over plain HTTP on this machine, under a random one-time token in the path; the server stops after the first complete
//! download or after `--timeout` seconds. By default it listens on 127.0.0.1 only. Nothing is uploaded to any service and no
//! usage data is sent anywhere.
//!
//! What is never in the bundle: the instance's environment and user data, registry logins, API tokens, build secrets, SSH
//! keys and host folders (the archive holds only the guest's disk files); `share` lists them. What cannot be removed is whatever
//! lives *inside* the guest's disk, and the output says so.
//!
//! Not verified against a real macOS VM or across two Macs: tested with temporary files and a loopback server.

use crate::mac_errors::coded;
use crate::mac_image_ops as ops;
use crate::mac_imgx::sidecar_path;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(clap::Args, Clone, Debug)]
pub struct ShareArgs {
    /// The STOPPED macOS instance to share (see `fluxctl mac ps -a`; stop it with `fluxctl mac stop NAME`).
    pub name: String,
    /// Where to write the bundle. Default: ./NAME-share.tar. An existing file is never replaced.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Also offer the bundle once over HTTP from this machine, and wait until it is fetched (or the timeout passes).
    #[arg(long)]
    pub serve: bool,
    /// Port for --serve. Default: any free port.
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    /// Address for --serve. Default 127.0.0.1 (this machine only). Use this Mac's LAN address to let another Mac on your network
    /// fetch it; the connection is plain HTTP, so only do that on a network you trust.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// How long --serve waits for the download, in seconds.
    #[arg(long, default_value_t = 900)]
    pub timeout: u64,
    /// Print the result as JSON (the token is only in `import_command` when --serve is on).
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Clone, Debug)]
pub struct ImportArgs {
    /// A bundle file written by `fluxctl share`, or the http:// link it printed.
    pub source: String,
    /// Name to store it under (NAME:TAG). Default: derived from the file name, `NAME:shared`.
    #[arg(long = "as", value_name = "NAME:TAG")]
    pub as_ref: Option<String>,
}

/// `./NAME-share.tar`, with anything but letters, digits, `.`, `_` and `-` in NAME turned into `-`.
pub fn default_out(name: &str) -> PathBuf {
    PathBuf::from(format!("{}-share.tar", slug(name)))
}

fn slug(n: &str) -> String {
    let s: String = n
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let t = s.trim_matches(|c| c == '-' || c == '.');
    if t.is_empty() {
        "instance".into()
    } else {
        t.to_owned()
    }
}

/// The reference an import lands under when `--as` is not given: `build-1-share.tar` becomes `build-1:shared`.
pub fn default_reference(source: &str) -> String {
    let file = source
        .split(['?', '#'])
        .next()
        .unwrap_or(source)
        .rsplit('/')
        .next()
        .unwrap_or("");
    let stem = file.strip_suffix(".tar").unwrap_or(file);
    let stem = stem.strip_suffix("-share").unwrap_or(stem);
    format!("{}:shared", slug(&stem.to_ascii_lowercase()))
}

/// Everything that is deliberately not in the bundle, as plain sentences. Environment variable names from the instance are
/// listed (never their values).
pub fn excluded(vm: &Value) -> Vec<String> {
    let mut v = vec![
        "registry logins (`fluxctl mac login`) and API or fleet tokens".to_owned(),
        "build secrets and SSH keys or agent access given to `mac build` / `mac run`".to_owned(),
        "the instance's launch settings: user data, published ports and host folders".to_owned(),
    ];
    if let Some(env) = vm["request"]["env"].as_object()
        && !env.is_empty()
    {
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        v.push(format!(
            "environment variables ({}); the values are not read",
            names.join(", ")
        ));
    }
    v
}

/// What stays in the bundle no matter what, because it is part of the disk.
pub const GUEST_WARNING: &str = "The guest's disk is copied as it is. Anything stored inside the guest (saved passwords, SSH keys in its home folder, keychain items, shell history) travels with it. Remove what you do not want to hand over before you share.";

/// A random token that is not guessable: 244 bits from the OS.
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn friendly_host(bind: &str) -> String {
    match bind {
        "0.0.0.0" | "::" | "[::]" => "THIS-MAC-ADDRESS".to_owned(),
        b if b.contains(':') && !b.starts_with('[') => format!("[{b}]"),
        b => b.to_owned(),
    }
}

fn is_loopback(bind: &str) -> bool {
    bind.parse::<std::net::IpAddr>()
        .map(|a| a.is_loopback())
        .unwrap_or(bind == "localhost")
}

/// The line to run on the other machine.
pub fn import_command(source: &str) -> String {
    format!("fluxctl import {source}")
}

pub async fn run(r: &crate::remote::Remote, a: &ShareArgs) -> Result<()> {
    let out = a.out.clone().unwrap_or_else(|| default_out(&a.name));
    if out.exists() || sidecar_path(&out).exists() {
        bail!(
            "{} already exists and is not replaced. Pick another name with --out, or remove it first",
            out.display()
        );
    }
    let (vm, digest) = crate::mac_imgx::export_instance(r, &a.name, &out).await?;
    let abs = std::fs::canonicalize(&out).unwrap_or_else(|_| out.clone());
    let left_out = excluded(&vm);
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let file_cmd = import_command(&shell_quote(&abs.display().to_string()));

    if !a.serve {
        if a.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "path": abs.display().to_string(),
                    "bytes": size,
                    "sha256": digest,
                    "import_command": file_cmd,
                    "excluded": left_out,
                    "warning": GUEST_WARNING,
                    "sent_anywhere": false,
                }))?
            );
        } else {
            print!("{}", render_file(&abs, size, &digest, &file_cmd, &left_out));
        }
        return Ok(());
    }

    let token = new_token();
    let listener = TcpListener::bind((a.bind.as_str(), a.port))
        .await
        .map_err(|e| {
            coded(
                format!(
                    "could not serve the share bundle on {}:{}: {e}",
                    a.bind, a.port
                ),
                "SHARE_SERVE_FAILED",
            )
        })?;
    let addr = listener.local_addr()?;
    let file_name = out
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bundle.tar".into());
    let url = format!(
        "http://{}:{}/{token}/{file_name}",
        friendly_host(&a.bind),
        addr.port()
    );
    let serve_cmd = import_command(&url);
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": abs.display().to_string(),
                "bytes": size,
                "sha256": digest,
                "import_command": serve_cmd,
                "file_import_command": file_cmd,
                "serving_on": addr.to_string(),
                "loopback_only": is_loopback(&a.bind),
                "one_time": true,
                "timeout_seconds": a.timeout,
                "excluded": left_out,
                "warning": GUEST_WARNING,
                "sent_anywhere": false,
            }))?
        );
    } else {
        print!("{}", render_file(&abs, size, &digest, &file_cmd, &left_out));
        print!(
            "{}",
            render_serving(&a.bind, addr.port(), &serve_cmd, a.timeout)
        );
    }
    let done = serve_once(
        listener,
        token,
        file_name,
        out.clone(),
        Duration::from_secs(a.timeout),
    )
    .await;
    match done {
        Ok(true) => {
            if !a.json {
                println!(
                    "Downloaded once; the link no longer works. The file stays at {}.",
                    abs.display()
                );
            }
            Ok(())
        }
        Ok(false) => {
            if !a.json {
                println!(
                    "Nobody downloaded it within {} seconds, so the link was closed. The file is still at {}; run `fluxctl share {} --serve --out ...` with a new name to offer it again, or send the file by hand.",
                    a.timeout,
                    abs.display(),
                    a.name
                );
            }
            Ok(())
        }
        Err(e) => Err(coded(
            format!("could not serve the share bundle: {e:#}"),
            "SHARE_SERVE_FAILED",
        )),
    }
}

fn shell_quote(p: &str) -> String {
    if p.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '=' | ':'))
    {
        p.to_owned()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

fn render_file(path: &Path, bytes: u64, digest: &str, cmd: &str, left_out: &[String]) -> String {
    let mut s = format!(
        "Shared bundle written: {} ({})\nsha256 {digest} (also in {})\n\nNot included (never read into the bundle):\n",
        path.display(),
        crate::mac_inspect::human_bytes(bytes),
        sidecar_path(path).display()
    );
    for l in left_out {
        s.push_str(&format!("  - {l}\n"));
    }
    s.push_str(&format!("\n{GUEST_WARNING}\n"));
    s.push_str(&format!(
        "\nOn the other Mac, with the file copied over:\n  {cmd}\nNothing was uploaded or sent anywhere.\n"
    ));
    s
}

fn render_serving(bind: &str, port: u16, cmd: &str, timeout: u64) -> String {
    let mut s = format!(
        "\nServing it once on {bind}:{port} for {timeout} seconds (Ctrl-C stops it). On the other machine run:\n  {cmd}\n"
    );
    if matches!(bind, "0.0.0.0" | "::" | "[::]") {
        s.push_str(
            "Replace THIS-MAC-ADDRESS with this Mac's address as the other machine sees it.\n",
        );
    }
    if is_loopback(bind) {
        s.push_str("It listens on this machine only. To let another Mac on your network fetch it, add `--bind THIS-MACS-LAN-ADDRESS`.\n");
    } else {
        s.push_str("Warning: this is plain HTTP on a network; anyone who sees the link can download the bundle once. Use it on a network you trust.\n");
    }
    s
}

/// Serves `file` at `/TOKEN/NAME` to the first complete download, and its checksum at `/TOKEN/NAME.sha256` any number of times.
/// Returns `Ok(true)` after a complete download, `Ok(false)` on timeout. Every other path is a 404 with no hint.
pub async fn serve_once(
    listener: TcpListener,
    token: String,
    name: String,
    file: PathBuf,
    timeout: Duration,
) -> Result<bool> {
    let used = Arc::new(AtomicBool::new(false));
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let sidecar = std::fs::read_to_string(sidecar_path(&file)).unwrap_or_default();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Ok(false),
            _ = done_rx.recv() => return Ok(true),
            acc = listener.accept() => {
                let (sock, _) = acc.context("accepting a connection")?;
                let (used, tx) = (used.clone(), done_tx.clone());
                let (token, name, file, sidecar) = (token.clone(), name.clone(), file.clone(), sidecar.clone());
                tokio::spawn(async move {
                    let ok = handle(sock, &token, &name, &file, &sidecar, &used).await;
                    if ok == Some(true) {
                        let _ = tx.send(()).await;
                    }
                });
            }
        }
    }
}

/// Reads one request and answers it. `Some(true)` when the bundle was fully sent.
async fn handle(
    mut sock: TcpStream,
    token: &str,
    name: &str,
    file: &Path,
    sidecar: &str,
    used: &AtomicBool,
) -> Option<bool> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let n = sock.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(());
            }
            if buf.len() > 8192 {
                return None;
            }
        }
    })
    .await;
    if !matches!(read, Ok(Some(()))) {
        return None;
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        not_found(&mut sock).await;
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let want = format!("/{token}/{name}");
    let ct_eq = |a: &str, b: &str| {
        a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
    };
    if ct_eq(path, &format!("{want}.sha256")) && !sidecar.is_empty() {
        let _ = sock_write(&mut sock, "200 OK", "text/plain", sidecar.as_bytes()).await;
        return None;
    }
    if !ct_eq(path, &want) {
        not_found(&mut sock).await;
        return None;
    }
    // One download: the first request takes the token. If the transfer breaks, hand it back so the receiver can retry.
    if used.swap(true, Ordering::SeqCst) {
        not_found(&mut sock).await;
        return None;
    }
    let sent = send_file(&mut sock, file).await;
    if sent.is_err() {
        used.store(false, Ordering::SeqCst);
        return Some(false);
    }
    Some(true)
}

async fn not_found(s: &mut TcpStream) {
    let _ = sock_write(s, "404 Not Found", "text/plain", b"not found\n").await;
}

async fn sock_write(
    s: &mut TcpStream,
    status: &str,
    ctype: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body).await?;
    s.flush().await
}

async fn send_file(s: &mut TcpStream, file: &Path) -> std::io::Result<()> {
    let mut f = tokio::fs::File::open(file).await?;
    let len = f.metadata().await?.len();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    );
    s.write_all(head.as_bytes()).await?;
    tokio::io::copy(&mut f, s).await?;
    s.flush().await?;
    s.shutdown().await
}

/// `fluxctl import SOURCE`: a file, or an http(s) link from `share --serve`.
pub async fn run_import(a: &ImportArgs) -> Result<()> {
    let reference = a
        .as_ref
        .clone()
        .unwrap_or_else(|| default_reference(&a.source));
    let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
    let key = if a.source.starts_with("http://") || a.source.starts_with("https://") {
        let scratch = store.scratch("share-fetch");
        std::fs::create_dir_all(&scratch)?;
        let file = scratch.join("bundle.tar");
        let r = async {
            fetch(&a.source, &file).await?;
            ops::load(&store, &file, Some(&reference))
        }
        .await;
        std::fs::remove_dir_all(&scratch).ok();
        r?
    } else {
        let p = PathBuf::from(&a.source);
        if !p.is_file() {
            bail!(
                "{} is not a file. Pass the bundle `fluxctl share` wrote, or the http:// link it printed",
                p.display()
            );
        }
        ops::load(&store, &p, Some(&reference))?
    };
    println!("Imported as {key}.\nStart it with: fluxctl mac run {key}");
    Ok(())
}

fn fetch_failed(url: &str, why: impl std::fmt::Display) -> anyhow::Error {
    // Never echo the token path back: show the host only.
    let host = url
        .split("//")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or("the sender");
    coded(
        format!("could not download the shared bundle from {host}: {why}"),
        "SHARE_FETCH_FAILED",
    )
}

/// Downloads `url` to `dest` and its `.sha256` beside it (so `load` verifies it). A missing checksum is a warning, not an error.
async fn fetch(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    // The checksum comes first: the sender stops as soon as the bundle is fully sent.
    match client.get(format!("{url}.sha256")).send().await {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            std::fs::write(sidecar_path(dest), text)?;
        }
        _ => eprintln!(
            "note: no checksum was offered beside the bundle, so its integrity was not checked"
        ),
    }
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| fetch_failed(url, e.without_url()))?;
    if !resp.status().is_success() {
        return Err(fetch_failed(
            url,
            format!(
                "the sender answered {} (the link may already have been used)",
                resp.status().as_u16()
            ),
        ));
    }
    let mut f = tokio::fs::File::create(dest).await?;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| fetch_failed(url, e.without_url()))?
    {
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_imgx::verify_sidecar;

    #[test]
    fn names_and_references_are_derived_safely() {
        assert_eq!(default_out("Build 1"), PathBuf::from("Build-1-share.tar"));
        assert_eq!(default_out("../x"), PathBuf::from("x-share.tar"));
        assert_eq!(default_out("///"), PathBuf::from("instance-share.tar"));
        assert_eq!(
            default_reference("/tmp/build-1-share.tar"),
            "build-1:shared"
        );
        assert_eq!(
            default_reference("http://h:1/tok/Web-share.tar?x=1"),
            "web:shared"
        );
        assert_eq!(default_reference("plain.tar"), "plain:shared");
    }

    #[test]
    fn what_was_left_out_is_listed_and_env_values_never_appear() {
        let vm = json!({"request": {"env": {"API_TOKEN": "hunter2-very-secret", "MODE": "x"}}});
        let l = excluded(&vm);
        let all = l.join("\n");
        assert!(all.contains("registry logins") && all.contains("API_TOKEN, MODE"));
        assert!(!all.contains("hunter2"));
        assert!(excluded(&json!({})).len() == 3);
    }

    #[test]
    fn the_output_names_the_import_line_and_promises_no_upload() {
        let s = render_file(
            Path::new("/h/a-share.tar"),
            2048,
            "abc",
            "fluxctl import /h/a-share.tar",
            &excluded(&json!({})),
        );
        assert!(s.contains("fluxctl import /h/a-share.tar"));
        assert!(s.contains("Nothing was uploaded") && s.contains("Not included"));
        assert!(s.contains("travels with it"));
        assert_eq!(shell_quote("/a b/c.tar"), "'/a b/c.tar'");
        assert_eq!(shell_quote("/a/c.tar"), "/a/c.tar");
    }

    #[test]
    fn serving_defaults_to_this_machine_and_warns_on_a_network() {
        let lo = render_serving("127.0.0.1", 9, "fluxctl import u", 60);
        assert!(lo.contains("this machine only"));
        let lan = render_serving("0.0.0.0", 9, "fluxctl import u", 60);
        assert!(lan.contains("plain HTTP") && lan.contains("THIS-MAC-ADDRESS"));
        assert!(is_loopback("127.0.0.1") && is_loopback("::1") && !is_loopback("192.168.1.2"));
        assert_ne!(new_token(), new_token());
        assert!(new_token().len() >= 64);
    }

    async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).await.unwrap();
        let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&all[..split]).into_owned();
        let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (code, all[split + 4..].to_vec())
    }

    #[tokio::test]
    async fn the_server_gives_the_bundle_once_to_the_right_token_only() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a-share.tar");
        std::fs::write(&f, b"BUNDLE-BYTES").unwrap();
        crate::mac_imgx::write_sidecar(&f).unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = tokio::spawn(serve_once(
            l,
            "tok123".into(),
            "a-share.tar".into(),
            f.clone(),
            Duration::from_secs(20),
        ));
        assert_eq!(get(addr, "/wrong/a-share.tar").await.0, 404);
        assert_eq!(get(addr, "/tok123/other.tar").await.0, 404);
        assert_eq!(get(addr, "/").await.0, 404);
        let (c, side) = get(addr, "/tok123/a-share.tar.sha256").await;
        assert_eq!(c, 200);
        assert!(
            String::from_utf8(side)
                .unwrap()
                .ends_with("  a-share.tar\n")
        );
        let (c, body) = get(addr, "/tok123/a-share.tar").await;
        assert_eq!((c, body.as_slice()), (200, &b"BUNDLE-BYTES"[..]));
        assert!(srv.await.unwrap().unwrap(), "stops after one download");
    }

    #[tokio::test]
    async fn the_server_gives_up_at_the_timeout() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.tar");
        std::fs::write(&f, b"x").unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let done = serve_once(l, "t".into(), "a.tar".into(), f, Duration::from_millis(100))
            .await
            .unwrap();
        assert!(!done);
    }

    #[tokio::test]
    async fn a_second_download_of_the_same_link_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.tar");
        std::fs::write(&f, b"x").unwrap();
        let used = AtomicBool::new(true);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let client = tokio::spawn(async move { get(addr, "/t/a.tar").await });
        let (sock, _) = l.accept().await.unwrap();
        let r = handle(sock, "t", "a.tar", &f, "", &used).await;
        assert_eq!(r, None);
        assert_eq!(client.await.unwrap().0, 404);
    }

    #[tokio::test]
    async fn import_fetches_verifies_and_loads_what_serve_offers() {
        // build a real exportable archive the same way `mac export` does
        let root = tempfile::tempdir().unwrap();
        let store = crate::mac_store::Store::open(root.path().join("store")).unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let disk = ws.join("disk.raw");
        std::fs::write(&disk, vec![9u8; 2048]).unwrap();
        std::fs::write(ws.join("hardware.bin"), b"hw").unwrap();
        std::fs::write(ws.join("auxiliary.bin"), b"aux").unwrap();
        let out = root.path().join("vm-share.tar");
        crate::mac_imgx::export_files(&store, "vm", &disk, &ws, &out).unwrap();
        crate::mac_imgx::write_sidecar(&out).unwrap();

        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let srv = tokio::spawn(serve_once(
            l,
            "tk".into(),
            "vm-share.tar".into(),
            out.clone(),
            Duration::from_secs(30),
        ));
        let other = crate::mac_store::Store::open(root.path().join("store2")).unwrap();
        let dest = root.path().join("got.tar");
        fetch(&format!("http://{addr}/tk/vm-share.tar"), &dest)
            .await
            .unwrap();
        assert!(verify_sidecar(&dest).unwrap().is_some());
        let key = ops::load(&other, &dest, Some("vm:shared")).unwrap();
        assert_eq!(key, "vm:shared");
        assert!(srv.await.unwrap().unwrap());
        // the link is spent: a second fetch says what to do
        let e = fetch(&format!("http://{addr}/tk/vm-share.tar"), &dest).await;
        let msg = format!("{:#}", e.unwrap_err());
        assert!(msg.contains("explain SHARE_FETCH_FAILED"), "{msg}");
        assert!(!msg.contains("/tk/"), "the token must not be echoed: {msg}");
    }

    #[test]
    fn the_share_codes_are_explained_and_arguments_parse() {
        use clap::Parser;
        assert!(crate::mac_errors::find("SHARE_SERVE_FAILED").len() == 1);
        assert!(crate::mac_errors::find("share-fetch").len() == 1);
        #[derive(Parser)]
        struct P {
            #[command(flatten)]
            a: ShareArgs,
        }
        let p = P::try_parse_from(["x", "vm", "--serve", "--port", "9000"]).unwrap();
        assert!(p.a.serve && p.a.port == 9000 && p.a.bind == "127.0.0.1");
    }
}
