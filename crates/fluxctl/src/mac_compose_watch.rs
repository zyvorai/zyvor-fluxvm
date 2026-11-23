// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac compose watch`: `develop: watch:` rules, by polling file times (no extra dependency, no daemon support needed),
//! and the restart policies of the project's instances.
//!
//! A rule is `{path, action, target, ignore}`; the actions are `sync` (copy changed files into the guest), `sync+restart` (copy,
//! then run the service's command again), `restart` and `rebuild` (build the image again and recreate the instance).
//! `restart:` policies are applied by the same loop, because the daemon never restarts a macOS VM by itself:
//! `on-failure:N` is enforced here (see [`crate::mac_compose_ext::RestartBook`]).
//!
//! The planning (snapshots, diffs, reactions, which instances to start again) is pure and unit tested; the loop and the guest
//! actions were never run against a real macOS VM.

use crate::mac_compose::{Resolved, ServicePlan};
use crate::mac_compose_ext::RestartBook;
use crate::mac_config;
use crate::mac_errors::coded;
use crate::mac_inspect::SERVICE_LABEL;
use crate::mac_instance::Restart;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---- the rules -------------------------------------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Develop {
    #[serde(default)]
    watch: Vec<WatchCfg>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct WatchCfg {
    path: String,
    action: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    ignore: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchAction {
    /// Copy the changed files into the guest.
    Sync,
    /// Copy, then run the service's command again.
    SyncRestart,
    /// Run the service's command again.
    Restart,
    /// Build the image again and recreate the instance.
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRule {
    /// Absolute, on this Mac.
    pub path: PathBuf,
    pub action: WatchAction,
    /// Absolute, in the guest (sync actions).
    pub target: Option<String>,
    pub ignore: Vec<String>,
}

impl Develop {
    pub fn rules(&self, service: &str, base: &Path, has_build: bool) -> Result<Vec<WatchRule>> {
        let bad = |m: String| {
            coded(
                format!("service {service:?}: develop.watch: {m}"),
                "MAC_COMPOSE_WATCH_RULE",
            )
        };
        let mut out = vec![];
        for w in &self.watch {
            let action = match w.action.as_str() {
                "sync" => WatchAction::Sync,
                "sync+restart" => WatchAction::SyncRestart,
                "restart" => WatchAction::Restart,
                "rebuild" => WatchAction::Rebuild,
                other => {
                    return Err(bad(format!(
                        "action {other:?} is not one of sync, sync+restart, restart, rebuild"
                    )));
                }
            };
            if w.path.trim().is_empty() {
                return Err(bad("a rule has an empty `path:`".into()));
            }
            let target = w
                .target
                .as_deref()
                .map(|t| t.trim_end_matches('/'))
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if matches!(action, WatchAction::Sync | WatchAction::SyncRestart)
                && !target.as_deref().is_some_and(|t| t.starts_with('/'))
            {
                return Err(bad(format!(
                    "the {} rule on {} needs an absolute `target:` in the guest (for example /Users/admin/app)",
                    w.action, w.path
                )));
            }
            if action == WatchAction::Rebuild && !has_build {
                return Err(bad(format!(
                    "the rebuild rule on {} needs the service to have `build:`",
                    w.path
                )));
            }
            out.push(WatchRule {
                path: base.join(w.path.strip_prefix("./").unwrap_or(&w.path)),
                action,
                target,
                ignore: w.ignore.clone(),
            });
        }
        Ok(out)
    }
}

/// `*` matches within one path part, `**` across parts, `?` one character. A pattern with no `/` matches any one part of the
/// path (`node_modules`, `*.log`); one with `/` matches the whole path relative to the watched folder.
pub fn glob_match(pattern: &str, rel: &str) -> bool {
    fn m(p: &[u8], t: &[u8]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((b'*', rest)) => {
                if let Some(rest2) = rest.strip_prefix(b"*") {
                    let rest2 = rest2.strip_prefix(b"/").unwrap_or(rest2);
                    (0..=t.len()).any(|i| m(rest2, &t[i..]))
                } else {
                    (0..=t.len())
                        .take_while(|&i| i == 0 || t[i - 1] != b'/')
                        .any(|i| m(rest, &t[i..]))
                }
            }
            Some((b'?', rest)) => t.first().is_some_and(|c| *c != b'/') && m(rest, &t[1..]),
            Some((c, rest)) => t.first() == Some(c) && m(rest, &t[1..]),
        }
    }
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    if pattern.contains('/') {
        m(pattern.as_bytes(), rel.as_bytes())
    } else {
        rel.split('/')
            .any(|part| m(pattern.as_bytes(), part.as_bytes()))
    }
}

// ---- polling ---------------------------------------------------------------------------------------------------------

/// What a file looked like: modification time and size are enough for polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub mtime_ns: u128,
    pub len: u64,
}

pub type Snapshot = BTreeMap<PathBuf, Stamp>;

const ALWAYS_IGNORED: &[&str] = &[".git", ".DS_Store", ".svn", ".hg"];

fn stamp(md: &std::fs::Metadata) -> Stamp {
    Stamp {
        mtime_ns: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos()),
        len: md.len(),
    }
}

fn walk(root: &Path, dir: &Path, ignore: &[String], out: &mut Snapshot, depth: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        let name = e.file_name().to_string_lossy().into_owned();
        if ALWAYS_IGNORED.contains(&name.as_str()) || ignore.iter().any(|g| glob_match(g, &rel)) {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if depth < 32 {
                walk(root, &p, ignore, out, depth + 1);
            }
        } else if let Ok(md) = e.metadata() {
            out.insert(p, stamp(&md));
        }
    }
}

/// Every file under the rule's path (or the file itself) that is not ignored.
pub fn scan(rule: &WatchRule) -> Snapshot {
    let mut out = Snapshot::new();
    match std::fs::metadata(&rule.path) {
        Ok(md) if md.is_file() => {
            out.insert(rule.path.clone(), stamp(&md));
        }
        Ok(_) => walk(&rule.path, &rule.path, &rule.ignore, &mut out, 0),
        Err(_) => {}
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Added or modified.
    Changed(PathBuf),
    Removed(PathBuf),
}

pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Change> {
    let mut out: Vec<Change> = new
        .iter()
        .filter(|(p, s)| old.get(*p) != Some(s))
        .map(|(p, _)| Change::Changed(p.clone()))
        .collect();
    out.extend(
        old.keys()
            .filter(|p| !new.contains_key(*p))
            .map(|p| Change::Removed(p.clone())),
    );
    out
}

/// What one service needs done after a poll.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    /// `(file on this Mac, path in the guest)`.
    pub copies: Vec<(PathBuf, String)>,
    pub removes: Vec<String>,
    pub restart: bool,
    pub rebuild: bool,
}

impl Reaction {
    pub fn is_empty(&self) -> bool {
        *self == Reaction::default()
    }
}

fn guest_path(rule: &WatchRule, file: &Path) -> Option<String> {
    let target = rule.target.as_deref()?;
    let rel = file.strip_prefix(&rule.path).ok()?;
    if rel.as_os_str().is_empty() {
        return Some(target.to_string());
    }
    Some(format!(
        "{target}/{}",
        rel.to_string_lossy().replace('\\', "/")
    ))
}

/// Turn the changes found for each rule (same order as `rules`) into one reaction. A rebuild covers everything else.
pub fn react(rules: &[WatchRule], changes: &[Vec<Change>]) -> Reaction {
    let mut r = Reaction::default();
    for (rule, cs) in rules.iter().zip(changes) {
        if cs.is_empty() {
            continue;
        }
        match rule.action {
            WatchAction::Rebuild => r.rebuild = true,
            WatchAction::Restart => r.restart = true,
            WatchAction::Sync | WatchAction::SyncRestart => {
                for c in cs {
                    match c {
                        Change::Changed(p) => {
                            if let Some(g) = guest_path(rule, p) {
                                r.copies.push((p.clone(), g));
                            }
                        }
                        Change::Removed(p) => {
                            if let Some(g) = guest_path(rule, p) {
                                r.removes.push(g);
                            }
                        }
                    }
                }
                if rule.action == WatchAction::SyncRestart {
                    r.restart = true;
                }
            }
        }
    }
    if r.rebuild {
        r = Reaction {
            rebuild: true,
            ..Default::default()
        };
    }
    r
}

/// Shell text that stops the command started by [`detach_pid`] (it stops the process and its children).
pub fn stop_command() -> &'static str {
    "p=$(cat /tmp/fluxvm-compose.pid 2>/dev/null); [ -n \"$p\" ] && { pkill -P \"$p\" 2>/dev/null; kill \"$p\" 2>/dev/null; sleep 1; }; true"
}

/// The service's command in the background, like `mac run -d`, recording its pid so a restart can stop it first.
pub fn detach_pid(command: &str) -> String {
    format!(
        "nohup sh -c 'echo $$ >/tmp/fluxvm-compose.pid; exec sh -c \"$0\"' {} >/tmp/fluxvm-mac-cmd.log 2>&1 </dev/null &",
        mac_config::sq(command)
    )
}

pub fn validate_watchable(rules: &[WatchRule]) -> Result<()> {
    for r in rules {
        if !r.path.exists() {
            return Err(coded(
                format!(
                    "develop.watch: {} does not exist on this Mac; create it or fix `path:`",
                    r.path.display()
                ),
                "MAC_COMPOSE_WATCH_RULE",
            ));
        }
    }
    Ok(())
}

// ---- carrying a reaction out -----------------------------------------------------------------------------------------

/// What the loop does to a guest; the real one talks to the daemon and ssh, the test one records.
pub(crate) trait Runner {
    async fn exec(&mut self, instance: &str, text: &str) -> Result<()>;
    async fn upload(&mut self, instance: &str, local: &Path, guest: &str) -> Result<()>;
    /// Build the service's image again and recreate the instance from it.
    async fn rebuild(&mut self, sp: &ServicePlan) -> Result<()>;
}

/// Carry out one reaction on one instance; returns a line for the user.
pub(crate) async fn apply(
    run: &mut impl Runner,
    sp: &ServicePlan,
    command: Option<&str>,
    re: &Reaction,
) -> Result<String> {
    let name = &sp.spec.name;
    if re.rebuild {
        run.rebuild(sp)
            .await
            .with_context(|| format!("rebuilding {name}"))?;
        return Ok(format!(
            "{name}: rebuilt the image and recreated the instance"
        ));
    }
    let mut did: Vec<String> = vec![];
    for g in &re.removes {
        run.exec(name, &format!("rm -rf {}", mac_config::sq(g)))
            .await?;
    }
    if !re.removes.is_empty() {
        did.push(format!("removed {} file(s)", re.removes.len()));
    }
    let dirs: BTreeSet<&str> = re
        .copies
        .iter()
        .filter_map(|(_, g)| {
            g.rsplit_once('/')
                .map(|(d, _)| if d.is_empty() { "/" } else { d })
        })
        .collect();
    if !dirs.is_empty() {
        let mk: Vec<String> = dirs.iter().map(|d| mac_config::sq(d)).collect();
        run.exec(name, &format!("mkdir -p {}", mk.join(" ")))
            .await?;
    }
    for (local, g) in &re.copies {
        run.upload(name, local, g).await?;
    }
    if !re.copies.is_empty() {
        did.push(format!("synced {} file(s)", re.copies.len()));
    }
    if re.restart {
        match command {
            Some(c) => {
                run.exec(name, stop_command()).await?;
                run.exec(name, &detach_pid(c)).await?;
                did.push("ran its command again".into());
            }
            None => did.push("has no command to run again".into()),
        }
    }
    Ok(format!("{name}: {}", did.join(", ")))
}

// ---- restart policies ------------------------------------------------------------------------------------------------

/// One instance the policy says to start again, or one it gave up on.
#[derive(Debug, PartialEq, Eq)]
pub struct Revive {
    pub name: String,
    pub id: String,
    pub service: String,
    /// `false`: `on-failure:N` has used up its N restarts.
    pub allowed: bool,
    pub tries: u32,
    pub limit: Option<u32>,
}

/// The instances `compose watch` starts again: what `mac reconcile` would start ([`crate::mac_more::should_restart`]), minus
/// services that run to completion (their VM is stopped on purpose), minus instances whose `on-failure:N` is used up.
pub fn revivals(vms: &[serde_json::Value], plan: &Resolved, book: &mut RestartBook) -> Vec<Revive> {
    let mut out = vec![];
    for v in vms.iter().filter(|v| crate::mac_more::should_restart(v)) {
        let service = v["labels"][SERVICE_LABEL].as_str().unwrap_or("");
        let Some(sp) = plan.services.iter().find(|s| s.service == service) else {
            continue;
        };
        let name = v["name"].as_str().unwrap_or("");
        if sp.run_to_completion || !plan.services.iter().any(|s| s.spec.name == name) {
            continue;
        }
        let allowed = book.allow(name, sp.restart_limit);
        out.push(Revive {
            name: name.to_string(),
            id: v["id"].as_str().unwrap_or("").to_string(),
            service: service.to_string(),
            allowed,
            tries: book.tries(name),
            limit: sp.restart_limit,
        });
    }
    out
}

// ---- the loop against the daemon -------------------------------------------------------------------------------------

struct Live<'a> {
    r: &'a crate::remote::Remote,
    store: crate::mac_store::Store,
    wait: u64,
}

impl Live<'_> {
    async fn target(&self, instance: &str) -> Result<(String, String)> {
        let vm = crate::mac_cmd::find_instance(self.r, instance).await?;
        let ip = vm["guest_ip"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{instance} has no address (status {}): it is not running, so nothing can be copied into it. Start it with `fluxctl mac compose start`, or let a restart policy do it",
                    crate::mac_inspect::status(&vm)
                )
            })?;
        Ok((ip, crate::mac_health::user_of(&vm)))
    }
}

impl Runner for Live<'_> {
    async fn exec(&mut self, instance: &str, text: &str) -> Result<()> {
        let (ip, user) = self.target(instance).await?;
        crate::run::ssh_to(&ip, &user, 22, true, &[text.to_string()]).await
    }
    async fn upload(&mut self, instance: &str, local: &Path, guest: &str) -> Result<()> {
        let (ip, user) = self.target(instance).await?;
        let t = crate::mac_cp::Transfer {
            name: instance.to_string(),
            guest_path: guest.to_string(),
            local: local.to_path_buf(),
            upload: true,
        };
        crate::mac_cp::copy(&t, &user, &ip).await
    }
    async fn rebuild(&mut self, sp: &ServicePlan) -> Result<()> {
        crate::mac_compose::rebuild_service(self.r, &self.store, sp, self.wait).await
    }
}

/// The command each service runs (its `command:`, else the image's), found once from the store.
fn commands(
    plan: &Resolved,
    store: &crate::mac_store::Store,
) -> Result<BTreeMap<String, Option<String>>> {
    plan.services
        .iter()
        .map(|s| {
            let cfg = mac_config::load(&store.resolve(&s.spec.image)?)?;
            Ok((
                s.service.clone(),
                mac_config::effective_command(&cfg, &s.command, &s.env, s.workdir.as_deref()),
            ))
        })
        .collect()
}

/// `compose watch`: `up` unless `no_up`, then poll every `interval` seconds until Ctrl-C (`polls` > 0 stops after that many).
pub async fn watch(
    r: &crate::remote::Remote,
    plan: Resolved,
    wait: u64,
    no_up: bool,
    interval: u64,
    polls: u64,
) -> Result<()> {
    let applies_restarts = plan
        .services
        .iter()
        .any(|s| s.spec.restart.is_some_and(|p| p != Restart::No) && !s.run_to_completion);
    let watched: Vec<&ServicePlan> = plan
        .services
        .iter()
        .filter(|s| s.replica == 1 && !s.watch.is_empty())
        .collect();
    if watched.is_empty() && !applies_restarts {
        return Err(coded(
            format!(
                "project {:?} has nothing to watch: no service has `develop: watch:` rules and none has a `restart:` policy",
                plan.project
            ),
            "MAC_COMPOSE_WATCH_NONE",
        ));
    }
    for s in &watched {
        validate_watchable(&s.watch)?;
    }
    let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
    if no_up {
        let have = crate::mac_compose::project_instances(r, &plan.project).await?;
        for s in &plan.services {
            if !have
                .iter()
                .any(|v| v["name"].as_str() == Some(&s.spec.name))
            {
                bail!(
                    "{} is not running, so there is nothing to watch: run `fluxctl mac compose watch` without --no-up, or `fluxctl mac compose up` first",
                    s.spec.name
                );
            }
        }
    } else {
        crate::mac_compose::up_project(r, &plan, wait).await?;
    }
    let commands = commands(&plan, &store)?;
    let mut snaps: BTreeMap<String, Vec<Snapshot>> = watched
        .iter()
        .map(|s| (s.service.clone(), s.watch.iter().map(scan).collect()))
        .collect();
    eprintln!(
        "watching {} service(s){} every {interval}s. Ctrl-C stops watching (the instances keep running; `fluxctl mac compose down` removes them).",
        watched.len(),
        if applies_restarts {
            " and applying restart policies"
        } else {
            ""
        }
    );
    let mut live = Live { r, store, wait };
    let mut book = RestartBook::default();
    let mut told: BTreeSet<String> = BTreeSet::new();
    let mut n = 0u64;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval.max(1))).await;
        n += 1;
        for s in &watched {
            let new: Vec<Snapshot> = s.watch.iter().map(scan).collect();
            let changes: Vec<_> = snaps[&s.service]
                .iter()
                .zip(&new)
                .map(|(o, n)| diff(o, n))
                .collect();
            let re = react(&s.watch, &changes);
            snaps.insert(s.service.clone(), new);
            if re.is_empty() {
                continue;
            }
            for inst in plan.services.iter().filter(|p| p.service == s.service) {
                match apply(&mut live, inst, commands[&s.service].as_deref(), &re).await {
                    Ok(line) => println!("{line}"),
                    Err(e) => eprintln!("{}: {e:#} (still watching)", inst.spec.name),
                }
            }
        }
        if applies_restarts {
            let vms = crate::mac_compose::project_instances(r, &plan.project)
                .await
                .unwrap_or_default();
            for v in revivals(&vms, &plan, &mut book) {
                if !v.allowed {
                    if told.insert(v.name.clone()) {
                        eprintln!(
                            "{}: stopped again after {} restart(s), the limit of `restart: on-failure:{}`; not starting it again. `fluxctl mac compose start {}` starts it by hand",
                            v.name,
                            v.tries,
                            v.limit.unwrap_or(0),
                            v.service
                        );
                    }
                    continue;
                }
                match crate::mac_more::run_daemon(
                    r,
                    crate::mac_more::MacMore::Start {
                        names: vec![v.name.clone()],
                    },
                )
                .await
                {
                    Ok(()) => {
                        eprintln!(
                            "{}: it had stopped, started it again (restart policy, try {}{})",
                            v.name,
                            v.tries,
                            v.limit.map_or(String::new(), |l| format!(" of {l}"))
                        );
                        if let Some(Some(c)) = commands.get(&v.service) {
                            let wait_s = wait;
                            let user = crate::mac_health::user_of(
                                &crate::mac_cmd::find_instance(r, &v.name).await?,
                            );
                            let job = crate::mac_instance::GuestJob {
                                user,
                                wait: wait_s,
                                command: Some(detach_pid(c)),
                                mounts: vec![],
                                detach: false,
                                ..Default::default()
                            };
                            if let Err(e) = crate::mac_instance::run_in_guest(r, &v.id, &job).await
                            {
                                eprintln!(
                                    "{}: started, but its command did not run: {e:#}",
                                    v.name
                                );
                            }
                        }
                    }
                    Err(e) => eprintln!("{}: could not start it again: {e:#}", v.name),
                }
            }
        }
        if polls > 0 && n >= polls {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(y: &str, has_build: bool) -> Result<Vec<WatchRule>> {
        let d: Develop = serde_yaml::from_str(y).unwrap();
        d.rules("web", Path::new("/proj"), has_build)
    }

    #[test]
    fn globs() {
        assert!(glob_match("node_modules", "a/node_modules/x.js"));
        assert!(glob_match("*.log", "x/y.log"));
        assert!(!glob_match("*.log", "x/y.txt"));
        assert!(glob_match("build/**", "build/a/b"));
        assert!(glob_match("src/*.tmp", "src/a.tmp"));
        assert!(!glob_match("src/*.tmp", "src/a/b.tmp"));
    }

    #[test]
    fn rules_are_checked_with_plain_messages() {
        let ok = rules(
            "watch: [{action: sync+restart, path: ./src, target: /app/}, {action: rebuild, path: Package.swift}]",
            true,
        )
        .unwrap();
        assert_eq!(ok[0].path, Path::new("/proj/src"));
        assert_eq!(ok[0].target.as_deref(), Some("/app"));
        assert_eq!(ok[1].action, WatchAction::Rebuild);
        for (y, want) in [
            ("watch: [{action: sync, path: src}]", "absolute `target:`"),
            (
                "watch: [{action: sync, path: src, target: rel}]",
                "absolute `target:`",
            ),
            ("watch: [{action: rebuild, path: src}]", "`build:`"),
            ("watch: [{action: dance, path: src}]", "not one of sync"),
            ("watch: [{action: restart, path: ''}]", "empty"),
        ] {
            let e = format!("{:#}", rules(y, false).unwrap_err());
            assert!(
                e.contains(want) && e.contains("MAC_COMPOSE_WATCH_RULE"),
                "{y}: {e}"
            );
        }
        assert!(
            serde_yaml::from_str::<Develop>("watch: [{action: sync, path: a, bogus: 1}]").is_err()
        );
    }

    fn put(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn snapshots_find_added_changed_and_removed_files_and_honour_ignore() {
        let d = tempfile::tempdir().unwrap();
        put(&d.path().join("a.txt"), "1");
        put(&d.path().join("sub/b.txt"), "1");
        put(&d.path().join("node_modules/x.js"), "1");
        put(&d.path().join(".git/HEAD"), "1");
        let rule = WatchRule {
            path: d.path().to_path_buf(),
            action: WatchAction::Sync,
            target: Some("/app".into()),
            ignore: vec!["node_modules".into()],
        };
        let one = scan(&rule);
        assert_eq!(one.len(), 2, "{one:?}");
        put(&d.path().join("a.txt"), "longer");
        put(&d.path().join("c.txt"), "new");
        std::fs::remove_file(d.path().join("sub/b.txt")).unwrap();
        let two = scan(&rule);
        let ch = diff(&one, &two);
        assert!(ch.contains(&Change::Changed(d.path().join("a.txt"))));
        assert!(ch.contains(&Change::Changed(d.path().join("c.txt"))));
        assert!(ch.contains(&Change::Removed(d.path().join("sub/b.txt"))));
        assert!(diff(&two, &two).is_empty());
    }

    fn rule(action: WatchAction, target: Option<&str>) -> WatchRule {
        WatchRule {
            path: PathBuf::from("/proj/src"),
            action,
            target: target.map(String::from),
            ignore: vec![],
        }
    }

    #[test]
    fn reactions_map_files_into_the_guest_and_rebuild_wins() {
        let ch = |p: &str| vec![Change::Changed(PathBuf::from(p))];
        let r = react(
            &[rule(WatchAction::SyncRestart, Some("/app"))],
            &[ch("/proj/src/a/b.swift")],
        );
        assert_eq!(
            r.copies,
            [(
                PathBuf::from("/proj/src/a/b.swift"),
                "/app/a/b.swift".to_string()
            )]
        );
        assert!(r.restart && !r.rebuild);
        let r = react(
            &[rule(WatchAction::Sync, Some("/app"))],
            &[vec![Change::Removed(PathBuf::from("/proj/src/old.swift"))]],
        );
        assert_eq!(r.removes, ["/app/old.swift"]);
        assert!(!r.restart);
        let r = react(
            &[
                rule(WatchAction::Sync, Some("/app")),
                rule(WatchAction::Rebuild, None),
            ],
            &[ch("/proj/src/a"), ch("/proj/src/Package.swift")],
        );
        assert_eq!(
            r,
            Reaction {
                rebuild: true,
                ..Default::default()
            }
        );
        assert!(react(&[rule(WatchAction::Restart, None)], &[vec![]]).is_empty());
    }

    #[derive(Default)]
    struct Rec(Vec<String>);

    impl Runner for Rec {
        async fn exec(&mut self, i: &str, t: &str) -> Result<()> {
            self.0.push(format!("exec {i} {t}"));
            Ok(())
        }
        async fn upload(&mut self, i: &str, l: &Path, g: &str) -> Result<()> {
            self.0.push(format!("upload {i} {} {g}", l.display()));
            Ok(())
        }
        async fn rebuild(&mut self, sp: &ServicePlan) -> Result<()> {
            self.0.push(format!("rebuild {}", sp.spec.name));
            Ok(())
        }
    }

    fn plan_of(y: &str) -> Resolved {
        crate::mac_compose::resolve_text(y).unwrap()
    }

    #[tokio::test]
    async fn a_sync_then_restart_copies_first_and_stops_the_old_command_before_the_new() {
        let r = plan_of("name: p\nservices:\n  web:\n    image: w:1\n    command: ./serve\n");
        let sp = &r.services[0];
        let mut run = Rec::default();
        let re = Reaction {
            copies: vec![(PathBuf::from("/proj/src/a"), "/app/d/a".into())],
            restart: true,
            ..Default::default()
        };
        let line = apply(&mut run, sp, Some("./serve"), &re).await.unwrap();
        assert!(
            line.contains("synced 1 file(s)") && line.contains("ran its command again"),
            "{line}"
        );
        assert!(
            run.0[0].contains("mkdir -p '/app/d'") || run.0[0].contains("mkdir -p /app/d"),
            "{:?}",
            run.0
        );
        assert!(run.0[1].starts_with("upload p-web /proj/src/a /app/d/a"));
        assert!(
            run.0[2].contains("pkill")
                && run.0[3].contains("fluxvm-compose.pid")
                && run.0[3].contains("nohup")
        );
        let mut run = Rec::default();
        apply(
            &mut run,
            sp,
            None,
            &Reaction {
                restart: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(run.0.is_empty(), "no command: nothing to run again");
        let mut run = Rec::default();
        let line = apply(
            &mut run,
            sp,
            Some("x"),
            &Reaction {
                rebuild: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(run.0, ["rebuild p-web"]);
        assert!(line.contains("rebuilt"));
    }

    fn vm(name: &str, service: &str, status: &str, policy: &str) -> serde_json::Value {
        serde_json::json!({"id": format!("id-{name}"), "name": name, "status": status,
            "labels": {SERVICE_LABEL: service, crate::mac_inspect::RESTART_LABEL: policy}})
    }

    #[test]
    fn on_failure_n_stops_after_n_restarts_and_one_shots_are_left_stopped() {
        let r = plan_of(
            "name: p\nservices:\n  web:\n    image: w:1\n    restart: 'on-failure:2'\n  job:\n    image: j:1\n    command: ./once\n    restart: always\n  app:\n    image: a:1\n    restart: always\n    depends_on:\n      job: {condition: service_completed_successfully}\n",
        );
        let mut book = RestartBook::default();
        let vms = vec![
            vm("p-web", "web", "failed", "on-failure"),
            vm("p-job", "job", "stopped", "always"),
            vm("p-app", "app", "running", "always"),
        ];
        for _ in 0..2 {
            let v = revivals(&vms, &r, &mut book);
            assert_eq!(v.len(), 1, "{v:?}");
            assert!(v[0].allowed && v[0].name == "p-web" && v[0].limit == Some(2));
        }
        let v = revivals(&vms, &r, &mut book);
        assert!(!v[0].allowed && v[0].tries == 2, "{v:?}");
    }

    #[test]
    fn detach_records_a_pid_and_quotes_the_command() {
        let s = detach_pid("echo 'hi there'");
        assert!(s.contains("/tmp/fluxvm-compose.pid") && s.contains("nohup"));
        assert!(s.contains("hi there"));
    }
}
