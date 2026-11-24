// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl find QUERY [--json]`: look for a name, label (`env=dev`), image reference, kube app or port across machines,
//! images, kube apps and volumes, and print one line per hit with the command that opens it. The same sets, and the
//! same ranking, as the search box in Studio (Cmd/Ctrl-K; `ui/node/src/lib/search.ts`). Read-only: it only lists.

use crate::remote::Remote;
use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::{Value, json};

#[derive(clap::Args, Debug, Clone)]
pub struct FindArgs {
    /// What to look for: part of a name, a label like `env=dev`, an image such as `nginx`, or a port such as `8080` or
    /// `:8080`. Several words must all match.
    #[arg(required = true)]
    pub query: Vec<String>,
    /// Show at most this many hits (default 20).
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub kind: &'static str,
    pub name: String,
    pub detail: String,
    /// Other words that find it: labels, address, image reference, namespace.
    pub terms: Vec<String>,
    pub ports: Vec<u64>,
    /// The command that shows it.
    pub open: String,
    pub score: f64,
}

fn weight(kind: &str) -> u8 {
    match kind {
        "machine" => 5,
        "app" => 4,
        "image" => 3,
        _ => 2,
    }
}

fn word_start(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        i == 0
            || hay[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || "-._/:@=".contains(c))
    })
}

fn subsequence(needle: &str, hay: &str) -> bool {
    let mut n = needle.chars();
    let mut cur = n.next();
    for c in hay.chars() {
        if Some(c) == cur {
            cur = n.next();
        }
    }
    cur.is_none()
}

/// 100 exact, 80 prefix, 60 word start, 40 inside, 15 loosely (3+ letters), 0 not at all.
pub fn match_text(term: &str, text: &str) -> f64 {
    let (t, h) = (term.to_lowercase(), text.to_lowercase());
    if t.is_empty() || h.is_empty() {
        0.0
    } else if h == t {
        100.0
    } else if h.starts_with(&t) {
        80.0
    } else if word_start(&h, &t) {
        60.0
    } else if h.contains(&t) {
        40.0
    } else if t.chars().count() >= 3 && subsequence(&t, &h) {
        15.0
    } else {
        0.0
    }
}

fn port_of(word: &str) -> Option<u64> {
    let w = word.strip_prefix(':').unwrap_or(word);
    (!w.is_empty() && w.len() <= 5 && w.bytes().all(|b| b.is_ascii_digit()))
        .then(|| w.parse().ok())
        .flatten()
}

/// 0 means no match. Every word must match something; the name counts double, other words once.
pub fn score(h: &Hit, words: &[&str]) -> f64 {
    if words.is_empty() {
        return 0.0;
    }
    let mut total = 0.0;
    for w in words {
        let mut best = match_text(w, &h.name) * 2.0;
        for t in &h.terms {
            let k = if t.contains('=') { 1.5 } else { 1.0 };
            best = best.max(match_text(w, t) * k);
        }
        best = best.max(match_text(w, &h.detail) * 0.5);
        if let Some(p) = port_of(w)
            && h.ports.contains(&p)
        {
            best = best.max(if w.starts_with(':') { 190.0 } else { 150.0 });
        }
        if best == 0.0 {
            return 0.0;
        }
        total += best;
    }
    total / words.len() as f64
}

/// Hits that match, best first; equal scores keep the order they were listed in.
pub fn rank(all: Vec<Hit>, query: &str, limit: usize) -> Vec<Hit> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut v: Vec<(usize, Hit)> = all
        .into_iter()
        .enumerate()
        .filter_map(|(n, mut h)| {
            h.score = score(&h, &words);
            (h.score > 0.0).then_some((n, h))
        })
        .collect();
    v.sort_by(|a, b| {
        b.1.score
            .total_cmp(&a.1.score)
            .then(weight(b.1.kind).cmp(&weight(a.1.kind)))
            .then(a.0.cmp(&b.0))
    });
    v.into_iter().take(limit).map(|(_, h)| h).collect()
}

fn word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@=,-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn rows(v: &Value, key: &str) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        _ => v
            .get(key)
            .or_else(|| v.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    }
}

pub fn machines(v: &Value) -> Vec<Hit> {
    rows(v, "items")
        .iter()
        .filter(|m| !s(m, "name").is_empty())
        .map(|m| {
            let name = s(m, "name");
            let mut terms = vec![s(m, "id"), s(m, "guest_ip")];
            if let Some(l) = m.get("labels").and_then(Value::as_object) {
                terms.extend(
                    l.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())),
                );
            }
            terms.retain(|t| !t.is_empty());
            Hit {
                kind: "machine",
                detail: [s(m, "status"), s(m, "guest_ip"), s(m, "backend")]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
                open: format!("fluxctl get {}", word(&name)),
                name,
                terms,
                ports: vec![],
                score: 0.0,
            }
        })
        .collect()
}

pub fn catalog(v: &Value) -> Vec<Hit> {
    rows(v, "items")
        .iter()
        .filter(|r| !s(r, "name").is_empty())
        .map(|r| Hit {
            kind: "image",
            name: s(r, "name"),
            detail: format!(
                "catalog {} {} {}",
                s(r, "format"),
                s(r, "distro"),
                s(r, "version")
            )
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" · "),
            terms: vec![s(r, "distro"), s(r, "version")]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect(),
            ports: vec![],
            open: "fluxctl catalog list".into(),
            score: 0.0,
        })
        .collect()
}

pub fn oci(v: &Value) -> Vec<Hit> {
    rows(v, "items")
        .iter()
        .filter_map(|r| {
            let name = [s(r, "reference"), s(r, "image"), s(r, "name")]
                .into_iter()
                .find(|x| !x.is_empty())?;
            let digest = s(r, "digest");
            let short = digest.trim_start_matches("sha256:");
            Some(Hit {
                kind: "image",
                detail: format!("OCI image · {}", &short[..short.len().min(12)]),
                terms: if short.is_empty() {
                    vec![]
                } else {
                    vec![short.to_string()]
                },
                name,
                ports: vec![],
                open: "fluxctl oci ls".into(),
                score: 0.0,
            })
        })
        .collect()
}

pub fn volumes(v: &Value) -> Vec<Hit> {
    rows(v, "items")
        .iter()
        .filter(|r| !s(r, "name").is_empty())
        .map(|r| {
            let used: Vec<String> = r
                .get("attached_to")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Hit {
                kind: "volume",
                name: s(r, "name"),
                detail: format!(
                    "{} GiB · {}",
                    s(r, "size_gib"),
                    if used.is_empty() {
                        "unused".to_string()
                    } else {
                        format!("used by {}", used.join(", "))
                    }
                ),
                terms: used,
                ports: vec![],
                open: "Studio: Volumes page (REST: GET /v1/volumes)".into(),
                score: 0.0,
            }
        })
        .collect()
}

pub fn apps(cluster: &str, v: &Value) -> Vec<Hit> {
    rows(v, "apps")
        .iter()
        .filter(|a| !s(a, "name").is_empty())
        .map(|a| {
            let port = a.get("port").and_then(Value::as_u64);
            Hit {
                kind: "app",
                name: s(a, "name"),
                detail: [
                    s(a, "summary"),
                    s(a, "image"),
                    port.map(|p| format!("port {p}")).unwrap_or_default(),
                ]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
                terms: [s(a, "image"), s(a, "namespace"), cluster.to_string()]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect(),
                ports: port.into_iter().collect(),
                open: format!("fluxctl kube apps --cluster {}", word(cluster)),
                score: 0.0,
            }
        })
        .collect()
}

/// What to say when a set could not be read: what happened and what to do.
pub fn problem(set: &str, err: &anyhow::Error) -> String {
    let m = format!("{err:#}");
    let hint = if m.contains("401") || m.contains("403") {
        "your token may not read this. Pass an admin token with --server-token"
    } else if m.contains("404") {
        "this daemon does not have it (an older release?)"
    } else if m.contains("error sending request") || m.contains("onnection") {
        "the daemon did not answer. Is FluxVM running? Try `fluxctl doctor`, or point at it with --server"
    } else {
        "run `fluxctl doctor` to see what is wrong"
    };
    format!(
        "could not read {set}: {hint}  ({})",
        m.lines().next().unwrap_or("")
    )
}

async fn get(r: &Remote, set: &str, path: &str) -> Result<Value, String> {
    r.call(Method::GET, path, None)
        .await
        .map_err(|e| problem(set, &e))
}

/// The one line per hit: kind, name, detail, then the command.
pub fn render(hits: &[Hit], query: &str, unreadable: &[String]) -> String {
    let mut out = String::new();
    if hits.is_empty() {
        out.push_str(&format!(
            "Nothing matches \"{query}\". Try part of a name, a label like env=dev, an image such as nginx, or a port number.\n"
        ));
    }
    let w = hits
        .iter()
        .map(|h| h.name.chars().count())
        .max()
        .unwrap_or(0);
    for h in hits {
        let detail = if h.detail.is_empty() {
            String::new()
        } else {
            format!("  {}", h.detail)
        };
        out.push_str(&format!(
            "{:<8} {:<w$}{detail}\n         open: {}\n",
            h.kind, h.name, h.open
        ));
    }
    for u in unreadable {
        out.push_str(&format!("note: {u}\n"));
    }
    out
}

pub fn to_json(hits: &[Hit], query: &str, unreadable: &[String]) -> Value {
    json!({
        "query": query,
        "hits": hits.iter().map(|h| json!({
            "kind": h.kind, "name": h.name, "detail": h.detail, "open": h.open, "score": (h.score * 10.0).round() / 10.0,
        })).collect::<Vec<_>>(),
        "unreadable": unreadable,
    })
}

pub async fn run(r: &Remote, a: &FindArgs) -> Result<()> {
    let query = a.query.join(" ");
    if query.trim().is_empty() {
        bail!(
            "give something to look for, for example `fluxctl find web`, `fluxctl find env=dev` or `fluxctl find :8080`"
        );
    }
    let mut all: Vec<Hit> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let mut ok = 0;
    macro_rules! set {
        ($name:expr, $path:expr, $f:expr) => {
            match get(r, $name, $path).await {
                Ok(v) => {
                    ok += 1;
                    all.extend($f(&v));
                }
                Err(e) => unreadable.push(e),
            }
        };
    }
    set!("machines", "/v1/vms", machines);
    set!("catalog images", "/v1/images/catalog", catalog);
    set!("OCI images", "/v1/oci/images", oci);
    set!("volumes", "/v1/volumes", volumes);
    match get(r, "kube apps", "/v1/kube").await {
        Ok(v) => {
            ok += 1;
            for c in rows(&v, "items").iter().take(8) {
                let name = s(c, "name");
                if name.is_empty() {
                    continue;
                }
                let path = format!("/v1/kube/{}/apps", crate::remote::encode_query(&name));
                match get(r, "kube apps", &path).await {
                    Ok(v) => all.extend(apps(&name, &v)),
                    Err(e) => unreadable.push(e),
                }
            }
        }
        Err(e) => unreadable.push(e),
    }
    if ok == 0 {
        bail!(
            "could not search: {}\n  Start FluxVM, or point at a running one with --server URL --server-token TOKEN",
            unreadable.first().cloned().unwrap_or_default()
        );
    }
    let hits = rank(all, &query, a.limit.max(1));
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&to_json(&hits, &query, &unreadable))?
        );
    } else {
        print!("{}", render(&hits, &query, &unreadable));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Hit> {
        let mut v = machines(&json!({"items": [
            {"id": "aaaa1111", "name": "web-1", "status": "running", "guest_ip": "10.0.0.5", "labels": {"env": "dev", "team": "blue"}},
            {"id": "bbbb2222", "name": "db", "status": "stopped", "labels": {"env": "prod"}},
        ]}));
        v.extend(apps(
            "dev",
            &json!({"apps": [{"namespace": "default", "name": "web", "image": "nginx:1.27", "port": 8080, "summary": "2 of 2 running"}]}),
        ));
        v.extend(catalog(
            &json!({"items": [{"name": "debian-13", "format": "qcow2", "distro": "debian", "version": "13"}]}),
        ));
        v.extend(oci(
            &json!([{"reference": "docker.io/library/alpine:3.22", "digest": "sha256:abcdef0123456789"}]),
        ));
        v.extend(volumes(
            &json!([{"name": "pg-data", "size_gib": 50, "attached_to": ["db"]}]),
        ));
        v
    }

    fn names(q: &str) -> Vec<String> {
        rank(all(), q, 20).into_iter().map(|h| h.name).collect()
    }

    #[test]
    fn matches_are_ordered_exact_prefix_word_inside_loose() {
        assert_eq!(match_text("web", "web"), 100.0);
        assert_eq!(match_text("we", "web-1"), 80.0);
        assert_eq!(match_text("1", "web-1"), 60.0);
        assert_eq!(match_text("eb", "web-1"), 40.0);
        assert_eq!(match_text("wb1", "web-1"), 15.0);
        assert_eq!(match_text("wb", "web-1"), 0.0);
        assert_eq!(match_text("zzz", "web-1"), 0.0);
        assert_eq!(match_text("WEB", "web"), 100.0);
    }

    #[test]
    fn exact_name_comes_before_prefix() {
        assert_eq!(&names("web")[..2], ["web", "web-1"]);
    }

    #[test]
    fn finds_by_label_image_port_address_and_user() {
        assert_eq!(names("env=dev"), ["web-1"]);
        assert_eq!(names("nginx"), ["web"]);
        assert_eq!(names("8080"), ["web"]);
        assert_eq!(names(":8080"), ["web"]);
        assert!(names("9999").is_empty());
        assert_eq!(names("10.0.0.5"), ["web-1"]);
        assert_eq!(names("pg")[0], "pg-data");
        assert_eq!(names("alpine"), ["docker.io/library/alpine:3.22"]);
    }

    #[test]
    fn every_word_must_match() {
        assert_eq!(names("web blue"), ["web-1"]);
        assert!(names("web prod").is_empty());
    }

    #[test]
    fn limit_and_stable_order() {
        let many = machines(
            &json!({"items": (0..10).map(|i| json!({"id": i.to_string(), "name": format!("node-{i}")})).collect::<Vec<_>>()}),
        );
        let got: Vec<String> = rank(many, "node", 3).into_iter().map(|h| h.name).collect();
        assert_eq!(got, ["node-0", "node-1", "node-2"]);
    }

    #[test]
    fn each_hit_says_how_to_open_it() {
        let hits = rank(all(), "web", 20);
        assert!(hits.iter().any(|h| h.open == "fluxctl get web-1"));
        assert!(
            hits.iter()
                .any(|h| h.open == "fluxctl kube apps --cluster dev")
        );
        assert_eq!(word("my vm's"), "'my vm'\\''s'");
    }

    #[test]
    fn empty_result_and_unreadable_sets_are_explained() {
        let t = render(&[], "zzz", &["could not read volumes: x".into()]);
        assert!(t.contains("Nothing matches \"zzz\""));
        assert!(t.contains("note: could not read volumes"));
        let e = anyhow::anyhow!("GET /v1/volumes: 403 Forbidden: no");
        assert!(problem("volumes", &e).contains("--server-token"));
        let e = anyhow::anyhow!("error sending request for url");
        assert!(problem("machines", &e).contains("Is FluxVM running"));
    }

    #[test]
    fn json_has_the_open_command() {
        let v = to_json(&rank(all(), "db", 5), "db", &[]);
        assert_eq!(v["hits"][0]["name"], "db");
        assert_eq!(v["hits"][0]["open"], "fluxctl get db");
        assert_eq!(v["query"], "db");
    }
}
