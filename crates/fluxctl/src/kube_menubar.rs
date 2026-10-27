//! `fluxctl kube menubar`: the cluster's state as a menu-bar item, in the text format SwiftBar and xbar read (the first line
//! is the title, `---` starts the menu, `|` adds attributes). There is no daemon or app to install: the plugin is a
//! one-line script that runs this command every 30 seconds.

use crate::kube_ps::{Snapshot, State, ago, compact_names, pod_summary};

/// A value that is safe inside a plugin line: no newline or `|` can end the line or start an attribute.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '|' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// A clickable item that runs `fluxctl kube ARGS --name CLUSTER` and refreshes the menu.
fn action(label: &str, bin: &str, cluster: &str, args: &[&str], terminal: bool) -> String {
    let mut params = vec!["kube".to_owned()];
    params.extend(args.iter().map(|a| (*a).to_owned()));
    params.extend(["--name".into(), cluster.to_owned()]);
    let p: String = params
        .iter()
        .enumerate()
        .map(|(i, a)| format!(" param{}={}", i + 1, clean(a).replace(' ', "")))
        .collect();
    format!(
        "{label} | bash={}{p} terminal={terminal} refresh=true\n",
        clean(bin).replace(' ', "\\ ")
    )
}

pub fn render(s: &Snapshot, bin: &str) -> String {
    let (fine, bad) = pod_summary(&s.pods);
    let mut out = String::new();
    // The title: what you want to know without opening the menu.
    out.push_str(&match &s.state {
        State::Running if bad.is_empty() => format!("⎈ {fine}\n"),
        State::Running => format!("⎈ {} ⚠ | color=red\n", bad.len()),
        State::Asleep => "⎈ zzz\n".to_owned(),
        State::Stopped(_) => "⎈ off\n".to_owned(),
        State::Missing => "⎈ –\n".to_owned(),
    });
    out.push_str("---\n");
    let c = clean(&s.name);
    match &s.state {
        State::Missing => {
            out.push_str(&format!("{c}: not created\n"));
            out.push_str(&action("Create the cluster", bin, &s.name, &["up"], true));
        }
        State::Stopped(status) => {
            out.push_str(&format!("{c} is {}\n", clean(status)));
            out.push_str(&action("Start", bin, &s.name, &["up"], true));
        }
        State::Asleep => {
            out.push_str(&format!("{c} is asleep\n"));
            out.push_str(&action("Wake", bin, &s.name, &["wake"], false));
        }
        State::Running => {
            out.push_str(&format!(
                "{c} · running · {}\n",
                clean(s.guest_ip.as_deref().unwrap_or("no address yet"))
            ));
            out.push_str(&format!("{fine} pod(s) fine\n"));
            for b in &bad {
                out.push_str(&format!("⚠ {} | color=red\n", clean(b)));
            }
            if !s.api_answers {
                out.push_str("API port is not answering | color=red\n");
            }
            if !s.apps.is_empty() {
                out.push_str("---\nApps\n");
                for (app, ports) in &s.apps {
                    let ports: Vec<String> = ports.iter().map(|p| format!(":{p}")).collect();
                    out.push_str(&format!("--{} {}\n", clean(app), ports.join(" ")));
                }
            }
            let names = compact_names(s.names.clone());
            if s.names_listener && !names.is_empty() {
                out.push_str("---\nOpen\n");
                for n in names.iter().take(12) {
                    let n = clean(n);
                    out.push_str(&format!("--{n} | href=http://{n}\n"));
                }
            }
            if let Some((label, secs)) = &s.newest {
                out.push_str(&format!(
                    "---\n{} checkpoint(s), newest {} {} ago\n",
                    s.checkpoints,
                    clean(label),
                    ago(*secs)
                ));
            }
            out.push_str("---\n");
            out.push_str(&action("Sleep", bin, &s.name, &["sleep"], false));
            out.push_str(&action("Doctor", bin, &s.name, &["doctor"], true));
            out.push_str(&action("Status", bin, &s.name, &["ps"], true));
        }
    }
    out.push_str("Refresh | refresh=true\n");
    out
}

/// The plugin file: `fluxvm-kube.30s.sh` in SwiftBar's or xbar's plugin folder.
pub fn plugin_script(bin: &str, cluster: &str) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    format!(
        "#!/bin/bash\n# <xbar.title>FluxVM Kubernetes</xbar.title>\n# <xbar.desc>State of the {cluster} cluster</xbar.desc>\n# <swiftbar.hideAbout>true</swiftbar.hideAbout>\nexec {} kube menubar --name {}\n",
        q(bin),
        q(cluster)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_doctor::PodRow;

    fn pod(name: &str, status: &str, restarts: u32) -> PodRow {
        PodRow {
            namespace: "default".into(),
            name: name.into(),
            status: status.into(),
            restarts,
        }
    }

    fn snap(state: State, pods: Vec<PodRow>) -> Snapshot {
        Snapshot {
            name: "dr".into(),
            state,
            guest_ip: Some("192.168.64.2".into()),
            api_port: 6551,
            api_answers: true,
            pods,
            apps: vec![("postgres".into(), vec![5432])],
            forwards: vec![],
            names: vec!["web.localhost".into()],
            ingress: false,
            names_listener: true,
            https_listener: false,
            checkpoints: 2,
            newest: Some(("auto-1".into(), 120)),
        }
    }

    #[test]
    fn a_healthy_cluster_shows_its_pod_count_and_actions() {
        let t = render(
            &snap(State::Running, vec![pod("a", "Running", 0)]),
            "/opt/bin/fluxctl",
        );
        assert!(t.starts_with("⎈ 1\n---\n"), "{t}");
        for want in [
            "dr · running · 192.168.64.2",
            "--postgres :5432",
            "--web.localhost | href=http://web.localhost",
            "2 checkpoint(s), newest auto-1 2m ago",
            "Sleep | bash=/opt/bin/fluxctl param1=kube param2=sleep param3=--name param4=dr terminal=false refresh=true",
        ] {
            assert!(t.contains(want), "{want} in {t}");
        }
    }

    #[test]
    fn a_failing_pod_turns_the_title_red() {
        let t = render(
            &snap(
                State::Running,
                vec![pod("a", "Running", 0), pod("b", "CrashLoopBackOff", 5)],
            ),
            "f",
        );
        assert!(t.starts_with("⎈ 1 ⚠ | color=red\n"), "{t}");
        assert!(
            t.contains("⚠ default/b CrashLoopBackOff (restarts 5) | color=red"),
            "{t}"
        );
    }

    #[test]
    fn other_states_offer_the_one_thing_to_do() {
        let asleep = render(&snap(State::Asleep, vec![]), "f");
        assert!(
            asleep.starts_with("⎈ zzz\n")
                && asleep.contains("Wake | bash=f param1=kube param2=wake"),
            "{asleep}"
        );
        let off = render(&snap(State::Stopped("stopped".into()), vec![]), "f");
        assert!(
            off.starts_with("⎈ off\n") && off.contains("Start | bash=f"),
            "{off}"
        );
        let none = render(&snap(State::Missing, vec![]), "f");
        assert!(
            none.starts_with("⎈ –\n") && none.contains("Create the cluster"),
            "{none}"
        );
    }

    #[test]
    fn nothing_can_break_out_of_a_line() {
        let mut s = snap(State::Running, vec![pod("a|b\nc", "Running", 0)]);
        s.name = "dr".into();
        s.apps = vec![("x | bash=/bin/sh".into(), vec![1])];
        let t = render(&s, "/Applications/My Tools/fluxctl");
        assert!(!t.contains("x | bash"), "{t}");
        assert!(t.contains("bash=/Applications/My\\ Tools/fluxctl"), "{t}");
        for line in t.lines() {
            assert!(
                line.matches(" | ").count() <= 1 || line.contains("terminal="),
                "{line}"
            );
        }
    }

    #[test]
    fn the_plugin_runs_this_command_with_quoting() {
        let p = plugin_script("/Users/me/it's/fluxctl", "dr");
        assert!(
            p.starts_with("#!/bin/bash\n")
                && p.contains("exec '/Users/me/it'\\''s/fluxctl' kube menubar --name 'dr'\n"),
            "{p}"
        );
    }
}
