// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Shell scripts and parsers for the HA, rotation, snapshot-schedule, node-removal and rebuild work. Like `script.rs`, these
//! are pure functions of validated input and no script here contains a secret: tokens and S3 keys only travel on stdin.
//! Each script starts with a no-op `: fluxvm-op NAME;` so tests (and a reader of `ps` in a guest) can tell which step it is.

use super::model::*;
use super::script::{K3S_BIN, SERVER_DIR, TOKEN_PATH};

pub const DROPIN_DIR: &str = "/etc/rancher/k3s/config.yaml.d";
pub const SNAPSHOT_DROPIN: &str = "/etc/rancher/k3s/config.yaml.d/50-fluxvm-snapshots.yaml";
pub const TOKEN_DROPIN: &str = "/etc/rancher/k3s/config.yaml.d/60-fluxvm-token.yaml";
pub const KUBE_VIP_MANIFEST: &str = "/var/lib/rancher/k3s/agent/pod-manifests/kube-vip.yaml";
/// A pinned kube-vip release. Not exercised on hardware.
pub const KUBE_VIP_IMAGE: &str = "ghcr.io/kube-vip/kube-vip:v0.8.7";

fn op(name: &str, body: &str) -> String {
    format!(": fluxvm-op {name}; {body}")
}

/// Receives a k3s configuration drop-in on stdin into a root-only file (it may hold S3 keys).
pub fn write_dropin_script(path: &str) -> String {
    assert!(path.starts_with(DROPIN_DIR));
    op(
        "write-dropin",
        &format!(
            "set -eu; sudo -n install -d -m 0700 {DROPIN_DIR}; sudo -n sh -c 'umask 077; cat > {path}'"
        ),
    )
}

pub fn remove_dropin_script(path: &str) -> String {
    assert!(path.starts_with(DROPIN_DIR));
    op("remove-dropin", &format!("set -eu; sudo -n rm -f {path}"))
}

pub fn restart_script(role: Role) -> String {
    op(
        "restart",
        &format!("set -eu; sudo -n systemctl restart {}.service", role.unit()),
    )
}

/// The non-secret snapshot keys of the drop-in (the S3 keys are filtered out), for `doctor` and `rebuild`.
pub fn snapshot_config_probe_script() -> String {
    op(
        "snapshot-config",
        &format!(
            "sudo -n grep -E '^(etcd-snapshot-schedule-cron|etcd-snapshot-retention|etcd-snapshot-compress|etcd-disable-snapshots|etcd-s3|etcd-s3-endpoint|etcd-s3-bucket|etcd-s3-region|etcd-s3-folder):' {SNAPSHOT_DROPIN} || true"
        ),
    )
}

/// Seconds since the newest snapshot file on this server was written.
pub fn snapshot_age_script() -> String {
    op(
        "snapshot-age",
        &format!(
            "echo now=$(date +%s); echo newest=$(sudo -n find {SERVER_DIR}/db/snapshots -type f -printf '%T@\\n' 2>/dev/null | sort -n | tail -1 | cut -d. -f1)"
        ),
    )
}

/// Seconds since the newest snapshot, `None` when there is none.
pub fn parse_snapshot_age(out: &str) -> Option<u64> {
    let get = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    Some(get("now=")?.saturating_sub(get("newest=")?))
}

/// The unit as systemd sees it (the `ExecStart` line is what the Mac compares across servers).
pub fn unit_probe_script(role: Role) -> String {
    op(
        "unit-probe",
        &format!("systemctl cat {}.service", role.unit()),
    )
}

/// What the Mac can read back from a server's unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitFlags {
    pub cluster_init: bool,
    pub tls_sans: Vec<String>,
    pub join_url: Option<String>,
    pub shared: SharedFlags,
}

/// Parses the `ExecStart=` line of `systemctl cat`. Unknown flags are ignored.
pub fn parse_unit_flags(text: &str) -> Option<UnitFlags> {
    let line = text
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("ExecStart="))?;
    let mut it = line.split_whitespace().peekable();
    let mut u = UnitFlags::default();
    while let Some(w) = it.next() {
        let mut val = || it.next().map(str::to_owned);
        match w {
            "--cluster-init" => u.cluster_init = true,
            "--secrets-encryption" => u.shared.secrets_encryption = true,
            "--tls-san" => {
                if let Some(v) = val()
                    && v != "127.0.0.1"
                {
                    u.tls_sans.push(v);
                }
            }
            "--server" => u.join_url = val(),
            "--disable" => {
                if let Some(v) = val()
                    && v != "traefik"
                {
                    u.shared.disable.push(v);
                }
            }
            "--cluster-cidr" => u.shared.cluster_cidr = val(),
            "--service-cidr" => u.shared.service_cidr = val(),
            "--cluster-dns" => u.shared.cluster_dns = val(),
            "--cluster-domain" => u.shared.cluster_domain = val(),
            _ => {}
        }
    }
    u.shared.disable.sort();
    u.shared.disable.dedup();
    Some(u)
}

/// Where a server's shared flags differ from what the journal says every server must have. Empty when they agree.
pub fn flags_drift(expected: &SharedFlags, node: &str, got: &SharedFlags) -> Vec<String> {
    let mut want = expected.clone();
    want.disable.retain(|d| d != "traefik");
    want.disable.sort();
    want.disable.dedup();
    let mut d = vec![];
    let mut cmp = |name: &str, a: String, b: String| {
        if a != b {
            d.push(format!("{node}: {name} is {b:?}, the cluster has {a:?}"));
        }
    };
    let o = |v: &Option<String>| v.clone().unwrap_or_default();
    cmp("--cluster-cidr", o(&want.cluster_cidr), o(&got.cluster_cidr));
    cmp("--service-cidr", o(&want.service_cidr), o(&got.service_cidr));
    cmp("--cluster-dns", o(&want.cluster_dns), o(&got.cluster_dns));
    cmp(
        "--cluster-domain",
        o(&want.cluster_domain),
        o(&got.cluster_domain),
    );
    cmp("--disable", want.disable.join(","), got.disable.join(","));
    cmp(
        "--secrets-encryption",
        want.secrets_encryption.to_string(),
        got.secrets_encryption.to_string(),
    );
    d
}

// ---- token rotation ---------------------------------------------------------------------------------------------

/// Rotates the server token on this server with `k3s token rotate`. Stdin: the old token, a newline, the new token, a
/// newline. They reach k3s in its environment, never in an argument. Whether this k3s build reads `K3S_NEW_TOKEN` was not
/// checked on a running cluster; if it does not, k3s refuses and nothing changes.
pub fn rotate_server_secret_script() -> String {
    op(
        "rotate-server-secret",
        &format!(
            "set -eu; IFS= read -r OLD; IFS= read -r NEW; export K3S_TOKEN=\"$OLD\" K3S_NEW_TOKEN=\"$NEW\"; \
             sudo -n --preserve-env=K3S_TOKEN,K3S_NEW_TOKEN {K3S_BIN} token rotate >&2"
        ),
    )
}

/// Receives the new server token on stdin into [`TOKEN_PATH`] and makes k3s read it from there (a config drop-in), so a
/// restarted server uses the rotated value whichever way it first got its token.
pub fn write_server_secret_script() -> String {
    op(
        "write-server-secret",
        &format!(
            "set -eu; sudo -n install -d -m 0700 /etc/rancher/k3s {DROPIN_DIR}; \
             sudo -n sh -c 'umask 077; cat > {TOKEN_PATH}'; \
             sudo -n sh -c 'umask 077; echo \"token-file: {TOKEN_PATH}\" > {TOKEN_DROPIN}'"
        ),
    )
}

/// The token k3s itself keeps (`server/token`), to see whether a rotation reached it.
pub fn server_dir_secret_script() -> String {
    op(
        "read-server-dir-secret",
        &format!("set -eu; sudo -n cat {SERVER_DIR}/token"),
    )
}

// ---- node removal -----------------------------------------------------------------------------------------------

pub fn cordon_script(node: &str) -> String {
    assert!(name_ok(node));
    op("cordon", &format!("sudo -n {K3S_BIN} kubectl cordon {node}"))
}

/// Server nodes that hold an etcd member, as `node/NAME` lines.
pub fn etcd_members_script() -> String {
    op(
        "etcd-members",
        &format!(
            "set -eu; sudo -n {K3S_BIN} kubectl get nodes -l node-role.kubernetes.io/etcd=true -o name"
        ),
    )
}

pub fn parse_etcd_members(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("node/"))
        .map(str::to_owned)
        .collect()
}

/// What the servers logged recently about etcd members being removed.
pub fn etcd_member_log_script() -> String {
    op(
        "etcd-member-log",
        "sudo -n journalctl -u k3s.service --since '15 min ago' --no-pager 2>/dev/null | grep -i -E 'remov.*(member|etcd)|member.*remov' | tail -3 || true",
    )
}

/// Stops k3s on a node and keeps it from starting again (so it cannot re-register after its node object is deleted).
pub fn disable_unit_script(role: Role) -> String {
    op(
        "disable-unit",
        &format!("sync; sudo -n systemctl disable --now {}.service", role.unit()),
    )
}

/// How many members must be healthy for etcd to commit changes.
pub fn majority(members: usize) -> usize {
    members / 2 + 1
}

// ---- kube-vip ---------------------------------------------------------------------------------------------------

/// The static-pod manifest for kube-vip in control-plane mode (ARP, leader election). `__IFACE__` is replaced inside the
/// guest by the private network card. Not run on hardware; it needs the image to be pullable by the servers.
pub fn kube_vip_manifest(vip: &str) -> String {
    assert!(private_vip_ok(vip));
    const T: &str = r#"apiVersion: v1
kind: Pod
metadata:
  name: kube-vip
  namespace: kube-system
spec:
  hostNetwork: true
  containers:
  - name: kube-vip
    image: __IMAGE__
    args: ["manager"]
    env:
    - {name: vip_arp, value: "true"}
    - {name: port, value: "6443"}
    - {name: vip_interface, value: "__IFACE__"}
    - {name: vip_cidr, value: "32"}
    - {name: cp_enable, value: "true"}
    - {name: cp_namespace, value: kube-system}
    - {name: svc_enable, value: "false"}
    - {name: vip_leaderelection, value: "true"}
    - {name: address, value: "__VIP__"}
    securityContext:
      capabilities:
        add: ["NET_ADMIN", "NET_RAW"]
    volumeMounts:
    - {mountPath: /etc/kubernetes/admin.conf, name: kubeconfig}
  hostAliases:
  - hostnames: [kubernetes]
    ip: 127.0.0.1
  volumes:
  - name: kubeconfig
    hostPath:
      path: /etc/rancher/k3s/k3s.yaml
"#;
    T.replace("__IMAGE__", KUBE_VIP_IMAGE).replace("__VIP__", vip)
}

/// A kube-vip address: on the cluster's private switch (10.89.x.y), like the nodes.
pub fn private_vip_ok(s: &str) -> bool {
    super::script::private_ip_ok(s)
}

/// Receives the manifest on stdin and writes it with the private card substituted.
pub fn install_kube_vip_script() -> String {
    op(
        "kube-vip",
        &format!(
            "set -eu; PRIV=$(ip -4 -o addr show | grep -o '10\\.89\\.[0-9]*\\.[0-9]*' | head -1); \
             IFACE=$(ip -o -4 addr show | grep -F \"$PRIV/\" | cut -d' ' -f2); [ -n \"$IFACE\" ] || {{ echo 'no private network card' >&2; exit 25; }}; \
             sudo -n install -d -m 0755 /var/lib/rancher/k3s/agent/pod-manifests; \
             sed \"s/__IFACE__/$IFACE/\" | sudo -n tee {KUBE_VIP_MANIFEST} >/dev/null"
        ),
    )
}

/// Prints the kube-vip address from the manifest, or `absent`.
pub fn kube_vip_probe_script() -> String {
    op(
        "kube-vip-probe",
        &format!(
            "if test -f {KUBE_VIP_MANIFEST}; then grep -o 'name: address, value: \"[0-9.]*\"' {KUBE_VIP_MANIFEST} || echo present; else echo absent; fi"
        ),
    )
}

/// `Some(None)`: kube-vip is installed but its address could not be read; `Some(Some(ip))`: installed with that address.
pub fn parse_kube_vip_probe(out: &str) -> Option<Option<String>> {
    let t = out.trim();
    if t.is_empty() || t == "absent" {
        return None;
    }
    let vip = t
        .split("value: \"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .filter(|v| private_vip_ok(v))
        .map(str::to_owned);
    Some(vip)
}

/// Rebuilds the policy from the non-secret lines [`snapshot_config_probe_script`] prints. `None` when no drop-in exists.
pub fn parse_snapshot_config(out: &str) -> Option<SnapshotPolicy> {
    let get = |k: &str| {
        out.lines().find_map(|l| {
            l.strip_prefix(k)
                .and_then(|r| r.strip_prefix(':'))
                .map(|v| v.trim().trim_matches('"').to_owned())
        })
    };
    let cron = get("etcd-snapshot-schedule-cron")?;
    let s3 = (get("etcd-s3").as_deref() == Some("true")).then(|| S3Target {
        endpoint: get("etcd-s3-endpoint").unwrap_or_default(),
        bucket: get("etcd-s3-bucket").unwrap_or_default(),
        region: get("etcd-s3-region"),
        folder: get("etcd-s3-folder"),
    });
    Some(SnapshotPolicy {
        cron,
        retention: get("etcd-snapshot-retention")
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
        compress: get("etcd-snapshot-compress").as_deref() == Some("true"),
        s3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIT: &str = "# /etc/systemd/system/k3s.service\n[Service]\nExecStart=/usr/local/bin/k3s server --disable traefik --disable coredns \
        --cluster-cidr 10.50.0.0/16 --write-kubeconfig-mode 600 --tls-san 127.0.0.1 --tls-san k.example --cluster-init \
        --secrets-encryption --agent-token-file /etc/rancher/k3s/agent-token --node-name dev --node-ip $PRIV\n";

    #[test]
    fn unit_flags_are_read_back() {
        let u = parse_unit_flags(UNIT).unwrap();
        assert!(u.cluster_init);
        assert_eq!(u.tls_sans, vec!["k.example"]);
        assert_eq!(u.shared.disable, vec!["coredns"]);
        assert_eq!(u.shared.cluster_cidr.as_deref(), Some("10.50.0.0/16"));
        assert!(u.shared.secrets_encryption);
        assert!(parse_unit_flags("[Service]\n").is_none());
    }

    #[test]
    fn drift_names_each_difference() {
        let want = SharedFlags {
            cluster_cidr: Some("10.50.0.0/16".into()),
            disable: vec!["coredns".into()],
            secrets_encryption: true,
            ..Default::default()
        };
        let got = parse_unit_flags(UNIT).unwrap().shared;
        assert!(flags_drift(&want, "dev", &got).is_empty());
        let mut other = got.clone();
        other.cluster_cidr = Some("10.60.0.0/16".into());
        other.disable.clear();
        other.secrets_encryption = false;
        let d = flags_drift(&want, "dev-s2", &other);
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d.iter().all(|l| l.starts_with("dev-s2:")));
    }

    #[test]
    fn rotation_scripts_carry_no_token_and_use_stdin_and_environment() {
        let r = rotate_server_secret_script();
        assert!(r.contains("read -r OLD") && r.contains("K3S_NEW_TOKEN"));
        assert!(!r.contains("--new-token") && !r.contains("--token "));
        let w = write_server_secret_script();
        assert!(w.contains("umask 077") && w.contains("cat > /etc/rancher/k3s/token"));
        assert!(!w.contains("K10"));
    }

    #[test]
    fn dropin_scripts_stay_inside_the_dropin_folder() {
        assert!(write_dropin_script(SNAPSHOT_DROPIN).contains("umask 077"));
        assert!(remove_dropin_script(TOKEN_DROPIN).contains("rm -f"));
        assert!(std::panic::catch_unwind(|| write_dropin_script("/etc/passwd")).is_err());
        // The probe must never print the S3 keys.
        let p = snapshot_config_probe_script();
        assert!(!p.contains("access-key") && !p.contains("secret-key"));
    }

    #[test]
    fn snapshot_age_and_members_are_parsed() {
        assert_eq!(parse_snapshot_age("now=1000\nnewest=400\n"), Some(600));
        assert_eq!(parse_snapshot_age("now=1000\nnewest=\n"), None);
        assert_eq!(
            parse_etcd_members("node/dev\nnode/dev-s2\n\nsomething\n"),
            vec!["dev", "dev-s2"]
        );
        assert_eq!((majority(1), majority(2), majority(3), majority(5)), (1, 2, 2, 3));
    }

    #[test]
    fn probes_are_parsed_back_into_the_journal_types() {
        let p = SnapshotPolicy {
            cron: "0 */6 * * *".into(),
            retention: 7,
            compress: true,
            s3: Some(S3Target {
                endpoint: "s3.example:9000".into(),
                bucket: "snaps".into(),
                region: None,
                folder: Some("dev".into()),
            }),
        };
        let sec = S3Secret::new("AK", "SK").unwrap();
        let text = p.dropin(Some(&sec)).unwrap();
        // What the probe prints: the drop-in without its key lines.
        let shown: String = text.lines().filter(|l| !l.contains("-key")).map(|l| format!("{l}\n")).collect();
        assert_eq!(parse_snapshot_config(&shown), Some(p));
        assert_eq!(parse_snapshot_config(""), None);
        assert_eq!(parse_kube_vip_probe("absent\n"), None);
        assert_eq!(parse_kube_vip_probe("present\n"), Some(None));
        assert_eq!(
            parse_kube_vip_probe("name: address, value: \"10.89.0.250\"\n"),
            Some(Some("10.89.0.250".into()))
        );
    }

    #[test]
    fn kube_vip_manifest_has_the_address_and_a_placeholder_for_the_card() {
        let m = kube_vip_manifest("10.89.0.250");
        assert!(m.contains("10.89.0.250") && m.contains("__IFACE__") && m.contains("cp_enable"));
        serde_yaml::from_str::<serde_yaml::Value>(&m).expect("valid yaml");
        assert!(std::panic::catch_unwind(|| kube_vip_manifest("192.168.1.1; id")).is_err());
        assert!(install_kube_vip_script().contains("sed \"s/__IFACE__/$IFACE/\""));
    }
}
