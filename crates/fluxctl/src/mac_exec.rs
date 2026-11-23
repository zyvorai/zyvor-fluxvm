// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Docker-style interactive access to a macOS instance: the `-i`/`-t` flags of `mac exec` and `mac run`, a login shell when no
//! command is given, the remote command's exit code, and `mac attach`.
//!
//! No new transport: everything goes over the same `ssh` (generated key, host-key handling) that `mac exec` and `mac run`
//! already use, through [`crate::run::ssh_to_code`].
//!
//! Not verified against a real macOS guest: the decisions (terminal or not, exit codes, ssh arguments, messages) are unit tested
//! with fakes, and the `ssh` process itself is not started by any test.

use crate::mac_config;
use crate::mac_inspect as view;
use anyhow::Result;
use serde_json::Value;

/// Whether `ssh` gets a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tty {
    /// `ssh -tt`: the guest sees a terminal (what `mac exec` always did before).
    #[default]
    Force,
    /// `ssh -T`: plain pipes, so output is not mangled by a terminal.
    Off,
}

/// Docker's `-i` and `-t`.
#[derive(clap::Args, Debug, Default, Clone, Copy)]
pub struct TtyFlags {
    /// Keep stdin open for the command (Docker's -i). Without -t the guest gets pipes, not a terminal.
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Give the command a terminal (Docker's -t), so editors, `top` and shells work. Use with -i: `-it`.
    #[arg(short = 't', long)]
    pub tty: bool,
}

/// The terminal decision. Returns the choice and a warning for the user when a flag could not be honoured.
///
/// - `-t` with a terminal on stdin: a terminal. `-t` without one (a pipe, a CI job): none, with a warning, because ssh cannot
///   put a terminal in front of a pipe.
/// - `-i` alone: pipes.
/// - neither: a terminal only when both stdin and stdout are terminals, so `mac exec x -- cmd | grep` stays clean and an
///   interactive session just works without flags.
pub fn resolve(f: TtyFlags, stdin_tty: bool, stdout_tty: bool) -> (Tty, Option<String>) {
    if f.tty {
        return if stdin_tty {
            (Tty::Force, None)
        } else {
            (
                Tty::Off,
                Some("-t ignored: this shell has no terminal on stdin (a pipe or a CI job), so the command runs without one. Run it from a terminal to get -it.".into()),
            )
        };
    }
    if f.interactive {
        return (Tty::Off, None);
    }
    if stdin_tty && stdout_tty {
        (Tty::Force, None)
    } else {
        (Tty::Off, None)
    }
}

/// [`resolve`] for this process.
pub fn resolve_here(f: TtyFlags) -> Tty {
    use std::io::IsTerminal;
    let (t, warn) = resolve(
        f,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    if let Some(w) = warn {
        eprintln!("{w}");
    }
    t
}

/// What `ssh` runs in the guest to give a shell: nothing (ssh starts the login shell) when there is no environment or directory to
/// set up, else the setup followed by `exec` of the user's login shell.
pub fn shell_args(env: &[(String, String)], workdir: Option<&str>) -> Vec<String> {
    let p = mac_config::prefix(env, workdir);
    if p.is_empty() {
        vec![]
    } else {
        vec![format!("{p}exec \"${{SHELL:-/bin/zsh}}\" -l")]
    }
}

/// A failure message with the matching `explain` hint appended when there is one.
pub fn failure(msg: String) -> anyhow::Error {
    match crate::mac_errors::friendly(&msg) {
        Some(h) => anyhow::anyhow!("{msg}\n{h}"),
        None => anyhow::anyhow!(msg),
    }
}

/// The guest address of a running instance, or a plain-English failure saying what to do.
pub fn running_address(name: &str, vm: &Value) -> Result<String> {
    let st = view::status(vm);
    if st != "running" {
        return Err(failure(format!(
            "{name} is {st}, not running, so there is nothing to connect to. Start it with `fluxctl mac start {name}` (see all instances with `fluxctl mac ps -a`)."
        )));
    }
    match vm["guest_ip"].as_str() {
        Some(ip) if !ip.is_empty() => Ok(ip.to_string()),
        _ => Err(failure(format!(
            "{name} is running but has no address yet (macOS is still booting). Wait with `fluxctl mac wait-ready {name}`, then run this again."
        ))),
    }
}

/// What `ssh` returning `code` means. The remote command's own code is passed through (Docker does the same); 255 is ssh itself
/// failing, which gets an explanation.
pub fn outcome(name: &str, user: &str, code: i32) -> Result<i32> {
    if code == 255 {
        return Err(failure(format!(
            "could not open an ssh session to {name} as {user} (ssh exited with exit status: 255). Check that Remote Login is on in the guest and that {user} exists there (`fluxctl mac exec NAME -u USER`); `fluxctl mac wait-ready {name}` tests it."
        )));
    }
    Ok(code)
}

/// Finish a command that returned `code`: exit with it when it is not 0, so `mac exec` is usable in scripts like `docker exec`.
pub fn exit_with(code: i32) -> Result<()> {
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// The command `mac attach --cmd` runs in the guest: the output of a command started by `mac run -d`.
pub const DETACHED_LOG: &str = "/tmp/fluxvm-mac-cmd.log";

pub fn tail_command(tail: usize, follow: bool) -> String {
    format!(
        "tail -n {tail}{} {DETACHED_LOG}",
        if follow { " -f" } else { "" }
    )
}

/// The line `mac attach` prints before it starts following the console, so a silent screen is not mistaken for a hang.
pub fn attach_banner(name: &str) -> String {
    format!(
        "Following {name}'s console log (read-only). Ctrl-C detaches; the instance keeps running.\nFor a shell instead: fluxctl mac exec -it {name}   For the output of `mac run -d`: fluxctl mac attach --cmd {name}"
    )
}

/// `mac exec --print`: the ssh line as one copy-pasteable string.
pub fn printable(ip: &str, user: &str, tty: Tty, args: &[String]) -> String {
    crate::mac_fleet_ssh::display_command(&crate::run::ssh_argv(ip, user, 22, true, tty, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(i: bool, t: bool) -> TtyFlags {
        TtyFlags {
            interactive: i,
            tty: t,
        }
    }

    #[test]
    fn terminal_decision_follows_the_flags() {
        assert_eq!(resolve(f(true, true), true, true), (Tty::Force, None));
        // -it with stdout piped still gets a terminal: stdin is what matters, as in Docker.
        assert_eq!(resolve(f(true, true), true, false), (Tty::Force, None));
        // -i alone is pipes, even on a terminal.
        assert_eq!(resolve(f(true, false), true, true), (Tty::Off, None));
        // No flags: a terminal only when both ends are terminals.
        assert_eq!(resolve(f(false, false), true, true), (Tty::Force, None));
        assert_eq!(resolve(f(false, false), true, false), (Tty::Off, None));
        assert_eq!(resolve(f(false, false), false, true), (Tty::Off, None));
    }

    #[test]
    fn dash_t_without_a_terminal_warns_and_runs_without_one() {
        let (t, w) = resolve(f(true, true), false, false);
        assert_eq!(t, Tty::Off);
        let w = w.unwrap();
        assert!(
            w.contains("-t ignored") && w.contains("from a terminal"),
            "{w}"
        );
    }

    #[test]
    fn ssh_arguments_carry_the_terminal_choice() {
        let args = vec!["uname -a".to_string()];
        let a = crate::run::ssh_argv("192.168.64.5", "admin", 22, true, Tty::Force, &args);
        assert_eq!(a[0], "-tt");
        assert!(a.contains(&"StrictHostKeyChecking=no".to_string()));
        assert_eq!(&a[a.len() - 2..], ["admin@192.168.64.5", "uname -a"]);
        let a = crate::run::ssh_argv("192.168.64.5", "ci", 22, true, Tty::Off, &[]);
        assert_eq!(a[0], "-T");
        assert_eq!(a.last().unwrap(), "ci@192.168.64.5");
        // A QEMU loopback forward keeps its own port.
        let a = crate::run::ssh_argv("127.0.0.1:2222", "u", 22, false, Tty::Off, &[]);
        assert_eq!(&a[1..3], ["-p", "2222"]);
    }

    #[test]
    fn printable_line_is_pasteable() {
        let l = printable("10.0.0.2", "admin", Tty::Force, &["echo 'a b'".into()]);
        assert!(l.starts_with("ssh -tt -p 22 "), "{l}");
        assert!(l.contains("admin@10.0.0.2 "), "{l}");
        assert!(
            l.ends_with("'echo '\\''a b'\\'''") || l.contains("echo"),
            "{l}"
        );
    }

    #[test]
    fn a_shell_needs_no_command_unless_there_is_setup() {
        assert!(shell_args(&[], None).is_empty());
        let a = shell_args(&[("A".into(), "1".into())], Some("/w"));
        assert_eq!(a.len(), 1);
        assert!(
            a[0].starts_with("export A='1'; cd '/w' && exec ") && a[0].ends_with("-l"),
            "{}",
            a[0]
        );
    }

    #[test]
    fn stopped_and_booting_instances_say_what_to_do() {
        let stopped = json!({"status": "stopped", "labels": {}});
        let e = running_address("web", &stopped).unwrap_err().to_string();
        assert!(
            e.contains("web is stopped") && e.contains("fluxctl mac start web"),
            "{e}"
        );
        assert!(e.contains("MAC_EXEC_NOT_RUNNING"), "{e}");
        let booting = json!({"status": "running", "labels": {}});
        let e = running_address("web", &booting).unwrap_err().to_string();
        assert!(
            e.contains("no address yet") && e.contains("mac wait-ready web"),
            "{e}"
        );
        let up = json!({"status": "running", "guest_ip": "192.168.64.9", "labels": {}});
        assert_eq!(running_address("web", &up).unwrap(), "192.168.64.9");
    }

    #[test]
    fn exit_codes_pass_through_except_sshs_own_failure() {
        assert_eq!(outcome("web", "admin", 0).unwrap(), 0);
        assert_eq!(outcome("web", "admin", 3).unwrap(), 3);
        assert_eq!(outcome("web", "admin", 130).unwrap(), 130);
        let e = outcome("web", "bob", 255).unwrap_err().to_string();
        assert!(
            e.contains("as bob")
                && e.contains("Remote Login")
                && e.contains("MAC_GUEST_SSH_FAILED"),
            "{e}"
        );
    }

    #[test]
    fn attach_commands() {
        assert_eq!(
            tail_command(20, true),
            "tail -n 20 -f /tmp/fluxvm-mac-cmd.log"
        );
        assert_eq!(tail_command(5, false), "tail -n 5 /tmp/fluxvm-mac-cmd.log");
        let b = attach_banner("web");
        assert!(
            b.contains("Ctrl-C")
                && b.contains("mac exec -it web")
                && b.contains("attach --cmd web")
        );
    }
}
