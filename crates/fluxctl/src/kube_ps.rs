//! `fluxctl kube ps`: one screen that says what the cluster is doing, the way `docker ps` does for containers: its state,
//! the pods that are failing, the apps and forwards on this Mac, the names that work, and the checkpoints. The gathering is
//! thin; `render` and the small summaries are pure.

use crate::kube_doctor::{PodRow, is_bad, parse_pods};
use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;
use anyhow::Result;

#[derive(Debug, PartialEq, Eq)]
pub enum State {
    Missing,
    Stopped(String),
    Asleep,
    Running,
}

#[derive(Debug)]
pub struct Snapshot {
    pub name: String,
    pub state: State,
    pub guest_ip: Option<String>,
    pub api_port: u16,
    pub api_answers: bool,
    pub pods: Vec<PodRow>,
    /// (app, Mac ports)
    pub apps: Vec<(String, Vec<u16>)>,
    pub forwards: Vec<String>,
    pub names: Vec<String>,
    pub ingress: bool,
    pub names_listener: bool,
    pub https_listener: bool,
    pub checkpoints: usize,
    /// (label, seconds ago) of the newest
    pub newest: Option<(String, i64)>,
}

/// `90` -> `1m`, `7300` -> `2h`, `200000` -> `2d`.
pub fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// How many pods are fine and which are not: a bad status, or three or more restarts.
pub fn pod_summary(rows: &[PodRow]) -> (usize, Vec<String>) {
    let bad: Vec<String> = rows
        .iter()
        .filter(|p| is_bad(&p.status) || p.restarts >= 3)
        .map(|p| {
            format!(
                "{}/{} {} (restarts {})",
                p.namespace, p.name, p.status, p.restarts
            )
        })
        .collect();
    (rows.len() - bad.len(), bad)
}

/// `web.default.localhost` is dropped when `web.localhost` says the same thing.
pub fn compact_names(names: Vec<String>) -> Vec<String> {
    let all = names.clone();
    names
        .into_iter()
        .filter(|n| match n.strip_suffix(".default.localhost") {
            Some(svc) => !all.contains(&format!("{svc}.localhost")),
            None => true,
        })
        .collect()
}

fn row(label: &str, text: impl AsRef<str>) -> String {
    format!("{label:<12} {}\n", text.as_ref())
}

pub fn render(s: &Snapshot) -> String {
    let mut out = String::new();
    match &s.state {
        State::Missing => {
            return row(
                "cluster",
                format!("{}: not created. fluxctl kube up", s.name),
            );
        }
        State::Stopped(status) => {
            return row(
                "cluster",
                format!("{} is {status}. fluxctl kube up", s.name),
            );
        }
        State::Asleep => {
            return row(
                "cluster",
                format!(
                    "{} is asleep; the next kubectl call under `kube supervise` wakes it, or run fluxctl kube wake",
                    s.name
                ),
            );
        }
        State::Running => {}
    }
    out.push_str(&row(
        "cluster",
        format!(
            "{}  running  {}",
            s.name,
            s.guest_ip.as_deref().unwrap_or("(no address yet)")
        ),
    ));
    out.push_str(&row(
        "kubectl",
        format!(
            "context {}, API https://127.0.0.1:{} {}",
            s.name,
            s.api_port,
            if s.api_answers {
                "answers"
            } else {
                "is NOT answering: fluxctl kube up"
            }
        ),
    ));
    let (fine, bad) = pod_summary(&s.pods);
    out.push_str(&row(
        "pods",
        if bad.is_empty() {
            format!("{fine} fine")
        } else {
            format!("{fine} fine, {} failing", bad.len())
        },
    ));
    for b in bad.iter().take(5) {
        out.push_str(&row("", format!("  {b}")));
    }
    if bad.len() > 5 {
        out.push_str(&row(
            "",
            format!("  and {} more: fluxctl kube doctor", bad.len() - 5),
        ));
    }
    out.push_str(&row(
        "apps",
        if s.apps.is_empty() {
            "none (fluxctl kube add postgres)".to_owned()
        } else {
            s.apps
                .iter()
                .map(|(n, ports)| {
                    format!(
                        "{n} {}",
                        ports
                            .iter()
                            .map(|p| format!(":{p}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                })
                .collect::<Vec<_>>()
                .join("  ·  ")
        },
    ));
    out.push_str(&row(
        "forwards",
        if s.forwards.is_empty() {
            "none (fluxctl kube forward SERVICE)".to_owned()
        } else {
            s.forwards.join("  ·  ")
        },
    ));
    let scheme = if s.https_listener { "https" } else { "http" };
    let mut names: Vec<String> = s.names.iter().map(|n| format!("{scheme}://{n}")).collect();
    if s.ingress {
        names.push("any other *.localhost (Ingress)".to_owned());
    }
    out.push_str(&row(
        "names",
        if !s.names_listener {
            "not served: start fluxctl kube supervise".to_owned()
        } else if names.is_empty() {
            "none yet (publish a NodePort or LoadBalancer Service)".to_owned()
        } else {
            names.join("  ")
        },
    ));
    out.push_str(&row(
        "checkpoints",
        match &s.newest {
            None => "none (fluxctl kube checkpoint LABEL)".to_owned(),
            Some((label, secs)) => {
                format!("{}, newest {label} ({} ago)", s.checkpoints, ago(*secs))
            }
        },
    ));
    out
}

pub async fn gather<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Snapshot> {
    let mut s = Snapshot {
        name: o.name.clone(),
        state: State::Missing,
        guest_ip: None,
        api_port: o.api_port,
        api_answers: false,
        pods: vec![],
        apps: vec![],
        forwards: vec![],
        names: vec![],
        ingress: false,
        names_listener: false,
        https_listener: false,
        checkpoints: 0,
        newest: None,
    };
    let Some((id, status)) = api.find(&o.name).await? else {
        return Ok(s);
    };
    if status != "running" {
        s.state = if crate::kube_checkpoint::is_asleep(api, o).await {
            State::Asleep
        } else {
            State::Stopped(status)
        };
        return Ok(s);
    }
    s.state = State::Running;
    s.guest_ip = api.guest_ip(id).await.unwrap_or(None);
    s.api_answers = kube_host::api_answers(o.api_port);
    if let Ok(out) = kube_host::guest_sh(api, o, "sudo k3s kubectl get pods -A --no-headers").await
    {
        s.pods = parse_pods(&out);
    }
    let published = kube_host::published_services(api, o)
        .await
        .unwrap_or_default();
    let running = kube_host::guest_sh(
        api,
        o,
        "sudo k3s kubectl -n default get deploy -l app.kubernetes.io/managed-by=fluxvm -o jsonpath='{.items[*].metadata.name}'",
    )
    .await
    .unwrap_or_default();
    for app in crate::kube_apps::ALL {
        if running.split_whitespace().any(|r| r == app.name()) {
            let mut ports: Vec<u16> = published
                .iter()
                .filter(|p| p.namespace == "default" && p.name == app.name())
                .map(|p| p.host_port())
                .collect();
            ports.sort_unstable();
            s.apps.push((app.name().to_owned(), ports));
        }
    }
    if let Ok(text) = crate::kube_forward::list(api, o).await
        && !text.starts_with("nothing")
    {
        s.forwards = text.lines().map(str::to_owned).collect();
    }
    let table = crate::kube_names::route_table(&published);
    s.ingress = table.contains_key(crate::kube_names::INGRESS);
    s.names = compact_names(
        table
            .keys()
            .filter(|k| *k != crate::kube_names::INGRESS)
            .cloned()
            .collect(),
    );
    s.names_listener = kube_host::names_are_served();
    s.https_listener = kube_host::api_answers(443);
    if let Ok(v) = crate::kube_checkpoint::list(api, o).await {
        let list = v["checkpoints"].as_array().cloned().unwrap_or_default();
        s.checkpoints = list.len();
        s.newest = list.first().and_then(|c| {
            let label = c["label"].as_str()?.to_owned();
            let at = chrono::DateTime::parse_from_rfc3339(c["created_at"].as_str()?).ok()?;
            Some((
                label,
                (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds(),
            ))
        });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Snapshot {
        Snapshot {
            name: "dr".into(),
            state: State::Running,
            guest_ip: Some("192.168.64.9".into()),
            api_port: 6443,
            api_answers: true,
            pods: parse_pods(
                "default web 1/1 Running 0 5m\ndefault api 0/1 CrashLoopBackOff 7 (3s ago) 9m\n",
            ),
            apps: vec![
                ("postgres".into(), vec![5432]),
                ("redpanda".into(), vec![9092, 9644]),
            ],
            forwards: vec!["web (default) 80 -> 127.0.0.1:8080".into()],
            names: vec!["web.localhost".into(), "web.default.localhost".into()],
            ingress: true,
            names_listener: true,
            https_listener: false,
            checkpoints: 3,
            newest: Some(("auto-20261026-101500".into(), 725)),
        }
    }

    #[test]
    fn a_running_cluster_shows_everything_on_one_screen() {
        let text = render(&base());
        for want in [
            "cluster      dr  running  192.168.64.9",
            "API https://127.0.0.1:6443 answers",
            "1 fine, 1 failing",
            "default/api CrashLoopBackOff (restarts 7)",
            "postgres :5432  ·  redpanda :9092 :9644",
            "web (default) 80 -> 127.0.0.1:8080",
            "http://web.localhost  http://web.default.localhost  any other *.localhost (Ingress)",
            "3, newest auto-20261026-101500 (12m ago)",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn empty_sections_say_what_to_run() {
        let mut s = base();
        s.pods = vec![];
        s.apps = vec![];
        s.forwards = vec![];
        s.names = vec![];
        s.ingress = false;
        s.newest = None;
        s.checkpoints = 0;
        let t = render(&s);
        assert!(t.contains("0 fine\n"), "{t}");
        assert!(t.contains("fluxctl kube add postgres"));
        assert!(t.contains("fluxctl kube forward SERVICE"));
        assert!(t.contains("none yet"));
        assert!(t.contains("fluxctl kube checkpoint LABEL"));
        s.names_listener = false;
        assert!(render(&s).contains("start fluxctl kube supervise"));
    }

    #[test]
    fn a_cluster_that_is_not_running_gets_one_line() {
        let mut s = base();
        s.state = State::Asleep;
        assert!(render(&s).contains("asleep") && render(&s).lines().count() == 1);
        s.state = State::Stopped("stopped".into());
        assert!(render(&s).contains("dr is stopped. fluxctl kube up"));
        s.state = State::Missing;
        assert!(render(&s).contains("not created"));
    }

    #[test]
    fn https_names_are_shown_with_https() {
        let mut s = base();
        s.https_listener = true;
        assert!(render(&s).contains("https://web.localhost"));
    }

    #[test]
    fn restarts_alone_count_as_failing_and_long_lists_are_cut() {
        let rows =
            parse_pods("default flap 1/1 Running 5 (20s ago) 4m\ndefault ok 1/1 Running 0 4m\n");
        assert_eq!(pod_summary(&rows).0, 1);
        assert!(pod_summary(&rows).1[0].starts_with("default/flap"));
        let many: String = (0..8)
            .map(|i| format!("default p{i} 0/1 Error 1 1m\n"))
            .collect();
        let mut s = base();
        s.pods = parse_pods(&many);
        assert!(render(&s).contains("and 3 more: fluxctl kube doctor"));
    }

    #[test]
    fn the_default_namespace_name_is_dropped_when_the_short_one_exists() {
        let names = vec![
            "redis.default.localhost".to_owned(),
            "redis.localhost".to_owned(),
            "web.shop.localhost".to_owned(),
            "odd.default.localhost".to_owned(),
        ];
        assert_eq!(
            compact_names(names),
            [
                "redis.localhost",
                "web.shop.localhost",
                "odd.default.localhost"
            ]
        );
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(90), "1m");
        assert_eq!(ago(7300), "2h");
        assert_eq!(ago(200000), "2d");
        assert_eq!(ago(-4), "0s");
    }
}
