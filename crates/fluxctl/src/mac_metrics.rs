// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac metrics`: Prometheus text for what the daemon's own `/metrics` cannot know, because it is fluxctl's view of
//! the Mac: instances by state, macOS slots in use (of 2), the macOS template store's size, and, with `--kube`, readiness of
//! the `kube apple` nodes and the age of the newest etcd snapshot.
//!
//! It prints to stdout, or with `--textfile PATH` writes the file atomically for node_exporter's textfile collector (run it
//! from launchd every minute). Names follow `fleet-metrics.md`: prefix `fluxvm_`, and a `node` label for this Mac when
//! `--node` is given (the same meaning `node` has in the fleet families). A Kubernetes node is `k8s_node`, so the two never
//! clash. Nothing in the output comes from a VM's environment, kubeconfig or the cluster token.

use crate::mac_events::MAX_SLOTS;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fmt::Write as _;
use std::path::Path;

/// States an instance can report, so every one is exported (zero included) and alerts never see an absent series.
pub const STATES: [&str; 6] = [
    "creating", "running", "paused", "stopped", "failed", "unknown",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KubeNodeReading {
    pub name: String,
    pub role: String,
    /// The VM is running.
    pub up: bool,
    /// The Kubernetes Ready condition, when it could be read.
    pub ready: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KubeClusterReading {
    pub name: String,
    pub healthy: bool,
    pub nodes: Vec<KubeNodeReading>,
    /// Seconds since the newest etcd snapshot; `None` for a SQLite cluster, no snapshot, or an unreadable list.
    pub newest_snapshot_age: Option<u64>,
    pub snapshots: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
    pub node: Option<String>,
    /// Instance states; `None` when the daemon could not be asked.
    pub instance_states: Option<Vec<String>>,
    /// `(templates, bytes)`; `None` when the store could not be read.
    pub templates: Option<(usize, u64)>,
    /// `None` when `--kube` was not given.
    pub kube: Option<Vec<KubeClusterReading>>,
}

/// A label value as the Prometheus text format needs it.
pub fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn labels(node: Option<&str>, extra: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = node
        .map(|n| format!("node=\"{}\"", esc(n)))
        .into_iter()
        .collect();
    parts.extend(extra.iter().map(|(k, v)| format!("{k}=\"{}\"", esc(v))));
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

fn head(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
}

fn flag(b: bool) -> u8 {
    u8::from(b)
}

/// The exposition text.
pub fn render(i: &Inputs) -> String {
    let n = i.node.as_deref();
    let mut o = String::new();

    head(
        &mut o,
        "fluxvm_mac_collect_ok",
        "1 when that part of the reading succeeded",
    );
    for (src, ok) in [
        ("daemon", i.instance_states.is_some()),
        ("store", i.templates.is_some()),
    ] {
        let _ = writeln!(
            o,
            "fluxvm_mac_collect_ok{} {}",
            labels(n, &[("source", src)]),
            flag(ok)
        );
    }

    if let Some(states) = &i.instance_states {
        head(
            &mut o,
            "fluxvm_mac_instances",
            "macOS instances (fluxctl mac) by state",
        );
        for s in STATES {
            let c = states
                .iter()
                .filter(|x| {
                    let x = x.as_str();
                    (if STATES.contains(&x) { x } else { "unknown" }) == s
                })
                .count();
            let _ = writeln!(o, "fluxvm_mac_instances{} {c}", labels(n, &[("state", s)]));
        }
        let used = states
            .iter()
            .filter(|s| matches!(s.as_str(), "running" | "paused"))
            .count();
        head(
            &mut o,
            "fluxvm_mac_slots_used",
            "macOS VM slots held by instances (running or paused)",
        );
        let _ = writeln!(o, "fluxvm_mac_slots_used{} {used}", labels(n, &[]));
        head(
            &mut o,
            "fluxvm_mac_slots_max",
            "macOS VMs one Mac may run (Apple licence limit)",
        );
        let _ = writeln!(o, "fluxvm_mac_slots_max{} {MAX_SLOTS}", labels(n, &[]));
    }

    if let Some((count, bytes)) = i.templates {
        head(
            &mut o,
            "fluxvm_mac_templates",
            "named macOS images in the local template store",
        );
        let _ = writeln!(o, "fluxvm_mac_templates{} {count}", labels(n, &[]));
        head(
            &mut o,
            "fluxvm_mac_template_store_bytes",
            "bytes the template store occupies",
        );
        let _ = writeln!(
            o,
            "fluxvm_mac_template_store_bytes{} {bytes}",
            labels(n, &[])
        );
    }

    if let Some(clusters) = &i.kube {
        head(
            &mut o,
            "fluxvm_kube_apple_cluster_healthy",
            "1 when kube apple status finds no problem",
        );
        for c in clusters {
            let l = labels(n, &[("cluster", &c.name)]);
            let _ = writeln!(
                o,
                "fluxvm_kube_apple_cluster_healthy{l} {}",
                flag(c.healthy)
            );
        }
        let node_l = |c: &KubeClusterReading, nd: &KubeNodeReading| {
            labels(
                n,
                &[
                    ("cluster", &c.name),
                    ("k8s_node", &nd.name),
                    ("role", &nd.role),
                ],
            )
        };
        head(
            &mut o,
            "fluxvm_kube_apple_node_up",
            "1 when the node's VM is running",
        );
        for c in clusters {
            for nd in &c.nodes {
                let _ = writeln!(
                    o,
                    "fluxvm_kube_apple_node_up{} {}",
                    node_l(c, nd),
                    flag(nd.up)
                );
            }
        }
        head(
            &mut o,
            "fluxvm_kube_apple_node_ready",
            "1 when Kubernetes reports the node Ready (absent when unknown)",
        );
        for c in clusters {
            for nd in &c.nodes {
                if let Some(r) = nd.ready {
                    let _ = writeln!(
                        o,
                        "fluxvm_kube_apple_node_ready{} {}",
                        node_l(c, nd),
                        flag(r)
                    );
                }
            }
        }
        head(
            &mut o,
            "fluxvm_kube_apple_snapshots",
            "etcd snapshots on the server (absent for SQLite or unreadable)",
        );
        for c in clusters {
            if let Some(k) = c.snapshots {
                let _ = writeln!(
                    o,
                    "fluxvm_kube_apple_snapshots{} {k}",
                    labels(n, &[("cluster", &c.name)])
                );
            }
        }
        head(
            &mut o,
            "fluxvm_kube_apple_snapshot_age_seconds",
            "seconds since the newest etcd snapshot (absent when there is none)",
        );
        for c in clusters {
            if let Some(a) = c.newest_snapshot_age {
                let _ = writeln!(
                    o,
                    "fluxvm_kube_apple_snapshot_age_seconds{} {a}",
                    labels(n, &[("cluster", &c.name)])
                );
            }
        }
    }
    o
}

/// The creation times (Unix seconds) found in `k3s etcd-snapshot ls` output.
///
/// The table's exact layout is inferred, not verified on a node: one snapshot per line, a name first, a size, and a creation
/// time. So each line is searched for an RFC 3339 token; failing that, k3s's name convention `<name>-<node>-<unix seconds>`
/// is used. The header and lines with neither are skipped.
pub fn parse_snapshot_times(ls: &str) -> Vec<i64> {
    ls.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            if name.eq_ignore_ascii_case("name") {
                return None;
            }
            let rest: Vec<&str> = words.collect();
            rest.iter()
                .find_map(|w| DateTime::parse_from_rfc3339(w).ok().map(|t| t.timestamp()))
                .or_else(|| {
                    let tail = name.rsplit('-').next()?;
                    (tail.len() == 10 && tail.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| tail.parse().ok())
                        .flatten()
                })
        })
        .collect()
}

/// `(count, age in seconds of the newest)`.
pub fn snapshot_summary(ls: &str, now: DateTime<Utc>) -> (usize, Option<u64>) {
    let times = parse_snapshot_times(ls);
    let newest = times
        .iter()
        .max()
        .map(|t| (now.timestamp() - t).max(0) as u64);
    (times.len(), newest)
}

/// Writes `text` to `path` through a temporary file in the same directory, so a scrape never reads half of it.
pub fn write_textfile(path: &Path, text: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("metrics"),
        std::process::id()
    ));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn template_store() -> Option<(usize, u64)> {
    let store = crate::mac_store::Store::open(crate::mac_store::default_root().ok()?).ok()?;
    let rows = store.list().ok()?;
    // Two names can share one directory (a tag is not a copy): count each directory's files once.
    let mut dirs: Vec<&str> = rows.iter().map(|(_, e)| e.dir.as_str()).collect();
    dirs.sort_unstable();
    dirs.dedup();
    let bytes = dirs
        .iter()
        .map(|d| crate::mac_cmd::dir_size(&store.root().join(d)))
        .sum();
    Some((rows.len(), bytes))
}

async fn kube_clusters(r: &crate::remote::Remote, now: DateTime<Utc>) -> Vec<KubeClusterReading> {
    use crate::kube_apple::{host::RealHost, lifecycle, model};
    let Ok(root) = model::default_root() else {
        return vec![];
    };
    let store = model::Store::new(root);
    let Ok(states) = store.list() else {
        return vec![];
    };
    let Ok(h) = RealHost::new(r, &store, None) else {
        return vec![];
    };
    let mut out = vec![];
    for st in states {
        let mut c = KubeClusterReading {
            name: st.name.clone(),
            ..Default::default()
        };
        if let Ok(s) = lifecycle::status(&h, &store, &st.name).await {
            c.healthy = s.healthy;
            c.nodes = s
                .nodes
                .iter()
                .map(|n| KubeNodeReading {
                    name: n.name.clone(),
                    role: n.role.clone(),
                    up: n.vm == "running",
                    ready: n.ready,
                })
                .collect();
        }
        if st.datastore == model::Datastore::Etcd
            && let Ok(ls) = lifecycle::snapshot_ls(&h, &store, &st.name).await
        {
            let (count, age) = snapshot_summary(&ls, now);
            c.snapshots = Some(count);
            c.newest_snapshot_age = age;
        }
        out.push(c);
    }
    out
}

/// Reads the daemon, the template store and (with `kube`) the clusters, and renders the text.
pub async fn collect(r: &crate::remote::Remote, node: Option<String>, kube: bool) -> String {
    let instance_states = r
        .list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR))
        .await
        .ok()
        .map(|vms| {
            crate::mac_cmd::filter_instances(&serde_json::Value::Array(vms))
                .iter()
                .map(|v| crate::mac_inspect::status(v).to_owned())
                .collect()
        });
    let kube = if kube {
        Some(kube_clusters(r, Utc::now()).await)
    } else {
        None
    };
    render(&Inputs {
        node,
        instance_states,
        templates: template_store(),
        kube,
    })
}

/// `mac metrics`.
pub async fn run(
    r: &crate::remote::Remote,
    node: Option<String>,
    kube: bool,
    textfile: Option<&Path>,
) -> Result<()> {
    let text = collect(r, node, kube).await;
    match textfile {
        Some(p) => write_textfile(p, &text),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} in\n{text}"))
    }

    fn base() -> Inputs {
        Inputs {
            node: Some("studio-1".into()),
            instance_states: Some(vec![
                "running".into(),
                "paused".into(),
                "stopped".into(),
                "weird".into(),
            ]),
            templates: Some((3, 123_456)),
            kube: None,
        }
    }

    #[test]
    fn instances_by_state_slots_and_templates() {
        let t = render(&base());
        assert_eq!(
            line(
                &t,
                "fluxvm_mac_instances{node=\"studio-1\",state=\"running\"}"
            ),
            "fluxvm_mac_instances{node=\"studio-1\",state=\"running\"} 1"
        );
        // A state the daemon invents lands in "unknown", and an unused state is still exported as zero.
        assert!(
            line(
                &t,
                "fluxvm_mac_instances{node=\"studio-1\",state=\"unknown\"}"
            )
            .ends_with(" 1")
        );
        assert!(
            line(
                &t,
                "fluxvm_mac_instances{node=\"studio-1\",state=\"failed\"}"
            )
            .ends_with(" 0")
        );
        assert!(line(&t, "fluxvm_mac_slots_used").ends_with(" 2"));
        assert!(line(&t, "fluxvm_mac_slots_max").ends_with(" 2"));
        assert!(line(&t, "fluxvm_mac_templates{").ends_with(" 3"));
        assert!(line(&t, "fluxvm_mac_template_store_bytes").ends_with(" 123456"));
        assert!(t.contains("# TYPE fluxvm_mac_slots_used gauge"));
    }

    #[test]
    fn a_failed_read_is_reported_not_faked() {
        let t = render(&Inputs::default());
        assert!(line(&t, "fluxvm_mac_collect_ok{source=\"daemon\"}").ends_with(" 0"));
        assert!(line(&t, "fluxvm_mac_collect_ok{source=\"store\"}").ends_with(" 0"));
        assert!(!t.contains("fluxvm_mac_instances") && !t.contains("fluxvm_mac_slots_used"));
        assert!(!t.contains("fluxvm_kube_apple"));
    }

    #[test]
    fn kube_readiness_and_snapshot_age_have_distinct_node_labels() {
        let mut i = base();
        i.kube = Some(vec![KubeClusterReading {
            name: "dev".into(),
            healthy: false,
            nodes: vec![
                KubeNodeReading {
                    name: "cp1".into(),
                    role: "server".into(),
                    up: true,
                    ready: Some(true),
                },
                KubeNodeReading {
                    name: "w1".into(),
                    role: "agent".into(),
                    up: false,
                    ready: None,
                },
            ],
            newest_snapshot_age: Some(7200),
            snapshots: Some(4),
        }]);
        let t = render(&i);
        assert_eq!(
            line(&t, "fluxvm_kube_apple_node_ready"),
            "fluxvm_kube_apple_node_ready{node=\"studio-1\",cluster=\"dev\",k8s_node=\"cp1\",role=\"server\"} 1"
        );
        // An unknown readiness is absent, not 0.
        assert_eq!(t.matches("fluxvm_kube_apple_node_ready{").count(), 1);
        assert!(
            line(
                &t,
                "fluxvm_kube_apple_node_up{node=\"studio-1\",cluster=\"dev\",k8s_node=\"w1\""
            )
            .ends_with(" 0")
        );
        assert!(line(&t, "fluxvm_kube_apple_cluster_healthy{").ends_with(" 0"));
        assert!(line(&t, "fluxvm_kube_apple_snapshot_age_seconds{").ends_with(" 7200"));
        assert!(line(&t, "fluxvm_kube_apple_snapshots{").ends_with(" 4"));
    }

    #[test]
    fn a_cluster_without_snapshots_has_no_age_series() {
        let mut i = Inputs::default();
        i.kube = Some(vec![KubeClusterReading {
            name: "dev".into(),
            ..Default::default()
        }]);
        let t = render(&i);
        assert!(!t.contains("fluxvm_kube_apple_snapshot_age_seconds{"));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        let i = Inputs {
            node: Some("we\"ird".into()),
            ..Default::default()
        };
        assert!(render(&i).contains("node=\"we\\\"ird\""));
    }

    #[test]
    fn snapshot_times_come_from_a_timestamp_column_or_the_name() {
        let ls = "Name                              Location                      Size     Created\n\
                  nightly-cp1-1700000000            file:///snap/nightly-cp1-1700000000  1234  2023-11-14T22:13:20Z\n\
                  on-demand-cp1-1700003600          file:///snap/on-demand-cp1-1700003600  1234  -\n\
                  garbage line\n";
        assert_eq!(parse_snapshot_times(ls), [1_700_000_000, 1_700_003_600]);
        let now = DateTime::from_timestamp(1_700_003_600 + 90, 0).unwrap();
        assert_eq!(snapshot_summary(ls, now), (2, Some(90)));
        assert_eq!(
            snapshot_summary("Name Location Size Created\n", now),
            (0, None)
        );
        // A clock behind the snapshot never gives a negative age.
        let early = DateTime::from_timestamp(1_600_000_000, 0).unwrap();
        assert_eq!(snapshot_summary(ls, early).1, Some(0));
    }

    #[test]
    fn the_textfile_is_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("fluxvm-mac-metrics-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("fluxvm_mac.prom");
        write_textfile(&f, "a 1\n").unwrap();
        write_textfile(&f, "b 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "b 2\n");
        // No temporary file is left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
