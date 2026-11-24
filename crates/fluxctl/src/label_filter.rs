// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `--filter KEY=VALUE` for the lists that carry labels but have no filter of their own: `fluxctl kube apps` and `fluxctl fleet nodes`
//! (`mac ps`, `mac images` and `mac events` have their own, richer parsers). A filter is `label=KEY` (has the label), `label=KEY=VALUE`
//! (has it with that value) or `FIELD=VALUE` for one of the fields the list allows. Every filter must match.
//! Pure: it takes JSON rows, so it is unit tested without a cluster.

use crate::mac_errors::coded;
use anyhow::Result;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `label=KEY` or `label=KEY=VALUE`.
    Label(String, Option<String>),
    /// `FIELD=VALUE`; `name` and `image` also match a part of the value.
    Field(String, String),
}

/// Parse one `--filter`. `fields` are the keys this list accepts besides `label`.
pub fn parse(s: &str, fields: &[&str]) -> Result<Filter> {
    let keys = format!("label, {}", fields.join(", "));
    let Some((k, v)) = s.split_once('=') else {
        return Err(coded(
            format!("filter {s:?} must be KEY=VALUE (keys: {keys})"),
            "MAC_FILTER_INVALID",
        ));
    };
    if v.is_empty() {
        return Err(coded(
            format!("filter {s:?} has an empty value (keys: {keys})"),
            "MAC_FILTER_INVALID",
        ));
    }
    if k == "label" {
        return Ok(match v.split_once('=') {
            Some((lk, lv)) if !lk.is_empty() => Filter::Label(lk.into(), Some(lv.into())),
            Some(_) => {
                return Err(coded(
                    format!(
                        "filter {s:?}: the label name is empty (use label=team or label=team=ios)"
                    ),
                    "MAC_FILTER_INVALID",
                ));
            }
            None => Filter::Label(v.into(), None),
        });
    }
    if fields.contains(&k) {
        return Ok(Filter::Field(k.into(), v.into()));
    }
    Err(coded(
        format!("unknown filter {k:?}: use one of {keys}"),
        "MAC_FILTER_INVALID",
    ))
}

pub fn parse_all(list: &[String], fields: &[&str]) -> Result<Vec<Filter>> {
    list.iter().map(|s| parse(s, fields)).collect()
}

/// Does `row` pass every filter? `field` reads a field of the row as text (`None` when the row has none).
pub fn matches(
    row: &Value,
    field: impl Fn(&Value, &str) -> Option<String>,
    filters: &[Filter],
) -> bool {
    filters.iter().all(|f| match f {
        Filter::Label(k, v) => match (row["labels"][k.as_str()].as_str(), v) {
            (Some(x), Some(want)) => x == want,
            (Some(_), None) => true,
            (None, _) => false,
        },
        Filter::Field(k, want) => field(row, k).is_some_and(|x| {
            x == *want || (matches!(k.as_str(), "name" | "image") && x.contains(want.as_str()))
        }),
    })
}

pub fn text(row: &Value, key: &str) -> Option<String> {
    row[key].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_labels_and_fields_and_explains_the_rest() {
        let f = ["name", "state"];
        assert_eq!(
            parse("label=team", &f).unwrap(),
            Filter::Label("team".into(), None)
        );
        assert_eq!(
            parse("label=team=ios", &f).unwrap(),
            Filter::Label("team".into(), Some("ios".into()))
        );
        assert_eq!(
            parse("state=running", &f).unwrap(),
            Filter::Field("state".into(), "running".into())
        );
        for bad in ["team", "label=", "label==x", "colour=red", "name="] {
            let e = parse(bad, &f).unwrap_err().to_string();
            assert!(e.contains("MAC_FILTER_INVALID"), "{bad}: {e}");
        }
        assert!(
            parse("colour=red", &f)
                .unwrap_err()
                .to_string()
                .contains("label, name, state")
        );
    }

    #[test]
    fn every_filter_must_match() {
        let row = json!({"name": "web-1", "state": "running", "labels": {"team": "ios"}});
        let m = |f: &[&str]| {
            let fl = parse_all(
                &f.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &["name", "state"],
            )
            .unwrap();
            matches(&row, text, &fl)
        };
        assert!(m(&[]));
        assert!(m(&["label=team"]) && m(&["label=team=ios"]));
        assert!(!m(&["label=team=android"]) && !m(&["label=tier"]));
        assert!(
            m(&["name=web", "state=running"]),
            "name matches a part, state is exact"
        );
        assert!(!m(&["state=run"]));
        assert!(!m(&["label=team=ios", "state=stopped"]));
    }
}
