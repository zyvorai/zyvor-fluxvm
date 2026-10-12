// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet ssh`, `tunnel-stdio`, `port-forward` and `tunnels`: SSH and other ports of a VM, through central.
//!
//! Every connection is one WebSocket to central's `GET /fleet/vms/{node}/{id}/tunnel?port=N`, carrying the bytes as
//! binary messages; central relays it to the VM's node, which connects to the guest. Nothing on the Mac needs to be
//! reachable from here. `ssh` runs the system `ssh` with `ProxyCommand` set to `tunnel-stdio`, so host keys, agents,
//! `scp`, `rsync -e` and `ssh_config` all work as usual.

use super::*;
use futures_util::{SinkExt, StreamExt};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The login name used when neither the command line nor the VM's image names one.
const DEFAULT_USER: &str = "admin";
/// Largest read sent as one message.
const CHUNK: usize = 32 * 1024;

#[derive(clap::Subcommand, Debug)]
pub enum TunnelsCommand {
    /// The open tunnels (a tenant sees its own): VM, port, traffic and idle time.
    List,
    /// Close a tunnel now. REST: `DELETE /fleet/tunnels/{id}`.
    Close { id: String },
}

// ---- opening a tunnel -------------------------------------------------------------------------------------------

/// `node/id` as given, or a VM found by name, id or id prefix.
async fn locate(central: &str, token: Option<&str>, vm: &str) -> Result<(String, String, String)> {
    if let Some((node, id)) = vm.split_once('/')
        && !node.is_empty()
        && !id.is_empty()
        && !id.contains('/')
    {
        return Ok((node.to_owned(), id.to_owned(), id.to_owned()));
    }
    resolve_vm(central, token, vm).await
}

fn tunnel_url(central: &str, node: &str, id: &str, port: u16) -> String {
    let base = central
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!(
        "{}/fleet/vms/{}/{}/tunnel?port={port}",
        base.trim_end_matches('/'),
        esc(node),
        esc(id)
    )
}

/// What to do about a refused tunnel.
fn refusal_hint(status: u16) -> Option<&'static str> {
    match status {
        401 => Some("pass a tenant token with --token or FLUXVM_AGENT_TOKEN"),
        403 => Some(
            "a tunnel needs a key that may make changes, to a port the policy allows (see `fluxctl fleet tunnels`)",
        ),
        404 => Some("list your VMs with `fluxctl fleet vms`"),
        409 => Some("the VM has to be running, with something listening on that port"),
        429 => Some("close one with `fluxctl fleet tunnels close <id>`"),
        _ => None,
    }
}

/// Opens one tunnel to `port` of the VM, turning central's refusal into a readable error.
async fn open(central: &str, token: Option<&str>, node: &str, id: &str, port: u16) -> Result<Ws> {
    let url = tunnel_url(central, node, id, port);
    let mut req = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("bad central URL {central}"))?;
    if let Some(t) = token {
        let v: tungstenite::http::HeaderValue = format!("Bearer {t}")
            .parse()
            .context("the token holds characters a header cannot carry")?;
        req.headers_mut().insert("authorization", v);
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => {
            let status = resp.status().as_u16();
            let body = resp.body().as_deref().unwrap_or_default();
            let msg = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
            let mut text = format!("opening a tunnel to port {port} failed ({status}): {msg}");
            if let Some(h) = refusal_hint(status) {
                text.push_str("\nhint: ");
                text.push_str(h);
            }
            bail!(text)
        }
        Err(e) => Err(anyhow::Error::new(e).context(format!(
            "could not reach central at {central} for the tunnel\nhint: is `fluxvm-agent central` running there? Set --central or CENTRAL_URL"
        ))),
    }
}

/// Copies `input` into the tunnel and the tunnel into `output`. When the input ends the tunnel is closed and what
/// is still on its way is written out (for up to 5 seconds); returns central's reason when it closed the tunnel.
async fn pump<R, W>(ws: Ws, mut input: R, mut output: W) -> Option<String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut tx, mut rx) = ws.split();
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match input.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = tx.close().await;
    };
    let down = async {
        while let Some(msg) = rx.next().await {
            let data: Vec<u8> = match msg {
                Ok(Message::Binary(b)) => b.to_vec(),
                Ok(Message::Text(t)) => {
                    let s: &str = &t;
                    s.as_bytes().to_vec()
                }
                Ok(Message::Close(frame)) => {
                    return frame.map(|f| {
                        let r: &str = &f.reason;
                        r.to_owned()
                    });
                }
                Ok(_) => continue,
                Err(_) => break,
            };
            if output.write_all(&data).await.is_err() || output.flush().await.is_err() {
                break;
            }
        }
        None
    };
    tokio::pin!(up);
    tokio::pin!(down);
    tokio::select! {
        _ = &mut up => tokio::time::timeout(Duration::from_secs(5), &mut down).await.ok().flatten(),
        reason = &mut down => reason,
    }
}

/// Reasons central gives for closing a tunnel that the user did not ask for.
fn worth_telling(reason: &str) -> bool {
    !reason.is_empty() && reason != "the client disconnected"
}

// ---- commands -----------------------------------------------------------------------------------------------------

/// `fluxctl fleet tunnel-stdio <vm> <port>`: the tunnel on stdin/stdout, for `ssh -o ProxyCommand`. Exits when the
/// tunnel ends (the process does not wait for a pending stdin read).
pub(super) async fn tunnel_stdio(
    central: &str,
    token: Option<&str>,
    vm: &str,
    port: u16,
) -> Result<()> {
    let (node, id, _) = locate(central, token, vm).await?;
    let ws = open(central, token, &node, &id, port).await?;
    let reason = pump(ws, tokio::io::stdin(), tokio::io::stdout()).await;
    if let Some(r) = reason.filter(|r| worth_telling(r)) {
        eprintln!("fluxctl: tunnel to {vm} port {port} closed: {r}");
    }
    let _ = std::io::stdout().flush();
    std::process::exit(0);
}

/// `ssh` quoting for one ProxyCommand word: single quotes for the shell, `%%` for ssh's own `%` tokens.
fn proxy_word(w: &str) -> String {
    format!("'{}'", w.replace('\'', r"'\''").replace('%', "%%"))
}

/// The `ssh` arguments `fluxctl fleet ssh` runs with (before the user's own).
fn ssh_args(exe: &str, central: &str, node: &str, id: &str, name: &str, user: &str) -> Vec<String> {
    let proxy = format!(
        "{} fleet --central {} tunnel-stdio {} %p",
        proxy_word(exe),
        proxy_word(central),
        proxy_word(&format!("{node}/{id}"))
    );
    vec![
        "-o".into(),
        format!("ProxyCommand={proxy}"),
        // Keyed by the VM's id, so a new VM that reuses a name does not trip over the old one's host key.
        "-o".into(),
        format!("HostKeyAlias=fluxvm-{id}"),
        "-l".into(),
        user.into(),
        name.into(),
    ]
}

/// The login user of the VM's image (`ssh_user` in the catalog), if it has one.
async fn image_user(central: &str, token: Option<&str>, node: &str, id: &str) -> Option<String> {
    let vms = fleet_client::list_vms(central, token).await.ok()?;
    let image = items(&vms)
        .into_iter()
        .find(|v| s(v, "node") == node && s(v, "id") == id)?
        .pointer("/request/image")?
        .as_str()?
        .to_owned();
    let images = get(central, token, "/fleet/images").await.ok()?;
    items(&images)
        .into_iter()
        .find(|i| s(i, "image") == image)
        .map(|i| s(&i, "ssh_user").to_owned())
        .filter(|u| !u.is_empty())
}

/// A login name safe to hand to `ssh -l`.
fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && !u.starts_with('-')
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

/// `fluxctl fleet ssh [user@]<vm> [-- ssh args]`: the system `ssh`, through a tunnel to port 22.
pub(super) async fn ssh(
    central: &str,
    token: Option<&str>,
    target: &str,
    user: Option<String>,
    extra: Vec<String>,
) -> Result<()> {
    let (at_user, vm) = match target.split_once('@') {
        Some((u, v)) => (Some(u.to_owned()), v),
        None => (None, target),
    };
    let (node, id, name) = locate(central, token, vm).await?;
    let user = match user.or(at_user) {
        Some(u) => u,
        None => image_user(central, token, &node, &id)
            .await
            .unwrap_or_else(|| DEFAULT_USER.to_owned()),
    };
    if !valid_user(&user) {
        bail!("'{user}' is not a login name ssh accepts here (letters, digits, '-', '.', '_')");
    }
    let exe = crate::stable_exe::stable_exe()
        .context("finding the fluxctl executable for ProxyCommand")?;
    let exe = exe.to_string_lossy().into_owned();
    let mut args = ssh_args(&exe, central, &node, &id, &name, &user);
    args.extend(extra);
    let mut cmd = std::process::Command::new("ssh");
    cmd.args(&args);
    // The ProxyCommand inherits ssh's environment: the token goes there, never on a command line.
    match token {
        Some(t) => cmd.env("FLUXVM_AGENT_TOKEN", t),
        None => cmd.env_remove("FLUXVM_AGENT_TOKEN"),
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(anyhow::Error::new(err).context(
            "could not run ssh\nhint: install the OpenSSH client, or use `fluxctl fleet tunnel-stdio` as a ProxyCommand",
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().context("could not run ssh")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// `local:remote` or `remote` (the same port locally).
fn parse_ports(spec: &str) -> Result<(u16, u16)> {
    let parse = |p: &str, what: &str| -> Result<u16> {
        match p.trim().parse::<u16>() {
            Ok(n) if n > 0 => Ok(n),
            _ => bail!("{what} port '{p}' is not a number from 1 to 65535"),
        }
    };
    match spec.split_once(':') {
        Some((l, r)) => Ok((parse(l, "local")?, parse(r, "remote")?)),
        None => {
            let p = parse(spec, "remote")?;
            Ok((p, p))
        }
    }
}

/// Whether the policy central reports (`GET /fleet/tunnels`'s `ports`) lets a tunnel reach `port`.
fn policy_allows(policy: &Value, port: u16) -> bool {
    let Some(rules) = policy.get("ports").and_then(Value::as_array) else {
        return true;
    };
    rules.iter().filter_map(Value::as_str).any(|r| {
        let (a, b) = r.split_once('-').unwrap_or((r, r));
        match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
            (Ok(a), Ok(b)) => (a..=b).contains(&port),
            _ => false,
        }
    })
}

/// `fluxctl fleet port-forward <vm> <local>:<remote>`: a local listener; every connection to it gets its own tunnel.
pub(super) async fn port_forward(
    central: &str,
    token: Option<&str>,
    out: &Out,
    vm: &str,
    spec: &str,
    address: &str,
) -> Result<()> {
    let (local, remote) = parse_ports(spec)?;
    let ip: IpAddr = address
        .parse()
        .with_context(|| format!("--address '{address}' is not an IP address"))?;
    let (node, id, name) = locate(central, token, vm).await?;
    // Fail now rather than on the first connection when the policy closes the port.
    if let Ok(p) = get(central, token, "/fleet/tunnels").await
        && (p.get("enabled") == Some(&Value::Bool(false)) || !policy_allows(&p, remote))
    {
        bail!(
            "port {remote} is not open to tunnels in this cloud (allowed: {})",
            p.get("ports")
                .and_then(Value::as_array)
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
    }
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, local))
        .await
        .with_context(|| format!("listening on {ip}:{local}"))?;
    let bound = listener.local_addr()?;
    if out.human {
        out.done(format!(
            "Forwarding {} to {name} port {remote}, through central. Press Ctrl-C to stop.",
            out.bold(bound)
        ));
    } else {
        out.json(&json!({"listening": bound.to_string(), "node": node, "id": id, "port": remote}))?;
    }
    let central = central.to_owned();
    let token = token.map(str::to_owned);
    loop {
        let (conn, peer) = tokio::select! {
            r = listener.accept() => r.context("accepting a connection")?,
            _ = tokio::signal::ctrl_c() => break,
        };
        let (central, token, node, id, name) = (
            central.clone(),
            token.clone(),
            node.clone(),
            id.clone(),
            name.clone(),
        );
        tokio::spawn(async move {
            let _ = conn.set_nodelay(true);
            match open(&central, token.as_deref(), &node, &id, remote).await {
                Ok(ws) => {
                    let (r, w) = conn.into_split();
                    if let Some(reason) = pump(ws, r, w).await.filter(|r| worth_telling(r)) {
                        eprintln!("{peer}: tunnel to {name} port {remote} closed: {reason}");
                    }
                }
                Err(e) => eprintln!("{peer}: {e:#}"),
            }
        });
    }
    if out.human {
        println!();
        out.done("Stopped forwarding.");
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `fluxctl fleet tunnels [list|close <id>]`.
pub(super) async fn tunnels(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: Option<TunnelsCommand>,
) -> Result<()> {
    match command.unwrap_or(TunnelsCommand::List) {
        TunnelsCommand::List => {
            let v = get(central, token, "/fleet/tunnels").await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("No tunnels are open.");
                out.next("fluxctl fleet ssh <vm>   or   fluxctl fleet port-forward <vm> 8080:80");
                return Ok(());
            }
            let op = list.iter().any(|t| !s(t, "tenant").is_empty());
            let mut headers = vec!["ID", "VM", "PORT"];
            if op {
                headers.push("TENANT");
            }
            headers.extend(["OPENED", "IDLE", "SENT", "RECEIVED"]);
            let mut table = Table::new(headers);
            for t in &list {
                let mut row = vec![
                    c(s(t, "id")),
                    c(format!("{} ({})", s(t, "vm_name"), s(t, "node"))),
                    c(u(t, "port")),
                ];
                if op {
                    let tenant = s(t, "tenant");
                    row.push(if tenant.is_empty() {
                        ct("operator", Tone::Dim)
                    } else {
                        c(tenant)
                    });
                }
                row.push(c(time(t, "opened_at").map(rel).unwrap_or_default()));
                row.push(c(span(u(t, "idle_seconds") as i64)));
                row.push(c(bytes(u(t, "bytes_sent"))));
                row.push(c(bytes(u(t, "bytes_received"))));
                table.row(row);
            }
            table.print(out);
            println!(
                "{}",
                out.dim(format!(
                    "{} open; at most {} per tenant; closed after {} minutes without traffic.",
                    list.len(),
                    u(&v, "max_per_tenant"),
                    u(&v, "idle_timeout_minutes")
                ))
            );
            Ok(())
        }
        TunnelsCommand::Close { id } => {
            call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/tunnels/{}", esc(&id)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&json!({"closed": true, "id": id}));
            }
            out.done(format!("Closed tunnel {id}."));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_command_line() {
        let a = ssh_args(
            "/Applications/Flux VM/fluxctl",
            "https://cloud.example.com",
            "studio-1",
            "4f1c",
            "web-1",
            "admin",
        );
        assert_eq!(
            a[1],
            "ProxyCommand='/Applications/Flux VM/fluxctl' fleet --central 'https://cloud.example.com' tunnel-stdio \
             'studio-1/4f1c' %p"
        );
        assert_eq!(a[3], "HostKeyAlias=fluxvm-4f1c");
        assert_eq!(&a[4..], ["-l", "admin", "web-1"]);
        // Quotes and ssh's % tokens in a path cannot break out of the word.
        assert_eq!(proxy_word("/tmp/it's 100%"), r"'/tmp/it'\''s 100%%'");
    }

    #[test]
    fn ports_and_policy() {
        assert_eq!(parse_ports("8080:80").unwrap(), (8080, 80));
        assert_eq!(parse_ports("5432").unwrap(), (5432, 5432));
        assert!(parse_ports("0:80").is_err());
        assert!(parse_ports("80:x").is_err());
        assert!(parse_ports("70000").is_err());
        let p = json!({"ports": ["22", "1024-65535"]});
        assert!(policy_allows(&p, 22) && policy_allows(&p, 8080));
        assert!(!policy_allows(&p, 80));
        assert!(
            policy_allows(&json!({}), 80),
            "no policy known: let central decide"
        );
    }

    #[test]
    fn urls_users_and_sizes() {
        assert_eq!(
            tunnel_url("http://127.0.0.1:7799/", "mac 1", "ab-12", 22),
            "ws://127.0.0.1:7799/fleet/vms/mac%201/ab-12/tunnel?port=22"
        );
        assert!(tunnel_url("https://c.example", "m", "i", 2222).starts_with("wss://c.example/"));
        assert!(valid_user("ec2-user") && valid_user("admin"));
        assert!(!valid_user("-oProxyCommand=x") && !valid_user("a b") && !valid_user(""));
        assert_eq!(bytes(1536), "1.5 KiB");
        assert!(worth_telling("idle timeout") && !worth_telling("the client disconnected"));
        assert_eq!(refusal_hint(429).map(|h| h.contains("close")), Some(true));
    }

    #[tokio::test]
    async fn pump_relays_and_reports_the_close_reason() {
        use tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};
        // A server that echoes one message, then closes with a reason.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut s = tokio_tungstenite::accept_async(tcp).await.unwrap();
            if let Some(Ok(Message::Binary(b))) = s.next().await {
                let _ = s.send(Message::Binary(b)).await;
            }
            let _ = s
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "idle timeout".into(),
                })))
                .await;
            while let Some(Ok(_)) = s.next().await {}
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/t"))
            .await
            .unwrap();
        // The input stays open (nobody writes to it again), so only the server ends the tunnel.
        let (mut feed, input) = tokio::io::duplex(1024);
        feed.write_all(b"ping").await.unwrap();
        let mut output = Vec::new();
        let reason = pump(ws, input, &mut output).await;
        assert_eq!(output, b"ping");
        assert_eq!(reason.as_deref(), Some("idle timeout"));
        drop(feed);
    }
}
