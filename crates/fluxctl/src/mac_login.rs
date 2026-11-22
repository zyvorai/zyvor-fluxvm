// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac login|logout|logins`: a per-registry credential store that `mac pull`, `push`, `verify`, `sign` and `search` use when
//! `FLUXVM_REGISTRY_USER` is not set (the environment always wins).
//!
//! The file is `~/.config/fluxvm/mac-registry-auth.json` (override: `FLUXVM_MAC_REGISTRY_AUTH`), mode 0600, holding the password in
//! clear text like `~/.docker/config.json` without a credential helper. It is refused, for reading and for writing, while any
//! group or other permission bit is set. The password is taken from stdin or a no-echo prompt, never from an argument, and is never
//! printed: every message that could carry text from the network passes through [`scrub`].
//!
//! Verification is one `GET /v2/` (answering a Bearer challenge at the registry's token endpoint when asked). Nothing here has run
//! against a real registry: the tests use a loopback fake.

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

const HUB: &str = "registry-1.docker.io";

/// One stored login. `Debug` hides the password so a stray `{:?}` or a panic cannot print it.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Login {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Login {{ username: {:?}, password: <hidden> }}",
            self.username
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    registries: BTreeMap<String, Login>,
}

/// Where the logins live.
pub fn default_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_REGISTRY_AUTH").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")
        .context("HOME is not set (or set FLUXVM_MAC_REGISTRY_AUTH to a file path)")?;
    Ok(PathBuf::from(home).join(".config/fluxvm/mac-registry-auth.json"))
}

/// `https://GHCR.io/`, `ghcr.io/acme/x:1` and `ghcr.io` all name the registry `ghcr.io`; Docker Hub's spellings become one.
pub fn normalize_registry(s: &str) -> Result<String> {
    let t = s.trim();
    let t = t
        .strip_prefix("https://")
        .or_else(|| t.strip_prefix("http://"))
        .unwrap_or(t);
    let host = t.split('/').next().unwrap_or("").to_ascii_lowercase();
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')),
        "{s:?} is not a registry host (example: ghcr.io, or localhost:5000)"
    );
    Ok(if host == "docker.io" || host == "index.docker.io" {
        HUB.to_string()
    } else {
        host
    })
}

/// The registry an image reference points at (`ghcr.io/acme/x:1` gives `ghcr.io`); a bare host gives itself.
pub fn registry_of(reference: &str) -> Option<String> {
    match fluxvm_image::oci_registry::ImageRef::parse(reference) {
        Ok(r) if reference.contains('/') => Some(r.registry),
        _ => normalize_registry(reference).ok(),
    }
}

/// Remove `secret` from text that may have come from the network or a library error.
pub fn scrub(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<hidden>")
    }
}

#[cfg(unix)]
fn insecure(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o077 != 0
}
#[cfg(not(unix))]
fn insecure(_: &std::fs::Metadata) -> bool {
    false
}

fn refuse_text(path: &Path) -> String {
    format!(
        "MAC_REGISTRY_CREDS_PERMISSIONS: {} can be read by other users, so fluxctl will not use it. Run `chmod 600 {}` (or delete it and log in again), then retry. More: fluxctl explain MAC_REGISTRY_CREDS_PERMISSIONS",
        path.display(),
        path.display()
    )
}

fn load(path: &Path) -> Result<AuthFile> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AuthFile::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    ensure!(!insecure(&meta), "{}", refuse_text(path));
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // A parse error is not echoed: serde's message can quote file text.
    serde_json::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "{} is not a valid login file. Delete it (`rm {}`) and run `fluxctl mac login REGISTRY` again",
            path.display(),
            path.display()
        )
    })
}

fn save(path: &Path, file: &AuthFile) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        ensure!(!insecure(&meta), "{}", refuse_text(path));
    }
    let dir = path.parent().context("the login file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(serde_json::to_string_pretty(file)?.as_bytes())?;
    f.sync_all().ok();
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The stored login for `registry` in the file at `path`.
pub fn stored_in(path: &Path, registry: &str) -> Result<Option<Login>> {
    Ok(load(path)?
        .registries
        .get(&normalize_registry(registry)?)
        .cloned())
}

/// Credentials for the registry that `reference` (an image reference or a bare host) names: the environment first, then the file.
/// A file that is group or world accessible is not used; a warning says how to fix it and the request goes out anonymously.
pub fn credentials_for(reference: &str) -> Option<fluxvm_image::oci_registry::Credentials> {
    let path = default_path().ok();
    pick(crate::mac_cmd::env_creds(), path.as_deref(), reference)
}

fn pick(
    env: Option<fluxvm_image::oci_registry::Credentials>,
    path: Option<&Path>,
    reference: &str,
) -> Option<fluxvm_image::oci_registry::Credentials> {
    if env.is_some() {
        return env;
    }
    let registry = registry_of(reference)?;
    match stored_in(path?, &registry) {
        Ok(l) => l.map(|l| fluxvm_image::oci_registry::Credentials {
            username: l.username,
            password: l.password,
        }),
        Err(e) => {
            eprintln!("warning: not using saved registry logins: {e:#}");
            None
        }
    }
}

/// What a `/v2/` check found.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The registry accepted the credentials.
    Accepted,
    /// It answered and refused them (the HTTP status).
    Rejected(u16),
    /// No answer, or one this check cannot interpret (the reason).
    Unreachable(String),
}

fn base_url(registry: &str) -> String {
    let local = registry.starts_with("localhost") || registry.starts_with("127.0.0.1");
    format!("{}://{registry}/v2/", if local { "http" } else { "https" })
}

fn bearer_params(h: &str) -> Option<Vec<(String, String)>> {
    let rest = h
        .trim()
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("bearer "))?;
    let rest = &h.trim()[rest.len()..];
    let mut out = Vec::new();
    for part in rest.split(',') {
        // A scope such as `repository:a:pull,push` is split by the comma: the tail has no `=` and is skipped.
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        out.push((k.trim().to_string(), v.trim().trim_matches('"').to_string()));
    }
    Some(out)
}

/// One `GET /v2/` with the credentials, following a Bearer challenge to the token endpoint once.
pub async fn verify(registry: &str, username: &str, password: &str) -> Verdict {
    let http = match reqwest::Client::builder()
        .user_agent(concat!("fluxvm/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(h) => h,
        Err(e) => return Verdict::Unreachable(scrub(&e.to_string(), password)),
    };
    let url = base_url(registry);
    let first = match http
        .get(&url)
        .basic_auth(username, Some(password))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Verdict::Unreachable(scrub(&format!("{e:#}"), password)),
    };
    let status = first.status().as_u16();
    if status != 401 {
        return match status {
            200..=299 => Verdict::Accepted,
            403 => Verdict::Rejected(403),
            s => Verdict::Unreachable(format!(
                "{url} answered HTTP {s}, which is not a registry /v2/ answer"
            )),
        };
    }
    let challenge = first
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if challenge.trim().to_ascii_lowercase().starts_with("basic") {
        // The registry took Basic and refused it.
        return Verdict::Rejected(401);
    }
    let Some(params) = bearer_params(&challenge) else {
        return Verdict::Unreachable(format!(
            "{url} answered 401 with a challenge this check does not understand"
        ));
    };
    let Some(realm) = params
        .iter()
        .find(|(k, _)| k == "realm")
        .map(|(_, v)| v.clone())
    else {
        return Verdict::Unreachable("the registry's auth challenge has no realm".into());
    };
    let query: Vec<_> = params.into_iter().filter(|(k, _)| k != "realm").collect();
    match http
        .get(&realm)
        .query(&query)
        .basic_auth(username, Some(password))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Verdict::Accepted,
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => Verdict::Rejected(r.status().as_u16()),
        Ok(r) => Verdict::Unreachable(format!("the token endpoint answered HTTP {}", r.status())),
        Err(e) => Verdict::Unreachable(scrub(&format!("token endpoint: {e:#}"), password)),
    }
}

/// Read a password with echo off from the terminal.
#[cfg(unix)]
fn prompt_secret(prompt: &str) -> Result<String> {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: termios is plain data, filled by tcgetattr on a valid descriptor.
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::tcgetattr(fd, &mut saved) } == 0,
        "cannot turn off echo on this terminal; pipe the password instead: `printf %s \"$TOKEN\" | fluxctl mac login REGISTRY -u USER --password-stdin`"
    );
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    // SAFETY: as above; the saved state is restored below on every path.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
    eprint!("{prompt}");
    let mut line = String::new();
    let read = std::io::stdin().read_line(&mut line);
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    eprintln!();
    read.context("reading the password")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
#[cfg(not(unix))]
fn prompt_secret(_: &str) -> Result<String> {
    bail!("no-echo prompts are not supported here; use --password-stdin")
}

/// The first line of `r`, without its terminator.
pub fn read_secret_line(mut r: impl Read) -> Result<String> {
    let mut buf = String::new();
    r.read_to_string(&mut buf)
        .context("reading the password from stdin")?;
    Ok(buf
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string())
}

#[derive(Subcommand, Debug)]
pub enum LoginCommand {
    /// Save a registry login that `mac pull`, `push`, `verify`, `sign` and `search` then use by themselves (the environment variables
    /// FLUXVM_REGISTRY_USER / FLUXVM_REGISTRY_PASSWORD still win). The password is read from stdin or typed at a no-echo prompt, never
    /// taken from an argument. The login is checked with one request to the registry; when none can be reached it is saved
    /// anyway and the output says plainly that it was not verified. Saved in ~/.config/fluxvm/mac-registry-auth.json (mode 0600).
    #[command(
        after_help = "Examples:\n  fluxctl mac login ghcr.io -u alice                 (asks for the token without echo)\n  printf %s \"$TOKEN\" | fluxctl mac login ghcr.io -u alice --password-stdin\n  fluxctl mac login localhost:5000 -u ci --password-stdin < token.txt\n\nUse an access token, not an account password. The file holds it in clear text (like ~/.docker/config.json) and is refused if other users can read it. Remove it with `fluxctl mac logout ghcr.io`. If something fails: fluxctl explain MAC_REGISTRY_LOGIN_FAILED"
    )]
    Login {
        registry: String,
        /// User name (asked for when omitted and stdin is a terminal).
        #[arg(short = 'u', long)]
        username: Option<String>,
        /// Read the password from stdin (first line) instead of a prompt.
        #[arg(long)]
        password_stdin: bool,
        #[arg(long)]
        json: bool,
    },
    /// Forget the saved login for a registry, or every saved login with `--all`. Does not revoke the token at the registry.
    #[command(
        after_help = "Examples:\n  fluxctl mac logout ghcr.io\n  fluxctl mac logout --all\n\nTo make a leaked token useless, revoke it in the registry's own settings; removing it here only stops fluxctl using it."
    )]
    Logout {
        #[arg(required_unless_present = "all")]
        registry: Option<String>,
        #[arg(long, conflicts_with = "registry")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// The registries you are logged in to and as whom. Never prints a password.
    Logins {
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: LoginCommand) -> Result<()> {
    let path = default_path()?;
    match cmd {
        LoginCommand::Login {
            registry,
            username,
            password_stdin,
            json,
        } => {
            let (user, pass) = collect(username, password_stdin)?;
            let out = login(&path, &registry, &user, &pass).await?;
            print_outcome(&out, json)
        }
        LoginCommand::Logout {
            registry,
            all,
            json,
        } => {
            let removed = logout(&path, registry.as_deref(), all)?;
            if json {
                println!("{}", json!({"removed": removed}));
            } else if removed.is_empty() {
                println!(
                    "nothing to remove: no saved login{}",
                    if all { "s" } else { " for that registry" }
                );
            } else {
                for r in &removed {
                    println!("logged out of {r}");
                }
                println!("(the token itself is not revoked at the registry)");
            }
            Ok(())
        }
        LoginCommand::Logins { json } => {
            let list = logins(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&logins_json(&path)?)?);
            } else if list.is_empty() {
                println!("no saved registry logins. Add one: fluxctl mac login REGISTRY -u USER");
            } else {
                for (r, u) in &list {
                    println!("{r}  {u}");
                }
            }
            Ok(())
        }
    }
}

fn collect(username: Option<String>, password_stdin: bool) -> Result<(String, String)> {
    let tty = std::io::stdin().is_terminal();
    let user = match username {
        Some(u) => u,
        None if tty && !password_stdin => {
            eprint!("Username: ");
            let mut l = String::new();
            std::io::stdin().read_line(&mut l)?;
            l.trim().to_string()
        }
        None => bail!(
            "pass the user name with -u USER (the password then comes from stdin with --password-stdin)"
        ),
    };
    ensure!(!user.is_empty(), "the user name is empty: pass -u USER");
    let pass = if password_stdin {
        read_secret_line(std::io::stdin())?
    } else if tty {
        prompt_secret("Password or token (not shown): ")?
    } else {
        bail!("no terminal to ask for the password on: pipe it in with --password-stdin")
    };
    ensure!(!pass.is_empty(), "the password is empty");
    Ok((user, pass))
}

/// The result of `login`, for text and `--json`. No password.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LoginOutcome {
    pub registry: String,
    pub username: String,
    pub saved: bool,
    pub verified: bool,
    /// What was not checked, in plain words.
    pub not_verified: Vec<String>,
}

fn print_outcome(o: &LoginOutcome, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(o)?);
        return Ok(());
    }
    println!(
        "{} login for {} to {}",
        if o.verified {
            "verified and saved"
        } else {
            "saved"
        },
        o.username,
        o.registry
    );
    for n in &o.not_verified {
        println!("NOT verified: {n}");
    }
    Ok(())
}

pub async fn login(path: &Path, registry: &str, user: &str, pass: &str) -> Result<LoginOutcome> {
    let reg = normalize_registry(registry)?;
    // Check the file is usable before any network call, so a bad mode is reported first.
    let mut file = load(path)?;
    let verdict = verify(&reg, user, pass).await;
    let mut not_verified = vec![
        "whether this login may pull or push a particular repository (only that the registry accepts the credentials)".to_string(),
    ];
    let verified = match verdict {
        Verdict::Accepted => true,
        Verdict::Rejected(s) => bail!(
            "MAC_REGISTRY_LOGIN_FAILED: {reg} refused the login for {user} (HTTP {s}). Nothing was saved. Check the user name and use an access token (not the account password) that is still valid; then run `fluxctl mac login {reg} -u {user}` again. More: fluxctl explain MAC_REGISTRY_LOGIN_FAILED"
        ),
        Verdict::Unreachable(why) => {
            not_verified.insert(
                0,
                format!(
                    "whether {reg} accepts it: the registry could not be checked ({}). Check the address and your network; the first `mac pull` will show if the login is wrong",
                    scrub(&why, pass)
                ),
            );
            false
        }
    };
    file.registries.insert(
        reg.clone(),
        Login {
            username: user.to_string(),
            password: pass.to_string(),
        },
    );
    save(path, &file)?;
    Ok(LoginOutcome {
        registry: reg,
        username: user.to_string(),
        saved: true,
        verified,
        not_verified,
    })
}

/// Remove one login (or all); returns the registries removed.
pub fn logout(path: &Path, registry: Option<&str>, all: bool) -> Result<Vec<String>> {
    let mut file = load(path)?;
    let removed: Vec<String> = if all {
        std::mem::take(&mut file.registries).into_keys().collect()
    } else {
        let reg = normalize_registry(registry.context("name a registry, or pass --all")?)?;
        file.registries
            .remove(&reg)
            .map(|_| vec![reg])
            .unwrap_or_default()
    };
    if !removed.is_empty() {
        save(path, &file)?;
    }
    Ok(removed)
}

/// `(registry, user)` pairs, no passwords.
pub fn logins(path: &Path) -> Result<Vec<(String, String)>> {
    Ok(load(path)?
        .registries
        .into_iter()
        .map(|(r, l)| (r, l.username))
        .collect())
}

/// The logins as JSON without secrets (what a daemon route or a status page may show).
pub fn logins_json(path: &Path) -> Result<Value> {
    Ok(json!(
        logins(path)?
            .into_iter()
            .map(|(r, u)| json!({"registry": r, "username": u}))
            .collect::<Vec<_>>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SECRET: &str = "s3cr3t-T0KEN-xyz";

    /// A loopback registry. `mode`: "basic" answers /v2/ 200 for the right Basic header, "bearer" sends a challenge pointing at its
    /// own /token, "basic401" always answers 401 with a Basic challenge.
    async fn fake(mode: &'static str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    break;
                };
                let seen = s2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = c.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let auth = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().push(format!("{path}|{auth}"));
                    use base64::Engine;
                    let good = format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(format!("alice:{SECRET}"))
                    );
                    let ok = auth.contains(&good);
                    let resp = if path.starts_with("/token") {
                        if ok {
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 14\r\nconnection: close\r\n\r\n{\"token\":\"tk\"}".to_string()
                        } else {
                            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                        }
                    } else if mode == "basic" && ok {
                        "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
                            .to_string()
                    } else if mode == "bearer" {
                        format!(
                            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Bearer realm=\"http://{}/token\",service=\"fake\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                            addr
                        )
                    } else {
                        "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm=\"fake\"\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
                    };
                    c.write_all(resp.as_bytes()).await.ok();
                });
            }
        });
        (addr.to_string(), seen)
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn registry_names_are_normalised() {
        assert_eq!(normalize_registry("https://GHCR.io/").unwrap(), "ghcr.io");
        assert_eq!(normalize_registry("ghcr.io/acme/x:1").unwrap(), "ghcr.io");
        assert_eq!(normalize_registry("docker.io").unwrap(), HUB);
        assert_eq!(
            normalize_registry("localhost:5000").unwrap(),
            "localhost:5000"
        );
        assert!(normalize_registry("").is_err() && normalize_registry("a b").is_err());
        assert_eq!(registry_of("ghcr.io/acme/x:1").as_deref(), Some("ghcr.io"));
        assert_eq!(registry_of("ghcr.io").as_deref(), Some("ghcr.io"));
        assert_eq!(
            registry_of("localhost:5000/a/b:2").as_deref(),
            Some("localhost:5000")
        );
    }

    #[tokio::test]
    async fn login_verifies_basic_saves_0600_and_never_prints_the_secret() {
        let (addr, _) = fake("basic").await;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cfg/auth.json");
        let o = login(&p, &addr, "alice", SECRET).await.unwrap();
        assert!(o.verified && o.saved);
        assert_eq!(mode_of(&p), 0o600);
        let st = stored_in(&p, &addr).unwrap().unwrap();
        assert_eq!(
            (st.username.as_str(), st.password.as_str()),
            ("alice", SECRET)
        );
        // Every output path: JSON outcome, Debug of outcome and of the stored login, the listing, the logins JSON.
        let outputs = [
            serde_json::to_string(&o).unwrap(),
            format!("{o:?}"),
            format!("{st:?}"),
            format!("{:?}", logins(&p).unwrap()),
            logins_json(&p).unwrap().to_string(),
        ];
        for t in outputs {
            assert!(!t.contains(SECRET), "{t}");
        }
    }

    #[tokio::test]
    async fn login_follows_a_bearer_challenge() {
        let (addr, seen) = fake("bearer").await;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        assert!(login(&p, &addr, "alice", SECRET).await.unwrap().verified);
        assert!(seen.lock().unwrap().iter().any(|s| s.starts_with("/token")));
        let e = login(&p, &addr, "alice", "wrong")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_REGISTRY_LOGIN_FAILED") && e.contains("Nothing was saved"),
            "{e}"
        );
        assert!(!e.contains("wrong"));
        // The earlier good login is still there.
        assert_eq!(stored_in(&p, &addr).unwrap().unwrap().password, SECRET);
    }

    #[tokio::test]
    async fn rejected_login_is_not_saved_and_error_has_no_secret() {
        let (addr, _) = fake("basic401").await;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        let e = login(&p, &addr, "alice", SECRET).await.unwrap_err();
        let t = format!("{e:#} {e:?}");
        assert!(
            t.contains("MAC_REGISTRY_LOGIN_FAILED") && !t.contains(SECRET),
            "{t}"
        );
        assert!(!p.exists());
    }

    #[tokio::test]
    async fn unreachable_registry_saves_and_says_what_was_not_verified() {
        // Port 1 on loopback: connection refused.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        let o = login(&p, "127.0.0.1:1", "alice", SECRET).await.unwrap();
        assert!(o.saved && !o.verified);
        assert!(o.not_verified.len() >= 2 && o.not_verified[0].contains("could not be checked"));
        let j = serde_json::to_string(&o).unwrap();
        assert!(!j.contains(SECRET), "{j}");
        assert!(!o.not_verified.join(" ").contains(SECRET));
    }

    #[test]
    fn group_or_world_accessible_file_is_refused_for_read_and_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        let mut f = AuthFile::default();
        f.registries.insert(
            "r.io".into(),
            Login {
                username: "u".into(),
                password: SECRET.into(),
            },
        );
        save(&p, &f).unwrap();
        for mode in [0o644, 0o640, 0o604, 0o660] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = stored_in(&p, "r.io").unwrap_err().to_string();
            assert!(
                e.contains("MAC_REGISTRY_CREDS_PERMISSIONS") && e.contains("chmod 600"),
                "{e}"
            );
            assert!(!e.contains(SECRET));
            assert!(save(&p, &f).is_err());
            assert!(logout(&p, None, true).is_err());
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(stored_in(&p, "r.io").unwrap().is_some());
    }

    #[test]
    fn corrupt_file_error_does_not_quote_its_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        std::fs::write(&p, format!("{{\"registries\": {SECRET}")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let e = format!("{:#}", stored_in(&p, "r.io").unwrap_err());
        assert!(
            !e.contains(SECRET) && e.contains("not a valid login file"),
            "{e}"
        );
    }

    #[test]
    fn logout_one_all_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        let mut f = AuthFile::default();
        for r in ["a.io", "b.io"] {
            f.registries.insert(
                r.into(),
                Login {
                    username: "u".into(),
                    password: SECRET.into(),
                },
            );
        }
        save(&p, &f).unwrap();
        assert_eq!(logout(&p, Some("https://A.io/"), false).unwrap(), ["a.io"]);
        assert!(logout(&p, Some("a.io"), false).unwrap().is_empty());
        assert_eq!(logout(&p, None, true).unwrap(), ["b.io"]);
        assert!(logins(&p).unwrap().is_empty());
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn env_wins_and_a_login_is_used_only_for_its_own_registry() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json");
        let mut f = AuthFile::default();
        f.registries.insert(
            "ghcr.io".into(),
            Login {
                username: "u".into(),
                password: SECRET.into(),
            },
        );
        save(&p, &f).unwrap();
        let env = fluxvm_image::oci_registry::Credentials {
            username: "env".into(),
            password: "e".into(),
        };
        assert_eq!(
            pick(Some(env), Some(&p), "ghcr.io/a/b:1").unwrap().username,
            "env"
        );
        assert_eq!(
            pick(None, Some(&p), "ghcr.io/a/b:1").unwrap().password,
            SECRET
        );
        assert!(
            pick(None, Some(&p), "quay.io/a/b:1").is_none(),
            "a login must not leak to another registry"
        );
        assert!(pick(None, Some(&dir.path().join("none.json")), "ghcr.io/a/b").is_none());
        // An unsafe file means anonymous, not a failure and not a read.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(pick(None, Some(&p), "ghcr.io/a/b:1").is_none());
    }

    #[test]
    fn password_comes_from_the_first_stdin_line_only() {
        assert_eq!(read_secret_line(&b"tok\r\nsecond"[..]).unwrap(), "tok");
        assert_eq!(read_secret_line(&b""[..]).unwrap(), "");
    }

    #[test]
    fn scrub_removes_the_secret_from_library_text() {
        assert_eq!(scrub(&format!("x {SECRET} y"), SECRET), "x <hidden> y");
        assert_eq!(scrub("abc", ""), "abc");
    }

    #[test]
    fn login_has_no_password_argument() {
        use clap::CommandFactory;
        #[derive(clap::Parser)]
        struct P {
            #[command(subcommand)]
            c: LoginCommand,
        }
        let cmd = P::command();
        let login = cmd.find_subcommand("login").unwrap();
        assert!(!login.get_arguments().any(|a| a.get_id().as_str().contains("password") && a.get_id() != "password_stdin"));
        assert!(
            P::command()
                .try_get_matches_from(["x", "login", "r.io", "--password", "x"])
                .is_err()
        );
        assert!(P::command().try_get_matches_from(["x", "logout"]).is_err());
        assert!(
            P::command()
                .try_get_matches_from(["x", "logout", "--all", "r.io"])
                .is_err()
        );
        assert!(
            P::command()
                .try_get_matches_from(["x", "logout", "--all"])
                .is_ok()
        );
    }
}
