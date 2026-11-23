// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The commands people reach for first, grouped, for `fluxctl cheatsheet` and Studio's Cheatsheet page
//! (`GET /v1/cheatsheet`). `fluxctl` has a test that parses every command here with the real CLI, so it cannot drift
//! from what exists.

/// (group, [(command, what it does)]).
pub const SHEET: &[(&str, &[(&str, &str)])] = &[
    (
        "Is it working",
        &[
            (
                "fluxctl doctor",
                "check this host: daemon, binaries, disk, and the fix for each problem",
            ),
            (
                "fluxctl cluster status",
                "the health of the cluster in plain English",
            ),
            (
                "fluxctl explain CODE",
                "what an error code means and what to do about it",
            ),
            ("fluxctl status", "the host panel: what is running"),
        ],
    ),
    (
        "Start something",
        &[
            (
                "fluxctl templates ls",
                "ready-made stacks: Postgres, Redis, nginx and more",
            ),
            (
                "fluxctl templates run redis",
                "start one with a single command",
            ),
            (
                "fluxctl recipe list",
                "a dev database, a website, a CI runner, monitoring",
            ),
            ("fluxctl up", "start the stack in fluxvm.toml"),
            ("fluxctl dashboard", "open Studio, the web UI"),
        ],
    ),
    (
        "Settings",
        &[
            (
                "fluxctl config ls",
                "every setting that is changed from its default",
            ),
            ("fluxctl config get cli.upgrade_url", "read one setting"),
            (
                "fluxctl config set cli.upgrade_url URL",
                "change one, checked before it is written",
            ),
            ("fluxctl config path", "which file holds them"),
        ],
    ),
    (
        "This tool",
        &[
            (
                "fluxctl upgrade --check",
                "is there a newer release, and what would change",
            ),
            (
                "fluxctl upgrade",
                "replace this binary after verifying its checksum (asks first)",
            ),
            (
                "fluxctl completion install",
                "tab completion for your shell",
            ),
            (
                "fluxctl mac cheatsheet",
                "the same idea for macOS instances",
            ),
        ],
    ),
];

pub fn json() -> serde_json::Value {
    let groups: Vec<_> = SHEET
        .iter()
        .map(|(g, rows)| {
            serde_json::json!({
                "group": g,
                "commands": rows.iter().map(|(c, d)| serde_json::json!({"command": c, "does": d})).collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({ "schema": 1, "groups": groups })
}

pub fn text() -> String {
    let width = SHEET
        .iter()
        .flat_map(|(_, r)| r.iter())
        .map(|(c, _)| c.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (g, rows) in SHEET {
        out.push_str(&format!("{g}\n"));
        for (c, d) in *rows {
            out.push_str(&format!("  {c:<width$}  {d}\n"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_filled_in_and_the_json_matches() {
        for (g, rows) in SHEET {
            assert!(!g.is_empty() && !rows.is_empty());
            for (c, d) in *rows {
                assert!(c.starts_with("fluxctl ") && !d.is_empty());
            }
        }
        assert_eq!(json()["groups"].as_array().unwrap().len(), SHEET.len());
        assert!(text().contains("fluxctl upgrade"));
    }
}
