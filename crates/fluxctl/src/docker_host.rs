// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl docker`: a real Docker engine inside a FluxVM VM, reachable from the Mac as a unix socket.
//!
//! The built-in Docker Engine API subset (`docker_sock`, one microVM per container) cannot `build`, run Compose,
//! bind-mount Mac paths or create Docker networks. This is the full-fidelity alternative: one managed Debian VM runs
//! `dockerd`, host folders are shared into it at the **same path** (so `-v $PWD/conf:/etc/conf` and Compose bind
//! mounts resolve identically on both sides), and a supervised SSH forward publishes the guest's `/var/run/docker.sock`
//! at `{run_dir}/docker-engine.sock`. Point the Docker CLI at it with `fluxctl docker env`.

use crate::run::{self, VmApi};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const DEFAULT_NAME: &str = "docker-engine";
/// The first boot installs Docker with apt, so allow for a slow mirror.
const FIRST_BOOT_BUDGET: Duration = Duration::from_secs(900);
pub const AGENT_LABEL: &str = "dev.zyvor.fluxvm.docker-engine";

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub name: String,
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub image: Option<String>,
    /// `HOST` or `HOST:GUEST` (default guest path = host path), optional `:ro`.
    pub shares: Vec<String>,
    pub socket: Option<PathBuf>,
    pub user: Option<String>,
    /// `HOST` or `HOST:GUEST` TCP ports forwarded from this Mac's loopback to the guest's loopback.
    pub publish: Vec<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.into(),
            cpus: 2,
            memory_mib: 3072,
            disk_gib: 40,
            image: None,
            shares: vec![],
            socket: None,
            user: None,
            publish: vec![],
        }
    }
}

/// Where the Docker CLI connects: `{run_dir}/docker-engine.sock` (next to the built-in `docker.sock`).
pub fn default_socket() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    PathBuf::from(format!("/tmp/fluxvm-run-{}", unsafe { libc::getuid() }))
        .join("docker-engine.sock")
}

fn socket_of(o: &EngineOptions) -> PathBuf {
    o.socket.clone().unwrap_or_else(default_socket)
}

pub(crate) fn pid_file(socket: &Path) -> PathBuf {
    let mut p = socket.as_os_str().to_owned();
    p.push(".pid");
    PathBuf::from(p)
}

/// `~/ci` or `/Users/me/ci:/Users/me/ci:ro` -> a shared folder whose guest path equals the host path by default.
pub fn parse_share(s: &str, cwd: &Path, home: Option<&Path>) -> Result<Value> {
    let (rest, ro) = match s.strip_suffix(":ro") {
        Some(r) => (r, true),
        None => (s.strip_suffix(":rw").unwrap_or(s), false),
    };
    let (host, guest) = match rest.split_once(':') {
        Some((h, g)) => (h, Some(g)),
        None => (rest, None),
    };
    let expanded = match (host.strip_prefix("~/"), home) {
        (Some(r), Some(h)) => h.join(r),
        _ if host == "~" && home.is_some() => home.unwrap().to_path_buf(),
        _ => PathBuf::from(host),
    };
    let abs = cwd
        .join(expanded)
        .canonicalize()
        .with_context(|| format!("share {s:?}: {host} does not exist"))?;
    let guest = match guest {
        Some(g) if !g.starts_with('/') => bail!("share {s:?}: the guest path must be absolute"),
        Some(g) => g.to_owned(),
        None => abs.to_string_lossy().into_owned(),
    };
    Ok(json!({"host_path": abs, "guest_path": guest, "read_only": ro}))
}

/// The VM request for the engine: Debian 13, Docker from apt, the user's SSH key, folders shared at matching paths.
pub fn engine_spec(o: &EngineOptions, user: &str, key: &str, shares: Vec<Value>) -> Value {
    json!({
        "name": o.name,
        "backend": "auto",
        "image": o.image.clone().unwrap_or_else(|| "debian-13".into()),
        "vcpus": o.cpus,
        "memory_mib": o.memory_mib,
        "disk_size_gib": o.disk_gib,
        "network": {"mode": "user", "forwards": []},
        "shared_folders": shares,
        "cloud_init": {
            "hostname": o.name,
            "user": user,
            "ssh_authorized_keys": [key],
            "packages": ["docker.io", "docker-compose", "docker-buildx", "curl", "ca-certificates", "openssl", "git", "jq"],
            "runcmd": [
                format!("usermod -aG docker {user}"),
                "systemctl enable --now docker".to_owned()
            ],
        },
    })
}

/// True when something answers the Docker Engine `/_ping` on this unix socket.
pub fn ping(socket: &Path) -> bool {
    let Ok(mut s) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(3)));
    if s.write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    ping_ok(&out)
}

fn ping_ok(response: &str) -> bool {
    response.starts_with("HTTP/1.1 200") && response.trim_end().ends_with("OK")
}

pub struct Engine {
    pub publish: Vec<(u16, u16)>,
    #[allow(dead_code)] // kept for callers that want to address the VM after `up`
    pub id: Uuid,
    pub ip: String,
    pub user: String,
    pub identity: Option<PathBuf>,
    pub socket: PathBuf,
}

pub(crate) fn user_name(o: &EngineOptions) -> Result<String> {
    o.user
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .filter(|u| {
            !u.is_empty()
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .context("could not tell which user to create in the VM; pass --user")
}

pub(crate) fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// Finds or creates the engine VM, starts it if stopped, and waits until its Docker answers over SSH.
pub async fn ensure_vm<A: VmApi>(api: &A, o: &EngineOptions) -> Result<Engine> {
    let home = home()?;
    let user = user_name(o)?;
    let (key, identity) = run::ssh_key(&home)?;
    let socket = socket_of(o);
    let started = Instant::now();
    let id = match api.find(&o.name).await? {
        Some((id, status)) => {
            if status != "running" {
                eprintln!("starting {} ({status})…", o.name);
                api.start(id).await?;
            }
            id
        }
        None => {
            let cwd = std::env::current_dir()?;
            let shares = o
                .shares
                .iter()
                .map(|s| parse_share(s, &cwd, Some(&home)))
                .collect::<Result<Vec<_>>>()?;
            eprintln!(
                "creating {} (first boot installs Docker, this takes a few minutes)…",
                o.name
            );
            let id = api.create(engine_spec(o, &user, &key, shares)).await?;
            let labels = std::collections::BTreeMap::from([(
                "fluxvm.docker-engine".to_owned(),
                "1".to_owned(),
            )]);
            let _ = api.set_labels(id, labels).await; // a convenience for `list`, not needed to work
            id
        }
    };
    let ip = run::wait_ssh(api, id, &user, &identity).await?;
    let mut last = 0u64;
    loop {
        let ready = run::ssh_base(&ip, &user, &identity)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "docker info >/dev/null 2>&1 || sudo docker info >/dev/null 2>&1",
            ])
            .stdin(Stdio::null())
            .status()
            .context("running ssh")?
            .success();
        if ready {
            break;
        }
        if started.elapsed() > FIRST_BOOT_BUDGET {
            bail!(
                "{}",
                run::with_guest_report(
                    format!(
                        "Docker did not come up in the VM within {}s",
                        FIRST_BOOT_BUDGET.as_secs()
                    ),
                    run::cloud_init_report(&ip, &user, &identity)
                )
            );
        }
        let secs = started.elapsed().as_secs();
        if secs / 30 > last {
            last = secs / 30;
            eprintln!("waiting for Docker in the guest ({secs}s)…");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Ok(Engine {
        publish: parse_publish_all(&o.publish)?,
        id,
        ip,
        user,
        identity,
        socket,
    })
}

/// `8080` or `18443:8443` -> (host port, guest port). Both are loopback ports; the Mac side is never exposed beyond 127.0.0.1.
pub fn parse_publish(s: &str) -> Result<(u16, u16)> {
    let port = |t: &str| -> Result<u16> {
        match t.parse::<u16>() {
            Ok(p) if p != 0 => Ok(p),
            _ => bail!("publish {s:?}: {t:?} is not a port from 1 to 65535"),
        }
    };
    match s.split_once(':') {
        None => port(s).map(|p| (p, p)),
        Some((h, g)) => Ok((port(h)?, port(g)?)),
    }
}

pub(crate) fn parse_publish_all(all: &[String]) -> Result<Vec<(u16, u16)>> {
    let mut out: Vec<(u16, u16)> = all
        .iter()
        .map(|s| parse_publish(s))
        .collect::<Result<_>>()?;
    out.sort_unstable();
    out.dedup();
    let mut seen = std::collections::BTreeSet::new();
    for (h, _) in &out {
        if !seen.insert(*h) {
            bail!("publish: host port {h} is used twice");
        }
    }
    Ok(out)
}

/// What the forwarder was started with, so a change of options restarts it instead of being ignored.
fn forward_spec(e: &Engine) -> String {
    let mut s = format!("{}@{}|{}", e.user, e.ip, e.socket.display());
    for (h, g) in &e.publish {
        s.push_str(&format!("|{h}:{g}"));
    }
    s
}

/// A detached `ssh -N` with these `-L` forwards. Options must precede the destination `ssh_base` already appended, so
/// the argument order is rebuilt. Shared with `fluxctl kube`.
pub(crate) fn forward_command(
    ip: &str,
    user: &str,
    identity: &Option<PathBuf>,
    forwards: &[String],
) -> Result<Command> {
    let base = run::ssh_base(ip, user, identity);
    let program = base.get_program().to_owned();
    let mut args: Vec<std::ffi::OsString> = base.get_args().map(|a| a.to_owned()).collect();
    let dest = args.pop().context("ssh destination missing")?;
    let mut c = Command::new(program);
    c.args(args);
    c.args([
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StreamLocalBindUnlink=yes",
        "-o",
        "BatchMode=yes",
        "-N",
    ]);
    for f in forwards {
        c.arg("-L").arg(f);
    }
    c.arg(dest);
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c.process_group(0); // survive Ctrl-C and the terminal closing
    Ok(c)
}

/// Starts the SSH forward of the guest's Docker socket (and any published ports) unless one with the same settings already
/// answers. Returns the forwarder's pid when it started a new one.
pub fn ensure_forward(e: &Engine) -> Result<Option<u32>> {
    let spec = forward_spec(e);
    let recorded = std::fs::read_to_string(pid_file(&e.socket))
        .ok()
        .and_then(|t| t.lines().nth(1).map(str::to_owned));
    if ping(&e.socket) && recorded.as_deref() == Some(spec.as_str()) {
        return Ok(None);
    }
    if let Some(dir) = e.socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    stop_forwarder(&e.socket);
    let _ = std::fs::remove_file(&e.socket);
    let mut forwards = vec![format!("{}:/var/run/docker.sock", e.socket.display())];
    forwards.extend(
        e.publish
            .iter()
            .map(|(h, g)| format!("127.0.0.1:{h}:127.0.0.1:{g}")),
    );
    let mut c = forward_command(&e.ip, &e.user, &e.identity, &forwards)?;
    let child = c.spawn().context("starting the SSH forward")?;
    let pid = child.id();
    std::fs::write(pid_file(&e.socket), format!("{pid}\n{spec}\n"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if ping(&e.socket) {
            return Ok(Some(pid));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!(
        "the SSH forward started but {} does not answer",
        e.socket.display()
    )
}

fn stop_forwarder(socket: &Path) {
    stop_recorded_ssh(&pid_file(socket), "docker.sock");
}

/// Stops the `ssh` whose pid is on the first line of `pidfile`, but only if its command line contains `needle`, so a
/// stale pid can never signal an unrelated process. Removes the pid file. Shared with `fluxctl kube`.
pub(crate) fn stop_recorded_ssh(pidfile: &Path, needle: &str) {
    if let Ok(text) = std::fs::read_to_string(pidfile)
        && let Some(first) = text.lines().next()
        && let Ok(pid) = first.trim().parse::<i32>()
        && pid > 1
        && is_our_ssh(pid, needle)
    {
        // SAFETY: kill with a pid read from our own pid file and confirmed by its command line.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pidfile);
}

fn is_our_ssh(pid: i32, needle: &str) -> bool {
    Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let c = String::from_utf8_lossy(&o.stdout);
            c.contains("ssh") && c.contains(needle)
        })
        .unwrap_or(false)
}

/// `fluxctl docker up`: idempotent. Returns the socket path.
pub async fn up<A: VmApi>(api: &A, o: &EngineOptions) -> Result<PathBuf> {
    let engine = ensure_vm(api, o).await?;
    match ensure_forward(&engine)? {
        Some(pid) => eprintln!("forwarding the guest's Docker socket (pid {pid})"),
        None => eprintln!("the socket already answers"),
    }
    Ok(engine.socket)
}

pub fn env_line(socket: &Path) -> String {
    format!("export DOCKER_HOST=unix://{}", socket.display())
}

/// `fluxctl docker status` as JSON.
pub async fn status<A: VmApi>(api: &A, o: &EngineOptions) -> Result<Value> {
    let socket = socket_of(o);
    let vm = api.find(&o.name).await?;
    let ip = match &vm {
        Some((id, _)) => api.guest_ip(*id).await.unwrap_or(None),
        None => None,
    };
    Ok(json!({
        "name": o.name,
        "vm": vm.as_ref().map(|(id, status)| json!({"id": id, "status": status})),
        "guest_ip": ip,
        "socket": socket,
        "socket_answers": ping(&socket),
        "env": env_line(&socket),
    }))
}

/// `fluxctl docker down`: stop the forward and the VM; `purge` also deletes the VM and its Docker data.
pub async fn down<A: VmApi>(api: &A, o: &EngineOptions, purge: bool) -> Result<()> {
    let socket = socket_of(o);
    stop_forwarder(&socket);
    let _ = std::fs::remove_file(&socket);
    if let Some((id, status)) = api.find(&o.name).await? {
        if purge {
            if status == "running" {
                api.stop(id).await?;
            }
            api.delete(id).await?;
            eprintln!("deleted {} and its Docker data", o.name);
        } else if status == "running" {
            api.stop(id).await?;
            eprintln!("stopped {}", o.name);
        }
    }
    Ok(())
}

/// `fluxctl docker supervise`: keeps the VM running and the socket answering, for launchd. Never returns.
pub async fn supervise<A: VmApi>(api: &A, o: &EngineOptions, every: Duration) -> Result<()> {
    loop {
        let socket = socket_of(o);
        let healthy =
            ping(&socket) && matches!(api.find(&o.name).await, Ok(Some((_, s))) if s == "running");
        if !healthy {
            match up(api, o).await {
                Ok(_) => eprintln!("docker engine is up"),
                Err(e) => eprintln!("docker engine is not ready, will retry: {e:#}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A LaunchAgent that runs `args` at login and restarts it if it exits. Shared with `fluxctl kube`.
pub(crate) fn render_plist(
    label: &str,
    args: &[String],
    log: &Path,
    env: &[(String, String)],
) -> String {
    let items: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let env_block = if env.is_empty() {
        String::new()
    } else {
        let rows: String = env
            .iter()
            .map(|(k, v)| {
                format!(
                    "    <key>{}</key><string>{}</string>\n",
                    xml_escape(k),
                    xml_escape(v)
                )
            })
            .collect();
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{rows}  </dict>\n")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n{items}  </array>\n{env_block}  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ThrottleInterval</key><integer>30</integer>\n  <key>StandardOutPath</key><string>{log}</string>\n  <key>StandardErrorPath</key><string>{log}</string>\n</dict>\n</plist>\n",
        log = xml_escape(&log.display().to_string()),
    )
}

/// Where the daemon is and which VZ runner it uses, carried into a plist. Never a token: nothing secret goes in a plist.
pub(crate) fn carried_env() -> Vec<(String, String)> {
    ["FLUXVM_URL", "FLUXVM_VZ_RUNNER"]
        .iter()
        .filter_map(|k| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), v))
        })
        .collect()
}

/// The LaunchAgent that runs `fluxctl docker supervise` at login and restarts it if it exits.
pub fn render_agent(
    fluxctl: &Path,
    o: &EngineOptions,
    log: &Path,
    env: &[(String, String)],
) -> String {
    let mut args = vec![
        fluxctl.display().to_string(),
        "docker".into(),
        "supervise".into(),
    ];
    args.push("--name".into());
    args.push(o.name.clone());
    args.push("--cpus".into());
    args.push(o.cpus.to_string());
    args.push("--memory-mib".into());
    args.push(o.memory_mib.to_string());
    args.push("--disk-gib".into());
    args.push(o.disk_gib.to_string());
    if let Some(i) = &o.image {
        args.push("--image".into());
        args.push(i.clone());
    }
    for s in &o.shares {
        args.push("--share".into());
        args.push(s.clone());
    }
    if let Some(s) = &o.socket {
        args.push("--socket".into());
        args.push(s.display().to_string());
    }
    if let Some(u) = &o.user {
        args.push("--user".into());
        args.push(u.clone());
    }
    for p in &o.publish {
        args.push("--publish".into());
        args.push(p.clone());
    }
    render_plist(AGENT_LABEL, &args, log, env)
}

fn agent_paths(home: &Path) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-docker-engine.log"),
    )
}

fn gui_domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Load (replacing any loaded copy of) the LaunchAgent in `plist`.
pub(crate) fn launchctl_load(label: &str, plist: &Path) -> Result<()> {
    let domain = gui_domain();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(plist)
        .output()
        .context("running launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Unload the agent and delete its plist; false when there was none.
pub(crate) fn launchctl_unload(label: &str, plist: &Path) -> Result<bool> {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{}/{label}", gui_domain())])
        .output();
    match std::fs::remove_file(plist) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// `fluxctl docker agent install`: write the LaunchAgent and load it.
pub fn agent_install(o: &EngineOptions) -> Result<PathBuf> {
    let home = home()?;
    let (plist, log) = agent_paths(&home);
    let exe = std::env::current_exe().context("finding fluxctl's own path")?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(&plist, render_agent(&exe, o, &log, &carried_env()))?;
    launchctl_load(AGENT_LABEL, &plist)?;
    Ok(plist)
}

/// `fluxctl docker agent uninstall`.
pub fn agent_uninstall() -> Result<bool> {
    let (plist, _) = agent_paths(&home()?);
    launchctl_unload(AGENT_LABEL, &plist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_docker_and_matching_shares() {
        let o = EngineOptions::default();
        let spec = engine_spec(
            &o,
            "mini",
            "ssh-ed25519 AAAA test",
            vec![
                json!({"host_path": "/Users/mini/ci", "guest_path": "/Users/mini/ci", "read_only": false}),
            ],
        );
        assert_eq!(spec["name"], "docker-engine");
        assert_eq!(spec["vcpus"], 2);
        assert_eq!(spec["memory_mib"], 3072);
        assert_eq!(spec["disk_size_gib"], 40);
        assert_eq!(spec["network"]["mode"], "user");
        let pk = spec["cloud_init"]["packages"].as_array().unwrap();
        for want in ["docker.io", "docker-compose", "docker-buildx"] {
            assert!(pk.iter().any(|p| p == want), "{want}");
        }
        // cloud-init runcmd entries are strings, not argv arrays.
        assert!(
            spec["cloud_init"]["runcmd"]
                .as_array()
                .unwrap()
                .iter()
                .all(Value::is_string)
        );
        assert_eq!(spec["cloud_init"]["user"], "mini");
        assert_eq!(spec["shared_folders"][0]["guest_path"], "/Users/mini/ci");
    }

    #[test]
    fn shares_default_to_the_same_path_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let v = parse_share(real.to_str().unwrap(), Path::new("/"), None).unwrap();
        assert_eq!(v["host_path"], real.to_str().unwrap());
        assert_eq!(v["guest_path"], real.to_str().unwrap());
        assert_eq!(v["read_only"], false);
        let ro = parse_share(
            &format!("{}:/work:ro", real.display()),
            Path::new("/"),
            None,
        )
        .unwrap();
        assert_eq!(
            (ro["guest_path"].as_str(), ro["read_only"].as_bool()),
            (Some("/work"), Some(true))
        );
        assert!(
            parse_share(
                &format!("{}:relative", real.display()),
                Path::new("/"),
                None
            )
            .is_err()
        );
        assert!(parse_share("/definitely/not/here", Path::new("/"), None).is_err());
    }

    #[test]
    fn ping_reply_parsing() {
        assert!(ping_ok("HTTP/1.1 200 OK\r\nApi-Version: 1.43\r\n\r\nOK"));
        assert!(!ping_ok("HTTP/1.1 500 Internal Server Error\r\n\r\nOK"));
        assert!(!ping_ok("HTTP/1.1 200 OK\r\n\r\nnope"));
        assert!(!ping_ok(""));
    }

    #[test]
    fn ping_against_a_real_socket() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = [0u8; 256];
            let n = s.read(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("GET /_ping"));
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .unwrap();
        });
        assert!(ping(&sock));
        t.join().unwrap();
        assert!(!ping(&dir.path().join("missing.sock")));
    }

    #[test]
    fn env_line_and_default_socket() {
        assert_eq!(
            env_line(Path::new("/tmp/x.sock")),
            "export DOCKER_HOST=unix:///tmp/x.sock"
        );
        let s = default_socket();
        assert!(s.to_string_lossy().starts_with("/tmp/fluxvm-run-"));
        assert_eq!(s.file_name().unwrap(), "docker-engine.sock");
        assert_eq!(
            pid_file(Path::new("/tmp/x.sock")),
            PathBuf::from("/tmp/x.sock.pid")
        );
    }

    #[test]
    fn agent_plist_carries_the_options_and_escapes() {
        let o = EngineOptions {
            name: "eng".into(),
            shares: vec!["/Users/a&b".into()],
            memory_mib: 4096,
            ..Default::default()
        };
        let env = vec![("FLUXVM_URL".to_owned(), "http://127.0.0.1:7788".to_owned())];
        let p = render_agent(
            Path::new("/opt/homebrew/bin/fluxctl"),
            &o,
            Path::new("/tmp/log"),
            &env,
        );
        assert!(p.contains("<key>FLUXVM_URL</key><string>http://127.0.0.1:7788</string>"));
        assert!(
            !render_agent(Path::new("/x"), &o, Path::new("/l"), &[])
                .contains("EnvironmentVariables")
        );
        assert!(p.contains(AGENT_LABEL));
        assert!(p.contains("<string>supervise</string>"));
        assert!(p.contains("<string>4096</string>"));
        assert!(p.contains("/Users/a&amp;b"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
    }

    #[test]
    fn publish_ports_parse_and_reject_clashes() {
        assert_eq!(parse_publish("8080").unwrap(), (8080, 8080));
        assert_eq!(parse_publish("18443:8443").unwrap(), (18443, 8443));
        for bad in ["", "0", "70000", "a", "1:", ":2", "1:2:3", "-5"] {
            assert!(parse_publish(bad).is_err(), "{bad:?}");
        }
        let ok = parse_publish_all(&["8080".into(), "8080".into(), "9000:90".into()]).unwrap();
        assert_eq!(ok, vec![(8080, 8080), (9000, 90)]);
        assert!(parse_publish_all(&["8080:1".into(), "8080:2".into()]).is_err());
    }

    #[test]
    fn forward_spec_changes_when_ports_change() {
        let mk = |publish: Vec<(u16, u16)>| Engine {
            publish,
            id: Uuid::nil(),
            ip: "192.168.64.5".into(),
            user: "mini".into(),
            identity: None,
            socket: PathBuf::from("/tmp/d.sock"),
        };
        let a = forward_spec(&mk(vec![]));
        let b = forward_spec(&mk(vec![(18443, 8443)]));
        assert_ne!(a, b);
        assert_eq!(b, "mini@192.168.64.5|/tmp/d.sock|18443:8443");
    }

    #[test]
    fn user_names_are_validated() {
        let ok = EngineOptions {
            user: Some("mini".into()),
            ..Default::default()
        };
        assert_eq!(user_name(&ok).unwrap(), "mini");
        for bad in ["", "a b", "x;rm", "a/b"] {
            let o = EngineOptions {
                user: Some(bad.into()),
                ..Default::default()
            };
            assert!(user_name(&o).is_err(), "{bad:?}");
        }
    }
}
