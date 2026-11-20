// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl image inspect|tags|tag|rm|referrers|attach|index`: OCI registry management. Client-side only (the
//! registry HTTP API), so it never needs the daemon. Credentials: `FLUXVM_REGISTRY_USER` / `FLUXVM_REGISTRY_PASSWORD`.
//! JSON by default; `-o table` prints a readable summary. See docs/oci-registry-tools.md.

use crate::output::OutputFormat;
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fluxvm_image::oci_manage as m;
use fluxvm_image::oci_registry::Credentials;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum ImageCommand {
    /// Show a manifest or index: digest, config, layers, sizes, platforms, annotations.
    Inspect { reference: String },
    /// List a repository's tags (follows registry pagination).
    Tags {
        reference: String,
        /// Ask the registry for pages of this many tags (`?n=`).
        #[arg(long)]
        page_size: Option<usize>,
    },
    /// Point NEW_TAG at the manifest REFERENCE names (same repository).
    Tag { reference: String, new_tag: String },
    /// Delete a manifest by digest (`repo@sha256:...`). Removes every tag that points at it.
    Rm { reference: String },
    /// List artifacts (SBOMs, signatures, attestations) whose subject is REFERENCE. Uses the OCI 1.1 referrers API,
    /// falling back to the `sha256-<hex>` index tag.
    Referrers {
        reference: String,
        /// Only this artifact type, e.g. `application/spdx+json`.
        #[arg(long)]
        artifact_type: Option<String>,
    },
    /// Publish FILE as an artifact whose subject is REFERENCE (an SBOM, attestation, any file).
    Attach {
        reference: String,
        file: PathBuf,
        /// The artifact's type, e.g. `application/spdx+json`.
        #[arg(long)]
        artifact_type: String,
        /// Media type of the file layer (default: the artifact type).
        #[arg(long)]
        layer_media_type: Option<String>,
        /// `key=value` manifest annotation (repeatable).
        #[arg(long = "annotation", value_parser = parse_kv)]
        annotations: Vec<(String, String)>,
    },
    /// Multi-architecture indexes.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum IndexCommand {
    /// Build an OCI image index at REFERENCE (a tag) from MANIFESTS of the same repository; each platform is read
    /// from that manifest's image config.
    Create {
        reference: String,
        #[arg(required = true)]
        manifests: Vec<String>,
        /// `key=value` index annotation (repeatable).
        #[arg(long = "annotation", value_parser = parse_kv)]
        annotations: Vec<(String, String)>,
    },
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("{s:?}: use key=value"))
}

fn credentials() -> Option<Credentials> {
    let username = std::env::var("FLUXVM_REGISTRY_USER").ok()?;
    let password = std::env::var("FLUXVM_REGISTRY_PASSWORD").unwrap_or_default();
    Some(Credentials { username, password })
}

fn emit<T: Serialize>(v: &T, text: impl FnOnce() -> String, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(v)?),
        _ => print!("{}", text()),
    }
    Ok(())
}

pub async fn run(command: ImageCommand, format: OutputFormat) -> Result<()> {
    let creds = credentials();
    match command {
        ImageCommand::Inspect { reference } => {
            let r = m::inspect(&reference, creds)
                .await
                .with_context(|| format!("inspecting {reference}"))?;
            emit(&r, || m::render_inspect(&r), format)
        }
        ImageCommand::Tags {
            reference,
            page_size,
        } => {
            if page_size == Some(0) {
                bail!("--page-size must be at least 1");
            }
            let tags = m::list_tags(&reference, creds, page_size)
                .await
                .with_context(|| format!("listing tags of {reference}"))?;
            emit(
                &serde_json::json!({"tags": tags}),
                || tags.iter().map(|t| format!("{t}\n")).collect(),
                format,
            )
        }
        ImageCommand::Tag { reference, new_tag } => {
            let r = m::retag(&reference, &new_tag, creds)
                .await
                .with_context(|| format!("tagging {reference} as {new_tag}"))?;
            emit(
                &r,
                || format!("{}:{} -> {}\n", r.repository, r.tag, r.digest),
                format,
            )
        }
        ImageCommand::Rm { reference } => {
            let d = m::delete_manifest(&reference, creds).await?;
            emit(
                &serde_json::json!({"deleted": d}),
                || format!("deleted {d}\n"),
                format,
            )
        }
        ImageCommand::Referrers {
            reference,
            artifact_type,
        } => {
            let r = m::referrers(&reference, artifact_type.as_deref(), creds)
                .await
                .with_context(|| format!("listing referrers of {reference}"))?;
            emit(
                &r,
                || {
                    let mut o = format!("Subject {} (via {})\n", r.subject, r.via);
                    for x in &r.referrers {
                        o.push_str(&format!(
                            "  {}  {:>10}  {}\n",
                            x.digest,
                            m::human_size(x.size),
                            x.artifact_type.as_deref().unwrap_or("-")
                        ));
                    }
                    if r.referrers.is_empty() {
                        o.push_str("  no referrers\n");
                    }
                    o
                },
                format,
            )
        }
        ImageCommand::Attach {
            reference,
            file,
            artifact_type,
            layer_media_type,
            annotations,
        } => {
            let opts = m::AttachOptions {
                artifact_type,
                layer_media_type,
                annotations: annotations.into_iter().collect::<BTreeMap<_, _>>(),
            };
            let r = m::attach(&reference, &file, &opts, creds)
                .await
                .with_context(|| format!("attaching {} to {reference}", file.display()))?;
            emit(
                &r,
                || {
                    format!(
                        "attached {} ({}) to {}\n",
                        r.artifact_digest, r.artifact_type, r.subject
                    )
                },
                format,
            )
        }
        ImageCommand::Index {
            command:
                IndexCommand::Create {
                    reference,
                    manifests,
                    annotations,
                },
        } => {
            let r = m::create_index(
                &reference,
                &manifests,
                &annotations.into_iter().collect(),
                creds,
            )
            .await
            .with_context(|| format!("creating index {reference}"))?;
            emit(
                &r,
                || {
                    let mut o = format!("{} {}\n", r.reference, r.digest);
                    for e in &r.manifests {
                        o.push_str(&format!(
                            "  {}  {}\n",
                            e.digest,
                            e.platform.as_ref().map_or_else(String::new, |p| p.label())
                        ));
                    }
                    o
                },
                format,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_value_parsing() {
        assert_eq!(parse_kv("a=b=c").unwrap(), ("a".into(), "b=c".into()));
        assert!(parse_kv("novalue").is_err());
        assert!(parse_kv("=v").is_err());
    }
}
