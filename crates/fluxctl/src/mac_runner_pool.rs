// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac runner --org`: a warm pool for an organisation.
//!
//! GitHub has no queue-depth call for an organisation, so the queue is read from the repositories the operator names
//! (`--queue-repo OWNER/REPO`, repeatable) or, with `--queue-scan`, from the organisation's most recently pushed
//! repositories (at most `--scan-limit`, default 30, list cached for 5 minutes). Runners are registered at organisation
//! level, in the runner group chosen by `--runner-group ID` or `--runner-group-name NAME`, with the labels from `--label`.
//! `--ephemeral-pool N` is the ceiling on runners at once: the pool grows with the queued jobs, never past N (nor past 2
//! guests per Mac) and shrinks by itself because every runner takes one job and goes.
//!
//! Not run against GitHub: see docs/mac-runner.md.

use crate::mac_fleet_remote::Secret;
use crate::mac_runner::{Github, Jit, RealGithub, RunnerInfo, Scope};
use anyhow::{Context, Result, bail};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many guests the pool may run: `--ephemeral-pool` and `--max-jobs` lower the hard limit, never raise it.
pub fn pool_cap(hard: usize, ephemeral_pool: Option<usize>) -> usize {
    ephemeral_pool.map_or(hard, |n| n.min(hard)).max(1)
}

/// One line saying what the pool decided, for the log.
pub fn describe(
    queued: usize,
    busy: usize,
    in_flight: usize,
    warm: usize,
    cap: usize,
    spawn: usize,
) -> String {
    format!(
        "pool: queued={queued} busy={busy} waiting={} warm={warm} cap={cap} -> start {spawn}",
        in_flight.saturating_sub(busy)
    )
}

pub fn group_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b" -_.".contains(&c))
}

/// The id of the runner group called `name` in a `runner-groups` listing.
pub fn find_group(listing: &serde_json::Value, name: &str) -> Option<u64> {
    listing["runner_groups"].as_array()?.iter().find(|g| {
        g["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })?["id"]
        .as_u64()
}

/// Repositories of an `orgs/ORG/repos` listing, newest push first, at most `limit`, archived ones skipped.
pub fn scan_repos(listing: &serde_json::Value, limit: usize) -> Vec<(String, String)> {
    let mut v: Vec<(String, String, String)> = listing
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["archived"].as_bool() != Some(true) && r["disabled"].as_bool() != Some(true))
        .filter_map(|r| {
            let full = r["full_name"].as_str()?;
            let (o, n) = full.split_once('/')?;
            Some((
                o.to_owned(),
                n.to_owned(),
                r["pushed_at"].as_str().unwrap_or("").to_owned(),
            ))
        })
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v.into_iter().take(limit).map(|(o, n, _)| (o, n)).collect()
}

pub fn runner_groups_path(org: &str) -> String {
    format!("orgs/{org}/actions/runner-groups?per_page=100")
}

pub fn org_repos_path(org: &str) -> String {
    format!("orgs/{org}/repos?type=all&sort=pushed&per_page=100")
}

/// An organisation's runners, whose queue is the sum over some repositories.
pub struct OrgPool {
    pub org: RealGithub,
    pub api: String,
    pub token: Secret,
    pub explicit: Vec<(String, String)>,
    pub scan: Option<usize>,
    cache: Mutex<Option<(Instant, Vec<(String, String)>)>>,
}

impl OrgPool {
    pub fn new(
        org: RealGithub,
        api: &str,
        token: Secret,
        explicit: Vec<(String, String)>,
        scan: Option<usize>,
    ) -> Self {
        Self {
            org,
            api: api.into(),
            token,
            explicit,
            scan,
            cache: Mutex::new(None),
        }
    }

    fn repos(&self) -> Result<Vec<(String, String)>> {
        let mut out = self.explicit.clone();
        if let Some(limit) = self.scan {
            let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = c
                .as_ref()
                .is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(300));
            if !fresh {
                let Scope::Org(o) = self.org.scope() else {
                    bail!("--queue-scan needs --org")
                };
                let v = self.org.get(
                    &org_repos_path(o),
                    "listing the organisation's repositories",
                )?;
                *c = Some((Instant::now(), scan_repos(&v, limit)));
            }
            for r in c.as_ref().map(|(_, v)| v.clone()).unwrap_or_default() {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
        Ok(out)
    }
}

impl Github for OrgPool {
    fn queued(&self, labels: &[String]) -> Result<Option<usize>> {
        let repos = self.repos()?;
        if repos.is_empty() {
            return Ok(None);
        }
        let mut total = 0;
        for (o, n) in repos {
            let g = RealGithub::new(&self.api, Scope::Repo(o, n), 1, self.token.clone())?;
            total += g.queued(labels)?.unwrap_or(0);
        }
        Ok(Some(total))
    }
    fn busy(&self) -> Result<usize> {
        self.org.busy()
    }
    fn jit(&self, name: &str, labels: &[String]) -> Result<Jit> {
        self.org.jit(name, labels)
    }
    fn remove(&self, runner_id: u64) -> Result<()> {
        self.org.remove(runner_id)
    }
    fn runners(&self) -> Result<Vec<RunnerInfo>> {
        self.org.runners()
    }
}

/// Resolve `--runner-group-name` against the organisation.
pub fn resolve_group(api: &str, org: &str, token: &Secret, name: &str) -> Result<u64> {
    if !group_name_ok(name) {
        bail!("--runner-group-name {name:?} has characters a runner group cannot have");
    }
    let g = RealGithub::new(api, Scope::Org(org.into()), 1, token.clone())?;
    let v = g.get(&runner_groups_path(org), "listing runner groups")?;
    find_group(&v, name)
        .with_context(|| format!("organisation {org} has no runner group called {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_pool_never_exceeds_its_ceiling_or_the_two_per_mac_limit() {
        assert_eq!(pool_cap(6, None), 6);
        assert_eq!(pool_cap(6, Some(3)), 3);
        assert_eq!(pool_cap(4, Some(50)), 4);
        assert_eq!(pool_cap(4, Some(0)), 1);
        // driven by the queue: with cap 3, five queued jobs start three runners, then none while they are busy
        use crate::mac_runner::plan_spawn;
        assert_eq!(plan_spawn(0, 0, 5, 0, 3), 3);
        assert_eq!(plan_spawn(3, 3, 5, 0, 3), 0);
        assert_eq!(
            plan_spawn(3, 3, 0, 0, 3),
            0,
            "nothing queued: no new runner, finished ones simply go"
        );
        assert!(
            describe(5, 1, 3, 0, 3, 2).contains("queued=5")
                && describe(5, 1, 3, 0, 3, 2).contains("waiting=2")
        );
    }

    #[test]
    fn runner_groups_are_found_by_name() {
        let l =
            json!({"runner_groups": [{"id": 1, "name": "Default"}, {"id": 7, "name": "Mac CI"}]});
        assert_eq!(find_group(&l, "mac ci"), Some(7));
        assert_eq!(find_group(&l, "nope"), None);
        assert!(group_name_ok("Mac CI") && !group_name_ok("a/b") && !group_name_ok(""));
        assert_eq!(
            runner_groups_path("acme"),
            "orgs/acme/actions/runner-groups?per_page=100"
        );
    }

    #[test]
    fn the_scan_takes_the_newest_pushed_repositories_and_skips_archived_ones() {
        let l = json!([
            {"full_name": "acme/old", "pushed_at": "2025-01-01T00:00:00Z"},
            {"full_name": "acme/new", "pushed_at": "2026-11-01T00:00:00Z"},
            {"full_name": "acme/dead", "pushed_at": "2026-12-01T00:00:00Z", "archived": true},
            {"full_name": "acme/mid", "pushed_at": "2026-06-01T00:00:00Z"},
        ]);
        assert_eq!(
            scan_repos(&l, 2),
            vec![
                ("acme".to_owned(), "new".to_owned()),
                ("acme".to_owned(), "mid".to_owned())
            ]
        );
    }
}
