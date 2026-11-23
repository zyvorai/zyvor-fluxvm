// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl completion bash|zsh|fish` prints a completion script; `fluxctl completion install` puts it where the shell
//! looks, and says exactly which file it wrote. It is also offered once, on the first interactive run.
//!
//! Rules: it writes one file of its own (never an existing file of yours, never a shell startup file unless you pass
//! `--edit-rc`, and then only to add one line it shows you first). Anything else it needs from you, it prints as a line to
//! copy. The offer is made once; the answer is remembered in `cli.completion_offered` of the config file.

use crate::cli_errors::coded;
use anyhow::{Context, Result};
use clap::CommandFactory;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug, Clone)]
pub enum CompletionCommand {
    /// Print the bash completion script.
    Bash,
    /// Print the zsh completion script.
    Zsh,
    /// Print the fish completion script.
    Fish,
    /// Install completion for your shell (detected from $SHELL, or `--shell`). Writes one file and tells you which.
    Install(InstallArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct InstallArgs {
    /// bash, zsh or fish (default: from $SHELL).
    #[arg(long)]
    pub shell: Option<String>,
    /// Show what would be written and where, and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// zsh only: also add the one `fpath` line to ~/.zshrc if it is not there (the line is shown first).
    #[arg(long)]
    pub edit_rc: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sh {
    Bash,
    Zsh,
    Fish,
}

impl Sh {
    pub fn parse(s: &str) -> Option<Sh> {
        match Path::new(s.trim()).file_name()?.to_str()? {
            "bash" => Some(Sh::Bash),
            "zsh" => Some(Sh::Zsh),
            "fish" => Some(Sh::Fish),
            _ => None,
        }
    }
    fn shell(self) -> clap_complete::Shell {
        match self {
            Sh::Bash => clap_complete::Shell::Bash,
            Sh::Zsh => clap_complete::Shell::Zsh,
            Sh::Fish => clap_complete::Shell::Fish,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Sh::Bash => "bash",
            Sh::Zsh => "zsh",
            Sh::Fish => "fish",
        }
    }
}

pub fn script(sh: Sh) -> Vec<u8> {
    // Building the whole command tree is deep recursion; a debug build overflows the main thread's stack doing it, so it runs
    // on a thread with room.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut buf = Vec::new();
            clap_complete::generate(sh.shell(), &mut crate::Cli::command(), "fluxctl", &mut buf);
            buf
        })
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
}

/// Where the script goes, and what else the shell needs.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub shell: Sh,
    pub file: PathBuf,
    /// A line the shell must read, with the file it belongs in, when installing the script alone is not enough.
    pub rc_line: Option<(PathBuf, String)>,
}

pub fn plan(sh: Sh, home: &Path, xdg_data: Option<&Path>, xdg_config: Option<&Path>) -> Plan {
    match sh {
        Sh::Bash => Plan {
            shell: sh,
            file: xdg_data
                .map_or_else(|| home.join(".local/share"), Path::to_path_buf)
                .join("bash-completion/completions/fluxctl"),
            rc_line: None,
        },
        Sh::Fish => Plan {
            shell: sh,
            file: xdg_config
                .map_or_else(|| home.join(".config"), Path::to_path_buf)
                .join("fish/completions/fluxctl.fish"),
            rc_line: None,
        },
        Sh::Zsh => Plan {
            shell: sh,
            file: home.join(".zfunc/_fluxctl"),
            rc_line: Some((
                home.join(".zshrc"),
                "fpath=(~/.zfunc $fpath); autoload -Uz compinit && compinit".to_owned(),
            )),
        },
    }
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn detect(explicit: Option<&str>) -> Result<Sh> {
    let raw = explicit
        .map(str::to_owned)
        .or_else(|| std::env::var("SHELL").ok());
    raw.as_deref().and_then(Sh::parse).ok_or_else(|| {
        coded(
            "cannot tell which shell to install completion for (bash, zsh and fish are supported)",
            "COMPLETION_SHELL_UNKNOWN",
        )
    })
}

fn current_plan(sh: Sh) -> Result<Plan> {
    let home = env_path("HOME").context("HOME is not set")?;
    Ok(plan(
        sh,
        &home,
        env_path("XDG_DATA_HOME").as_deref(),
        env_path("XDG_CONFIG_HOME").as_deref(),
    ))
}

/// True when the zshrc already mentions the folder.
fn rc_has(rc: &Path) -> bool {
    std::fs::read_to_string(rc).is_ok_and(|t| t.contains(".zfunc"))
}

/// Writes the script; returns the lines to show the user.
pub fn install(p: &Plan, script: &[u8], edit_rc: bool) -> Result<Vec<String>> {
    let mut say = Vec::new();
    if let Some(dir) = p.file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&p.file, script).with_context(|| format!("writing {}", p.file.display()))?;
    say.push(format!("wrote {}", p.file.display()));
    match &p.rc_line {
        Some((rc, line)) if !rc_has(rc) => {
            if edit_rc {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(rc)
                    .with_context(|| format!("opening {}", rc.display()))?;
                writeln!(f, "\n# added by `fluxctl completion install`\n{line}")?;
                say.push(format!("added one line to {}: {line}", rc.display()));
            } else {
                say.push(format!(
                    "zsh also needs this line in {} (not added, because you did not pass --edit-rc):\n  {line}",
                    rc.display()
                ));
            }
        }
        _ => {}
    }
    say.push(match p.shell {
        Sh::Zsh => "open a new terminal (or run `exec zsh`) to use it".to_owned(),
        s => format!("open a new {} terminal to use it", s.name()),
    });
    Ok(say)
}

pub fn run(cmd: &CompletionCommand) -> Result<()> {
    let print = |sh: Sh| -> Result<()> {
        match std::io::stdout().write_all(&script(sh)) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => Ok(other?),
        }
    };
    match cmd {
        CompletionCommand::Bash => print(Sh::Bash),
        CompletionCommand::Zsh => print(Sh::Zsh),
        CompletionCommand::Fish => print(Sh::Fish),
        CompletionCommand::Install(a) => {
            let p = current_plan(detect(a.shell.as_deref())?)?;
            if a.dry_run {
                println!("would write {}", p.file.display());
                if let Some((rc, line)) = &p.rc_line
                    && !rc_has(rc)
                {
                    println!(
                        "zsh would also need this line in {}:\n  {line}",
                        rc.display()
                    );
                }
                return Ok(());
            }
            for l in install(&p, &script(p.shell), a.edit_rc)? {
                println!("{l}");
            }
            Ok(())
        }
    }
}

/// Whether to ask now: a person at a terminal, not asked before, no completion already there, and not opted out.
pub fn should_offer(
    interactive: bool,
    opted_out: bool,
    already_offered: bool,
    installed: bool,
) -> bool {
    interactive && !opted_out && !already_offered && !installed
}

/// The one-time offer on the first interactive run. Never writes without a "y", names the file first, and remembers the
/// answer. Best effort: any problem here is silent, a command must never fail because of it.
pub fn offer_once(config: Option<&Path>) {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let opted_out =
        std::env::var_os("FLUXCTL_NO_PROMPT").is_some() || std::env::var_os("CI").is_some();
    if !interactive || opted_out || crate::user_config::flag(config, "completion_offered") {
        return;
    }
    let Ok(sh) = detect(None) else { return };
    let Ok(p) = current_plan(sh) else { return };
    let Some(cfg) = crate::user_config::resolve(config, env_path("HOME").as_deref()) else {
        return;
    };
    if !should_offer(interactive, opted_out, false, p.file.exists()) {
        return;
    }
    eprintln!(
        "fluxctl can complete commands with Tab in {}.\nInstalling writes one file: {}\n(and writes `cli.completion_offered = true` to {} so you are asked only once)",
        sh.name(),
        p.file.display(),
        cfg.display()
    );
    eprint!("Install it? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return;
    }
    let _ = crate::user_config::set_in(&cfg, "cli.completion_offered", "true");
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        match install(&p, &script(sh), false) {
            Ok(say) => say.iter().for_each(|l| eprintln!("{l}")),
            Err(e) => eprintln!(
                "could not install completion: {e:#}\nTry `fluxctl completion install` later."
            ),
        }
    } else {
        eprintln!("Not installed. `fluxctl completion install` does it any time.");
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_are_detected_from_a_path_or_a_name() {
        assert_eq!(Sh::parse("/bin/zsh"), Some(Sh::Zsh));
        assert_eq!(Sh::parse("fish"), Some(Sh::Fish));
        assert_eq!(Sh::parse("/usr/bin/bash\n"), Some(Sh::Bash));
        assert_eq!(Sh::parse("/bin/tcsh"), None);
        assert!(
            detect(Some("nushell"))
                .unwrap_err()
                .to_string()
                .contains("COMPLETION_SHELL_UNKNOWN")
        );
    }

    #[test]
    fn each_shell_gets_its_own_autoloaded_file() {
        let h = Path::new("/h");
        assert_eq!(
            plan(Sh::Bash, h, None, None).file,
            Path::new("/h/.local/share/bash-completion/completions/fluxctl")
        );
        assert_eq!(
            plan(Sh::Fish, h, None, Some(Path::new("/x"))).file,
            Path::new("/x/fish/completions/fluxctl.fish")
        );
        let z = plan(Sh::Zsh, h, None, None);
        assert_eq!(z.file, Path::new("/h/.zfunc/_fluxctl"));
        assert!(z.rc_line.unwrap().1.contains("fpath"));
    }

    #[test]
    fn the_scripts_are_real_and_name_the_binary() {
        for s in [Sh::Bash, Sh::Zsh, Sh::Fish] {
            let t = String::from_utf8(script(s)).unwrap();
            assert!(t.contains("fluxctl") && t.len() > 500, "{s:?}");
        }
    }

    #[test]
    fn install_writes_only_its_file_and_never_edits_rc_unasked() {
        let d = tempfile::tempdir().unwrap();
        let p = plan(Sh::Zsh, d.path(), None, None);
        let rc = d.path().join(".zshrc");
        std::fs::write(&rc, "export A=1\n").unwrap();
        let say = install(&p, b"#compdef fluxctl\n", false).unwrap();
        assert!(say[0].starts_with("wrote ") && say.iter().any(|l| l.contains("--edit-rc")));
        assert_eq!(std::fs::read_to_string(&rc).unwrap(), "export A=1\n");
        assert_eq!(
            std::fs::read_to_string(&p.file).unwrap(),
            "#compdef fluxctl\n"
        );
        // with --edit-rc the one line is added, once
        install(&p, b"x", true).unwrap();
        install(&p, b"x", true).unwrap();
        let t = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(t.matches("fpath=(~/.zfunc").count(), 1);
    }

    #[test]
    fn bash_and_fish_need_no_rc_line() {
        let d = tempfile::tempdir().unwrap();
        let say = install(&plan(Sh::Bash, d.path(), None, None), b"x", false).unwrap();
        assert_eq!(say.len(), 2);
    }

    #[test]
    fn the_offer_is_only_for_a_person_who_has_not_been_asked() {
        assert!(should_offer(true, false, false, false));
        assert!(!should_offer(false, false, false, false));
        assert!(!should_offer(true, true, false, false));
        assert!(!should_offer(true, false, true, false));
        assert!(!should_offer(true, false, false, true));
    }
}
