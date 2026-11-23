// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl image sbom|sign|verify|scan|policy`: supply-chain checks for OCI container images.
//!
//! The logic is in `fluxvm_image::{sbom, advisory, supply}` (also used by the daemon and Studio); signatures reuse the
//! Ed25519 scheme, trust store and OCI-referrer layout of `fluxctl mac` (`mac_trust`), so a key trusted with
//! `mac keys trust` is trusted here. Everything but fetching layers, asking a registry and `scan --update` works offline.
//! See docs/image-supply-chain.md.

use crate::mac_trust::{self, TrustStore, Verdict};
use crate::output::OutputFormat;
use crate::supply_errors::coded;
use anyhow::{Context, Result};
use clap::Subcommand;
use fluxvm_core::config::Config;
use fluxvm_image::advisory::{self, ScanReport, Severity};
use fluxvm_image::oci_registry::{Arch, BlobStore, Credentials};
use fluxvm_image::sbom::{self, Format, Subject};
use fluxvm_image::supply::{self, Assessment, VerifyRecord};
use fluxvm_scheduler::oci_images;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum SupplyCommand {
    /// Software bill of materials of an image (SPDX or CycloneDX JSON), read from the layers' package databases
    /// (dpkg, apk, python, node). Needs no network once the image is cached.
    Sbom {
        image: String,
        /// `spdx` (default) or `cyclonedx`.
        #[arg(long, default_value = "spdx")]
        sbom_format: String,
        /// Write to FILE instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Never use the network: fail if the image is not cached.
        #[arg(long)]
        offline: bool,
        /// Pull again even if the image is known.
        #[arg(long)]
        refresh: bool,
        /// `linux/arm64` (default) or `linux/amd64`.
        #[arg(long)]
        platform: Option<String>,
    },
    /// Sign the image's manifest digest with your key and attach the signature to the registry (OCI referrer).
    Sign { image: String },
    /// Is this image signed by a key you trust, and do the layers on this machine match their digests? Prints a plain
    /// verdict and saves it, so the image policy can use it. Exits 1 unless TRUSTED.
    Verify {
        image: String,
        /// Use the signatures saved by the last successful check instead of asking the registry.
        #[arg(long)]
        offline: bool,
        /// Pull again even if the image is known.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        platform: Option<String>,
    },
    /// Match the image's SBOM against the locally cached advisory data. Offline by default; `--update` replaces the
    /// data from `--source`. Never says "clean" without data: it says how old the data is, or that there is none.
    Scan {
        /// The image. Omit it with `--update` to only refresh the advisory data.
        image: Option<String>,
        /// Replace the cached advisory data from `--source` (or `FLUXVM_ADVISORY_SOURCE`) first.
        #[arg(long)]
        update: bool,
        /// A file or https URL of OSV records or FluxVM advisory JSON.
        #[arg(long)]
        source: Option<String>,
        /// Exit 1 if a finding is at this severity or worse (`critical`, `high`, `medium`, `low`); exit 2 if there is no
        /// advisory data to judge by.
        #[arg(long)]
        fail_on: Option<String>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        platform: Option<String>,
    },
    /// The allow/deny file the daemon applies when a container sandbox starts (block unsigned, block critical CVEs).
    Policy {
        #[command(subcommand)]
        command: Option<PolicyCommand>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PolicyCommand {
    /// Show the policy as understood, in plain words (the default).
    Show,
    /// Write a starter policy: block unsigned images and critical findings. Never overwrites.
    Init,
    /// What would happen if IMAGE were started now, and why. Exits 1 if it would be refused.
    Check { image: String },
    /// Runs that went ahead against the policy (`--policy-override`), oldest first.
    Overrides,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn creds() -> Option<Credentials> {
    let username = std::env::var("FLUXVM_REGISTRY_USER").ok()?;
    let password = std::env::var("FLUXVM_REGISTRY_PASSWORD").unwrap_or_default();
    Some(Credentials { username, password })
}

fn short(d: &str) -> &str {
    let h = d.strip_prefix("sha256:").unwrap_or(d);
    &h[..h.len().min(12)]
}

// ---- finding the image ----

fn subjects_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("oci").join("subjects")
}

fn subject_file(state_dir: &Path, s: &Subject) -> PathBuf {
    subjects_dir(state_dir).join(format!(
        "{}.json",
        s.manifest_digest.trim_start_matches("sha256:")
    ))
}

/// Remember a pulled image, so later commands find it without the registry.
fn remember(state_dir: &Path, s: &Subject) {
    let p = subject_file(state_dir, s);
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    if let Ok(b) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(p, b);
    }
}

/// An image already known here: a cached `oci` rootfs, or one a previous command pulled. Matches the reference, the
/// digest, or a 12+ character digest prefix.
pub fn known(cfg: &Config, image: &str) -> Option<(Subject, Option<String>)> {
    if let Ok(e) = oci_images::find(cfg, image) {
        let subject = Subject {
            reference: e
                .references
                .first()
                .cloned()
                .unwrap_or_else(|| e.manifest_digest.clone()),
            manifest_digest: e.manifest_digest.clone(),
            architecture: e.architecture.clone(),
            layers: e.layers.clone(),
        };
        return Some((subject, Some(e.config_digest)));
    }
    let prefix = image.strip_prefix("sha256:").unwrap_or(image);
    let mut found: Vec<Subject> = Vec::new();
    for f in std::fs::read_dir(subjects_dir(&cfg.state_dir))
        .ok()?
        .flatten()
    {
        let Ok(s) = std::fs::read(f.path())
            .map_err(drop)
            .and_then(|b| serde_json::from_slice::<Subject>(&b).map_err(drop))
        else {
            continue;
        };
        let by_digest =
            prefix.len() >= 12 && s.manifest_digest["sha256:".len()..].starts_with(prefix);
        if s.reference == image || s.manifest_digest == image || by_digest {
            found.push(s);
        }
    }
    // The newest pull of a moved tag wins.
    found.sort_by_key(|s| {
        std::fs::metadata(subject_file(&cfg.state_dir, s))
            .and_then(|m| m.modified())
            .ok()
    });
    found.pop().map(|s| (s, None))
}

async fn resolve(
    cfg: &Config,
    image: &str,
    platform: Option<&str>,
    offline: bool,
    refresh: bool,
) -> Result<(Subject, Option<String>)> {
    if !refresh && let Some(k) = known(cfg, image) {
        return Ok(k);
    }
    if offline {
        return Err(coded(
            format!("{image} is not in the local image cache, and --offline was given"),
            "IMAGE_NOT_CACHED",
        ));
    }
    eprintln!("pulling {image} (the layers are fetched once; this needs the network)");
    let arch = platform.map(Arch::parse).transpose()?.unwrap_or_default();
    let p = oci_images::pull(cfg, image, arch)
        .await
        .with_context(|| format!("pulling {image}"))?;
    let s = Subject::from(&p);
    remember(&cfg.state_dir, &s);
    Ok((s, Some(p.config_digest)))
}

fn map_layers_error(e: anyhow::Error) -> anyhow::Error {
    if format!("{e:#}").contains("pull the image first") {
        coded(format!("{e:#}"), "IMAGE_LAYERS_MISSING")
    } else {
        e
    }
}

// ---- rendering (pure, unit-tested) ----

pub fn render_scan(r: &ScanReport) -> String {
    let mut o = format!(
        "{}  {}  ({} packages)\n",
        r.reference,
        short(&r.manifest_digest),
        r.packages
    );
    o.push_str(&format!("{}\n", capitalize(&r.advisory.describe())));
    if !r.advisory.present {
        o.push_str("This is not a clean result: nothing was checked.\n");
        o.push_str("Load advisory data: fluxctl image scan --update --source FILE_OR_URL\n  more: fluxctl explain IMAGE_ADVISORY_SOURCE_MISSING\n");
    } else if r.findings.is_empty() {
        o.push_str("No known problems in the advisory data for these packages.\n");
    } else {
        let counts: Vec<String> = [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Unknown,
        ]
        .iter()
        .filter_map(|s| {
            r.counts
                .get(s.as_str())
                .map(|n| format!("{n} {}", s.as_str()))
        })
        .collect();
        o.push_str(&format!(
            "{} finding(s): {}\n\n",
            r.findings.len(),
            counts.join(", ")
        ));
        for f in &r.findings {
            o.push_str(&format!(
                "  {:<8}  {:<18}  {} {}  {}\n",
                f.severity.as_str().to_ascii_uppercase(),
                f.id,
                f.package,
                f.version,
                f.fixed
                    .as_deref()
                    .map_or("no fix known".to_string(), |v| format!("fixed in {v}")),
            ));
        }
    }
    for n in &r.notes {
        o.push_str(&format!("note: {n}\n"));
    }
    o
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or(String::new(), |f| {
        f.to_uppercase().collect::<String>() + c.as_str()
    })
}

pub fn render_assessment(a: &Assessment) -> String {
    let mut o = format!("{}  {}\n", a.reference, short(&a.manifest_digest));
    o.push_str(&format!("  {}\n", a.summary));
    if let Some(d) = &a.decision {
        if d.allowed {
            o.push_str("  Policy: it would start.\n");
        } else {
            o.push_str("  Policy: it would be REFUSED:\n");
            for v in &d.violations {
                o.push_str(&format!(
                    "    - {}\n      more: fluxctl explain {}\n",
                    v.message, v.code
                ));
            }
            o.push_str("    to start it anyway (logged loudly): --policy-override\n");
        }
        for w in &d.warnings {
            o.push_str(&format!("  Warning: {w}\n"));
        }
    } else {
        o.push_str("  Policy: none (no policy.toml), so nothing is blocked.\n");
    }
    if let Some(e) = &a.error {
        o.push_str(&format!("  Problem: {e}\n"));
    }
    o
}

pub fn render_policy(p: &supply::Policy, path: &Path) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    format!(
        "Image policy ({}):\n  unsigned images: {}\n  vulnerable images: {}\n  always allowed: {}\n  never allowed: {}\n  advisory data older than {} days is flagged\n",
        path.display(),
        if p.block_unsigned {
            "refused (until `image verify` finds a trusted signature)"
        } else {
            "allowed"
        },
        match p.block_severity {
            Some(s) => format!("refused at {} or worse", s.as_str()),
            None => "allowed".into(),
        },
        list(&p.allow),
        list(&p.deny),
        p.max_advisory_age_days,
    )
}

fn verdict_words(v: &str) -> (&'static str, &'static str) {
    match v {
        "trusted" => ("TRUSTED", ""),
        "unsigned" => ("UNSIGNED: nothing vouches for this image", "IMAGE_UNSIGNED"),
        "untrusted-signer" => (
            "SIGNED, BUT NOT BY A KEY YOU TRUST",
            "IMAGE_SIGNER_UNTRUSTED",
        ),
        _ => ("INVALID: do not run this image", "IMAGE_SIGNATURE_INVALID"),
    }
}

pub fn render_verify(r: &VerifyReport) -> String {
    let (words, code) = verdict_words(&r.verdict);
    let mut o = format!(
        "{}  {}\n  Verdict: {words}\n",
        r.reference,
        short(&r.digest)
    );
    if let Some(s) = &r.signer {
        o.push_str(&format!("  Signed by: {s}\n"));
    }
    for n in &r.notes {
        o.push_str(&format!("  {n}\n"));
    }
    o.push_str(&format!("  Layers: {}\n", r.layers));
    if !code.is_empty() {
        o.push_str(&format!("  more: fluxctl explain {code}\n"));
    }
    o
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyReport {
    pub reference: String,
    pub digest: String,
    pub verdict: String,
    pub signer: Option<String>,
    pub layers: String,
    pub notes: Vec<String>,
    pub checked_unix: u64,
}

fn layers_sentence(total: usize, missing: usize, corrupt: usize) -> String {
    if corrupt > 0 {
        format!("{corrupt} of {total} do NOT match their digests")
    } else if missing > 0 {
        format!("{missing} of {total} are not on this machine, so those were not checked")
    } else {
        format!("all {total} re-hashed and match their digests")
    }
}

// ---- commands ----

pub async fn run(
    command: SupplyCommand,
    format: OutputFormat,
    state_dir: Option<PathBuf>,
) -> Result<()> {
    let mut cfg = Config::default();
    if let Some(d) = state_dir {
        cfg.state_dir = d;
    }
    let json = matches!(format, OutputFormat::Json);
    match command {
        SupplyCommand::Sbom {
            image,
            sbom_format,
            out,
            offline,
            refresh,
            platform,
        } => {
            let fmt = Format::parse(&sbom_format)?;
            let (subject, _) = resolve(&cfg, &image, platform.as_deref(), offline, refresh).await?;
            let s =
                sbom::load_or_collect(&cfg.state_dir, &subject, now()).map_err(map_layers_error)?;
            let doc =
                serde_json::to_string_pretty(&sbom::render(&s, fmt, env!("CARGO_PKG_VERSION")))?;
            match out {
                Some(p) => {
                    std::fs::write(&p, doc + "\n")
                        .with_context(|| format!("writing {}", p.display()))?;
                    eprintln!("wrote {} ({} packages)", p.display(), s.packages.len());
                }
                None => println!("{doc}"),
            }
            for n in &s.notes {
                eprintln!("note: {n}");
            }
            Ok(())
        }
        SupplyCommand::Sign { image } => sign(&image).await,
        SupplyCommand::Verify {
            image,
            offline,
            refresh,
            platform,
        } => {
            let rep = verify(&cfg, &image, offline, refresh, platform.as_deref()).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                print!("{}", render_verify(&rep));
            }
            if rep.verdict != "trusted" {
                std::process::exit(1);
            }
            Ok(())
        }
        SupplyCommand::Scan {
            image,
            update,
            source,
            fail_on,
            offline,
            refresh,
            platform,
        } => {
            if update {
                update_advisories(&cfg.state_dir, source.as_deref()).await?;
            }
            let Some(image) = image else {
                if !update {
                    anyhow::bail!(
                        "give an image to scan, or --update to refresh the advisory data"
                    );
                }
                return Ok(());
            };
            let threshold = fail_on
                .as_deref()
                .map(|s| {
                    Severity::parse(s)
                        .filter(|v| *v != Severity::Unknown)
                        .with_context(|| {
                            format!("--fail-on {s:?}: use critical, high, medium or low")
                        })
                })
                .transpose()?;
            let (subject, _) = resolve(&cfg, &image, platform.as_deref(), offline, refresh).await?;
            let s =
                sbom::load_or_collect(&cfg.state_dir, &subject, now()).map_err(map_layers_error)?;
            let data = advisory::load(&cfg.state_dir).map_err(|e| {
                crate::supply_errors::coded(format!("{e:#}"), "IMAGE_ADVISORY_UNUSABLE")
            })?;
            let report = advisory::scan(&s, data.as_ref(), now());
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_scan(&report));
            }
            if let Some(t) = threshold {
                if !report.advisory.present {
                    eprintln!(
                        "--fail-on: there is no advisory data, so this image could not be judged"
                    );
                    std::process::exit(2);
                }
                if report.worst().is_some_and(|w| w >= t) {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        SupplyCommand::Policy { command } => {
            policy(&cfg, command.unwrap_or(PolicyCommand::Show), json).await
        }
    }
}

async fn policy(cfg: &Config, command: PolicyCommand, json: bool) -> Result<()> {
    let sd = &cfg.state_dir;
    match command {
        PolicyCommand::Show => {
            let path = supply::policy_path(sd);
            match supply::load_policy(sd)
                .map_err(|e| coded(format!("{e:#}"), "IMAGE_POLICY_INVALID"))?
            {
                None if json => println!("{}", serde_json::json!({"present": false, "path": path})),
                None => println!(
                    "No image policy ({} does not exist), so nothing is blocked.\nCreate one: fluxctl image policy init",
                    path.display()
                ),
                Some(p) if json => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"present": true, "path": path, "policy": p})
                    )?
                ),
                Some(p) => print!("{}", render_policy(&p, &path)),
            }
            Ok(())
        }
        PolicyCommand::Init => {
            let p = supply::init_policy(sd)?;
            println!(
                "wrote {}\nIt blocks unsigned images and critical findings. Edit it, then `fluxctl image policy` to review.",
                p.display()
            );
            Ok(())
        }
        PolicyCommand::Check { image } => {
            let (subject, _) = resolve(cfg, &image, None, true, false).await.map_err(|_| {
                coded(
                    format!("{image} is not known here yet; pull it first (`fluxctl oci pull {image}`, or any `image verify|scan|sbom`) so the check can read it"),
                    "IMAGE_NOT_CACHED",
                )
            })?;
            let a = supply::assess(sd, &subject, now());
            if json {
                println!("{}", serde_json::to_string_pretty(&a)?);
            } else {
                print!("{}", render_assessment(&a));
            }
            if a.decision.as_ref().is_some_and(|d| !d.allowed) {
                std::process::exit(1);
            }
            Ok(())
        }
        PolicyCommand::Overrides => {
            let text = std::fs::read_to_string(sd.join("oci").join(supply::OVERRIDE_LOG))
                .unwrap_or_default();
            if text.trim().is_empty() {
                println!("No runs have gone ahead against the policy.");
            } else if json {
                print!("{text}");
            } else {
                for l in text.lines() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(l) {
                        println!(
                            "{}  {}  by {}  {}",
                            sbom::rfc3339(v["unix"].as_u64().unwrap_or(0)),
                            v["reference"].as_str().unwrap_or("?"),
                            v["actor"].as_str().unwrap_or("?"),
                            v["violations"].as_array().map_or(String::new(), |a| a
                                .iter()
                                .filter_map(|x| x["code"].as_str())
                                .collect::<Vec<_>>()
                                .join(", "))
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

async fn update_advisories(state_dir: &Path, source: Option<&str>) -> Result<()> {
    let source = source
        .map(str::to_owned)
        .or_else(|| std::env::var("FLUXVM_ADVISORY_SOURCE").ok().filter(|s| !s.is_empty()))
        .ok_or_else(|| {
            coded(
                "no advisory source: pass --source FILE_OR_URL or set FLUXVM_ADVISORY_SOURCE (FluxVM does not pick a vulnerability feed for you)",
                "IMAGE_ADVISORY_SOURCE_MISSING",
            )
        })?;
    let (bytes, dated) = if source.starts_with("http://") || source.starts_with("https://") {
        (advisory::fetch(&source).await?, now())
    } else {
        let p = Path::new(&source);
        let mtime = std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(now, |d| d.as_secs());
        (
            std::fs::read(p).with_context(|| format!("reading {source}"))?,
            mtime,
        )
    };
    let mut file = advisory::parse(&bytes, dated)
        .map_err(|e| coded(format!("{source}: {e:#}"), "IMAGE_ADVISORY_UNUSABLE"))?;
    if file.advisories.is_empty() {
        return Err(coded(
            format!("{source} holds no advisories; the cached data was left as it was"),
            "IMAGE_ADVISORY_UNUSABLE",
        ));
    }
    file.source = source.clone();
    advisory::save(state_dir, &file)?;
    eprintln!(
        "advisory data replaced: {} advisories from {source}; {}",
        file.advisories.len(),
        advisory::status(Some(&file), now()).describe()
    );
    Ok(())
}

async fn sign(image: &str) -> Result<()> {
    let trust = open_trust()?;
    let rep = fluxvm_image::oci_manage::inspect(image, creds())
        .await
        .map_err(|e| {
            coded(
                format!("asking the registry about {image}: {e:#}"),
                "IMAGE_REGISTRY_UNREACHABLE",
            )
        })?;
    let rec = trust.sign_digest(&rep.digest)?;
    let r = fluxvm_image::oci_registry::ImageRef::parse(image)?;
    let pinned = fluxvm_image::oci_registry::ImageRef {
        reference: rep.digest.clone(),
        ..r
    };
    let tmp = std::env::temp_dir().join(format!("fluxvm-image-sig-{}.json", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&rec)?)?;
    let res = fluxvm_image::oci_manage::attach(
        &pinned.to_string(),
        &tmp,
        &fluxvm_image::oci_manage::AttachOptions {
            artifact_type: mac_trust::SIG_TYPE.into(),
            layer_media_type: None,
            annotations: mac_trust::record_annotations(&rec),
        },
        creds(),
    )
    .await
    .context("attaching the signature (does the registry accept OCI artifacts with a subject?)");
    let _ = std::fs::remove_file(&tmp);
    res?;
    trust.save_signatures(&rep.digest, &[rec])?;
    println!("signed {image} ({})", rep.digest);
    Ok(())
}

fn open_trust() -> Result<TrustStore> {
    let root = crate::mac_store::default_root()?;
    TrustStore::open(mac_trust::trust_dir(&root))
}

pub async fn verify(
    cfg: &Config,
    image: &str,
    offline: bool,
    refresh: bool,
    platform: Option<&str>,
) -> Result<VerifyReport> {
    let trust = open_trust()?;
    let trusted = trust.parsed()?;
    let (subject, config_digest) = resolve(cfg, image, platform, offline, refresh).await?;
    let mut notes = Vec::new();

    // 1. The signature.
    let (reg_digest, records) = if offline {
        notes.push("Offline: used the signatures saved by the last successful check.".to_string());
        (
            subject.manifest_digest.clone(),
            trust.load_signatures(&subject.manifest_digest)?,
        )
    } else {
        use mac_trust::SignatureSource;
        let src = mac_trust::RegistrySource { creds: creds() };
        let (d, recs) = src.signatures(image).await.map_err(|e| {
            coded(
                format!("could not ask the registry about {image}: {e:#}"),
                "IMAGE_REGISTRY_UNREACHABLE",
            )
        })?;
        trust.save_signatures(&d, &recs)?;
        (d, recs)
    };
    let verdict = mac_trust::evaluate(&reg_digest, &records, &trusted);
    let mut signer = None;
    let mut word = match &verdict {
        Verdict::Trusted { signer: s } => {
            signer = Some(s.clone());
            "trusted"
        }
        Verdict::Unsigned => "unsigned",
        Verdict::UntrustedSigner { fingerprints } => {
            notes.push(format!("Signing key(s): {}", fingerprints.join(", ")));
            "untrusted-signer"
        }
        Verdict::Invalid => "invalid",
    }
    .to_string();
    if trusted.is_empty() && word != "unsigned" {
        notes.push(
            "Your trust store is empty: add a key with `fluxctl mac keys trust NAME FILE`.".into(),
        );
    }

    // 2. The tag still means what we have cached.
    if !offline && reg_digest != subject.manifest_digest {
        let is_member = fluxvm_image::oci_manage::inspect(image, creds())
            .await
            .map(|r| {
                r.manifests
                    .iter()
                    .any(|m| m.digest == subject.manifest_digest)
            })
            .unwrap_or(false);
        if is_member {
            notes.push(
                "The signature covers the multi-platform index this image is part of.".into(),
            );
        } else {
            word = "invalid".into();
            signer = None;
            notes.push(format!(
                "The registry's {image} is now {} but the copy on this machine is {}: the tag moved. Pull again (`--refresh`).",
                short(&reg_digest),
                short(&subject.manifest_digest)
            ));
        }
    }

    // 3. The layers on this machine.
    let store = BlobStore::for_state_dir(&cfg.state_dir);
    let (missing, corrupt) = supply::check_blobs(&store, &subject, config_digest.as_deref());
    let total = subject.layers.len() + usize::from(config_digest.is_some());
    if !corrupt.is_empty() {
        word = "invalid".into();
        signer = None;
    }
    let layers = layers_sentence(total, missing.len(), corrupt.len());

    let rec = VerifyRecord {
        reference: subject.reference.clone(),
        digest: subject.manifest_digest.clone(),
        verdict: word.clone(),
        signer: signer.clone(),
        blobs_ok: missing.is_empty() && corrupt.is_empty(),
        checked_unix: now(),
    };
    supply::save_verify(&cfg.state_dir, &rec)?;
    Ok(VerifyReport {
        reference: subject.reference,
        digest: subject.manifest_digest,
        verdict: word,
        signer,
        layers,
        notes,
        checked_unix: rec.checked_unix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_image::advisory::{Advisory, AdvisoryFile};

    fn sbom_of(pkgs: &[(&str, &str)]) -> sbom::Sbom {
        sbom::Sbom {
            reference: "ghcr.io/acme/app:1".into(),
            manifest_digest: format!("sha256:{}", "ab".repeat(32)),
            architecture: "arm64".into(),
            distro: None,
            packages: pkgs
                .iter()
                .map(|(n, v)| sbom::Package {
                    name: (*n).into(),
                    version: (*v).into(),
                    ecosystem: "deb".into(),
                    arch: None,
                    purl: String::new(),
                })
                .collect(),
            notes: vec![],
            generated_unix: 0,
        }
    }

    #[test]
    fn a_scan_without_data_is_not_a_clean_result() {
        let r = advisory::scan(&sbom_of(&[("openssl", "1")]), None, 10);
        let t = render_scan(&r);
        assert!(t.contains("not a clean result"), "{t}");
        assert!(t.contains("nothing was checked"));
        assert!(t.contains("IMAGE_ADVISORY_SOURCE_MISSING"));
        assert!(!t.contains("No known problems"));
    }

    #[test]
    fn a_scan_with_data_names_the_age_and_the_findings() {
        let f = AdvisoryFile {
            format: advisory::FORMAT.into(),
            updated_unix: 1_000_000,
            source: "s".into(),
            advisories: vec![Advisory {
                id: "CVE-2024-1".into(),
                package: "openssl".into(),
                ecosystem: "deb".into(),
                severity: Severity::Critical,
                introduced: None,
                fixed: Some("2".into()),
                summary: "x".into(),
            }],
        };
        let r = advisory::scan(
            &sbom_of(&[("openssl", "1"), ("zlib", "1")]),
            Some(&f),
            1_000_000 + 9 * 86_400,
        );
        let t = render_scan(&r);
        assert!(t.contains("Advisory data is 9 days old"), "{t}");
        assert!(t.contains("1 finding(s): 1 critical"));
        assert!(t.contains("CRITICAL") && t.contains("CVE-2024-1") && t.contains("fixed in 2"));
        let clean = advisory::scan(&sbom_of(&[("zlib", "1")]), Some(&f), 1_000_000);
        assert!(render_scan(&clean).contains("No known problems in the advisory data"));
    }

    #[test]
    fn the_verify_card_gives_one_verdict_and_the_code() {
        let r = VerifyReport {
            reference: "a:1".into(),
            digest: format!("sha256:{}", "cd".repeat(32)),
            verdict: "untrusted-signer".into(),
            signer: None,
            layers: layers_sentence(4, 1, 0),
            notes: vec![],
            checked_unix: 0,
        };
        let t = render_verify(&r);
        assert!(
            t.contains("NOT BY A KEY YOU TRUST") && t.contains("IMAGE_SIGNER_UNTRUSTED"),
            "{t}"
        );
        assert!(t.contains("1 of 4 are not on this machine"));
        assert_eq!(
            layers_sentence(3, 0, 0),
            "all 3 re-hashed and match their digests"
        );
        assert!(layers_sentence(3, 1, 2).contains("do NOT match"));
    }

    #[test]
    fn the_policy_reads_in_plain_words() {
        let p = supply::Policy::parse(supply::STARTER).unwrap();
        let t = render_policy(&p, Path::new("/x/policy.toml"));
        assert!(
            t.contains("unsigned images: refused") && t.contains("refused at critical or worse"),
            "{t}"
        );
    }

    #[test]
    fn known_finds_a_remembered_image_by_reference_and_digest_prefix() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.state_dir = d.path().to_path_buf();
        let s = Subject {
            reference: "ghcr.io/acme/app:1".into(),
            manifest_digest: format!("sha256:{}", "ab".repeat(32)),
            architecture: "arm64".into(),
            layers: vec![],
        };
        assert!(known(&cfg, &s.reference).is_none());
        remember(d.path(), &s);
        assert_eq!(known(&cfg, "ghcr.io/acme/app:1").unwrap().0, s);
        assert_eq!(
            known(&cfg, &format!("sha256:{}", "ab".repeat(8)))
                .unwrap()
                .0,
            s
        );
        assert!(known(&cfg, "ghcr.io/acme/other:1").is_none());
    }

    #[tokio::test]
    async fn offline_with_an_unknown_image_is_a_coded_error() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.state_dir = d.path().to_path_buf();
        let e = resolve(&cfg, "nope:1", None, true, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("IMAGE_NOT_CACHED") && e.contains("--offline"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn updating_from_a_file_replaces_the_data_and_refuses_junk() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("osv.json");
        std::fs::write(
            &src,
            r#"[{"id":"GHSA-1","summary":"s","database_specific":{"severity":"HIGH"},"affected":[{"package":{"name":"left-pad","ecosystem":"npm"},"ranges":[{"type":"SEMVER","events":[{"introduced":"0"},{"fixed":"1.3.1"}]}]}]}]"#,
        )
        .unwrap();
        let state = d.path().join("state");
        update_advisories(&state, Some(src.to_str().unwrap()))
            .await
            .unwrap();
        let f = advisory::load(&state).unwrap().unwrap();
        assert_eq!(f.advisories.len(), 1);
        assert_eq!(f.source, src.to_str().unwrap());
        std::fs::write(&src, "not json").unwrap();
        let e = update_advisories(&state, Some(src.to_str().unwrap()))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("IMAGE_ADVISORY_UNUSABLE"), "{e}");
        assert_eq!(
            advisory::load(&state).unwrap().unwrap().advisories.len(),
            1,
            "bad update leaves the old data"
        );
        // No source at all.
        // SAFETY: tests in this crate do not read this variable concurrently.
        let none = update_advisories(&state, None).await;
        if std::env::var_os("FLUXVM_ADVISORY_SOURCE").is_none() {
            assert!(
                none.unwrap_err()
                    .to_string()
                    .contains("IMAGE_ADVISORY_SOURCE_MISSING")
            );
        }
    }
}
