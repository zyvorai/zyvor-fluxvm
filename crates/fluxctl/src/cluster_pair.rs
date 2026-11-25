// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl cluster pair | pending | approve | deny`: add a machine with a short code instead of a URL and a token.
//! The protocol, the code and the server's rules are `fluxvm_core::pairing`; discovery is `fluxvm_core::mdns` behind the
//! `Browser` trait. This file is the machine's side (find the fleet, ask, show the code, wait, store the credential) and
//! the operator's small CLI. docs/cluster-pairing.md has the protocol and the threat model.
//!
//! Keys never appear in argv: the signing seed lives in `<state-dir>/pair-key` (0600) and the credential that comes back
//! is written to `<state-dir>/node-credential` (0600), where `fluxvm-agent node` already looks for it.

use anyhow::{Context, Result, bail};
use fluxvm_core::mdns::{self, Browser, Found};
use fluxvm_core::pairing::{NodeKeys, hex, unhex};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 7799;

#[derive(clap::Subcommand)]
pub enum ClusterCommand {
    /// On the machine that wants to join: find the fleet on the LAN, show a short code, and wait for someone to approve it.
    Pair(PairArgs),
    /// On the server: machines asking to join, with the code each one shows.
    Pending(ServerArgs),
    /// On the server: let a machine join. Give the code its screen shows.
    Approve {
        /// The code on the machine's screen (case, dashes and spaces do not matter).
        code: String,
        #[command(flatten)]
        server: ServerArgs,
    },
    /// On the server: refuse a machine that asked to join.
    Deny {
        code: String,
        #[command(flatten)]
        server: ServerArgs,
    },
    /// One shareable document about this cluster: machines, versions, health, how to add one, back up, undo, logs.
    Report(crate::cluster_report::ReportArgs),
}

#[derive(clap::Args, Clone, Debug)]
pub struct ServerArgs {
    /// The fleet server. Default: CENTRAL_URL, else http://127.0.0.1:7799 (run this on the server).
    #[arg(long, env = "CENTRAL_URL")]
    pub server: Option<String>,
    /// File holding the operator token. Default: FLUXVM_AGENT_TOKEN, else the server's own `operator-token`
    /// (/var/lib/fluxvm-agent/operator-token). There is no flag that takes the token itself.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
}

#[derive(clap::Args, Clone, Debug)]
pub struct PairArgs {
    /// Skip discovery and ask this server (host, host:port or URL). Needed only when the LAN blocks multicast.
    #[arg(long)]
    pub server: Option<String>,
    /// Pair with the fleet of this name when several are found.
    #[arg(long)]
    pub fleet: Option<String>,
    /// This machine's name in the fleet (lowercase letters, digits, dashes and dots). Default: its host name.
    #[arg(long)]
    pub name: Option<String>,
    /// Do not ask before contacting the fleet that was found.
    #[arg(long)]
    pub yes: bool,
    /// Where the node keeps its key, id and credential. Default: ~/Library/Application Support/FluxVM/agent on a Mac,
    /// /var/lib/fluxvm-agent as root, else ~/.local/share/fluxvm/agent.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// Seconds to look for a fleet on the LAN.
    #[arg(long, default_value = "4")]
    pub discover_secs: u64,
    /// After approval, install the node agent as a LaunchAgent (macOS) instead of just printing the command.
    #[arg(long)]
    pub install: bool,
}

pub fn default_state_dir(home: &Path, root: bool) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/FluxVM/agent")
    } else if root {
        PathBuf::from("/var/lib/fluxvm-agent")
    } else {
        home.join(".local/share/fluxvm/agent")
    }
}

/// `host`, `host:port` or a URL as a base URL; the default port is central's.
pub fn normalize_server(s: &str) -> Result<String> {
    let s = s.trim().trim_end_matches('/');
    let (scheme, rest) = match s.split_once("://") {
        Some((sc @ ("http" | "https"), r)) => (sc, r),
        Some(_) => bail!("the server must be http:// or https://"),
        None => ("http", s),
    };
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('@')
        || rest.chars().any(char::is_whitespace)
    {
        bail!("{s:?} is not a host, host:port or URL");
    }
    let has_port = rest
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok());
    Ok(if has_port {
        format!("{scheme}://{rest}")
    } else {
        format!("{scheme}://{rest}:{DEFAULT_PORT}")
    })
}

pub fn url_of(f: &Found) -> Option<String> {
    let ip = f.addrs.first()?;
    let scheme = match f.txt.get("scheme").map(String::as_str) {
        Some("https") => "https",
        _ => "http",
    };
    Some(format!("{scheme}://{ip}:{}", f.port))
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct OfferResp {
    pub id: String,
    pub server_pub: String,
    pub expires_in: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClaimResp {
    Pending,
    Denied,
    Approved(Vec<u8>),
    /// Retry after this many seconds.
    Slow(u64),
    /// The request is gone (expired or already collected).
    Gone,
}

/// The two public calls; faked in tests.
pub trait Transport {
    fn request(&self, name: &str, ed_pub: &str, x_pub: &str) -> Result<OfferResp>;
    fn claim(&self, id: &str, sig: &str) -> Result<ClaimResp>;
}

pub struct Http {
    pub base: String,
    pub client: reqwest::Client,
}

impl Http {
    pub fn new(base: String) -> Result<Http> {
        Ok(Http {
            base,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(u16, serde_json::Value, Option<u64>)> {
        let url = format!("{}{path}", self.base);
        let c = self.client.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let r = c
                    .post(&url)
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("contacting {url}"))?;
                let st = r.status().as_u16();
                let retry = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok());
                let v = r
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null);
                Ok::<_, anyhow::Error>((st, v, retry))
            })
        })
    }
}

impl Transport for Http {
    fn request(&self, name: &str, ed_pub: &str, x_pub: &str) -> Result<OfferResp> {
        let (st, v, _) = self.post(
            "/fleet/pair/request",
            serde_json::json!({"name": name, "ed_pub": ed_pub, "x_pub": x_pub}),
        )?;
        match st {
            200 => Ok(serde_json::from_value(v)?),
            404 => bail!(
                "that server is not accepting pairing (start it with `fluxvm-agent central --pairing`)"
            ),
            429 => bail!("the server has too many open requests; try again in a minute"),
            _ => bail!(
                "the server refused ({st}): {}",
                v["error"].as_str().unwrap_or("no reason given")
            ),
        }
    }
    fn claim(&self, id: &str, sig: &str) -> Result<ClaimResp> {
        let (st, v, retry) = self.post(
            "/fleet/pair/claim",
            serde_json::json!({"id": id, "sig": sig}),
        )?;
        Ok(match (st, v["state"].as_str()) {
            (200, Some("pending")) => ClaimResp::Pending,
            (200, Some("denied")) => ClaimResp::Denied,
            (200, Some("approved")) => {
                let hexs = v["sealed"]
                    .as_str()
                    .context("the server sent no credential")?;
                if hexs.len() % 2 != 0 || !hexs.is_ascii() {
                    bail!("the server sent a malformed credential");
                }
                ClaimResp::Approved(
                    (0..hexs.len() / 2)
                        .map(|i| u8::from_str_radix(&hexs[i * 2..i * 2 + 2], 16))
                        .collect::<Result<_, _>>()?,
                )
            }
            (429, _) => ClaimResp::Slow(retry.unwrap_or(5).clamp(1, 60)),
            (404 | 409 | 410, _) => ClaimResp::Gone,
            _ => bail!(
                "the server refused ({st}): {}",
                v["error"].as_str().unwrap_or("no reason given")
            ),
        })
    }
}

/// What the person sees and what time it is; faked in tests.
pub trait Io {
    fn say(&mut self, s: &str);
    fn confirm(&mut self, q: &str) -> bool;
    fn sleep(&mut self, d: Duration);
    fn now(&self) -> i64;
}

pub struct Term;
impl Io for Term {
    fn say(&mut self, s: &str) {
        println!("{s}");
    }
    fn confirm(&mut self, q: &str) -> bool {
        use std::io::{BufRead, IsTerminal, Write};
        if !std::io::stdin().is_terminal() {
            return false;
        }
        print!("{q} [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut l = String::new();
        std::io::stdin().lock().read_line(&mut l).is_ok()
            && !l.trim().to_lowercase().starts_with('n')
    }
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(d) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(d)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_private(path: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            if m.mode() & 0o077 != 0 {
                bail!(
                    "{} is readable by group or others; chmod 600 it",
                    path.display()
                );
            }
            Ok(Some(std::fs::read_to_string(path)?.trim().to_owned()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The machine's identity: the seed kept in `pair-key`, made on first use.
pub fn load_keys(dir: &Path) -> Result<NodeKeys> {
    let p = dir.join("pair-key");
    let seed = match read_private(&p)? {
        Some(h) => {
            Some(unhex::<32>(&h).context("pair-key is damaged; delete it to make a new identity")?)
        }
        None => None,
    };
    let keys = NodeKeys::new(seed);
    if seed.is_none() {
        write_private(&p, &hex(&keys.seed()))?;
    }
    Ok(keys)
}

/// Picks the server: the one given, else the one found on the LAN.
pub fn choose_server(
    a: &PairArgs,
    browser: &dyn Browser,
    io: &mut dyn Io,
) -> Result<(String, String)> {
    if let Some(s) = &a.server {
        let u = normalize_server(s)?;
        return Ok((u.clone(), u));
    }
    io.say("Looking for a FluxVM fleet on this network...");
    let mut found = browser.browse(mdns::PAIR_SERVICE, Duration::from_secs(a.discover_secs));
    found.retain(|f| url_of(f).is_some());
    if let Some(want) = &a.fleet {
        found.retain(|f| f.instance.eq_ignore_ascii_case(want));
    }
    match found.len() {
        0 => bail!(
            "no fleet answered. Either none is accepting new machines (start the server with `fluxvm-agent central --pairing`) or this network blocks multicast. \
             Ask the server's owner for its address and run `fluxctl cluster pair --server ADDRESS`."
        ),
        1 => {
            let f = &found[0];
            let url = url_of(f).expect("filtered");
            let q = format!(
                "Found the fleet \"{}\" at {url}. Ask to join it?",
                f.instance
            );
            if !a.yes && !io.confirm(&q) {
                bail!(
                    "not contacting {url}; run again with --yes if that is the fleet you mean, or name it with --fleet"
                );
            }
            Ok((url, f.instance.clone()))
        }
        _ => {
            let list = found
                .iter()
                .map(|f| format!("  \"{}\" at {}", f.instance, url_of(f).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            bail!("more than one fleet answered; pick one with --fleet NAME:\n{list}")
        }
    }
}

#[derive(Debug)]
pub struct Paired {
    pub name: String,
    pub node_id: String,
    pub state_dir: PathBuf,
}

/// The whole machine-side flow. Returns once the credential is stored.
pub fn pair(t: &dyn Transport, name: &str, dir: &Path, io: &mut dyn Io) -> Result<Paired> {
    if !fluxvm_core::pairing::valid_name(name) {
        bail!(
            "{name:?} cannot be a node name: use lowercase letters, digits, dashes and dots (--name)"
        );
    }
    let keys = load_keys(dir)?;
    if let Some(existing) =
        read_private(&dir.join("node-credential"))?.filter(|c| c.starts_with("fvnc_"))
    {
        let _ = existing;
        bail!(
            "this machine already has a credential in {}; remove node-credential there to pair again",
            dir.display()
        );
    }
    let offer = t.request(name, &hex(&keys.ed_pub()), &hex(&keys.x_pub()))?;
    let sp: [u8; 32] = unhex(&offer.server_pub).context("the server sent a bad key")?;
    let code = keys.code(&offer.id, &sp);
    io.say(&format!(
        "\n  This machine is asking to join as \"{name}\".\n\n      Your code:  {code}\n\n  On the server, run `fluxctl cluster pending` (or open Studio) and approve the request that shows this exact code.\n  If the code differs, do not approve it. It expires in {} minutes.\n",
        (offer.expires_in + 59) / 60
    ));
    let deadline = io.now() + offer.expires_in;
    let mut wait;
    loop {
        if io.now() >= deadline {
            bail!("nobody approved the request in time; run `fluxctl cluster pair` again");
        }
        match t.claim(&offer.id, &keys.claim_signature(&offer.id, &sp))? {
            ClaimResp::Pending => wait = 2,
            ClaimResp::Slow(s) => wait = s,
            ClaimResp::Denied => bail!("the request was denied"),
            ClaimResp::Gone => {
                bail!("the request expired or was already used; run `fluxctl cluster pair` again")
            }
            ClaimResp::Approved(sealed) => {
                let node_id = keys.node_id();
                let cred = keys
                    .open_credential(&offer.id, &sp, &sealed)
                    .filter(|c| c.starts_with("fvnc_"))
                    .context(
                        "the credential could not be opened: it was not sealed for this machine",
                    )?;
                write_private(&dir.join("node-id"), &node_id)?;
                write_private(&dir.join("node-credential"), &cred)?;
                io.say("Approved. This machine has its credential.");
                return Ok(Paired {
                    name: name.to_owned(),
                    node_id,
                    state_dir: dir.to_path_buf(),
                });
            }
        }
        io.sleep(Duration::from_secs(wait));
    }
}

/// The line that starts the node agent with what pairing stored; no token, no URL typed.
pub fn next_step(p: &Paired, central: &str) -> String {
    format!(
        "fluxctl node-agent install --name {} --central {} --state-dir '{}'",
        p.name,
        central,
        p.state_dir.display()
    )
}

pub async fn run_pair(a: PairArgs) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let dir = a
        .state_dir
        .clone()
        .unwrap_or_else(|| default_state_dir(&home, unsafe { libc::geteuid() } == 0));
    let name = match &a.name {
        Some(n) => n.clone(),
        None => host_name(),
    };
    let mut term = Term;
    let (base, fleet) =
        tokio::task::block_in_place(|| choose_server(&a, &mdns::UdpBrowser::default(), &mut term))?;
    let http = Http::new(base.clone())?;
    let p = tokio::task::block_in_place(|| pair(&http, &name, &dir, &mut term))?;
    println!("\nJoined \"{fleet}\" as {}.", p.name);
    let line = next_step(&p, &base);
    if a.install {
        #[cfg(target_os = "macos")]
        {
            let args = crate::node_agent::NodeAgentArgs {
                name: p.name.clone(),
                central: base.clone(),
                fluxvm_url: "http://127.0.0.1:7788".into(),
                advertise_url: None,
                token_file: None,
                labels: vec![],
                no_auto_labels: false,
                no_overlay_sync: false,
                state_dir: Some(p.state_dir.clone()),
            };
            args.validate()?;
            let plist = crate::node_agent::install(&args)?;
            println!("Installed the node agent: {plist:?}");
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        println!(
            "--install is for macOS; on Linux start the agent with the line below (or the installer's service)."
        );
    }
    println!(
        "Start it with:\n  {line}\n(on Linux: fluxvm-agent node --name {} --central {base} --state-dir '{}')",
        p.name,
        p.state_dir.display()
    );
    Ok(())
}

fn host_name() -> String {
    let raw = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let n: String = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let n = n.trim_matches('-').to_owned();
    if n.is_empty() { "machine".into() } else { n }
}

fn operator_token(a: &ServerArgs) -> Result<String> {
    if let Some(f) = &a.token_file {
        return read_private(f)?.with_context(|| format!("{} does not exist", f.display()));
    }
    if let Ok(t) = std::env::var("FLUXVM_AGENT_TOKEN")
        && !t.is_empty()
    {
        return Ok(t);
    }
    let p = Path::new("/var/lib/fluxvm-agent/operator-token");
    read_private(p)
        .ok()
        .flatten()
        .context("no operator token: run this on the server as the user that runs central, or pass --token-file")
}

async fn admin(a: &ServerArgs, method: reqwest::Method, path: &str) -> Result<serde_json::Value> {
    let base = normalize_server(a.server.as_deref().unwrap_or("http://127.0.0.1:7799"))?;
    let token = operator_token(a)?;
    let r = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(token)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("contacting {base}"))?;
    let st = r.status();
    let v: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
    if !st.is_success() {
        bail!(
            "{}",
            v["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("the server answered {st}"))
        );
    }
    Ok(v)
}

/// GET with the operator token (also used by the report).
pub async fn admin_get(a: &ServerArgs, path: &str) -> Result<serde_json::Value> {
    admin(a, reqwest::Method::GET, path).await
}

pub fn pending_table(v: &serde_json::Value) -> String {
    let items = v["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return if v["enabled"] == false {
            "Pairing is off on this server. Start central with `--pairing`.\n".into()
        } else {
            "No machines are asking to join.\n".into()
        };
    }
    let mut s = format!(
        "{:<13} {:<20} {:<16} {}\n",
        "CODE", "NAME", "FROM", "EXPIRES IN"
    );
    for i in items {
        s.push_str(&format!(
            "{:<13} {:<20} {:<16} {}s\n",
            i["code"].as_str().unwrap_or("?"),
            i["name"].as_str().unwrap_or("?"),
            i["address"].as_str().unwrap_or("?"),
            i["expires_in"].as_i64().unwrap_or(0)
        ));
    }
    s.push_str(
        "\nApprove the one whose code matches the machine's screen: fluxctl cluster approve CODE\n",
    );
    s
}

pub async fn run(cmd: ClusterCommand) -> Result<()> {
    match cmd {
        ClusterCommand::Pair(a) => run_pair(a).await,
        ClusterCommand::Pending(s) => {
            print!(
                "{}",
                pending_table(&admin(&s, reqwest::Method::GET, "/fleet/pair/pending").await?)
            );
            Ok(())
        }
        ClusterCommand::Approve { code, server } => {
            let v = admin(
                &server,
                reqwest::Method::POST,
                &format!("/fleet/pair/{}/approve", enc(&code)),
            )
            .await?;
            println!(
                "Approved {}. The machine receives its credential in a few seconds.",
                v["approved"].as_str().unwrap_or("it")
            );
            Ok(())
        }
        ClusterCommand::Deny { code, server } => {
            admin(
                &server,
                reqwest::Method::POST,
                &format!("/fleet/pair/{}/deny", enc(&code)),
            )
            .await?;
            println!("Denied.");
            Ok(())
        }
        ClusterCommand::Report(a) => crate::cluster_report::run(a).await,
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::pairing::{Claim, PairBook, PairConfig};
    use std::cell::RefCell;

    /// An in-process server: the real state machine behind the `Transport` trait.
    struct Local {
        book: RefCell<PairBook>,
        clock: RefCell<i64>,
        approve_after: RefCell<Option<usize>>,
        claims: RefCell<usize>,
        deny: bool,
        approved_code: RefCell<Option<String>>,
    }

    impl Local {
        fn new() -> Self {
            Local {
                book: RefCell::new(PairBook::new(PairConfig {
                    claim_interval_secs: 0,
                    ..Default::default()
                })),
                clock: RefCell::new(1_000),
                approve_after: RefCell::new(Some(2)),
                claims: RefCell::new(0),
                deny: false,
                approved_code: RefCell::new(None),
            }
        }
    }

    impl Transport for Local {
        fn request(&self, name: &str, e: &str, x: &str) -> Result<OfferResp> {
            let o = self
                .book
                .borrow_mut()
                .request(*self.clock.borrow(), "10.0.0.2", name, e, x)
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            Ok(OfferResp {
                id: o.id,
                server_pub: o.server_pub,
                expires_in: o.expires_in,
            })
        }
        fn claim(&self, id: &str, sig: &str) -> Result<ClaimResp> {
            let n = {
                *self.claims.borrow_mut() += 1;
                *self.claims.borrow()
            };
            if *self.approve_after.borrow() == Some(n) {
                let now = *self.clock.borrow();
                let mut b = self.book.borrow_mut();
                let code = b.pending(now)[0].code.clone();
                *self.approved_code.borrow_mut() = Some(code.clone());
                if self.deny {
                    b.deny(now, &code).unwrap();
                } else {
                    b.approve(
                        now,
                        &code.to_lowercase(),
                        |_, _| Ok("fvnc_abc.1.def".into()),
                    )
                    .unwrap();
                }
            }
            let r = self
                .book
                .borrow_mut()
                .claim(*self.clock.borrow(), "10.0.0.2", id, sig);
            Ok(match r {
                Ok(Claim::Pending) => ClaimResp::Pending,
                Ok(Claim::Denied) => ClaimResp::Denied,
                Ok(Claim::Approved(s)) => ClaimResp::Approved(s),
                Err(_) => ClaimResp::Gone,
            })
        }
    }

    struct Script<'a> {
        said: Vec<String>,
        answer: bool,
        clock: &'a RefCell<i64>,
    }
    impl Io for Script<'_> {
        fn say(&mut self, s: &str) {
            self.said.push(s.to_owned());
        }
        fn confirm(&mut self, q: &str) -> bool {
            self.said.push(format!("ASK {q}"));
            self.answer
        }
        fn sleep(&mut self, d: Duration) {
            *self.clock.borrow_mut() += d.as_secs() as i64;
        }
        fn now(&self) -> i64 {
            *self.clock.borrow()
        }
    }

    #[test]
    fn a_machine_pairs_and_stores_a_private_credential_with_nothing_typed() {
        use std::os::unix::fs::MetadataExt;
        let srv = Local::new();
        let dir = tempfile::tempdir().unwrap();
        let mut io = Script {
            said: vec![],
            answer: true,
            clock: &srv.clock,
        };
        let p = pair(&srv, "mini-2", dir.path(), &mut io).unwrap();
        let cred = std::fs::read_to_string(dir.path().join("node-credential")).unwrap();
        assert_eq!(cred, "fvnc_abc.1.def");
        for f in ["node-credential", "node-id", "pair-key"] {
            assert_eq!(
                std::fs::metadata(dir.path().join(f)).unwrap().mode() & 0o777,
                0o600,
                "{f}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("node-id")).unwrap(),
            p.node_id
        );
        // The code on this screen is the one the server's operator was shown.
        let shown = io.said.join("\n");
        assert!(
            shown.contains(srv.approved_code.borrow().as_deref().unwrap()),
            "{shown}"
        );
        // The same identity again: the key is kept, so the node id is stable.
        assert_eq!(load_keys(dir.path()).unwrap().node_id(), p.node_id);
        // A machine that already has a credential does not pair twice.
        assert!(pair(&srv, "mini-2", dir.path(), &mut io).is_err());
        // The next step carries no secret.
        let line = next_step(&p, "http://10.0.0.1:7799");
        assert!(!line.contains("fvnc_") && line.contains("--state-dir"));
    }

    #[test]
    fn a_denied_request_stores_nothing() {
        let mut srv = Local::new();
        srv.deny = true;
        let dir = tempfile::tempdir().unwrap();
        let mut io = Script {
            said: vec![],
            answer: true,
            clock: &srv.clock,
        };
        let e = pair(&srv, "mini-2", dir.path(), &mut io)
            .unwrap_err()
            .to_string();
        assert!(e.contains("denied"), "{e}");
        assert!(!dir.path().join("node-credential").exists());
    }

    #[test]
    fn nobody_approving_ends_with_a_clear_message() {
        let srv = Local::new();
        *srv.approve_after.borrow_mut() = None;
        let dir = tempfile::tempdir().unwrap();
        let mut io = Script {
            said: vec![],
            answer: true,
            clock: &srv.clock,
        };
        let e = pair(&srv, "mini-2", dir.path(), &mut io)
            .unwrap_err()
            .to_string();
        assert!(e.contains("in time") || e.contains("expired"), "{e}");
        assert!(!dir.path().join("node-credential").exists());
    }

    #[test]
    fn a_bad_name_is_refused_before_anything_is_sent() {
        let srv = Local::new();
        let dir = tempfile::tempdir().unwrap();
        let mut io = Script {
            said: vec![],
            answer: true,
            clock: &srv.clock,
        };
        assert!(pair(&srv, "Mini 2", dir.path(), &mut io).is_err());
        assert!(srv.book.borrow_mut().pending(1_000).is_empty());
        assert!(!dir.path().join("pair-key").exists());
    }

    fn found(name: &str, ip: [u8; 4]) -> Found {
        Found {
            instance: name.into(),
            host: "h.local".into(),
            port: 7799,
            addrs: vec![std::net::Ipv4Addr::from(ip)],
            txt: Default::default(),
        }
    }

    fn args(fleet: Option<&str>, yes: bool) -> PairArgs {
        PairArgs {
            server: None,
            fleet: fleet.map(str::to_owned),
            name: None,
            yes,
            state_dir: None,
            discover_secs: 0,
            install: false,
        }
    }

    #[test]
    fn discovery_asks_before_contacting_and_handles_none_or_many() {
        let clock = RefCell::new(0);
        let mut io = Script {
            said: vec![],
            answer: false,
            clock: &clock,
        };
        let one = mdns::FakeBrowser(vec![found("Home Lab", [192, 168, 1, 20])]);
        // Declining means nothing is sent.
        assert!(choose_server(&args(None, false), &one, &mut io).is_err());
        io.answer = true;
        assert_eq!(
            choose_server(&args(None, false), &one, &mut io).unwrap().0,
            "http://192.168.1.20:7799"
        );
        assert_eq!(
            choose_server(
                &args(None, true),
                &one,
                &mut Script {
                    said: vec![],
                    answer: false,
                    clock: &clock
                }
            )
            .unwrap()
            .1,
            "Home Lab"
        );
        let none = mdns::FakeBrowser(vec![]);
        let e = choose_server(&args(None, true), &none, &mut io)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--server") && e.contains("--pairing"), "{e}");
        let two = mdns::FakeBrowser(vec![found("A", [10, 0, 0, 1]), found("B", [10, 0, 0, 2])]);
        let e = choose_server(&args(None, true), &two, &mut io)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--fleet") && e.contains("10.0.0.2"), "{e}");
        assert_eq!(
            choose_server(&args(Some("b"), true), &two, &mut io)
                .unwrap()
                .0,
            "http://10.0.0.2:7799"
        );
    }

    #[test]
    fn server_addresses_are_normalised_and_junk_is_refused() {
        assert_eq!(
            normalize_server("10.0.0.5").unwrap(),
            "http://10.0.0.5:7799"
        );
        assert_eq!(
            normalize_server("mini.local:8000").unwrap(),
            "http://mini.local:8000"
        );
        assert_eq!(
            normalize_server("https://c.example/").unwrap(),
            "https://c.example:7799"
        );
        assert_eq!(
            normalize_server("https://c.example:443").unwrap(),
            "https://c.example:443"
        );
        assert!(normalize_server("ftp://x").is_err());
        assert!(normalize_server("http://u:p@x").is_err());
        assert!(normalize_server("a b").is_err());
        assert!(normalize_server("").is_err());
    }

    #[test]
    fn the_pending_table_shows_code_name_and_where_from() {
        let v = serde_json::json!({"enabled": true, "items": [{"code": "ABCDE-FGHJK", "name": "mini-2", "address": "10.0.0.2", "expires_in": 540}]});
        let t = pending_table(&v);
        assert!(
            t.contains("ABCDE-FGHJK")
                && t.contains("mini-2")
                && t.contains("10.0.0.2")
                && t.contains("cluster approve")
        );
        assert!(
            pending_table(&serde_json::json!({"enabled": false, "items": []}))
                .contains("--pairing")
        );
        assert!(
            pending_table(&serde_json::json!({"enabled": true, "items": []}))
                .contains("No machines")
        );
    }
}
