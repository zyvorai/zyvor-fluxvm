// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Data model, validation and the on-disk journal of `fluxctl kube apple`.
//!
//! The journal (`~/.fluxvm/apple-k3s/NAME.json`, mode 600) is written *before* every VM is created, so a crash between
//! "ask the daemon for a VM" and "remember its id" can be repaired from the ownership labels (see `recover`).
//! It never holds the cluster token.

pub use super::ha_model::*;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Label values on the VMs: `fluxvm.stack` (so `list_stack` finds them), `fluxvm.service` (role and node) and
/// `fluxvm.spec-hash`, which carries the owner id of the journal that created the VM.
pub const STACK_PREFIX: &str = "apple-k3s-";
/// Default Mac-side port of the API tunnel.
pub const DEFAULT_API_PORT: u16 = 16443;
pub const GUEST_API_PORT: u16 = 6443;
/// Nodes a cluster may have, to keep a typo from creating dozens of VMs.
pub const MAX_NODES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Server,
    Agent,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Agent => "agent",
        }
    }
    /// The systemd unit k3s runs as in this role.
    pub fn unit(self) -> &'static str {
        match self {
            Role::Server => "k3s",
            Role::Agent => "k3s-agent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Datastore {
    Sqlite,
    Etcd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// The journal exists; some VM may or may not.
    Creating,
    /// The VM exists; k3s is not yet installed and answering.
    Bootstrapping,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub role: Role,
    /// The VM's name in the daemon (`k3s-NODE`).
    pub vm_name: String,
    /// Unset until the daemon answered `create`; `recover` fills it in after a crash.
    #[serde(default)]
    pub id: Option<String>,
    /// The pinned SSH host key (public half, `ssh-ed25519 AAAA…`).
    pub host_key: String,
    /// True once the node answered Ready at least once.
    #[serde(default)]
    pub joined: bool,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Canary {
    pub nonce: String,
    pub boot_id: String,
    pub written_at: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Params {
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub image: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub name: String,
    pub owner: String,
    pub phase: Phase,
    pub user: String,
    pub api_port: u16,
    pub datastore: Datastore,
    /// For a record of nodes that join a cluster owned by another Mac: its API URL. There is no local server then.
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub tls_sans: Vec<String>,
    #[serde(default)]
    pub k3s_sha256: Option<String>,
    #[serde(default)]
    pub k3s_version: Option<String>,
    pub params: Params,
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub canary: Option<Canary>,
    /// Control-plane nodes the cluster is meant to have (`create --servers N`); 1 for an ordinary cluster.
    #[serde(default = "one_server")]
    pub servers_wanted: u8,
    /// Flags every server must agree on (k3s misbehaves when they differ).
    #[serde(default)]
    pub flags: SharedFlags,
    /// The stable registration endpoint of a multi-server cluster.
    #[serde(default)]
    pub endpoint: Option<EndpointCfg>,
    /// The scheduled-snapshot policy last applied (never credentials).
    #[serde(default)]
    pub snapshots: Option<SnapshotPolicy>,
    /// UTC stamps of the last rotation of the agent secret and of the server secret.
    #[serde(default)]
    pub agent_secret_rotated: Option<String>,
    #[serde(default)]
    pub server_secret_rotated: Option<String>,
}

impl State {
    pub fn servers(&self) -> Vec<&Node> {
        self.nodes
            .iter()
            .filter(|n| n.role == Role::Server)
            .collect()
    }
    /// True when the Mac reaches the API through the failover forwarder instead of one ssh tunnel.
    pub fn uses_proxy(&self) -> bool {
        self.servers_wanted > 1 || self.servers().len() > 1
    }
    /// The Mac-side port of the tunnel to the i-th server, in journal order (the forwarder's backends).
    pub fn backend_port(&self, i: usize) -> u16 {
        self.api_port + 1 + i as u16
    }
    pub fn next_server_name(&self) -> String {
        let mut i = 2;
        loop {
            let n = format!("{}-s{i}", self.name);
            if self.node(&n).is_none() {
                return n;
            }
            i += 1;
        }
    }
    pub fn server(&self) -> Option<&Node> {
        self.nodes.iter().find(|n| n.role == Role::Server)
    }
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }
    pub fn node_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.name == name)
    }
    /// Servers first, then agents: the order to upgrade and to start in; stop and delete use the reverse.
    pub fn ordered(&self) -> Vec<&Node> {
        let mut v: Vec<&Node> = self.nodes.iter().collect();
        v.sort_by_key(|n| n.role == Role::Agent);
        v
    }
    /// `known_hosts` text pinning every node, each under its own alias.
    pub fn known_hosts(&self) -> String {
        self.nodes
            .iter()
            .filter(|n| !n.host_key.is_empty())
            .map(|n| format!("{} {}\n", host_alias(&n.name), n.host_key))
            .collect()
    }
    pub fn next_agent_name(&self) -> String {
        let mut i = 1;
        loop {
            let n = format!("{}-a{i}", self.name);
            if self.node(&n).is_none() {
                return n;
            }
            i += 1;
        }
    }
}

pub fn stack_label(cluster: &str) -> String {
    format!("{STACK_PREFIX}{cluster}")
}

pub fn vm_name(node: &str) -> String {
    format!("k3s-{node}")
}

/// The name a node's host key is pinned under, so a recycled DHCP address never matches (or mismatches) another node.
pub fn host_alias(node: &str) -> String {
    format!("fluxvm-k3s-{node}")
}

/// `server.NODE` / `agent.NODE`, the value of the `fluxvm.service` label.
pub fn service_label(role: Role, node: &str) -> String {
    format!("{}.{node}", role.as_str())
}

pub fn parse_service_label(s: &str) -> Option<(Role, String)> {
    let (r, n) = s.split_once('.')?;
    let role = match r {
        "server" => Role::Server,
        "agent" => Role::Agent,
        _ => return None,
    };
    name_ok(n).then(|| (role, n.to_owned()))
}

/// A lowercase DNS label of at most 40 characters: a cluster or node name. It ends up in hostnames, VM names and shell text.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
}

/// A DNS name or IP for `--tls-san` and a join URL: plain characters only, because they are placed in a unit file.
pub fn san_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-:".contains(&c))
}

/// `https://HOST:PORT` of a server's API, with host and port of plain characters.
pub fn join_url_ok(u: &str) -> bool {
    u.strip_prefix("https://").is_some_and(|rest| {
        let (host, port) = rest.rsplit_once(':').unwrap_or((rest, "6443"));
        san_ok(host) && port.parse::<u16>().is_ok_and(|p| p != 0)
    })
}

/// A k3s release such as `v1.31.4+k3s1`.
pub fn k3s_version_ok(v: &str) -> bool {
    parse_version(v).is_some()
}

/// A k3s node token: plain characters only (it travels on stdin, but it is also checked before it is used anywhere).
pub fn token_ok(t: &str) -> bool {
    crate::kube_host::token_ok(t)
}

/// Hides a secret in text that is about to be shown (an ssh error, a log line).
pub fn redact(text: &str, secret: &str) -> String {
    if secret.len() < 4 {
        return text.to_owned();
    }
    text.replace(secret, "<redacted>")
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn sha256_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Reads the expected digest from `--sha256` or from a `sha256sum`-style file (the release's `sha256sum-arm64.txt`).
pub fn expected_sha(
    sha256: Option<&str>,
    sums_file: Option<&Path>,
    entry: &str,
) -> Result<Option<String>> {
    match (sha256, sums_file) {
        (Some(_), Some(_)) => bail!("give --sha256 or --sha256-file, not both"),
        (Some(h), None) => {
            if !sha256_ok(h) {
                bail!("--sha256 must be 64 hexadecimal characters");
            }
            Ok(Some(h.to_ascii_lowercase()))
        }
        (None, Some(f)) => {
            let text =
                std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
            let sums = crate::kube_host::parse_sums(&text);
            sums.get(entry)
                .cloned()
                .map(Some)
                .with_context(|| format!("{} has no entry for {entry}", f.display()))
        }
        (None, None) => Ok(None),
    }
}

/// Reads a binary and checks it against the published digest before anything is sent to a guest.
pub fn read_verified(path: &Path, expected: &str) -> Result<Vec<u8>> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let got = sha256_hex(&data);
    if got != expected.to_ascii_lowercase() {
        bail!(
            "{} does not match the expected SHA-256 (expected {expected}, got {got}); nothing was sent to the guest",
            path.display()
        );
    }
    Ok(data)
}

/// `(major, minor, patch, build)` of `v1.31.4+k3s1`.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = v.strip_prefix('v')?;
    let (core, build) = rest.split_once("+k3s")?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch, build.parse().ok()?))
}

/// What an upgrade from `from` to `to` may do: never a downgrade (unless allowed) and never more than one minor release at
/// a time, which is the Kubernetes skew policy.
pub fn upgrade_allowed(from: &str, to: &str, allow_downgrade: bool) -> Result<()> {
    let (Some(f), Some(t)) = (parse_version(from), parse_version(to)) else {
        bail!("cannot compare versions {from:?} and {to:?}");
    };
    if t < f && !allow_downgrade {
        bail!(
            "{to} is older than {from}; pass --allow-downgrade to go back (the datastore may not accept it)"
        );
    }
    if t.0 != f.0 || t.1.abs_diff(f.1) > 1 {
        bail!("{from} to {to} skips a minor release; upgrade one minor version at a time");
    }
    Ok(())
}

/// `~/.fluxvm/apple-k3s`, or `$FLUXVM_APPLE_K3S_HOME`.
pub fn default_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_APPLE_K3S_HOME") {
        return Ok(PathBuf::from(p));
    }
    Ok(crate::docker_host::home()?
        .join(".fluxvm")
        .join("apple-k3s"))
}

/// Journals on disk, one per cluster.
pub struct Store {
    pub root: PathBuf,
}

pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }
    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.json"))
    }
    pub fn known_hosts_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.known_hosts"))
    }
    pub fn tunnel_pid_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.tunnel"))
    }
    pub fn backups_dir(&self, name: &str) -> PathBuf {
        self.root.join("backups").join(name)
    }
    pub fn load(&self, name: &str) -> Result<Option<State>> {
        match std::fs::read_to_string(self.path(name)) {
            Ok(t) => {
                Ok(Some(serde_json::from_str(&t).with_context(|| {
                    format!("reading the journal of {name}")
                })?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn require(&self, name: &str) -> Result<State> {
        self.load(name)?.with_context(|| {
            format!("no apple k3s cluster named {name}; see `fluxctl kube apple list`")
        })
    }
    pub fn save(&self, s: &State) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_private(&self.known_hosts_path(&s.name), &s.known_hosts())?;
        write_private(&self.path(&s.name), &serde_json::to_string_pretty(s)?)
    }
    pub fn remove(&self, name: &str) {
        for p in [
            self.path(name),
            self.known_hosts_path(name),
            self.tunnel_pid_path(name),
        ] {
            let _ = std::fs::remove_file(p);
        }
    }
    pub fn list(&self) -> Result<Vec<State>> {
        let mut out = vec![];
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && let Some(s) = self.load(stem)?
            {
                out.push(s);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
    /// One operation at a time per cluster. A lock older than an hour is treated as left by a crash.
    pub fn lock(&self, name: &str) -> Result<Lock> {
        use std::io::Write;
        std::fs::create_dir_all(&self.root)?;
        let p = self.root.join(format!("{name}.lock"));
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Lock(p));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|a| a > std::time::Duration::from_secs(3600));
                    if !stale {
                        bail!(
                            "another operation on {name} is running (lock {}); remove the file if it is left over from a crash",
                            p.display()
                        );
                    }
                    let _ = std::fs::remove_file(&p);
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("could not lock {name}")
    }
}

/// Writes a file readable by this user only, replacing any old one atomically.
pub fn write_private(path: &Path, text: &str) -> Result<()> {
    write_private_bytes(path, text.as_bytes(), true)
}

/// `replace = false` refuses to overwrite (backups are never replaced).
pub fn write_private_bytes(path: &Path, data: &[u8], replace: bool) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if !replace {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {} (it must not exist)", path.display()))?;
        f.write_all(data)?;
        return Ok(f.sync_all()?);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn sample_state(name: &str) -> State {
    State {
        schema: 1,
        name: name.into(),
        owner: "owner1".into(),
        phase: Phase::Ready,
        user: "mini".into(),
        api_port: DEFAULT_API_PORT,
        datastore: Datastore::Sqlite,
        external_url: None,
        tls_sans: vec![],
        k3s_sha256: Some("a".repeat(64)),
        k3s_version: Some("v1.31.4+k3s1".into()),
        params: Params {
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 30,
            image: "debian-13".into(),
        },
        nodes: vec![Node {
            name: name.into(),
            role: Role::Server,
            vm_name: vm_name(name),
            id: Some("00000000-0000-0000-0000-000000000001".into()),
            host_key: "ssh-ed25519 AAAA".into(),
            joined: true,
            version: None,
        }],
        canary: None,
        servers_wanted: 1,
        flags: SharedFlags::default(),
        endpoint: None,
        snapshots: None,
        agent_secret_rotated: None,
        server_secret_rotated: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels() {
        assert!(name_ok("dev"));
        assert!(name_ok("dev-a1"));
        for bad in [
            "",
            "Dev",
            "-x",
            "x-",
            "a.b",
            "1x",
            "a b",
            "a;b",
            &"a".repeat(41),
        ] {
            assert!(!name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn join_urls_and_sans_are_plain() {
        assert!(join_url_ok("https://10.89.1.2:6443"));
        assert!(join_url_ok("https://cluster.example"));
        assert!(!join_url_ok("http://x:6443"));
        assert!(!join_url_ok("https://x:0"));
        assert!(!join_url_ok("https://x';rm:6443"));
        assert!(san_ok("cluster.example"));
        assert!(!san_ok("a b"));
        assert!(!san_ok("$(id)"));
    }

    #[test]
    fn service_labels_round_trip() {
        let s = service_label(Role::Agent, "dev-a1");
        assert_eq!(s, "agent.dev-a1");
        assert_eq!(
            parse_service_label(&s),
            Some((Role::Agent, "dev-a1".into()))
        );
        assert_eq!(parse_service_label("worker.x"), None);
        assert_eq!(parse_service_label("server.Bad"), None);
        assert_eq!(parse_service_label("noperiod"), None);
    }

    #[test]
    fn versions_parse_and_upgrade_policy_holds() {
        assert_eq!(parse_version("v1.31.4+k3s1"), Some((1, 31, 4, 1)));
        assert_eq!(parse_version("1.31.4"), None);
        assert_eq!(parse_version("v1.31+k3s1"), None);
        assert!(upgrade_allowed("v1.31.4+k3s1", "v1.31.5+k3s1", false).is_ok());
        assert!(upgrade_allowed("v1.31.4+k3s1", "v1.32.0+k3s1", false).is_ok());
        assert!(upgrade_allowed("v1.31.4+k3s1", "v1.33.0+k3s1", false).is_err());
        assert!(upgrade_allowed("v1.32.0+k3s1", "v1.31.4+k3s1", false).is_err());
        assert!(upgrade_allowed("v1.31.5+k3s1", "v1.31.4+k3s1", true).is_ok());
        assert!(upgrade_allowed("v1.31.4+k3s1", "junk", false).is_err());
    }

    #[test]
    fn redact_hides_the_secret_everywhere() {
        let t = "ssh: K10abc::server:xyz failed; again K10abc::server:xyz";
        let r = redact(t, "K10abc::server:xyz");
        assert!(!r.contains("K10abc"));
        assert_eq!(r.matches("<redacted>").count(), 2);
        assert_eq!(redact("abc", "a"), "abc");
    }

    #[test]
    fn expected_sha_from_flag_or_sums_file() {
        let h = "AB".repeat(32);
        assert_eq!(
            expected_sha(Some(&h), None, "k3s-arm64").unwrap(),
            Some(h.to_lowercase())
        );
        assert!(expected_sha(Some("zz"), None, "k3s-arm64").is_err());
        assert_eq!(expected_sha(None, None, "k3s-arm64").unwrap(), None);
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("sums.txt");
        std::fs::write(
            &f,
            format!("{}  k3s-arm64\n{}  k3s\n", "1".repeat(64), "2".repeat(64)),
        )
        .unwrap();
        assert_eq!(
            expected_sha(None, Some(&f), "k3s-arm64").unwrap(),
            Some("1".repeat(64))
        );
        assert!(expected_sha(None, Some(&f), "nope").is_err());
        assert!(expected_sha(Some(&h), Some(&f), "k3s-arm64").is_err());
    }

    #[test]
    fn read_verified_rejects_a_wrong_digest() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("k3s");
        std::fs::write(&f, b"binary").unwrap();
        assert!(read_verified(&f, &sha256_hex(b"binary")).is_ok());
        let e = read_verified(&f, &sha256_hex(b"other"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("nothing was sent"), "{e}");
    }

    #[test]
    fn store_saves_privately_and_pins_host_keys() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let st = Store::new(d.path().to_path_buf());
        assert!(st.load("dev").unwrap().is_none());
        let s = sample_state("dev");
        st.save(&s).unwrap();
        assert_eq!(st.load("dev").unwrap().unwrap(), s);
        let mode = std::fs::metadata(st.path("dev"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let kh = std::fs::read_to_string(st.known_hosts_path("dev")).unwrap();
        assert_eq!(kh, "fluxvm-k3s-dev ssh-ed25519 AAAA\n");
        assert_eq!(st.list().unwrap().len(), 1);
        st.remove("dev");
        assert!(st.load("dev").unwrap().is_none());
    }

    #[test]
    fn journal_never_contains_a_token_field() {
        let json = serde_json::to_string(&sample_state("dev")).unwrap();
        assert!(!json.to_lowercase().contains("token"));
    }

    #[test]
    fn lock_is_exclusive_and_released() {
        let d = tempfile::tempdir().unwrap();
        let st = Store::new(d.path().to_path_buf());
        let l = st.lock("dev").unwrap();
        assert!(st.lock("dev").is_err());
        drop(l);
        assert!(st.lock("dev").is_ok());
    }

    #[test]
    fn backups_are_never_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("b.tgz");
        write_private_bytes(&p, b"one", false).unwrap();
        assert!(write_private_bytes(&p, b"two", false).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
    }

    #[test]
    fn ordered_puts_servers_first_and_names_do_not_collide() {
        let mut s = sample_state("dev");
        let mut a = s.nodes[0].clone();
        a.name = "dev-a1".into();
        a.role = Role::Agent;
        s.nodes.insert(0, a);
        assert_eq!(s.ordered()[0].role, Role::Server);
        assert_eq!(s.next_agent_name(), "dev-a2");
    }
}
