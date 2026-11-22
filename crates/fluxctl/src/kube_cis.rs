// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `kube doctor --cis` and `kube apple doctor --cis`: the read-only CIS profile audit of `kube addons cis-check`
//! ([`crate::kube_harden::cis_audit`]) folded into the doctor reports. One script reads a server's settings; the parser and
//! the verdict mapping are pure. It checks that the profile's settings are in place, not a CIS benchmark result.

use crate::kube_doctor::{Check, Level};
use crate::kube_harden::{CisFacts, cis_audit};
use crate::kube_preflight::Sev;
use crate::{
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::Result;

const SERVER_DIR: &str = "/var/lib/rancher/k3s/server";
const PLATFORM_DROPIN: &str = "/etc/rancher/k3s/config.yaml.d/40-fluxvm-platform.yaml";
const SYSCTLS: [&str; 4] = [
    "vm.panic_on_oom",
    "vm.overcommit_memory",
    "kernel.panic",
    "kernel.panic_on_oops",
];

/// Prints the settings the audit needs in three marked sections. The k3s settings come from the platform drop-in when one
/// exists (what `kube apple platform apply --harden cis` writes), otherwise from `config.yaml` (what `kube addons config` writes).
pub fn gather_script() -> String {
    let mut s = format!(
        "for f in {PLATFORM_DROPIN} /etc/rancher/k3s/config.yaml; do \
         if sudo -n test -f $f; then echo '@@config'; sudo -n cat $f; break; fi; done; echo '@@sysctl'; "
    );
    for k in SYSCTLS {
        s.push_str(&format!("echo \"{k}=$(sysctl -n {k} 2>/dev/null)\"; "));
    }
    s.push_str("echo '@@files'; ");
    for f in ["psa.yaml", "audit.yaml"] {
        let p = format!("{SERVER_DIR}/{f}");
        s.push_str(&format!(
            "if sudo -n test -f {p}; then echo '{p} yes'; else echo '{p} no'; fi; "
        ));
    }
    s
}

pub fn parse_facts(out: &str) -> CisFacts {
    let mut f = CisFacts::default();
    let mut section = "";
    let mut config = String::new();
    let mut have_config = false;
    for line in out.lines() {
        match line.trim_end() {
            "@@config" => {
                section = "config";
                have_config = true;
            }
            "@@sysctl" => section = "sysctl",
            "@@files" => section = "files",
            l => match section {
                "config" => {
                    config.push_str(l);
                    config.push('\n');
                }
                "sysctl" => {
                    if let Some((k, v)) = l.split_once('=')
                        && !v.trim().is_empty()
                    {
                        f.sysctls.insert(k.to_owned(), v.trim().to_owned());
                    }
                }
                "files" => {
                    if let Some((p, yn)) = l.rsplit_once(' ') {
                        f.files.insert(p.to_owned(), yn == "yes");
                    }
                }
                _ => {}
            },
        }
    }
    if have_config {
        f.config = Some(config);
    }
    f
}

/// The audit as doctor checks. `fix` is what to run for a failing check.
pub fn to_checks(f: &CisFacts, fix: &str) -> Vec<Check> {
    cis_audit(f)
        .into_iter()
        .map(|c| Check {
            name: match c.name {
                "config.yaml" => "CIS config",
                "secrets-encryption" => "CIS secrets-encryption",
                "protect-kernel-defaults" => "CIS kernel defaults",
                "pod security admission" => "CIS pod security",
                "audit log" => "CIS audit log",
                "audit policy" => "CIS audit policy",
                "kubelet idle timeout" => "CIS kubelet timeout",
                "service account credentials" => "CIS sa credentials",
                "sysctl" => "CIS sysctl",
                "file" => "CIS file",
                _ => "CIS",
            },
            level: match c.sev {
                Sev::Ok => Level::Ok,
                Sev::Warn => Level::Warn,
                Sev::Fail => Level::Fail,
            },
            fix: if c.sev == Sev::Ok {
                String::new()
            } else {
                fix.to_owned()
            },
            detail: c.detail,
            remedy: None,
        })
        .collect()
}

/// `kube doctor --cis`: the audit of the `kube up` server VM.
pub async fn run_kube<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Vec<Check>> {
    let fix = format!(
        "fluxctl kube addons config --cis --ssh USER@HOST --restart k3s (and harden-files first); see docs/apple-k3s-addons.md ({})",
        o.name
    );
    match kube_host::guest_sh(api, o, &gather_script()).await {
        Ok(out) => Ok(to_checks(&parse_facts(&out), &fix)),
        Err(e) => Ok(vec![Check {
            name: "CIS config",
            level: Level::Warn,
            detail: format!("could not read the server's settings: {e:#}"),
            fix: "start the cluster (fluxctl kube up) and run again".into(),
            remedy: None,
        }]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_reads_the_dropin_then_config_and_every_sysctl_and_file() {
        let s = gather_script();
        assert!(s.starts_with("for f in /etc/rancher/k3s/config.yaml.d/40-fluxvm-platform.yaml /etc/rancher/k3s/config.yaml;"));
        for k in SYSCTLS {
            assert!(s.contains(&format!("sysctl -n {k}")));
        }
        assert!(
            s.contains("/var/lib/rancher/k3s/server/psa.yaml") && s.contains("/server/audit.yaml")
        );
    }

    const GOOD: &str = "@@config\nprotect-kernel-defaults: true\nsecrets-encryption: true\n\
kube-apiserver-arg:\n- admission-control-config-file=/var/lib/rancher/k3s/server/psa.yaml\n\
- audit-log-path=/var/lib/rancher/k3s/server/logs/audit.log\n- audit-policy-file=/var/lib/rancher/k3s/server/audit.yaml\n\
kube-controller-manager-arg:\n- use-service-account-credentials=true\nkubelet-arg:\n- streaming-connection-idle-timeout=5m\n\
@@sysctl\nvm.panic_on_oom=0\nvm.overcommit_memory=1\nkernel.panic=10\nkernel.panic_on_oops=1\n\
@@files\n/var/lib/rancher/k3s/server/psa.yaml yes\n/var/lib/rancher/k3s/server/audit.yaml yes\n";

    #[test]
    fn a_hardened_node_passes() {
        let checks = to_checks(&parse_facts(GOOD), "fix");
        assert!(checks.iter().all(|c| c.level == Level::Ok), "{checks:?}");
        assert!(checks.iter().all(|c| c.fix.is_empty()));
    }

    #[test]
    fn a_plain_node_fails_with_a_fix() {
        let out = "@@sysctl\nvm.panic_on_oom=0\nvm.overcommit_memory=\n@@files\n/var/lib/rancher/k3s/server/psa.yaml no\n";
        let f = parse_facts(out);
        assert!(f.config.is_none());
        let checks = to_checks(&f, "apply it");
        assert_eq!(checks.len(), 1);
        assert_eq!(
            (checks[0].name, checks[0].level),
            ("CIS config", Level::Fail)
        );
        assert_eq!(checks[0].fix, "apply it");
        // with a config but missing settings, each missing setting is its own failure
        let f2 = parse_facts("@@config\nsecrets-encryption: true\n@@sysctl\n@@files\n");
        let c2 = to_checks(&f2, "apply it");
        assert!(
            c2.iter()
                .any(|c| c.name == "CIS kernel defaults" && c.level == Level::Fail)
        );
        assert!(
            c2.iter()
                .any(|c| c.name == "CIS secrets-encryption" && c.level == Level::Ok)
        );
        assert!(
            c2.iter()
                .any(|c| c.name == "CIS sysctl" && c.level == Level::Fail)
        );
    }
}
