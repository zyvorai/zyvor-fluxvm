//! `fluxctl kube diff` and `kube timeline`: git log for the cluster. Every checkpoint (including the `try-`, `auto-` and
//! `git-` ones) is saved with a small inventory of what ran in the cluster at that moment: each Deployment, StatefulSet,
//! DaemonSet, CronJob, Service, Ingress, PersistentVolumeClaim, ConfigMap, Secret and Namespace with its images, its
//! replica count and a hash of its spec. Two inventories compare without starting either checkpoint, so "what changed
//! between when it worked and now" is an answer, not a restore. Secret values are hashed on the Mac and never stored.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::kube_checkpoint;
use crate::kube_host::{self, KubeOptions};
use crate::run::VmApi;

/// The kinds the inventory covers, as `kubectl get` names them.
const KINDS: &str = "deploy,sts,ds,cronjob,svc,ingress,pvc,cm,secret,ns";
/// Namespaces of the cluster's own machinery: changes there are not the user's.
const SKIP_NS: [&str; 3] = ["kube-system", "kube-public", "kube-node-lease"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub images: Vec<String>,
    pub replicas: Option<i64>,
    /// First 12 hex digits of the SHA-256 of the object's spec (or data).
    pub hash: String,
}

/// `Kind/namespace/name` -> what it was.
pub type Inventory = BTreeMap<String, Item>;

fn short_hash(v: &Value) -> String {
    let digest = Sha256::digest(v.to_string().as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn pod_spec(item: &Value) -> Option<&Value> {
    let spec = &item["spec"];
    [
        &spec["template"]["spec"],
        &spec["jobTemplate"]["spec"]["template"]["spec"],
    ]
    .into_iter()
    .find(|s| s.is_object())
}

fn images_of(item: &Value) -> Vec<String> {
    let Some(pod) = pod_spec(item) else {
        return vec![];
    };
    let mut v: Vec<String> = ["initContainers", "containers"]
        .iter()
        .flat_map(|k| pod[*k].as_array().into_iter().flatten())
        .filter_map(|c| c["image"].as_str().map(str::to_owned))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Builds the inventory from the JSON of `kubectl get KINDS -A -o json`.
pub fn summarize(list: &Value) -> Inventory {
    let mut out = Inventory::new();
    for item in list["items"].as_array().into_iter().flatten() {
        let kind = item["kind"].as_str().unwrap_or("");
        let ns = item["metadata"]["namespace"].as_str().unwrap_or("");
        let name = item["metadata"]["name"].as_str().unwrap_or("");
        if kind.is_empty() || name.is_empty() || SKIP_NS.contains(&ns) {
            continue;
        }
        // Noise every cluster has.
        if (kind == "Service" && name == "kubernetes" && ns == "default")
            || (kind == "ConfigMap" && name == "kube-root-ca.crt")
            || (kind == "Namespace" && SKIP_NS.contains(&name))
        {
            continue;
        }
        if kind == "Secret" {
            let t = item["type"].as_str().unwrap_or("");
            if t == "kubernetes.io/service-account-token" {
                continue;
            }
        }
        let content = if item["spec"].is_object() {
            item["spec"].clone()
        } else {
            serde_json::json!([item["data"], item["binaryData"]])
        };
        out.insert(
            format!("{kind}/{ns}/{name}"),
            Item {
                images: images_of(item),
                replicas: item["spec"]["replicas"].as_i64(),
                hash: short_hash(&content),
            },
        );
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Added(String),
    Removed(String),
    Changed(String, Vec<String>),
}

/// What differs from `old` to `new`, sorted by object.
pub fn diff(old: &Inventory, new: &Inventory) -> Vec<Change> {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut out = vec![];
    for k in keys {
        match (old.get(k), new.get(k)) {
            (None, Some(_)) => out.push(Change::Added(k.clone())),
            (Some(_), None) => out.push(Change::Removed(k.clone())),
            (Some(a), Some(b)) if a != b => out.push(Change::Changed(k.clone(), details(k, a, b))),
            _ => {}
        }
    }
    out
}

fn details(key: &str, a: &Item, b: &Item) -> Vec<String> {
    let mut d = vec![];
    if a.images != b.images {
        let gone: Vec<&String> = a.images.iter().filter(|i| !b.images.contains(i)).collect();
        let new: Vec<&String> = b.images.iter().filter(|i| !a.images.contains(i)).collect();
        if gone.len() == new.len() {
            for (x, y) in gone.iter().zip(&new) {
                d.push(format!("image {x} -> {y}"));
            }
        } else {
            d.extend(gone.iter().map(|i| format!("image -{i}")));
            d.extend(new.iter().map(|i| format!("image +{i}")));
        }
    }
    if a.replicas != b.replicas {
        let show = |r: Option<i64>| r.map_or("-".to_owned(), |n| n.to_string());
        d.push(format!(
            "replicas {} -> {}",
            show(a.replicas),
            show(b.replicas)
        ));
    }
    if a.hash != b.hash && d.is_empty() {
        let data = key.starts_with("ConfigMap/") || key.starts_with("Secret/");
        d.push(if data { "data changed" } else { "spec changed" }.to_owned());
    } else if a.hash != b.hash {
        d.push("spec changed".to_owned());
    }
    d
}

/// `+N -N ~N`, or `no changes`.
pub fn summary(changes: &[Change]) -> String {
    let n = |f: fn(&Change) -> bool| changes.iter().filter(|c| f(c)).count();
    let (a, r, c) = (
        n(|c| matches!(c, Change::Added(_))),
        n(|c| matches!(c, Change::Removed(_))),
        n(|c| matches!(c, Change::Changed(..))),
    );
    if a + r + c == 0 {
        "no changes".to_owned()
    } else {
        format!("+{a} -{r} ~{c}")
    }
}

pub fn render(changes: &[Change]) -> String {
    if changes.is_empty() {
        return "no changes".to_owned();
    }
    let mut out = String::new();
    for c in changes {
        match c {
            Change::Added(k) => out.push_str(&format!("+ {k}\n")),
            Change::Removed(k) => out.push_str(&format!("- {k}\n")),
            Change::Changed(k, d) => out.push_str(&format!("~ {k}: {}\n", d.join(", "))),
        }
    }
    out.push_str(&format!("({})", summary(changes)));
    out
}

fn dir(cluster: &str) -> PathBuf {
    kube_host::run_dir().join(format!("kube-{cluster}.inventory"))
}

fn file(cluster: &str, label: &str) -> PathBuf {
    dir(cluster).join(format!("{label}.json"))
}

fn encode(inv: &Inventory) -> String {
    serde_json::to_string(
        &inv.iter()
            .map(|(k, i)| {
                (
                    k.clone(),
                    serde_json::json!({"images": i.images, "replicas": i.replicas, "hash": i.hash}),
                )
            })
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap_or_default()
}

pub fn decode(text: &str) -> Option<Inventory> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(
        v.as_object()?
            .iter()
            .map(|(k, i)| {
                (
                    k.clone(),
                    Item {
                        images: i["images"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|s| s.as_str().map(str::to_owned))
                            .collect(),
                        replicas: i["replicas"].as_i64(),
                        hash: i["hash"].as_str().unwrap_or("").to_owned(),
                    },
                )
            })
            .collect(),
    )
}

/// The cluster's inventory right now.
pub async fn live<A: VmApi>(api: &A, o: &KubeOptions) -> Result<Inventory> {
    let out = kube_host::guest_sh(api, o, &format!("sudo k3s kubectl get {KINDS} -A -o json"))
        .await
        .context("reading what runs in the cluster")?;
    Ok(summarize(&serde_json::from_str(&out)?))
}

/// Saves the inventory for a checkpoint about to be made. Best effort: a cluster that will not answer in 20 s must not stop
/// the checkpoint.
pub async fn record<A: VmApi>(api: &A, o: &KubeOptions, label: &str) {
    let inv = match tokio::time::timeout(std::time::Duration::from_secs(20), live(api, o)).await {
        Ok(Ok(i)) => i,
        _ => return,
    };
    let _ = std::fs::create_dir_all(dir(&o.name));
    let _ = kube_host::write_private(&file(&o.name, label), &encode(&inv));
}

/// Forgets a checkpoint's inventory.
pub fn forget(cluster: &str, label: &str) {
    let _ = std::fs::remove_file(file(cluster, label));
}

fn load(cluster: &str, label: &str) -> Option<Inventory> {
    decode(&std::fs::read_to_string(file(cluster, label)).ok()?)
}

/// `now` or a checkpoint's label.
async fn side<A: VmApi>(api: &A, o: &KubeOptions, which: &str) -> Result<Inventory> {
    if which == "now" {
        return live(api, o).await;
    }
    load(&o.name, which).with_context(|| {
        format!(
            "no inventory for checkpoint {which:?}: it was made before `kube diff` existed, was removed, or the cluster could not be read at the time"
        )
    })
}

/// `kube diff [A] [B]`: A (default: the newest checkpoint) against B (default: now).
pub async fn run<A: VmApi>(
    api: &A,
    o: &KubeOptions,
    a: Option<&str>,
    b: Option<&str>,
) -> Result<String> {
    let a = match a {
        Some(a) => a.to_owned(),
        None => newest(api, o).await?,
    };
    let b = b.unwrap_or("now");
    let (old, new) = (side(api, o, &a).await?, side(api, o, b).await?);
    Ok(format!("{a} -> {b}\n{}", render(&diff(&old, &new))))
}

async fn newest<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    let listed = kube_checkpoint::list(api, o).await?;
    listed["checkpoints"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["label"].as_str())
        .map(str::to_owned)
        .context("there are no checkpoints yet; make one with `fluxctl kube checkpoint LABEL`")
}

/// One line per checkpoint, newest first, with what changed since the one before it.
pub fn timeline_lines(
    rows: &[(String, String)],
    invs: &BTreeMap<String, Inventory>,
) -> Vec<String> {
    // `rows` are (label, created_at), newest first.
    let mut out = vec![];
    for (i, (label, at)) in rows.iter().enumerate() {
        let note = match (
            invs.get(label),
            rows.get(i + 1).and_then(|(prev, _)| invs.get(prev)),
        ) {
            (Some(new), Some(old)) => summary(&diff(old, new)),
            (Some(new), None) => format!("{} objects", new.len()),
            (None, _) => "no inventory".to_owned(),
        };
        out.push(format!("{at:<26} {label:<28} {note}"));
    }
    out
}

pub async fn timeline<A: VmApi>(api: &A, o: &KubeOptions) -> Result<String> {
    let listed = kube_checkpoint::list(api, o).await?;
    let rows: Vec<(String, String)> = listed["checkpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some((
                c["label"].as_str()?.to_owned(),
                c["created_at"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect();
    if rows.is_empty() {
        bail!("there are no checkpoints yet; make one with `fluxctl kube checkpoint LABEL`");
    }
    // Inventories of checkpoints that no longer exist (pruned `try-` and `auto-` ones) are cleaned up here.
    if let Ok(entries) = std::fs::read_dir(dir(&o.name)) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(label) = name.strip_suffix(".json")
                && !rows.iter().any(|(l, _)| l == label)
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let invs: BTreeMap<String, Inventory> = rows
        .iter()
        .filter_map(|(l, _)| Some((l.clone(), load(&o.name, l)?)))
        .collect();
    Ok(timeline_lines(&rows, &invs).join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deploy(ns: &str, name: &str, image: &str, replicas: i64) -> Value {
        json!({"kind": "Deployment", "metadata": {"namespace": ns, "name": name},
            "spec": {"replicas": replicas, "template": {"spec": {"containers": [{"name": name, "image": image}]}}}})
    }

    fn inv(items: Vec<Value>) -> Inventory {
        summarize(&json!({"items": items}))
    }

    #[test]
    fn an_inventory_keeps_images_replicas_and_a_hash_and_skips_the_cluster_s_own_objects() {
        let i = inv(vec![
            deploy("shop", "web", "nginx:1.25", 2),
            deploy("kube-system", "coredns", "coredns:1", 1),
            json!({"kind": "Service", "metadata": {"namespace": "default", "name": "kubernetes"}, "spec": {}}),
            json!({"kind": "ConfigMap", "metadata": {"namespace": "shop", "name": "kube-root-ca.crt"}, "data": {}}),
            json!({"kind": "Secret", "type": "kubernetes.io/service-account-token", "metadata": {"namespace": "shop", "name": "t"}, "data": {}}),
        ]);
        assert_eq!(i.len(), 1);
        let web = &i["Deployment/shop/web"];
        assert_eq!(web.images, vec!["nginx:1.25"]);
        assert_eq!(web.replicas, Some(2));
        assert_eq!(web.hash.len(), 12);
    }

    #[test]
    fn a_cronjob_s_images_are_found_in_its_job_template() {
        let i = inv(vec![
            json!({"kind": "CronJob", "metadata": {"namespace": "d", "name": "c"},
            "spec": {"jobTemplate": {"spec": {"template": {"spec": {
                "initContainers": [{"image": "busybox"}], "containers": [{"image": "app:2"}]}}}}}}),
        ]);
        assert_eq!(i["CronJob/d/c"].images, vec!["app:2", "busybox"]);
    }

    #[test]
    fn what_was_added_removed_and_changed_is_named() {
        let old = inv(vec![
            deploy("shop", "web", "nginx:1.25", 2),
            deploy("shop", "gone", "x:1", 1),
            deploy("shop", "same", "y:1", 1),
        ]);
        let new = inv(vec![
            deploy("shop", "web", "nginx:1.27", 5),
            deploy("shop", "same", "y:1", 1),
            deploy("shop", "fresh", "z:1", 1),
        ]);
        let d = diff(&old, &new);
        assert_eq!(
            d,
            vec![
                Change::Added("Deployment/shop/fresh".into()),
                Change::Removed("Deployment/shop/gone".into()),
                Change::Changed(
                    "Deployment/shop/web".into(),
                    vec![
                        "image nginx:1.25 -> nginx:1.27".into(),
                        "replicas 2 -> 5".into(),
                        "spec changed".into()
                    ]
                ),
            ]
        );
        assert_eq!(summary(&d), "+1 -1 ~1");
        let text = render(&d);
        assert!(text.contains("+ Deployment/shop/fresh"));
        assert!(text.contains("- Deployment/shop/gone"));
        assert!(
            text.contains("~ Deployment/shop/web: image nginx:1.25 -> nginx:1.27, replicas 2 -> 5")
        );
    }

    #[test]
    fn identical_clusters_have_no_changes() {
        let a = inv(vec![deploy("shop", "web", "nginx:1.25", 2)]);
        assert!(diff(&a, &a.clone()).is_empty());
        assert_eq!(render(&[]), "no changes");
        assert_eq!(summary(&[]), "no changes");
    }

    #[test]
    fn config_and_secret_changes_say_data_changed_and_never_keep_the_values() {
        let cm = |v: &str| json!({"kind": "ConfigMap", "metadata": {"namespace": "d", "name": "c"}, "data": {"k": v}});
        let sec = |v: &str| json!({"kind": "Secret", "type": "Opaque", "metadata": {"namespace": "d", "name": "s"}, "data": {"pw": v}});
        let (a, b) = (
            inv(vec![cm("1"), sec("c2VjcmV0")]),
            inv(vec![cm("2"), sec("b3RoZXI=")]),
        );
        let d = diff(&a, &b);
        assert_eq!(
            d,
            vec![
                Change::Changed("ConfigMap/d/c".into(), vec!["data changed".into()]),
                Change::Changed("Secret/d/s".into(), vec!["data changed".into()]),
            ]
        );
        // What is written to disk holds a short hash, not the secret's bytes.
        let saved = encode(&b);
        assert!(
            !saved.contains("b3RoZXI=") && !saved.contains("other"),
            "{saved}"
        );
    }

    #[test]
    fn an_added_and_a_removed_image_are_listed_when_they_do_not_pair_up() {
        let two = json!({"kind": "Deployment", "metadata": {"namespace": "d", "name": "w"},
            "spec": {"template": {"spec": {"containers": [{"image": "a:1"}, {"image": "side:1"}]}}}});
        let one = deploy("d", "w", "a:1", 1);
        let mut one = one;
        one["spec"].as_object_mut().unwrap().remove("replicas");
        let d = diff(&inv(vec![two]), &inv(vec![one]));
        let Change::Changed(_, det) = &d[0] else {
            panic!("{d:?}")
        };
        assert!(det.contains(&"image -side:1".to_owned()), "{det:?}");
    }

    #[test]
    fn an_inventory_round_trips_through_its_file() {
        let a = inv(vec![deploy("shop", "web", "nginx:1.25", 2)]);
        assert_eq!(decode(&encode(&a)), Some(a));
        assert_eq!(decode("not json"), None);
        assert_eq!(decode("[]"), None);
    }

    #[test]
    fn the_timeline_says_what_changed_since_the_checkpoint_before() {
        let v1 = inv(vec![deploy("shop", "web", "nginx:1.25", 2)]);
        let v2 = inv(vec![
            deploy("shop", "web", "nginx:1.27", 2),
            deploy("shop", "db", "pg:17", 1),
        ]);
        let rows = vec![
            ("c".to_owned(), "2026-10-27T12:00:00Z".to_owned()),
            ("b".to_owned(), "2026-10-27T11:00:00Z".to_owned()),
            ("a".to_owned(), "2026-10-27T10:00:00Z".to_owned()),
        ];
        let invs: BTreeMap<String, Inventory> =
            [("b".to_owned(), v1.clone()), ("c".to_owned(), v2)].into();
        let lines = timeline_lines(&rows, &invs);
        assert!(lines[0].ends_with("+1 -0 ~1"), "{lines:?}");
        // The checkpoint before has an inventory, but the oldest has none.
        assert!(lines[1].ends_with("1 objects"), "{lines:?}");
        assert!(lines[2].ends_with("no inventory"), "{lines:?}");
    }
}
