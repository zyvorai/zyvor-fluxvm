// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl cluster`: the k3s-style front door to FluxVM's own fleet. One machine runs `init` (the fleet central as a
//! service, plus a short-lived join token); every other machine runs `join URL`; `nodes`, `token`, `remove` and `add`
//! manage it afterwards. k3s is an optional add-on (`addon k3s`), never a dependency.
//!
//! Nothing here is a new API. It speaks the fleet central's existing one (docs/fleet-enroll.md): `POST|GET
//! /fleet/enroll/tokens`, `DELETE /fleet/enroll/tokens/{id}`, `GET /fleet/enroll/nodes`, `GET /fleet/nodes`,
//! `POST /fleet/nodes/{name}/revoke`, `DELETE /fleet/nodes/{name}` and `GET /fleet/nodes/{name}/overlays` (a node's own
//! credential check), through `fleet_client`. On Linux `init` and `join` hand over to `share/install.sh` from the same
//! package (one implementation of package installs, services and sudo handling); on macOS they are native LaunchAgents.
//!
//! Secrets: the operator token comes from `--token-file`, `$FLUXVM_AGENT_TOKEN` or the file central keeps; a join token
//! from `--token-file`, `$FLUXVM_TOKEN` or stdin. Neither is ever an argument.

// Some helpers are used by one operating system's code path only (and by the tests).
#![allow(dead_code)]

use crate::cluster_remote::{self as remote, DEFAULT_INSTALLER_URL, JoinSpec};
use crate::fleet_client;
use crate::mac_fleet::{Conn, HostResult, Which, blank, fail, finish, jobs, par_map, resolve, transport};
use crate::mac_fleet_remote::{Level, Secret, remote as sh_c, scrub};
use crate::mac_fleet_ssh::Target;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 7799;
pub const CENTRAL_LABEL: &str = "dev.zyvor.fluxvm-central";

#[derive(clap::Subcommand)]
pub enum ClusterCommand {
    /// How the cluster is doing, in plain English: a green, yellow or red light for each machine and for the whole
    /// cluster, how much room is left, the last backup, and the next best thing to do. Read-only; works with the daemon down.
    Status(crate::cluster_cmd::StatusArgs),
    /// Add machines with a short code instead of a URL and a token, and see the cluster in one document:
    /// `cluster pair` (on the new machine), `cluster pending` / `approve` / `deny` (on the server), `cluster report`.
    /// See docs/cluster-pairing.md and docs/cluster-passport.md.
    #[command(flatten)]
    Pairing(crate::cluster_pair::ClusterCommand),
    /// Make this machine the cluster server: run the fleet central as a service, make a short-lived join token and print
    /// the one command that adds a machine. Installs the prerequisites (qemu, cloud-image-utils, kvm group) first, after
    /// printing the list. Linux (systemd or OpenRC) and macOS.
    Init(InitArgs),
    /// Join this machine to a cluster: `fluxctl cluster join http://server:7799` with the token in a file, in
    /// $FLUXVM_TOKEN or on stdin (never an argument). Waits until central accepts the node.
    Join(JoinArgs),
    /// The nodes of the cluster: name, health, VMs, labels, how long since the last heartbeat.
    Nodes(Central),
    /// Join tokens: `create` (short-lived by default), `list` (ids and limits, never secrets), `revoke`.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Take a node out: revoke its credential (it stops being accepted), then deregister it. A node that is still
    /// heartbeating is refused by central until the credential has stopped it; re-run in half a minute.
    Remove(RemoveArgs),
    /// Bring Linux machines in over SSH from here: `fluxctl cluster add user@host ...`. Pinned host keys (BatchMode, strict
    /// checking unless --accept-new-hostkey), the join token is minted for you and travels on ssh stdin only.
    Add(AddArgs),
    /// Optional add-ons. They need a cluster, the cluster does not need them.
    Addon {
        #[command(subcommand)]
        command: AddonCommand,
    },
    /// Boot a tiny VM from the base image, run a command in it, delete it: PASS, or FAIL with the fix.
    Selftest(SelftestArgs),
}

#[derive(clap::Args, Clone)]
pub struct Central {
    /// The central's URL.
    #[arg(long, env = "CENTRAL_URL", default_value = "http://127.0.0.1:7799")]
    pub central: String,
    /// File holding the operator token (default: $FLUXVM_AGENT_TOKEN, then the token file central keeps in its state
    /// directory when this user can read it).
    #[arg(long, value_name = "FILE")]
    pub operator_token_file: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args)]
pub struct InitArgs {
    /// Central's port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// How nodes reach this server (default: http://<this machine's address>:<port>).
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
    /// Lifetime of the first join token, in seconds.
    #[arg(long, default_value_t = 3600)]
    pub token_ttl: u64,
    /// Also set up the optional k3s layer (`fluxctl kube up`).
    #[arg(long)]
    pub with_k3s: bool,
    /// Do not ask before installing prerequisites.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Do not install prerequisites.
    #[arg(long)]
    pub no_deps: bool,
    /// Skip the boot-a-VM self-test at the end.
    #[arg(long)]
    pub no_selftest: bool,
    /// Skip the Debian 13 base image download.
    #[arg(long)]
    pub no_image: bool,
    /// Print the command that would run and stop.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args)]
pub struct JoinArgs {
    /// The server's URL, e.g. http://server:7799 (a comma-separated list for an HA central).
    pub url: String,
    /// This node's name (default: the host name).
    #[arg(long)]
    pub name: Option<String>,
    /// How central reaches this node's daemon (default: http://<this machine's address>:7788).
    #[arg(long, value_name = "URL")]
    pub advertise_url: Option<String>,
    /// File holding the join token (`fvjt_...`). Alternatives: $FLUXVM_TOKEN, or --token-stdin.
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
    /// Read the join token from stdin.
    #[arg(long)]
    pub token_stdin: bool,
    /// Node label `key=value`, repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
    #[arg(long)]
    pub with_k3s: bool,
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(long)]
    pub no_deps: bool,
    #[arg(long)]
    pub no_selftest: bool,
    #[arg(long)]
    pub no_image: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Subcommand)]
pub enum TokenCommand {
    /// Make a join token. It is shown once. Default lifetime one hour.
    Create {
        #[command(flatten)]
        c: Central,
        /// Lifetime in seconds.
        #[arg(long, default_value_t = 3600)]
        ttl: u64,
        /// Stop accepting after this many nodes.
        #[arg(long)]
        max_uses: Option<u32>,
        /// Only nodes whose name matches, e.g. `rack1-*`.
        #[arg(long)]
        name_pattern: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
        /// Also write the token to this file (mode 0600).
        #[arg(long, value_name = "FILE")]
        write_to: Option<PathBuf>,
    },
    /// Ids, limits, uses and expiry. Never the secrets.
    List(Central),
    /// No new nodes with this token (nodes already enrolled keep their credentials).
    Revoke {
        #[command(flatten)]
        c: Central,
        id: String,
    },
}

#[derive(clap::Args)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub c: Central,
    pub node: String,
}

#[derive(clap::Args)]
pub struct AddArgs {
    #[command(flatten)]
    pub which: Which,
    #[command(flatten)]
    pub conn: Conn,
    /// The central the hosts join (default: this machine's, http://<address>:7799).
    #[arg(long, value_name = "URL")]
    pub join: Option<String>,
    #[command(flatten)]
    pub operator: OperatorOpts,
    /// Use this join token instead of minting one (file with `fvjt_...`, mode 0600).
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
    /// Read the join token from stdin.
    #[arg(long)]
    pub token_stdin: bool,
    /// Lifetime of the token minted for this run, in seconds.
    #[arg(long, default_value_t = 900)]
    pub token_ttl: u64,
    /// Node name (a single host only; default: the host's first label).
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, value_name = "URL")]
    pub advertise_url: Option<String>,
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub labels: Vec<String>,
    /// Where each host fetches install.sh from.
    #[arg(long, value_name = "URL", default_value = DEFAULT_INSTALLER_URL)]
    pub installer_url: String,
    /// Refuse install.sh unless its SHA-256 is this.
    #[arg(long, value_name = "HEX")]
    pub installer_sha256: Option<String>,
    #[arg(long)]
    pub with_k3s: bool,
    /// Skip the boot-a-VM self-test on each host.
    #[arg(long)]
    pub no_selftest: bool,
    /// Seconds to wait for each node to show up healthy in central afterwards (0 = do not wait).
    #[arg(long, default_value_t = 120)]
    pub wait: u64,
}

#[derive(clap::Args, Clone)]
pub struct OperatorOpts {
    #[arg(long, value_name = "FILE")]
    pub operator_token_file: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
pub enum AddonCommand {
    /// Set up the k3s layer: `fluxctl kube up` on this machine, or on Linux hosts over SSH with `--ssh user@host`.
    K3s(K3sArgs),
}

#[derive(clap::Args)]
pub struct K3sArgs {
    /// Hosts to run it on over SSH (default: this machine).
    #[arg(long = "ssh", value_name = "USER@HOST")]
    pub hosts: Vec<String>,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(clap::Args)]
pub struct SelftestArgs {
    /// The image to boot (default: the Debian 13 image the installer cached).
    #[arg(long, value_name = "PATH")]
    pub image: Option<PathBuf>,
    /// Config for the per-user paths (default: $FLUXVM_CONFIG, then ~/.config/fluxvm/config.toml when it exists).
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Give up after this many seconds.
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
}

// ---- operator token ---------------------------------------------------------------------------------------------------

/// Where central keeps the operator token it made on first start, most likely first.
pub fn operator_token_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/var/lib/fluxvm-central/operator-token")];
    if let Some(h) = home {
        v.push(h.join("Library/Application Support/FluxVM/central/operator-token"));
        v.push(h.join(".local/share/fluxvm/central/operator-token"));
    }
    v
}

/// `--operator-token-file`, then `$FLUXVM_AGENT_TOKEN`, then the first readable candidate.
pub fn resolve_operator_token(
    file: Option<&Path>,
    env: Option<&str>,
    candidates: &[PathBuf],
) -> Result<Option<String>> {
    let clean = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_owned())
    };
    if let Some(f) = file {
        let s = std::fs::read_to_string(f)
            .with_context(|| format!("operator token file {}", f.display()))?;
        return Ok(Some(clean(&s).context("the operator token file is empty")?));
    }
    if let Some(t) = env.and_then(clean) {
        return Ok(Some(t));
    }
    for c in candidates {
        if let Ok(s) = std::fs::read_to_string(c)
            && let Some(t) = clean(&s)
        {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

fn operator_token(file: Option<&Path>) -> Result<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let env = std::env::var("FLUXVM_AGENT_TOKEN").ok();
    resolve_operator_token(file, env.as_deref(), &operator_token_candidates(home.as_deref()))?
        .context(
            "no operator token: pass --operator-token-file FILE, or set FLUXVM_AGENT_TOKEN, or run this as root on the server (central keeps it in /var/lib/fluxvm-central/operator-token)",
        )
}

// ---- the API: only what fleet_client and central already have -------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct NewToken {
    pub id: String,
    pub token: String,
}

pub fn token_request(ttl: u64, max_uses: Option<u32>, pattern: Option<&str>, note: &str) -> Value {
    let mut b = json!({"ttl_secs": ttl, "note": note});
    if let Some(m) = max_uses {
        b["max_uses"] = json!(m);
    }
    if let Some(p) = pattern {
        b["name_pattern"] = json!(p);
    }
    b
}

pub fn parse_new_token(v: &Value) -> Result<NewToken> {
    let id = v["id"].as_str().context("central's answer has no token id")?;
    let token = v["token"].as_str().context("central's answer has no token")?;
    if !token.starts_with("fvjt_") {
        bail!("central returned something that is not a join token");
    }
    Ok(NewToken {
        id: id.to_owned(),
        token: token.to_owned(),
    })
}

pub async fn create_token(
    central: &str,
    op: &str,
    ttl: u64,
    max_uses: Option<u32>,
    pattern: Option<&str>,
    note: &str,
) -> Result<NewToken> {
    let v = fleet_client::call(
        central,
        Some(op),
        reqwest::Method::POST,
        "/fleet/enroll/tokens",
        Some(token_request(ttl, max_uses, pattern, note)),
    )
    .await?;
    parse_new_token(&v)
}

/// Writes `secret` to `path` with mode 0600, atomically.
pub fn write_secret_file(path: &Path, secret: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("new");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    writeln!(f, "{secret}")?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn age(secs: i64) -> String {
    match secs {
        s if s < 0 => "-".into(),
        s if s < 90 => format!("{s}s"),
        s if s < 5400 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

/// The node table: name, state, VMs, capacity, last heartbeat, labels. `now` is unix seconds.
pub fn render_nodes(list: &Value, enrolled: &Value, now: i64) -> String {
    let items = list["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "No nodes yet. On the server: fluxctl cluster token create; on a node: fluxctl cluster join URL\n".into();
    }
    let enrolled: std::collections::HashSet<String> = enrolled["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["revoked"] != json!(true))
                .filter_map(|e| e["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut rows = vec![vec![
        "NAME".to_owned(),
        "STATE".into(),
        "VMS".into(),
        "VCPUS".into(),
        "MEMORY".into(),
        "AUTH".into(),
        "SEEN".into(),
        "LABELS".into(),
    ]];
    for n in &items {
        let name = n["name"].as_str().unwrap_or("?");
        let seen = n["last_seen"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| now - t.timestamp());
        let healthy = n["healthy"]
            .as_bool()
            .unwrap_or_else(|| seen.is_some_and(|s| s < 30));
        let state = match (healthy, n["cordoned"].as_bool().unwrap_or(false)) {
            (true, false) => "Ready",
            (true, true) => "Ready,Cordoned",
            (false, _) => "NotReady",
        };
        let mut labels: Vec<String> = n["labels"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        labels.sort();
        rows.push(vec![
            name.to_owned(),
            state.into(),
            n["vm_count"].as_u64().unwrap_or(0).to_string(),
            n["vcpus_total"].as_u64().unwrap_or(0).to_string(),
            format!("{}G", n["memory_mib_total"].as_u64().unwrap_or(0) / 1024),
            if enrolled.contains(name) { "enrolled" } else { "fleet-token" }.into(),
            seen.map(age).unwrap_or_else(|| "-".into()),
            labels.join(","),
        ]);
    }
    let w: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|r| {
            let line: Vec<String> = r
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<width$}", width = w[i]))
                .collect();
            format!("{}\n", line.join("  ").trim_end())
        })
        .collect()
}

pub fn render_tokens(list: &Value, now: i64) -> String {
    let items = list["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "No join tokens. Make one: fluxctl cluster token create\n".into();
    }
    let mut out = format!("{:<12} {:<9} {:<7} {:<10} NOTE\n", "ID", "STATE", "USES", "EXPIRES");
    for t in &items {
        let exp = t["expires"].as_i64();
        let state = if t["revoked"] == json!(true) {
            "revoked"
        } else if exp.is_some_and(|e| e <= now) {
            "expired"
        } else {
            "active"
        };
        let uses = match t["max_uses"].as_u64() {
            Some(m) => format!("{}/{m}", t["uses"].as_u64().unwrap_or(0)),
            None => t["uses"].as_u64().unwrap_or(0).to_string(),
        };
        let expires = exp.map(|e| age(e - now)).unwrap_or_else(|| "never".into());
        out.push_str(&format!(
            "{:<12} {:<9} {:<7} {:<10} {}\n",
            t["id"].as_str().unwrap_or("?"),
            state,
            uses,
            expires,
            t["note"].as_str().unwrap_or("")
        ));
    }
    out
}

/// The one command that adds a machine. With `token_literal` the token is in it; otherwise it reads the token file
/// (when the output is a log, the token is not printed: docs/cluster-quickstart.md, "Where the token is printed").
pub fn join_command(public_url: &str, token_literal: Option<&str>, token_file_hint: &str) -> String {
    let env = match token_literal {
        Some(t) => format!("FLUXVM_TOKEN={t}"),
        None => format!("FLUXVM_TOKEN=\"$(cat {token_file_hint})\""),
    };
    format!(
        "curl -sfL {DEFAULT_INSTALLER_URL} | {env} bash -s -- --join {public_url}"
    )
}

/// `fluxctl cluster join` on a machine that already has fluxctl: the arguments for the package's own `install.sh`.
pub fn installer_args_init(a: &InitArgs) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "--skip-download".into(),
        "--server".into(),
        "--port".into(),
        a.port.to_string(),
        "--token-ttl".into(),
        a.token_ttl.to_string(),
    ];
    if let Some(u) = &a.public_url {
        v.extend(["--public-url".into(), u.clone()]);
    }
    push_common(&mut v, a.with_k3s, a.yes, a.no_deps, a.no_selftest, a.no_image, a.dry_run);
    v
}

pub fn installer_args_join(a: &JoinArgs) -> Vec<String> {
    let mut v: Vec<String> = vec!["--skip-download".into(), "--join".into(), a.url.clone()];
    if let Some(n) = &a.name {
        v.extend(["--node-name".into(), n.clone()]);
    }
    if let Some(u) = &a.advertise_url {
        v.extend(["--advertise-url".into(), u.clone()]);
    }
    if let Some(f) = &a.token_file {
        v.extend(["--token-file".into(), f.display().to_string()]);
    }
    if a.token_stdin {
        v.push("--token-stdin".into());
    }
    for l in &a.labels {
        v.extend(["--label".into(), l.clone()]);
    }
    push_common(&mut v, a.with_k3s, a.yes, a.no_deps, a.no_selftest, a.no_image, a.dry_run);
    v
}

fn push_common(v: &mut Vec<String>, k3s: bool, yes: bool, no_deps: bool, no_selftest: bool, no_image: bool, dry: bool) {
    for (on, flag) in [
        (k3s, "--with-k3s"),
        (yes, "--yes"),
        (no_deps, "--no-deps"),
        (no_selftest, "--no-selftest"),
        (no_image, "--no-image"),
        (dry, "--dry-run"),
    ] {
        if on {
            v.push(flag.into());
        }
    }
}

/// `share/install.sh` of the package this fluxctl came from: `<package>/bin/fluxctl` -> `<package>/share/install.sh`.
pub fn installer_path(exe: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(exe).ok()?;
    let p = real.parent()?.parent()?.join("share/install.sh");
    p.is_file().then_some(p)
}

#[cfg(target_os = "linux")]
fn run_installer(args: Vec<String>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let script = std::env::var_os("FLUXVM_INSTALLER")
        .map(PathBuf::from)
        .or_else(|| installer_path(&exe))
        .context(
            "this fluxctl is not from the release package (no share/install.sh next to it). Run the installer instead: curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --server",
        )?;
    // stdin, stdout and stderr are inherited: a token on stdin or in $FLUXVM_TOKEN reaches the script, an argument never does
    let status = std::process::Command::new("bash")
        .arg(&script)
        .args(&args)
        .status()
        .context("running bash")?;
    if !status.success() {
        bail!("install.sh exited with {status}");
    }
    Ok(())
}

// ---- macOS: native LaunchAgents --------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn central_paths(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents").join(format!("{CENTRAL_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-central.log"),
        home.join("Library/Application Support/FluxVM/central"),
    )
}

/// The command line of the central LaunchAgent. No token: central makes the operator token itself.
pub fn central_args(agent: &Path, port: u16, state_dir: &Path) -> Vec<String> {
    vec![
        agent.display().to_string(),
        "central".into(),
        "--listen".into(),
        format!("0.0.0.0:{port}"),
        "--state-dir".into(),
        state_dir.display().to_string(),
    ]
}

#[cfg(target_os = "macos")]
fn mac_init(a: &InitArgs) -> Result<()> {
    use crate::docker_host as dh;
    let home = dh::home()?;
    let (plist, log, state) = central_paths(&home);
    let agent = crate::node_agent::agent_binary()?;
    let args = central_args(&agent, a.port, &state);
    if a.dry_run {
        println!("would write {} and load it: {}", plist.display(), args.join(" "));
        return Ok(());
    }
    std::fs::create_dir_all(&state)?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(
        &plist,
        dh::render_plist_with(CENTRAL_LABEL, &args, &log, &[], &dh::PlistOpts::default()),
    )?;
    dh::launchctl_load(CENTRAL_LABEL, &plist)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn mac_join(a: &JoinArgs, token: &str) -> Result<()> {
    use crate::docker_host as dh;
    let home = dh::home()?;
    let tokf = home.join(".config/fluxvm/agent.token");
    write_secret_file(&tokf, token)?;
    // the daemon first (idempotent: only when it is not loaded)
    let loaded = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{}/{}", unsafe { libc::getuid() }, crate::launch_agent::LABEL)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !loaded {
        let st = std::process::Command::new(std::env::current_exe()?)
            .args(["service", "install"])
            .status()?;
        if !st.success() {
            bail!("fluxctl service install failed");
        }
    }
    let name = a.name.clone().unwrap_or_else(default_node_name);
    let args = crate::node_agent::NodeAgentArgs {
        name,
        central: a.url.clone(),
        fluxvm_url: "http://127.0.0.1:7788".into(),
        advertise_url: a.advertise_url.clone(),
        token_file: Some(tokf),
        labels: a
            .labels
            .iter()
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .collect(),
        no_auto_labels: false,
        no_overlay_sync: false,
        state_dir: None,
    };
    crate::node_agent::install(&args)?;
    Ok(())
}

pub fn default_node_name() -> String {
    let raw = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let n = crate::mac_fleet_ssh::sanitize_name(raw.trim());
    if n.is_empty() { "node".into() } else { n }
}

// ---- join token for `join` -------------------------------------------------------------------------------------------

/// `--token-file`, `$FLUXVM_TOKEN`, then stdin (only with `--token-stdin`). Always `fvjt_...`; a fleet or operator token
/// is refused here.
pub fn read_join_token(file: Option<&Path>, env: Option<&str>, stdin: Option<&mut dyn std::io::Read>) -> Result<String> {
    let raw = if let Some(f) = file {
        std::fs::read_to_string(f).with_context(|| format!("token file {}", f.display()))?
    } else if let Some(e) = env.filter(|e| !e.trim().is_empty()) {
        e.to_owned()
    } else if let Some(r) = stdin {
        let mut s = String::new();
        r.read_to_string(&mut s)?;
        s
    } else {
        bail!(
            "no join token: --token-file FILE, $FLUXVM_TOKEN or --token-stdin (never an argument). Make one on the server: fluxctl cluster token create"
        );
    };
    let t = raw.trim();
    if !t.starts_with("fvjt_") {
        bail!("that is not a join token (they start with fvjt_); an operator or fleet token is not accepted here");
    }
    if !t.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) {
        bail!("the join token has characters a token never has");
    }
    Ok(t.to_owned())
}

// ---- readiness -------------------------------------------------------------------------------------------------------

/// Polls `GET /fleet/nodes/{name}` until the node is healthy. The operator sees `healthy`; a node only sees itself accepted.
pub async fn wait_node_ready(central: &str, op: &str, name: &str, secs: u64) -> Result<bool> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Ok(n) = fleet_client::get_node(central, Some(op), name).await
            && n["healthy"].as_bool().unwrap_or(true)
        {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

// ---- add: Linux hosts over ssh ---------------------------------------------------------------------------------------

pub struct AddSettings {
    pub join: JoinSpecBase,
    pub token: Option<Secret>,
}

/// What every host shares; the name and advertise URL are per host.
#[derive(Clone)]
pub struct JoinSpecBase {
    pub central: String,
    pub advertise_url: Option<String>,
    pub labels: Vec<String>,
    pub installer_url: String,
    pub installer_sha256: Option<String>,
    pub with_k3s: bool,
    pub selftest: bool,
}

impl JoinSpecBase {
    pub fn for_host(&self, t: &Target, name_override: Option<&str>) -> JoinSpec {
        let name = name_override.unwrap_or(&t.name).to_owned();
        JoinSpec {
            central: self.central.clone(),
            advertise_url: self.advertise_url.as_ref().map(|u| {
                u.replace("{host}", &t.host).replace("{name}", &name)
            }),
            name,
            labels: self.labels.clone(),
            installer_url: self.installer_url.clone(),
            installer_sha256: self.installer_sha256.clone(),
            with_k3s: self.with_k3s,
            selftest: self.selftest,
        }
    }
}

/// One host: probe, preflight, then the plan. The same shape as `mac_fleet::up_host`.
pub fn add_host(
    t: &dyn crate::mac_fleet_ssh::Transport,
    target: &Target,
    s: &AddSettings,
    name_override: Option<&str>,
) -> HostResult {
    let r = blank(target);
    let out = match t.run(target, &sh_c(&remote::linux_probe_script()), None) {
        Err(e) => return fail(r, format!("{e:#}")),
        Ok(o) if o.ssh_failed() => {
            return fail(r, crate::mac_fleet_ssh::explain_ssh_failure(target, &o.stderr));
        }
        Ok(o) if !o.ok() => {
            return fail(r, format!("the probe failed: {}", crate::mac_fleet::last_lines(&o.stderr, 3)));
        }
        Ok(o) => o.stdout,
    };
    let facts = remote::parse_linux_facts(&out);
    let findings = remote::check_linux(&facts);
    let mut r = r;
    r.findings = findings.iter().filter(|f| f.level != Level::Ok).cloned().collect();
    if remote::has_errors(&findings) {
        let msgs: Vec<String> = findings
            .iter()
            .filter(|f| f.level == Level::Error)
            .map(|f| format!("{} (fix: {})", f.msg, f.fix))
            .collect();
        return fail(r, format!("preflight failed: {}", msgs.join("; ")));
    }
    let spec = s.join.for_host(target, name_override);
    if let Err(e) = remote::validate_join(&spec) {
        return fail(r, e);
    }
    let steps = remote::plan_add(&facts, &spec);
    if steps.iter().any(|st| st.stdin == crate::mac_fleet_remote::StepIn::Token) && s.token.is_none() && facts.known {
        return fail(r, "no join token for a host that is not enrolled yet");
    }
    crate::mac_fleet::run_steps(t, target, &steps, s.token.as_ref(), r)
}

// ---- entry -----------------------------------------------------------------------------------------------------------

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub async fn run(cmd: ClusterCommand) -> Result<()> {
    match cmd {
        // Handled in main, which has the loaded config `cluster status` reads.
        ClusterCommand::Status(_) => unreachable!("cluster status is dispatched before cluster::run"),
        ClusterCommand::Pairing(c) => crate::cluster_pair::run(c).await,
        ClusterCommand::Init(a) => {
            #[cfg(target_os = "linux")]
            {
                return run_installer(installer_args_init(&a));
            }
            #[cfg(target_os = "macos")]
            {
                mac_init(&a)?;
                if a.dry_run {
                    return Ok(());
                }
                return mac_init_finish(&a).await;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = a;
                bail!("fluxctl cluster init runs on Linux and macOS");
            }
        }
        ClusterCommand::Join(a) => {
            #[cfg(target_os = "linux")]
            {
                return run_installer(installer_args_join(&a));
            }
            #[cfg(target_os = "macos")]
            {
                if a.dry_run {
                    println!("would join {} as {}", a.url, a.name.clone().unwrap_or_else(default_node_name));
                    return Ok(());
                }
                let env = std::env::var("FLUXVM_TOKEN").ok();
                let mut stdin = std::io::stdin();
                let token = read_join_token(
                    a.token_file.as_deref(),
                    env.as_deref(),
                    a.token_stdin.then_some(&mut stdin as &mut dyn std::io::Read),
                )?;
                mac_join(&a, &token)?;
                println!("==> node agent installed; waiting for {} to accept it", a.url);
                return mac_join_wait(&a).await;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = a;
                bail!("fluxctl cluster join runs on Linux and macOS");
            }
        }
        ClusterCommand::Nodes(c) => {
            let op = operator_token(c.operator_token_file.as_deref())?;
            let list = fleet_client::list_nodes(&c.central, Some(&op)).await?;
            if c.json {
                return print_json(&list);
            }
            let enrolled = fleet_client::call(&c.central, Some(&op), reqwest::Method::GET, "/fleet/enroll/nodes", None)
                .await
                .unwrap_or(Value::Null);
            print!("{}", render_nodes(&list, &enrolled, now()));
            Ok(())
        }
        ClusterCommand::Token { command } => run_token(command).await,
        ClusterCommand::Remove(a) => {
            let op = operator_token(a.c.operator_token_file.as_deref())?;
            fleet_client::call(
                &a.c.central,
                Some(&op),
                reqwest::Method::POST,
                &format!("/fleet/nodes/{}/revoke", a.node),
                None,
            )
            .await
            .or_else(|e| {
                // a node that never enrolled (it used the fleet token) has no credential to revoke
                if format!("{e:#}").contains("404") { Ok(Value::Null) } else { Err(e) }
            })?;
            match fleet_client::deregister(&a.c.central, Some(&op), &a.node).await {
                Ok(()) => println!("{} removed from the cluster", a.node),
                Err(e) if format!("{e:#}").contains("404") => {
                    println!("{} is not registered with this central (already removed?)", a.node)
                }
                Err(e) => println!(
                    "{}'s credential is revoked, but central still counts it as healthy ({e:#}). Re-run in about 30 seconds, after it has stopped.",
                    a.node
                ),
            }
            Ok(())
        }
        ClusterCommand::Add(a) => run_add(a).await,
        ClusterCommand::Addon { command: AddonCommand::K3s(k) } => run_k3s(k),
        ClusterCommand::Selftest(a) => run_selftest(a),
    }
}

async fn run_token(cmd: TokenCommand) -> Result<()> {
    match cmd {
        TokenCommand::Create { c, ttl, max_uses, name_pattern, note, write_to } => {
            let op = operator_token(c.operator_token_file.as_deref())?;
            let t = create_token(&c.central, &op, ttl, max_uses, name_pattern.as_deref(), &note).await?;
            if let Some(p) = &write_to {
                write_secret_file(p, &t.token)?;
            }
            if c.json {
                return print_json(&json!({"id": t.id, "token": t.token, "ttl_secs": ttl}));
            }
            // The token is printed once, and only to a terminal; a log gets the id and the file.
            if std::io::stdout().is_terminal() {
                println!("{}", t.token);
                eprintln!("join token {} (shown once, valid {}): FLUXVM_TOKEN=<it> ... --join {}", t.id, age(ttl as i64), c.central);
            } else if write_to.is_some() {
                println!("join token {} written to {} (mode 0600)", t.id, write_to.unwrap().display());
            } else {
                bail!("stdout is not a terminal, so the token was not printed (and is now lost: revoke {} and use --write-to FILE or --json)", t.id);
            }
            Ok(())
        }
        TokenCommand::List(c) => {
            let op = operator_token(c.operator_token_file.as_deref())?;
            let v = fleet_client::call(&c.central, Some(&op), reqwest::Method::GET, "/fleet/enroll/tokens", None).await?;
            if c.json {
                return print_json(&v);
            }
            print!("{}", render_tokens(&v, now()));
            Ok(())
        }
        TokenCommand::Revoke { c, id } => {
            let op = operator_token(c.operator_token_file.as_deref())?;
            fleet_client::call(&c.central, Some(&op), reqwest::Method::DELETE, &format!("/fleet/enroll/tokens/{id}"), None).await?;
            println!("token {id} revoked (nodes already enrolled keep their credentials)");
            Ok(())
        }
    }
}

async fn run_add(a: AddArgs) -> Result<()> {
    let (targets, inv) = resolve(&a.which, &a.conn)?;
    if a.name.is_some() && targets.len() != 1 {
        bail!("--name is for a single host");
    }
    let central = match &a.join {
        Some(u) => u.trim_end_matches('/').to_owned(),
        None => bail!("--join URL is required: the central these hosts should join (e.g. --join http://this-machine:7799)"),
    };
    let central_ctl = std::env::var("CENTRAL_URL").unwrap_or_else(|_| central.clone());
    // token: the one given, or a fresh short-lived one minted here with the operator token
    let mut minted: Option<NewToken> = None;
    let mut op: Option<String> = None;
    let token = if a.token_file.is_some() || a.token_stdin {
        let env = None;
        let mut stdin = std::io::stdin();
        Some(read_join_token(
            a.token_file.as_deref(),
            env,
            a.token_stdin.then_some(&mut stdin as &mut dyn std::io::Read),
        )?)
    } else if a.conn.dry_run {
        // a placeholder: the dry run shows that a token would travel on stdin, never a real one
        Some("fvjt_dry-run-placeholder".to_owned())
    } else {
        let o = operator_token(a.operator.operator_token_file.as_deref())?;
        let t = create_token(&central_ctl, &o, a.token_ttl, Some(targets.len() as u32), None, "cluster add").await?;
        let s = t.token.clone();
        minted = Some(t);
        op = Some(o);
        Some(s)
    };
    let secret = token.as_ref().map(|t| Secret::new(t.as_bytes().to_vec()));
    let s = AddSettings {
        join: JoinSpecBase {
            central: central.clone(),
            advertise_url: a.advertise_url.clone(),
            labels: a.labels.clone(),
            installer_url: a.installer_url.clone(),
            installer_sha256: a.installer_sha256.clone(),
            with_k3s: a.with_k3s,
            selftest: !a.no_selftest,
        },
        token: secret,
    };
    let tr = transport(&a.conn, inv.as_ref());
    let name = a.name.clone();
    let rs: Vec<HostResult> = tokio::task::block_in_place(|| {
        par_map(&targets, jobs(&a.conn), |t| add_host(tr.as_ref(), t, &s, name.as_deref()))
    });
    if a.conn.dry_run {
        eprintln!("# dry run: nothing was sent to any host. Each command below is what would run.");
        if let Some(d) = tr.as_dry() {
            for l in d.lines.lock().unwrap().iter() {
                println!("{l}");
            }
        }
        return Ok(());
    }
    // a minted token is only good for these hosts: revoke it now, whatever happened
    if let (Some(t), Some(o)) = (&minted, &op) {
        let _ = fleet_client::call(&central_ctl, Some(o), reqwest::Method::DELETE, &format!("/fleet/enroll/tokens/{}", t.id), None).await;
    }
    let mut rs = rs;
    if a.wait > 0
        && let Some(o) = &op
    {
        for (r, t) in rs.iter_mut().zip(&targets) {
            if r.ok && !wait_node_ready(&central_ctl, o, name.as_deref().unwrap_or(&t.name), a.wait).await? {
                r.ok = false;
                r.error = Some(format!("joined, but not healthy in central within {}s: fluxctl cluster nodes", a.wait));
            }
        }
    }
    if a.conn.json {
        println!("{}", serde_json::to_string_pretty(&rs)?);
    } else {
        for r in &rs {
            let state = if r.ok { "Ready".to_owned() } else { format!("FAILED: {}", scrub(r.error.as_deref().unwrap_or(""), s.token.as_ref())) };
            println!("{:<28} {}", r.host, state);
        }
    }
    finish(&rs, "add").map_err(|e| anyhow::anyhow!("{}", e.to_string().replace("Macs", "hosts")))
}

fn run_k3s(k: K3sArgs) -> Result<()> {
    if k.hosts.is_empty() {
        let st = std::process::Command::new(std::env::current_exe()?).args(["kube", "up"]).status()?;
        if !st.success() {
            bail!("fluxctl kube up failed; the cluster itself is unaffected");
        }
        return Ok(());
    }
    let which = Which { hosts: k.hosts.clone(), inventory: None, all: false };
    let (targets, inv) = resolve(&which, &k.conn)?;
    let tr = transport(&k.conn, inv.as_ref());
    let script = sh_c(&remote::k3s_script());
    let rs = par_map(&targets, jobs(&k.conn), |t| {
        let r = blank(t);
        match tr.run(t, &script, None) {
            Ok(o) if o.ok() => HostResult { ok: true, ..r },
            Ok(o) => fail(r, crate::mac_fleet::last_lines(&format!("{}\n{}", o.stdout, o.stderr), 3)),
            Err(e) => fail(r, format!("{e:#}")),
        }
    });
    for r in &rs {
        println!("{:<28} {}", r.host, if r.ok { "k3s add-on started".into() } else { format!("FAILED: {}", r.error.clone().unwrap_or_default()) });
    }
    finish(&rs, "k3s add-on").map_err(|e| anyhow::anyhow!("{}", e.to_string().replace("Macs", "hosts")))
}

// ---- self-test --------------------------------------------------------------------------------------------------------

/// The one fix most likely to matter for what the test printed. Mirrors `selftest_hint` in scripts/install.sh.
pub fn selftest_hint(output: &str) -> &'static str {
    let o = output;
    if o.contains("waiting for the guest's address") {
        "fluxctl run cannot reach a guest on Linux's user-mode network yet (no port forwards); the fix is in the kube-unified work. The cluster itself is fine; re-run: fluxctl cluster selftest after upgrading."
    } else if o.contains("kvm") && o.contains("ermission") {
        "Your user cannot open /dev/kvm: sudo usermod -aG kvm $USER, then log out and in (or: newgrp kvm)."
    } else if o.contains("/dev/kvm") || o.contains("KVM") {
        "No usable /dev/kvm: enable virtualisation in the firmware; on a cloud VM use an instance type with nested virtualisation."
    } else if o.contains("cloud-localds") {
        "Install cloud-image-utils (cloud-localds): sudo apt-get install cloud-image-utils (Fedora: cloud-utils)."
    } else if o.contains("qemu") && (o.contains("not found") || o.contains("No such file")) {
        "Install qemu: sudo apt-get install qemu-system-x86 qemu-utils (Fedora: qemu-kvm qemu-img)."
    } else if o.contains("/run/fluxvm") || o.contains("Permission denied") {
        "A system path is root-owned: use the per-user config the installer wrote (export FLUXVM_CONFIG=$HOME/.config/fluxvm/config.toml)."
    } else if o.contains("No such file") || o.contains("not found") {
        "The base image is missing: run the installer again (it downloads and verifies Debian 13) or pass --image."
    } else {
        "Run: fluxctl doctor (every failure prints its fix)."
    }
}

pub fn default_image(cache: &Path, arch: &str) -> PathBuf {
    let a = if arch == "aarch64" || arch == "arm64" { "arm64" } else { "amd64" };
    cache.join(format!("fluxvm/images/debian-13-genericcloud-{a}.qcow2"))
}

fn run_selftest(a: SelftestArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache"));
    let image = a.image.unwrap_or_else(|| default_image(&cache, std::env::consts::ARCH));
    if !image.is_file() {
        println!("SELF-TEST FAIL\nFix: {} is missing: run the installer again (it downloads and verifies Debian 13) or pass --image.", image.display());
        bail!("self-test failed");
    }
    let cfg = a
        .config
        .or_else(|| std::env::var_os("FLUXVM_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let p = home.join(".config/fluxvm/config.toml");
            p.is_file().then_some(p)
        });
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    if let Some(c) = &cfg {
        cmd.arg("--config").arg(c);
    }
    cmd.args(["run"]).arg(&image).args(["--cpus", "1", "--memory-mib", "512", "--", "sh", "-c", "echo fluxvm-selftest-$((6*7))"]);
    let out = crate::mac_fleet_timed::run_with_timeout(cmd, None, Some(std::time::Duration::from_secs(a.timeout)), "fluxctl run")?;
    let text = format!("{}\n{}", out.stdout, out.stderr);
    if text.contains("fluxvm-selftest-42") {
        println!("SELF-TEST PASS: a VM booted, ran a command and was deleted.");
        Ok(())
    } else {
        let tail: Vec<&str> = text.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
        println!("SELF-TEST FAIL (exit {})\n{}\nFix: {}", out.code, tail.join("\n"), selftest_hint(&text));
        bail!("self-test failed")
    }
}

#[cfg(target_os = "macos")]
async fn mac_init_finish(a: &InitArgs) -> Result<()> {
    let central = format!("http://127.0.0.1:{}", a.port);
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let tokf = home.join("Library/Application Support/FluxVM/central/operator-token");
    let mut op = None;
    for _ in 0..30 {
        if let Ok(s) = std::fs::read_to_string(&tokf) && !s.trim().is_empty() {
            op = Some(s.trim().to_owned());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let op = op.context("central did not make its operator token within 30s: see ~/Library/Logs/fluxvm-central.log")?;
    let t = create_token(&central, &op, a.token_ttl, None, None, "cluster init").await?;
    let out = home.join(".config/fluxvm/join.token");
    write_secret_file(&out, &t.token)?;
    let public = a.public_url.clone().unwrap_or_else(|| format!("http://{}:{}", local_host(), a.port));
    let literal = std::io::stdout().is_terminal().then_some(t.token.as_str());
    println!("\n==> FluxVM server is Ready.\n    Central:   {public}\n    Dashboard: http://127.0.0.1:{}/console/  (over ssh: ssh -L {0}:127.0.0.1:{0} you@this-mac)\n", a.port);
    println!("    Add a machine:\n\n      {}\n\n    The token is also in {} (mode 0600).", join_command(&public, literal, &out.display().to_string()), out.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn local_host() -> String {
    let n = std::process::Command::new("hostname").output().ok().and_then(|o| String::from_utf8(o.stdout).ok()).unwrap_or_default();
    let n = n.trim();
    if n.is_empty() { "127.0.0.1".into() } else { n.to_owned() }
}

#[cfg(target_os = "macos")]
async fn mac_join_wait(a: &JoinArgs) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let cred = home.join("Library/Application Support/FluxVM/agent/node-credential");
    for _ in 0..60 {
        if cred.is_file() {
            let _ = std::fs::remove_file(home.join(".config/fluxvm/agent.token"));
            println!("==> {} is enrolled; the join token was removed from this Mac. On the server: fluxctl cluster nodes", a.name.clone().unwrap_or_else(default_node_name));
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    bail!("the node did not enroll within 2 minutes: see ~/Library/Logs/fluxvm-agent.log (the token may have expired: make a new one with fluxctl cluster token create)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, failed, ok};
    use fluxvm_agent::central::{CentralConfig, router};

    async fn spawn_central(token: &str) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let app = router(CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some(token.into()),
            reconcile_interval: None,
            ha: None,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), dir)
    }

    #[test]
    fn the_join_command_has_the_url_and_never_both_token_forms() {
        let lit = join_command("http://s:7799", Some("fvjt_a.b"), "~/.config/fluxvm/join.token");
        assert!(lit.ends_with("bash -s -- --join http://s:7799"));
        assert!(lit.contains("FLUXVM_TOKEN=fvjt_a.b bash"));
        let f = join_command("http://s:7799", None, "~/.config/fluxvm/join.token");
        assert!(!f.contains("fvjt_") && f.contains("$(cat ~/.config/fluxvm/join.token)"));
    }

    #[test]
    fn the_installer_gets_flags_and_never_a_token() {
        let a = JoinArgs {
            url: "http://s:7799".into(),
            name: Some("w1".into()),
            advertise_url: None,
            token_file: Some("/t/tok".into()),
            token_stdin: false,
            labels: vec!["zone=lab".into()],
            with_k3s: true,
            yes: true,
            no_deps: false,
            no_selftest: true,
            no_image: false,
            dry_run: false,
        };
        let v = installer_args_join(&a);
        assert_eq!(v[..3], ["--skip-download", "--join", "http://s:7799"]);
        assert!(v.windows(2).any(|w| w == ["--token-file", "/t/tok"]));
        assert!(v.contains(&"--with-k3s".to_owned()) && v.contains(&"--yes".to_owned()) && v.contains(&"--no-selftest".to_owned()));
        assert!(!v.iter().any(|x| x.starts_with("fvjt_")));
        let i = InitArgs { port: 7800, public_url: Some("http://x:7800".into()), token_ttl: 600, with_k3s: false, yes: true, no_deps: false, no_selftest: false, no_image: false, dry_run: false };
        let v = installer_args_init(&i);
        assert_eq!(v[1], "--server");
        assert!(v.windows(2).any(|w| w == ["--port", "7800"]) && v.windows(2).any(|w| w == ["--token-ttl", "600"]));
        assert!(v.windows(2).any(|w| w == ["--public-url", "http://x:7800"]));
    }

    #[test]
    fn the_central_service_has_no_token_in_its_command_line() {
        let v = central_args(Path::new("/opt/fluxvm-agent"), 7799, Path::new("/s"));
        assert_eq!(v, ["/opt/fluxvm-agent", "central", "--listen", "0.0.0.0:7799", "--state-dir", "/s"]);
        assert!(!v.iter().any(|a| a.contains("token")));
    }

    #[test]
    fn the_operator_token_comes_from_the_file_then_the_environment_then_central_s_own_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("t");
        std::fs::write(&f, "from-file\n").unwrap();
        let c = d.path().join("c");
        std::fs::write(&c, "from-central\n").unwrap();
        let cands = vec![d.path().join("missing"), c];
        assert_eq!(resolve_operator_token(Some(&f), Some("env"), &cands).unwrap().as_deref(), Some("from-file"));
        assert_eq!(resolve_operator_token(None, Some(" env "), &cands).unwrap().as_deref(), Some("env"));
        assert_eq!(resolve_operator_token(None, None, &cands).unwrap().as_deref(), Some("from-central"));
        assert_eq!(resolve_operator_token(None, Some("  "), &[]).unwrap(), None);
        assert!(resolve_operator_token(Some(&d.path().join("nope")), None, &[]).is_err());
        let e = d.path().join("empty");
        std::fs::write(&e, "\n").unwrap();
        assert!(resolve_operator_token(Some(&e), None, &[]).is_err());
    }

    #[test]
    fn a_join_token_is_checked_and_an_operator_token_is_refused() {
        assert_eq!(read_join_token(None, Some(" fvjt_ab.cd\n"), None).unwrap(), "fvjt_ab.cd");
        let mut s: &[u8] = b"fvjt_x.y\n";
        assert_eq!(read_join_token(None, None, Some(&mut s)).unwrap(), "fvjt_x.y");
        assert!(read_join_token(None, Some("operatorsecret"), None).unwrap_err().to_string().contains("not a join token"));
        assert!(read_join_token(None, None, None).unwrap_err().to_string().contains("never an argument"));
        assert!(read_join_token(None, Some("fvjt_a b"), None).is_err());
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("tok");
        std::fs::write(&f, "fvjt_file.tok\n").unwrap();
        assert_eq!(read_join_token(Some(&f), Some("fvjt_env"), None).unwrap(), "fvjt_file.tok");
    }

    #[test]
    fn a_secret_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/join.token");
        write_secret_file(&p, "fvjt_x").unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fvjt_x\n");
        assert!(!p.with_extension("new").exists());
    }

    #[test]
    fn the_tables_show_state_auth_and_never_a_secret() {
        let list = json!({"items": [
            {"name": "a", "healthy": true, "cordoned": false, "vm_count": 2, "vcpus_total": 8, "memory_mib_total": 16384, "labels": {"zone": "lab"}, "last_seen": "2026-01-01T00:00:10Z"},
            {"name": "b", "healthy": false, "cordoned": true, "vm_count": 0, "vcpus_total": 4, "memory_mib_total": 8192, "labels": {}, "last_seen": "2026-01-01T00:00:00Z"}
        ]});
        let enrolled = json!({"items": [{"name": "a", "revoked": false}, {"name": "b", "revoked": true}]});
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:40Z").unwrap().timestamp();
        let t = render_nodes(&list, &enrolled, now);
        let a = t.lines().find(|l| l.starts_with('a')).unwrap();
        assert!(a.contains("Ready") && a.contains("enrolled") && a.contains("zone=lab") && a.contains("30s"), "{a}");
        let b = t.lines().find(|l| l.starts_with('b')).unwrap();
        assert!(b.contains("NotReady") && b.contains("fleet-token"), "{b}");
        assert!(render_nodes(&json!({"items": []}), &Value::Null, 0).contains("No nodes yet"));
        let toks = json!({"items": [
            {"id": "t1", "revoked": false, "expires": now + 3000, "uses": 1, "max_uses": 5, "note": "x"},
            {"id": "t2", "revoked": true, "expires": null, "uses": 0, "max_uses": null, "note": ""},
            {"id": "t3", "revoked": false, "expires": now - 5, "uses": 0, "max_uses": null, "note": ""}
        ]});
        let r = render_tokens(&toks, now);
        assert!(r.contains("t1") && r.contains("1/5") && r.contains("active"));
        assert!(r.contains("revoked") && r.contains("expired") && !r.contains("fvjt_"));
    }

    #[test]
    fn the_hint_names_the_kvm_fix() {
        assert!(selftest_hint("qemu: Could not access KVM kernel module: Permission denied /dev/kvm").contains("usermod -aG kvm"));
        assert!(selftest_hint("still waiting for the guest's address (60s)…").contains("user-mode network"));
        assert!(selftest_hint("cloud-localds: not found").contains("cloud-image-utils"));
        assert!(selftest_hint("mkdir /run/fluxvm: Permission denied").contains("FLUXVM_CONFIG"));
        assert!(selftest_hint("weird").contains("fluxctl doctor"));
        assert_eq!(
            default_image(Path::new("/c"), "x86_64"),
            PathBuf::from("/c/fluxvm/images/debian-13-genericcloud-amd64.qcow2")
        );
        assert!(default_image(Path::new("/c"), "aarch64").to_string_lossy().ends_with("arm64.qcow2"));
    }

    // ---- the API, against a real central (the same one the fleet tests use) ------------------------------------------

    #[tokio::test]
    async fn tokens_nodes_and_remove_work_against_a_real_central() {
        let (url, _d) = spawn_central("op-secret").await;
        // create, list, revoke
        let t = create_token(&url, "op-secret", 600, Some(2), Some("w-*"), "test").await.unwrap();
        assert!(t.token.starts_with("fvjt_"));
        let list = fleet_client::call(&url, Some("op-secret"), reqwest::Method::GET, "/fleet/enroll/tokens", None).await.unwrap();
        assert_eq!(list["items"][0]["id"], json!(t.id));
        assert!(!list.to_string().contains(&t.token), "the list never has the secret");
        // a wrong operator token is refused
        assert!(create_token(&url, "nope", 600, None, None, "").await.is_err());
        // a node enrolls with the token, then registers with its credential
        let http = reqwest::Client::new();
        let r: Value = http
            .post(format!("{url}/fleet/enroll"))
            .bearer_auth(&t.token)
            .json(&json!({"name": "w-1", "node_id": "a".repeat(32)}))
            .send().await.unwrap().json().await.unwrap();
        let cred = r["credential"].as_str().unwrap().to_owned();
        let reg = http
            .post(format!("{url}/fleet/register"))
            .bearer_auth(&cred)
            .json(&json!({"name": "w-1", "fluxvm_url": "http://10.0.0.5:7788", "vcpus_total": 8, "memory_mib_total": 16384, "vm_count": 0, "labels": {"zone": "lab"}}))
            .send().await.unwrap();
        assert!(reg.status().is_success());
        // the node's own credential check (what install.sh waits on) is accepted
        let ov = http.get(format!("{url}/fleet/nodes/w-1/overlays")).bearer_auth(&cred).send().await.unwrap();
        assert!(ov.status().is_success(), "{}", ov.status());
        // the operator sees it as a Ready, enrolled node
        let nodes = fleet_client::list_nodes(&url, Some("op-secret")).await.unwrap();
        let enrolled = fleet_client::call(&url, Some("op-secret"), reqwest::Method::GET, "/fleet/enroll/nodes", None).await.unwrap();
        let table = render_nodes(&nodes, &enrolled, now());
        assert!(table.contains("w-1") && table.contains("enrolled") && table.contains("zone=lab"), "{table}");
        assert!(wait_node_ready(&url, "op-secret", "w-1", 5).await.unwrap());
        assert!(!wait_node_ready(&url, "op-secret", "nobody", 0).await.unwrap());
        // the name pattern holds
        let bad = http.post(format!("{url}/fleet/enroll")).bearer_auth(&t.token).json(&json!({"name": "other", "node_id": "b".repeat(32)})).send().await.unwrap();
        assert_eq!(bad.status(), 403);
        // revoke the token: no new nodes
        fleet_client::call(&url, Some("op-secret"), reqwest::Method::DELETE, &format!("/fleet/enroll/tokens/{}", t.id), None).await.unwrap();
        let after = http.post(format!("{url}/fleet/enroll")).bearer_auth(&t.token).json(&json!({"name": "w-2", "node_id": "c".repeat(32)})).send().await.unwrap();
        assert_eq!(after.status(), 401);
        // remove: revoke the credential, then deregister (a healthy node is refused until it has stopped)
        fleet_client::call(&url, Some("op-secret"), reqwest::Method::POST, "/fleet/nodes/w-1/revoke", None).await.unwrap();
        let denied = http.post(format!("{url}/fleet/register")).bearer_auth(&cred).json(&json!({"name": "w-1", "fluxvm_url": "http://10.0.0.5:7788", "vcpus_total": 8, "memory_mib_total": 1, "vm_count": 0})).send().await.unwrap();
        assert!(denied.status().is_client_error(), "a revoked credential stops registering");
    }

    // ---- add over ssh, against the fake transport ---------------------------------------------------------------------

    fn base() -> JoinSpecBase {
        JoinSpecBase {
            central: "http://10.0.0.2:7799".into(),
            advertise_url: Some("http://{host}:7788".into()),
            labels: vec!["zone=lab".into()],
            installer_url: DEFAULT_INSTALLER_URL.into(),
            installer_sha256: None,
            with_k3s: false,
            selftest: true,
        }
    }

    const FRESH: &str = "os=Linux\narch=x86_64\ndistro=ubuntu\nuser=sus\nsudo=passwordless\ninit=systemd\nkvm=yes\ncurl=yes\nfluxctl_version=\nagent_active=no\nenrolled=no\n";

    fn is_probe(cmd: &str) -> bool {
        cmd.contains("echo \"os=$(uname -s)\"")
    }

    #[test]
    fn a_fresh_host_gets_the_installer_with_the_token_on_stdin_only() {
        let tr = FakeTransport::new(|_, cmd| if is_probe(cmd) { ok(FRESH) } else { ok("done") });
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let s = AddSettings { join: base(), token: Some(Secret::new(b"fvjt_SECRET.tok".to_vec())) };
        let r = add_host(&tr, &t, &s, None);
        assert!(r.ok, "{:?}", r.error);
        let calls = tr.calls_for("10.0.0.9");
        assert_eq!(calls.len(), 2);
        assert!(calls[0].stdin.is_none());
        assert_eq!(calls[1].stdin.as_deref(), Some(&b"fvjt_SECRET.tok"[..]));
        assert!(calls.iter().all(|c| !c.command.contains("fvjt_SECRET")), "the token is not in any command");
        assert!(calls[1].command.contains("--node-name") && calls[1].command.contains("10-0-0-9"));
        assert!(calls[1].command.contains("http://10.0.0.9:7788"), "{{host}} is replaced");
        assert!(calls[1].command.contains("zone=lab"));
    }

    #[test]
    fn a_node_that_is_already_in_is_left_alone_and_needs_no_token() {
        let tr = FakeTransport::new(|_, cmd| {
            if is_probe(cmd) { ok(&FRESH.replace("agent_active=no", "agent_active=yes").replace("enrolled=no", "enrolled=yes")) } else { ok("") }
        });
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let r = add_host(&tr, &t, &AddSettings { join: base(), token: None }, None);
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(tr.calls_for("10.0.0.9").len(), 1, "only the probe ran");
    }

    #[test]
    fn preflight_stops_before_anything_is_installed() {
        let tr = FakeTransport::new(|_, cmd| if is_probe(cmd) { ok(&FRESH.replace("passwordless", "password")) } else { ok("") });
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let r = add_host(&tr, &t, &AddSettings { join: base(), token: Some(Secret::new(b"fvjt_x.y".to_vec())) }, None);
        assert!(!r.ok);
        assert!(r.error.as_deref().unwrap().contains("NOPASSWD"));
        assert_eq!(tr.calls_for("10.0.0.9").len(), 1);
    }

    #[test]
    fn an_unreachable_or_unverified_host_says_why_and_others_go_on() {
        let tr = FakeTransport::new(|t, cmd| {
            if t.host == "10.0.0.8" { return failed(255, "Host key verification failed."); }
            if is_probe(cmd) { ok(FRESH) } else { ok("done") }
        });
        let s = AddSettings { join: base(), token: Some(Secret::new(b"fvjt_x.y".to_vec())) };
        let targets = vec![Target::parse("a@10.0.0.8").unwrap(), Target::parse("a@10.0.0.9").unwrap()];
        let rs = par_map(&targets, 2, |t| add_host(&tr, t, &s, None));
        assert!(!rs[0].ok && rs[0].error.as_deref().unwrap().contains("known_hosts"));
        assert!(rs[1].ok);
    }

    #[test]
    fn a_failing_install_step_never_shows_the_token() {
        let tr = FakeTransport::new(|_, cmd| if is_probe(cmd) { ok(FRESH) } else { failed(1, "install.sh: bad token fvjt_SECRET.tok") });
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let r = add_host(&tr, &t, &AddSettings { join: base(), token: Some(Secret::new(b"fvjt_SECRET.tok".to_vec())) }, None);
        assert!(!r.ok);
        assert!(!format!("{r:?}").contains("SECRET"));
    }

    #[test]
    fn a_dry_run_prints_the_ssh_lines_and_sends_nothing() {
        let dry = crate::mac_fleet_ssh::DryTransport::new(Default::default());
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let r = add_host(&dry, &t, &AddSettings { join: base(), token: Some(Secret::new(b"fvjt_x.y".to_vec())) }, None);
        assert!(r.ok);
        let lines = dry.lines.lock().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("StrictHostKeyChecking=yes") && lines[0].contains("BatchMode=yes"));
        assert!(lines[1].contains("stdin: the join token"));
    }

    #[test]
    fn the_k3s_addon_is_a_step_only_for_a_node_that_is_already_in() {
        let mut b = base();
        b.with_k3s = true;
        let tr = FakeTransport::new(|_, cmd| if is_probe(cmd) { ok(&FRESH.replace("agent_active=no", "agent_active=yes").replace("enrolled=no", "enrolled=yes")) } else { ok("") });
        let t = Target::parse("sus@10.0.0.9").unwrap();
        let r = add_host(&tr, &t, &AddSettings { join: b, token: None }, None);
        assert!(r.ok);
        let calls = tr.calls_for("10.0.0.9");
        assert_eq!(calls.len(), 2);
        assert!(calls[1].command.contains("fluxctl kube up"));
    }
}
