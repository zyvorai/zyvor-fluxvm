//! `fluxctl kube pin`: the images the cluster is running right now, each as `name:tag@sha256:...`. A tag such as
//! `postgres:17-alpine` can point at a different image next month; the digest cannot. Paste the output into a manifest
//! (or a `kube start` project) and the same bytes run on every machine, every time.

use crate::{
    kube_host::{self, KubeOptions},
    run::VmApi,
};
use anyhow::Result;
use std::collections::BTreeSet;

/// One line per container: `namespace image imageID`.
const SCRIPT: &str = "sudo k3s kubectl get pods -A -o go-template='{{range .items}}{{$ns := .metadata.namespace}}{{range .status.containerStatuses}}{{$ns}} {{.image}} {{.imageID}}{{\"\\n\"}}{{end}}{{end}}'";

/// `docker.io/library/postgres@sha256:ab..` -> `sha256:ab..`
fn digest_of(image_id: &str) -> Option<&str> {
    let (_, d) = image_id.rsplit_once('@')?;
    let hex = d.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit())).then_some(d)
}

/// The sorted, de-duplicated pinned references. System namespaces are left out unless `all`.
pub fn parse(text: &str, all: bool) -> Vec<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(ns), Some(image), Some(id)) = (f.next(), f.next(), f.next()) else {
            continue; // a container that has not pulled yet has no imageID
        };
        if !all && ns == "kube-system" {
            continue;
        }
        let Some(digest) = digest_of(id) else {
            continue;
        };
        // An image already pinned in the manifest keeps its own digest.
        let name = image.split('@').next().unwrap_or(image);
        out.insert(format!("{name}@{digest}"));
    }
    out.into_iter().collect()
}

pub async fn run<A: VmApi>(api: &A, o: &KubeOptions, all: bool) -> Result<Vec<String>> {
    Ok(parse(&kube_host::guest_sh(api, o, SCRIPT).await?, all))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
    const D2: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn a_tag_and_a_digest_make_a_pinned_reference() {
        let t = format!(
            "default docker.io/library/postgres:17-alpine docker.io/library/postgres@{D1}\n\
             default docker.io/library/postgres:17-alpine docker.io/library/postgres@{D1}\n\
             shop ghcr.io/acme/web:1.2 ghcr.io/acme/web@{D2}\n"
        );
        assert_eq!(
            parse(&t, false),
            vec![
                format!("docker.io/library/postgres:17-alpine@{D1}"),
                format!("ghcr.io/acme/web:1.2@{D2}"),
            ]
        );
    }

    #[test]
    fn system_pods_and_unpulled_or_odd_lines_are_skipped() {
        let t = format!(
            "kube-system rancher/coredns:1.12 rancher/coredns@{D1}\n\
             default app:dev\n\
             default app:dev sha256:short\n\
             default app:dev app@sha256:zz\n\
             \n\
             default app:dev docker.io/library/app@{D2}\n"
        );
        assert_eq!(parse(&t, false), vec![format!("app:dev@{D2}")]);
        assert_eq!(parse(&t, true).len(), 2, "--all keeps kube-system");
    }

    #[test]
    fn an_image_already_pinned_keeps_one_digest() {
        let t = format!("default web:1@{D1} web@{D2}\n");
        assert_eq!(parse(&t, false), vec![format!("web:1@{D2}")]);
    }

    #[test]
    fn the_script_asks_for_one_line_per_container() {
        assert!(SCRIPT.contains("get pods -A") && SCRIPT.contains(".imageID"));
        assert!(!SCRIPT.contains(';'));
    }
}
