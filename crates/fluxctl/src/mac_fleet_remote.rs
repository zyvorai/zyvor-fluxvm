// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet`: everything that is *sent* to a Mac, as pure functions of their inputs, so that the exact remote
//! commands are golden-tested and `--dry-run` can print them. Nothing here touches the network or the disk.
//!
//! Every script is idempotent: it checks what is already true and says so. Secrets never appear in a script: the join
//! token travels on the stdin of a fixed `cat > file` command (see [`put_token_script`]).

use std::fmt::Write as _;

pub const DAEMON_LABEL: &str = "dev.zyvor.fluxvm";
pub const AGENT_LABEL: &str = "dev.zyvor.fluxvm-agent";
pub const KUBE_LABEL: &str = "dev.zyvor.fluxvm.kube";
pub const DEFAULT_RELEASE_BASE: &str = "https://github.com/zyvorai/zyvor-fluxvm/releases";
/// Apple's licence limit, and the daemon's: at most two macOS guests run at once on one Mac.
pub const MACOS_SLOTS: usize = 2;
/// First macOS with the Virtualization.framework features FluxVM uses (docs/macos.md).
pub const MIN_MACOS_MAJOR: u32 = 14;
pub const MIN_DISK_GIB: u64 = 20;
pub const K3S_DISK_GIB: u64 = 40;

/// POSIX single-quote a string so a shell reads it back as one literal word.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `/bin/sh -c '<script>'`: the one shape every remote command takes, whatever the login shell of the Mac is.
pub fn remote(script: &str) -> String {
    format!("/bin/sh -c {}", sh_quote(script))
}

/// A join or bearer token held in memory. Debug never shows it.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Replace anything that looks like a FluxVM token (`fvjt_`, `fvnc_`, `fvt_`) and, when given, the exact secret.
pub fn scrub(text: &str, secret: Option<&Secret>) -> String {
    let mut out = text.to_owned();
    if let Some(s) = secret
        && let Ok(t) = std::str::from_utf8(s.expose())
        && !t.trim().is_empty()
    {
        out = out.replace(t.trim(), "<redacted>");
    }
    let mut res = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(i) = ["fvjt_", "fvnc_", "fvt_"]
        .iter()
        .filter_map(|p| rest.find(p))
        .min()
    {
        res.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || "._-".contains(c)))
            .unwrap_or(tail.len());
        res.push_str("<redacted>");
        rest = &tail[end..];
    }
    res.push_str(rest);
    res
}

// ---- validation: values that end up inside a script ----------------------------------------------------------------

pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.starts_with(|c: char| c.is_ascii_digit())
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'+')
}

pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn valid_node_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && !n.starts_with(['-', '.'])
        && n.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.')
}

pub fn valid_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://") || u.starts_with("file:///"))
        && u.len() <= 512
        && u.bytes()
            .all(|c| c.is_ascii_graphic() && !b"'\"\\`$;&|<>(){}*?!".contains(&c))
}

/// `key=value` for a node label: nothing the shell or the plist would treat specially.
pub fn valid_label(l: &str) -> bool {
    match l.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && l.len() <= 200
                && l.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-/=:".contains(&c))
        }
        None => false,
    }
}

// ---- the probe and what it tells us ----------------------------------------------------------------------------------

pub(crate) const PRELUDE: &str = r#"export PATH="/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH"
ROOT="$HOME/.local/share/fluxvm"
FLUXCTL="$ROOT/current/bin/fluxctl"
[ -x "$FLUXCTL" ] || FLUXCTL="$(command -v fluxctl || true)"
GUI="gui/$(id -u)"
"#;

/// A read-only script that prints `key=value` lines. Used by `doctor`, `status` and as the first step of `up`.
pub fn probe_script() -> String {
    format!(
        r#"{PRELUDE}yn() {{ if "$@" >/dev/null 2>&1; then echo yes; else echo no; fi; }}
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "macos=$(sw_vers -productVersion 2>/dev/null)"
echo "user=$(id -un)"
echo "console_user=$(stat -f %Su /dev/console 2>/dev/null)"
echo "disk_free_gib=$(df -g "$HOME" 2>/dev/null | awk 'NR==2{{print $4}}')"
echo "filevault=$(fdesetup status 2>/dev/null | head -n1)"
echo "autologin=$(defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null)"
echo "brew=$(yn command -v brew)"
echo "fluxctl_version=$([ -n "$FLUXCTL" ] && "$FLUXCTL" --version 2>/dev/null | awk '{{print $2}}')"
echo "agent_binary=$([ -n "$FLUXCTL" ] && yn test -x "$(dirname "$FLUXCTL")/fluxvm-agent")"
echo "daemon_loaded=$(yn launchctl print "$GUI/{DAEMON_LABEL}")"
echo "agent_loaded=$(yn launchctl print "$GUI/{AGENT_LABEL}")"
echo "kube_loaded=$(yn launchctl print "$GUI/{KUBE_LABEL}")"
echo "daemon_healthy=$(yn curl -fsS -m 3 http://127.0.0.1:7788/healthz)"
echo "enrolled=$(yn test -f "$HOME/Library/Application Support/FluxVM/agent/node-credential")"
echo "vms_json=$(curl -fsS -m 5 http://127.0.0.1:7788/v1/vms 2>/dev/null | tr -d '\n')"
echo "kube_json=$([ -n "$FLUXCTL" ] && "$FLUXCTL" kube status 2>/dev/null | tr -d '\n')"
echo "headless_json=$([ -n "$FLUXCTL" ] && "$FLUXCTL" mac host headless-check --json 2>/dev/null | tr -d '\n')"
"#
    )
}

/// What the probe found. `known` is false when nothing parsed (a dry run, or a probe that failed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    pub known: bool,
    pub os: String,
    pub arch: String,
    pub macos: String,
    pub user: String,
    pub console_user: String,
    pub disk_free_gib: Option<u64>,
    pub filevault_on: Option<bool>,
    pub autologin: String,
    pub brew: bool,
    pub fluxctl_version: Option<String>,
    pub agent_binary: bool,
    pub daemon_loaded: bool,
    pub agent_loaded: bool,
    pub kube_loaded: bool,
    pub daemon_healthy: bool,
    pub enrolled: bool,
    pub vms_json: Option<serde_json::Value>,
    pub kube_json: Option<serde_json::Value>,
    /// `fluxctl mac host headless-check --json` run on the Mac (None from an older install that lacks it).
    pub headless_json: Option<serde_json::Value>,
}

pub fn parse_facts(out: &str) -> Facts {
    let mut f = Facts::default();
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
            "macos" => f.macos = v.into(),
            "user" => f.user = v.into(),
            "console_user" => f.console_user = v.into(),
            "disk_free_gib" => f.disk_free_gib = v.parse().ok(),
            "filevault" => {
                f.filevault_on = if v.contains("is On") {
                    Some(true)
                } else if v.contains("is Off") {
                    Some(false)
                } else {
                    None
                }
            }
            "autologin" => f.autologin = v.into(),
            "brew" => f.brew = yes,
            "fluxctl_version" => f.fluxctl_version = (!v.is_empty()).then(|| v.to_owned()),
            "agent_binary" => f.agent_binary = yes,
            "daemon_loaded" => f.daemon_loaded = yes,
            "agent_loaded" => f.agent_loaded = yes,
            "kube_loaded" => f.kube_loaded = yes,
            "daemon_healthy" => f.daemon_healthy = yes,
            "enrolled" => f.enrolled = yes,
            "vms_json" => f.vms_json = serde_json::from_str(v).ok(),
            "kube_json" => f.kube_json = serde_json::from_str(v).ok(),
            "headless_json" => f.headless_json = serde_json::from_str(v).ok(),
            _ => {}
        }
    }
    f
}

impl Facts {
    pub fn macos_major(&self) -> Option<u32> {
        self.macos.split('.').next()?.parse().ok()
    }

    /// `(total, running macOS guests)` from the daemon's VM list, or None when it did not answer.
    pub fn vm_counts(&self) -> Option<(usize, usize)> {
        let items = self.vms_json.as_ref()?.get("items")?.as_array()?;
        let macos = items
            .iter()
            .filter(|v| {
                v["status"].as_str() == Some("running")
                    && v["request"]["apple"]["guest_os"].as_str() == Some("macos")
            })
            .count();
        Some((items.len(), macos))
    }

    pub fn running_vms(&self) -> usize {
        self.vms_json
            .as_ref()
            .and_then(|v| v.get("items")?.as_array().cloned())
            .map(|items| {
                items
                    .iter()
                    .filter(|v| v["status"].as_str() == Some("running"))
                    .count()
            })
            .unwrap_or(0)
    }

    fn headless_check(&self, name: &str) -> Option<&serde_json::Value> {
        self.headless_json
            .as_ref()?
            .get("checks")?
            .as_array()?
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
    }

    /// One word for the status table: `unlocked`, `locked`, `login window`, or `-` when the Mac could not say.
    pub fn session_state(&self) -> String {
        let status = |n: &str| {
            self.headless_check(n)
                .and_then(|c| c["status"].as_str().map(str::to_owned))
        };
        match (
            status("console session").as_deref(),
            status("screen lock").as_deref(),
        ) {
            (Some("fail"), _) => "login window",
            (_, Some("fail")) => "locked",
            (_, Some("ok")) => "unlocked",
            _ => "-",
        }
        .into()
    }

    /// One word for the table: `none`, `stopped`, `starting`, `running`, or `-` when `kube status` could not be read.
    pub fn k3s_state(&self) -> String {
        let Some(k) = &self.kube_json else {
            return "-".into();
        };
        match k["vm"]["status"].as_str() {
            None => "none".into(),
            Some("running") if k["api_answers"].as_bool() == Some(true) => "running".into(),
            Some("running") => "starting".into(),
            Some(s) => s.to_owned(),
        }
    }
}

// ---- doctor ----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Finding {
    pub level: Level,
    pub id: &'static str,
    pub msg: String,
    /// The exact thing to do about it; empty for `ok`.
    pub fix: String,
}

pub(crate) fn fnd(
    level: Level,
    id: &'static str,
    msg: impl Into<String>,
    fix: impl Into<String>,
) -> Finding {
    Finding {
        level,
        id,
        msg: msg.into(),
        fix: fix.into(),
    }
}

/// What the operator asked for, as far as preflight cares.
#[derive(Debug, Clone, Copy, Default)]
pub struct Want {
    pub enroll: bool,
    pub k3s: bool,
}

/// Preflight: every finding for this Mac, `ok` ones included. Empty when the probe learned nothing (dry run).
pub fn check(facts: &Facts, want: Want) -> Vec<Finding> {
    if !facts.known {
        return vec![];
    }
    let mut v = vec![];
    if facts.os != "Darwin" || facts.arch != "arm64" {
        v.push(fnd(
            Level::Error,
            "platform",
            format!("{} {} is not Apple silicon macOS", facts.os, facts.arch),
            "FluxVM's macOS backend needs an Apple silicon Mac (M1 or later); Intel Macs are not supported",
        ));
        return v;
    }
    v.push(fnd(Level::Ok, "platform", "Apple silicon macOS", ""));
    match facts.macos_major() {
        Some(m) if m >= MIN_MACOS_MAJOR => {
            v.push(fnd(Level::Ok, "macos", format!("macOS {}", facts.macos), ""))
        }
        _ => v.push(fnd(
            Level::Error,
            "macos",
            format!("macOS {} is older than {MIN_MACOS_MAJOR}", facts.macos),
            "update with `softwareupdate --install --all --restart` (or System Settings > General > Software Update)",
        )),
    }
    let need = if want.k3s { K3S_DISK_GIB } else { MIN_DISK_GIB };
    match facts.disk_free_gib {
        Some(g) if g >= need => v.push(fnd(Level::Ok, "disk", format!("{g} GiB free"), "")),
        Some(g) => v.push(fnd(
            Level::Error,
            "disk",
            format!(
                "{g} GiB free, {need} needed{}",
                if want.k3s { " for a k3s guest" } else { "" }
            ),
            "free disk space (`du -sh ~/Library/Application\\ Support/FluxVM` shows what FluxVM uses) or use a Mac with more storage",
        )),
        None => v.push(fnd(
            Level::Warn,
            "disk",
            "free disk space could not be read",
            "run `df -h ~` on the Mac",
        )),
    }
    if facts.console_user == facts.user && !facts.user.is_empty() {
        v.push(fnd(
            Level::Ok,
            "session",
            format!("{} is logged in at the console", facts.user),
            "",
        ));
    } else {
        let who = if facts.console_user.is_empty() || facts.console_user == "root" {
            "nobody is logged in at the console".to_owned()
        } else {
            format!(
                "the console user is {}, not {}",
                facts.console_user, facts.user
            )
        };
        v.push(fnd(
            Level::Error,
            "session",
            who,
            format!(
                "FluxVM runs as a LaunchAgent in the logged-in user's session: log in as {} on the Mac, or ssh as the console user",
                facts.user
            ),
        ));
    }
    match (facts.filevault_on, facts.autologin.is_empty()) {
        (Some(true), _) => v.push(fnd(
            Level::Warn,
            "filevault",
            "FileVault is on: after a reboot nothing runs, and ssh is not available, until someone unlocks the disk at the console",
            "for an unattended Mac turn it off (`sudo fdesetup disable`, or System Settings > Privacy & Security > FileVault), or plan to unlock it by hand after reboots",
        )),
        (Some(false), true) => v.push(fnd(
            Level::Warn,
            "autologin",
            "automatic login is off: after a reboot there is no user session, so the daemon and every VM stay down",
            "System Settings > Users & Groups > Automatically log in as: a dedicated user (docs/agent-density.md#unattended-hosts)",
        )),
        (Some(false), false) => v.push(fnd(
            Level::Ok,
            "autologin",
            format!("automatic login as {}", facts.autologin),
            "",
        )),
        (None, _) => v.push(fnd(
            Level::Warn,
            "filevault",
            "FileVault state could not be read",
            "run `fdesetup status` on the Mac",
        )),
    }
    if want.enroll && facts.fluxctl_version.is_some() && !facts.agent_binary {
        v.push(fnd(
            Level::Error,
            "agent-binary",
            "fluxvm-agent is not next to fluxctl: this release does not ship the node agent",
            "install a release after 0.4.21 (`--release`), or build `cargo build --release -p fluxvm-agent` and put it beside fluxctl",
        ));
    }
    v
}

pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|x| x.level == Level::Error)
}

// ---- the steps of `up`, `upgrade`, `remove` --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Via {
    /// Download the release package, verify its SHA-256, unpack under ~/.local/share/fluxvm/<version>.
    Release,
    /// `brew install zyvorai/tap/fluxvm` (the tap was withdrawn after 0.4.21; the version cannot be pinned).
    Brew,
}

#[derive(Debug, Clone)]
pub struct InstallSpec {
    pub version: String,
    pub sha256: Option<String>,
    pub release_base: String,
    pub via: Via,
}

#[derive(Debug, Clone)]
pub struct K3sSpec {
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub wait_secs: u64,
}

impl Default for K3sSpec {
    fn default() -> Self {
        Self {
            cpus: 2,
            memory_mib: 4096,
            disk_gib: 30,
            wait_secs: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnrollSpec {
    pub name: String,
    pub central: String,
    pub advertise_url: Option<String>,
    pub labels: Vec<String>,
}

pub fn install_script(s: &InstallSpec) -> String {
    if s.via == Via::Brew {
        return format!(
            r#"set -eu
{PRELUDE}command -v brew >/dev/null || {{ echo "Homebrew is not installed on this Mac" >&2; exit 1; }}
if brew list zyvorai/tap/fluxvm >/dev/null 2>&1; then brew upgrade zyvorai/tap/fluxvm || true; else brew install zyvorai/tap/fluxvm; fi
echo "installed via Homebrew"
"#
        );
    }
    let v = &s.version;
    let base = &s.release_base;
    let pin = match &s.sha256 {
        Some(p) => {
            let p = p.to_ascii_lowercase();
            format!(
                "[ \"$got\" = \"{p}\" ] || {{ echo \"sha256 $got is not the pinned {p}\" >&2; exit 1; }}\n"
            )
        }
        None => String::new(),
    };
    format!(
        r#"set -eu
{PRELUDE}V={v}
BASE={base}
if [ -x "$ROOT/$V/bin/fluxctl" ] && [ "$(readlink "$ROOT/current" 2>/dev/null)" = "$V" ]; then echo "fluxvm $V already installed"; exit 0; fi
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || {{ echo "not Apple silicon macOS" >&2; exit 2; }}
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
A="fluxvm-$V-macos-arm64.tar.gz"
curl -fsSL --retry 3 -o "$T/$A" "$BASE/download/v$V/$A"
curl -fsSL --retry 3 -o "$T/$A.sha256" "$BASE/download/v$V/$A.sha256"
got="$(shasum -a 256 "$T/$A" | awk '{{print $1}}')"
want="$(awk '{{print $1}}' "$T/$A.sha256" | head -n1)"
[ "${{#want}}" = 64 ] || {{ echo "the .sha256 file is not a SHA-256" >&2; exit 1; }}
[ "$got" = "$want" ] || {{ echo "checksum mismatch: got $got, the release says $want" >&2; exit 1; }}
{pin}tar -xzf "$T/$A" -C "$T"
mkdir -p "$ROOT" "$HOME/.local/bin"
rm -rf "$ROOT/$V"
mv "$T/fluxvm-$V-macos-arm64" "$ROOT/$V"
ln -sfn "$V" "$ROOT/current"
for f in "$ROOT/current/bin/"*; do
  b="$HOME/.local/bin/$(basename "$f")"
  if [ ! -e "$b" ] || [ -L "$b" ]; then ln -sfn "$f" "$b"; fi
done
if command -v brew >/dev/null 2>&1 && ! brew list hivex >/dev/null 2>&1; then brew install hivex >/dev/null 2>&1 || echo "note: brew install hivex failed; guest image inspection needs it" >&2; fi
echo "installed fluxvm $V (sha256 $got)"
"#
    )
}

const WAIT_HEALTHY: &str = r#"i=0
until curl -fsS -m 2 http://127.0.0.1:7788/healthz >/dev/null 2>&1; do
  i=$((i+1)); [ "$i" -le 30 ] || { echo "the daemon did not answer /healthz within 30s (see ~/Library/Logs/fluxvm.log)" >&2; exit 1; }
  sleep 1
done
echo "daemon healthy"
"#;

pub fn daemon_script(via: Via) -> String {
    if via == Via::Brew {
        return format!(
            "set -eu\n{PRELUDE}brew services start zyvorai/tap/fluxvm >/dev/null\n{WAIT_HEALTHY}"
        );
    }
    format!(
        r#"set -eu
{PRELUDE}[ -n "$FLUXCTL" ] || {{ echo "fluxctl is not installed" >&2; exit 1; }}
if launchctl print "$GUI/{DAEMON_LABEL}" >/dev/null 2>&1; then echo "daemon already loaded"; else "$FLUXCTL" service install >/dev/null; fi
{WAIT_HEALTHY}"#
    )
}

/// The fixed command that stores the join token from its stdin: mode 0600, atomically, never through argv.
pub fn put_token_script() -> String {
    r#"set -eu
umask 077
D="$HOME/.config/fluxvm"
mkdir -p "$D"
cat > "$D/agent.token.new"
mv "$D/agent.token.new" "$D/agent.token"
"#
    .into()
}

pub fn enroll_script(e: &EnrollSpec, with_token: bool) -> String {
    let mut args = format!(
        "--name {} --central {}",
        sh_quote(&e.name),
        sh_quote(&e.central)
    );
    if let Some(u) = &e.advertise_url {
        let _ = write!(args, " --advertise-url {}", sh_quote(u));
    }
    if with_token {
        args.push_str(" --token-file \"$TOKF\"");
    }
    for l in &e.labels {
        let _ = write!(args, " --label {}", sh_quote(l));
    }
    let wait = if with_token {
        r#"i=0
until [ -f "$CRED" ] || [ "$i" -ge 30 ]; do sleep 2; i=$((i+1)); done
if [ -f "$CRED" ]; then rm -f "$TOKF"; echo "enrolled; join token removed from this Mac"
else echo "the node did not enroll within 60s; the join token file is kept at $TOKF (see ~/Library/Logs/fluxvm-agent.log)" >&2; exit 1; fi
"#
    } else {
        "echo \"node agent installed (already enrolled)\"\n"
    };
    format!(
        r#"set -eu
{PRELUDE}[ -n "$FLUXCTL" ] || {{ echo "fluxctl is not installed" >&2; exit 1; }}
TOKF="$HOME/.config/fluxvm/agent.token"
CRED="$HOME/Library/Application Support/FluxVM/agent/node-credential"
"$FLUXCTL" node-agent install {args} >/dev/null
{wait}"#
    )
}

pub fn k3s_script(k: &K3sSpec) -> String {
    let wait = if k.wait_secs > 0 {
        format!(
            r#"i=0
until "$FLUXCTL" kube status 2>/dev/null | grep -q '"api_answers": true'; do
  i=$((i+5)); [ "$i" -le {w} ] || {{ echo "k3s did not answer within {w}s; it keeps starting in the background (fluxctl kube status)" >&2; exit 1; }}
  sleep 5
done
echo "k3s API answers"
"#,
            w = k.wait_secs
        )
    } else {
        "echo \"k3s is starting in the background; fluxctl mac-fleet status shows when it answers\"\n"
            .into()
    };
    format!(
        r#"set -eu
{PRELUDE}[ -n "$FLUXCTL" ] || {{ echo "fluxctl is not installed" >&2; exit 1; }}
if launchctl print "$GUI/{KUBE_LABEL}" >/dev/null 2>&1; then echo "k3s agent already loaded"; else "$FLUXCTL" kube agent install --cpus {} --memory-mib {} --disk-gib {} >/dev/null; fi
{wait}"#,
        k.cpus, k.memory_mib, k.disk_gib
    )
}

/// After a new version is unpacked: point the LaunchAgents at it (they hold the versioned path) and restart them.
pub fn restart_script(version: &str, via: Via) -> String {
    if via == Via::Brew {
        return format!(
            "set -eu\n{PRELUDE}brew services restart zyvorai/tap/fluxvm >/dev/null\necho \"daemon restarted\"\n"
        );
    }
    format!(
        r#"set -eu
{PRELUDE}V={version}
for L in {DAEMON_LABEL} {AGENT_LABEL} {KUBE_LABEL}; do
  P="$HOME/Library/LaunchAgents/$L.plist"
  [ -f "$P" ] || continue
  if grep -q "$ROOT/$V/bin/" "$P" && launchctl print "$GUI/$L" >/dev/null 2>&1; then echo "$L already runs $V"; continue; fi
  sed -i '' "s#$ROOT/[^/]*/bin/#$ROOT/$V/bin/#g" "$P"
  launchctl bootout "$GUI/$L" 2>/dev/null || true
  launchctl bootstrap "$GUI" "$P"
  echo "$L restarted on $V"
done
"#
    )
}

pub fn remove_script(k3s: bool, purge: bool) -> String {
    let kube = if k3s {
        "[ -n \"$FLUXCTL\" ] && \"$FLUXCTL\" kube down --purge >/dev/null 2>&1 || true\n"
    } else {
        ""
    };
    let purge_cmds = if purge {
        "rm -rf \"$HOME/Library/Application Support/FluxVM/agent\"\n"
    } else {
        ""
    };
    format!(
        r#"set -eu
{PRELUDE}{kube}for L in {KUBE_LABEL} {AGENT_LABEL} {DAEMON_LABEL}; do
  launchctl bootout "$GUI/$L" 2>/dev/null || true
  rm -f "$HOME/Library/LaunchAgents/$L.plist"
done
rm -f "$HOME/.config/fluxvm/agent.token" "$HOME/.config/fluxvm/agent.token.new"
{purge_cmds}for b in "$HOME/.local/bin/"*; do
  [ -L "$b" ] && case "$(readlink "$b")" in "$ROOT"/*) rm -f "$b";; esac
done
rm -rf "$ROOT"
echo "removed FluxVM (VM images and data under ~/Library/Application Support/FluxVM are kept)"
"#
    )
}

/// `exec`: one argument is shell text as typed; several are quoted word by word.
pub fn exec_script(args: &[String]) -> String {
    let cmd = if args.len() == 1 {
        args[0].clone()
    } else {
        args.iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!("{PRELUDE}{cmd}\n")
}

// ---- the plan --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum StepIn {
    None,
    /// The join token goes on stdin.
    Token,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: &'static str,
    pub script: String,
    pub stdin: StepIn,
}

fn step(name: &'static str, script: String) -> Step {
    Step {
        name,
        script,
        stdin: StepIn::None,
    }
}

#[derive(Debug, Clone)]
pub struct UpSpec {
    pub install: InstallSpec,
    pub enroll: Option<EnrollSpec>,
    pub have_token: bool,
    pub k3s: Option<K3sSpec>,
}

/// The steps `up` runs, given what the probe found. Enrollment is left out when the facts show it is done; with unknown
/// facts (dry run) every step is listed, and each script is still a no-op where its work is already done.
pub fn plan_up(facts: &Facts, u: &UpSpec) -> Vec<Step> {
    let mut v = vec![
        step("install", install_script(&u.install)),
        step("daemon", daemon_script(u.install.via)),
    ];
    if let Some(e) = &u.enroll {
        let done = facts.known && facts.enrolled && facts.agent_loaded;
        if !done {
            let send_token = !(facts.known && facts.enrolled) && u.have_token;
            if send_token {
                v.push(Step {
                    name: "join-token",
                    script: put_token_script(),
                    stdin: StepIn::Token,
                });
            }
            v.push(step("enroll", enroll_script(e, send_token)));
        }
    }
    if let Some(k) = &u.k3s {
        v.push(step("k3s", k3s_script(k)));
    }
    v
}

pub fn plan_upgrade(install: &InstallSpec) -> Vec<Step> {
    vec![
        step("install", install_script(install)),
        step("restart", restart_script(&install.version, install.via)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/mac_fleet_golden")
            .join(name);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, actual).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {name}; run with UPDATE_GOLDEN=1"));
        assert_eq!(
            want, actual,
            "golden {name} differs (UPDATE_GOLDEN=1 to accept)"
        );
    }

    pub fn install() -> InstallSpec {
        InstallSpec {
            version: "0.4.21".into(),
            sha256: Some("a".repeat(64)),
            release_base: DEFAULT_RELEASE_BASE.into(),
            via: Via::Release,
        }
    }

    fn enroll() -> EnrollSpec {
        EnrollSpec {
            name: "mini-1".into(),
            central: "https://fleet.example:7790".into(),
            advertise_url: None,
            labels: vec!["zone=lab".into()],
        }
    }

    #[test]
    fn golden_scripts() {
        golden("probe.sh", &probe_script());
        golden("install.sh", &install_script(&install()));
        golden(
            "install-unpinned.sh",
            &install_script(&InstallSpec {
                sha256: None,
                ..install()
            }),
        );
        golden("daemon.sh", &daemon_script(Via::Release));
        golden("put-token.sh", &put_token_script());
        golden("enroll-with-token.sh", &enroll_script(&enroll(), true));
        golden("enroll-enrolled.sh", &enroll_script(&enroll(), false));
        golden("k3s.sh", &k3s_script(&K3sSpec::default()));
        golden(
            "k3s-wait.sh",
            &k3s_script(&K3sSpec {
                wait_secs: 300,
                ..K3sSpec::default()
            }),
        );
        golden("restart.sh", &restart_script("0.4.22", Via::Release));
        golden("remove.sh", &remove_script(true, false));
        golden("remove-purge.sh", &remove_script(false, true));
        golden(
            "exec-multi.sh",
            &exec_script(&["echo".into(), "it's here".into()]),
        );
    }

    #[test]
    fn quoting_survives_a_single_quote() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert!(remote("echo hi").starts_with("/bin/sh -c 'echo hi'"));
    }

    #[test]
    fn a_single_exec_argument_is_shell_text_several_are_words() {
        assert!(exec_script(&["uname -a | wc -c".into()]).ends_with("uname -a | wc -c\n"));
        assert!(exec_script(&["ls".into(), "a b".into()]).ends_with("'ls' 'a b'\n"));
    }

    #[test]
    fn the_token_is_never_in_any_script() {
        let tok = "fvjt_abc.SECRETSECRET";
        let u = UpSpec {
            install: install(),
            enroll: Some(enroll()),
            have_token: true,
            k3s: Some(K3sSpec::default()),
        };
        for s in plan_up(&Facts::default(), &u) {
            assert!(!s.script.contains(tok), "{}", s.name);
            assert!(!s.script.contains("--token "), "{}", s.name);
        }
        let s = Secret::new(tok.as_bytes().to_vec());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(
            scrub(
                "denied fvjt_abc.SECRETSECRET now fvnc_x.1.mac end",
                Some(&s)
            ),
            "denied <redacted> now <redacted> end"
        );
    }

    #[test]
    fn validation_rejects_shell_metacharacters() {
        assert!(valid_version("0.4.21") && valid_version("0.5.0-rc1"));
        assert!(
            !valid_version("0.4.21; rm -rf /") && !valid_version("latest") && !valid_version("")
        );
        assert!(
            valid_sha256(&"f".repeat(64)) && !valid_sha256("zz") && !valid_sha256(&"f".repeat(63))
        );
        assert!(valid_url("https://github.com/x/y/releases") && valid_url("file:///srv/mirror"));
        assert!(
            !valid_url("https://a/b'; id; '") && !valid_url("ftp://x") && !valid_url("https://a b")
        );
        assert!(valid_label("zone=lab") && valid_label("kairon.zyvor.dev/chip=m4"));
        assert!(!valid_label("a=b c") && !valid_label("novalue") && !valid_label("a=$(id)"));
        assert!(
            valid_node_name("mini-1.lab") && !valid_node_name("Mini") && !valid_node_name("-x")
        );
    }

    pub fn facts_ok() -> Facts {
        parse_facts(
            "os=Darwin\narch=arm64\nmacos=15.1\nuser=admin\nconsole_user=admin\ndisk_free_gib=200\n\
             filevault=FileVault is Off.\nautologin=admin\nbrew=yes\nfluxctl_version=0.4.21\nagent_binary=yes\n\
             daemon_loaded=yes\nagent_loaded=no\nkube_loaded=no\ndaemon_healthy=yes\nenrolled=no\n\
             vms_json={\"items\":[{\"status\":\"running\",\"request\":{\"apple\":{\"guest_os\":\"macos\"}}},{\"status\":\"stopped\",\"request\":{}}]}\n\
             kube_json={\"vm\":{\"id\":\"x\",\"status\":\"running\"},\"api_answers\":true}\n",
        )
    }

    #[test]
    fn the_probe_output_parses() {
        let f = facts_ok();
        assert!(f.known && f.daemon_healthy && !f.enrolled);
        assert_eq!(f.vm_counts(), Some((2, 1)));
        assert_eq!(f.running_vms(), 1);
        assert_eq!(f.k3s_state(), "running");
        assert_eq!(f.macos_major(), Some(15));
        assert!(!parse_facts("garbage").known);
        assert_eq!(Facts::default().k3s_state(), "-");
    }

    #[test]
    fn doctor_names_the_fix() {
        let want = Want {
            enroll: true,
            k3s: true,
        };
        assert!(!has_errors(&check(&facts_ok(), want)));
        let mut x = facts_ok();
        x.arch = "x86_64".into();
        assert!(check(&x, Want::default())[0].fix.contains("Apple silicon"));
        let mut x = facts_ok();
        x.macos = "13.6".into();
        x.disk_free_gib = Some(10);
        x.console_user = "".into();
        x.filevault_on = Some(true);
        x.agent_binary = false;
        let c = check(&x, want);
        let ids: Vec<_> = c
            .iter()
            .filter(|f| f.level != Level::Ok)
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            ["macos", "disk", "session", "filevault", "agent-binary"]
        );
        assert!(c.iter().all(|f| f.level == Level::Ok || !f.fix.is_empty()));
        assert!(check(&Facts::default(), Want::default()).is_empty());
    }

    #[test]
    fn the_plan_skips_what_is_already_true_and_never_resends_the_token() {
        let u = UpSpec {
            install: install(),
            enroll: Some(enroll()),
            have_token: true,
            k3s: None,
        };
        let names = |f: &Facts| plan_up(f, &u).iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(&Facts::default()),
            ["install", "daemon", "join-token", "enroll"]
        );
        assert_eq!(
            names(&facts_ok()),
            ["install", "daemon", "join-token", "enroll"]
        );
        let mut enrolled = facts_ok();
        enrolled.enrolled = true;
        assert_eq!(names(&enrolled), ["install", "daemon", "enroll"]);
        enrolled.agent_loaded = true;
        assert_eq!(names(&enrolled), ["install", "daemon"]);
    }
}
