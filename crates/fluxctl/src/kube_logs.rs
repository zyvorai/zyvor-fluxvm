//! `fluxctl kube logs`: the logs of every pod behind a workload in one stream, each line tagged with its pod. Plain
//! `kubectl logs deploy/web` shows one pod only; this follows them all, including pods that start while you watch.

use crate::{
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, IsTerminal, Write};

pub struct Logs<'a> {
    pub target: &'a str,
    pub namespace: &'a str,
    pub container: Option<&'a str>,
    pub since: Option<&'a str>,
    pub tail: Option<u32>,
    pub follow: bool,
    pub previous: bool,
    pub timestamps: bool,
}

/// `web` -> `deploy/web`; a `pod/NAME` is read directly, the others through their label selector.
pub fn resolve_target(t: &str) -> Result<(&'static str, String)> {
    let (kind, name) = t.split_once('/').unwrap_or(("deploy", t));
    let kind = match kind {
        "deploy" | "deployment" | "deployments" => "deploy",
        "pod" | "pods" | "po" => "pod",
        "statefulset" | "sts" => "statefulset",
        "daemonset" | "ds" => "daemonset",
        other => bail!(
            "cannot read logs of a {other:?}: use NAME, deploy/NAME, pod/NAME, statefulset/NAME or daemonset/NAME"
        ),
    };
    if !kube_host::resource_name_ok(name) {
        bail!("{name:?} is not a Kubernetes name");
    }
    Ok((kind, name.to_owned()))
}

/// `30s`, `5m`, `2h`: what `kubectl logs --since` accepts, and nothing a shell could read differently.
fn since_ok(s: &str) -> bool {
    let n = s.trim_end_matches(['s', 'm', 'h']);
    s.len() > n.len()
        && s.len() - n.len() == 1
        && !n.is_empty()
        && n.len() <= 6
        && n.bytes().all(|c| c.is_ascii_digit())
}

pub fn validate(l: &Logs<'_>) -> Result<()> {
    resolve_target(l.target)?;
    if !kube_host::resource_name_ok(l.namespace) {
        bail!("namespace {:?} is not a Kubernetes name", l.namespace);
    }
    if let Some(c) = l.container
        && !kube_host::resource_name_ok(c)
    {
        bail!("container {c:?} is not a Kubernetes name");
    }
    if let Some(s) = l.since
        && !since_ok(s)
    {
        bail!("--since {s:?}: use a number and s, m or h, for example 10m");
    }
    Ok(())
}

/// The remote command. A workload's pods are found through its selector, so every replica is read at once.
pub fn script(l: &Logs<'_>) -> Result<String> {
    validate(l)?;
    let (kind, name) = resolve_target(l.target)?;
    let mut flags = String::from("--prefix --max-log-requests 50 --ignore-errors");
    match l.container {
        Some(c) => flags.push_str(&format!(" -c {c}")),
        None => flags.push_str(" --all-containers"),
    }
    if let Some(s) = l.since {
        flags.push_str(&format!(" --since={s}"));
    }
    flags.push_str(&format!(" --tail={}", l.tail.unwrap_or(50)));
    if l.follow {
        flags.push_str(" -f");
    }
    if l.previous {
        flags.push_str(" -p");
    }
    if l.timestamps {
        flags.push_str(" --timestamps");
    }
    let ns = l.namespace;
    if kind == "pod" {
        return Ok(format!("sudo k3s kubectl logs -n {ns} {flags} pod/{name}"));
    }
    // The selector is read from the workload itself: `key=value` pairs joined by commas.
    let tpl = "{{range $k,$v := .spec.selector.matchLabels}}{{$k}}={{$v}},{{end}}";
    Ok(format!(
        "sel=$(sudo k3s kubectl get {kind}/{name} -n {ns} -o go-template='{tpl}') && sel=${{sel%,}} && \
         [ -n \"$sel\" ] && exec sudo k3s kubectl logs -n {ns} {flags} -l \"$sel\""
    ))
}

const COLORS: [u8; 6] = [36, 33, 35, 32, 34, 91];

/// Gives each pod's `[pod/NAME/container]` tag its own color so replicas can be told apart at a glance.
pub fn colorize(line: &str, on: bool) -> String {
    if !on || !line.starts_with('[') {
        return line.to_owned();
    }
    let Some(end) = line.find("] ") else {
        return line.to_owned();
    };
    let tag = &line[..=end];
    let hash = tag
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let color = COLORS[hash as usize % COLORS.len()];
    format!("\x1b[{color}m{tag}\x1b[0m{}", &line[end + 1..])
}

pub async fn run<A: VmApi>(api: &A, o: &KubeOptions, l: &Logs<'_>) -> Result<()> {
    let script = script(l)?;
    let mut child = kube_host::guest_spawn(api, o, &script).await?;
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let out = child
        .stdout
        .take()
        .context("no output from the server VM")?;
    let mut stdout = std::io::stdout().lock();
    for line in BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        // A closed pipe (`| head`) is a normal way to stop.
        if writeln!(stdout, "{}", colorize(&line, color)).is_err() {
            let _ = child.kill();
            return Ok(());
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "kubectl logs exited with {status}; does {} exist in {}?",
            l.target,
            l.namespace
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Logs<'static> {
        Logs {
            target: "web",
            namespace: "default",
            container: None,
            since: None,
            tail: None,
            follow: false,
            previous: false,
            timestamps: false,
        }
    }

    #[test]
    fn targets_resolve_and_bad_ones_are_refused() {
        assert_eq!(resolve_target("web").unwrap(), ("deploy", "web".into()));
        assert_eq!(
            resolve_target("sts/db").unwrap(),
            ("statefulset", "db".into())
        );
        assert_eq!(
            resolve_target("pod/web-1").unwrap(),
            ("pod", "web-1".into())
        );
        assert!(resolve_target("job/x").is_err());
        assert!(resolve_target("deploy/a;id").is_err());
        assert!(resolve_target("").is_err());
    }

    #[test]
    fn since_takes_only_a_number_and_a_unit() {
        for ok in ["30s", "5m", "2h", "100m"] {
            assert!(since_ok(ok), "{ok}");
        }
        for bad in [
            "", "5", "m", "5d", "1h30m", "5m;id", "-5m", "5 m", "1234567s",
        ] {
            assert!(!since_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_workload_is_read_through_its_selector() {
        let s = script(&base()).unwrap();
        assert!(
            s.contains("get deploy/web -n default -o go-template"),
            "{s}"
        );
        assert!(s.contains("-l \"$sel\""), "{s}");
        assert!(
            s.contains("--prefix") && s.contains("--all-containers") && s.contains("--tail=50")
        );
        assert!(!s.contains(" -f"));
    }

    #[test]
    fn a_pod_is_read_directly_and_flags_pass_through() {
        let l = Logs {
            target: "pod/web-1",
            container: Some("app"),
            since: Some("10m"),
            tail: Some(5),
            follow: true,
            previous: true,
            timestamps: true,
            ..base()
        };
        let s = script(&l).unwrap();
        assert!(s.ends_with("pod/web-1"), "{s}");
        for f in [
            "-c app",
            "--since=10m",
            "--tail=5",
            " -f",
            " -p",
            "--timestamps",
        ] {
            assert!(s.contains(f), "{f} in {s}");
        }
        assert!(!s.contains("--all-containers"));
    }

    #[test]
    fn hostile_input_never_reaches_the_script() {
        assert!(
            script(&Logs {
                namespace: "x;id",
                ..base()
            })
            .is_err()
        );
        assert!(
            script(&Logs {
                container: Some("a b"),
                ..base()
            })
            .is_err()
        );
        assert!(
            script(&Logs {
                since: Some("1m;id"),
                ..base()
            })
            .is_err()
        );
    }

    #[test]
    fn pod_tags_are_colored_stably_and_text_is_left_alone() {
        let a = colorize("[pod/web-1/app] hello", true);
        assert!(
            a.starts_with("\x1b[") && a.ends_with("\x1b[0m hello"),
            "{a:?}"
        );
        assert_eq!(a, colorize("[pod/web-1/app] hello", true));
        assert_eq!(
            colorize("[pod/web-1/app] hello", false),
            "[pod/web-1/app] hello"
        );
        assert_eq!(colorize("plain line", true), "plain line");
        assert_eq!(colorize("[unclosed", true), "[unclosed");
    }
}
