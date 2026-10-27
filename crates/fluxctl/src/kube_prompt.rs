//! `fluxctl kube prompt`: one short line about the cluster for a shell prompt, such as `⎈ dev 12/12`, `⎈ dev 10/12 ✗2`
//! or `⎈ dev 💤`. It reads a small file that `kube supervise` keeps up to date, so it answers in a millisecond, never wakes
//! a sleeping cluster and never talks to the VM. With no supervisor, or a file that has gone stale, it prints nothing, so a
//! prompt never shows a state that is no longer true.

use crate::kube_doctor::PodRow;
use crate::kube_ps::pod_summary;
use anyhow::{Result, bail};
use std::time::{SystemTime, UNIX_EPOCH};

/// The file is older than this when the supervisor has not touched it for a few of its checks.
pub const STALE_SECS: u64 = 150;

#[derive(Debug, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub ready: usize,
    pub total: usize,
    pub bad: usize,
    /// Seconds since the epoch.
    pub at: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    Running,
    Asleep,
    Down,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What the supervisor writes: `running 10 12 2 1700000000`.
pub fn encode(state: State, rows: &[PodRow], at: u64) -> String {
    let (ok, bad) = pod_summary(rows);
    let word = match state {
        State::Running => "running",
        State::Asleep => "asleep",
        State::Down => "down",
    };
    format!("{word} {ok} {} {} {at}\n", rows.len(), bad.len())
}

pub fn parse(text: &str) -> Option<Status> {
    let mut f = text.split_whitespace();
    let state = match f.next()? {
        "running" => State::Running,
        "asleep" => State::Asleep,
        "down" => State::Down,
        _ => return None,
    };
    let mut n = || f.next()?.parse::<u64>().ok();
    let (ready, total, bad, at) = (n()? as usize, n()? as usize, n()? as usize, n()?);
    Some(Status {
        state,
        ready,
        total,
        bad,
        at,
    })
}

/// The prompt text, or None when it should be empty (nothing recorded, or too old to trust).
pub fn render(name: &str, status: Option<&Status>, now: u64, plain: bool) -> Option<String> {
    let s = status.filter(|s| now.saturating_sub(s.at) <= STALE_SECS)?;
    let (mark, asleep, fail) = if plain {
        ("k8s", "asleep", "!")
    } else {
        ("⎈", "💤", "✗")
    };
    Some(match s.state {
        State::Asleep => format!("{mark} {name} {asleep}"),
        State::Down => format!("{mark} {name} down"),
        State::Running if s.bad > 0 => {
            format!("{mark} {name} {}/{} {fail}{}", s.ready, s.total, s.bad)
        }
        State::Running => format!("{mark} {name} {}/{}", s.ready, s.total),
    })
}

/// Setup lines for a shell, printed by `kube prompt --snippet SHELL`.
pub fn snippet(shell: &str, bin: &str, cluster: &str) -> Result<String> {
    let cmd = format!("{bin} kube prompt --name {cluster}");
    Ok(match shell {
        "starship" => format!(
            "# ~/.config/starship.toml\n[custom.fluxvm]\ncommand = \"{cmd}\"\nwhen = true\nshell = [\"sh\"]\nformat = \"[$output]($style) \"\nstyle = \"cyan\"\n"
        ),
        "zsh" => format!(
            "# ~/.zshrc\nsetopt PROMPT_SUBST\nfluxvm_prompt() {{ {cmd} 2>/dev/null; }}\nRPROMPT='$(fluxvm_prompt)'\n"
        ),
        "bash" => format!(
            "# ~/.bashrc\nfluxvm_prompt() {{ {cmd} 2>/dev/null; }}\nPS1='$(fluxvm_prompt) '\"$PS1\"\n"
        ),
        "fish" => format!(
            "# ~/.config/fish/config.fish\nfunction fish_right_prompt\n    {cmd} 2>/dev/null\nend\n"
        ),
        other => bail!("no snippet for {other}; use starship, zsh, bash or fish"),
    })
}

/// Reads the supervisor's file for this cluster and renders it.
pub fn run(name: &str, plain: bool) -> Option<String> {
    let text = std::fs::read_to_string(crate::kube_host::prompt_file(name)).ok()?;
    render(name, parse(&text).as_ref(), now(), plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: &str, restarts: u32) -> PodRow {
        PodRow {
            namespace: "default".into(),
            name: "p".into(),
            status: status.into(),
            restarts,
        }
    }

    #[test]
    fn the_supervisor_writes_what_the_prompt_reads() {
        let rows = vec![row("Running", 0), row("Running", 0), row("CrashLoopBackOff", 7)];
        let text = encode(State::Running, &rows, 1000);
        assert_eq!(text, "running 2 3 1 1000\n");
        assert_eq!(
            parse(&text),
            Some(Status {
                state: State::Running,
                ready: 2,
                total: 3,
                bad: 1,
                at: 1000
            })
        );
        assert_eq!(parse("sleeping 1 1 0 5"), None);
        assert_eq!(parse("running 1 1"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn the_line_shows_health_sleep_and_failures() {
        let s = |state, ready, total, bad| Status {
            state,
            ready,
            total,
            bad,
            at: 100,
        };
        let line = |st: &Status, plain| render("dev", Some(st), 110, plain).unwrap();
        assert_eq!(line(&s(State::Running, 12, 12, 0), false), "⎈ dev 12/12");
        assert_eq!(line(&s(State::Running, 10, 12, 2), false), "⎈ dev 10/12 ✗2");
        assert_eq!(line(&s(State::Asleep, 0, 0, 0), false), "⎈ dev 💤");
        assert_eq!(line(&s(State::Down, 0, 0, 0), false), "⎈ dev down");
        assert_eq!(line(&s(State::Asleep, 0, 0, 0), true), "k8s dev asleep");
        assert_eq!(line(&s(State::Running, 10, 12, 2), true), "k8s dev 10/12 !2");
    }

    #[test]
    fn nothing_is_shown_for_a_missing_or_stale_status() {
        let st = Status {
            state: State::Running,
            ready: 1,
            total: 1,
            bad: 0,
            at: 100,
        };
        assert_eq!(render("dev", None, 110, false), None);
        assert!(render("dev", Some(&st), 100 + STALE_SECS, false).is_some());
        assert_eq!(render("dev", Some(&st), 100 + STALE_SECS + 1, false), None);
        // a clock that went backwards is not stale
        assert!(render("dev", Some(&st), 50, false).is_some());
    }

    #[test]
    fn snippets_exist_for_the_common_shells_and_reject_others() {
        for sh in ["starship", "zsh", "bash", "fish"] {
            let text = snippet(sh, "fluxctl", "dev").unwrap();
            assert!(text.contains("fluxctl kube prompt --name dev"), "{sh}");
        }
        assert!(snippet("powershell", "fluxctl", "dev").is_err());
    }
}
