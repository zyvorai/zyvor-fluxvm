// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! One-command stacks shipped inside the binary: a few Compose files and k3s manifests that are worth starting with
//! (`fluxctl templates ls | show | run NAME`). The data lives here so `fluxctl` and the daemon's Studio gallery
//! (`GET /v1/templates`) list exactly the same thing. Nothing here runs anything.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A Compose file, started with `docker compose` against the Docker engine.
    Compose,
    /// Kubernetes manifests, applied with `kubectl apply` to a FluxVM k3s cluster.
    K3s,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Compose => "compose",
            Kind::K3s => "k3s",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Template {
    pub name: &'static str,
    pub kind: Kind,
    pub title: &'static str,
    pub summary: &'static str,
    /// Where to look once it is up.
    pub after: &'static str,
    /// The file as it is written to disk and handed to `docker compose` or `kubectl`.
    pub body: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        name: "postgres-adminer",
        kind: Kind::Compose,
        title: "Postgres and Adminer",
        summary: "Postgres 17 on 127.0.0.1:5432 (user dev, database app) with the Adminer web page, data kept on a volume.",
        after: "Adminer: http://127.0.0.1:8080  (server db, user dev, password fluxvm-dev)",
        body: include_str!("../templates/postgres-adminer.yaml"),
    },
    Template {
        name: "redis",
        kind: Kind::Compose,
        title: "Redis",
        summary: "Redis 7 on 127.0.0.1:6379 with its data kept on a volume.",
        after: "redis-cli -h 127.0.0.1 ping",
        body: include_str!("../templates/redis.yaml"),
    },
    Template {
        name: "mailpit",
        kind: Kind::Compose,
        title: "Mailpit mail catcher",
        summary: "Catches the mail your app sends (SMTP on 127.0.0.1:1025) and shows it in a web page.",
        after: "Mail: http://127.0.0.1:8025",
        body: include_str!("../templates/mailpit.yaml"),
    },
    Template {
        name: "nginx-hello",
        kind: Kind::K3s,
        title: "nginx, three copies",
        summary: "A Deployment of three nginx pods behind a Service on your FluxVM k3s cluster.",
        after: "kubectl port-forward svc/fluxvm-nginx-hello 8081:80, then http://127.0.0.1:8081",
        body: include_str!("../templates/nginx-hello.yaml"),
    },
    Template {
        name: "whoami",
        kind: Kind::K3s,
        title: "whoami",
        summary: "Two pods that answer with their own name, to watch a Service balance requests.",
        after: "kubectl port-forward svc/fluxvm-whoami 8082:80, then http://127.0.0.1:8082",
        body: include_str!("../templates/whoami.yaml"),
    },
];

pub fn find(name: &str) -> Option<&'static Template> {
    let n = name.trim().to_ascii_lowercase();
    TEMPLATES.iter().find(|t| t.name == n)
}

/// The gallery as JSON, the same for `fluxctl templates ls --json` and `GET /v1/templates`.
pub fn gallery_json() -> serde_json::Value {
    let items: Vec<_> = TEMPLATES
        .iter()
        .map(|t| {
            let run = format!("fluxctl templates run {}", t.name);
            serde_json::json!({
                "name": t.name, "kind": t.kind, "title": t.title, "summary": t.summary,
                "after": t.after, "run": run, "show": format!("fluxctl templates show {}", t.name),
            })
        })
        .collect();
    serde_json::json!({ "schema": 1, "templates": items })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_bodies_are_not_empty() {
        let mut seen = std::collections::HashSet::new();
        for t in TEMPLATES {
            assert!(seen.insert(t.name), "{}", t.name);
            assert!(t.body.contains(':') && !t.after.is_empty());
        }
        assert!(TEMPLATES.iter().any(|t| t.kind == Kind::Compose));
        assert!(TEMPLATES.iter().any(|t| t.kind == Kind::K3s));
    }

    #[test]
    fn find_is_exact_and_case_insensitive() {
        assert_eq!(find(" Redis ").unwrap().name, "redis");
        assert!(find("red").is_none());
    }

    #[test]
    fn the_gallery_names_the_command_to_run_each() {
        let g = gallery_json();
        assert_eq!(
            g["templates"][0]["run"],
            "fluxctl templates run postgres-adminer"
        );
    }
}
