// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The `filters` of the socket's lists, with Docker's meaning: values of one key are alternatives and different keys
//! must all match (`label` values must all match too).
//!
//! A filter that cannot be honoured is an error, never ignored: `docker rm -f $(docker ps -aq --filter name=x)` must
//! not be handed a wider list than it asked for. For the same reason an empty set of values is refused.

use super::*;
use chrono::{DateTime, Utc};
use fluxvm_image::oci_registry::ImageRef;
use fluxvm_scheduler::oci_images::OCI_LABEL;
use regex::Regex;
use std::collections::BTreeSet;

/// Where `docker create --label` values are kept, in the container's workspace. Apart from the record's own labels, so
/// a Docker label can neither collide with FluxVM's nor change how the container is treated.
const LABELS_FILE: &str = "docker-labels.json";

#[derive(Deserialize)]
#[serde(untagged)]
enum Values {
    List(Vec<String>),
    /// What the CLI sends: a set written as a map. Docker reads the keys, whatever the booleans say.
    Set(BTreeMap<String, bool>),
}

/// A parsed `filters` query parameter.
#[derive(Debug, Default)]
pub(super) struct Filters(pub(super) BTreeMap<String, Vec<String>>);

impl Filters {
    /// `supported` keys are honoured by the caller. `known` keys exist in Docker but not here (501); any other key,
    /// and anything malformed, is a bad request.
    pub(super) fn parse(
        raw: Option<&str>,
        what: &str,
        supported: &[&str],
        known: &[&str],
    ) -> DockerResult<Self> {
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            return Ok(Self::default());
        };
        let groups: BTreeMap<String, Values> = serde_json::from_str(raw).map_err(|e| {
            DockerError::bad(format!(
                "invalid {what} filters (want {{\"key\": {{\"value\": true}}}}): {e}"
            ))
        })?;
        let mut out = BTreeMap::new();
        for (key, values) in groups {
            if !supported.contains(&key.as_str()) {
                let list = supported.join(", ");
                return Err(if known.contains(&key.as_str()) {
                    DockerError::new(
                        StatusCode::NOT_IMPLEMENTED,
                        format!(
                            "The built-in engine does not support the {what} filter {key:?}; supported filters are {list}. {}",
                            extra::FULL_ENGINE
                        ),
                    )
                } else {
                    DockerError::bad(format!(
                        "invalid {what} filter {key:?}; supported filters are {list}"
                    ))
                });
            }
            let values: Vec<String> = match values {
                Values::List(v) => v,
                Values::Set(v) => v.into_keys().collect(),
            };
            if values.is_empty() {
                return Err(DockerError::bad(format!(
                    "the {what} filter {key:?} has no value"
                )));
            }
            out.insert(key, values);
        }
        Ok(Self(out))
    }

    fn values(&self, key: &str) -> &[String] {
        self.0.get(key).map_or(&[], Vec::as_slice)
    }

    /// A true-or-false filter such as `dangling`.
    fn boolean(&self, key: &str) -> DockerResult<Option<bool>> {
        let values = self.values(key);
        if values.is_empty() {
            return Ok(None);
        }
        let truthy = values.iter().any(|v| v == "1" || v == "true");
        let falsy = values.iter().any(|v| v == "0" || v == "false");
        let known = values
            .iter()
            .all(|v| matches!(v.as_str(), "0" | "1" | "true" | "false"));
        if !known || truthy == falsy {
            return Err(DockerError::bad(format!(
                "invalid filter {key}={}: want true or false",
                values.join(",")
            )));
        }
        Ok(Some(truthy))
    }

    /// Every `label` value (`key` or `key=value`) must be present.
    fn labels_match(&self, labels: &BTreeMap<String, String>) -> bool {
        self.values("label")
            .iter()
            .all(|want| match want.split_once('=') {
                Some((k, v)) => labels.get(k).is_some_and(|have| have == v),
                None => labels.contains_key(want),
            })
    }

    /// The values of `key` as the regular expressions Docker matches names with.
    fn patterns(&self, key: &str) -> DockerResult<Vec<Regex>> {
        self.values(key)
            .iter()
            .map(|v| {
                Regex::new(v)
                    .map_err(|e| DockerError::bad(format!("invalid filter {key}={v}: {e}")))
            })
            .collect()
    }
}

// ---- labels ----

/// Keep the labels `docker create` was given.
pub(super) async fn store_labels(
    vm: &VmRecord,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if labels.is_empty() {
        return Ok(());
    }
    tokio::fs::write(vm.workspace.join(LABELS_FILE), serde_json::to_vec(labels)?).await?;
    Ok(())
}

/// The labels Docker sees on a container: the record's, then the ones `docker create` was given.
pub(super) async fn labels_of(vm: &VmRecord) -> DockerResult<BTreeMap<String, String>> {
    let mut labels = vm.labels.clone();
    let path = vm.workspace.join(LABELS_FILE);
    match tokio::fs::read(&path).await {
        Ok(raw) => {
            let own: BTreeMap<String, String> = serde_json::from_slice(&raw).map_err(|e| {
                DockerError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("the labels of container {} are unreadable: {e}", vm.name),
                )
            })?;
            labels.extend(own);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // A container whose workspace cannot be read still lists, but a filter must not guess at its labels.
        Err(e) => {
            return Err(DockerError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reading the labels of container {}: {e}", vm.name),
            ));
        }
    }
    Ok(labels)
}

// ---- containers ----

const CONTAINER_FILTERS: [&str; 8] = [
    "ancestor", "before", "exited", "id", "label", "name", "since", "status",
];
const OTHER_CONTAINER_FILTERS: [&str; 7] = [
    "expose",
    "health",
    "is-task",
    "isolation",
    "network",
    "publish",
    "volume",
];
const STATES: [&str; 7] = [
    "created",
    "restarting",
    "running",
    "removing",
    "paused",
    "exited",
    "dead",
];

/// An image id or digest as typed: `sha256:` optional, at least 12 hex characters.
fn digest_prefix(value: &str) -> Option<String> {
    let hex = value.strip_prefix("sha256:").unwrap_or(value);
    (hex.len() >= 12 && hex.len() <= 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("sha256:{}", hex.to_ascii_lowercase()))
}

/// One `ancestor` value. There is no image parentage here, so it matches containers of that image only.
enum Ancestor {
    /// A digest, or the digests a cached reference stands for.
    Digests(Vec<String>),
    /// A reference that is not in the image cache: containers created from the same reference.
    Reference(ImageRef),
}

impl Ancestor {
    fn new(value: &str, cache: &[OciImageEntry]) -> DockerResult<Self> {
        if let Some(prefix) = digest_prefix(value) {
            return Ok(Self::Digests(vec![prefix]));
        }
        let want = ImageRef::parse(value)
            .map_err(|e| DockerError::bad(format!("invalid filter ancestor={value}: {e:#}")))?;
        let digests: Vec<String> = cache
            .iter()
            .filter(|e| {
                e.references
                    .iter()
                    .any(|r| ImageRef::parse(r).is_ok_and(|r| r == want))
            })
            .map(|e| e.manifest_digest.clone())
            .collect();
        Ok(if digests.is_empty() {
            Self::Reference(want)
        } else {
            Self::Digests(digests)
        })
    }

    fn matches(&self, vm: &VmRecord) -> bool {
        match self {
            Self::Digests(d) => vm
                .labels
                .get(OCI_LABEL)
                .is_some_and(|have| d.iter().any(|d| have.starts_with(d.as_str()))),
            Self::Reference(want) => vm
                .labels
                .get(OCI_IMAGE_LABEL)
                .is_some_and(|have| ImageRef::parse(have).is_ok_and(|have| &have == want)),
        }
    }
}

/// The filters of `GET /containers/json`.
pub(super) struct ContainerFilter {
    filters: Filters,
    ids: Option<BTreeSet<Uuid>>,
    names: Vec<Regex>,
    exited: Vec<i32>,
    ancestors: Vec<Ancestor>,
    before: Vec<(DateTime<Utc>, Uuid)>,
    since: Vec<(DateTime<Utc>, Uuid)>,
}

impl ContainerFilter {
    pub(super) async fn parse(m: &VmManager, raw: Option<&str>) -> DockerResult<Self> {
        let filters = Filters::parse(
            raw,
            "container list",
            &CONTAINER_FILTERS,
            &OTHER_CONTAINER_FILTERS,
        )?;
        if let Some(bad) = filters
            .values("status")
            .iter()
            .find(|s| !STATES.contains(&s.as_str()))
        {
            return Err(DockerError::bad(format!("invalid filter status={bad}")));
        }
        let exited = filters
            .values("exited")
            .iter()
            .map(|v| {
                v.parse()
                    .map_err(|_| DockerError::bad(format!("invalid filter exited={v}")))
            })
            .collect::<DockerResult<Vec<i32>>>()?;
        let names = filters.patterns("name")?;
        // As Docker: an id value selects the one container it is a prefix of; an ambiguous or empty one selects none.
        let ids = if filters.values("id").is_empty() {
            None
        } else {
            let all = oci_containers(m).await;
            Some(
                filters
                    .values("id")
                    .iter()
                    .filter(|prefix| !prefix.is_empty())
                    .filter_map(|prefix| {
                        let mut hits = all
                            .iter()
                            .filter(|v| docker_id(v.id).starts_with(prefix.as_str()));
                        match (hits.next(), hits.next()) {
                            (Some(one), None) => Some(one.id),
                            _ => None,
                        }
                    })
                    .collect(),
            )
        };
        let mut ancestors = Vec::new();
        if !filters.values("ancestor").is_empty() {
            let cache = m.oci_list()?;
            for v in filters.values("ancestor") {
                ancestors.push(Ancestor::new(v, &cache)?);
            }
        }
        let mut before = Vec::new();
        let mut since = Vec::new();
        for (key, out) in [("before", &mut before), ("since", &mut since)] {
            for v in filters.values(key) {
                let vm = resolve_container(m, v).await?;
                out.push((vm.created_at, vm.id));
            }
        }
        Ok(Self {
            filters,
            ids,
            names,
            exited,
            ancestors,
            before,
            since,
        })
    }

    /// A `status` filter looks at every container, not only the running ones.
    pub(super) fn includes_stopped(&self) -> bool {
        !self.filters.values("status").is_empty()
    }

    pub(super) fn matches(
        &self,
        vm: &VmRecord,
        state: ProcessState,
        labels: &BTreeMap<String, String>,
    ) -> bool {
        let at = (vm.created_at, vm.id);
        let status = self.filters.values("status");
        let slashed = format!("/{}", vm.name);
        self.ids.as_ref().is_none_or(|ids| ids.contains(&vm.id))
            && (self.names.is_empty()
                || self
                    .names
                    .iter()
                    .any(|n| n.is_match(&vm.name) || n.is_match(&slashed)))
            && self.filters.labels_match(labels)
            && (self.exited.is_empty()
                || matches!(state, ProcessState::Exited(c) if self.exited.contains(&c)))
            && (status.is_empty() || status.iter().any(|s| s == state_words(state).0))
            && (self.ancestors.is_empty() || self.ancestors.iter().any(|a| a.matches(vm)))
            && self.before.iter().all(|b| at < *b)
            && self.since.iter().all(|s| at > *s)
    }
}

// ---- images ----

/// Go's `path.Match` pattern (what Docker matches image references with) as an anchored regular expression.
fn path_pattern(pattern: &str) -> Option<Regex> {
    let literal = |c: char| format!("\\x{{{:X}}}", u32::from(c));
    let mut out = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            '\\' => out.push_str(&literal(chars.next()?)),
            '[' => {
                out.push('[');
                if chars.next_if_eq(&'^').is_some() {
                    out.push('^');
                }
                let mut ranges = 0;
                loop {
                    if ranges > 0 && chars.next_if_eq(&']').is_some() {
                        break;
                    }
                    let mut one = || match chars.next()? {
                        '-' | ']' => None,
                        '\\' => chars.next(),
                        c => Some(c),
                    };
                    let low = one()?;
                    out.push_str(&literal(low));
                    if chars.next_if_eq(&'-').is_some() {
                        let high = match chars.next()? {
                            '-' | ']' => return None,
                            '\\' => chars.next()?,
                            c => c,
                        };
                        out.push('-');
                        out.push_str(&literal(high));
                    }
                    ranges += 1;
                }
                out.push(']');
            }
            c => out.push_str(&literal(c)),
        }
    }
    out.push('$');
    Regex::new(&out).ok()
}

/// A reference the way Docker shows it (`alpine`, `alpine:3.22`): without Docker Hub's registry and `library/`.
fn familiar(reference: &str) -> (String, String) {
    let Ok(r) = ImageRef::parse(reference) else {
        return (reference.to_string(), reference.to_string());
    };
    let name = if r.registry == "registry-1.docker.io" {
        r.repository
            .strip_prefix("library/")
            .unwrap_or(&r.repository)
            .to_string()
    } else {
        format!("{}/{}", r.registry, r.repository)
    };
    let sep = if r.reference.starts_with("sha256:") {
        '@'
    } else {
        ':'
    };
    let full = format!("{name}{sep}{}", r.reference);
    (name, full)
}

fn strings(image: &Value, key: &str) -> Vec<String> {
    image[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
}

/// When the image `value` (an id or a reference) was made.
fn created_of(images: &[Value], value: &str) -> DockerResult<i64> {
    let prefix = digest_prefix(value);
    let want = ImageRef::parse(value).ok();
    images
        .iter()
        .find(|i| {
            prefix
                .as_ref()
                .is_some_and(|p| i["Id"].as_str().is_some_and(|id| id.starts_with(p)))
                || want.as_ref().is_some_and(|want| {
                    ["RepoTags", "RepoDigests"].iter().any(|k| {
                        strings(i, k)
                            .iter()
                            .any(|r| ImageRef::parse(r).is_ok_and(|r| &r == want))
                    })
                })
        })
        .and_then(|i| i["Created"].as_i64())
        .ok_or_else(|| DockerError::not_found(format!("No such image: {value}")))
}

/// The filters of `GET /images/json`, applied to the summaries. `reference` also narrows each image's tags to the
/// matching ones, so a script that lists names does not get the others.
pub(super) fn filter_images(images: Vec<Value>, raw: Option<&str>) -> DockerResult<Vec<Value>> {
    let filters = Filters::parse(
        raw,
        "image list",
        &["before", "dangling", "reference", "since"],
        &["label", "label!", "until"],
    )?;
    let dangling = filters.boolean("dangling")?;
    let references = filters
        .values("reference")
        .iter()
        .map(|v| {
            path_pattern(v)
                .map(|p| (v.as_str(), p))
                .ok_or_else(|| DockerError::bad(format!("invalid filter reference={v}")))
        })
        .collect::<DockerResult<Vec<_>>>()?;
    let times = |key: &str| {
        filters
            .values(key)
            .iter()
            .map(|v| created_of(&images, v))
            .collect::<DockerResult<Vec<i64>>>()
    };
    let (before, since) = (times("before")?, times("since")?);
    let matching = |r: &String| {
        let (name, full) = familiar(r);
        references
            .iter()
            .any(|(raw, p)| *raw == r || p.is_match(&name) || p.is_match(&full))
    };
    let mut out = Vec::new();
    for mut image in images {
        let created = image["Created"].as_i64().unwrap_or(0);
        if dangling.is_some_and(|d| d != strings(&image, "RepoTags").is_empty())
            || !before.iter().all(|t| created < *t)
            || !since.iter().all(|t| created > *t)
        {
            continue;
        }
        if !references.is_empty() {
            let tags: Vec<String> = strings(&image, "RepoTags")
                .into_iter()
                .filter(matching)
                .collect();
            if tags.is_empty() && !strings(&image, "RepoDigests").iter().any(matching) {
                continue;
            }
            image["RepoTags"] = if tags.is_empty() {
                Value::Null
            } else {
                json!(tags)
            };
        }
        out.push(image);
    }
    Ok(out)
}

// ---- networks and volumes ----

/// The filters of `GET /networks`, for the one built-in network.
pub(super) fn filter_networks(networks: Vec<Value>, raw: Option<&str>) -> DockerResult<Vec<Value>> {
    let filters = Filters::parse(
        raw,
        "network list",
        &["dangling", "driver", "id", "label", "name", "scope", "type"],
        &[],
    )?;
    let names = filters.patterns("name")?;
    if let Some(bad) = filters
        .values("type")
        .iter()
        .find(|t| !matches!(t.as_str(), "builtin" | "custom"))
    {
        return Err(DockerError::bad(format!("invalid filter type={bad}")));
    }
    let dangling = filters.boolean("dangling")?;
    let one_of = |key: &str, have: &str| {
        let want = filters.values(key);
        want.is_empty() || want.iter().any(|w| w == have)
    };
    Ok(networks
        .into_iter()
        .filter(|n| {
            let text = |k: &str| n[k].as_str().unwrap_or_default().to_string();
            let labels = serde_json::from_value(n["Labels"].clone()).unwrap_or_default();
            let ids = filters.values("id");
            (names.is_empty() || names.iter().any(|p| p.is_match(&text("Name"))))
                && (ids.is_empty()
                    || ids
                        .iter()
                        .any(|i| !i.is_empty() && text("Id").starts_with(i.as_str())))
                && one_of("driver", &text("Driver"))
                && one_of("scope", &text("Scope"))
                // The built-in network cannot be removed: it is `builtin` and never dangling.
                && one_of("type", "builtin")
                && dangling != Some(true)
                && filters.labels_match(&labels)
        })
        .collect())
}

/// `GET /volumes` is always empty, but a filter it would not understand is still refused.
pub(super) fn check_volume_filters(raw: Option<&str>) -> DockerResult<()> {
    let filters = Filters::parse(
        raw,
        "volume list",
        &["dangling", "driver", "label", "name"],
        &[],
    )?;
    filters.boolean("dangling")?;
    Ok(())
}

#[cfg(test)]
mod tests;
