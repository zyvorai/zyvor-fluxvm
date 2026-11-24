// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl docker builds`: the `docker build` history of the daemon's built-in Docker socket (the same list as Studio's
//! Images > Builds). The daemon keeps it in a file next to the socket, so this needs no daemon and no token.

use crate::docker_host;
use anyhow::{Context, Result};
use fluxvm_api::docker_build::{BuildRecord, HISTORY_FILE, short_id};
use std::path::PathBuf;

#[derive(clap::Args, Debug, Clone)]
pub struct BuildsArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    /// How many builds to show (newest first).
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// The history file (default: next to the daemon's built-in Docker socket).
    #[arg(long)]
    pub file: Option<PathBuf>,
}

fn dur(ms: Option<u64>) -> String {
    match ms {
        Some(m) if m >= 60_000 => format!("{}m{:02}s", m / 60_000, (m % 60_000) / 1000),
        Some(m) => format!("{}s", m / 1000),
        None => "-".into(),
    }
}

/// The history as a table, newest first.
pub fn render(rows: &[BuildRecord]) -> String {
    if rows.is_empty() {
        return "no builds yet. Run `docker build -t NAME:TAG .` against the FluxVM socket (`fluxctl docker context install --builtin --use` sets that up)\n".into();
    }
    let mut out = String::from("ID           STATE      TAKES   IMAGE         NAME\n");
    for r in rows {
        out.push_str(&format!(
            "{:<12} {:<10} {:<7} {:<13} {}\n",
            r.id,
            r.state,
            dur(r.duration_ms),
            r.image_id.as_deref().map(short_id).unwrap_or("-"),
            if r.tags.is_empty() {
                "(untagged)".into()
            } else {
                r.tags.join(", ")
            }
        ));
        if let Some(e) = &r.error {
            out.push_str(&format!(
                "             {}\n",
                e.lines().next_back().unwrap_or("")
            ));
        }
    }
    out
}

pub fn run(a: BuildsArgs) -> Result<()> {
    let path = a
        .file
        .unwrap_or_else(|| docker_host::builtin_socket().with_file_name(HISTORY_FILE));
    let mut rows: Vec<BuildRecord> = match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| {
            format!(
                "{} is not a build history; delete it and build again",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    rows.reverse();
    rows.truncate(a.limit.max(1));
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", render(&rows));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(state: &str, err: Option<&str>) -> BuildRecord {
        BuildRecord {
            id: "abc123".into(),
            started_unix: 1,
            finished_unix: Some(2),
            duration_ms: Some(75_000),
            state: state.into(),
            tags: vec!["app:1".into()],
            dockerfile: "Macfile".into(),
            build_arg_names: vec![],
            target: None,
            platform: None,
            no_cache: false,
            context_bytes: 1,
            image_id: Some("sha256:0123456789abcdef".into()),
            error: err.map(str::to_owned),
            builder: "fluxctl mac build".into(),
            fluxctl: "fluxctl mac build -t app:1 .".into(),
        }
    }

    #[test]
    fn an_empty_history_says_how_to_start() {
        assert!(render(&[]).contains("docker build -t NAME:TAG ."));
    }

    #[test]
    fn rows_show_state_time_image_and_the_failure() {
        let t = render(&[
            rec("succeeded", None),
            rec("failed", Some("step 2\nthe disk is full")),
        ]);
        assert!(t.contains("1m15s") && t.contains("0123456789ab") && t.contains("app:1"));
        assert!(t.contains("the disk is full") && !t.contains("step 2"));
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            run(BuildsArgs {
                json: true,
                limit: 5,
                file: Some(d.path().join("none.json"))
            })
            .is_ok()
        );
    }
}
