// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! VM images as OCI artifacts: push a VM bundle (a raw disk, or a prepared macOS template directory) to any OCI
//! registry and pull it back onto another host, so a fleet of Macs shares one golden image instead of copying 50 GB
//! disks around by hand.
//!
//! # Format
//!
//! An OCI image manifest with `artifactType` [`ARTIFACT_TYPE`]:
//!
//! - **config** ([`MT_CONFIG`]): a [`VmArtifactConfig`]: guest OS, architecture, and every file's name and logical
//!   size.
//! - **layers**: each file is cut into fixed-size chunks (64 MiB by default), each chunk zstd-compressed into one
//!   layer ([`MT_CHUNK`]) annotated with its file, offset, raw size and raw sha256. **All-zero chunks are not
//!   uploaded**: a file is recreated sparse at its logical size and only non-zero chunks are written, so a 100 GB macOS
//!   disk with 20 GB used moves about 20 GB of compressed data.
//!
//! Identical chunks hash to identical layers, so pushing a new version of an image uploads only the chunks that
//! changed (each layer is checked with `HEAD` first), and registries deduplicate across repositories.
//!
//! # What is (and is not) in a bundle
//!
//! A directory push takes every regular file at its top level except per-VM identity and runtime state:
//! `identity.bin` (a macOS guest's machine identifier: every clone must get its own, see
//! `fluxvm_apple::runner::MACOS_TEMPLATE_FILES`), `generic-id.bin`, `vz-mac`, saved state (`*.vzvmsave`, host-bound),
//! logs, sockets, pid files and dot-files. A macOS template is `disk.raw` + `hardware.bin` + `auxiliary.bin`; pulled
//! into a directory, its `disk.raw` is directly usable as `image` for a `vz` macOS clone.
//!
//! Integrity: the manifest digest pins the config and every layer descriptor (digest, size, annotations); each layer is
//! checked against its digest before decompression and against its raw sha256 after.

use crate::oci_registry::{
    Credentials, ImageRef, parse_bearer_challenge, read_capped, sha256_digest, validate_digest,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ARTIFACT_TYPE: &str = "application/vnd.zyvor.fluxvm.vm.v1";
pub const MT_CONFIG: &str = "application/vnd.zyvor.fluxvm.vm.config.v1+json";
pub const MT_CHUNK: &str = "application/vnd.zyvor.fluxvm.chunk.v1.zstd";
const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

pub const ANN_FILE: &str = "dev.zyvor.fluxvm.file";
pub const ANN_OFFSET: &str = "dev.zyvor.fluxvm.offset";
pub const ANN_RAW_SIZE: &str = "dev.zyvor.fluxvm.raw-size";
pub const ANN_RAW_DIGEST: &str = "dev.zyvor.fluxvm.raw-digest";

pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024 * 1024;
/// Largest chunk a pull accepts (raw and compressed), bounding memory per layer.
pub const MAX_CHUNK_SIZE: usize = 512 * 1024 * 1024;
const MIN_CHUNK_SIZE: usize = 1024 * 1024;
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
/// Zero runs at least this long are left as holes when writing a chunk.
const SPARSE_BLOCK: usize = 64 * 1024;
const ZSTD_LEVEL: i32 = 3;

/// Files never pushed from a directory: per-VM identity and runtime state.
const EXCLUDED_NAMES: &[&str] = &["identity.bin", "generic-id.bin", "vz-mac"];
const EXCLUDED_SUFFIXES: &[&str] = &[".vzvmsave", ".log", ".sock", ".pid", ".lock", ".tmp"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFile {
    pub name: String,
    /// Logical size in bytes; the file is recreated at exactly this size.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmArtifactConfig {
    pub format_version: u32,
    /// `linux` or `macos`.
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub chunk_size: u64,
    pub files: Vec<ArtifactFile>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Bytes per chunk; 0 means [`DEFAULT_CHUNK_SIZE`].
    pub chunk_size: usize,
    /// Extra file names to leave out of a directory push.
    pub exclude: Vec<String>,
    /// `linux` or `macos`; detected when unset (a directory with `hardware.bin` and `auxiliary.bin` is macOS).
    pub os: Option<String>,
    pub name: Option<String>,
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushReport {
    pub reference: String,
    pub manifest_digest: String,
    pub files: Vec<ArtifactFile>,
    pub layers: usize,
    pub layers_uploaded: usize,
    pub layers_already_present: usize,
    pub zero_chunks_skipped: usize,
    pub bytes_uploaded: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullReport {
    pub reference: String,
    pub manifest_digest: String,
    pub dir: PathBuf,
    pub config: VmArtifactConfig,
    pub bytes_downloaded: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Desc {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType", default)]
    media_type: String,
    #[serde(
        rename = "artifactType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    artifact_type: Option<String>,
    config: Desc,
    layers: Vec<Desc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

/// Which files of `src` a push takes, with their sizes. `src` is a file (a single disk) or a directory.
pub fn bundle_files(src: &Path, exclude: &[String]) -> Result<Vec<(PathBuf, ArtifactFile)>> {
    let meta = fs::metadata(src).with_context(|| format!("reading {}", src.display()))?;
    if meta.is_file() {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .context("source file has no usable name")?
            .to_owned();
        return Ok(vec![(
            src.to_path_buf(),
            ArtifactFile {
                name,
                size: meta.len(),
            },
        )]);
    }
    ensure!(
        meta.is_dir(),
        "{} is neither a file nor a directory",
        src.display()
    );
    let mut out = Vec::new();
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if !ft.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.')
            || EXCLUDED_NAMES.contains(&name.as_str())
            || EXCLUDED_SUFFIXES.iter().any(|s| name.ends_with(s))
            || exclude.iter().any(|e| e == &name)
        {
            continue;
        }
        let size = entry.metadata()?.len();
        out.push((entry.path(), ArtifactFile { name, size }));
    }
    out.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    ensure!(!out.is_empty(), "nothing to push in {}", src.display());
    Ok(out)
}

fn detect_os(files: &[(PathBuf, ArtifactFile)]) -> &'static str {
    let has = |n: &str| files.iter().any(|(_, f)| f.name == n);
    if has("hardware.bin") && has("auxiliary.bin") {
        "macos"
    } else {
        "linux"
    }
}

/// A file name a pull may create: one plain path component.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn read_full(f: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Writes `data` at `offset`, leaving all-zero blocks of [`SPARSE_BLOCK`] as holes (the file was pre-sized, so holes
/// read back as zeros).
fn write_sparse(f: &fs::File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    let mut pos = 0usize;
    while pos < data.len() {
        let end = (pos + SPARSE_BLOCK).min(data.len());
        let block = &data[pos..end];
        if block.iter().any(|&b| b != 0) {
            f.write_all_at(block, offset + pos as u64)?;
        }
        pos = end;
    }
    Ok(())
}

/// A registry session for one repository: bearer-token or basic auth, retried once on 401.
struct Session {
    http: reqwest::Client,
    r: ImageRef,
    creds: Option<Credentials>,
    auth: Option<String>,
}

impl Session {
    fn new(r: ImageRef, creds: Option<Credentials>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("fluxvm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("building the registry HTTP client")?;
        Ok(Self {
            http,
            r,
            creds,
            auth: None,
        })
    }

    /// `scheme://host[:port]`, for relative upload `Location`s.
    fn origin(&self) -> String {
        let base = self.r.base_url();
        let cut = base.find("/v2/").unwrap_or(base.len());
        base[..cut].to_owned()
    }

    async fn send(
        &mut self,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        for attempt in 0..2 {
            let mut req = build(&self.http);
            if let Some(a) = &self.auth {
                req = req.header("Authorization", a);
            }
            let resp = req.send().await.context("registry request")?;
            if resp.status() != reqwest::StatusCode::UNAUTHORIZED || attempt == 1 {
                return Ok(resp);
            }
            let challenge = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            self.auth = Some(self.authorize(&challenge).await?);
        }
        unreachable!("the loop returns on its second attempt")
    }

    async fn authorize(&self, challenge: &str) -> Result<String> {
        let lower = challenge.trim_start().to_ascii_lowercase();
        if lower.starts_with("basic") {
            let c = self.creds.as_ref().context(
                "registry wants a username and password (set FLUXVM_REGISTRY_USER / _PASSWORD)",
            )?;
            use base64::Engine as _;
            let raw = format!("{}:{}", c.username, c.password);
            return Ok(format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(raw)
            ));
        }
        let mut params = parse_bearer_challenge(challenge)
            .with_context(|| format!("registry asked for unsupported auth: {challenge:?}"))?;
        let realm = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.clone())
            .context("auth challenge has no realm")?;
        params.retain(|(k, _)| k != "realm");
        // Ask for push and pull on this repository whatever the challenge scoped, so one token serves the session.
        params.retain(|(k, _)| k != "scope");
        params.push((
            "scope".into(),
            format!("repository:{}:pull,push", self.r.repository),
        ));
        let mut req = self.http.get(&realm).query(&params);
        if let Some(c) = &self.creds {
            req = req.basic_auth(&c.username, Some(&c.password));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("token request to {realm}"))?;
        ensure!(
            resp.status().is_success(),
            "token request to {realm}: HTTP {}",
            resp.status()
        );
        #[derive(Deserialize)]
        struct Tok {
            token: Option<String>,
            access_token: Option<String>,
        }
        let bytes = read_capped(resp, MAX_JSON_BYTES).await?;
        let t: Tok = serde_json::from_slice(&bytes).context("parsing token response")?;
        let token = t
            .token
            .or(t.access_token)
            .context("token response has no token")?;
        Ok(format!("Bearer {token}"))
    }

    async fn blob_exists(&mut self, digest: &str) -> Result<bool> {
        let url = format!("{}/blobs/{digest}", self.r.base_url());
        let resp = self.send(|h| h.head(&url)).await?;
        match resp.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            s => bail!("HEAD {url}: HTTP {s}"),
        }
    }

    /// Monolithic upload: `POST …/blobs/uploads/` then `PUT <location>?digest=`.
    async fn upload(&mut self, digest: &str, bytes: Vec<u8>) -> Result<()> {
        let start = format!("{}/blobs/uploads/", self.r.base_url());
        let resp = self.send(|h| h.post(&start)).await?;
        ensure!(
            resp.status().as_u16() == 202,
            "POST {start}: HTTP {}",
            resp.status()
        );
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .context("upload start returned no Location")?
            .to_owned();
        let loc = if loc.starts_with("http://") || loc.starts_with("https://") {
            loc
        } else {
            format!("{}{loc}", self.origin())
        };
        let sep = if loc.contains('?') { '&' } else { '?' };
        let url = format!("{loc}{sep}digest={digest}");
        let len = bytes.len();
        let resp = self
            .send(|h| {
                h.put(&url)
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", len)
                    .body(bytes.clone())
            })
            .await?;
        ensure!(
            resp.status().as_u16() == 201,
            "PUT blob {digest}: HTTP {}",
            resp.status()
        );
        Ok(())
    }

    async fn put_manifest(&mut self, reference: &str, bytes: Vec<u8>) -> Result<()> {
        let url = format!("{}/manifests/{reference}", self.r.base_url());
        let resp = self
            .send(|h| {
                h.put(&url)
                    .header("Content-Type", MT_OCI_MANIFEST)
                    .body(bytes.clone())
            })
            .await?;
        ensure!(
            resp.status().is_success(),
            "PUT manifest {reference}: HTTP {}",
            resp.status()
        );
        Ok(())
    }

    async fn get_bytes(&mut self, url: &str, accept: &str, cap: u64) -> Result<Vec<u8>> {
        let resp = self.send(|h| h.get(url).header("Accept", accept)).await?;
        ensure!(
            resp.status().is_success(),
            "GET {url}: HTTP {}",
            resp.status()
        );
        read_capped(resp, cap).await
    }

    async fn blob(&mut self, d: &Desc, cap: u64) -> Result<Vec<u8>> {
        validate_digest(&d.digest)?;
        ensure!(
            d.size <= cap,
            "blob {} is {} bytes (limit {cap})",
            d.digest,
            d.size
        );
        let url = format!("{}/blobs/{}", self.r.base_url(), d.digest);
        let bytes = self.get_bytes(&url, "*/*", d.size).await?;
        let got = sha256_digest(&bytes);
        ensure!(
            got == d.digest,
            "blob digest mismatch: expected {}, got {got}",
            d.digest
        );
        Ok(bytes)
    }
}

async fn push_blob(s: &mut Session, report: &mut PushReport, bytes: Vec<u8>) -> Result<String> {
    let digest = sha256_digest(&bytes);
    if s.blob_exists(&digest).await? {
        report.layers_already_present += 1;
    } else {
        report.bytes_uploaded += bytes.len() as u64;
        report.layers_uploaded += 1;
        s.upload(&digest, bytes).await?;
    }
    Ok(digest)
}

/// Pushes `src` (a disk file or a VM directory) to `reference` (`registry/repo:tag`).
pub async fn push(
    src: &Path,
    reference: &str,
    creds: Option<Credentials>,
    opts: &PushOptions,
) -> Result<PushReport> {
    let r = ImageRef::parse(reference)?;
    ensure!(
        !r.reference.starts_with("sha256:"),
        "push needs a tag, not a digest"
    );
    let chunk = match opts.chunk_size {
        0 => DEFAULT_CHUNK_SIZE,
        n => n,
    };
    ensure!(
        (MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk),
        "chunk size must be between {MIN_CHUNK_SIZE} and {MAX_CHUNK_SIZE} bytes"
    );
    let files = bundle_files(src, &opts.exclude)?;
    let os = opts
        .os
        .clone()
        .unwrap_or_else(|| detect_os(&files).to_owned());
    ensure!(
        os == "linux" || os == "macos",
        "os must be linux or macos, not {os:?}"
    );
    let config = VmArtifactConfig {
        format_version: 1,
        os,
        architecture: "arm64".into(),
        name: opts.name.clone(),
        chunk_size: chunk as u64,
        files: files.iter().map(|(_, f)| f.clone()).collect(),
        annotations: opts.annotations.clone(),
    };

    let mut s = Session::new(r.clone(), creds)?;
    let mut report = PushReport {
        reference: r.to_string(),
        manifest_digest: String::new(),
        files: config.files.clone(),
        layers: 0,
        layers_uploaded: 0,
        layers_already_present: 0,
        zero_chunks_skipped: 0,
        bytes_uploaded: 0,
    };

    let mut layers = Vec::new();
    for (path, file) in &files {
        let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut offset = 0u64;
        let mut buf = vec![0u8; chunk];
        loop {
            let n = read_full(&mut f, &mut buf)?;
            if n == 0 {
                break;
            }
            let data = &buf[..n];
            if data.iter().all(|&b| b == 0) {
                report.zero_chunks_skipped += 1;
            } else {
                let raw_digest = sha256_digest(data);
                let owned = data.to_vec();
                let compressed =
                    tokio::task::spawn_blocking(move || zstd::bulk::compress(&owned, ZSTD_LEVEL))
                        .await
                        .context("compression task")?
                        .context("zstd compression")?;
                let size = compressed.len() as u64;
                let digest = push_blob(&mut s, &mut report, compressed).await?;
                let mut annotations = BTreeMap::new();
                annotations.insert(ANN_FILE.to_owned(), file.name.clone());
                annotations.insert(ANN_OFFSET.to_owned(), offset.to_string());
                annotations.insert(ANN_RAW_SIZE.to_owned(), n.to_string());
                annotations.insert(ANN_RAW_DIGEST.to_owned(), raw_digest);
                layers.push(Desc {
                    media_type: MT_CHUNK.into(),
                    digest,
                    size,
                    annotations,
                });
            }
            offset += n as u64;
            if n < chunk {
                break;
            }
        }
        ensure!(
            offset == file.size,
            "{} changed size during the push ({} then {offset} bytes)",
            path.display(),
            file.size
        );
    }
    report.layers = layers.len();

    let config_bytes = serde_json::to_vec(&config)?;
    let config_desc = Desc {
        media_type: MT_CONFIG.into(),
        size: config_bytes.len() as u64,
        digest: push_blob(&mut s, &mut report, config_bytes).await?,
        annotations: BTreeMap::new(),
    };
    let mut annotations = BTreeMap::new();
    if let Some(n) = &opts.name {
        annotations.insert("org.opencontainers.image.title".to_owned(), n.clone());
    }
    let manifest = Manifest {
        schema_version: 2,
        media_type: MT_OCI_MANIFEST.into(),
        artifact_type: Some(ARTIFACT_TYPE.into()),
        config: config_desc,
        layers,
        annotations,
    };
    let bytes = serde_json::to_vec(&manifest)?;
    report.manifest_digest = sha256_digest(&bytes);
    s.put_manifest(&r.reference, bytes).await?;
    Ok(report)
}

/// Checks a manifest and its config describe a well-formed VM bundle, returning each file's name with its layers.
fn validate(manifest: &Manifest, config: &VmArtifactConfig) -> Result<()> {
    ensure!(
        config.format_version == 1,
        "unsupported VM artifact format {}",
        config.format_version
    );
    ensure!(!config.files.is_empty(), "artifact has no files");
    let mut seen = std::collections::HashSet::new();
    for f in &config.files {
        ensure!(
            safe_name(&f.name),
            "unsafe file name {:?} in artifact",
            f.name
        );
        ensure!(seen.insert(&f.name), "file {:?} listed twice", f.name);
    }
    for l in &manifest.layers {
        ensure!(
            l.media_type == MT_CHUNK,
            "unexpected layer media type {:?}",
            l.media_type
        );
        let file = l.annotations.get(ANN_FILE).context("layer without file")?;
        let f = config
            .files
            .iter()
            .find(|f| &f.name == file)
            .with_context(|| format!("layer for unknown file {file:?}"))?;
        let offset: u64 = l
            .annotations
            .get(ANN_OFFSET)
            .context("layer without offset")?
            .parse()
            .context("bad layer offset")?;
        let raw: u64 = l
            .annotations
            .get(ANN_RAW_SIZE)
            .context("layer without raw size")?
            .parse()
            .context("bad layer raw size")?;
        ensure!(
            raw > 0 && raw as usize <= MAX_CHUNK_SIZE,
            "layer raw size {raw} out of range"
        );
        ensure!(
            offset.checked_add(raw).is_some_and(|end| end <= f.size),
            "layer for {file:?} at {offset}+{raw} runs past the file's {} bytes",
            f.size
        );
        validate_digest(
            l.annotations
                .get(ANN_RAW_DIGEST)
                .context("layer without raw digest")?,
        )?;
    }
    Ok(())
}

/// Pulls a VM artifact into `dest`, which must not exist yet (it is created atomically: files land in a sibling
/// temporary directory that is renamed into place only once every chunk verified).
pub async fn pull(reference: &str, dest: &Path, creds: Option<Credentials>) -> Result<PullReport> {
    let r = ImageRef::parse(reference)?;
    ensure!(
        !dest.exists(),
        "{} already exists; pull into a new directory",
        dest.display()
    );
    let mut s = Session::new(r.clone(), creds)?;
    let url = format!("{}/manifests/{}", r.base_url(), r.reference);
    let bytes = s.get_bytes(&url, MT_OCI_MANIFEST, MAX_JSON_BYTES).await?;
    let manifest_digest = sha256_digest(&bytes);
    if r.reference.starts_with("sha256:") {
        ensure!(
            manifest_digest == r.reference,
            "manifest digest mismatch: asked for {}, got {manifest_digest}",
            r.reference
        );
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parsing manifest")?;
    if crate::tart::is_tart(manifest.layers.iter().map(|l| l.media_type.as_str())) {
        return pull_tart(&mut s, &r, &manifest, manifest_digest, dest).await;
    }
    ensure!(
        manifest.config.media_type == MT_CONFIG,
        "{r} is not a FluxVM VM image (config media type {:?})",
        manifest.config.media_type
    );
    let config_bytes = s.blob(&manifest.config, MAX_JSON_BYTES).await?;
    let config: VmArtifactConfig =
        serde_json::from_slice(&config_bytes).context("parsing VM artifact config")?;
    validate(&manifest, &config)?;

    let tmp = partial_dir(dest)?;
    let result = async {
        let mut handles = BTreeMap::new();
        for f in &config.files {
            let file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(tmp.join(&f.name))?;
            file.set_len(f.size)?;
            handles.insert(f.name.clone(), file);
        }
        let mut downloaded = 0u64;
        for l in &manifest.layers {
            let compressed = s.blob(l, MAX_CHUNK_SIZE as u64).await?;
            downloaded += compressed.len() as u64;
            let raw_size: usize = l.annotations[ANN_RAW_SIZE].parse()?;
            let raw =
                tokio::task::spawn_blocking(move || zstd::bulk::decompress(&compressed, raw_size))
                    .await
                    .context("decompression task")?
                    .context("zstd decompression")?;
            ensure!(
                raw.len() == raw_size,
                "chunk decompressed to {} bytes, expected {raw_size}",
                raw.len()
            );
            ensure!(
                sha256_digest(&raw) == l.annotations[ANN_RAW_DIGEST],
                "chunk content does not match its raw digest"
            );
            let offset: u64 = l.annotations[ANN_OFFSET].parse()?;
            write_sparse(&handles[&l.annotations[ANN_FILE]], offset, &raw)?;
        }
        for f in handles.values() {
            f.sync_all()?;
        }
        anyhow::Ok(downloaded)
    }
    .await;
    let downloaded = match result {
        Ok(d) => d,
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    fs::rename(&tmp, dest)
        .with_context(|| format!("moving the pulled image to {}", dest.display()))?;
    Ok(PullReport {
        reference: r.to_string(),
        manifest_digest,
        dir: dest.to_path_buf(),
        config,
        bytes_downloaded: downloaded,
    })
}

/// A fresh sibling directory of `dest` for a pull in progress; renamed onto `dest` once everything verified.
fn partial_dir(dest: &Path) -> Result<PathBuf> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.partial-{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("vm"),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir(&tmp)?;
    Ok(tmp)
}

/// Pulls a Tart image (see [`crate::tart`]) and converts it into a FluxVM bundle: `disk.raw`, the NVRAM as
/// `auxiliary.bin` (macOS) or `efi.bin` (Linux), and `hardware.bin` for macOS. Tart's machine identifier is dropped.
async fn pull_tart(
    s: &mut Session,
    r: &ImageRef,
    manifest: &Manifest,
    manifest_digest: String,
    dest: &Path,
) -> Result<PullReport> {
    use crate::tart;
    let layers: Vec<(&str, &BTreeMap<String, String>)> = manifest
        .layers
        .iter()
        .map(|l| (l.media_type.as_str(), &l.annotations))
        .collect();
    let (slices, disk_size) = tart::plan(&layers)?;
    let config_bytes = s.blob(&manifest.layers[0], tart::MAX_SMALL_BLOB).await?;
    let tcfg = tart::TartConfig::parse(&config_bytes)?;
    let hardware = tcfg.hardware_model_bytes()?;
    let nvram_desc = &manifest.layers[manifest.layers.len() - 1];

    let tmp = partial_dir(dest)?;
    let result = async {
        let mut downloaded = config_bytes.len() as u64;
        let disk = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(tmp.join("disk.raw"))?;
        disk.set_len(disk_size)?;
        for sl in &slices {
            // LZ4 can grow incompressible data slightly; allow for that over the 512 MiB slice.
            let compressed = s
                .blob(
                    &manifest.layers[sl.layer],
                    tart::MAX_SLICE + tart::MAX_SLICE / 64,
                )
                .await?;
            downloaded += compressed.len() as u64;
            let expected = usize::try_from(sl.size)?;
            let raw =
                tokio::task::spawn_blocking(move || tart::apple_lz4_decode(&compressed, expected))
                    .await
                    .context("decompression task")??;
            if let Some(d) = &sl.digest {
                ensure!(
                    &sha256_digest(&raw) == d,
                    "disk slice at offset {} does not match its uncompressed digest",
                    sl.offset
                );
            }
            write_sparse(&disk, sl.offset, &raw)?;
        }
        disk.sync_all()?;
        let nvram = s.blob(nvram_desc, tart::MAX_SMALL_BLOB).await?;
        downloaded += nvram.len() as u64;
        fs::write(tmp.join(tcfg.nvram_file()), &nvram)?;
        if let Some(hw) = &hardware {
            fs::write(tmp.join("hardware.bin"), hw)?;
        }
        anyhow::Ok((downloaded, nvram.len() as u64))
    }
    .await;
    let (downloaded, nvram_size) = match result {
        Ok(x) => x,
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    fs::rename(&tmp, dest)
        .with_context(|| format!("moving the pulled image to {}", dest.display()))?;

    let mut files = vec![
        ArtifactFile {
            name: "disk.raw".into(),
            size: disk_size,
        },
        ArtifactFile {
            name: tcfg.nvram_file().into(),
            size: nvram_size,
        },
    ];
    if let Some(hw) = &hardware {
        files.push(ArtifactFile {
            name: "hardware.bin".into(),
            size: hw.len() as u64,
        });
    }
    let mut annotations = BTreeMap::new();
    annotations.insert("dev.zyvor.fluxvm.source".to_owned(), "tart".to_owned());
    annotations.insert(
        "dev.zyvor.fluxvm.tart.cpu-count".to_owned(),
        tcfg.cpu_count.to_string(),
    );
    annotations.insert(
        "dev.zyvor.fluxvm.tart.memory-bytes".to_owned(),
        tcfg.memory_size.to_string(),
    );
    Ok(PullReport {
        reference: r.to_string(),
        manifest_digest,
        dir: dest.to_path_buf(),
        config: VmArtifactConfig {
            format_version: 1,
            os: tcfg.fluxvm_os().into(),
            architecture: "arm64".into(),
            name: manifest
                .annotations
                .get("org.opencontainers.image.title")
                .cloned(),
            chunk_size: tart::MAX_SLICE,
            files,
            annotations,
        },
        bytes_downloaded: downloaded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        http::{HeaderMap, Method, StatusCode, Uri},
        response::{IntoResponse, Response},
    };
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Reg {
        blobs: HashMap<String, Vec<u8>>,
        manifests: HashMap<String, Vec<u8>>,
        uploads: u32,
        puts: u32,
        require_auth: bool,
        realm: String,
    }

    type Shared = Arc<Mutex<Reg>>;

    async fn handle(
        state: Shared,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let path = uri.path().to_owned();
        let query = uri.query().unwrap_or_default().to_owned();
        let mut reg = state.lock().unwrap();
        if path == "/token" {
            return (StatusCode::OK, r#"{"token":"good"}"#).into_response();
        }
        let authed =
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good");
        if reg.require_auth && !authed {
            let challenge = format!(
                "Bearer realm=\"{}\",service=\"test\",scope=\"repository:vm:pull\"",
                reg.realm
            );
            return (
                StatusCode::UNAUTHORIZED,
                [("www-authenticate", challenge)],
                "",
            )
                .into_response();
        }
        if let Some((_, rest)) = path.split_once("/blobs/uploads/") {
            if method == Method::POST && rest.is_empty() {
                reg.uploads += 1;
                let loc = format!("{}{}", path, reg.uploads);
                return (StatusCode::ACCEPTED, [("location", loc)], "").into_response();
            }
            if method == Method::PUT {
                let digest = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("digest="))
                    .unwrap_or_default()
                    .to_owned();
                if sha256_digest(&body) != digest {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                reg.puts += 1;
                reg.blobs.insert(digest, body.to_vec());
                return StatusCode::CREATED.into_response();
            }
        }
        if let Some((_, digest)) = path.split_once("/blobs/") {
            return match reg.blobs.get(digest) {
                Some(b) if method == Method::GET => (StatusCode::OK, b.clone()).into_response(),
                Some(_) => StatusCode::OK.into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        if let Some((_, reference)) = path.split_once("/manifests/") {
            if method == Method::PUT {
                reg.manifests.insert(reference.to_owned(), body.to_vec());
                reg.manifests.insert(sha256_digest(&body), body.to_vec());
                return StatusCode::CREATED.into_response();
            }
            return match reg.manifests.get(reference) {
                Some(b) => (
                    StatusCode::OK,
                    [("content-type", MT_OCI_MANIFEST)],
                    b.clone(),
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        StatusCode::NOT_FOUND.into_response()
    }

    async fn registry(require_auth: bool) -> (String, Shared) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state: Shared = Arc::new(Mutex::new(Reg {
            require_auth,
            realm: format!("http://{addr}/token"),
            ..Default::default()
        }));
        let st = state.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                handle(st.clone(), method, uri, headers, body)
            },
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("127.0.0.1:{}", addr.port()), state)
    }

    /// A 5 MiB "disk": non-zero data, a 3 MiB hole, a non-zero tail.
    fn macos_template(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        let mut disk = vec![0u8; 5 * 1024 * 1024];
        disk[..1024 * 1024].fill(7);
        let tail = disk.len() - 10;
        disk[tail..].copy_from_slice(b"0123456789");
        fs::write(dir.join("disk.raw"), &disk).unwrap();
        fs::write(dir.join("hardware.bin"), b"hw-model").unwrap();
        fs::write(dir.join("auxiliary.bin"), b"nvram").unwrap();
        fs::write(dir.join("identity.bin"), b"machine-id").unwrap();
        fs::write(dir.join("runner.log"), b"log").unwrap();
        fs::write(dir.join("state.vzvmsave"), b"saved").unwrap();
    }

    #[test]
    fn identity_and_runtime_files_are_left_out() {
        let d = tempfile::tempdir().unwrap();
        macos_template(d.path());
        let names: Vec<String> = bundle_files(d.path(), &[])
            .unwrap()
            .into_iter()
            .map(|(_, f)| f.name)
            .collect();
        assert_eq!(names, ["auxiliary.bin", "disk.raw", "hardware.bin"]);
        let files = bundle_files(d.path(), &["hardware.bin".into()]).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(detect_os(&bundle_files(d.path(), &[]).unwrap()), "macos");
    }

    #[test]
    fn unsafe_names_are_refused() {
        for bad in ["", ".", "..", "../x", "a/b", ".hidden", "x\0y"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        assert!(safe_name("disk.raw"));
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_sparsely_with_auth() {
        let (host, reg) = registry(true).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("tmpl");
        macos_template(&src);
        let opts = PushOptions {
            chunk_size: 1024 * 1024,
            name: Some("macos-27-xcode".into()),
            ..Default::default()
        };
        let reference = format!("{host}/vm/macos:27");
        let report = push(&src, &reference, None, &opts).await.unwrap();
        // disk: chunks 0 and 4 have data, 1-3 are zero; plus hardware.bin and auxiliary.bin.
        assert_eq!(report.zero_chunks_skipped, 3);
        assert_eq!(report.layers, 4);
        assert_eq!(report.layers_uploaded, 5, "4 chunks + config");

        // A second push of the same bundle uploads nothing new.
        let again = push(&src, &reference, None, &opts).await.unwrap();
        assert_eq!(again.layers_uploaded, 0);
        assert_eq!(again.layers_already_present, 5);
        assert_eq!(again.manifest_digest, report.manifest_digest);
        assert_eq!(reg.lock().unwrap().puts, 5);

        let dest = d.path().join("pulled");
        let pulled = pull(&reference, &dest, None).await.unwrap();
        assert_eq!(pulled.config.os, "macos");
        assert_eq!(pulled.config.name.as_deref(), Some("macos-27-xcode"));
        for f in ["disk.raw", "hardware.bin", "auxiliary.bin"] {
            assert_eq!(
                fs::read(dest.join(f)).unwrap(),
                fs::read(src.join(f)).unwrap(),
                "{f}"
            );
        }
        assert!(!dest.join("identity.bin").exists());
        assert!(!dest.join("runner.log").exists());

        // By digest too.
        let by_digest = format!("{host}/vm/macos@{}", report.manifest_digest);
        let dest2 = d.path().join("pulled2");
        pull(&by_digest, &dest2, None).await.unwrap();
        assert_eq!(
            fs::read(dest2.join("disk.raw")).unwrap(),
            fs::read(src.join("disk.raw")).unwrap()
        );
    }

    #[tokio::test]
    async fn a_single_disk_file_pushes_as_linux() {
        let (host, _) = registry(false).await;
        let d = tempfile::tempdir().unwrap();
        let disk = d.path().join("debian.raw");
        fs::write(&disk, vec![1u8; 3 * 1024 * 1024 + 5]).unwrap();
        let reference = format!("{host}/vm/debian:13");
        let report = push(
            &disk,
            &reference,
            None,
            &PushOptions {
                chunk_size: 1024 * 1024,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Identical 1 MiB chunks dedupe to one blob: 4 layers, 2 distinct chunk blobs + config uploaded.
        assert_eq!(report.layers, 4);
        assert_eq!(report.layers_uploaded, 3);
        let dest = d.path().join("out");
        let pulled = pull(&reference, &dest, None).await.unwrap();
        assert_eq!(pulled.config.os, "linux");
        assert_eq!(
            fs::read(dest.join("debian.raw")).unwrap(),
            fs::read(&disk).unwrap()
        );
    }

    #[tokio::test]
    async fn tampered_chunks_fail_and_leave_nothing_behind() {
        let (host, reg) = registry(false).await;
        let d = tempfile::tempdir().unwrap();
        let disk = d.path().join("x.raw");
        fs::write(&disk, vec![9u8; 2 * 1024 * 1024]).unwrap();
        let reference = format!("{host}/vm/x:1");
        push(
            &disk,
            &reference,
            None,
            &PushOptions {
                chunk_size: 1024 * 1024,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Corrupt every stored chunk blob (not the config).
        {
            let mut r = reg.lock().unwrap();
            let config_like: Vec<String> = r
                .blobs
                .iter()
                .filter(|(_, b)| b.starts_with(b"{"))
                .map(|(k, _)| k.clone())
                .collect();
            for (k, v) in r.blobs.iter_mut() {
                if !config_like.contains(k) {
                    v[0] ^= 0xff;
                }
            }
        }
        let dest = d.path().join("out");
        let err = pull(&reference, &dest, None).await.unwrap_err();
        assert!(format!("{err:#}").contains("digest mismatch"), "{err:#}");
        assert!(!dest.exists());
        let leftovers: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(leftovers.is_empty());
    }

    /// Stores a Tart image in the fake registry: three disk slices (LZ4 with a cross-block match, an all-zero stored
    /// slice, a plain LZ4 block) and the NVRAM. Returns the slices' expected raw bytes.
    fn store_tart_image(reg: &Shared, reference: &str, os: &str, tamper: bool) -> Vec<u8> {
        use crate::tart::{
            self,
            tests::{BLOCK, frame},
        };
        let mut reg = reg.lock().unwrap();
        let mut put = |bytes: Vec<u8>| -> (String, u64) {
            let d = sha256_digest(&bytes);
            let n = bytes.len() as u64;
            reg.blobs.insert(d.clone(), bytes);
            (d, n)
        };
        let second: &[u8] = &[0x0F, 3, 0, 0, 0x10, b'!'];
        let slices: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (
                frame(&[(b"xyz".as_slice(), None), (second, Some(20))]),
                b"xyzxyzxyzxyzxyzxyzxyzx!".to_vec(),
            ),
            (frame(&[(&[0u8; 4096][..], None)]), vec![0u8; 4096]),
            (frame(&[(BLOCK, Some(13))]), b"abcabcabcabcx".to_vec()),
        ];
        let config = json!({"version": 1, "os": os, "arch": "arm64", "cpuCount": 4,
            "memorySize": 8u64 << 30, "hardwareModel": "aHctbW9kZWw=", "ecid": "aWQ="})
        .to_string()
        .into_bytes();
        let (cd, cn) = put(config);
        let mut layers = vec![json!({"mediaType": tart::MT_CONFIG, "digest": cd, "size": cn})];
        let mut raw_all = Vec::new();
        for (i, (compressed, raw)) in slices.into_iter().enumerate() {
            let digest = if tamper && i == 2 {
                sha256_digest(b"other")
            } else {
                sha256_digest(&raw)
            };
            let (d, n) = put(compressed);
            layers.push(
                json!({"mediaType": tart::MT_DISK_V2, "digest": d, "size": n, "annotations": {
                    tart::ANN_UNCOMPRESSED_SIZE: raw.len().to_string(),
                    tart::ANN_UNCOMPRESSED_DIGEST: digest,
                }}),
            );
            raw_all.extend(raw);
        }
        let (nd, nn) = put(b"nvram-bytes".to_vec());
        layers.push(json!({"mediaType": tart::MT_NVRAM, "digest": nd, "size": nn}));
        let (sd, sn) = put(br#"{"architecture":"arm64","os":"darwin"}"#.to_vec());
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": MT_OCI_MANIFEST,
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": sd, "size": sn},
            "layers": layers,
            "annotations": {"org.cirruslabs.tart.uncompressed-disk-size": raw_all.len().to_string()},
        })
        .to_string()
        .into_bytes();
        reg.manifests.insert(reference.to_owned(), manifest);
        raw_all
    }

    #[tokio::test]
    async fn tart_images_convert_into_fluxvm_bundles() {
        let (host, reg) = registry(true).await;
        let raw = store_tart_image(&reg, "sequoia-base", "darwin", false);
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("mac");
        let pulled = pull(
            &format!("{host}/cirruslabs/macos:sequoia-base"),
            &dest,
            None,
        )
        .await
        .unwrap();
        assert_eq!(pulled.config.os, "macos");
        assert_eq!(pulled.config.annotations["dev.zyvor.fluxvm.source"], "tart");
        assert_eq!(
            pulled.config.annotations["dev.zyvor.fluxvm.tart.cpu-count"],
            "4"
        );
        assert_eq!(fs::read(dest.join("disk.raw")).unwrap(), raw);
        assert_eq!(
            fs::read(dest.join("auxiliary.bin")).unwrap(),
            b"nvram-bytes"
        );
        assert_eq!(fs::read(dest.join("hardware.bin")).unwrap(), b"hw-model");
        assert!(
            !dest.join("identity.bin").exists(),
            "Tart's ecid is not carried over"
        );

        // Linux: the NVRAM is the EFI variable store and there is no hardware model.
        store_tart_image(&reg, "debian", "linux", false);
        let dest = d.path().join("linux");
        pull(&format!("{host}/cirruslabs/macos:debian"), &dest, None)
            .await
            .unwrap();
        assert_eq!(fs::read(dest.join("efi.bin")).unwrap(), b"nvram-bytes");
        assert!(!dest.join("hardware.bin").exists());
    }

    #[tokio::test]
    async fn tart_slices_are_checked_against_their_digest() {
        let (host, reg) = registry(false).await;
        store_tart_image(&reg, "bad", "darwin", true);
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("mac");
        let err = pull(&format!("{host}/cirruslabs/macos:bad"), &dest, None)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("uncompressed digest"),
            "{err:#}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn manifests_pointing_outside_their_files_are_refused() {
        let config = VmArtifactConfig {
            format_version: 1,
            os: "linux".into(),
            architecture: "arm64".into(),
            name: None,
            chunk_size: 1024,
            files: vec![ArtifactFile {
                name: "disk.raw".into(),
                size: 100,
            }],
            annotations: BTreeMap::new(),
        };
        let layer = |file: &str, offset: u64, raw: u64| Desc {
            media_type: MT_CHUNK.into(),
            digest: sha256_digest(b"x"),
            size: 1,
            annotations: [
                (ANN_FILE.to_owned(), file.to_owned()),
                (ANN_OFFSET.to_owned(), offset.to_string()),
                (ANN_RAW_SIZE.to_owned(), raw.to_string()),
                (ANN_RAW_DIGEST.to_owned(), sha256_digest(b"y")),
            ]
            .into(),
        };
        let manifest = |l: Desc| Manifest {
            schema_version: 2,
            media_type: MT_OCI_MANIFEST.into(),
            artifact_type: Some(ARTIFACT_TYPE.into()),
            config: layer("disk.raw", 0, 1),
            layers: vec![l],
            annotations: BTreeMap::new(),
        };
        assert!(validate(&manifest(layer("disk.raw", 0, 100)), &config).is_ok());
        assert!(validate(&manifest(layer("disk.raw", 50, 51)), &config).is_err());
        assert!(validate(&manifest(layer("other.raw", 0, 1)), &config).is_err());
        assert!(validate(&manifest(layer("disk.raw", u64::MAX, 2)), &config).is_err());
        let mut bad = config.clone();
        bad.files[0].name = "../etc/passwd".into();
        assert!(validate(&manifest(layer("../etc/passwd", 0, 1)), &bad).is_err());
    }
}
