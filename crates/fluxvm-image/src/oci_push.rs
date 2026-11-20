// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A VM disk image as a single-layer OCI artifact, like KubeVirt's containerDisk but pushed and pulled in pure Rust
//! (no skopeo or docker): [`push`] streams one raw or qcow2 file to a registry, [`pull_disk`] fetches it back.
//!
//! # Format
//!
//! An OCI image manifest (v1.1) with `artifactType` [`ARTIFACT_TYPE`]:
//!
//! - **config** ([`MT_CONFIG`]): JSON with `architecture`, `os` (`linux`), the disk `format` and its virtual size.
//! - **one layer**: the disk file itself, uncompressed, with media type `application/vnd.fluxvm.disk.layer.v1.raw` or
//!   `….qcow2`. Its digest is the file's sha256, computed while streaming; nothing larger than one upload chunk is
//!   ever held in memory.
//!
//! For chunked, compressed, sparse-aware transfers of whole VM bundles (macOS templates), see [`crate::vm_artifact`].
//!
//! # Integrity
//!
//! The pusher hashes the file before uploading (so an existing blob is skipped with `HEAD`) and again while sending;
//! a file that changed in between aborts the push. The registry's `Docker-Content-Digest` for the manifest must equal
//! the digest computed locally. The puller checks the manifest against a pinned `@sha256:` reference, the config
//! against its descriptor, and the layer's size and digest while streaming it into a temporary file that is renamed
//! onto the destination only once everything verified.

use crate::oci_registry::{
    Arch, Credentials, Descriptor, ImageRef, MAX_LAYER_BYTES, sha256_digest, validate_digest,
};
use crate::vm_artifact::Session;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const ARTIFACT_TYPE: &str = "application/vnd.fluxvm.disk.v1";
pub const MT_CONFIG: &str = "application/vnd.fluxvm.disk.config.v1+json";
pub const MT_LAYER_PREFIX: &str = "application/vnd.fluxvm.disk.layer.v1.";
const MT_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";

/// Bytes per upload request. Files up to this size go up in one monolithic `PUT`; larger ones in `PATCH` chunks.
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024 * 1024;
const MAX_CHUNK_SIZE: usize = 512 * 1024 * 1024;
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;

/// On-disk format of the pushed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    Raw,
    Qcow2,
}

impl DiskFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Ok(Self::Raw),
            "qcow2" => Ok(Self::Qcow2),
            other => bail!("unknown disk format {other:?}: use raw or qcow2"),
        }
    }

    /// `application/vnd.fluxvm.disk.layer.v1.<raw|qcow2>`.
    pub fn layer_media_type(self) -> String {
        format!("{MT_LAYER_PREFIX}{}", self.as_str())
    }

    pub fn from_layer_media_type(mt: &str) -> Result<Self> {
        Self::parse(
            mt.strip_prefix(MT_LAYER_PREFIX)
                .with_context(|| format!("unsupported layer media type {mt:?}"))?,
        )
    }

    /// `qcow2` when the file starts with the qcow2 magic, else `raw`.
    pub fn detect(path: &Path) -> Result<Self> {
        Ok(if crate::qcow2::is_qcow2(path)? {
            Self::Qcow2
        } else {
            Self::Raw
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Detected from the file's magic when unset; when set it must agree with the file.
    pub format: Option<DiskFormat>,
    /// OCI architecture (`arm64` or `amd64`); the host's when unset.
    pub architecture: Option<String>,
    /// Bytes per upload request; 0 means [`DEFAULT_CHUNK_SIZE`].
    pub chunk_size: usize,
    /// Manifest annotations, for example `org.opencontainers.image.title`.
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushedImage {
    pub reference: String,
    pub manifest_digest: String,
    /// Size of the manifest document in bytes.
    pub size: u64,
    pub layer_digest: String,
    /// Size of the disk file, which is the layer.
    pub layer_size: u64,
    pub format: DiskFormat,
    pub virtual_size: u64,
    /// False when the registry already had the layer and nothing was uploaded.
    pub layer_uploaded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PulledDisk {
    pub reference: String,
    pub manifest_digest: String,
    pub path: PathBuf,
    pub format: DiskFormat,
    pub architecture: String,
    pub size: u64,
    pub virtual_size: u64,
    pub layer_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskInfo {
    format: DiskFormat,
    #[serde(rename = "virtualSize")]
    virtual_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RootFs {
    #[serde(rename = "type")]
    kind: String,
    diff_ids: Vec<String>,
}

/// The config blob: the standard `architecture`/`os`/`rootfs` fields tools expect, plus the disk description.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskConfig {
    architecture: String,
    os: String,
    rootfs: RootFs,
    disk: DiskInfo,
}

#[derive(Debug, Serialize, Deserialize)]
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
    config: Descriptor,
    layers: Vec<Descriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

fn qcow2_virtual_size(path: &Path) -> Result<u64> {
    let mut b = [0u8; 8];
    std::fs::File::open(path)?
        .read_exact_at(&mut b, 24)
        .with_context(|| format!("{} is too short for a qcow2 header", path.display()))?;
    Ok(u64::from_be_bytes(b))
}

/// sha256 and length of a file, read in 1 MiB steps.
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((format!("sha256:{}", hex::encode(h.finalize())), total))
}

/// The upload URL a registry's `Location` points at, which may be absolute, root-relative or relative to `base`.
fn resolve_location(base: &str, location: &str) -> Result<String> {
    let base = reqwest::Url::parse(base).with_context(|| format!("bad upload URL {base:?}"))?;
    Ok(base
        .join(location)
        .with_context(|| format!("bad upload Location {location:?}"))?
        .to_string())
}

fn location_of(resp: &reqwest::Response, base: &str) -> Result<String> {
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("upload response has no Location")?;
    resolve_location(base, loc)
}

fn with_digest(url: &str, digest: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}digest={digest}")
}

async fn read_chunk(f: &mut tokio::fs::File, n: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n);
    (&mut *f).take(n as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Uploads a file as blob `digest` without holding more than one chunk in memory: a single `PUT` when it fits in one
/// chunk, otherwise `POST`, a `PATCH` per chunk and a closing `PUT`.
async fn upload_file(
    s: &mut Session,
    path: &Path,
    digest: &str,
    size: u64,
    chunk: usize,
) -> Result<()> {
    let start = format!("{}/blobs/uploads/", s.r.base_url());
    let resp = s.send(|h| h.post(&start)).await?;
    ensure!(
        resp.status().as_u16() == 202,
        "POST {start}: HTTP {}",
        resp.status()
    );
    let mut location = location_of(&resp, &start)?;
    let mut f = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut sent = 0u64;

    if size <= chunk as u64 {
        let data = read_chunk(&mut f, chunk).await?;
        ensure!(
            data.len() as u64 == size && sha256_digest(&data) == digest,
            "{} changed during the push",
            path.display()
        );
        let url = with_digest(&location, digest);
        let n = data.len();
        let resp = s
            .send(|h| {
                h.put(&url)
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", n)
                    .body(data.clone())
            })
            .await?;
        ensure!(
            resp.status().as_u16() == 201,
            "PUT blob {digest}: HTTP {}",
            resp.status()
        );
        return Ok(());
    }

    while sent < size {
        let data = read_chunk(&mut f, chunk).await?;
        ensure!(
            !data.is_empty(),
            "{} shrank during the push ({sent} of {size} bytes)",
            path.display()
        );
        hasher.update(&data);
        let n = data.len() as u64;
        let range = format!("{sent}-{}", sent + n - 1);
        let url = location.clone();
        let resp = s
            .send(|h| {
                h.patch(&url)
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Range", &range)
                    .header("Content-Length", n)
                    .body(data.clone())
            })
            .await?;
        ensure!(
            resp.status().as_u16() == 202,
            "PATCH blob {digest} at {sent}: HTTP {}",
            resp.status()
        );
        location = location_of(&resp, &url)?;
        sent += n;
    }
    ensure!(
        sent == size && format!("sha256:{}", hex::encode(hasher.finalize())) == digest,
        "{} changed during the push",
        path.display()
    );
    let url = with_digest(&location, digest);
    let resp = s.send(|h| h.put(&url).header("Content-Length", 0)).await?;
    ensure!(
        resp.status().as_u16() == 201,
        "closing upload of blob {digest}: HTTP {}",
        resp.status()
    );
    Ok(())
}

/// Pushes the disk image `path` to `reference` (`registry/repo:tag`). Credentials come from the caller; the token
/// request asks for `pull,push` on the repository.
pub async fn push(
    path: &Path,
    reference: &str,
    creds: Option<Credentials>,
    opts: &PushOptions,
) -> Result<PushedImage> {
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
        chunk <= MAX_CHUNK_SIZE,
        "chunk size must be at most {MAX_CHUNK_SIZE} bytes"
    );
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(meta.is_file(), "{} is not a file", path.display());
    ensure!(meta.len() > 0, "{} is empty", path.display());
    ensure!(
        meta.len() <= MAX_LAYER_BYTES,
        "{} is {} bytes (limit {MAX_LAYER_BYTES})",
        path.display(),
        meta.len()
    );

    let detected = DiskFormat::detect(path)?;
    let format = opts.format.unwrap_or(detected);
    ensure!(
        format == detected,
        "{} looks like {} but --format says {}",
        path.display(),
        detected.as_str(),
        format.as_str()
    );
    let virtual_size = match format {
        DiskFormat::Raw => meta.len(),
        DiskFormat::Qcow2 => qcow2_virtual_size(path)?,
    };
    let architecture = match &opts.architecture {
        Some(a) => Arch::parse(a)?,
        None => Arch::parse(std::env::consts::ARCH)?,
    }
    .as_str()
    .to_owned();

    let hash_path = path.to_path_buf();
    let (layer_digest, layer_size) = tokio::task::spawn_blocking(move || hash_file(&hash_path))
        .await
        .context("hashing task")??;
    ensure!(
        layer_size == meta.len(),
        "{} changed size while hashing",
        path.display()
    );

    let config = DiskConfig {
        architecture,
        os: "linux".into(),
        rootfs: RootFs {
            kind: "layers".into(),
            diff_ids: vec![layer_digest.clone()],
        },
        disk: DiskInfo {
            format,
            virtual_size,
        },
    };
    let config_bytes = serde_json::to_vec(&config)?;
    let config_desc = Descriptor {
        media_type: MT_CONFIG.into(),
        digest: sha256_digest(&config_bytes),
        size: config_bytes.len() as u64,
        platform: None,
    };

    let mut s = Session::new(r.clone(), creds)?;
    let layer_uploaded = if s.blob_exists(&layer_digest).await? {
        false
    } else {
        upload_file(&mut s, path, &layer_digest, layer_size, chunk).await?;
        true
    };
    if !s.blob_exists(&config_desc.digest).await? {
        s.upload(&config_desc.digest, config_bytes).await?;
    }

    let manifest = Manifest {
        schema_version: 2,
        media_type: MT_OCI_MANIFEST.into(),
        artifact_type: Some(ARTIFACT_TYPE.into()),
        config: config_desc,
        layers: vec![Descriptor {
            media_type: format.layer_media_type(),
            digest: layer_digest.clone(),
            size: layer_size,
            platform: None,
        }],
        annotations: opts.annotations.clone(),
    };
    let bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = sha256_digest(&bytes);
    let size = bytes.len() as u64;
    let url = format!("{}/manifests/{}", r.base_url(), r.reference);
    let resp = s
        .send(|h| {
            h.put(&url)
                .header("Content-Type", MT_OCI_MANIFEST)
                .body(bytes.clone())
        })
        .await?;
    ensure!(
        resp.status().is_success(),
        "PUT manifest {}: HTTP {}",
        r.reference,
        resp.status()
    );
    if let Some(got) = resp
        .headers()
        .get("docker-content-digest")
        .and_then(|v| v.to_str().ok())
    {
        ensure!(
            got == manifest_digest,
            "registry stored manifest {got}, expected {manifest_digest}"
        );
    }
    Ok(PushedImage {
        reference: r.to_string(),
        manifest_digest,
        size,
        layer_digest,
        layer_size,
        format,
        virtual_size,
        layer_uploaded,
    })
}

/// A fresh sibling of `out` for a download in progress.
fn partial_path(out: &Path) -> Result<PathBuf> {
    let name = out.file_name().context("destination has no file name")?;
    let mut n = name.to_os_string();
    n.push(format!(".part-{}", std::process::id()));
    Ok(out.with_file_name(n))
}

/// Pulls the disk image `reference` into the new file `out`, verifying the manifest (when pinned by digest), the
/// config and the layer's size and sha256. `out` must not exist; it appears only once the whole layer verified.
pub async fn pull_disk(
    reference: &str,
    out: &Path,
    creds: Option<Credentials>,
) -> Result<PulledDisk> {
    let r = ImageRef::parse(reference)?;
    ensure!(
        !out.exists(),
        "{} already exists; pull into a new file",
        out.display()
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
    ensure!(
        manifest.artifact_type.as_deref() == Some(ARTIFACT_TYPE),
        "{r} is not a FluxVM disk image (artifact type {:?})",
        manifest.artifact_type
    );
    ensure!(
        manifest.config.media_type == MT_CONFIG,
        "unexpected config media type {:?}",
        manifest.config.media_type
    );
    ensure!(
        manifest.layers.len() == 1,
        "a disk image has one layer, this has {}",
        manifest.layers.len()
    );
    let layer = &manifest.layers[0];
    let format = DiskFormat::from_layer_media_type(&layer.media_type)?;
    validate_digest(&layer.digest)?;
    validate_digest(&manifest.config.digest)?;
    ensure!(
        layer.size > 0 && layer.size <= MAX_LAYER_BYTES,
        "layer size {} out of range",
        layer.size
    );

    ensure!(
        manifest.config.size <= MAX_JSON_BYTES,
        "config is {} bytes (limit {MAX_JSON_BYTES})",
        manifest.config.size
    );
    let curl = format!("{}/blobs/{}", r.base_url(), manifest.config.digest);
    let config_bytes = s.get_bytes(&curl, "*/*", manifest.config.size).await?;
    let got = sha256_digest(&config_bytes);
    ensure!(
        got == manifest.config.digest,
        "config digest mismatch: expected {}, got {got}",
        manifest.config.digest
    );
    let config: DiskConfig = serde_json::from_slice(&config_bytes).context("parsing config")?;
    ensure!(
        config.disk.format == format,
        "config says {} but the layer is {}",
        config.disk.format.as_str(),
        format.as_str()
    );

    let tmp = partial_path(out)?;
    let result = download_layer(&mut s, &r, layer, &tmp).await;
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("moving the pulled disk to {}", out.display()));
    }
    Ok(PulledDisk {
        reference: r.to_string(),
        manifest_digest,
        path: out.to_path_buf(),
        format,
        architecture: config.architecture,
        size: layer.size,
        virtual_size: config.disk.virtual_size,
        layer_digest: layer.digest.clone(),
    })
}

/// Streams a layer blob into `tmp`, failing on any byte past its declared size, a short read or a digest mismatch.
async fn download_layer(
    s: &mut Session,
    r: &ImageRef,
    layer: &Descriptor,
    tmp: &Path,
) -> Result<()> {
    let url = format!("{}/blobs/{}", r.base_url(), layer.digest);
    let mut resp = s.send(|h| h.get(&url)).await?;
    ensure!(
        resp.status().is_success(),
        "GET {url}: HTTP {}",
        resp.status()
    );
    let mut f = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    while let Some(chunk) = resp.chunk().await.context("reading layer")? {
        got += chunk.len() as u64;
        ensure!(
            got <= layer.size,
            "layer {} is larger than its declared {} bytes",
            layer.digest,
            layer.size
        );
        hasher.update(&chunk);
        f.write_all(&chunk).await?;
    }
    ensure!(
        got == layer.size,
        "layer {} is {got} bytes, expected {}",
        layer.digest,
        layer.size
    );
    let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
    ensure!(
        digest == layer.digest,
        "layer digest mismatch: expected {}, got {digest}",
        layer.digest
    );
    f.sync_all().await?;
    Ok(())
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
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const QCOW2_MAGIC: &[u8; 4] = b"QFI\xfb";

    #[derive(Default)]
    struct Reg {
        blobs: HashMap<String, Vec<u8>>,
        manifests: HashMap<String, Vec<u8>>,
        uploads: HashMap<String, Vec<u8>>,
        next_upload: u32,
        patches: u32,
        blob_puts: u32,
        token_requests: u32,
        require_auth: bool,
        realm: String,
        /// Reply to a manifest PUT with a wrong `Docker-Content-Digest`.
        lie_about_digest: bool,
        /// Serve these blobs with their first byte flipped.
        corrupt: Vec<String>,
        /// Use `http://host/…` rather than `/v2/…` upload locations.
        absolute_location: bool,
    }

    type Shared = Arc<Mutex<Reg>>;

    fn digest_param(query: &str) -> String {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix("digest="))
            .unwrap_or_default()
            .to_owned()
    }

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
            reg.token_requests += 1;
            if !(query.contains("pull,push") || query.contains("pull%2Cpush")) {
                return StatusCode::FORBIDDEN.into_response();
            }
            return (StatusCode::OK, r#"{"token":"good"}"#).into_response();
        }
        let authed =
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good");
        if reg.require_auth && !authed {
            let challenge = format!(
                "Bearer realm=\"{}\",service=\"test\",scope=\"repository:disk:pull\"",
                reg.realm
            );
            return (
                StatusCode::UNAUTHORIZED,
                [("www-authenticate", challenge)],
                "",
            )
                .into_response();
        }
        if let Some((prefix, rest)) = path.split_once("/blobs/uploads/") {
            let host = headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let absolute = reg.absolute_location;
            let loc = |id: &str| {
                if absolute {
                    format!("http://{host}{prefix}/blobs/uploads/{id}")
                } else {
                    format!("{prefix}/blobs/uploads/{id}")
                }
            };
            if method == Method::POST && rest.is_empty() {
                reg.next_upload += 1;
                let id = reg.next_upload.to_string();
                reg.uploads.insert(id.clone(), Vec::new());
                let l = loc(&id);
                return (StatusCode::ACCEPTED, [("location", l)], "").into_response();
            }
            if method == Method::PATCH {
                let want = headers
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.split_once('-'))
                    .and_then(|(a, _)| a.parse::<usize>().ok());
                let cur = reg.uploads.get(rest).map(|u| u.len());
                if cur.is_none() || want != cur {
                    return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
                }
                reg.patches += 1;
                reg.uploads.get_mut(rest).unwrap().extend_from_slice(&body);
                let l = loc(rest);
                return (StatusCode::ACCEPTED, [("location", l)], "").into_response();
            }
            if method == Method::PUT {
                let Some(mut data) = reg.uploads.remove(rest) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                data.extend_from_slice(&body);
                let digest = digest_param(&query);
                if sha256_digest(&data) != digest {
                    return StatusCode::BAD_REQUEST.into_response();
                }
                reg.blob_puts += 1;
                reg.blobs.insert(digest, data);
                return StatusCode::CREATED.into_response();
            }
        }
        if let Some((_, digest)) = path.split_once("/blobs/") {
            return match reg.blobs.get(digest) {
                Some(b) if method == Method::GET => {
                    let mut b = b.clone();
                    if reg.corrupt.iter().any(|d| d == digest) {
                        b[0] ^= 0xff;
                    }
                    (StatusCode::OK, b).into_response()
                }
                Some(_) => StatusCode::OK.into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        if let Some((_, reference)) = path.split_once("/manifests/") {
            if method == Method::PUT {
                let digest = sha256_digest(&body);
                reg.manifests.insert(reference.to_owned(), body.to_vec());
                reg.manifests.insert(digest.clone(), body.to_vec());
                let reported = if reg.lie_about_digest {
                    sha256_digest(b"something else")
                } else {
                    digest
                };
                return (
                    StatusCode::CREATED,
                    [("docker-content-digest", reported)],
                    "",
                )
                    .into_response();
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

    async fn registry(configure: impl FnOnce(&mut Reg)) -> (String, Shared) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut reg = Reg {
            realm: format!("http://{addr}/token"),
            ..Default::default()
        };
        configure(&mut reg);
        let state: Shared = Arc::new(Mutex::new(reg));
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

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8 + 1).collect()
    }

    fn opts(chunk: usize) -> PushOptions {
        PushOptions {
            architecture: Some("arm64".into()),
            chunk_size: chunk,
            ..Default::default()
        }
    }

    fn qcow2_file(dir: &Path, virtual_size: u64) -> PathBuf {
        let mut b = sample(3000);
        b[..4].copy_from_slice(QCOW2_MAGIC);
        b[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        let p = dir.join("d.qcow2");
        std::fs::write(&p, b).unwrap();
        p
    }

    #[test]
    fn format_names_and_media_types() {
        assert_eq!(DiskFormat::parse("QCOW2").unwrap(), DiskFormat::Qcow2);
        assert!(DiskFormat::parse("vmdk").is_err());
        assert_eq!(
            DiskFormat::Raw.layer_media_type(),
            "application/vnd.fluxvm.disk.layer.v1.raw"
        );
        assert_eq!(
            DiskFormat::from_layer_media_type("application/vnd.fluxvm.disk.layer.v1.qcow2")
                .unwrap(),
            DiskFormat::Qcow2
        );
        assert!(
            DiskFormat::from_layer_media_type("application/vnd.oci.image.layer.v1.tar").is_err()
        );
    }

    #[test]
    fn locations_resolve_whatever_form_the_registry_uses() {
        let base = "http://r:5000/v2/a/b/blobs/uploads/";
        assert_eq!(
            resolve_location(base, "/v2/a/b/blobs/uploads/9").unwrap(),
            "http://r:5000/v2/a/b/blobs/uploads/9"
        );
        assert_eq!(
            resolve_location(base, "http://other:1/x?_state=abc").unwrap(),
            "http://other:1/x?_state=abc"
        );
        assert_eq!(
            resolve_location(base, "7?_state=abc").unwrap(),
            "http://r:5000/v2/a/b/blobs/uploads/7?_state=abc"
        );
        assert_eq!(
            with_digest("http://r/u?_state=abc", "sha256:x"),
            "http://r/u?_state=abc&digest=sha256:x"
        );
    }

    #[test]
    fn manifest_is_an_oci_1_1_artifact() {
        let m = Manifest {
            schema_version: 2,
            media_type: MT_OCI_MANIFEST.into(),
            artifact_type: Some(ARTIFACT_TYPE.into()),
            config: Descriptor {
                media_type: MT_CONFIG.into(),
                digest: sha256_digest(b"{}"),
                size: 2,
                platform: None,
            },
            layers: vec![],
            annotations: BTreeMap::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&m).unwrap();
        assert_eq!(v["schemaVersion"], 2);
        assert_eq!(v["artifactType"], "application/vnd.fluxvm.disk.v1");
        assert_eq!(v["mediaType"], MT_OCI_MANIFEST);
        assert!(v.get("annotations").is_none());
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_through_chunks_and_a_token() {
        let (host, st) = registry(|r| r.require_auth = true).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        let data = sample(2500);
        std::fs::write(&src, &data).unwrap();
        let reference = format!("{host}/fleet/disk:v1");

        let pushed = push(&src, &reference, None, &opts(1000)).await.unwrap();
        assert_eq!(pushed.format, DiskFormat::Raw);
        assert_eq!(pushed.layer_size, 2500);
        assert_eq!(pushed.virtual_size, 2500);
        assert!(pushed.layer_uploaded);
        assert_eq!(pushed.layer_digest, sha256_digest(&data));
        {
            let reg = st.lock().unwrap();
            assert_eq!(reg.patches, 3, "2500 bytes in 1000-byte chunks");
            assert_eq!(reg.blobs[&pushed.layer_digest], data);
            assert!(reg.token_requests >= 1, "the 401 challenge was answered");
            let m: serde_json::Value = serde_json::from_slice(&reg.manifests["v1"]).unwrap();
            assert_eq!(m["artifactType"], ARTIFACT_TYPE);
            assert_eq!(
                m["layers"][0]["mediaType"],
                "application/vnd.fluxvm.disk.layer.v1.raw"
            );
            assert_eq!(pushed.size, reg.manifests["v1"].len() as u64);
            assert_eq!(pushed.manifest_digest, sha256_digest(&reg.manifests["v1"]));
        }

        let out = d.path().join("pulled.raw");
        let pulled = pull_disk(&reference, &out, None).await.unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert_eq!(pulled.manifest_digest, pushed.manifest_digest);
        assert_eq!(pulled.architecture, "arm64");
        assert_eq!(pulled.size, 2500);

        // Pinned by digest, into a second file.
        let pinned = format!("{host}/fleet/disk@{}", pushed.manifest_digest);
        let out2 = d.path().join("pinned.raw");
        pull_disk(&pinned, &out2, None).await.unwrap();
        assert_eq!(std::fs::read(&out2).unwrap(), data);
        // No partial files are left next to the result.
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn small_files_go_up_in_one_put_with_an_absolute_location() {
        let (host, st) = registry(|r| r.absolute_location = true).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        std::fs::write(&src, sample(500)).unwrap();
        let reference = format!("{host}/fleet/small:1");
        push(&src, &reference, None, &opts(1000)).await.unwrap();
        let reg = st.lock().unwrap();
        assert_eq!(reg.patches, 0);
        assert_eq!(reg.blob_puts, 2, "layer and config");
    }

    #[tokio::test]
    async fn a_second_push_skips_blobs_the_registry_has() {
        let (host, st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        std::fs::write(&src, sample(2500)).unwrap();
        let first = push(&src, &format!("{host}/fleet/disk:a"), None, &opts(1000))
            .await
            .unwrap();
        let (puts, patches) = {
            let r = st.lock().unwrap();
            (r.blob_puts, r.patches)
        };
        let second = push(&src, &format!("{host}/fleet/disk:b"), None, &opts(1000))
            .await
            .unwrap();
        assert!(first.layer_uploaded);
        assert!(!second.layer_uploaded);
        assert_eq!(second.manifest_digest, first.manifest_digest);
        let r = st.lock().unwrap();
        assert_eq!((r.blob_puts, r.patches), (puts, patches));
        assert!(r.manifests.contains_key("b"));
    }

    #[tokio::test]
    async fn qcow2_records_its_virtual_size_and_media_type() {
        let (host, st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let src = qcow2_file(d.path(), 10 << 30);
        let reference = format!("{host}/fleet/q:1");
        let pushed = push(&src, &reference, None, &opts(0)).await.unwrap();
        assert_eq!(pushed.format, DiskFormat::Qcow2);
        assert_eq!(pushed.virtual_size, 10 << 30);
        {
            let reg = st.lock().unwrap();
            let m: serde_json::Value = serde_json::from_slice(&reg.manifests["1"]).unwrap();
            assert_eq!(
                m["layers"][0]["mediaType"],
                "application/vnd.fluxvm.disk.layer.v1.qcow2"
            );
        }
        let out = d.path().join("out.qcow2");
        let pulled = pull_disk(&reference, &out, None).await.unwrap();
        assert_eq!(pulled.format, DiskFormat::Qcow2);
        assert_eq!(pulled.virtual_size, 10 << 30);
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&src).unwrap());
    }

    #[tokio::test]
    async fn a_format_that_disagrees_with_the_file_is_refused() {
        let (host, _st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let q = qcow2_file(d.path(), 1 << 20);
        let o = PushOptions {
            format: Some(DiskFormat::Raw),
            ..opts(0)
        };
        let e = push(&q, &format!("{host}/f/q:1"), None, &o)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("looks like qcow2"), "{e:#}");
        let raw = d.path().join("r.raw");
        std::fs::write(&raw, sample(100)).unwrap();
        let o = PushOptions {
            format: Some(DiskFormat::Qcow2),
            ..opts(0)
        };
        assert!(
            push(&raw, &format!("{host}/f/q:1"), None, &o)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_wrong_docker_content_digest_fails_the_push() {
        let (host, _st) = registry(|r| r.lie_about_digest = true).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        std::fs::write(&src, sample(100)).unwrap();
        let e = push(&src, &format!("{host}/f/d:1"), None, &opts(0))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("registry stored manifest"), "{e:#}");
    }

    #[tokio::test]
    async fn a_corrupted_layer_is_rejected_and_leaves_nothing_behind() {
        let (host, st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        std::fs::write(&src, sample(2500)).unwrap();
        let reference = format!("{host}/f/c:1");
        let pushed = push(&src, &reference, None, &opts(1000)).await.unwrap();
        st.lock().unwrap().corrupt.push(pushed.layer_digest.clone());
        let out = d.path().join("out.raw");
        let e = pull_disk(&reference, &out, None).await.unwrap_err();
        assert!(e.to_string().contains("layer digest mismatch"), "{e:#}");
        assert!(!out.exists());
        let parts = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".part-"))
            .count();
        assert_eq!(parts, 0);
    }

    #[tokio::test]
    async fn a_manifest_that_is_not_the_pinned_digest_is_rejected() {
        let (host, st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("disk.raw");
        std::fs::write(&src, sample(100)).unwrap();
        let pushed = push(&src, &format!("{host}/f/p:1"), None, &opts(0))
            .await
            .unwrap();
        // The registry now serves different bytes under the pinned digest.
        let swapped = br#"{"schemaVersion":2}"#.to_vec();
        st.lock()
            .unwrap()
            .manifests
            .insert(pushed.manifest_digest.clone(), swapped);
        let pinned = format!("{host}/f/p@{}", pushed.manifest_digest);
        let e = pull_disk(&pinned, &d.path().join("o"), None)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("manifest digest mismatch"), "{e:#}");
    }

    #[tokio::test]
    async fn other_artifacts_and_existing_outputs_are_refused() {
        let (host, st) = registry(|_| {}).await;
        let d = tempfile::tempdir().unwrap();
        let other = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":2},"layers":[]}"#;
        st.lock()
            .unwrap()
            .manifests
            .insert("img".into(), other.to_vec());
        let e = pull_disk(&format!("{host}/f/x:img"), &d.path().join("o"), None)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not a FluxVM disk image"), "{e:#}");

        let existing = d.path().join("here");
        std::fs::write(&existing, b"keep").unwrap();
        let e = pull_disk(&format!("{host}/f/x:img"), &existing, None)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e:#}");
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn push_validates_its_input_before_touching_the_network() {
        let d = tempfile::tempdir().unwrap();
        let empty = d.path().join("e");
        std::fs::write(&empty, b"").unwrap();
        assert!(
            push(&empty, "localhost:1/a/b:1", None, &opts(0))
                .await
                .is_err()
        );
        assert!(
            push(d.path(), "localhost:1/a/b:1", None, &opts(0))
                .await
                .is_err()
        );
        let f = d.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        let digest = sha256_digest(b"x");
        assert!(
            push(&f, &format!("localhost:1/a/b@{digest}"), None, &opts(0))
                .await
                .is_err()
        );
    }
}
