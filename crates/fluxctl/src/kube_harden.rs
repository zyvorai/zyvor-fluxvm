// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Hardening profiles for k3s, rendered as `config.yaml` keys plus the files they point at: Secrets encryption at
//! rest, and the CIS profile from the k3s hardening guide (Pod Security Admission defaults, an audit policy, kernel
//! sysctls). Pure generators; [`apply_files`] pushes them through a [`GuestOps`].
//!
//! Generating the profile is not the same as passing a CIS scan: nothing here has been audited with kube-bench.

use crate::kube_addons::{GuestOps, K3sConfig};
use anyhow::Result;

const SERVER_DIR: &str = "/var/lib/rancher/k3s/server";

/// `secrets-encryption: true`. k3s creates the encryption config itself; rotate keys with `k3s secrets-encrypt rotate-keys`.
pub fn secrets_encryption() -> K3sConfig {
    let mut c = K3sConfig::default();
    c.set_bool("secrets-encryption", true)
        .expect("fresh config");
    c
}

/// The CIS profile's config keys (k3s hardening guide for v1.25+). Needs [`cis_files`] in place first, and the sysctls
/// applied, or k3s refuses to start (`protect-kernel-defaults`).
pub fn cis_config() -> K3sConfig {
    let mut c = secrets_encryption();
    c.set_bool("protect-kernel-defaults", true)
        .expect("fresh config");
    for a in [
        format!("admission-control-config-file={SERVER_DIR}/psa.yaml"),
        format!("audit-log-path={SERVER_DIR}/logs/audit.log"),
        format!("audit-policy-file={SERVER_DIR}/audit.yaml"),
        "audit-log-maxage=30".into(),
        "audit-log-maxbackup=10".into(),
        "audit-log-maxsize=100".into(),
        "request-timeout=300s".into(),
        "service-account-lookup=true".into(),
    ] {
        c.push("kube-apiserver-arg", &a);
    }
    for a in [
        "terminated-pod-gc-threshold=10",
        "use-service-account-credentials=true",
    ] {
        c.push("kube-controller-manager-arg", a);
    }
    for a in [
        "streaming-connection-idle-timeout=5m",
        "make-iptables-util-chains=true",
    ] {
        c.push("kubelet-arg", a);
    }
    c
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFile {
    pub path: String,
    pub mode: &'static str,
    pub content: String,
}

pub fn sysctl_conf() -> String {
    "vm.panic_on_oom=0\nvm.overcommit_memory=1\nkernel.panic=10\nkernel.panic_on_oops=1\n".into()
}

/// A Pod Security Admission configuration enforcing `level` (`privileged`, `baseline` or `restricted`) cluster-wide,
/// with the system namespaces exempt.
pub fn psa_config(level: &str) -> Result<String> {
    if !["privileged", "baseline", "restricted"].contains(&level) {
        anyhow::bail!("PSA level {level:?} must be privileged, baseline or restricted");
    }
    Ok(format!(
        "apiVersion: apiserver.config.k8s.io/v1\n\
         kind: AdmissionConfiguration\n\
         plugins:\n\
         - name: PodSecurity\n  \
         configuration:\n    \
         apiVersion: pod-security.admission.config.k8s.io/v1\n    \
         kind: PodSecurityConfiguration\n    \
         defaults:\n      \
         enforce: \"{level}\"\n      \
         enforce-version: \"latest\"\n      \
         audit: \"{level}\"\n      \
         audit-version: \"latest\"\n      \
         warn: \"{level}\"\n      \
         warn-version: \"latest\"\n    \
         exemptions:\n      \
         usernames: []\n      \
         runtimeClasses: []\n      \
         namespaces: [kube-system, cis-operator-system]\n"
    ))
}

/// Log request metadata only: no bodies, so Secrets never reach the audit log.
pub fn audit_policy() -> String {
    "apiVersion: audit.k8s.io/v1\nkind: Policy\nrules:\n- level: Metadata\n".into()
}

pub fn cis_files() -> Vec<GuestFile> {
    vec![
        GuestFile {
            path: "/etc/sysctl.d/90-kubelet.conf".into(),
            mode: "0644",
            content: sysctl_conf(),
        },
        GuestFile {
            path: format!("{SERVER_DIR}/psa.yaml"),
            mode: "0600",
            content: psa_config("restricted").expect("known level"),
        },
        GuestFile {
            path: format!("{SERVER_DIR}/audit.yaml"),
            mode: "0600",
            content: audit_policy(),
        },
    ]
}

/// Writes the files, creates the audit log directory and loads the sysctls. Does not touch config.yaml or restart k3s,
/// so the order stays the caller's: files first, then config, then restart.
pub fn apply_files(g: &dyn GuestOps, files: &[GuestFile]) -> Result<Vec<String>> {
    let mut done = vec![];
    for f in files {
        g.write(&f.path, &f.content, f.mode)?;
        done.push(format!("wrote {}", f.path));
    }
    g.run(&format!("sudo mkdir -p {SERVER_DIR}/logs"))?;
    g.run("sudo sysctl -p /etc/sysctl.d/90-kubelet.conf")?;
    done.push("loaded sysctls".into());
    Ok(done)
}

// ---------------------------------------------------------------- read-only audit

const SYSCTLS: [(&str, &str); 4] = [
    ("vm.panic_on_oom", "0"),
    ("vm.overcommit_memory", "1"),
    ("kernel.panic", "10"),
    ("kernel.panic_on_oops", "1"),
];

/// What `cis_audit` needs from a node: the text of config.yaml, sysctl values by name, and which files exist.
#[derive(Debug, Default, Clone)]
pub struct CisFacts {
    pub config: Option<String>,
    pub sysctls: std::collections::BTreeMap<String, String>,
    pub files: std::collections::BTreeMap<String, bool>,
}

fn arg_present(cfg: &serde_yaml::Value, key: &str, prefix: &str) -> bool {
    cfg.get(key).and_then(|v| v.as_sequence()).is_some_and(|s| {
        s.iter()
            .any(|i| i.as_str().is_some_and(|x| x.starts_with(prefix)))
    })
}

/// Read-only check of the settings [`cis_config`] and [`cis_files`] produce. It reads only `config.yaml`: a setting
/// given on the k3s command line or in a `config.yaml.d` drop-in is not seen and shows as missing. It checks that the
/// settings are present, not that the cluster passes a CIS benchmark.
pub fn cis_audit(f: &CisFacts) -> Vec<crate::kube_preflight::Check> {
    use crate::kube_preflight::{Check, Sev};
    let mk = |name: &'static str, ok: bool, sev_if_not: Sev, good: &str, bad: &str| Check {
        name,
        sev: if ok { Sev::Ok } else { sev_if_not },
        detail: if ok { good } else { bad }.to_string(),
    };
    let Some(text) = &f.config else {
        return vec![Check {
            name: "config.yaml",
            sev: Sev::Fail,
            detail: "could not read /etc/rancher/k3s/config.yaml".into(),
        }];
    };
    let Ok(cfg) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return vec![Check {
            name: "config.yaml",
            sev: Sev::Fail,
            detail: "config.yaml is not valid YAML".into(),
        }];
    };
    let truthy = |k: &str| cfg.get(k).and_then(|v| v.as_bool()) == Some(true);
    let mut out = vec![
        mk(
            "secrets-encryption",
            truthy("secrets-encryption"),
            Sev::Fail,
            "enabled",
            "not set; Secrets are stored unencrypted",
        ),
        mk(
            "protect-kernel-defaults",
            truthy("protect-kernel-defaults"),
            Sev::Fail,
            "enabled",
            "not set",
        ),
        mk(
            "pod security admission",
            arg_present(&cfg, "kube-apiserver-arg", "admission-control-config-file="),
            Sev::Fail,
            "admission config file set",
            "no admission-control-config-file",
        ),
        mk(
            "audit log",
            arg_present(&cfg, "kube-apiserver-arg", "audit-log-path="),
            Sev::Fail,
            "audit-log-path set",
            "no audit-log-path",
        ),
        mk(
            "audit policy",
            arg_present(&cfg, "kube-apiserver-arg", "audit-policy-file="),
            Sev::Fail,
            "audit-policy-file set",
            "no audit-policy-file",
        ),
        mk(
            "kubelet idle timeout",
            arg_present(&cfg, "kubelet-arg", "streaming-connection-idle-timeout="),
            Sev::Warn,
            "set",
            "streaming-connection-idle-timeout not set",
        ),
        mk(
            "service account credentials",
            arg_present(
                &cfg,
                "kube-controller-manager-arg",
                "use-service-account-credentials=",
            ),
            Sev::Warn,
            "set",
            "use-service-account-credentials not set",
        ),
    ];
    for (name, want) in SYSCTLS {
        let got = f.sysctls.get(name).map(|s| s.trim());
        out.push(Check {
            name: "sysctl",
            sev: if got == Some(want) {
                Sev::Ok
            } else {
                Sev::Fail
            },
            detail: format!("{name} = {} (want {want})", got.unwrap_or("unreadable")),
        });
    }
    for path in [
        format!("{SERVER_DIR}/psa.yaml"),
        format!("{SERVER_DIR}/audit.yaml"),
    ] {
        let there = f.files.get(&path).copied().unwrap_or(false);
        out.push(Check {
            name: "file",
            sev: if there { Sev::Ok } else { Sev::Fail },
            detail: format!("{path} {}", if there { "exists" } else { "is missing" }),
        });
    }
    out
}

pub fn cis_gather(g: &dyn GuestOps) -> CisFacts {
    let mut f = CisFacts {
        config: g.run("sudo cat /etc/rancher/k3s/config.yaml").ok(),
        ..Default::default()
    };
    for (name, _) in SYSCTLS {
        if let Ok(v) = g.run(&format!("sysctl -n {name}")) {
            f.sysctls.insert(name.to_string(), v.trim().to_string());
        }
    }
    for path in [
        format!("{SERVER_DIR}/psa.yaml"),
        format!("{SERVER_DIR}/audit.yaml"),
    ] {
        let there = g
            .run(&format!("sudo test -f {path} && echo yes"))
            .is_ok_and(|o| o.trim() == "yes");
        f.files.insert(path, there);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_addons::FakeGuest;

    #[test]
    fn secrets_encryption_golden() {
        assert_eq!(
            secrets_encryption().render(),
            "# generated by `fluxctl kube addons`; edit with care\nsecrets-encryption: true\n"
        );
    }

    #[test]
    fn cis_config_golden() {
        let want = "# generated by `fluxctl kube addons`; edit with care\n\
kube-apiserver-arg:\n\
- admission-control-config-file=/var/lib/rancher/k3s/server/psa.yaml\n\
- audit-log-path=/var/lib/rancher/k3s/server/logs/audit.log\n\
- audit-policy-file=/var/lib/rancher/k3s/server/audit.yaml\n\
- audit-log-maxage=30\n\
- audit-log-maxbackup=10\n\
- audit-log-maxsize=100\n\
- request-timeout=300s\n\
- service-account-lookup=true\n\
kube-controller-manager-arg:\n\
- terminated-pod-gc-threshold=10\n\
- use-service-account-credentials=true\n\
kubelet-arg:\n\
- streaming-connection-idle-timeout=5m\n\
- make-iptables-util-chains=true\n\
protect-kernel-defaults: true\n\
secrets-encryption: true\n";
        assert_eq!(cis_config().render(), want);
    }

    #[test]
    fn psa_config_is_valid_yaml_with_the_level() {
        let y = psa_config("baseline").unwrap();
        let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
        let d = &v["plugins"][0]["configuration"]["defaults"];
        assert_eq!(d["enforce"], "baseline");
        assert_eq!(
            v["plugins"][0]["configuration"]["exemptions"]["namespaces"][0],
            "kube-system"
        );
        assert!(psa_config("strict").is_err());
    }

    #[test]
    fn audit_policy_logs_metadata_only() {
        let v: serde_yaml::Value = serde_yaml::from_str(&audit_policy()).unwrap();
        assert_eq!(v["rules"][0]["level"], "Metadata");
    }

    #[test]
    fn files_are_where_the_config_points() {
        let cfg = cis_config().render();
        for f in cis_files().iter().filter(|f| f.path.contains("/server/")) {
            assert!(cfg.contains(&f.path), "{} missing from config", f.path);
            assert_eq!(f.mode, "0600");
        }
        assert!(sysctl_conf().contains("kernel.panic_on_oops=1"));
    }

    #[test]
    fn apply_files_orders_writes_before_sysctl() {
        let g = FakeGuest::default();
        let out = apply_files(&g, &cis_files()).unwrap();
        assert_eq!(out.len(), 4);
        let calls = g.calls();
        assert!(calls[0].starts_with("write /etc/sysctl.d/90-kubelet.conf 0644"));
        assert!(calls[3].contains("mkdir -p /var/lib/rancher/k3s/server/logs"));
        assert!(calls[4].contains("sysctl -p"));
    }

    #[test]
    fn audit_passes_for_the_profile_we_generate() {
        let mut f = CisFacts {
            config: Some(cis_config().render()),
            ..Default::default()
        };
        for (k, v) in SYSCTLS {
            f.sysctls.insert(k.into(), format!("{v}\n"));
        }
        for file in cis_files().iter().filter(|x| x.path.contains("/server/")) {
            f.files.insert(file.path.clone(), true);
        }
        let r = cis_audit(&f);
        assert!(
            r.iter().all(|c| c.sev == crate::kube_preflight::Sev::Ok),
            "{}",
            crate::kube_preflight::render(&r)
        );
    }

    #[test]
    fn audit_flags_what_is_missing() {
        use crate::kube_preflight::Sev;
        let f = CisFacts {
            config: Some("secrets-encryption: true\n".into()),
            ..Default::default()
        };
        let r = cis_audit(&f);
        let sev = |n: &str| r.iter().find(|c| c.name == n).unwrap().sev;
        assert_eq!(sev("secrets-encryption"), Sev::Ok);
        assert_eq!(sev("protect-kernel-defaults"), Sev::Fail);
        assert_eq!(sev("kubelet idle timeout"), Sev::Warn);
        assert!(
            r.iter()
                .filter(|c| c.name == "sysctl")
                .all(|c| c.sev == Sev::Fail)
        );
        assert!(crate::kube_preflight::failed(&r));
        assert!(crate::kube_preflight::failed(&cis_audit(
            &CisFacts::default()
        )));
        let bad = CisFacts {
            config: Some("a: [".into()),
            ..Default::default()
        };
        assert!(cis_audit(&bad)[0].detail.contains("not valid YAML"));
    }

    #[test]
    fn gather_asks_the_guest() {
        let g = FakeGuest {
            answers: vec![
                (
                    "cat /etc/rancher".into(),
                    "secrets-encryption: true\n".into(),
                ),
                ("vm.overcommit_memory".into(), "1\n".into()),
                ("psa.yaml".into(), "yes\n".into()),
            ],
            ..Default::default()
        };
        let f = cis_gather(&g);
        assert_eq!(f.sysctls["vm.overcommit_memory"], "1");
        assert!(f.files["/var/lib/rancher/k3s/server/psa.yaml"]);
        assert!(!f.files["/var/lib/rancher/k3s/server/audit.yaml"]);
        assert!(f.config.is_some());
    }
}
