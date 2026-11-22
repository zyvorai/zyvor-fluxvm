// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Whether `kube cluster create` installs the Envoy gateway. The decision is a pure function of the flags and of whether
//! a person is at a terminal; the question itself is asked only when the answer is `Ask`. Scripts keep their behaviour:
//! with no terminal and no flag the cluster is created exactly as before (no gateway).

use super::platform::{ADDON_PROFILES, ENVOY_GATEWAY_PROFILE};
use anyhow::{Result, bail};
use std::io::{BufRead, Write};

/// The question asked on a terminal.
pub const QUESTION: &str = "Install the Envoy gateway so you can reach apps by name?";

/// What `create` does about the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `--addons` was given: that is the whole add-on choice, nothing is asked or changed.
    Explicit,
    /// `--gateway`: install it, no question.
    Install,
    /// `--no-gateway`, or nobody to ask: no gateway.
    Skip,
    /// A person is at a terminal and gave no flag: ask.
    Ask,
}

/// How the gateway choice was made, recorded in the create report next to the other platform choices.
/// `NAME` stands for the cluster name; the caller fills it in.
pub fn choice_note(d: Decision, installed: bool) -> &'static str {
    match (d, installed) {
        (Decision::Install, _) => "gateway: installed (--gateway)",
        (Decision::Ask, true) => "gateway: installed (you chose yes when asked)",
        (Decision::Ask, false) => {
            "gateway: not installed (you chose no when asked); add it later with `fluxctl kube cluster platform envoy-gateway NAME`"
        }
        (Decision::Skip, _) => {
            "gateway: not installed; add it later with `fluxctl kube cluster platform envoy-gateway NAME`"
        }
        (Decision::Explicit, true) => "gateway: installed (--addons envoy-gateway)",
        (Decision::Explicit, false) => {
            "gateway: not installed (--addons was given, so nothing was asked)"
        }
    }
}

/// The one command that installs Envoy Gateway on a FluxVM cluster.
pub fn install_command(cluster: &str) -> String {
    format!("fluxctl kube cluster platform envoy-gateway {cluster}")
}

pub fn decide(
    addons: Option<&str>,
    gateway: bool,
    no_gateway: bool,
    tty: bool,
) -> Result<Decision> {
    if gateway && no_gateway {
        bail!(
            "--gateway and --no-gateway contradict each other; give one of them, or neither to be asked on a terminal"
        );
    }
    if let Some(a) = addons {
        if gateway && a != ENVOY_GATEWAY_PROFILE {
            bail!(
                "--gateway cannot be combined with --addons {a}: an add-on profile is one choice. Use `--addons {ENVOY_GATEWAY_PROFILE}` (everything k3s ships, plus the gateway) or drop --addons ({})",
                ADDON_PROFILES.join(", ")
            );
        }
        if no_gateway && a == ENVOY_GATEWAY_PROFILE {
            bail!("--no-gateway contradicts --addons {ENVOY_GATEWAY_PROFILE}; drop one of them");
        }
        return Ok(if gateway {
            Decision::Install
        } else {
            Decision::Explicit
        });
    }
    Ok(match (gateway, no_gateway, tty) {
        (true, _, _) => Decision::Install,
        (_, true, _) => Decision::Skip,
        (_, _, true) => Decision::Ask,
        _ => Decision::Skip,
    })
}

/// `[Y/n]`: an empty answer is yes. Anything not understood asks again (`None`).
pub fn parse_answer(line: &str) -> Option<bool> {
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Asks the question until it gets an answer. End of input (a closed terminal) means no, never a surprise install.
pub fn ask(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<bool> {
    for _ in 0..3 {
        write!(out, "{QUESTION} [Y/n] ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(out)?;
            return Ok(false);
        }
        if let Some(a) = parse_answer(&line) {
            return Ok(a);
        }
        writeln!(out, "Please answer y or n.")?;
    }
    Ok(false)
}

/// The final `--addons` value after the decision, and whether the gateway ends up installed.
pub fn resolve(
    addons: Option<String>,
    d: Decision,
    answer: impl FnOnce() -> Result<bool>,
) -> Result<(Option<String>, bool)> {
    let want = match d {
        Decision::Install => true,
        Decision::Ask => answer()?,
        Decision::Skip | Decision::Explicit => false,
    };
    let installed =
        want || (d == Decision::Explicit && addons.as_deref() == Some(ENVOY_GATEWAY_PROFILE));
    Ok((
        if want {
            Some(ENVOY_GATEWAY_PROFILE.into())
        } else {
            addons
        },
        installed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_table() {
        use Decision::*;
        let g = ENVOY_GATEWAY_PROFILE;
        // (addons, --gateway, --no-gateway, tty) -> decision
        let cases = [
            (None, false, false, true, Ask),
            (None, false, false, false, Skip),
            (None, true, false, true, Install),
            (None, true, false, false, Install),
            (None, false, true, true, Skip),
            (None, false, true, false, Skip),
            (Some("minimal"), false, false, true, Explicit),
            (Some("minimal"), false, true, true, Explicit),
            (Some("default"), false, false, false, Explicit),
            (Some(g), false, false, true, Explicit),
            (Some(g), true, false, false, Install),
        ];
        for (a, gw, ng, tty, want) in cases {
            assert_eq!(
                decide(a, gw, ng, tty).unwrap(),
                want,
                "{a:?} {gw} {ng} {tty}"
            );
        }
    }

    #[test]
    fn contradictions_say_what_to_do() {
        let e = decide(None, true, true, true).unwrap_err().to_string();
        assert!(e.contains("contradict") && e.contains("give one"), "{e}");
        let e = decide(Some("metallb"), true, false, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--addons envoy-gateway"), "{e}");
        let e = decide(Some(ENVOY_GATEWAY_PROFILE), false, true, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("drop one"), "{e}");
    }

    #[test]
    fn answers() {
        for y in ["", "\n", "y", "Y", " yes \n"] {
            assert_eq!(parse_answer(y), Some(true), "{y:?}");
        }
        for n in ["n", "N", "no\n"] {
            assert_eq!(parse_answer(n), Some(false), "{n:?}");
        }
        assert_eq!(parse_answer("maybe"), None);
    }

    #[test]
    fn asking_defaults_to_yes_retries_and_treats_eof_as_no() {
        let run = |s: &str| {
            let mut out = Vec::new();
            let r = ask(&mut s.as_bytes(), &mut out).unwrap();
            (r, String::from_utf8(out).unwrap())
        };
        let (r, out) = run("\n");
        assert!(
            r && out.contains("Install the Envoy gateway so you can reach apps by name? [Y/n]")
        );
        assert!(!run("n\n").0);
        let (r, out) = run("what\ny\n");
        assert!(r && out.contains("Please answer y or n."));
        assert!(!run("").0, "end of input is no");
        assert!(!run("a\nb\nc\n").0, "three unclear answers is no");
    }

    #[test]
    fn resolve_sets_the_profile_only_when_wanted() {
        let never = || -> Result<bool> { panic!("must not ask") };
        assert_eq!(resolve(None, Decision::Skip, never).unwrap(), (None, false));
        assert_eq!(
            resolve(Some("minimal".into()), Decision::Explicit, never).unwrap(),
            (Some("minimal".into()), false)
        );
        assert_eq!(
            resolve(
                Some(ENVOY_GATEWAY_PROFILE.into()),
                Decision::Explicit,
                never
            )
            .unwrap(),
            (Some(ENVOY_GATEWAY_PROFILE.into()), true)
        );
        assert_eq!(
            resolve(None, Decision::Install, never).unwrap(),
            (Some(ENVOY_GATEWAY_PROFILE.into()), true)
        );
        assert_eq!(
            resolve(None, Decision::Ask, || Ok(true))
                .unwrap()
                .0
                .as_deref(),
            Some("envoy-gateway")
        );
        assert_eq!(
            resolve(None, Decision::Ask, || Ok(false)).unwrap(),
            (None, false)
        );
    }

    #[test]
    fn the_install_command_is_one_line() {
        assert_eq!(
            install_command("dev"),
            "fluxctl kube cluster platform envoy-gateway dev"
        );
        assert!(choice_note(Decision::Skip, false).contains("kube cluster platform envoy-gateway"));
    }
}
