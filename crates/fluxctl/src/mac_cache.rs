// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Build cache for `fluxctl mac build`.
//!
//! After every `RUN`/`COPY` the build instance is stopped and its disk stored as an intermediate template named
//! `fluxvm-cache/<key>:c` in the image store. The key chains: `key(step i) = sha256(key(step i-1), step text, COPY context hash)`,
//! starting from the identity of the `FROM` template (its registry digest, or its store directory for a local import). So a
//! later build with the same prefix of steps on the same base restarts from the longest cached prefix instead of from `FROM`.
//!
//! Costs, stated plainly: each checkpoint stops the guest and boots it again from the stored disk (a macOS boot is minutes),
//! and each entry is a full disk clone (cheap on APFS while it shares blocks, a full copy elsewhere). `mac build --no-cache`
//! runs the old straight-through build; `mac cache prune` and a size budget (`FLUXVM_MAC_CACHE_MAX`, default 100g) bound the disk.
//! A `RUN` that is not deterministic (`curl`, `apt`-style updates) is cached like any other: use `--no-cache` to refresh.
//!
//! Not run against a real macOS guest: the executor is tested with a fake host.

use crate::mac_build::Step;
use crate::mac_build_exec::{BuildHost, check_supported, execute};
use crate::mac_store::{Entry, REQUIRED_FILES, Store};
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;

pub const PREFIX: &str = "fluxvm-cache/";
/// Dot-file in a cache directory describing the step (dot-files are never pushed).
const STEP_FILE: &str = ".cache-step";
const DEFAULT_BUDGET_MIB: u64 = 100 * 1024;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex(&h.finalize())
}

/// The store name for a cache key.
pub fn cache_ref(key: &str) -> String {
    format!("{PREFIX}{}:c", &key[..32.min(key.len())])
}

pub fn is_cache_ref(name: &str) -> bool {
    name.starts_with(PREFIX)
}

/// Identity of a template: its registry digest, else its (unique) store directory.
pub fn base_id(store: &Store, reference: &str) -> Result<String> {
    let key = crate::mac_store::normalize(reference)?;
    for (n, e) in store.list()? {
        if n == key {
            return Ok(if e.digest.is_empty() {
                format!("dir:{}", e.dir)
            } else {
                e.digest
            });
        }
    }
    bail!("no image {key} to build from (fluxctl mac images)")
}

/// Split a plan into its base reference and the steps that change the disk. Anything but
/// `CloneTemplate, (RUN|COPY)*, StoreTemplate` is not cacheable.
pub fn split(steps: &[Step]) -> Option<(&str, &[Step])> {
    let [
        Step::CloneTemplate { reference },
        work @ ..,
        Step::StoreTemplate,
    ] = steps
    else {
        return None;
    };
    work.iter()
        .all(|s| matches!(s, Step::Run { .. } | Step::Copy { .. }))
        .then_some((reference.as_str(), work))
}

/// The cache keys of `work`, one per step. `ctx` hashes a COPY source (see [`hash_context`]).
pub fn chain_keys(
    base: &str,
    work: &[Step],
    ctx: &dyn Fn(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut parent = sha(&["base", base]);
    for s in work {
        let step = match s {
            Step::Run {
                user,
                command,
                mounts,
                ..
            } => {
                let user = user.as_deref().unwrap_or("");
                if mounts.is_empty() {
                    sha(&["run", user, command])
                } else {
                    // Which secrets a step can read is part of what it does; their contents are not (as in Docker).
                    let m = format!("{mounts:?}");
                    sha(&["run", user, command, &m])
                }
            }
            Step::Copy { user, src, dst, .. } => {
                sha(&["copy", user.as_deref().unwrap_or(""), src, dst, &ctx(src)?])
            }
            _ => bail!("only RUN and COPY steps are cached"),
        };
        parent = sha(&["step", &parent, &step]);
        keys.push(parent.clone());
    }
    Ok(keys)
}

/// sha256 over a file or directory tree (names, sizes, contents, symlink targets), in sorted order.
pub fn hash_context(path: &Path) -> Result<String> {
    fn walk(p: &Path, rel: &str, h: &mut Sha256) -> Result<()> {
        let md = std::fs::symlink_metadata(p)?;
        if md.file_type().is_symlink() {
            h.update(format!("l\0{rel}\0{}\0", std::fs::read_link(p)?.display()).as_bytes());
        } else if md.is_dir() {
            h.update(format!("d\0{rel}\0").as_bytes());
            let mut names: Vec<_> = std::fs::read_dir(p)?
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            for n in names {
                walk(&p.join(&n), &format!("{rel}/{n}"), h)?;
            }
        } else {
            use std::io::Read;
            h.update(format!("f\0{rel}\0{}\0", md.len()).as_bytes());
            let mut f = std::fs::File::open(p)?;
            let mut buf = vec![0u8; 1 << 20];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
        }
        Ok(())
    }
    ensure!(
        path.exists(),
        "COPY source {} does not exist",
        path.display()
    );
    let mut h = Sha256::new();
    walk(path, "", &mut h)?;
    Ok(hex(&h.finalize()))
}

/// A build host that can also read and write the cache.
pub trait CachedHost: BuildHost {
    fn cache_has(&self, cache_ref: &str) -> bool;
    /// Refresh the entry's age so pruning keeps what is in use.
    fn cache_touch(&mut self, cache_ref: &str);
    /// Stop the instance and store its disk as `cache_ref`; with `restart`, delete it and boot a new one from that entry
    /// (so later steps run on exactly what is cached).
    async fn checkpoint(&mut self, cache_ref: &str, describe: &str, restart: bool) -> Result<()>;
    /// Make the final template from the cache entry (clones the files, so the entry stays intact).
    fn materialize(&mut self, cache_ref: &str) -> Result<()>;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheReport {
    /// Steps answered by the cache.
    pub cached: usize,
    /// Steps that ran.
    pub ran: usize,
}

fn describe(s: &Step) -> String {
    match s {
        Step::Run { command, .. } => {
            format!("RUN {}", command.chars().take(200).collect::<String>())
        }
        Step::Copy { src, dst, .. } => format!("COPY {src} {dst}"),
        _ => String::new(),
    }
}

/// Run a build with the cache. `keys` come from [`chain_keys`]. Falls back to the plain executor for plans it cannot cache.
pub async fn execute_cached<H: CachedHost>(
    host: &mut H,
    steps: &[Step],
    keys: &[String],
) -> Result<CacheReport> {
    check_supported(steps)?;
    let Some((base, work)) = split(steps) else {
        execute(host, steps).await?;
        return Ok(CacheReport::default());
    };
    ensure!(work.len() == keys.len(), "one cache key per step");
    if work.is_empty() {
        execute(host, steps).await?;
        return Ok(CacheReport::default());
    }
    let n = work.len();
    // Longest cached prefix (a key already encodes everything before it, so presence is enough).
    let hit = (1..=n)
        .rev()
        .find(|&j| host.cache_has(&cache_ref(&keys[j - 1])))
        .unwrap_or(0);
    for (i, s) in work.iter().enumerate().take(hit) {
        eprintln!("step {}/{n}: {} (cached)", i + 1, describe(s));
    }
    if hit > 0 {
        host.cache_touch(&cache_ref(&keys[hit - 1]));
    }
    if hit == n {
        host.materialize(&cache_ref(&keys[n - 1]))?;
        return Ok(CacheReport { cached: n, ran: 0 });
    }
    let start = if hit == 0 {
        base.to_string()
    } else {
        cache_ref(&keys[hit - 1])
    };
    eprintln!(
        "starting from {}",
        if hit == 0 { base } else { "the cache" }
    );
    let mut created = false;
    let mut outcome: Result<()> = async {
        host.create(&start).await?;
        created = true;
        host.wait_ready().await?;
        for i in hit..n {
            let what = describe(&work[i]);
            eprintln!("step {}/{n}: {what}", i + 1);
            let r = match &work[i] {
                Step::Run {
                    user,
                    command,
                    mounts,
                    ..
                } => {
                    host.run_mounted(
                        user.as_deref()
                            .unwrap_or(crate::mac_build_exec::DEFAULT_USER),
                        command,
                        mounts,
                    )
                    .await
                }
                Step::Copy { user, src, dst, .. } => {
                    host.copy(
                        user.as_deref()
                            .unwrap_or(crate::mac_build_exec::DEFAULT_USER),
                        src,
                        dst,
                    )
                    .await
                }
                _ => unreachable!("split() checked"),
            };
            r.map_err(|e| e.context(format!("build failed at step {} ({what})", i + 1)))?;
            host.checkpoint(&cache_ref(&keys[i]), &what, i + 1 < n)
                .await?;
        }
        Ok(())
    }
    .await;
    if created && let Err(e) = host.delete().await {
        eprintln!("warning: the build instance was not deleted: {e:#}");
    }
    if outcome.is_ok() {
        outcome = host.materialize(&cache_ref(&keys[n - 1]));
    }
    outcome?;
    Ok(CacheReport {
        cached: hit,
        ran: n - hit,
    })
}

/// Clone the cache entry's files into a new template named `key`.
pub fn materialize(store: &Store, cache_name: &str, key: &str) -> Result<()> {
    let src = store.resolve(cache_name)?;
    let name = format!("local-{}", crate::mac_cmd::short_id());
    let scratch = store.scratch("build");
    std::fs::create_dir_all(&scratch)?;
    for f in REQUIRED_FILES {
        if let Err(e) = crate::mac_cmd::clone_file(&src.join(f), &scratch.join(f)) {
            std::fs::remove_dir_all(&scratch).ok();
            return Err(e);
        }
    }
    store.adopt(&scratch, &name)?;
    store.add(
        key,
        Entry {
            dir: name,
            digest: String::new(),
            source: "build".into(),
            added_unix: 0,
        },
    )?;
    Ok(())
}

/// Record what a cache directory holds, for `mac cache ls`.
pub fn note_step(store: &Store, cache_name: &str, text: &str) {
    if let Ok(d) = store.resolve(cache_name) {
        std::fs::write(d.join(STEP_FILE), text).ok();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub bytes: u64,
    pub added_unix: u64,
    pub step: String,
}

pub fn list(store: &Store) -> Result<Vec<Item>> {
    let mut v: Vec<Item> = store
        .list()?
        .into_iter()
        .filter(|(n, _)| is_cache_ref(n))
        .map(|(name, e)| {
            let dir = store.root().join(&e.dir);
            Item {
                name,
                bytes: crate::mac_image_ops::dir_usage(&dir).1,
                added_unix: e.added_unix,
                step: std::fs::read_to_string(dir.join(STEP_FILE)).unwrap_or_default(),
            }
        })
        .collect();
    v.sort_by(|a, b| b.added_unix.cmp(&a.added_unix).then(a.name.cmp(&b.name)));
    Ok(v)
}

/// The size budget in bytes: `FLUXVM_MAC_CACHE_MAX` (`50g`, `512m`), default 100g.
pub fn budget_bytes() -> Result<u64> {
    let mib = match std::env::var("FLUXVM_MAC_CACHE_MAX") {
        Ok(v) if !v.trim().is_empty() => crate::mac_config::parse_memory_mib(&v)?,
        _ => DEFAULT_BUDGET_MIB,
    };
    Ok(mib * 1024 * 1024)
}

/// Which entries to drop: all of those older than `older_than_secs` (when given), then the oldest until the total fits
/// `budget`. `keep` is never chosen. Pure.
pub fn plan_prune(
    items: &[Item],
    budget: u64,
    older_than_secs: Option<u64>,
    now: u64,
    keep: &BTreeSet<String>,
    all: bool,
) -> Vec<String> {
    let mut drop: Vec<&Item> = Vec::new();
    let mut rest: Vec<&Item> = Vec::new();
    for i in items {
        let old = older_than_secs.is_some_and(|s| now.saturating_sub(i.added_unix) >= s);
        if !keep.contains(&i.name) && (all || old) {
            drop.push(i);
        } else {
            rest.push(i);
        }
    }
    let mut total: u64 = rest.iter().map(|i| i.bytes).sum();
    rest.sort_by_key(|i| i.added_unix); // oldest first
    for i in rest {
        if total <= budget {
            break;
        }
        if !keep.contains(&i.name) {
            total -= i.bytes;
            drop.push(i);
        }
    }
    drop.into_iter().map(|i| i.name.clone()).collect()
}

/// Remove `names` from the store; returns bytes freed (as measured before removal).
pub fn remove(store: &Store, names: &[String]) -> Result<u64> {
    let mut freed = 0;
    for n in names {
        if let Ok(d) = store.resolve(n) {
            freed += crate::mac_image_ops::dir_usage(&d).1;
        }
        store.remove(n)?;
    }
    Ok(freed)
}

/// Enforce the budget after a build; the entries of this build are kept.
pub fn enforce_budget(store: &Store, keys: &[String]) -> Result<(usize, u64)> {
    let keep: BTreeSet<String> = keys.iter().map(|k| cache_ref(k)).collect();
    let victims = plan_prune(
        &list(store)?,
        budget_bytes()?,
        None,
        crate::mac_image_ops::now_unix(),
        &keep,
        false,
    );
    let freed = remove(store, &victims)?;
    Ok((victims.len(), freed))
}

/// `mac cache ls` text.
pub fn render(items: &[Item], now: u64) -> String {
    if items.is_empty() {
        return "the build cache is empty\n".into();
    }
    let mut o = format!("{:<44}  {:>9}  {:>6}  STEP\n", "ENTRY", "SIZE", "AGE");
    for i in items {
        let age = now.saturating_sub(i.added_unix);
        let age = if age >= 86400 {
            format!("{}d", age / 86400)
        } else if age >= 3600 {
            format!("{}h", age / 3600)
        } else {
            format!("{}m", age / 60)
        };
        o.push_str(&format!(
            "{:<44}  {:>9}  {:>6}  {}\n",
            i.name,
            crate::mac_cmd::human(i.bytes),
            age,
            i.step.lines().next().unwrap_or("")
        ));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(c: &str) -> Step {
        Step::Run {
            line: 1,
            user: None,
            command: c.into(),
            mounts: Vec::new(),
        }
    }

    fn copy(src: &str) -> Step {
        Step::Copy {
            line: 2,
            user: Some("bob".into()),
            src: src.into(),
            dst: "/x".into(),
        }
    }

    fn plan(work: Vec<Step>) -> Vec<Step> {
        let mut v = vec![Step::CloneTemplate {
            reference: "base:1".into(),
        }];
        v.extend(work);
        v.push(Step::StoreTemplate);
        v
    }

    fn keys(base: &str, work: &[Step], ctx: &str) -> Vec<String> {
        chain_keys(base, work, &|_| Ok(ctx.to_string())).unwrap()
    }

    #[test]
    fn keys_chain_and_depend_on_base_step_and_context() {
        let w = vec![run("a"), copy("src"), run("b")];
        let k = keys("sha256:1", &w, "c1");
        assert_eq!(k.len(), 3);
        assert_eq!(k, keys("sha256:1", &w, "c1"), "deterministic");
        // A changed last step leaves earlier keys alone.
        let w2 = vec![run("a"), copy("src"), run("B")];
        let k2 = keys("sha256:1", &w2, "c1");
        assert_eq!(k[..2], k2[..2]);
        assert_ne!(k[2], k2[2]);
        // A changed middle step changes everything after it.
        let w3 = vec![run("a2"), copy("src"), run("b")];
        let k3 = keys("sha256:1", &w3, "c1");
        assert!(k.iter().zip(&k3).all(|(a, b)| a != b));
        // New base digest or new COPY content: the copy step and what follows change, the first does not.
        assert_ne!(k[0], keys("sha256:2", &w, "c1")[0]);
        let k4 = keys("sha256:1", &w, "c2");
        assert_eq!(k[0], k4[0]);
        assert_ne!(k[1], k4[1]);
        assert_ne!(k[2], k4[2]);
        // The Macfile line number does not matter.
        let moved = vec![
            Step::Run {
                line: 99,
                user: None,
                command: "a".into(),
                mounts: Vec::new(),
            },
            copy("src"),
            run("b"),
        ];
        assert_eq!(k, keys("sha256:1", &moved, "c1"));
    }

    #[test]
    fn split_accepts_only_clone_work_store() {
        assert!(split(&plan(vec![run("a")])).is_some());
        assert!(split(&[Step::InstallIpswLatest, Step::StoreTemplate]).is_none());
        let mut p = plan(vec![run("a")]);
        p.insert(
            1,
            Step::Snapshot {
                line: 3,
                tag: "t".into(),
            },
        );
        assert!(split(&p).is_none());
    }

    #[test]
    fn context_hash_sees_content_names_and_nested_files() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("ctx");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "one").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "two").unwrap();
        let h1 = hash_context(&dir).unwrap();
        assert_eq!(h1, hash_context(&dir).unwrap());
        std::fs::write(dir.join("sub/b.txt"), "tWo").unwrap();
        let h2 = hash_context(&dir).unwrap();
        assert_ne!(h1, h2);
        std::fs::rename(dir.join("a.txt"), dir.join("c.txt")).unwrap();
        assert_ne!(h2, hash_context(&dir).unwrap());
        assert!(hash_context(&d.path().join("missing")).is_err());
    }

    /// Fake host: an in-memory cache and a log of what happened.
    #[derive(Default)]
    struct Fake {
        cache: BTreeSet<String>,
        log: Vec<String>,
        fail_on: Option<String>,
        up: bool,
    }

    impl BuildHost for Fake {
        async fn create(&mut self, r: &str) -> Result<()> {
            self.log.push(format!("create {r}"));
            self.up = true;
            Ok(())
        }
        async fn wait_ready(&mut self) -> Result<()> {
            self.log.push("ready".into());
            Ok(())
        }
        async fn run(&mut self, _u: &str, c: &str) -> Result<()> {
            if self.fail_on.as_deref() == Some(c) {
                bail!("boom");
            }
            self.log.push(format!("run {c}"));
            Ok(())
        }
        async fn copy(&mut self, _u: &str, s: &str, _d: &str) -> Result<()> {
            self.log.push(format!("copy {s}"));
            Ok(())
        }
        async fn stop_and_store(&mut self) -> Result<()> {
            unreachable!("the cached path checkpoints instead")
        }
        async fn delete(&mut self) -> Result<()> {
            self.log.push("delete".into());
            self.up = false;
            Ok(())
        }
    }

    impl CachedHost for Fake {
        fn cache_has(&self, r: &str) -> bool {
            self.cache.contains(r)
        }
        fn cache_touch(&mut self, r: &str) {
            self.log
                .push(format!("touch {}", &r[PREFIX.len()..PREFIX.len() + 4]));
        }
        async fn checkpoint(&mut self, r: &str, _d: &str, restart: bool) -> Result<()> {
            self.cache.insert(r.to_string());
            self.log.push(format!("checkpoint restart={restart}"));
            Ok(())
        }
        fn materialize(&mut self, r: &str) -> Result<()> {
            self.log.push(format!(
                "materialize {}",
                &r[PREFIX.len()..PREFIX.len() + 4]
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_second_identical_build_runs_nothing_and_a_changed_tail_reruns_only_the_tail() {
        let steps = plan(vec![run("a"), run("b"), run("c")]);
        let (_, work) = split(&steps).unwrap();
        let k = keys("sha256:1", work, "");
        let mut h = Fake::default();
        let r = execute_cached(&mut h, &steps, &k).await.unwrap();
        assert_eq!(r, CacheReport { cached: 0, ran: 3 });
        assert_eq!(h.cache.len(), 3);
        assert_eq!(
            h.log
                .iter()
                .filter(|l| l.starts_with("checkpoint"))
                .collect::<Vec<_>>(),
            [
                "checkpoint restart=true",
                "checkpoint restart=true",
                "checkpoint restart=false"
            ]
        );
        assert!(h.log.first().unwrap() == "create base:1" && !h.up);

        // Same build again: no VM at all.
        h.log.clear();
        let r = execute_cached(&mut h, &steps, &k).await.unwrap();
        assert_eq!(r, CacheReport { cached: 3, ran: 0 });
        assert!(
            !h.log
                .iter()
                .any(|l| l.starts_with("create") || l.starts_with("run")),
            "{:?}",
            h.log
        );
        assert!(h.log.last().unwrap().starts_with("materialize"));

        // Last step changed: starts from the cache entry of step 2 and runs only step 3.
        h.log.clear();
        let steps2 = plan(vec![run("a"), run("b"), run("C")]);
        let (_, work2) = split(&steps2).unwrap();
        let k2 = keys("sha256:1", work2, "");
        let r = execute_cached(&mut h, &steps2, &k2).await.unwrap();
        assert_eq!(r, CacheReport { cached: 2, ran: 1 });
        assert!(
            h.log.iter().any(|l| l.starts_with("create fluxvm-cache/")),
            "{:?}",
            h.log
        );
        assert_eq!(
            h.log
                .iter()
                .filter(|l| l.starts_with("run "))
                .collect::<Vec<_>>(),
            ["run C"]
        );
    }

    #[tokio::test]
    async fn a_failing_step_deletes_the_instance_and_caches_only_what_finished() {
        let steps = plan(vec![run("a"), run("bad"), run("c")]);
        let (_, work) = split(&steps).unwrap();
        let k = keys("sha256:1", work, "");
        let mut h = Fake {
            fail_on: Some("bad".into()),
            ..Default::default()
        };
        let e = execute_cached(&mut h, &steps, &k).await.unwrap_err();
        assert!(format!("{e:#}").contains("step 2"), "{e:#}");
        assert_eq!(h.cache.len(), 1);
        assert!(!h.up, "instance deleted");
        assert!(!h.log.iter().any(|l| l.starts_with("materialize")));
    }

    #[test]
    fn prune_plan_honours_age_budget_and_keep() {
        let it = |n: &str, bytes: u64, at: u64| Item {
            name: format!("{PREFIX}{n}:c"),
            bytes,
            added_unix: at,
            step: String::new(),
        };
        let items = vec![
            it("a", 10, 100),
            it("b", 10, 200),
            it("c", 10, 300),
            it("d", 10, 400),
        ];
        let none = BTreeSet::new();
        // Budget 25 of 40: drop the two oldest.
        assert_eq!(
            plan_prune(&items, 25, None, 500, &none, false),
            vec![items[0].name.clone(), items[1].name.clone()]
        );
        // Older than 250s at now=500 => a and b; the rest (20) fits.
        let v = plan_prune(&items, 1000, Some(250), 500, &none, false);
        assert_eq!(v, vec![items[0].name.clone(), items[1].name.clone()]);
        // Keep protects an entry even from --all and from the budget.
        let keep = BTreeSet::from([items[0].name.clone()]);
        let v = plan_prune(&items, 0, None, 500, &keep, true);
        assert!(!v.contains(&items[0].name) && v.len() == 3);
        let v = plan_prune(&items, 5, None, 500, &keep, false);
        assert!(!v.contains(&items[0].name));
    }

    fn make_template(store: &Store, dir: &str) {
        let p = store.root().join(dir);
        std::fs::create_dir_all(&p).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(p.join(f), dir).unwrap();
        }
    }

    #[test]
    fn store_integration_base_id_materialize_list_and_remove() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("s")).unwrap();
        make_template(&store, "local-1");
        store
            .add(
                "base:1",
                Entry {
                    dir: "local-1".into(),
                    digest: String::new(),
                    source: "local".into(),
                    added_unix: 0,
                },
            )
            .unwrap();
        assert_eq!(base_id(&store, "base:1").unwrap(), "dir:local-1");
        assert!(base_id(&store, "nope:1").is_err());

        let c = cache_ref(&"ab".repeat(32));
        make_template(&store, "cache-dir");
        store
            .add(
                &c,
                Entry {
                    dir: "cache-dir".into(),
                    digest: String::new(),
                    source: "cache".into(),
                    added_unix: 5,
                },
            )
            .unwrap();
        note_step(&store, &c, "RUN make\nmore");
        let items = list(&store).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].step, "RUN make\nmore");
        assert!(render(&items, 10).contains("RUN make"));

        materialize(&store, &c, "out:1").unwrap();
        let out = store.resolve("out:1").unwrap();
        assert_ne!(
            out,
            store.resolve(&c).unwrap(),
            "a clone, not a shared directory"
        );
        assert_eq!(std::fs::read(out.join("disk.raw")).unwrap(), b"cache-dir");
        // Removing the cache entry leaves the built image intact.
        assert!(remove(&store, std::slice::from_ref(&c)).unwrap() > 0);
        assert!(store.resolve(&c).is_err());
        assert!(store.resolve("out:1").is_ok());
        // Plain images are never listed as cache.
        assert!(list(&store).unwrap().is_empty());
    }

    #[test]
    fn budget_reads_the_environment_variable() {
        // Not set in the test environment unless a developer exports it; only check the parser path.
        assert!(crate::mac_config::parse_memory_mib("50g").unwrap() == 50 * 1024);
        assert!(DEFAULT_BUDGET_MIB > 0);
    }
}
