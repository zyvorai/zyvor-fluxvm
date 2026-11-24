// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Apps on a FluxVM Kubernetes cluster, the Docker way: an image becomes a Deployment and a Service, and what the cluster says
//! about them is turned into one plain sentence per app. Shared by `fluxctl kube run | apps | scale | rm ...` and by the Studio
//! Apps page, so both build the same objects and say the same thing. Pure functions over `serde_json::Value`; nothing here
//! talks to a cluster.
//!
//! Everything an app creates carries the label `app.kubernetes.io/managed-by=fluxctl-app`. Listing, scaling and removing only
//! ever look at objects with that label, so an app can never be confused with, or delete, something the user made by hand.

use serde_json::{Value, json};

pub const MANAGED_BY_KEY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY: &str = "fluxctl-app";
pub const NAME_KEY: &str = "app.kubernetes.io/name";
/// The label selector that finds every app.
pub const SELECTOR: &str = "app.kubernetes.io/managed-by=fluxctl-app";
pub const MAX_REPLICAS: u32 = 100;

/// A Kubernetes DNS label: lowercase letters, digits and `-`, at most 63 characters, alphanumeric at both ends.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// An image reference as people type it: no spaces or control characters, no leading `-` (which would read as an option).
pub fn image_ok(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 255
        && !image.starts_with('-')
        && image
            .bytes()
            .all(|c| c.is_ascii_graphic() && c != b'\'' && c != b'"' && c != b'\\')
}

/// The app name for an image: `ghcr.io/acme/Web-App:1.2` is `web-app`.
pub fn default_name(image: &str) -> String {
    let no_digest = image.split('@').next().unwrap_or(image);
    let last = no_digest.rsplit('/').next().unwrap_or(no_digest);
    let base = last.split(':').next().unwrap_or(last);
    let mut name: String = base
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    name = name.trim_matches('-').to_owned();
    name.truncate(63);
    let name = name.trim_matches('-').to_owned();
    if name.is_empty() {
        "app".to_owned()
    } else {
        name
    }
}

/// The port an image listens on when the user did not say: the well-known ones, otherwise 80.
pub fn default_port(image: &str) -> u16 {
    let no_digest = image.split('@').next().unwrap_or(image);
    let last = no_digest.rsplit('/').next().unwrap_or(no_digest);
    match last.split(':').next().unwrap_or(last) {
        "grafana" => 3000,
        "redis" | "valkey" => 6379,
        "postgres" => 5432,
        "mysql" | "mariadb" => 3306,
        "mongo" => 27017,
        "node" | "ghost" | "gitea" => 3000,
        "traefik" | "whoami" => 80,
        "jenkins" => 8080,
        "registry" => 5000,
        "prometheus" => 9090,
        _ => 80,
    }
}

/// What to run.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub name: String,
    pub namespace: String,
    pub image: String,
    pub port: u16,
    pub replicas: u32,
    pub env: Vec<(String, String)>,
    /// Your own labels (`kube run --label team=ios`): on the Deployment and the Service, shown by `kube apps` and filtered with
    /// `kube apps --filter label=team=ios`. Not on the pods, so they never change what the Deployment selects.
    pub labels: Vec<(String, String)>,
}

/// Why a user label cannot be used, or `None`. Keys follow Kubernetes (letters, digits, `-`, `_`, `.`, at most 63) and may not
/// start with `app.kubernetes.io/` or `fluxvm`, which fluxctl keeps for itself.
pub fn label_problem(k: &str, v: &str) -> Option<String> {
    let part = |s: &str| {
        s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && s.chars().next().is_none_or(|c| c.is_ascii_alphanumeric())
            && s.chars().last().is_none_or(|c| c.is_ascii_alphanumeric())
    };
    if k.is_empty() || !part(k) {
        return Some(format!(
            "{k:?} is not a label name. Use letters, digits, dashes, underscores and dots, as in --label team=ios"
        ));
    }
    if !part(v) {
        return Some(format!(
            "the value {v:?} of label {k} is not usable. Use letters, digits, dashes, underscores and dots (at most 63)"
        ));
    }
    if k.starts_with("app.kubernetes.io/") || k.starts_with("fluxvm") {
        return Some(format!(
            "label {k:?} is kept by fluxctl. Choose another name, for example team or env"
        ));
    }
    None
}

/// Checks a [`Spec`] before anything is sent anywhere; the error is a sentence saying what to change.
pub fn validate(s: &Spec) -> Result<(), String> {
    if !name_ok(&s.name) {
        return Err(format!(
            "{:?} is not a usable app name. Use lowercase letters, digits and dashes (for example `web`), or choose one with --name",
            s.name
        ));
    }
    if !name_ok(&s.namespace) {
        return Err(format!(
            "{:?} is not a namespace name. Use lowercase letters, digits and dashes",
            s.namespace
        ));
    }
    if !image_ok(&s.image) {
        return Err(format!(
            "{:?} is not an image name. Give one like nginx or ghcr.io/you/app:1.0",
            s.image
        ));
    }
    if s.port == 0 {
        return Err("the port must be between 1 and 65535".to_owned());
    }
    if s.replicas > MAX_REPLICAS {
        return Err(format!(
            "an app can have at most {MAX_REPLICAS} copies; give a smaller number"
        ));
    }
    for (k, v) in &s.labels {
        if let Some(p) = label_problem(k, v) {
            return Err(p);
        }
    }
    for (k, _) in &s.env {
        let mut c = k.chars();
        let ok = c
            .next()
            .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
            && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if !ok {
            return Err(format!(
                "{k:?} is not an environment variable name. Use letters, digits and underscores, as in --env PORT=8080"
            ));
        }
    }
    Ok(())
}

fn labels(name: &str) -> Value {
    json!({NAME_KEY: name, MANAGED_BY_KEY: MANAGED_BY})
}

/// The labels of the Deployment and the Service: fluxctl's, then the user's.
fn object_labels(s: &Spec) -> Value {
    let mut l = labels(&s.name);
    for (k, v) in &s.labels {
        l[k.as_str()] = json!(v);
    }
    l
}

/// The labels a person set: everything except the `app.kubernetes.io/` and `fluxvm` keys.
fn user_labels(meta: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(o) = meta["labels"].as_object() {
        for (k, v) in o {
            if !k.starts_with("app.kubernetes.io/") && !k.starts_with("fluxvm") {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// The Deployment and the Service for an app. The Service is a ClusterIP on the app's port; `kube forward` and `kube expose` make
/// it reachable from this machine.
pub fn manifests(s: &Spec) -> Vec<Value> {
    let env: Vec<Value> = s
        .env
        .iter()
        .map(|(k, v)| json!({"name": k, "value": v}))
        .collect();
    vec![
        json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": {"name": s.name, "namespace": s.namespace, "labels": object_labels(s),
                         "annotations": {"fluxvm.dev/image": s.image}},
            "spec": {
                "replicas": s.replicas,
                "selector": {"matchLabels": {NAME_KEY: s.name}},
                "template": {
                    "metadata": {"labels": labels(&s.name)},
                    "spec": {"containers": [{
                        "name": s.name,
                        "image": s.image,
                        "ports": [{"containerPort": s.port}],
                        "env": env,
                    }]}
                }
            }
        }),
        json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": {"name": s.name, "namespace": s.namespace, "labels": object_labels(s)},
            "spec": {
                "selector": {NAME_KEY: s.name},
                "ports": [{"name": "http", "port": s.port, "targetPort": s.port}]
            }
        }),
    ]
}

/// True when the object carries our label.
pub fn is_managed(obj: &Value) -> bool {
    obj["metadata"]["labels"][MANAGED_BY_KEY] == MANAGED_BY
}

/// Why an app is not running, with the explain code that says more.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub code: &'static str,
    pub summary: String,
    /// The command to run next.
    pub next: String,
}

/// What a pod shows in `kubectl get pods`: a container's waiting or failed reason, else the phase.
pub fn pod_status(pod: &Value) -> String {
    let statuses = pod["status"]["containerStatuses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for c in &statuses {
        if let Some(r) = c["state"]["waiting"]["reason"].as_str() {
            return r.to_owned();
        }
    }
    for c in &statuses {
        if let Some(r) = c["state"]["terminated"]["reason"].as_str()
            && r != "Completed"
        {
            return r.to_owned();
        }
        if c["lastState"]["terminated"]["reason"] == "OOMKilled" && c["ready"] != true {
            return "OOMKilled".to_owned();
        }
    }
    if pod["status"]["phase"] == "Pending"
        && pod["status"]["conditions"].as_array().is_some_and(|cs| {
            cs.iter()
                .any(|c| c["type"] == "PodScheduled" && c["status"] == "False")
        })
    {
        return "Unschedulable".to_owned();
    }
    pod["status"]["phase"]
        .as_str()
        .unwrap_or("Unknown")
        .to_owned()
}

/// The plain-English problem behind a pod status, if it is one. `image` and `name` are the app's, for the sentence.
pub fn problem_for(status: &str, name: &str, image: &str, namespace: &str) -> Option<Problem> {
    let ns = if namespace == "default" {
        String::new()
    } else {
        format!(" -n {namespace}")
    };
    Some(match status {
        "ImagePullBackOff" | "ErrImagePull" | "InvalidImageName" => Problem {
            code: "APP_IMAGE_PULL",
            summary: format!(
                "The cluster cannot download the image {image}. The name or tag may be wrong, or the image is private or only on this machine."
            ),
            next: format!(
                "check the name with `docker pull {image}`; for an image built on this machine run `fluxctl kube load {image}` and then `fluxctl kube run {image} --name {name}` again; to remove the app run `fluxctl kube rm {name}{ns}`"
            ),
        },
        "CrashLoopBackOff" | "Error" | "RunContainerError" | "CreateContainerError" => Problem {
            code: "APP_CRASHING",
            summary: format!("{name} starts and then stops, so the cluster keeps restarting it."),
            next: format!("read why with `fluxctl kube logs {name} --previous{ns}`"),
        },
        "OOMKilled" => Problem {
            code: "APP_OUT_OF_MEMORY",
            summary: format!("{name} ran out of memory and was stopped."),
            next: "give the cluster more memory with `fluxctl kube up --memory-mib 8192`, or run fewer copies with `fluxctl kube scale`".to_owned(),
        },
        "CreateContainerConfigError" => Problem {
            code: "APP_CRASHING",
            summary: format!("{name} cannot start because its settings are wrong (a missing secret or config)."),
            next: format!("see the exact reason with `kubectl describe pod -l app.kubernetes.io/name={name}{ns}`"),
        },
        "Unschedulable" => Problem {
            code: "APP_NOT_READY",
            summary: format!("No machine in the cluster has room for {name}."),
            next: "give the cluster more memory or CPU (`fluxctl kube up --memory-mib 8192`), or run fewer copies".to_owned(),
        },
        _ => return None,
    })
}

fn items(list: &Value) -> Vec<&Value> {
    list["items"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// One row per app, from the raw `get deployments`, `get services` and `get pods` lists (all labelled with [`SELECTOR`], in any
/// namespace). `state` is `running`, `starting`, `stopped` or `problem`; `summary` is the sentence to show and, for a problem,
/// `code` and `next` say what to do.
pub fn summarize(deployments: &Value, services: &Value, pods: &Value) -> Vec<Value> {
    let mut rows: Vec<Value> = items(deployments)
        .into_iter()
        .filter(|d| is_managed(d))
        .map(|d| {
            let ns = d["metadata"]["namespace"].as_str().unwrap_or("default");
            let name = d["metadata"]["name"].as_str().unwrap_or("");
            let image = d["spec"]["template"]["spec"]["containers"][0]["image"]
                .as_str()
                .unwrap_or("");
            let desired = d["spec"]["replicas"].as_u64().unwrap_or(1);
            let ready = d["status"]["readyReplicas"].as_u64().unwrap_or(0);
            let svc = items(services).into_iter().find(|s| {
                s["metadata"]["namespace"].as_str() == Some(ns)
                    && s["metadata"]["name"].as_str() == Some(name)
            });
            let port = svc
                .and_then(|s| s["spec"]["ports"][0]["port"].as_u64())
                .or_else(|| {
                    d["spec"]["template"]["spec"]["containers"][0]["ports"][0]["containerPort"]
                        .as_u64()
                });
            let my_pods: Vec<&Value> = items(pods)
                .into_iter()
                .filter(|p| {
                    p["metadata"]["namespace"].as_str() == Some(ns)
                        && p["metadata"]["labels"][NAME_KEY].as_str() == Some(name)
                })
                .collect();
            let pod_rows: Vec<Value> = my_pods
                .iter()
                .map(|p| {
                    let cs = p["status"]["containerStatuses"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let want = p["spec"]["containers"].as_array().map_or(0, Vec::len);
                    let have = cs.iter().filter(|c| c["ready"] == true).count();
                    json!({
                        "namespace": ns,
                        "name": p["metadata"]["name"],
                        "containers": p["spec"]["containers"].as_array().map(|a| a.iter().filter_map(|c| c["name"].as_str()).collect::<Vec<_>>()).unwrap_or_default(),
                        "status": pod_status(p),
                        "phase": p["status"]["phase"],
                        "ready": format!("{have}/{want}"),
                        "restarts": cs.iter().map(|c| c["restartCount"].as_u64().unwrap_or(0)).sum::<u64>(),
                        "node": p["spec"]["nodeName"],
                        "pod_ip": p["status"]["podIP"],
                        "created": p["metadata"]["creationTimestamp"],
                    })
                })
                .collect();
            let problem = if desired == 0 {
                None
            } else {
                my_pods
                    .iter()
                    .find_map(|p| problem_for(&pod_status(p), name, image, ns))
            };
            let (state, summary) = if desired == 0 {
                ("stopped", format!("{name} is stopped (0 copies). Start it with `fluxctl kube scale {name} 1`."))
            } else if let Some(p) = &problem {
                ("problem", p.summary.clone())
            } else if ready >= desired {
                ("running", format!("{ready} of {desired} running"))
            } else {
                ("starting", format!("starting, {ready} of {desired} ready"))
            };
            json!({
                "namespace": ns,
                "name": name,
                "image": image,
                "desired": desired,
                "ready": ready,
                "port": port,
                "state": state,
                "summary": summary,
                "code": problem.as_ref().map(|p| p.code),
                "next": problem.as_ref().map(|p| p.next.clone()),
                "pods": pod_rows,
                "created": d["metadata"]["creationTimestamp"],
                "labels": user_labels(&d["metadata"]),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        (a["namespace"].as_str(), a["name"].as_str())
            .cmp(&(b["namespace"].as_str(), b["name"].as_str()))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            name: "web".into(),
            namespace: "default".into(),
            image: "nginx:1.27".into(),
            port: 80,
            replicas: 1,
            env: vec![("MODE".into(), "dev".into())],
            labels: vec![],
        }
    }

    #[test]
    fn user_labels_go_on_the_deployment_and_service_but_never_on_the_pods() {
        let mut s = spec();
        s.labels = vec![("team".into(), "ios".into())];
        validate(&s).unwrap();
        let m = manifests(&s);
        assert_eq!(m[0]["metadata"]["labels"]["team"], "ios");
        assert_eq!(m[1]["metadata"]["labels"]["team"], "ios");
        assert_eq!(m[0]["metadata"]["labels"][MANAGED_BY_KEY], MANAGED_BY);
        assert!(m[0]["spec"]["template"]["metadata"]["labels"]["team"].is_null());
        assert!(m[0]["spec"]["selector"]["matchLabels"]["team"].is_null());
    }

    #[test]
    fn bad_and_reserved_labels_say_what_to_change() {
        for (k, v) in [
            ("", "x"),
            ("a b", "x"),
            ("team", "has space"),
            ("app.kubernetes.io/name", "x"),
            ("fluxvm.io/x", "y"),
        ] {
            let mut s = spec();
            s.labels = vec![(k.into(), v.into())];
            assert!(validate(&s).unwrap_err().contains("label"), "{k}={v}");
        }
    }

    #[test]
    fn rows_list_only_the_labels_a_person_set() {
        let d = json!({"items": [{"metadata": {"name": "web", "namespace": "default",
            "labels": {"app.kubernetes.io/managed-by": "fluxctl-app", "app.kubernetes.io/name": "web", "team": "ios"}},
            "spec": {"replicas": 1, "template": {"spec": {"containers": [{"image": "nginx"}]}}}, "status": {}}]});
        let rows = summarize(&d, &json!({"items": []}), &json!({"items": []}));
        assert_eq!(rows[0]["labels"], json!({"team": "ios"}));
    }

    #[test]
    fn names_come_from_images() {
        assert_eq!(default_name("nginx"), "nginx");
        assert_eq!(default_name("ghcr.io/acme/Web_App:1.2"), "web-app");
        assert_eq!(default_name("localhost:5000/team/api@sha256:abcd"), "api");
        assert_eq!(default_name("---"), "app");
        assert_eq!(default_port("grafana/grafana:11"), 3000);
        assert_eq!(default_port("nginx"), 80);
    }

    #[test]
    fn bad_input_is_refused_with_a_sentence() {
        assert!(validate(&spec()).is_ok());
        let mut s = spec();
        s.name = "Web App".into();
        assert!(validate(&s).unwrap_err().contains("--name"));
        let mut s = spec();
        s.image = "--privileged".into();
        assert!(validate(&s).is_err());
        let mut s = spec();
        s.env = vec![("A-B".into(), "x".into())];
        assert!(validate(&s).unwrap_err().contains("--env"));
        let mut s = spec();
        s.replicas = 101;
        assert!(validate(&s).is_err());
    }

    #[test]
    fn manifests_are_labelled_and_match_each_other() {
        let m = manifests(&spec());
        assert_eq!(m.len(), 2);
        assert!(m.iter().all(is_managed));
        assert_eq!(
            m[0]["spec"]["selector"]["matchLabels"],
            m[1]["spec"]["selector"]
        );
        assert_eq!(
            m[0]["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"],
            "dev"
        );
        assert_eq!(m[1]["spec"]["ports"][0]["port"], 80);
    }

    fn dep(name: &str, desired: u64, ready: u64, managed: bool) -> Value {
        let labels = if managed { labels(name) } else { json!({}) };
        json!({"metadata": {"namespace": "default", "name": name, "labels": labels},
               "spec": {"replicas": desired, "template": {"spec": {"containers": [{"name": name, "image": "nginx"}]}}},
               "status": {"readyReplicas": ready}})
    }
    fn pod(name: &str, app: &str, waiting: Option<&str>) -> Value {
        let state = waiting.map_or(
            json!({"running": {}}),
            |r| json!({"waiting": {"reason": r}}),
        );
        json!({"metadata": {"namespace": "default", "name": name, "labels": {NAME_KEY: app}},
               "spec": {"containers": [{"name": app}]},
               "status": {"phase": "Running", "containerStatuses": [{"ready": waiting.is_none(), "restartCount": 2, "state": state}]}})
    }

    #[test]
    fn rows_say_running_starting_stopped_and_what_is_wrong() {
        let deps = json!({"items": [dep("ok", 2, 2, true), dep("slow", 1, 0, true), dep("off", 0, 0, true), dep("pull", 1, 0, true), dep("hand", 1, 1, false)]});
        let svcs = json!({"items": [{"metadata": {"namespace": "default", "name": "ok"}, "spec": {"ports": [{"port": 8080}]}}]});
        let pods = json!({"items": [pod("ok-1", "ok", None), pod("slow-1", "slow", Some("ContainerCreating")), pod("pull-1", "pull", Some("ImagePullBackOff"))]});
        let rows = summarize(&deps, &svcs, &pods);
        let by = |n: &str| rows.iter().find(|r| r["name"] == n).unwrap().clone();
        assert_eq!(rows.len(), 4, "a Deployment made by hand is never listed");
        assert_eq!(by("ok")["state"], "running");
        assert_eq!(by("ok")["port"], 8080);
        assert_eq!(by("slow")["state"], "starting");
        assert_eq!(by("off")["state"], "stopped");
        let p = by("pull");
        assert_eq!(p["state"], "problem");
        assert_eq!(p["code"], "APP_IMAGE_PULL");
        assert!(p["next"].as_str().unwrap().contains("fluxctl kube load"));
        assert_eq!(by("ok")["pods"][0]["restarts"], 2);
    }

    #[test]
    fn pod_status_reads_waiting_oom_and_unschedulable() {
        assert_eq!(
            pod_status(&pod("p", "a", Some("CrashLoopBackOff"))),
            "CrashLoopBackOff"
        );
        let oom = json!({"status": {"phase": "Running", "containerStatuses": [{"ready": false, "state": {"running": {}}, "lastState": {"terminated": {"reason": "OOMKilled"}}}]}});
        assert_eq!(pod_status(&oom), "OOMKilled");
        let un = json!({"status": {"phase": "Pending", "conditions": [{"type": "PodScheduled", "status": "False"}]}});
        assert_eq!(pod_status(&un), "Unschedulable");
        assert!(problem_for("Running", "a", "i", "default").is_none());
    }

    #[test]
    fn every_problem_code_is_explained() {
        for s in [
            "ImagePullBackOff",
            "CrashLoopBackOff",
            "OOMKilled",
            "Unschedulable",
        ] {
            let p = problem_for(s, "a", "i", "default").unwrap();
            assert!(
                crate::health::code(p.code).is_some(),
                "{} is not in the explain table",
                p.code
            );
        }
    }
}
