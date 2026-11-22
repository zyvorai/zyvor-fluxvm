// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl cluster add user@host...`: bring Linux machines into the cluster over SSH, from one place.
//!
//! The same approach as `mac-fleet` (and the same pieces): the system `ssh` with `BatchMode=yes` and pinned host keys
//! (`StrictHostKeyChecking=yes` unless `--accept-new-hostkey`), the `Transport` trait with a dry-run printer and a test
//! fake, and remote scripts that are pure functions of their inputs, so they are tested as golden text. A secret only
//! ever travels on stdin: the join token goes to a 0600 file in a private temp directory on the host and `install.sh
//! --join` reads it from there; it is never an argument of any process, and the file is removed when the script ends.

use crate::mac_fleet_remote::{
    Finding, Level, Step, StepIn, sh_quote, valid_label, valid_node_name, valid_sha256, valid_url,
};
use std::fmt::Write as _;

pub const DEFAULT_INSTALLER_URL: &str = "https://zyvorai.github.io/zyvor-fluxvm/install.sh";

/// Read-only: what the host is. One `key=value` per line.
pub fn linux_probe_script() -> String {
    r#"export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
yn() { if "$@" >/dev/null 2>&1; then echo yes; else echo no; fi; }
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "distro=$(. /etc/os-release 2>/dev/null; echo "${ID:-}")"
echo "user=$(id -un)"
if [ "$(id -u)" = 0 ]; then S=root
elif sudo -n true >/dev/null 2>&1; then S=passwordless
elif command -v sudo >/dev/null 2>&1; then S=password
else S=none; fi
echo "sudo=$S"
if [ -d /run/systemd/system ]; then I=systemd; elif command -v rc-service >/dev/null 2>&1; then I=openrc; else I=none; fi
echo "init=$I"
echo "kvm=$(yn test -e /dev/kvm)"
echo "curl=$(yn command -v curl)"
echo "fluxctl_version=$(fluxctl --version 2>/dev/null | awk '{print $2}')"
echo "agent_active=$(yn systemctl is-active --quiet fluxvm-agent)"
echo "enrolled=$(yn test -e /var/lib/fluxvm-agent/node-credential)"
"#
    .to_owned()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinuxFacts {
    pub known: bool,
    pub os: String,
    pub arch: String,
    pub distro: String,
    pub user: String,
    /// `root`, `passwordless`, `password` or `none`.
    pub sudo: String,
    /// `systemd`, `openrc` or `none`.
    pub init: String,
    pub kvm: bool,
    pub curl: bool,
    pub fluxctl_version: Option<String>,
    pub agent_active: bool,
    pub enrolled: bool,
}

pub fn parse_linux_facts(out: &str) -> LinuxFacts {
    let mut f = LinuxFacts::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let yes = v == "yes";
        match k {
            "os" => {
                f.os = v.into();
                f.known = !v.is_empty();
            }
            "arch" => f.arch = v.into(),
            "distro" => f.distro = v.into(),
            "user" => f.user = v.into(),
            "sudo" => f.sudo = v.into(),
            "init" => f.init = v.into(),
            "kvm" => f.kvm = yes,
            "curl" => f.curl = yes,
            "fluxctl_version" => f.fluxctl_version = (!v.is_empty()).then(|| v.to_owned()),
            "agent_active" => f.agent_active = yes,
            "enrolled" => f.enrolled = yes,
            _ => {}
        }
    }
    f
}

fn finding(level: Level, id: &'static str, msg: &str, fix: &str) -> Finding {
    Finding {
        level,
        id,
        msg: msg.into(),
        fix: fix.into(),
    }
}

/// Preflight for one Linux host: every finding, `ok` ones included. Empty when the probe learned nothing (dry run).
pub fn check_linux(f: &LinuxFacts) -> Vec<Finding> {
    if !f.known {
        return vec![];
    }
    let mut v = vec![];
    if f.os != "Linux" {
        v.push(finding(
            Level::Error,
            "os",
            &format!("this is {}, not Linux", f.os),
            "for a Mac use: fluxctl mac-fleet up user@host (cluster add is for Linux hosts)",
        ));
        return v;
    }
    if matches!(f.arch.as_str(), "x86_64" | "aarch64" | "arm64") {
        v.push(finding(
            Level::Ok,
            "arch",
            &format!("{} {}", f.distro, f.arch),
            "",
        ));
    } else {
        v.push(finding(
            Level::Error,
            "arch",
            &format!("no FluxVM package for {}", f.arch),
            "FluxVM ships Linux x86_64 and aarch64; build from source on other CPUs",
        ));
    }
    match f.sudo.as_str() {
        "root" | "passwordless" => v.push(finding(Level::Ok, "sudo", &format!("{} can install system packages", f.user), "")),
        "password" => v.push(finding(
            Level::Error,
            "sudo",
            &format!("{} needs a password for sudo, and ssh runs with no terminal to type it on", f.user),
            "allow it once on the host: echo '<user> ALL=(ALL) NOPASSWD:ALL' | sudo tee /etc/sudoers.d/fluxvm (and remove it later), or ssh in as root",
        )),
        _ => v.push(finding(
            Level::Error,
            "sudo",
            "no root and no sudo",
            "ssh in as root, or install sudo and allow this user",
        )),
    }
    if f.init == "none" {
        v.push(finding(
            Level::Error,
            "init",
            "no systemd or OpenRC: nothing to keep the node agent running",
            "run the agent by hand (docs/cluster-quickstart.md) or use a distro with systemd",
        ));
    }
    if !f.curl {
        v.push(finding(
            Level::Error,
            "curl",
            "curl is not installed on the host",
            "sudo apt-get install curl (Fedora: sudo dnf install curl)",
        ));
    }
    if !f.kvm {
        v.push(finding(
            Level::Warn,
            "kvm",
            "no /dev/kvm: the node joins, but VMs will not start here",
            "turn on virtualisation in the firmware; on a cloud VM choose an instance type with nested virtualisation",
        ));
    }
    v
}

pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.level == Level::Error)
}

#[derive(Debug, Clone)]
pub struct JoinSpec {
    /// The central the node joins (`http(s)://host:7799`).
    pub central: String,
    pub name: String,
    pub advertise_url: Option<String>,
    pub labels: Vec<String>,
    /// Where the host fetches `install.sh` from (a mirror, or the docs site).
    pub installer_url: String,
    /// Refuse the installer unless its SHA-256 is this (pin it; the default URL is a moving file).
    pub installer_sha256: Option<String>,
    pub with_k3s: bool,
    pub selftest: bool,
}

/// `JoinSpec` values end up inside a script: only what the shell cannot misread is let through.
pub fn validate_join(j: &JoinSpec) -> Result<(), String> {
    if !valid_url(&j.central) || !j.central.starts_with("http") {
        return Err(format!(
            "the central URL {:?} is not an http(s) URL",
            j.central
        ));
    }
    if !valid_node_name(&j.name) {
        return Err(format!(
            "{:?} is not a node name (lowercase letters, digits, dashes and dots)",
            j.name
        ));
    }
    if let Some(u) = &j.advertise_url
        && !valid_url(u)
    {
        return Err(format!("the advertise URL {u:?} is not an http(s) URL"));
    }
    if let Some(l) = j.labels.iter().find(|l| !valid_label(l)) {
        return Err(format!("{l:?} is not a key=value label"));
    }
    if !valid_url(&j.installer_url) {
        return Err(format!(
            "the installer URL {:?} is not an http(s) URL",
            j.installer_url
        ));
    }
    if let Some(h) = &j.installer_sha256
        && !valid_sha256(h)
    {
        return Err("--installer-sha256 must be 64 hex digits".into());
    }
    Ok(())
}

/// Fetch `install.sh`, check its pin, store the token (from stdin) in a private file, and run `install.sh --join`.
pub fn install_join_script(j: &JoinSpec) -> String {
    let mut args = format!(
        "--join {} --token-file \"$T/token\" --node-name {} --yes",
        sh_quote(&j.central),
        sh_quote(&j.name)
    );
    if let Some(u) = &j.advertise_url {
        let _ = write!(args, " --advertise-url {}", sh_quote(u));
    }
    for l in &j.labels {
        let _ = write!(args, " --label {}", sh_quote(l));
    }
    if !j.selftest {
        args.push_str(" --no-selftest");
    }
    if j.with_k3s {
        args.push_str(" --with-k3s");
    }
    let pin = match &j.installer_sha256 {
        Some(h) => format!(
            r#"if command -v sha256sum >/dev/null 2>&1; then G="$(sha256sum "$T/install.sh" | cut -d' ' -f1)"; else G="$(shasum -a 256 "$T/install.sh" | cut -d' ' -f1)"; fi
[ "$G" = {} ] || {{ echo "install.sh does not match the pinned SHA-256 (got $G); not running it" >&2; exit 1; }}
"#,
            sh_quote(h)
        ),
        None => String::new(),
    };
    format!(
        r#"set -eu
export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
T="$(umask 077; mktemp -d)"
trap 'rm -rf "$T"' EXIT
curl -fsSL -o "$T/install.sh" {url}
{pin}(umask 077; cat > "$T/token")
bash "$T/install.sh" {args}
"#,
        url = sh_quote(&j.installer_url),
    )
}

/// The optional k3s layer on a node that is already in the cluster.
pub fn k3s_script() -> String {
    r#"set -eu
export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$HOME/.local/bin:$PATH"
command -v fluxctl >/dev/null 2>&1 || { echo "fluxctl is not installed here" >&2; exit 1; }
fluxctl kube up
"#
    .to_owned()
}

/// The steps `add` runs for one host, given what the probe found. A host that is already enrolled with a running agent
/// needs nothing (and no token); with unknown facts (a dry run) every step is listed.
pub fn plan_add(facts: &LinuxFacts, j: &JoinSpec) -> Vec<Step> {
    let mut v = vec![];
    let done = facts.known && facts.enrolled && facts.agent_active;
    if !done {
        v.push(Step {
            name: "install-and-join",
            script: install_join_script(j),
            stdin: StepIn::Token,
        });
    }
    if j.with_k3s && !done {
        // `install.sh --with-k3s` already ran it; nothing more to do here
    } else if j.with_k3s {
        v.push(Step {
            name: "k3s",
            script: k3s_script(),
            stdin: StepIn::None,
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JoinSpec {
        JoinSpec {
            central: "http://10.0.0.2:7799".into(),
            name: "worker-1".into(),
            advertise_url: Some("http://10.0.0.5:7788".into()),
            labels: vec!["zone=lab".into(), "gpu=true".into()],
            installer_url: DEFAULT_INSTALLER_URL.into(),
            installer_sha256: None,
            with_k3s: false,
            selftest: true,
        }
    }

    /// Golden text: `UPDATE_GOLDEN=1 cargo test -p fluxctl cluster_remote` rewrites the files.
    fn golden(name: &str, got: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/cluster_golden")
            .join(name);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, got).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (UPDATE_GOLDEN=1 creates it)", path.display()));
        assert_eq!(got, want, "golden file {name} differs");
    }

    #[test]
    fn the_probe_script_is_stable() {
        golden("probe.sh", &linux_probe_script());
    }

    #[test]
    fn the_install_and_join_script_is_stable() {
        golden("join.sh", &install_join_script(&spec()));
    }

    #[test]
    fn a_pinned_installer_script_is_stable() {
        let mut s = spec();
        s.installer_sha256 = Some("a".repeat(64));
        s.installer_url = "https://mirror.example/install.sh".into();
        s.selftest = false;
        s.with_k3s = true;
        s.advertise_url = None;
        golden("join-pinned.sh", &install_join_script(&s));
    }

    #[test]
    fn the_k3s_script_is_stable() {
        golden("k3s.sh", &k3s_script());
    }

    #[test]
    fn the_token_is_never_in_a_script_or_an_argument() {
        let s = install_join_script(&spec());
        // it is read from stdin into a file and handed over as a file path
        assert!(s.contains(r#"cat > "$T/token""#));
        assert!(s.contains(r#"--token-file "$T/token""#));
        assert!(!s.contains("fvjt_") && !s.contains("--token "));
        assert!(s.contains("trap 'rm -rf \"$T\"' EXIT"));
    }

    #[test]
    fn hostile_values_are_refused_before_they_reach_a_script() {
        let mut s = spec();
        s.name = "a;reboot".into();
        assert!(validate_join(&s).is_err());
        let mut s = spec();
        s.central = "http://x';reboot;'".into();
        assert!(validate_join(&s).is_err());
        let mut s = spec();
        s.labels = vec!["a b=c".into()];
        assert!(validate_join(&s).is_err());
        let mut s = spec();
        s.installer_sha256 = Some("xyz".into());
        assert!(validate_join(&s).is_err());
        assert!(validate_join(&spec()).is_ok());
    }

    #[test]
    fn facts_parse_and_preflight_names_the_fix() {
        let f = parse_linux_facts(
            "os=Linux\narch=x86_64\ndistro=ubuntu\nuser=sus\nsudo=password\ninit=systemd\nkvm=no\ncurl=yes\nfluxctl_version=\nagent_active=no\nenrolled=no\n",
        );
        assert!(f.known && f.fluxctl_version.is_none());
        let c = check_linux(&f);
        assert!(has_errors(&c));
        let sudo = c.iter().find(|x| x.id == "sudo").unwrap();
        assert!(sudo.fix.contains("NOPASSWD"));
        assert_eq!(c.iter().find(|x| x.id == "kvm").unwrap().level, Level::Warn);
        let mac = parse_linux_facts("os=Darwin\narch=arm64\n");
        assert!(check_linux(&mac)[0].fix.contains("mac-fleet"));
        assert!(check_linux(&LinuxFacts::default()).is_empty());
    }

    #[test]
    fn an_enrolled_running_node_needs_no_steps() {
        let mut f = LinuxFacts {
            known: true,
            os: "Linux".into(),
            ..Default::default()
        };
        assert_eq!(plan_add(&f, &spec()).len(), 1);
        f.enrolled = true;
        f.agent_active = true;
        assert!(plan_add(&f, &spec()).is_empty());
        let mut k = spec();
        k.with_k3s = true;
        assert_eq!(plan_add(&f, &k)[0].name, "k3s");
        // unknown facts (dry run): everything is listed
        assert_eq!(
            plan_add(&LinuxFacts::default(), &spec())[0].stdin,
            StepIn::Token
        );
    }
}
