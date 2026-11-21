// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The VM request, the shell scripts run in the guests and the ssh command lines of `fluxctl kube apple`, as pure
//! functions of validated inputs. No script here takes the cluster token: it only ever travels on stdin.

use super::model::*;
use serde_json::{Value, json};
use std::path::Path;

pub const K3S_BIN: &str = "/usr/local/bin/k3s";
pub const TOKEN_PATH: &str = "/etc/rancher/k3s/token";
pub const AGENT_TOKEN_PATH: &str = "/etc/rancher/k3s/agent-token";
pub const SNAPSHOT_DIR: &str = "/var/lib/rancher/k3s/server/db/snapshots";
pub const SERVER_DIR: &str = "/var/lib/rancher/k3s/server";
pub const HOSTKEY_GUEST: &str = "/etc/fluxvm/ssh_host_ed25519_key";
pub const CANARY_FILE: &str = "/var/lib/rancher/k3s/.fluxvm-canary";
pub const CANARY_CM: &str = "fluxvm-persist-canary";

/// A host key pair made on the Mac for one node.
#[derive(Clone, Debug)]
pub struct HostKey {
    pub private: String,
    /// `ssh-ed25519 AAAA…`, without a comment.
    pub public: String,
}

/// Normalises a public key line to `type base64`.
pub fn pub_key_line(raw: &str) -> String {
    raw.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

/// The VM request for one node. The only secrets in it are the node's own SSH host key (so the Mac can pin it before the
/// first connection) and the user's public key; the cluster token is not in it.
pub fn node_spec(
    p: &Params,
    cluster: &str,
    node: &str,
    user: &str,
    ssh_pub: &str,
    key: &HostKey,
) -> Value {
    let dropin = format!("HostKey {HOSTKEY_GUEST}\n");
    json!({
        "name": vm_name(node),
        "backend": "auto",
        "image": p.image,
        "vcpus": p.cpus,
        "memory_mib": p.memory_mib,
        "disk_size_gib": p.disk_gib,
        "network": {"mode": "user", "forwards": []},
        // One private switch per cluster: the second card carries node-to-node and pod traffic.
        "apple": {"networks": [{"name": cluster}]},
        "shared_folders": [],
        "cloud_init": {
            "hostname": node,
            "user": user,
            "ssh_authorized_keys": [ssh_pub],
            "packages": [],
            "write_files": [
                {"path": HOSTKEY_GUEST, "content": key.private, "permissions": "0600"},
                {"path": format!("{HOSTKEY_GUEST}.pub"), "content": format!("{}\n", key.public), "permissions": "0644"},
                {"path": "/etc/ssh/sshd_config.d/00-fluxvm-hostkey.conf", "content": dropin, "permissions": "0644"},
            ],
            // cloud-init's own ssh module may replace /etc/ssh host keys; ours lives elsewhere and is read first.
            "runcmd": ["systemctl restart ssh || systemctl restart sshd || true"],
        },
    })
}

/// Reads the guest's boot id and architecture; the first command run in every node.
pub fn preflight_script() -> String {
    "set -eu; test \"$(uname -m)\" = aarch64 || { echo 'guest is not arm64' >&2; exit 21; }; \
     command -v systemctl >/dev/null || { echo 'guest has no systemd' >&2; exit 21; }; \
     sudo -n true || { echo 'passwordless sudo is required' >&2; exit 21; }; \
     command -v sha256sum >/dev/null || { echo 'sha256sum is missing' >&2; exit 21; }"
        .into()
}

/// Receives a k3s binary on stdin into a private temporary file, verifies its digest inside the guest and installs it.
/// `target` is where it goes. With `replace = false` an existing, different binary is refused (exit 22).
pub fn stage_binary_script(expected: &str, target: &str, replace: bool) -> String {
    assert!(sha256_ok(expected), "digest validated by the caller");
    let existing = if replace {
        format!("sudo -n install -m 0755 \"$d/k3s\" {target}")
    } else {
        format!(
            "if test -e {target}; then cmp -s {target} \"$d/k3s\" || {{ echo 'a different k3s is already installed' >&2; exit 22; }}; else sudo -n install -m 0755 \"$d/k3s\" {target}; fi"
        )
    };
    format!(
        "set -eu; d=$(mktemp -d); trap 'rm -rf \"$d\"' EXIT; cat > \"$d/k3s\"; \
         echo '{expected}  '\"$d/k3s\" | sha256sum -c - >&2 || {{ echo 'k3s checksum mismatch in the guest' >&2; exit 23; }}; {existing}"
    )
}

/// Streams the installed binary out of a guest (to copy a verified k3s to another node).
pub fn read_binary_script() -> String {
    format!("set -eu; sudo -n cat {K3S_BIN}")
}

pub fn sha_of_installed_script() -> String {
    format!("set -eu; sudo -n sha256sum {K3S_BIN} | cut -d' ' -f1")
}

pub fn preinstalled_check_script(role: Role) -> String {
    format!(
        "set -eu; test -x {K3S_BIN} || {{ echo 'k3s is not installed in this image' >&2; exit 24; }}; \
         systemctl cat {}.service >/dev/null 2>&1 || {{ echo 'no {}.service in this image' >&2; exit 24; }}",
        role.unit(),
        role.unit()
    )
}

/// What the unit's `ExecStart` runs for a node.
#[derive(Clone, Debug)]
pub struct UnitOpts<'a> {
    pub role: Role,
    pub node: &'a str,
    pub tls_sans: &'a [String],
    /// A server that starts a new embedded-etcd cluster.
    pub cluster_init: bool,
    /// The API URL to join (agents, and servers joining an etcd cluster).
    pub join_url: Option<&'a str>,
    /// The token was delivered to [`TOKEN_PATH`] before the service starts.
    pub token_file: bool,
    /// A server reads the token agents join with from [`AGENT_TOKEN_PATH`], so agents never hold the server token.
    pub agent_token_file: bool,
    /// The flags every server of the cluster shares (servers only; `None` means just `--disable traefik`).
    pub flags: Option<&'a SharedFlags>,
}

pub fn exec_args(o: &UnitOpts) -> String {
    let mut a: Vec<String> = vec![];
    match o.role {
        Role::Server => {
            a.push("server".into());
            match o.flags {
                Some(f) => a.extend(f.args()),
                None => a.extend(["--disable", "traefik"].map(String::from)),
            }
            a.extend(["--write-kubeconfig-mode", "600"].map(String::from));
            a.extend(["--tls-san".into(), "127.0.0.1".into()]);
            for s in o.tls_sans {
                a.extend(["--tls-san".into(), s.clone()]);
            }
            if o.cluster_init {
                a.push("--cluster-init".into());
            }
        }
        Role::Agent => a.push("agent".into()),
    }
    if let Some(u) = o.join_url {
        a.extend(["--server".into(), u.to_owned()]);
    }
    if o.token_file {
        a.extend(["--token-file".into(), TOKEN_PATH.into()]);
    }
    if o.agent_token_file && o.role == Role::Server {
        a.extend(["--agent-token-file".into(), AGENT_TOKEN_PATH.into()]);
    }
    a.extend(["--node-name".into(), o.node.to_owned()]);
    a.extend([
        "--node-ip".into(),
        "$PRIV".into(),
        "--flannel-iface".into(),
        "$IFACE".into(),
    ]);
    a.join(" ")
}

/// Writes `/etc/systemd/system/<unit>.service` and starts it. The private-network address and card are looked up in the
/// guest (they are assigned by the host's switch), the rest is validated text.
pub fn install_unit_script(o: &UnitOpts) -> String {
    let unit = o.role.unit();
    let exec = exec_args(o);
    format!(
        "set -eu; PRIV=''; for i in $(seq 1 90); do PRIV=$(ip -4 -o addr show | grep -o '10\\.89\\.[0-9]*\\.[0-9]*' | head -1 || true); [ -n \"$PRIV\" ] && break; sleep 2; done; \
         [ -n \"$PRIV\" ] || {{ echo 'no address on the private network card' >&2; exit 25; }}; \
         IFACE=$(ip -o -4 addr show | grep -F \"$PRIV/\" | cut -d' ' -f2); \
         sudo -n modprobe br_netfilter overlay 2>/dev/null || true; \
         sudo -n tee /etc/systemd/system/{unit}.service >/dev/null <<UNIT\n\
[Unit]\n\
Description=FluxVM K3s ({role})\n\
Wants=network-online.target\n\
After=network-online.target\n\
[Service]\n\
Type=notify\n\
ExecStart={K3S_BIN} {exec}\n\
KillMode=process\n\
Delegate=yes\n\
LimitNOFILE=1048576\n\
Restart=always\n\
RestartSec=5s\n\
TimeoutStartSec=0\n\
[Install]\n\
WantedBy=multi-user.target\n\
UNIT\n\
sudo -n systemctl daemon-reload && sudo -n systemctl enable --now {unit}.service",
        role = o.role.as_str()
    )
}

/// Receives the token on stdin into a root-only file. The token is never an argument or part of a script.
pub fn write_token_script() -> String {
    format!(
        "set -eu; sudo -n install -d -m 0700 /etc/rancher/k3s; sudo -n sh -c 'umask 077; cat > {TOKEN_PATH}'"
    )
}

/// Receives the agent token on stdin into a root-only file, before the server starts.
pub fn write_agent_token_script() -> String {
    format!(
        "set -eu; sudo -n install -d -m 0700 /etc/rancher/k3s; sudo -n sh -c 'umask 077; cat > {AGENT_TOKEN_PATH}'"
    )
}

/// The token a joining node needs: agents get the separate agent token (it cannot make them servers); a joining server
/// needs the server token.
pub fn read_token_script(role: Role) -> String {
    match role {
        Role::Agent => format!("set -eu; sudo -n cat {AGENT_TOKEN_PATH}"),
        // A server that joined (or was rotated) keeps the cluster's server token in its own token file; the first server
        // otherwise keeps it where k3s generated it.
        Role::Server => format!(
            "set -eu; if sudo -n test -s {TOKEN_PATH}; then sudo -n cat {TOKEN_PATH}; else sudo -n cat {SERVER_DIR}/token; fi"
        ),
    }
}

// ---- etcd snapshots ---------------------------------------------------------------------------------------------

/// A snapshot name or prefix: plain characters only.
pub fn snapshot_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 120
        && !n.starts_with('-')
        && !n.starts_with('.')
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub fn snapshot_save_script(name: &str) -> String {
    assert!(snapshot_name_ok(name));
    format!(
        "set -eu; sudo -n {K3S_BIN} etcd-snapshot save --name {name} >&2; sudo -n {K3S_BIN} etcd-snapshot ls 2>/dev/null"
    )
}

pub fn snapshot_ls_script() -> String {
    format!("set -eu; sudo -n {K3S_BIN} etcd-snapshot ls")
}

pub fn snapshot_delete_script(name: &str) -> String {
    assert!(snapshot_name_ok(name));
    format!(
        "set -eu; sudo -n test -f {SNAPSHOT_DIR}/{name}; sudo -n {K3S_BIN} etcd-snapshot delete {name}"
    )
}

pub fn snapshot_prune_script(prefix: &str, retention: u32) -> String {
    assert!(snapshot_name_ok(prefix));
    format!(
        "set -eu; sudo -n {K3S_BIN} etcd-snapshot prune --name {prefix} --etcd-snapshot-retention {retention}"
    )
}

/// Streams one snapshot file (with the token it needs) as a gzip tar, the layout `backup` makes.
pub fn snapshot_pull_script(name: &str) -> String {
    assert!(snapshot_name_ok(name));
    format!(
        "set -eu; sudo -n test -f {SNAPSHOT_DIR}/{name}; sudo -n tar -C {SERVER_DIR} -czf - token db/snapshots/{name}"
    )
}

/// Restores a snapshot that is on the server's disk (cluster-reset). The service is stopped first and started again.
pub fn snapshot_restore_script(name: &str) -> String {
    assert!(snapshot_name_ok(name));
    format!(
        "set -eu; sudo -n test -f {SNAPSHOT_DIR}/{name}; sudo -n systemctl stop k3s.service; \
         sudo -n {K3S_BIN} server --cluster-reset --cluster-reset-restore-path={SNAPSHOT_DIR}/{name} >&2; \
         sudo -n systemctl start k3s.service"
    )
}

pub fn private_ip_script() -> String {
    "ip -4 -o addr show | grep -o '10\\.89\\.[0-9]*\\.[0-9]*' | head -1".into()
}

pub fn private_ip_ok(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|a| a.octets()[..2] == [10, 89])
}

pub fn nodes_json_script() -> String {
    format!("set -eu; sudo -n {K3S_BIN} kubectl get nodes -o json")
}

pub fn readyz_script() -> String {
    format!("set -eu; sudo -n {K3S_BIN} kubectl get --raw /readyz")
}

pub fn is_active_script(role: Role) -> String {
    format!("systemctl is-active {}.service", role.unit())
}

pub fn version_script(path: &str) -> String {
    format!("set -eu; {path} --version | head -1")
}

/// `k3s version v1.31.4+k3s1 (hash)` -> `v1.31.4+k3s1`.
pub fn parse_version_line(s: &str) -> Option<String> {
    s.split_whitespace()
        .find(|w| k3s_version_ok(w))
        .map(str::to_owned)
}

pub fn kubeconfig_script() -> String {
    "set -eu; sudo -n cat /etc/rancher/k3s/k3s.yaml".into()
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct NodeInfo {
    pub name: String,
    pub ready: bool,
    pub version: String,
}

/// Parses `kubectl get nodes -o json`.
pub fn parse_nodes(json_text: &str) -> anyhow::Result<Vec<NodeInfo>> {
    let v: Value = serde_json::from_str(json_text)?;
    let items = v["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no items in the node list"))?;
    Ok(items
        .iter()
        .map(|n| NodeInfo {
            name: n["metadata"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            ready: n["status"]["conditions"].as_array().is_some_and(|c| {
                c.iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
            }),
            version: n["status"]["nodeInfo"]["kubeletVersion"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect())
}

pub fn delete_node_script(node: &str) -> String {
    assert!(name_ok(node));
    format!("set -eu; sudo -n {K3S_BIN} kubectl delete node {node} --wait=false")
}

pub fn drain_script(node: &str) -> String {
    assert!(name_ok(node));
    format!(
        "sudo -n {K3S_BIN} kubectl drain {node} --ignore-daemonsets --delete-emptydir-data --timeout=120s"
    )
}

pub fn uncordon_script(node: &str) -> String {
    assert!(name_ok(node));
    format!("sudo -n {K3S_BIN} kubectl uncordon {node}")
}

// ---- upgrade ----------------------------------------------------------------------------------------------------

pub const NEW_BIN: &str = "/usr/local/bin/k3s.new";
pub const PREV_BIN: &str = "/usr/local/bin/k3s.prev";

/// Swaps the staged binary in and restarts the service, remembering the old one for a rollback.
pub fn swap_script(role: Role) -> String {
    format!(
        "set -eu; sudo -n cp -p {K3S_BIN} {PREV_BIN}; sudo -n mv -f {NEW_BIN} {K3S_BIN}; sudo -n systemctl restart {}.service",
        role.unit()
    )
}

pub fn rollback_script(role: Role) -> String {
    format!(
        "set -eu; sudo -n test -f {PREV_BIN}; sudo -n mv -f {PREV_BIN} {K3S_BIN}; sudo -n systemctl restart {}.service",
        role.unit()
    )
}

pub fn cleanup_staged_script() -> String {
    format!("sudo -n rm -f {NEW_BIN}")
}

// ---- backup and restore -----------------------------------------------------------------------------------------

/// Writes a gzip tar of the datastore to stdout. SQLite is copied with the service stopped (the file is only consistent
/// then) and the service is started again whatever happens; embedded etcd uses k3s's own snapshot and needs no downtime.
pub fn backup_script(ds: Datastore, stamp: &str) -> String {
    assert!(
        stamp
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    );
    match ds {
        Datastore::Sqlite => format!(
            "set -eu; trap 'sudo -n systemctl start k3s.service >&2' EXIT; sudo -n systemctl stop k3s.service >&2; sudo -n sync; \
             sudo -n tar -C {SERVER_DIR} -czf - db token"
        ),
        Datastore::Etcd => format!(
            "set -eu; n=fluxvm-{stamp}; sudo -n {K3S_BIN} etcd-snapshot save --name \"$n\" >&2; \
             f=$(sudo -n sh -c 'ls -1t {SERVER_DIR}/db/snapshots/'\"$n\"'* | head -1'); \
             test -n \"$f\"; sudo -n tar -C {SERVER_DIR} -czf - token \"${{f#{SERVER_DIR}/}}\""
        ),
    }
}

/// Lists a backup archive read from stdin, failing unless it holds what a restore of this datastore needs.
pub fn verify_archive_script(ds: Datastore) -> String {
    let need = match ds {
        Datastore::Sqlite => "db/state.db",
        Datastore::Etcd => "db/snapshots/",
    };
    format!(
        "set -eu; l=$(tar -tzf -); echo \"$l\" | grep -q '^token$' || {{ echo 'archive has no token file' >&2; exit 31; }}; \
         echo \"$l\" | grep -q '{need}' || {{ echo 'archive has no {need}' >&2; exit 31; }}"
    )
}

/// Replaces the datastore from a gzip tar on stdin. The old SQLite directory is kept next to it.
pub fn restore_script(ds: Datastore, stamp: &str) -> String {
    assert!(
        stamp
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    );
    match ds {
        Datastore::Sqlite => format!(
            "set -eu; sudo -n systemctl stop k3s.service; \
             if test -d {SERVER_DIR}/db; then sudo -n mv {SERVER_DIR}/db {SERVER_DIR}/db.pre-restore-{stamp}; fi; \
             sudo -n tar -C {SERVER_DIR} -xzf -; sudo -n systemctl start k3s.service"
        ),
        Datastore::Etcd => format!(
            "set -eu; sudo -n systemctl stop k3s.service; sudo -n tar -C {SERVER_DIR} -xzf -; \
             f=$(sudo -n sh -c 'ls -1t {SERVER_DIR}/db/snapshots/* | head -1'); test -n \"$f\"; \
             sudo -n {K3S_BIN} server --cluster-reset --cluster-reset-restore-path=\"$f\" >&2; \
             sudo -n systemctl start k3s.service"
        ),
    }
}

// ---- persistence canary -----------------------------------------------------------------------------------------

pub fn boot_id_script() -> String {
    "cat /proc/sys/kernel/random/boot_id".into()
}

pub fn canary_mark_script(nonce: &str) -> String {
    assert!(nonce.bytes().all(|c| c.is_ascii_hexdigit()));
    format!(
        "set -eu; echo {nonce} | sudo -n tee {CANARY_FILE} >/dev/null; \
         sudo -n {K3S_BIN} kubectl -n kube-system create configmap {CANARY_CM} --from-literal=nonce={nonce} --dry-run=client -o yaml \
         | sudo -n {K3S_BIN} kubectl apply -f - >&2; sync"
    )
}

pub fn canary_read_script() -> String {
    format!(
        "set -eu; echo disk=$(sudo -n cat {CANARY_FILE} 2>/dev/null || true); \
         echo cm=$(sudo -n {K3S_BIN} kubectl -n kube-system get configmap {CANARY_CM} -o jsonpath='{{.data.nonce}}' 2>/dev/null || true)"
    )
}

/// `disk=…` / `cm=…` lines of [`canary_read_script`].
pub fn parse_canary(out: &str) -> (String, String) {
    let get = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    (get("disk="), get("cm="))
}

// ---- ssh --------------------------------------------------------------------------------------------------------

/// The ssh arguments for one node: the host key is checked strictly against the Mac's pinned file, under the node's alias,
/// so a different guest on the same address is refused. Extra arguments (a command, forwards) follow `user@ip`.
pub fn ssh_args(
    known_hosts: &Path,
    node: &str,
    user: &str,
    endpoint: &str,
    identity: Option<&Path>,
) -> Vec<String> {
    let (host, port) = crate::run::split_endpoint(endpoint);
    let mut a: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".into(),
        format!("HostKeyAlias={}", host_alias(node)),
        "-o".into(),
        "HostKeyAlgorithms=ssh-ed25519".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
    ];
    if let Some(k) = identity {
        a.push("-i".into());
        a.push(k.display().to_string());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    if let Some(p) = port {
        a.push("-p".into());
        a.push(p.to_string());
    }
    a.push(format!("{user}@{host}"));
    a
}

/// The arguments that open the API tunnel: `127.0.0.1:PORT` on the Mac to the server's own loopback API port.
pub fn tunnel_args(base: Vec<String>, port: u16) -> Vec<String> {
    let mut a = vec![
        "-N".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-L".into(),
        format!("127.0.0.1:{port}:127.0.0.1:{GUEST_API_PORT}"),
    ];
    a.extend(base);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 30,
            image: "debian-13".into(),
        }
    }

    fn key() -> HostKey {
        HostKey {
            private: "PRIVATE-KEY".into(),
            public: "ssh-ed25519 AAAA".into(),
        }
    }

    #[test]
    fn spec_attaches_the_private_network_and_pins_the_host_key() {
        let s = node_spec(&params(), "dev", "dev", "mini", "ssh-ed25519 USER", &key());
        assert_eq!(s["name"], "k3s-dev");
        assert_eq!(s["apple"]["networks"][0]["name"], "dev");
        let files = s["cloud_init"]["write_files"].as_array().unwrap();
        let k = files.iter().find(|f| f["path"] == HOSTKEY_GUEST).unwrap();
        assert_eq!(k["permissions"], "0600");
        assert!(files.iter().any(|f| {
            f["content"]
                .as_str()
                .unwrap()
                .contains("HostKey /etc/fluxvm")
        }));
        assert_eq!(
            s["cloud_init"]["ssh_authorized_keys"][0],
            "ssh-ed25519 USER"
        );
    }

    #[test]
    fn stage_script_checks_the_digest_in_the_guest() {
        let sc = stage_binary_script(&"ab".repeat(32), K3S_BIN, false);
        assert!(sc.contains("sha256sum -c"));
        assert!(sc.contains(&"ab".repeat(32)));
        assert!(sc.contains("exit 22"));
        assert!(stage_binary_script(&"ab".repeat(32), NEW_BIN, true).contains("install -m 0755"));
    }

    #[test]
    #[should_panic]
    fn stage_script_refuses_an_unchecked_digest() {
        stage_binary_script("x; rm -rf /", K3S_BIN, false);
    }

    #[test]
    fn server_unit_disables_traefik_and_reads_the_token_from_a_file() {
        let sans = vec!["cluster.example".to_owned()];
        let o = UnitOpts {
            role: Role::Server,
            node: "dev",
            tls_sans: &sans,
            cluster_init: true,
            join_url: None,
            token_file: false,
            agent_token_file: true,
            flags: None,
        };
        let sc = install_unit_script(&o);
        assert!(sc.contains("--disable traefik"));
        assert!(sc.contains("--tls-san 127.0.0.1 --tls-san cluster.example --cluster-init"));
        assert!(sc.contains("--node-ip $PRIV --flannel-iface $IFACE"));
        assert!(sc.contains("Restart=always"));
        assert!(
            !sc.contains("--token "),
            "a token argument would show in ps"
        );
        let join = UnitOpts {
            role: Role::Agent,
            node: "dev-a1",
            tls_sans: &[],
            cluster_init: false,
            join_url: Some("https://10.89.1.2:6443"),
            token_file: true,
            agent_token_file: false,
            flags: None,
        };
        let ex = exec_args(&join);
        assert!(ex.starts_with(
            "agent --server https://10.89.1.2:6443 --token-file /etc/rancher/k3s/token"
        ));
        assert!(install_unit_script(&join).contains("k3s-agent.service"));
    }

    #[test]
    fn token_scripts_never_carry_a_token() {
        for s in [
            write_token_script(),
            read_token_script(Role::Agent),
            read_token_script(Role::Server),
        ] {
            assert!(!s.contains("K10"));
        }
        assert!(write_token_script().contains("umask 077"));
        assert!(read_token_script(Role::Agent).ends_with("agent-token"));
        assert!(write_agent_token_script().contains("umask 077"));
        assert!(snapshot_restore_script("a-1").contains("--cluster-reset-restore-path"));
        assert!(!snapshot_name_ok("a;b") && !snapshot_name_ok("-x") && !snapshot_name_ok("../x"));
    }

    #[test]
    fn nodes_json_is_parsed() {
        let j = r#"{"items":[{"metadata":{"name":"dev"},"status":{"conditions":[{"type":"Ready","status":"True"}],"nodeInfo":{"kubeletVersion":"v1.31.4+k3s1"}}},
        {"metadata":{"name":"dev-a1"},"status":{"conditions":[{"type":"Ready","status":"False"}],"nodeInfo":{"kubeletVersion":"v1.31.4+k3s1"}}}]}"#;
        let n = parse_nodes(j).unwrap();
        assert_eq!(n.len(), 2);
        assert!(n[0].ready && !n[1].ready);
        assert_eq!(n[1].version, "v1.31.4+k3s1");
        assert!(parse_nodes("{}").is_err());
        assert!(parse_nodes("nope").is_err());
    }

    #[test]
    fn version_line_is_found() {
        assert_eq!(
            parse_version_line("k3s version v1.31.4+k3s1 (abcdef12)").as_deref(),
            Some("v1.31.4+k3s1")
        );
        assert_eq!(parse_version_line("garbage"), None);
    }

    #[test]
    fn sqlite_backup_stops_and_always_restarts_the_service() {
        let s = backup_script(Datastore::Sqlite, "20261121-101500");
        assert!(s.contains("trap 'sudo -n systemctl start k3s.service"));
        assert!(s.find("stop k3s").unwrap() < s.find("tar -C").unwrap());
        let e = backup_script(Datastore::Etcd, "20261121-101500");
        assert!(e.contains("etcd-snapshot save"));
        assert!(!e.contains("systemctl stop"));
    }

    #[test]
    fn restore_keeps_the_old_sqlite_directory() {
        let s = restore_script(Datastore::Sqlite, "20261121-101500");
        assert!(s.contains("db.pre-restore-20261121-101500"));
        assert!(restore_script(Datastore::Etcd, "x1").contains("--cluster-reset-restore-path"));
        assert!(verify_archive_script(Datastore::Sqlite).contains("db/state.db"));
    }

    #[test]
    fn ssh_is_strict_about_the_pinned_key() {
        let a = ssh_args(
            Path::new("/s/dev.known_hosts"),
            "dev",
            "mini",
            "192.168.64.5",
            None,
        );
        assert!(a.contains(&"StrictHostKeyChecking=yes".to_owned()));
        assert!(a.contains(&"UserKnownHostsFile=/s/dev.known_hosts".to_owned()));
        assert!(a.contains(&"HostKeyAlias=fluxvm-k3s-dev".to_owned()));
        assert_eq!(a.last().unwrap(), "mini@192.168.64.5");
        assert!(!a.iter().any(|x| x.contains("StrictHostKeyChecking=no")));
        let b = ssh_args(
            Path::new("/k"),
            "dev",
            "u",
            "127.0.0.1:2222",
            Some(Path::new("/id")),
        );
        assert!(b.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(b.contains(&"IdentitiesOnly=yes".to_owned()));
    }

    #[test]
    fn tunnel_binds_loopback_only() {
        let t = tunnel_args(vec!["u@h".into()], 16443);
        assert!(t.contains(&"127.0.0.1:16443:127.0.0.1:6443".to_owned()));
        assert!(t.contains(&"ExitOnForwardFailure=yes".to_owned()));
        assert_eq!(t.last().unwrap(), "u@h");
    }

    #[test]
    fn canary_lines_are_parsed() {
        assert_eq!(
            parse_canary("disk=abc\ncm=def\n"),
            ("abc".into(), "def".into())
        );
        assert_eq!(parse_canary("disk=\ncm=\n"), (String::new(), String::new()));
    }

    #[test]
    fn private_ips_are_only_the_switch_range() {
        assert!(private_ip_ok("10.89.3.4"));
        assert!(!private_ip_ok("192.168.64.2"));
        assert!(!private_ip_ok("10.89.3.4; id"));
    }

    #[test]
    fn swap_and_rollback_use_the_right_unit() {
        assert!(swap_script(Role::Agent).contains("k3s-agent.service"));
        assert!(rollback_script(Role::Server).contains("k3s.prev"));
    }
}
