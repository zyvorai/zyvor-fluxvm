// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The easy way in for `fluxctl mac`: `quickstart` (alias `setup`) walks a new Mac from nothing to a running macOS instance
//! and, if asked, a k3s cluster; `cheatsheet` prints the commands you will want; `complete` and `complete-script` give the
//! shell dynamic completion of image names, instance names and inventory hosts.
//!
//! Everything goes through the same `fluxctl` binary (`mac doctor`, `mac import`, `mac pull`, `mac run`, `kube up`), so this
//! module only decides the order, asks nothing off a terminal, and explains failures with `mac_errors`.

use crate::mac_errors;
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum MacUx {
    /// Guided setup: check this Mac (`mac doctor`, with a fix for each problem), get a macOS template (import a directory or
    /// pull from a registry), run a hello-world instance, and optionally start k3s. Safe to repeat: finished steps are skipped.
    /// Never prompts unless it runs on a terminal; `--yes` takes every default. Alias: setup.
    #[command(
        alias = "setup",
        after_help = "Examples:\n  fluxctl mac quickstart\n  fluxctl mac quickstart --yes --pull registry.example/macos-base:15\n  fluxctl mac quickstart --yes --from ~/templates/sequoia --template macos-base:15 --k3s\n  fluxctl mac quickstart --dry-run --yes --pull registry.example/macos-base:15"
    )]
    Quickstart(QuickstartArgs),
    /// The commands you will want, grouped: images, instances, fleet, k3s, and what to do when something fails.
    Cheatsheet {
        #[arg(long)]
        json: bool,
    },
    /// Completion candidates for the shell (used by `complete-script`): the words typed so far, after `--`. Prints one candidate
    /// per line, or nothing when the position has none, so the shell falls back to its static completion.
    #[command(hide = true)]
    Complete {
        #[arg(last = true)]
        words: Vec<String>,
    },
    /// A shell snippet that adds image, instance and host name completion on top of `fluxctl completions SHELL`:
    /// `source <(fluxctl completions zsh); source <(fluxctl mac complete-script zsh)`.
    #[command(name = "complete-script")]
    CompleteScript {
        #[arg(value_enum)]
        shell: ScriptShell,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptShell {
    Bash,
    Zsh,
}

#[derive(Args, Clone, Debug, Default)]
pub struct QuickstartArgs {
    /// Name of the template to use or create (`name:tag`). Default: what you pull, else `macos-base:latest`.
    #[arg(long, value_name = "NAME:TAG")]
    pub template: Option<String>,
    /// Import this template directory (disk.raw, hardware.bin, auxiliary.bin) when the template is not in the store yet.
    #[arg(long, value_name = "DIR", conflicts_with = "pull")]
    pub from: Option<PathBuf>,
    /// Pull this registry reference when the template is not in the store yet.
    #[arg(long, value_name = "REF")]
    pub pull: Option<String>,
    /// Also create a k3s cluster in a Linux VM (`kube up`). Asked on a terminal when not given.
    #[arg(long)]
    pub k3s: bool,
    /// Skip the hello-world instance.
    #[arg(long)]
    pub no_hello: bool,
    /// Carry on when `mac doctor` reports a failure.
    #[arg(long)]
    pub ignore_doctor: bool,
    /// Take every default and never prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print the steps and run nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(cmd: MacUx) -> Result<()> {
    match cmd {
        MacUx::Quickstart(a) => {
            let mut env = RealEnv::new()?;
            let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            tokio::task::block_in_place(|| quickstart(&a, &mut env, tty))
        }
        MacUx::Cheatsheet { json } => {
            print!("{}", cheatsheet(json));
            Ok(())
        }
        MacUx::Complete { words } => {
            for c in candidates(&words, &RealSource) {
                println!("{c}");
            }
            Ok(())
        }
        MacUx::CompleteScript { shell } => {
            print!("{}", complete_script(shell));
            Ok(())
        }
    }
}

// ---- quickstart -------------------------------------------------------------------------------------------------------

/// What the flow needs from the outside, so tests can fake it.
pub trait Env {
    /// Run `fluxctl ARGS`. `stream`: show its output live (long steps); otherwise it is captured. Returns the exit code and
    /// the captured output (empty when streamed).
    fn fluxctl(&mut self, args: &[String], stream: bool) -> Result<(i32, String)>;
    /// Is `reference` (`name:tag`) in the local image store?
    fn has_image(&self, reference: &str) -> bool;
    /// Ask a question; `None` = no answer given (use the default).
    fn ask(&mut self, question: &str) -> Option<String>;
    fn say(&mut self, line: &str);
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn yes_no(env: &mut dyn Env, q: &str, default_yes: bool) -> bool {
    let d = if default_yes { "Y/n" } else { "y/N" };
    match env.ask(&format!("{q} [{d}]")) {
        Some(a) if !a.trim().is_empty() => {
            matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }
        _ => default_yes,
    }
}

/// The template reference: `--template`, else what is pulled, else the default.
pub fn template_ref(a: &QuickstartArgs) -> String {
    a.template
        .clone()
        .or_else(|| a.pull.clone())
        .unwrap_or_else(|| "macos-base:latest".into())
}

pub fn quickstart(a: &QuickstartArgs, env: &mut dyn Env, tty: bool) -> Result<()> {
    let interactive = tty && !a.yes && !a.dry_run;
    let mut tmpl = template_ref(a);

    env.say("== 1/4  Check this Mac");
    if a.dry_run {
        env.say("   would run: fluxctl mac doctor");
    } else {
        let (code, out) = env.fluxctl(&s(&["mac", "doctor"]), false)?;
        env.say(out.trim_end());
        if code != 0 && !a.ignore_doctor {
            let go =
                interactive && yes_no(env, "mac doctor found problems. Continue anyway?", false);
            if !go {
                bail!(
                    "mac doctor found problems: apply the `fix:` lines above and run `fluxctl mac quickstart` again (or pass --ignore-doctor)"
                );
            }
        }
    }

    env.say("== 2/4  A macOS template");
    if !a.dry_run && env.has_image(&tmpl) {
        env.say(&format!(
            "   {tmpl} is already in the local store, nothing to do"
        ));
    } else {
        let (mut from, mut pull) = (a.from.clone(), a.pull.clone());
        if from.is_none() && pull.is_none() && interactive {
            let q =
                "Directory of a template to import, or registry reference to pull (empty to stop)";
            if let Some(ans) = env
                .ask(q)
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
            {
                if Path::new(&ans).is_dir() {
                    from = Some(PathBuf::from(ans));
                } else {
                    if a.template.is_none() {
                        tmpl = ans.clone();
                    }
                    pull = Some(ans);
                }
            }
        }
        let step = match (&from, &pull) {
            (Some(d), _) => s(&["mac", "import", &d.to_string_lossy(), &tmpl]),
            (_, Some(p)) => s(&["mac", "pull", p]),
            _ if a.dry_run => {
                env.say("   no template source given: it would need --from DIR or --pull REF");
                s(&["mac", "images"])
            }
            _ => bail!(
                "{tmpl} is not in the local store and no source was given: pass --from DIR (a template directory) or --pull REGISTRY/REPO:TAG\n{}",
                mac_errors::friendly("no such image").unwrap_or_default()
            ),
        };
        env.say(&format!("   fluxctl {}", step.join(" ")));
        if !a.dry_run {
            let (code, out) = env.fluxctl(&step, true)?;
            if code != 0 {
                bail!(
                    "fluxctl {} failed (exit {code})\n{}",
                    step[..2].join(" "),
                    mac_errors::friendly(&out)
                        .unwrap_or_else(|| "see `fluxctl explain MAC_TEMPLATE_MISSING`".into())
                );
            }
        }
    }

    env.say("== 3/4  Hello from a macOS instance");
    if a.no_hello {
        env.say("   skipped (--no-hello)");
    } else {
        let step = s(&[
            "mac",
            "run",
            "--rm",
            "--name",
            "quickstart-hello",
            &tmpl,
            "--",
            "sw_vers",
        ]);
        env.say(&format!(
            "   fluxctl {}   (boots a VM: allow a minute or two)",
            step.join(" ")
        ));
        if !a.dry_run {
            let (code, out) = env.fluxctl(&step, false)?;
            env.say(out.trim_end());
            if code != 0 {
                bail!(
                    "the hello-world instance failed (exit {code})\n{}",
                    mac_errors::friendly(&out).unwrap_or_else(|| {
                        "see `fluxctl mac doctor` and `fluxctl mac cheatsheet`".into()
                    })
                );
            }
            env.say("   hello from FluxVM: the guest answered over SSH");
        }
    }

    env.say("== 4/4  k3s (optional)");
    if a.k3s || (interactive && yes_no(env, "Also start a k3s cluster on this Mac?", false)) {
        env.say("   fluxctl kube up   (the first run downloads k3s and takes a few minutes)");
        if !a.dry_run {
            let (code, out) = env.fluxctl(&s(&["kube", "up"]), true)?;
            if code != 0 {
                bail!(
                    "kube up failed (exit {code})\n{}",
                    mac_errors::friendly(&out).unwrap_or_else(|| {
                        "see `fluxctl kube status` and `fluxctl explain --list`".into()
                    })
                );
            }
        }
    } else {
        env.say("   skipped (add --k3s, or run `fluxctl kube up` later)");
    }

    env.say("");
    env.say("Done. Next:");
    env.say(&format!(
        "  fluxctl mac run -d --name dev {tmpl}      start an instance that keeps running"
    ));
    env.say("  fluxctl mac ps                           see it");
    env.say("  fluxctl mac exec dev -- uname -a         run something in it");
    env.say("  fluxctl mac cheatsheet                   everything else");
    Ok(())
}

struct RealEnv {
    exe: PathBuf,
    store: crate::mac_store::Store,
}

impl RealEnv {
    fn new() -> Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            store: crate::mac_store::Store::open(crate::mac_store::default_root()?)?,
        })
    }
}

impl Env for RealEnv {
    fn fluxctl(&mut self, args: &[String], stream: bool) -> Result<(i32, String)> {
        let mut c = std::process::Command::new(&self.exe);
        c.args(args);
        if stream {
            return Ok((c.status()?.code().unwrap_or(1), String::new()));
        }
        let o = c.output()?;
        let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&o.stderr));
        Ok((o.status.code().unwrap_or(1), text))
    }
    fn has_image(&self, reference: &str) -> bool {
        let Ok(want) = crate::mac_store::normalize(reference) else {
            return false;
        };
        self.store
            .list()
            .map(|l| l.iter().any(|(n, _)| *n == want))
            .unwrap_or(false)
    }
    fn ask(&mut self, question: &str) -> Option<String> {
        print!("{question}: ");
        std::io::stdout().flush().ok()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok()?;
        Some(line.trim().to_string())
    }
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

// ---- cheatsheet -------------------------------------------------------------------------------------------------------

/// (group, [(command, what it does)]). A test parses every command with the real CLI, so this cannot drift.
pub const SHEET: &[(&str, &[(&str, &str)])] = &[
    (
        "First time",
        &[
            (
                "fluxctl mac quickstart",
                "guided: doctor, template, hello world, optional k3s",
            ),
            (
                "fluxctl mac doctor",
                "what is wrong with this Mac, and the fix",
            ),
        ],
    ),
    (
        "Images (templates)",
        &[
            ("fluxctl mac images", "list local templates"),
            ("fluxctl mac pull REGISTRY/REPO:TAG", "download one"),
            (
                "fluxctl mac import DIR NAME:TAG",
                "add a template directory (instant clone)",
            ),
            ("fluxctl mac rmi NAME:TAG", "remove one"),
        ],
    ),
    (
        "Instances (2 macOS VMs per Mac at most)",
        &[
            (
                "fluxctl mac run --rm NAME:TAG -- sw_vers",
                "run one command in a fresh VM, then delete it",
            ),
            (
                "fluxctl mac run -d --name dev NAME:TAG",
                "start one that keeps running",
            ),
            ("fluxctl mac ps -a", "list instances"),
            ("fluxctl mac exec dev -- uname -a", "run a command in it"),
            ("fluxctl mac rm dev", "stop and delete it"),
        ],
    ),
    (
        "Fleet of Macs over SSH",
        &[
            (
                "fluxctl mac-fleet doctor -f hosts.yaml",
                "preflight every Mac",
            ),
            (
                "fluxctl mac-fleet apply hosts.yaml",
                "install and converge to the inventory",
            ),
            (
                "fluxctl mac-fleet status -f hosts.yaml",
                "one table for the fleet",
            ),
            (
                "fluxctl mac-fleet templates distribute -f hosts.yaml --template REG/REPO:TAG@sha256:HEX",
                "put a template on every Mac, digest-verified",
            ),
            (
                "fluxctl mac-fleet run -f hosts.yaml --template NAME:TAG -- sw_vers",
                "run on whichever Mac has a free slot",
            ),
        ],
    ),
    (
        "k3s on a Mac",
        &[
            ("fluxctl kube up", "create or start the cluster"),
            (
                "fluxctl kube status",
                "is it up, and where is the kubeconfig",
            ),
        ],
    ),
    (
        "When something fails",
        &[
            (
                "fluxctl explain MAC_SLOT_LIMIT",
                "409: both macOS slots are taken",
            ),
            (
                "fluxctl explain MAC_SESSION_LOCKED",
                "503: the Mac's screen is locked",
            ),
            (
                "fluxctl explain SSH_HOSTKEY_MISMATCH",
                "ssh host key refused",
            ),
            (
                "fluxctl explain MAC_TEMPLATE_MISSING",
                "image not in the store",
            ),
            (
                "fluxctl explain MAC_DISK_FULL",
                "no room for the image or clone",
            ),
        ],
    ),
];

pub fn cheatsheet(json: bool) -> String {
    if json {
        let v: Vec<_> = SHEET
            .iter()
            .map(|(g, rows)| {
                json!({"group": g, "commands": rows.iter().map(|(c, d)| json!({"command": c, "does": d})).collect::<Vec<_>>()})
            })
            .collect();
        return serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
    }
    let width = SHEET
        .iter()
        .flat_map(|(_, r)| r.iter())
        .map(|(c, _)| c.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (g, rows) in SHEET {
        out.push_str(&format!("{g}\n"));
        for (c, d) in *rows {
            out.push_str(&format!("  {c:<width$}  {d}\n"));
        }
        out.push('\n');
    }
    out.push_str("Shell completion for image, instance and host names:\n  source <(fluxctl completions zsh); source <(fluxctl mac complete-script zsh)\n");
    out
}

// ---- dynamic completion -----------------------------------------------------------------------------------------------

/// Where candidates come from.
pub trait Source {
    fn templates(&self) -> Vec<String>;
    fn instances(&self) -> Vec<String>;
    fn hosts(&self, inventory: Option<&Path>) -> Vec<String>;
}

struct RealSource;

impl Source for RealSource {
    fn templates(&self) -> Vec<String> {
        crate::mac_store::default_root()
            .and_then(crate::mac_store::Store::open)
            .and_then(|s| s.list())
            .map(|l| l.into_iter().map(|(n, _)| n).collect())
            .unwrap_or_default()
    }
    fn instances(&self) -> Vec<String> {
        let Ok(exe) = std::env::current_exe() else {
            return vec![];
        };
        std::process::Command::new(exe)
            .args(["mac", "ps", "-a", "-q"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn hosts(&self, inventory: Option<&Path>) -> Vec<String> {
        let path = inventory
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("FLUXVM_MAC_INVENTORY").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("inventory"));
        crate::mac_fleet_inv::Inventory::load(&crate::mac_fleet_inv::expand_home(&path))
            .and_then(|i| i.targets())
            .map(|t| t.into_iter().map(|t| t.name).collect())
            .unwrap_or_default()
    }
}

/// `mac` subcommands whose positional is an image name (`run` takes it after its flags; any non-flag word is offered).
const IMAGE_CMDS: &[&str] = &[
    "run", "rmi", "tag", "push", "pull", "verify", "sign", "label", "sbom",
];
const INSTANCE_CMDS: &[&str] = &[
    "rm",
    "exec",
    "wait-ready",
    "inspect",
    "logs",
    "stop",
    "start",
    "restart",
];
const FLEET_HOST_CMDS: &[&str] = &[
    "up", "add", "status", "exec", "upgrade", "remove", "doctor", "drain", "resume", "run",
];

/// Candidates for the word being completed. `words` is the whole command line so far, program name first, the word under the
/// cursor (possibly empty) last. Empty when this position has no dynamic candidates.
pub fn candidates(words: &[String], src: &dyn Source) -> Vec<String> {
    let Some(cur) = words.last() else {
        return vec![];
    };
    if cur.starts_with('-') || words.len() < 4 {
        return vec![];
    }
    // After a flag the next word is probably its value, which we cannot know.
    let prev = &words[words.len() - 2];
    if prev.starts_with('-') && !prev.contains('=') {
        return vec![];
    }
    let Some(at) = words.iter().position(|w| w == "mac" || w == "mac-fleet") else {
        return vec![];
    };
    let Some(sub_at) = words[at + 1..]
        .iter()
        .position(|w| !w.starts_with('-'))
        .map(|i| at + 1 + i)
    else {
        return vec![];
    };
    // The subcommand itself is the word under the cursor: nothing dynamic.
    if sub_at == words.len() - 1 {
        return vec![];
    }
    let sub = words[sub_at].as_str();
    let mut out = if words[at] == "mac" {
        if IMAGE_CMDS.contains(&sub) {
            src.templates()
        } else if INSTANCE_CMDS.contains(&sub) {
            src.instances()
        } else {
            vec![]
        }
    } else if FLEET_HOST_CMDS.contains(&sub) {
        let inv = words
            .iter()
            .position(|w| w == "-f" || w == "--inventory")
            .and_then(|i| words.get(i + 1))
            .map(PathBuf::from);
        src.hosts(inv.as_deref())
    } else {
        vec![]
    };
    out.retain(|c| c.starts_with(cur.as_str()));
    out.sort();
    out.dedup();
    out
}

pub fn complete_script(shell: ScriptShell) -> String {
    match shell {
        ScriptShell::Zsh => {
            r#"# fluxctl: image, instance and host names (source after `fluxctl completions zsh`)
_fluxctl_dynamic() {
  local -a c
  c=("${(@f)$(fluxctl mac complete -- "${words[@]}" 2>/dev/null)}")
  if [[ -n "${c[1]}" ]]; then
    compadd -- "${c[@]}"
  else
    _fluxctl "$@"
  fi
}
compdef _fluxctl_dynamic fluxctl
"#
            .into()
        }
        ScriptShell::Bash => {
            r#"# fluxctl: image, instance and host names (source after `fluxctl completions bash`)
_fluxctl_dynamic() {
  local out
  out=$(fluxctl mac complete -- "${COMP_WORDS[@]:0:$((COMP_CWORD+1))}" 2>/dev/null)
  if [[ -n "$out" ]]; then
    local IFS=$'\n'
    COMPREPLY=($out)
  else
    _fluxctl "$@"
  fi
}
complete -o nosort -o bashdefault -o default -F _fluxctl_dynamic fluxctl
"#
            .into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Default)]
    struct Fake {
        calls: Vec<Vec<String>>,
        said: Vec<String>,
        images: Vec<String>,
        answers: Vec<String>,
        asked: usize,
        fail_on: Option<(&'static str, i32, &'static str)>,
    }
    impl Env for Fake {
        fn fluxctl(&mut self, args: &[String], _stream: bool) -> Result<(i32, String)> {
            self.calls.push(args.to_vec());
            if let Some((needle, code, out)) = self.fail_on
                && args.join(" ").contains(needle)
            {
                return Ok((code, out.into()));
            }
            Ok((0, "ok".into()))
        }
        fn has_image(&self, r: &str) -> bool {
            self.images.iter().any(|i| i == r)
        }
        fn ask(&mut self, _q: &str) -> Option<String> {
            self.asked += 1;
            if self.answers.is_empty() {
                None
            } else {
                Some(self.answers.remove(0))
            }
        }
        fn say(&mut self, l: &str) {
            self.said.push(l.into());
        }
    }
    fn cmds(f: &Fake) -> Vec<String> {
        f.calls.iter().map(|c| c.join(" ")).collect()
    }
    fn args(pull: Option<&str>) -> QuickstartArgs {
        QuickstartArgs {
            pull: pull.map(Into::into),
            yes: true,
            ..Default::default()
        }
    }

    #[test]
    fn pulls_then_runs_hello_without_prompting() {
        let mut f = Fake::default();
        quickstart(&args(Some("reg.example/macos:15")), &mut f, true).unwrap();
        assert_eq!(
            cmds(&f),
            [
                "mac doctor",
                "mac pull reg.example/macos:15",
                "mac run --rm --name quickstart-hello reg.example/macos:15 -- sw_vers"
            ]
        );
        assert_eq!(f.asked, 0, "--yes never prompts");
    }

    #[test]
    fn existing_template_is_not_fetched_and_k3s_is_opt_in() {
        let mut f = Fake {
            images: vec!["macos-base:latest".into()],
            ..Default::default()
        };
        let mut a = args(None);
        a.k3s = true;
        quickstart(&a, &mut f, false).unwrap();
        assert_eq!(cmds(&f).len(), 3);
        assert_eq!(cmds(&f)[2], "kube up");
        assert!(
            cmds(&f)
                .iter()
                .all(|c| !c.contains("pull") && !c.contains("import"))
        );
        let mut f = Fake {
            images: vec!["macos-base:latest".into()],
            ..Default::default()
        };
        quickstart(&args(None), &mut f, false).unwrap();
        assert!(!cmds(&f).contains(&"kube up".to_string()));
    }

    #[test]
    fn imports_a_directory_under_the_given_name() {
        let mut f = Fake::default();
        let mut a = args(None);
        a.from = Some("/t/seq".into());
        a.template = Some("macos-base:15".into());
        a.no_hello = true;
        quickstart(&a, &mut f, false).unwrap();
        assert_eq!(cmds(&f), ["mac doctor", "mac import /t/seq macos-base:15"]);
    }

    #[test]
    fn no_source_off_a_terminal_is_a_clear_error_with_the_fix() {
        let mut f = Fake::default();
        let e = quickstart(&args(None), &mut f, false)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--from DIR") && e.contains("--pull") && e.contains("MAC_TEMPLATE_MISSING"),
            "{e}"
        );
        assert_eq!(f.asked, 0);
    }

    #[test]
    fn failing_doctor_stops_unless_ignored_or_confirmed() {
        let have = vec!["macos-base:latest".to_string()];
        let mut f = Fake {
            fail_on: Some(("mac doctor", 1, "[FAIL] x")),
            images: have.clone(),
            ..Default::default()
        };
        assert!(
            quickstart(&args(None), &mut f, true)
                .unwrap_err()
                .to_string()
                .contains("fix:")
        );
        assert_eq!(cmds(&f), ["mac doctor"], "stopped before touching anything");

        let mut f = Fake {
            fail_on: Some(("mac doctor", 1, "x")),
            images: have.clone(),
            ..Default::default()
        };
        let mut a = args(None);
        a.ignore_doctor = true;
        quickstart(&a, &mut f, false).unwrap();

        // on a terminal without --yes: answer y to "continue anyway", n to k3s
        let mut f = Fake {
            fail_on: Some(("mac doctor", 1, "x")),
            images: have,
            answers: vec!["y".into(), "n".into()],
            ..Default::default()
        };
        let mut a = args(None);
        a.yes = false;
        quickstart(&a, &mut f, true).unwrap();
        assert_eq!(f.asked, 2);
    }

    #[test]
    fn prompts_for_a_source_on_a_terminal_and_pulls_a_reference() {
        let mut f = Fake {
            answers: vec!["reg.example/m:1".into(), "".into()],
            ..Default::default()
        };
        let mut a = args(None);
        a.yes = false;
        quickstart(&a, &mut f, true).unwrap();
        assert!(cmds(&f).contains(&"mac pull reg.example/m:1".to_string()));
        assert!(
            cmds(&f)
                .iter()
                .any(|c| c.contains("mac run --rm --name quickstart-hello reg.example/m:1"))
        );
    }

    #[test]
    fn a_failed_hello_explains_itself() {
        let mut f = Fake {
            fail_on: Some(("mac run", 1, "Error: 409 conflict: slot limit")),
            images: vec!["macos-base:latest".into()],
            ..Default::default()
        };
        let e = quickstart(&args(None), &mut f, false)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_SLOT_LIMIT") && e.contains("mac ps -a"),
            "{e}"
        );
    }

    #[test]
    fn dry_run_runs_nothing() {
        let mut f = Fake::default();
        let mut a = args(Some("r/m:1"));
        a.dry_run = true;
        quickstart(&a, &mut f, true).unwrap();
        assert!(f.calls.is_empty());
        assert!(f.said.iter().any(|l| l.contains("fluxctl mac pull r/m:1")));
    }

    #[test]
    fn every_cheatsheet_command_parses_with_the_real_cli() {
        for (_, rows) in SHEET {
            for (c, _) in *rows {
                let argv: Vec<&str> = c.split_whitespace().collect();
                crate::Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{c}: {e}"));
            }
        }
        let j: serde_json::Value = serde_json::from_str(&cheatsheet(true)).unwrap();
        assert_eq!(j.as_array().unwrap().len(), SHEET.len());
        assert!(cheatsheet(false).contains("mac quickstart"));
    }

    #[test]
    fn quickstart_and_setup_both_parse() {
        for w in ["quickstart", "setup"] {
            let c = crate::Cli::try_parse_from([
                "fluxctl", "mac", w, "--yes", "--pull", "r/m:1", "--k3s",
            ])
            .unwrap();
            assert!(matches!(
                c.command,
                crate::Command::Mac {
                    command: crate::mac_cmd::MacCommand::Ux(MacUx::Quickstart(_))
                }
            ));
        }
    }

    struct Fs;
    impl Source for Fs {
        fn templates(&self) -> Vec<String> {
            vec!["macos-base:15".into(), "macos-xcode:15".into()]
        }
        fn instances(&self) -> Vec<String> {
            vec!["ci".into(), "dev".into()]
        }
        fn hosts(&self, inv: Option<&Path>) -> Vec<String> {
            assert!(inv.is_none() || inv == Some(Path::new("h.yaml")));
            vec!["mini-1".into(), "mini-2".into()]
        }
    }
    /// A command line; `_` stands for the empty word under the cursor.
    fn w(s: &str) -> Vec<String> {
        s.split(' ').map(|x| x.replace('_', "")).collect()
    }

    #[test]
    fn completes_templates_instances_and_hosts() {
        assert_eq!(
            candidates(&w("fluxctl mac run macos-x"), &Fs),
            ["macos-xcode:15"]
        );
        assert_eq!(candidates(&w("fluxctl mac rmi _"), &Fs).len(), 2);
        assert_eq!(candidates(&w("fluxctl mac rm d"), &Fs), ["dev"]);
        assert_eq!(candidates(&w("fluxctl mac exec _"), &Fs), ["ci", "dev"]);
        assert_eq!(
            candidates(&w("fluxctl mac-fleet status -f h.yaml mini-2"), &Fs),
            ["mini-2"]
        );
    }

    #[test]
    fn leaves_everything_else_to_the_static_completion() {
        for line in [
            "fluxctl mac r",
            "fluxctl mac run --na",
            "fluxctl mac run --name _",
            "fluxctl mac images _",
            "fluxctl vm ls _",
            "fluxctl mac",
        ] {
            assert!(candidates(&w(line), &Fs).is_empty(), "{line}");
        }
    }

    #[test]
    fn scripts_call_the_complete_command() {
        for sh in [ScriptShell::Bash, ScriptShell::Zsh] {
            let t = complete_script(sh);
            assert!(t.contains("fluxctl mac complete --") && t.contains("_fluxctl_dynamic"));
        }
    }
}
