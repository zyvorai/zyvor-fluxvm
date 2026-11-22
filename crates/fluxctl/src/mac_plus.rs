// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Image trust and housekeeping commands for `fluxctl mac` (docs/macos-containers.md, "Signing, cache, search and contents"):
//! `verify`, `sign`, `keys`, `cache`, `search`, `label`, `sbom`, plus the signing, licence-guard and chunk-reuse logic that
//! `mac push`, `mac pull`, `mac images` and `mac build` call into.
//!
//! Everything here is client-side. None of it has run against a real registry or a real macOS template: signing and
//! verification are tested with a fake signature source, the registry calls (`referrers`, `attach`, `catalog`) only against the
//! mock registries of `fluxvm-image`.

use crate::mac_cmd::{check_template, creds, dir_size, names_a_registry};
use crate::mac_store::{self, Entry, Store};
use crate::mac_trust::{self, RegistrySource, TrustStore};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use fluxvm_image as image;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// `--sign`, `--label` and the licence override of `mac push`.
#[derive(clap::Args, Debug, Default)]
pub struct PushExtra {
    /// Sign the pushed manifest with your key (`mac keys generate`) and attach the signature to the registry.
    #[arg(long)]
    pub sign: bool,
    /// Push to a well-known public registry (Docker Hub, GHCR, Quay, ...) anyway. Apple's licence terms may forbid it; see the
    /// warning. Also: FLUXVM_MAC_ALLOW_PUBLIC_REGISTRY=1.
    #[arg(long = "i-have-checked-the-licence")]
    pub licence_override: bool,
    /// Extra label recorded on the pushed image (`KEY=VALUE`, repeatable), on top of the Macfile's LABELs.
    #[arg(long = "label", value_name = "KEY=VALUE")]
    pub label: Vec<String>,
}

/// `--verify` and `--no-reuse` of `mac pull`.
#[derive(clap::Args, Debug, Default)]
pub struct PullExtra {
    /// Refuse the image unless it carries a signature from a trusted key (`mac keys trust`). Also:
    /// FLUXVM_MAC_REQUIRE_SIGNED=1. The digest that was verified is the one pulled.
    #[arg(long)]
    pub verify: bool,
    /// Download every chunk instead of reusing identical chunks from a local image of the same repository.
    #[arg(long)]
    pub no_reuse: bool,
}

/// `--filter` and `--all` of `mac images`.
#[derive(clap::Args, Debug, Default)]
pub struct ImagesExtra {
    /// Keep images matching `label=KEY`, `label=KEY=VALUE`, `reference=GLOB` (`*` wildcard) or `source=TEXT`. Repeatable; all
    /// must match.
    #[arg(short = 'f', long = "filter", value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
    /// Also show build-cache entries.
    #[arg(short = 'a', long)]
    pub all: bool,
    /// List the images of every Mac of this inventory (`fleet.yaml`) over ssh instead of this Mac's store.
    #[arg(long, value_name = "INVENTORY", num_args = 0..=1, default_missing_value = "fleet")]
    pub fleet: Option<std::path::PathBuf>,
    /// Table of allocated and apparent size and when each image was last used by `mac run`.
    #[arg(long)]
    pub usage: bool,
    /// Print each image with a template such as `{{.Reference}} {{.Size}}`, or `table {{.Reference}}\t{{.ID}}`; the Go-template
    /// subset of `mac inspect --format`. Fields: Reference, Repository, Tag, ID, Digest, Size, SizeBytes, AllocatedBytes, Source,
    /// Labels (use `{{index .Labels "key"}}`), AddedUnix, LastUsedUnix. `{{json .}}` prints the whole record.
    #[arg(long, value_name = "TEMPLATE", conflicts_with_all = ["json", "usage", "fleet"])]
    pub format: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum KeysCommand {
    /// Create your signing keypair (kept in the trust directory, private key mode 0600) and print the public key.
    Generate,
    /// List the trusted public keys (and your own, as `self`).
    Ls,
    /// Trust a public key: `mac keys trust NAME FILE` (FILE holds the base64 key; `-` reads stdin).
    Trust { name: String, file: PathBuf },
    /// Stop trusting a key.
    Untrust { name: String },
}

#[derive(Subcommand, Debug)]
pub enum CacheCommand {
    /// List build-cache entries (intermediate templates), newest first.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove build-cache entries: those older than `--older-than`, `--all`, and then the oldest until the total fits
    /// FLUXVM_MAC_CACHE_MAX (default 100g).
    Prune {
        /// Remove entries older than this (`7d`, `12h`, `90m`).
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
        /// Remove every entry.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum MacPlus {
    /// Check that a registry image (or a pulled one) carries a signature from a trusted key. Exit status 1 when it does not.
    /// REFERENCE that names a registry is checked there; a local name is checked against the signatures kept at pull time.
    Verify {
        reference: String,
        #[arg(long)]
        json: bool,
    },
    /// Sign an image that is already in a registry (`push --sign` does it as part of the push).
    Sign { reference: String },
    /// Your signing key and the keys you trust.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// The build cache of `mac build`.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// List repositories of a registry that serves `/v2/_catalog`, optionally filtered by TERM (substring). Many public
    /// registries do not serve it.
    Search {
        registry: String,
        term: Option<String>,
        /// Also list each match's tags.
        #[arg(long)]
        tags: bool,
        /// Stop after this many matches.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Set or remove labels on a local image: `mac label IMAGE k=v ...`, `-k KEY` to remove. Names that share the image's
    /// files share its labels.
    Label {
        reference: String,
        labels: Vec<String>,
        #[arg(short = 'k', long = "remove", value_name = "KEY")]
        remove: Vec<String>,
    },
    /// The contents of an image as JSON: every file of the template bundle with its size and sha256, plus labels and history.
    /// Describes the bundle, not the files inside the guest's disk.
    Sbom {
        reference: String,
        /// Write to FILE instead of stdout.
        #[arg(long = "to", value_name = "FILE")]
        output: Option<PathBuf>,
        /// Also attach it to this registry image as a referrer (artifact type application/vnd.zyvor.fluxvm.contents.v1+json).
        #[arg(long, value_name = "REGISTRY_REF")]
        attach: Option<String>,
    },
}

pub async fn run(store: &Store, cmd: MacPlus) -> Result<()> {
    let trust = || TrustStore::open(mac_trust::trust_dir(store.root()));
    match cmd {
        MacPlus::Keys { command } => keys(&trust()?, command)?,
        MacPlus::Cache { command } => cache(store, command)?,
        MacPlus::Label {
            reference,
            labels,
            remove,
        } => {
            let l = label(store, &reference, &labels, &remove)?;
            println!(
                "{}",
                if l.is_empty() {
                    "no labels".to_string()
                } else {
                    l.iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            );
        }
        MacPlus::Sbom {
            reference,
            output,
            attach,
        } => sbom_cmd(store, &reference, output.as_deref(), attach.as_deref()).await?,
        MacPlus::Search {
            registry,
            term,
            tags,
            limit,
        } => {
            let repos = image::oci_manage::catalog(&registry, creds(&registry), None).await?;
            for r in filter_repos(&repos, term.as_deref(), limit) {
                if tags {
                    let t = image::oci_manage::list_tags(
                        &format!("{registry}/{r}"),
                        creds(&registry),
                        None,
                    )
                    .await
                    .map(|t| t.join(","))
                    .unwrap_or_else(|e| format!("(tags unavailable: {e})"));
                    println!("{registry}/{r}  {t}");
                } else {
                    println!("{registry}/{r}");
                }
            }
        }
        MacPlus::Sign { reference } => {
            ensure!(
                names_a_registry(&reference),
                "{reference:?} is not a registry reference (e.g. registry.example/acme/mac:15)"
            );
            let rec = sign_remote(&trust()?, &reference).await?;
            println!("signed {reference} ({})", rec.digest);
        }
        MacPlus::Verify { reference, json } => {
            let t = trust()?;
            let out = if names_a_registry(&reference) {
                let v = mac_trust::verify_remote(
                    &RegistrySource {
                        creds: creds(&reference),
                    },
                    &reference,
                    &t,
                )
                .await;
                v.map(|v| (v.digest, v.signer))
            } else {
                verify_local(store, &t, &reference)
            };
            match out {
                Ok((digest, signer)) => {
                    if json {
                        println!(
                            "{}",
                            json!({"reference": reference, "verified": true, "digest": digest, "signer": signer})
                        );
                    } else {
                        println!("verified {reference} ({digest}), signed by {signer}");
                    }
                }
                Err(e) => {
                    if json {
                        println!(
                            "{}",
                            json!({"reference": reference, "verified": false, "error": format!("{e:#}")})
                        );
                    }
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

fn keys(t: &TrustStore, c: KeysCommand) -> Result<()> {
    match c {
        KeysCommand::Generate => {
            let public = t.generate()?;
            println!("public key (give this to people who should trust your images):\n{public}");
        }
        KeysCommand::Ls => {
            let l = t.list()?;
            if l.is_empty() {
                println!("no trusted keys (mac keys generate, mac keys trust NAME FILE)");
            }
            for (n, k) in l {
                println!("{n:<16} {}  {k}", mac_trust::fingerprint(&k));
            }
        }
        KeysCommand::Trust { name, file } => {
            let key = if file.as_os_str() == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
            };
            t.trust(&name, &key)?;
            println!("trusting {name} ({})", mac_trust::fingerprint(&key));
        }
        KeysCommand::Untrust { name } => {
            t.untrust(&name)?;
            println!("no longer trusting {name}");
        }
    }
    Ok(())
}

/// Sign what `reference` points at in its registry and attach the signature as a referrer.
pub async fn sign_remote(t: &TrustStore, reference: &str) -> Result<mac_trust::SigRecord> {
    let r = image::oci_registry::ImageRef::parse(reference)?;
    let mut reg = image::oci_manage::Registry::new(creds(reference))?;
    let digest = reg.get_manifest(&r, &r.reference).await?.digest;
    let rec = t.sign_digest(&digest)?;
    attach_signature(&r, &digest, &rec).await?;
    let mut all = t.load_signatures(&digest)?;
    all.retain(|x| x.signer != rec.signer);
    all.push(rec.clone());
    t.save_signatures(&digest, &all)?;
    Ok(rec)
}

async fn attach_signature(
    r: &image::oci_registry::ImageRef,
    digest: &str,
    rec: &mac_trust::SigRecord,
) -> Result<()> {
    let pinned = image::oci_registry::ImageRef {
        reference: digest.to_string(),
        ..r.clone()
    };
    let tmp = TempFile::write("signature", &serde_json::to_vec_pretty(rec)?)?;
    image::oci_manage::attach(
        &pinned.to_string(),
        tmp.path(),
        &image::oci_manage::AttachOptions {
            artifact_type: mac_trust::SIG_TYPE.into(),
            layer_media_type: None,
            annotations: mac_trust::record_annotations(rec),
        },
        creds(&r.registry),
    )
    .await
    .context("attaching the signature (does the registry accept OCI artifacts with a subject?)")?;
    Ok(())
}

/// A file in the system temp directory, removed on drop (`attach` takes a path).
struct TempFile(PathBuf);

impl TempFile {
    fn write(label: &str, bytes: &[u8]) -> Result<Self> {
        let p = std::env::temp_dir().join(format!(
            "fluxvm-{label}-{}-{}.json",
            std::process::id(),
            crate::mac_cmd::short_id()
        ));
        std::fs::write(&p, bytes).with_context(|| format!("writing {}", p.display()))?;
        Ok(Self(p))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// Verify a local image against the signatures kept when it was pulled or pushed.
pub(crate) fn verify_local(
    store: &Store,
    t: &TrustStore,
    reference: &str,
) -> Result<(String, String)> {
    let key = mac_store::normalize(reference)?;
    let (_, e) = store
        .list()?
        .into_iter()
        .find(|(n, _)| *n == key)
        .with_context(|| format!("no image {key} (fluxctl mac images)"))?;
    ensure!(
        !e.digest.is_empty(),
        "{key} was not pulled from or pushed to a registry, so it has no manifest digest to verify; push it with --sign first"
    );
    let trusted = t.parsed()?;
    ensure!(
        !trusted.is_empty(),
        "no trusted keys: `fluxctl mac keys trust NAME FILE`"
    );
    let recs = t.load_signatures(&e.digest)?;
    let signer =
        mac_trust::require_trusted(&key, &mac_trust::evaluate(&e.digest, &recs, &trusted))?;
    Ok((e.digest, signer))
}

// ---------------------------------------------------------------------------------------------------------------------
// push and pull

/// Labels as push annotations (`dev.zyvor.fluxvm.label.KEY`).
pub fn label_annotations(
    cfg: &std::collections::BTreeMap<String, String>,
    extra: &[String],
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut all = cfg.clone();
    for kv in extra {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--label {kv:?}: expected KEY=VALUE"))?;
        ensure!(valid_label_key(k), "--label {kv:?}: bad key");
        all.insert(k.to_string(), v.to_string());
    }
    Ok(all
        .into_iter()
        .map(|(k, v)| (format!("dev.zyvor.fluxvm.label.{k}"), v))
        .collect())
}

fn valid_label_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
}

pub async fn push(store: &Store, reference: &str, target: &str, extra: &PushExtra) -> Result<()> {
    let dir = store.resolve(reference)?;
    let allow = extra.licence_override
        || std::env::var("FLUXVM_MAC_ALLOW_PUBLIC_REGISTRY").is_ok_and(|v| v == "1");
    // Everything that can refuse runs before a byte is uploaded.
    let warning = mac_trust::licence_check(target, allow)?;
    let trust = TrustStore::open(mac_trust::trust_dir(store.root()))?;
    if extra.sign {
        trust.private_key()?;
    }
    let cfg = crate::mac_config::load(&dir)?;
    let mut annotations = label_annotations(&cfg.labels, &extra.label)?;
    if let Some(w) = &warning {
        eprintln!("{w}");
        annotations.insert(
            mac_trust::ANN_LICENCE_ACK.into(),
            "public-registry-override".into(),
        );
    }
    let opts = image::vm_artifact::PushOptions {
        chunk_size: 0,
        exclude: Vec::new(),
        os: Some("macos".into()),
        name: Some(reference.to_string()),
        annotations,
    };
    let out = image::vm_artifact::push(&dir, target, creds(target), &opts)
        .await
        .with_context(|| format!("pushing {} to {target}", dir.display()))?;
    println!("{}", serde_json::to_string_pretty(&out)?);
    if extra.sign {
        let rec = trust.sign_digest(&out.manifest_digest)?;
        let r = image::oci_registry::ImageRef::parse(target)?;
        attach_signature(&r, &out.manifest_digest, &rec).await?;
        trust.save_signatures(&out.manifest_digest, &[rec])?;
        eprintln!("signed {target} ({})", out.manifest_digest);
    }
    Ok(())
}

/// Local images of the same repository (tag differs) whose chunks a pull may reuse, newest first, at most `max`.
pub fn seed_dirs(
    store_root: &Path,
    rows: &[(String, Entry)],
    key: &str,
    max: usize,
) -> Vec<PathBuf> {
    let repo = |k: &str| -> String {
        let last = k.rsplit('/').next().unwrap_or(k);
        match (k.split('@').next(), last.contains(':')) {
            (Some(base), true) if !k.contains('@') => {
                base.rsplit_once(':').map_or(base, |(r, _)| r).to_string()
            }
            (Some(base), _) => base.to_string(),
            _ => k.to_string(),
        }
    };
    let want = repo(key);
    let mut c: Vec<&(String, Entry)> = rows
        .iter()
        .filter(|(n, _)| n != key && !crate::mac_cache::is_cache_ref(n) && repo(n) == want)
        .collect();
    c.sort_by(|a, b| b.1.added_unix.cmp(&a.1.added_unix));
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, e) in c {
        let d = store_root.join(&e.dir);
        if d.join("disk.raw").is_file() && !out.contains(&d) {
            out.push(d);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

pub async fn pull(store: &Store, reference: &str, extra: &PullExtra) -> Result<()> {
    let key = mac_store::normalize(reference)?;
    let require =
        extra.verify || std::env::var("FLUXVM_MAC_REQUIRE_SIGNED").is_ok_and(|v| v == "1");
    let trust = TrustStore::open(mac_trust::trust_dir(store.root()))?;
    let mut target = key.clone();
    let mut verified = None;
    if require {
        let v =
            mac_trust::verify_remote(&RegistrySource { creds: creds(&key) }, &key, &trust).await?;
        // Pull exactly the digest that was verified: the tag may move between the check and the download.
        let r = image::oci_registry::ImageRef::parse(&key)?;
        target = image::oci_registry::ImageRef {
            reference: v.digest.clone(),
            ..r
        }
        .to_string();
        eprintln!("signature verified ({}), signed by {}", v.digest, v.signer);
        verified = Some(v);
    }
    let seeds = if extra.no_reuse {
        Vec::new()
    } else {
        seed_dirs(store.root(), &store.list()?, &key, 2)
    };
    let scratch = store.scratch("pull");
    let report = image::vm_artifact::pull_with(
        &target,
        &scratch,
        creds(&target),
        &image::vm_artifact::PullOptions { seeds },
    )
    .await
    .with_context(|| format!("pulling {key}"))?;
    let adopted = check_template(&scratch)
        .and_then(|()| mac_store::dir_for_digest(&report.manifest_digest))
        .and_then(|dir| store.adopt(&scratch, &dir).map(|()| dir));
    let dir = match adopted {
        Ok(d) => d,
        Err(e) => {
            std::fs::remove_dir_all(&scratch).ok();
            return Err(e);
        }
    };
    store.add(
        &key,
        Entry {
            dir,
            digest: report.manifest_digest.clone(),
            source: key.clone(),
            added_unix: 0,
        },
    )?;
    if let Some(v) = verified {
        trust.save_signatures(&v.digest, &v.records)?;
    }
    if report.chunks_reused > 0 {
        println!(
            "pulled {key} ({}); reused {} chunks ({}) from a local image, downloaded {}",
            report.manifest_digest,
            report.chunks_reused,
            crate::mac_cmd::human(report.bytes_reused),
            crate::mac_cmd::human(report.bytes_downloaded)
        );
    } else {
        println!("pulled {key} ({})", report.manifest_digest);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// images, labels

/// Glob with `*` only.
fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let mut rest = s;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(p) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            match rest.find(p) {
                Some(at) => rest = &rest[at + p.len()..],
                None => return false,
            }
        }
    }
    true
}

/// One `--filter`. `labels` are the image's labels.
pub fn matches_filter(
    f: &str,
    name: &str,
    e: &Entry,
    labels: &std::collections::BTreeMap<String, String>,
) -> Result<bool> {
    let (k, v) = f.split_once('=').with_context(|| {
        format!(
            "--filter {f:?}: expected KEY=VALUE (label=, reference=, source=); see `fluxctl explain MAC_FILTER_INVALID`"
        )
    })?;
    Ok(match k {
        "label" => match v.split_once('=') {
            Some((lk, lv)) => labels.get(lk).is_some_and(|x| x == lv),
            None => labels.contains_key(v),
        },
        "reference" => glob(v, name) || glob(v, name.strip_suffix(":latest").unwrap_or(name)),
        "source" => e.source.contains(v),
        _ => bail!(
            "--filter {f:?}: unknown filter {k:?} (label, reference, source); see `fluxctl explain MAC_FILTER_INVALID`"
        ),
    })
}

/// The document `mac images --format` renders: the `--json` row plus `repository`, `tag`, `id` (short digest) and `size`.
pub fn image_format_doc(
    store: &Store,
    name: &str,
    e: &Entry,
    labels: &std::collections::BTreeMap<String, String>,
    last_used: Option<u64>,
) -> Value {
    let dir = store.root().join(&e.dir);
    let repo = crate::mac_imgstat::repo_of(name);
    let tag = name
        .strip_prefix(repo)
        .and_then(|t| t.strip_prefix(':'))
        .unwrap_or("");
    let alloc = crate::mac_image_ops::dir_usage(&dir).1;
    json!({
        "reference": name, "repository": repo, "tag": tag,
        "id": if e.digest.is_empty() { "-".to_owned() } else { e.digest.trim_start_matches("sha256:").chars().take(12).collect::<String>() },
        "digest": e.digest, "source": e.source, "labels": labels,
        "size": crate::mac_inspect::human_bytes(alloc),
        "size_bytes": dir_size(&dir), "allocated_bytes": alloc,
        "added_unix": e.added_unix, "last_used_unix": last_used,
    })
}

pub fn images(store: &Store, json: bool, extra: &ImagesExtra) -> Result<()> {
    let mut rows = Vec::new();
    for (n, e) in store.list()? {
        if !extra.all && crate::mac_cache::is_cache_ref(&n) {
            continue;
        }
        let labels = crate::mac_config::load(&store.root().join(&e.dir))
            .map(|c| c.labels)
            .unwrap_or_default();
        let mut keep = true;
        for f in &extra.filter {
            keep &= matches_filter(f, &n, &e, &labels)?;
        }
        if keep {
            rows.push((n, e, labels));
        }
    }
    if let Some(fmt) = &extra.format {
        // Parsed first, so a bad template is reported even when no image matches.
        crate::mac_tmpl::Template::parse(fmt.strip_prefix("table").unwrap_or(fmt))?;
        let last_used = crate::mac_imgstat::load(store);
        let docs: Vec<Value> = rows
            .iter()
            .map(|(n, e, l)| image_format_doc(store, n, e, l, last_used.get(n).copied()))
            .collect();
        print!(
            "{}",
            crate::mac_tmpl::render_rows(
                &docs,
                fmt,
                r"{{.Repository}}\t{{.Tag}}\t{{.ID}}\t{{.Size}}"
            )?
        );
    } else if json {
        let last_used = crate::mac_imgstat::load(store);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!(
                rows.iter()
                    .map(|(n, e, l)| json!({
                        "reference": n, "digest": e.digest, "source": e.source, "labels": l,
                        "size_bytes": dir_size(&store.root().join(&e.dir)),
                        "allocated_bytes": crate::mac_image_ops::dir_usage(&store.root().join(&e.dir)).1,
                        "added_unix": e.added_unix,
                        "last_used_unix": last_used.get(n),
                    }))
                    .collect::<Vec<_>>()
            ))?
        );
    } else {
        let plain: Vec<(String, Entry)> = rows.into_iter().map(|(n, e, _)| (n, e)).collect();
        if plain.is_empty() && !extra.filter.is_empty() {
            println!("no images match the filter");
        } else if extra.usage {
            print!(
                "{}",
                crate::mac_imgstat::render_usage(store, &plain, crate::mac_health::now_unix())
            );
        } else {
            print!("{}", crate::mac_cmd::render_images(store, &plain));
        }
    }
    Ok(())
}

pub fn label(
    store: &Store,
    reference: &str,
    set: &[String],
    remove: &[String],
) -> Result<std::collections::BTreeMap<String, String>> {
    let dir = store.resolve(reference)?;
    let mut cfg = crate::mac_config::load(&dir)?;
    for kv in set {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("{kv:?}: expected KEY=VALUE"))?;
        ensure!(valid_label_key(k), "{k:?} is not a valid label key");
        cfg.labels.insert(k.into(), v.into());
    }
    for k in remove {
        cfg.labels.remove(k);
    }
    if !set.is_empty() || !remove.is_empty() {
        crate::mac_config::save(&dir, &cfg)?;
    }
    Ok(cfg.labels)
}

/// Repositories containing `term` (all when none), in order, at most `limit`.
pub fn filter_repos<'a>(repos: &'a [String], term: Option<&str>, limit: usize) -> Vec<&'a String> {
    repos
        .iter()
        .filter(|r| term.is_none_or(|t| r.contains(t)))
        .take(limit)
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// contents manifest

fn file_sha256(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The contents manifest of a template directory: each file's name, size and sha256, sorted by name. A sparse disk is read at
/// its full size, so hashing a 64 GiB `disk.raw` takes minutes.
pub fn sbom(store: &Store, reference: &str) -> Result<Value> {
    let key = mac_store::normalize(reference)?;
    let dir = store.resolve(&key)?;
    let entry = store
        .list()?
        .into_iter()
        .find(|(n, _)| *n == key)
        .map(|(_, e)| e)
        .context("image vanished")?;
    let mut names: Vec<String> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let mut files = Vec::new();
    for n in names {
        let p = dir.join(&n);
        files.push(json!({
            "name": n,
            "size": std::fs::metadata(&p)?.len(),
            "sha256": file_sha256(&p)?,
        }));
    }
    let cfg = crate::mac_config::load(&dir)?;
    Ok(json!({
        "format": "fluxvm.mac.contents.v1",
        "reference": key,
        "manifest_digest": entry.digest,
        "source": entry.source,
        "generated_unix": crate::mac_image_ops::now_unix(),
        "files": files,
        "labels": cfg.labels,
        "history": cfg.history.iter().map(|h| &h.created_by).collect::<Vec<_>>(),
        "note": "files of the template bundle; the guest's own files are inside disk.raw and are not listed",
    }))
}

async fn sbom_cmd(
    store: &Store,
    reference: &str,
    output: Option<&Path>,
    attach: Option<&str>,
) -> Result<()> {
    let text = serde_json::to_string_pretty(&sbom(store, reference)?)?;
    match output {
        Some(p) => {
            std::fs::write(p, &text).with_context(|| format!("writing {}", p.display()))?;
            eprintln!("wrote {}", p.display());
        }
        None if attach.is_none() => println!("{text}"),
        None => {}
    }
    if let Some(target) = attach {
        ensure!(
            names_a_registry(target),
            "{target:?} is not a registry reference"
        );
        let tmp = TempFile::write("contents", text.as_bytes())?;
        let a = image::oci_manage::attach(
            target,
            tmp.path(),
            &image::oci_manage::AttachOptions {
                artifact_type: "application/vnd.zyvor.fluxvm.contents.v1+json".into(),
                layer_media_type: None,
                annotations: Default::default(),
            },
            creds(target),
        )
        .await?;
        eprintln!(
            "attached the contents manifest to {} ({})",
            a.subject, a.artifact_digest
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// cache

fn parse_age(s: &str) -> Result<u64> {
    let t = s.trim();
    let (n, m) = match t.chars().last() {
        Some('d') => (&t[..t.len() - 1], 86400),
        Some('h') => (&t[..t.len() - 1], 3600),
        Some('m') => (&t[..t.len() - 1], 60),
        Some('s') => (&t[..t.len() - 1], 1),
        _ => (t, 1),
    };
    Ok(n.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{s:?} is not an age (7d, 12h, 90m)"))?
        * m)
}

fn cache(store: &Store, c: CacheCommand) -> Result<()> {
    let items = crate::mac_cache::list(store)?;
    match c {
        CacheCommand::Ls { json } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!(items
                        .iter()
                        .map(|i| json!({"name": i.name, "size_bytes": i.bytes, "added_unix": i.added_unix, "step": i.step}))
                        .collect::<Vec<_>>()))?
                );
            } else {
                print!(
                    "{}",
                    crate::mac_cache::render(&items, crate::mac_image_ops::now_unix())
                );
            }
        }
        CacheCommand::Prune {
            older_than,
            all,
            dry_run,
        } => {
            let age = older_than.as_deref().map(parse_age).transpose()?;
            let victims = crate::mac_cache::plan_prune(
                &items,
                crate::mac_cache::budget_bytes()?,
                age,
                crate::mac_image_ops::now_unix(),
                &Default::default(),
                all,
            );
            if dry_run {
                for v in &victims {
                    println!("would remove {v}");
                }
                println!("{} entries", victims.len());
            } else {
                let freed = crate::mac_cache::remove(store, &victims)?;
                println!(
                    "removed {} entries ({} allocated)",
                    victims.len(),
                    crate::mac_cmd::human(freed)
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_store::REQUIRED_FILES;

    fn entry(dir: &str, at: u64) -> Entry {
        Entry {
            dir: dir.into(),
            digest: String::new(),
            source: "local".into(),
            added_unix: at,
        }
    }

    fn template(store: &Store, dir: &str, body: &str) {
        let p = store.root().join(dir);
        std::fs::create_dir_all(&p).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(p.join(f), body).unwrap();
        }
    }

    #[test]
    fn glob_matches_stars_only() {
        assert!(glob("macos-*", "macos-xcode:15"));
        assert!(glob("*:15", "macos-xcode:15"));
        assert!(glob("a*c*e", "abcde"));
        assert!(!glob("a*c", "abd"));
        assert!(glob("exact", "exact") && !glob("exact", "exactly"));
    }

    #[test]
    fn filters_match_labels_references_and_sources() {
        let e = entry("d", 0);
        let l = std::collections::BTreeMap::from([("team".to_string(), "ios".to_string())]);
        let m = |f: &str, n: &str| matches_filter(f, n, &e, &l).unwrap();
        assert!(m("label=team", "x:1") && m("label=team=ios", "x:1"));
        assert!(!m("label=team=android", "x:1") && !m("label=other", "x:1"));
        assert!(m("reference=x:*", "x:1") && !m("reference=y*", "x:1"));
        assert!(m("reference=x", "x:latest"), "a bare name matches :latest");
        assert!(m("source=local", "x:1") && !m("source=ghcr", "x:1"));
        assert!(matches_filter("bogus=1", "x", &e, &l).is_err());
        assert!(matches_filter("nokey", "x", &e, &l).is_err());
    }

    #[test]
    fn labels_are_set_removed_and_validated() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("s")).unwrap();
        template(&store, "t1", "x");
        store.add("img:1", entry("t1", 0)).unwrap();
        let l = label(
            &store,
            "img:1",
            &["team=ios".into(), "tier=gold".into()],
            &[],
        )
        .unwrap();
        assert_eq!(l.len(), 2);
        let l = label(&store, "img:1", &[], &["tier".into()]).unwrap();
        assert_eq!(l.keys().collect::<Vec<_>>(), ["team"]);
        assert!(label(&store, "img:1", &["bad key=1".into()], &[]).is_err());
        assert!(label(&store, "img:1", &["novalue".into()], &[]).is_err());
        assert!(label(&store, "nope:1", &[], &[]).is_err());
        // A filter sees the stored label.
        let cfg = crate::mac_config::load(&store.resolve("img:1").unwrap()).unwrap();
        assert_eq!(cfg.labels["team"], "ios");
    }

    #[test]
    fn push_annotations_carry_config_and_flag_labels() {
        let cfg = std::collections::BTreeMap::from([("a".to_string(), "1".to_string())]);
        let got = label_annotations(&cfg, &["b=2".into(), "a=9".into()]).unwrap();
        assert_eq!(got["dev.zyvor.fluxvm.label.a"], "9", "the flag wins");
        assert_eq!(got["dev.zyvor.fluxvm.label.b"], "2");
        assert!(label_annotations(&cfg, &["nope".into()]).is_err());
    }

    #[test]
    fn sbom_lists_files_with_sizes_and_sha256_and_skips_dot_files() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("s")).unwrap();
        template(&store, "t1", "abc");
        std::fs::write(store.root().join("t1/.cache-step"), "RUN x").unwrap();
        store.add("img:1", entry("t1", 0)).unwrap();
        let v = sbom(&store, "img:1").unwrap();
        let files = v["files"].as_array().unwrap();
        let names: Vec<_> = files.iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["auxiliary.bin", "disk.raw", "hardware.bin"]);
        // sha256("abc")
        assert_eq!(
            files[0]["sha256"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(files[0]["size"], 3);
        assert_eq!(v["reference"], "img:1");
        assert!(sbom(&store, "nope").is_err());
    }

    #[test]
    fn seeds_are_other_tags_of_the_same_repository_newest_first() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("s")).unwrap();
        for name in ["t1", "t2", "t3", "t4"] {
            template(&store, name, "x");
        }
        let rows = vec![
            ("reg.example/acme/mac:1".to_string(), entry("t1", 10)),
            ("reg.example/acme/mac:2".to_string(), entry("t2", 20)),
            ("reg.example/acme/other:1".to_string(), entry("t3", 30)),
            ("reg.example/acme/mac:3".to_string(), entry("t4", 5)),
        ];
        let got = seed_dirs(store.root(), &rows, "reg.example/acme/mac:9", 2);
        assert_eq!(got, vec![store.root().join("t2"), store.root().join("t1")]);
        // The target itself is not its own seed; a different registry host is a different repository.
        let got = seed_dirs(store.root(), &rows, "reg.example/acme/mac:2", 5);
        assert!(!got.contains(&store.root().join("t2")) && got.len() == 2);
        assert!(seed_dirs(store.root(), &rows, "other.example/acme/mac:1", 2).is_empty());
    }

    #[test]
    fn repo_search_filters_by_substring_and_limit() {
        let repos: Vec<String> = ["mac/xcode", "mac/base", "linux/base"]
            .map(String::from)
            .to_vec();
        assert_eq!(filter_repos(&repos, Some("base"), 10).len(), 2);
        assert_eq!(filter_repos(&repos, None, 2).len(), 2);
        assert!(filter_repos(&repos, Some("zzz"), 10).is_empty());
    }

    #[test]
    fn ages_parse() {
        assert_eq!(parse_age("7d").unwrap(), 7 * 86400);
        assert_eq!(parse_age("90m").unwrap(), 5400);
        assert!(parse_age("soon").is_err());
    }

    #[test]
    fn local_verify_uses_kept_signatures_and_needs_a_digest() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("s")).unwrap();
        let t = TrustStore::open(d.path().join("trust")).unwrap();
        template(&store, "t1", "x");
        store.add("img:1", entry("t1", 0)).unwrap();
        let e = verify_local(&store, &t, "img:1").unwrap_err().to_string();
        assert!(e.contains("no manifest digest"), "{e}");

        let digest = format!("sha256:{}", "ab".repeat(32));
        store
            .add(
                "pulled:1",
                Entry {
                    dir: "t1".into(),
                    digest: digest.clone(),
                    source: "r/x:1".into(),
                    added_unix: 0,
                },
            )
            .unwrap();
        let e = verify_local(&store, &t, "pulled:1")
            .unwrap_err()
            .to_string();
        assert!(e.contains("no trusted keys"), "{e}");
        t.generate().unwrap();
        let e = verify_local(&store, &t, "pulled:1")
            .unwrap_err()
            .to_string();
        assert!(e.contains("no signature"), "{e}");
        t.save_signatures(&digest, &[t.sign_digest(&digest).unwrap()])
            .unwrap();
        assert_eq!(
            verify_local(&store, &t, "pulled:1").unwrap(),
            (digest.clone(), "self".to_string())
        );
        // A signature kept for another digest does not carry over.
        let other = format!("sha256:{}", "cd".repeat(32));
        t.save_signatures(&digest, &[t.sign_digest(&other).unwrap()])
            .unwrap();
        assert!(
            verify_local(&store, &t, "pulled:1")
                .unwrap_err()
                .to_string()
                .contains("does not verify")
        );
    }
}
